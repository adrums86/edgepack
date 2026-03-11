//! Local sandbox for edgepack.
//!
//! Provides a web UI and API server for testing the repackaging pipeline
//! locally without deploying to a CDN edge. Uses reqwest for HTTP transport
//! and the global in-memory cache singleton instead of Redis.
//!
//! Run with: `cargo run --bin sandbox --features sandbox --target $(rustc -vV | grep host | awk '{print $2}')`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use edgepack::cache::{CacheBackend, CacheKeys};
use edgepack::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, JitConfig, SpekeAuth,
};
use edgepack::manifest;
use edgepack::manifest::types::OutputFormat;
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::media::compat;
use edgepack::media::codec;
use edgepack::media::TrackType;
use edgepack::repackager::pipeline::RepackagePipeline;
use edgepack::repackager::progressive::ProgressiveOutput;
use edgepack::repackager::PipelineEvent;
use edgepack::repackager::RepackageRequest;
use edgepack::repackager::SourceConfig;

// ─── Shared HTTP Client ────────────────────────────────────────────────

/// Shared reqwest::blocking::Client singleton for the sandbox.
/// Prevents connection exhaustion when 20+ concurrent threads make HTTP requests.
fn shared_reqwest_client() -> &'static reqwest::blocking::Client {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<reqwest::blocking::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::blocking::Client::builder()
            .pool_max_idle_per_host(32)
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_else(|_| reqwest::blocking::Client::new())
    })
}

// ─── Application State ─────────────────────────────────────────────────

struct AppState {
    jobs: Mutex<HashMap<String, JobHandle>>,
}

#[allow(dead_code)]
struct JobHandle {
    content_id: String,
    format: OutputFormat,
}

// ─── Request / Response Types ───────────────────────────────────────────

#[derive(Deserialize)]
struct RepackagePayload {
    source_url: String,
    speke_url: String,
    #[serde(default = "default_speke_auth_type")]
    speke_auth_type: String,
    #[serde(default)]
    speke_auth_value: String,
    #[serde(default)]
    speke_api_key_header: String,
    output_format: String,
    #[serde(default)]
    target_schemes: Vec<String>,
    #[serde(default)]
    target_scheme: Option<String>,
    #[serde(default = "default_container_format")]
    container_format: String,
    #[serde(default)]
    cache_control: Option<CacheControlPayload>,
}

#[derive(Deserialize)]
struct CacheControlPayload {
    #[serde(default)]
    segment_max_age: Option<u64>,
    #[serde(default)]
    final_manifest_max_age: Option<u64>,
    #[serde(default)]
    live_manifest_max_age: Option<u64>,
    #[serde(default)]
    live_manifest_s_maxage: Option<u64>,
    #[serde(default)]
    immutable: Option<bool>,
}

fn default_speke_auth_type() -> String {
    "bearer".into()
}

fn default_container_format() -> String {
    "cmaf".into()
}

#[derive(Serialize)]
struct RepackageResponse {
    content_id: String,
    format: String,
    message: String,
    container_format: String,
}

#[derive(Serialize)]
struct StatusResponse {
    state: String,
    segments_completed: u32,
    segments_total: Option<u32>,
    output_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schemes: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    validation: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timing: Option<serde_json::Value>,
    #[serde(default)]
    playback_ready: bool,
    /// Variants that were detected in the source but skipped (unsupported container/codec).
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped_variants: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Metadata for a resolved video variant.
#[derive(Clone)]
struct VideoVariantInfo {
    url: String,
    bandwidth: u64,
    width: Option<u32>,
    height: Option<u32>,
    codecs: Option<String>,
    frame_rate: Option<String>,
    /// Original index from the source manifest (used for file naming: v{idx}_video.m3u8).
    /// Only set after filtering — defaults to the position in the variants Vec.
    original_index: Option<usize>,
}

/// A variant that was detected in the source but skipped due to unsupported container/codec.
#[derive(Clone, Serialize)]
struct SkippedVariant {
    /// Why this variant was skipped (e.g., "WebM container not supported — ISOBMFF only")
    reason: String,
    /// Bandwidth from source manifest
    bandwidth: u64,
    /// Resolution from source manifest
    width: Option<u32>,
    height: Option<u32>,
    /// Codec string from source manifest
    codecs: Option<String>,
    /// Container format (e.g., "video/webm", "audio/webm")
    mime_type: Option<String>,
}

/// Result of resolving a master playlist to media playlist(s).
struct ResolvedSource {
    video_url: String,
    audio_url: Option<String>,
    text_tracks: Vec<TextTrackInfo>,
    video_variants: Vec<VideoVariantInfo>,
    /// Variants that were detected but skipped (unsupported container/codec).
    skipped_variants: Vec<SkippedVariant>,
}

/// Information about a text/subtitle track resolved from a master playlist.
struct TextTrackInfo {
    url: String,
    name: String,
    language: Option<String>,
    is_raw_vtt: bool,
}

/// Shared state for progressive manifest updates during parallel processing.
/// All track processing threads hold a reference to this and update it as
/// segments arrive. After each update, the combined manifest is rebuilt on disk
/// so a player can begin playback before all tracks finish.
struct ProgressiveManifestContext {
    /// Content ID for output directory paths.
    content_id: String,
    /// Output formats being produced (HLS, DASH, or both).
    output_formats: Vec<OutputFormat>,
    /// Target encryption schemes.
    target_schemes: Vec<EncryptionScheme>,
    /// Container format for segment extensions.
    container_format: edgepack::media::container::ContainerFormat,
    /// Video variant metadata (for master manifest rendering).
    video_variants: Vec<VideoVariantInfo>,
    /// Text track source info (for subtitle rendition groups).
    text_source_tracks: Vec<TextTrackInfo>,
    /// Whether there are multiple video variants.
    is_multi_variant: bool,
    /// Number of expected video variants (for "all ready" check).
    expected_video_variants: usize,
    /// Whether audio is expected.
    expects_audio: bool,
    /// Set of variant indices that have produced at least one manifest.
    video_variants_ready: std::collections::HashSet<usize>,
    /// Whether audio manifest is available on disk.
    audio_manifest_available: bool,
    /// Set of text track indices with manifests available on disk.
    text_manifests_available: std::collections::HashSet<usize>,
    /// Whether the first combined manifest has been written.
    /// We wait until all variants + audio have at least one segment
    /// so the player gets a fully playable manifest on first load.
    first_manifest_written: bool,
}

impl ProgressiveManifestContext {
    /// Check if all required tracks are ready for initial playback.
    /// We wait until every video variant + audio (if expected) has produced
    /// at least one media manifest before writing the combined manifest.
    fn all_tracks_ready(&self) -> bool {
        let video_ready = self.video_variants_ready.len() >= self.expected_video_variants;
        let audio_ready = !self.expects_audio || self.audio_manifest_available;
        video_ready && audio_ready
    }

    /// Rebuild the combined manifest for all (format, scheme) pairs,
    /// incorporating whatever track manifests are currently on disk.
    /// Defers the first write until all variants + audio are playable.
    fn rebuild_combined_manifests(&mut self) {
        // Don't write until all tracks have at least one playable segment,
        // unless we've already started (subsequent updates are always written).
        if !self.first_manifest_written && !self.all_tracks_ready() {
            return;
        }
        self.first_manifest_written = true;

        for scheme in &self.target_schemes {
            let scheme_str = scheme.scheme_type_str();
            for out_format in &self.output_formats {
                let fmt_label = match out_format {
                    OutputFormat::Hls => "hls",
                    OutputFormat::Dash => "dash",
                };
                let out_dir = PathBuf::from(format!(
                    "sandbox/output/{}/{fmt_label}_{scheme_str}",
                    self.content_id
                ));

                // Read current per-track manifests from disk
                let ext = out_format.manifest_extension();

                let video_manifest = if self.is_multi_variant && *out_format == OutputFormat::Hls {
                    // For multi-variant HLS, the master manifest references per-variant
                    // media playlists (v0_video.m3u8, v1_video.m3u8, etc.) — we don't
                    // need the video manifest content in the master, just metadata.
                    String::new()
                } else if self.is_multi_variant && *out_format == OutputFormat::Dash {
                    // For multi-variant DASH, read the first ready variant's MPD from disk
                    // and use it as the base to build a multi-Representation MPD.
                    let ready_sorted: Vec<usize> = {
                        let mut v: Vec<usize> = self.video_variants_ready.iter().copied().collect();
                        v.sort();
                        v
                    };
                    if let Some(&first_vid) = ready_sorted.first() {
                        let first_oidx = self.video_variants.get(first_vid)
                            .and_then(|v| v.original_index)
                            .unwrap_or(first_vid);
                        std::fs::read_to_string(out_dir.join(format!("v{first_oidx}_video.{ext}"))).unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    std::fs::read_to_string(out_dir.join(format!("video.{ext}"))).unwrap_or_default()
                };

                let audio_manifest = if self.audio_manifest_available {
                    std::fs::read_to_string(out_dir.join(format!("audio.{ext}"))).unwrap_or_default()
                } else {
                    String::new()
                };

                // Collect text track infos for tracks that have manifests on disk
                let text_manifest_infos: Vec<TextManifestInfo> = self.text_source_tracks.iter()
                    .enumerate()
                    .filter(|(idx, tt)| self.text_manifests_available.contains(idx) || tt.is_raw_vtt)
                    .map(|(idx, tt)| TextManifestInfo {
                        index: idx,
                        name: tt.name.clone(),
                        language: tt.language.clone(),
                    })
                    .collect();

                // Only include variants that have produced manifests.
                // Preserve original_index so filenames match (v{idx}_video.m3u8).
                let ready_variants: Vec<VideoVariantInfo> = if self.is_multi_variant {
                    self.video_variants.iter()
                        .enumerate()
                        .filter(|(idx, _)| self.video_variants_ready.contains(idx))
                        .map(|(idx, v)| VideoVariantInfo {
                            url: String::new(),
                            bandwidth: v.bandwidth,
                            width: v.width,
                            height: v.height,
                            codecs: v.codecs.clone(),
                            frame_rate: v.frame_rate.clone(),
                            original_index: v.original_index.or(Some(idx)),
                        })
                        .collect()
                } else {
                    self.video_variants.iter()
                        .map(|v| VideoVariantInfo {
                            url: String::new(),
                            bandwidth: v.bandwidth,
                            width: v.width,
                            height: v.height,
                            codecs: v.codecs.clone(),
                            frame_rate: v.frame_rate.clone(),
                            original_index: v.original_index,
                        })
                        .collect()
                };

                let combined = build_progressive_combined_manifest(
                    *out_format,
                    &video_manifest,
                    &audio_manifest,
                    &self.content_id,
                    scheme,
                    self.container_format,
                    &text_manifest_infos,
                    &ready_variants,
                    false, // not complete — progressive processing
                );

                let final_manifest = if *out_format == OutputFormat::Dash {
                    fixup_dash_progressive(&combined)
                } else {
                    combined
                };
                let _ = std::fs::write(out_dir.join(format!("manifest.{ext}")), &final_manifest);
            }
        }
    }

    /// Signal that a video variant has produced its first manifest.
    /// `variant_idx` is the original variant index from the source.
    fn signal_video_manifest(&mut self, variant_idx: usize) {
        self.video_variants_ready.insert(variant_idx);
        self.rebuild_combined_manifests();
    }

    /// Signal that a video variant has failed and will never produce output.
    /// Decrements expected count so we don't wait indefinitely.
    fn signal_video_failed(&mut self) {
        if self.expected_video_variants > 0 {
            self.expected_video_variants -= 1;
        }
        // May now be ready if all remaining variants have completed
        self.rebuild_combined_manifests();
    }

    /// Signal that audio has failed and will never produce output.
    fn signal_audio_failed(&mut self) {
        self.expects_audio = false;
        self.rebuild_combined_manifests();
    }

    /// Signal that audio manifest has been updated on disk.
    fn signal_audio_manifest(&mut self) {
        if !self.audio_manifest_available {
            self.audio_manifest_available = true;
            self.rebuild_combined_manifests();
        }
    }

    /// Signal that a text track's manifest has been updated on disk.
    fn signal_text_manifest(&mut self, text_idx: usize) {
        if self.text_manifests_available.insert(text_idx) {
            // New text track — rebuild combined if we've already started
            self.rebuild_combined_manifests();
        }
    }

    /// Update the variant list to only include successful variants.
    /// Called after Phase 1 completes to produce a final manifest without
    /// references to variants that failed processing.
    fn finalize_with_successful_variants(&mut self, successful: Vec<VideoVariantInfo>) {
        self.video_variants = successful;
        self.expected_video_variants = self.video_variants.len();
        self.video_variants_ready = (0..self.video_variants.len()).collect();
        self.first_manifest_written = true;
        self.rebuild_combined_manifests();
    }
}

// ─── Handlers ───────────────────────────────────────────────────────────

async fn serve_ui() -> Html<&'static str> {
    Html(SANDBOX_HTML)
}

async fn handle_repackage(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RepackagePayload>,
) -> Response {
    let output_format = match payload.output_format.as_str() {
        "hls" => OutputFormat::Hls,
        "dash" => OutputFormat::Dash,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "output_format must be 'hls' or 'dash'".into(),
                }),
            )
                .into_response();
        }
    };

    // Resolve target schemes: target_schemes (array) > target_scheme (single) > default ["cenc"]
    let scheme_strings = if !payload.target_schemes.is_empty() {
        payload.target_schemes.clone()
    } else if let Some(ref single) = payload.target_scheme {
        vec![single.clone()]
    } else {
        vec!["cenc".to_string()]
    };
    let mut target_schemes = Vec::with_capacity(scheme_strings.len());
    for s in &scheme_strings {
        let scheme = match s.as_str() {
            "cenc" => EncryptionScheme::Cenc,
            "cbcs" => EncryptionScheme::Cbcs,
            "none" => EncryptionScheme::None,
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("target_scheme must be 'cenc', 'cbcs', or 'none' (got '{s}')"),
                    }),
                )
                    .into_response();
            }
        };
        target_schemes.push(scheme);
    }

    let container_format = match payload.container_format.as_str() {
        "cmaf" => edgepack::media::container::ContainerFormat::Cmaf,
        "fmp4" => edgepack::media::container::ContainerFormat::Fmp4,
        "iso" => edgepack::media::container::ContainerFormat::Iso,
        #[cfg(feature = "ts")]
        "ts" => edgepack::media::container::ContainerFormat::Ts,
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "container_format must be 'cmaf', 'fmp4', 'iso', or 'ts'".into(),
                }),
            )
                .into_response();
        }
    };

    // SPEKE configuration — only needed when any target scheme requires encryption.
    // For clear (None) output, use a dummy SPEKE config since it won't be called.
    let any_target_encrypted = target_schemes.iter().any(|s| s.is_encrypted());
    let (speke_url, speke_auth) = if any_target_encrypted {
        let url = match edgepack::url::Url::parse(&payload.speke_url) {
            Ok(u) => u,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("invalid speke_url: {e}"),
                    }),
                )
                    .into_response();
            }
        };

        let auth = match payload.speke_auth_type.as_str() {
            "bearer" => SpekeAuth::Bearer(payload.speke_auth_value.clone()),
            "api_key" => SpekeAuth::ApiKey {
                header: if payload.speke_api_key_header.is_empty() {
                    "x-api-key".into()
                } else {
                    payload.speke_api_key_header.clone()
                },
                value: payload.speke_auth_value.clone(),
            },
            "basic" => {
                let parts: Vec<&str> = payload.speke_auth_value.splitn(2, ':').collect();
                if parts.len() != 2 {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse {
                            error: "basic auth value must be 'username:password'".into(),
                        }),
                    )
                        .into_response();
                }
                SpekeAuth::Basic {
                    username: parts[0].into(),
                    password: parts[1].into(),
                }
            }
            _ => SpekeAuth::Bearer(payload.speke_auth_value.clone()),
        };

        (url, auth)
    } else {
        // Dummy SPEKE config for clear output — pipeline will skip SPEKE calls
        (
            edgepack::url::Url::parse("https://unused.local/speke").unwrap(),
            SpekeAuth::Bearer("unused".into()),
        )
    };

    // Generate a content_id from the source URL
    let content_id = generate_content_id(&payload.source_url);
    let fmt_str = match output_format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };

    // Check if source is a local file path
    let source_url = if is_local_path(&payload.source_url) {
        match start_local_file_server(&payload.source_url).await {
            Ok(url) => url,
            Err(e) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: format!("failed to serve local file: {e}"),
                    }),
                )
                    .into_response();
            }
        }
    } else {
        payload.source_url.clone()
    };

    // Resolve master playlists to media playlists (HLS multivariant → single variant)
    let resolved = match resolve_master_playlist(&source_url).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: format!("failed to resolve source manifest: {e}"),
                }),
            )
                .into_response();
        }
    };
    let source_url = resolved.video_url;
    let audio_source_url = resolved.audio_url;
    let text_tracks = resolved.text_tracks;
    let video_variants = resolved.video_variants;
    let skipped_variants = resolved.skipped_variants;

    // Build config
    let config = AppConfig {
        drm: DrmConfig {
            speke_url,
            speke_auth,
            system_ids: DrmSystemIds::default(),
        },
        cache: CacheConfig::default(),
        jit: JitConfig::default(),
        policy: edgepack::config::PolicyConfig::default(),
    };

    let container_format_str = payload.container_format.clone();
    let request = RepackageRequest {
        content_id: content_id.clone(),
        source_url,
        output_formats: vec![output_format],
        target_schemes: target_schemes.clone(),
        container_format,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: payload.cache_control.map(|cc| edgepack::config::CacheControlConfig {
            segment_max_age: cc.segment_max_age,
            final_manifest_max_age: cc.final_manifest_max_age,
            live_manifest_max_age: cc.live_manifest_max_age,
            live_manifest_s_maxage: cc.live_manifest_s_maxage,
            immutable: cc.immutable,
        }),
    };

    // Track the job
    {
        let mut jobs = state.jobs.lock().unwrap();
        jobs.insert(
            format!("{content_id}/{fmt_str}"),
            JobHandle {
                content_id: content_id.clone(),
                format: output_format,
            },
        );
    }

    // Run pipeline in a blocking thread, writing job state for polling
    let cid = content_id.clone();
    let _fmt = output_format;
    let audio_source = audio_source_url;
    let text_source_tracks = text_tracks;
    let video_variants_for_task = video_variants;
    tokio::task::spawn_blocking(move || {
        let cache = edgepack::cache::global_cache();
        let state_key = CacheKeys::job_state(&cid, fmt_str);

        // Write initial "Processing" state (include skipped variants if any)
        let skipped_init_json: serde_json::Value = if skipped_variants.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&skipped_variants).unwrap_or(serde_json::Value::Null)
        };
        let initial_state = serde_json::json!({
            "state": "Processing",
            "segments_completed": 0,
            "segments_total": null,
            "skipped_variants": skipped_init_json,
        });
        let _ = cache.set(
            &state_key,
            &serde_json::to_vec(&initial_state).unwrap(),
            3600,
        );

        let pipeline_start = Instant::now();
        let container_fmt = request.container_format;
        let has_audio = audio_source.is_some();
        let has_text = !text_source_tracks.is_empty();

        // ──────────────────────────────────────────────────────────────
        // Phase 0: Write master manifest immediately from variant metadata.
        // No segments needed — just metadata (bandwidth, resolution, codecs).
        // Set up shared progressive manifest context for Phase 1 updates.
        // ──────────────────────────────────────────────────────────────
        let is_multi_variant = video_variants_for_task.len() > 1;
        let mut scheme_list: Vec<String> = Vec::new();

        // Create output directories and write initial master manifest
        for scheme in &target_schemes {
            let scheme_str = scheme.scheme_type_str();
            for out_format in &request.output_formats {
                let fmt_label = match out_format {
                    OutputFormat::Hls => "hls",
                    OutputFormat::Dash => "dash",
                };
                let out_dir = PathBuf::from(format!(
                    "sandbox/output/{cid}/{fmt_label}_{scheme_str}"
                ));
                if out_dir.exists() {
                    let _ = std::fs::remove_dir_all(&out_dir);
                }
                let _ = std::fs::create_dir_all(&out_dir);
                if !scheme_list.contains(&scheme_str.to_string()) {
                    scheme_list.push(scheme_str.to_string());
                }

                // Master manifest is NOT written here — it's deferred until all
                // variants + audio have produced at least one playable segment.
                // The ProgressiveManifestContext handles this automatically.
            }
        }

        // Build shared progressive manifest context for Phase 1 updates.
        // Cloned text track info (we need the metadata but not the URLs).
        let progressive_text_tracks: Vec<TextTrackInfo> = text_source_tracks.iter()
            .map(|tt| TextTrackInfo {
                url: String::new(), // not needed for manifest building
                name: tt.name.clone(),
                language: tt.language.clone(),
                is_raw_vtt: tt.is_raw_vtt,
            })
            .collect();
        let progressive_variants: Vec<VideoVariantInfo> = video_variants_for_task.iter()
            .enumerate()
            .map(|(idx, v)| VideoVariantInfo {
                url: String::new(), // not needed for manifest building
                bandwidth: v.bandwidth,
                width: v.width,
                height: v.height,
                codecs: v.codecs.clone(),
                frame_rate: v.frame_rate.clone(),
                original_index: Some(idx),
            })
            .collect();
        let manifest_ctx = Arc::new(Mutex::new(ProgressiveManifestContext {
            content_id: cid.clone(),
            output_formats: request.output_formats.clone(),
            target_schemes: target_schemes.clone(),
            container_format: container_fmt,
            video_variants: progressive_variants,
            text_source_tracks: progressive_text_tracks,
            is_multi_variant,
            expected_video_variants: video_variants_for_task.len(),
            expects_audio: has_audio,
            video_variants_ready: std::collections::HashSet::new(),
            audio_manifest_available: false,
            text_manifests_available: std::collections::HashSet::new(),
            first_manifest_written: false,
        }));

        // ──────────────────────────────────────────────────────────────
        // Phase 1: Process ALL tracks in parallel — video variants,
        // audio, and text tracks run concurrently via std::thread::scope.
        // Each track writes its own segments/inits to disk independently.
        // ──────────────────────────────────────────────────────────────
        let text_count_str = if has_text {
            format!(" + {} text track(s)", text_source_tracks.len())
        } else {
            String::new()
        };
        eprintln!(
            "  Processing all tracks in parallel: {} video variant(s){}{}",
            video_variants_for_task.len(),
            if has_audio { " + audio" } else { "" },
            text_count_str,
        );

        type TrackResult = Result<Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>, String>;

        // All results collected after parallel processing
        let video_outputs: Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>;
        // (variant_index, pipeline_outputs) — tracks which variants succeeded
        let mut all_variant_outputs: Vec<(usize, Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>)> = Vec::new();
        let mut audio_outputs_final: Option<Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>> = None;
        let mut text_outputs_final: Vec<Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>> = Vec::new();
        let mut segments_total_count = 0usize;

        // Shared progress counters — updated atomically from segment callbacks
        let progress_segments = Arc::new(AtomicU32::new(0));
        let progress_playback_ready = Arc::new(std::sync::atomic::AtomicBool::new(false));

        // Background thread to flush progress to cache periodically
        let progress_segments_bg = progress_segments.clone();
        let progress_ready_bg = progress_playback_ready.clone();
        let state_key_bg = state_key.clone();
        let skipped_bg = skipped_init_json.clone();
        let schemes_bg = scheme_list.clone();
        let progress_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let progress_stop_bg = progress_stop.clone();
        let _progress_flusher = std::thread::spawn(move || {
            let cache = edgepack::cache::global_cache();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(500));
                if progress_stop_bg.load(Ordering::Relaxed) {
                    break;
                }
                let completed = progress_segments_bg.load(Ordering::Relaxed);
                let ready = progress_ready_bg.load(Ordering::Relaxed);
                let state = serde_json::json!({
                    "state": "Processing",
                    "segments_completed": completed,
                    "segments_total": null,
                    "playback_ready": ready,
                    "skipped_variants": skipped_bg,
                    "schemes": schemes_bg,
                });
                let _ = cache.set(
                    &state_key_bg,
                    &serde_json::to_vec(&state).unwrap(),
                    3600,
                );
            }
        });

        {
            // Use thread::scope so all spawned threads share references to our local variables
            let all_results: Vec<(&str, usize, TrackResult)> = std::thread::scope(|scope| {
                let mut handles: Vec<(&str, usize, std::thread::ScopedJoinHandle<'_, TrackResult>)> = Vec::new();

                // ── Spawn video variant threads ──────────────────────
                if is_multi_variant {
                    for (vid, variant) in video_variants_for_task.iter().enumerate() {
                        let config_clone = config.clone();
                        let cid_ref = &cid;
                        let request_ref = &request;
                        let container_fmt_ref = &container_fmt;
                        let manifest_ctx_ref = &manifest_ctx;
                        let progress_ref = &progress_segments;
                        let progress_ready_ref = &progress_playback_ready;
                        let variant_url = variant.url.clone();
                        handles.push(("video", vid, scope.spawn(move || {
                            let variant_request = RepackageRequest {
                                content_id: format!("{cid_ref}_v{vid}"),
                                source_url: variant_url,
                                output_formats: request_ref.output_formats.clone(),
                                target_schemes: request_ref.target_schemes.clone(),
                                container_format: request_ref.container_format,
                                key_ids: vec![],
                                raw_keys: vec![],
                                key_rotation: None,
                                clear_lead_segments: None,
                                drm_systems: vec![],
                                enable_iframe_playlist: false,
                                dvr_window_duration: None,
                                content_steering: None,
                                cache_control: None,
                            };

                            let prefix = format!("v{vid}");
                            let pipeline = RepackagePipeline::new(config_clone);
                            let result = pipeline.execute_progressive(&variant_request, |event| {
                                match event {
                                    PipelineEvent::InitReady { inits } => {
                                        for (out_format, scheme, init_data) in &inits {
                                            let scheme_str = scheme.scheme_type_str();
                                            let fmt_label = match out_format {
                                                OutputFormat::Hls => "hls",
                                                OutputFormat::Dash => "dash",
                                            };
                                            let out_dir = PathBuf::from(format!(
                                                "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                            ));
                                            if !init_data.is_empty() {
                                                let _ = std::fs::write(
                                                    out_dir.join(format!("{prefix}_init.mp4")),
                                                    init_data,
                                                );
                                            }
                                        }
                                    }
                                    PipelineEvent::SegmentProcessed {
                                        segment_number,
                                        outputs: seg_outputs,
                                        ..
                                    } => {
                                        for seg in &seg_outputs {
                                            let scheme_str = seg.scheme.scheme_type_str();
                                            let fmt_label = match seg.format {
                                                OutputFormat::Hls => "hls",
                                                OutputFormat::Dash => "dash",
                                            };
                                            let out_dir = PathBuf::from(format!(
                                                "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                            ));

                                            let seg_ext = container_fmt_ref.video_segment_extension();
                                            let _ = std::fs::write(
                                                out_dir.join(format!(
                                                    "{prefix}_segment_{segment_number}{seg_ext}"
                                                )),
                                                seg.segment_data,
                                            );

                                            // Write per-variant video manifest
                                            if let Some(ref manifest_str) = seg.manifest {
                                                let ext = seg.format.manifest_extension();
                                                let rewritten = rewrite_variant_manifest(
                                                    manifest_str, cid_ref, vid,
                                                    seg.format, &seg.scheme, *container_fmt_ref,
                                                );
                                                let _ = std::fs::write(
                                                    out_dir.join(format!("{prefix}_video.{ext}")),
                                                    &rewritten,
                                                );
                                                // Signal progressive manifest context
                                                if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                                    ctx.signal_video_manifest(vid);
                                                    // Check if playback is ready
                                                    if ctx.first_manifest_written {
                                                        progress_ready_ref.store(true, Ordering::Relaxed);
                                                    }
                                                }
                                            }
                                        }
                                        // Update shared progress counter
                                        progress_ref.fetch_add(1, Ordering::Relaxed);
                                    }
                                }
                            });
                            match result {
                                Ok(outputs) => Ok(outputs),
                                Err(e) => {
                                    // Signal failure so the manifest context doesn't
                                    // wait for this variant's data.
                                    if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                        ctx.signal_video_failed();
                                    }
                                    Err(format!("variant {vid} failed: {e}"))
                                }
                            }
                        })));
                    }
                } else {
                    // Single variant — same as before but in a thread
                    let config_clone = config.clone();
                    let cid_ref = &cid;
                    let request_clone = request.clone();
                    let container_fmt_ref = &container_fmt;
                    let manifest_ctx_ref = &manifest_ctx;
                    let progress_ref = &progress_segments;
                    let progress_ready_ref = &progress_playback_ready;
                    handles.push(("video", 0, scope.spawn(move || {
                        let pipeline = RepackagePipeline::new(config_clone);
                        let result = pipeline.execute_progressive(&request_clone, |event| {
                            match event {
                                PipelineEvent::InitReady { inits } => {
                                    for (out_format, scheme, init_data) in &inits {
                                        let scheme_str = scheme.scheme_type_str();
                                        let fmt_label = match out_format {
                                            OutputFormat::Hls => "hls",
                                            OutputFormat::Dash => "dash",
                                        };
                                        let out_dir = PathBuf::from(format!(
                                            "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                        ));
                                        if !init_data.is_empty() {
                                            let _ = std::fs::write(out_dir.join("init.mp4"), init_data);
                                        }
                                    }
                                }
                                PipelineEvent::SegmentProcessed {
                                    segment_number,
                                    outputs: seg_outputs,
                                    ..
                                } => {
                                    for seg in &seg_outputs {
                                        let scheme_str = seg.scheme.scheme_type_str();
                                        let fmt_label = match seg.format {
                                            OutputFormat::Hls => "hls",
                                            OutputFormat::Dash => "dash",
                                        };
                                        let out_dir = PathBuf::from(format!(
                                            "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                        ));
                                        let seg_ext = container_fmt_ref.video_segment_extension();
                                        let _ = std::fs::write(
                                            out_dir.join(format!("segment_{segment_number}{seg_ext}")),
                                            seg.segment_data,
                                        );
                                        if let Some(ref manifest_str) = seg.manifest {
                                            let ext = seg.format.manifest_extension();
                                            let _ = std::fs::write(
                                                out_dir.join(format!("video.{ext}")),
                                                manifest_str,
                                            );
                                            // Signal progressive manifest context
                                            if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                                ctx.signal_video_manifest(0);
                                                if ctx.first_manifest_written {
                                                    progress_ready_ref.store(true, Ordering::Relaxed);
                                                }
                                            }
                                        }
                                    }
                                    // Update shared progress counter
                                    progress_ref.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        });
                        match result {
                            Ok(outputs) => Ok(outputs),
                            Err(e) => {
                                if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                    ctx.signal_video_failed();
                                }
                                Err(format!("video pipeline failed: {e}"))
                            }
                        }
                    })));
                }

                // ── Spawn audio thread ───────────────────────────────
                if let Some(ref audio_src) = audio_source {
                    let config_clone = config.clone();
                    let cid_ref = &cid;
                    let request_ref = &request;
                    let container_fmt_ref = &container_fmt;
                    let manifest_ctx_ref = &manifest_ctx;
                    let progress_ref = &progress_segments;
                    let audio_url = audio_src.clone();
                    handles.push(("audio", 0, scope.spawn(move || {
                        let audio_request = RepackageRequest {
                            content_id: format!("{cid_ref}_audio"),
                            source_url: audio_url,
                            output_formats: request_ref.output_formats.clone(),
                            target_schemes: request_ref.target_schemes.clone(),
                            container_format: request_ref.container_format,
                            key_ids: vec![],
                            raw_keys: vec![],
                            key_rotation: None,
                            clear_lead_segments: None,
                            drm_systems: vec![],
                            enable_iframe_playlist: false,
                            dvr_window_duration: None,
                            content_steering: None,
                            cache_control: None,
                        };
                        let audio_pipeline = RepackagePipeline::new(config_clone);
                        let result = audio_pipeline.execute_progressive(&audio_request, |event| {
                            match event {
                                PipelineEvent::InitReady { inits } => {
                                    for (out_format, scheme, init_data) in &inits {
                                        let scheme_str = scheme.scheme_type_str();
                                        let fmt_label = match out_format {
                                            OutputFormat::Hls => "hls",
                                            OutputFormat::Dash => "dash",
                                        };
                                        let out_dir = PathBuf::from(format!(
                                            "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                        ));
                                        if !init_data.is_empty() {
                                            let _ = std::fs::write(out_dir.join("audio_init.mp4"), init_data);
                                        }
                                    }
                                }
                                PipelineEvent::SegmentProcessed {
                                    segment_number,
                                    outputs: seg_outputs,
                                    ..
                                } => {
                                    for seg in &seg_outputs {
                                        let scheme_str = seg.scheme.scheme_type_str();
                                        let fmt_label = match seg.format {
                                            OutputFormat::Hls => "hls",
                                            OutputFormat::Dash => "dash",
                                        };
                                        let out_dir = PathBuf::from(format!(
                                            "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                        ));
                                        let seg_ext = container_fmt_ref.audio_segment_extension();
                                        let _ = std::fs::write(
                                            out_dir.join(format!("audio_segment_{segment_number}{seg_ext}")),
                                            seg.segment_data,
                                        );
                                        if let Some(ref manifest_str) = seg.manifest {
                                            let ext = seg.format.manifest_extension();
                                            let rewritten = rewrite_audio_manifest(
                                                manifest_str, cid_ref, seg.format, &seg.scheme, *container_fmt_ref,
                                            );
                                            let _ = std::fs::write(
                                                out_dir.join(format!("audio.{ext}")),
                                                &rewritten,
                                            );
                                            // Signal audio manifest available — triggers
                                            // combined manifest rebuild so player can use audio
                                            if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                                ctx.signal_audio_manifest();
                                            }
                                        }
                                    }
                                    // Update shared progress counter
                                    progress_ref.fetch_add(1, Ordering::Relaxed);
                                }
                            }
                        });
                        match result {
                            Ok(outputs) => Ok(outputs),
                            Err(e) => {
                                if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                    ctx.signal_audio_failed();
                                }
                                Err(format!("audio pipeline failed: {e}"))
                            }
                        }
                    })));
                }

                // ── Spawn text track threads ─────────────────────────
                for (text_idx, text_track) in text_source_tracks.iter().enumerate() {
                    if text_track.is_raw_vtt {
                        // Raw WebVTT: spawn a lightweight download thread.
                        // For HLS subtitle tracks, the URL may be a media playlist (.m3u8)
                        // containing references to .vtt segment files. In that case, fetch
                        // the playlist, parse segment URLs, download all, and concatenate.
                        let cid_ref = &cid;
                        let request_ref = &request;
                        let target_schemes_ref = &target_schemes;
                        let manifest_ctx_ref = &manifest_ctx;
                        let vtt_url = text_track.url.clone();
                        handles.push(("text_vtt", text_idx, scope.spawn(move || {
                            let vtt_bytes = if vtt_url.contains(".m3u8") {
                                // HLS subtitle playlist: fetch, parse segments, download & concatenate
                                download_hls_vtt_segments(&vtt_url)?
                            } else {
                                // Direct VTT URL: simple download
                                match reqwest::blocking::get(&vtt_url) {
                                    Ok(resp) if resp.status().is_success() => {
                                        resp.bytes().map_err(|e| format!("VTT read failed: {e}"))?.to_vec()
                                    }
                                    Ok(resp) => return Err(format!("VTT download HTTP {}", resp.status())),
                                    Err(e) => return Err(format!("VTT download failed: {e}")),
                                }
                            };
                            for scheme in target_schemes_ref {
                                let scheme_str = scheme.scheme_type_str();
                                for out_format in &request_ref.output_formats {
                                    let fmt_label = match out_format {
                                        OutputFormat::Hls => "hls",
                                        OutputFormat::Dash => "dash",
                                    };
                                    let out_dir = PathBuf::from(format!(
                                        "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                    ));
                                    let vtt_filename = format!("text_{text_idx}.vtt");
                                    let _ = std::fs::write(out_dir.join(&vtt_filename), &vtt_bytes);
                                    if *out_format == OutputFormat::Hls {
                                        let wrapper = format!(
                                            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:99999\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:99999.0,\n{vtt_filename}\n#EXT-X-ENDLIST\n"
                                        );
                                        let _ = std::fs::write(
                                            out_dir.join(format!("text_{text_idx}.m3u8")),
                                            &wrapper,
                                        );
                                    }
                                }
                            }
                            // Signal text track available in combined manifest
                            if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                ctx.signal_text_manifest(text_idx);
                            }
                            Ok(vec![])
                        })));
                    } else {
                        // fMP4-wrapped text: run pipeline
                        let config_clone = config.clone();
                        let cid_ref = &cid;
                        let request_ref = &request;
                        let container_fmt_ref = &container_fmt;
                        let manifest_ctx_ref = &manifest_ctx;
                        let text_url = text_track.url.clone();
                        handles.push(("text", text_idx, scope.spawn(move || {
                            let text_request = RepackageRequest {
                                content_id: format!("{cid_ref}_text_{text_idx}"),
                                source_url: text_url,
                                output_formats: request_ref.output_formats.clone(),
                                target_schemes: request_ref.target_schemes.clone(),
                                container_format: request_ref.container_format,
                                key_ids: vec![],
                                raw_keys: vec![],
                                key_rotation: None,
                                clear_lead_segments: None,
                                drm_systems: vec![],
                                enable_iframe_playlist: false,
                                dvr_window_duration: None,
                                content_steering: None,
                                cache_control: None,
                            };
                            let text_prefix = format!("text_{text_idx}");
                            let text_pipeline = RepackagePipeline::new(config_clone);
                            let result = text_pipeline.execute_progressive(&text_request, |event| {
                                match event {
                                    PipelineEvent::InitReady { inits } => {
                                        for (out_format, scheme, init_data) in &inits {
                                            let scheme_str = scheme.scheme_type_str();
                                            let fmt_label = match out_format {
                                                OutputFormat::Hls => "hls",
                                                OutputFormat::Dash => "dash",
                                            };
                                            let out_dir = PathBuf::from(format!(
                                                "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                            ));
                                            if !init_data.is_empty() {
                                                let _ = std::fs::write(
                                                    out_dir.join(format!("{text_prefix}_init.mp4")),
                                                    init_data,
                                                );
                                            }
                                        }
                                    }
                                    PipelineEvent::SegmentProcessed {
                                        segment_number,
                                        outputs: seg_outputs,
                                        ..
                                    } => {
                                        for seg in &seg_outputs {
                                            let scheme_str = seg.scheme.scheme_type_str();
                                            let fmt_label = match seg.format {
                                                OutputFormat::Hls => "hls",
                                                OutputFormat::Dash => "dash",
                                            };
                                            let out_dir = PathBuf::from(format!(
                                                "sandbox/output/{cid_ref}/{fmt_label}_{scheme_str}"
                                            ));
                                            let seg_ext = container_fmt_ref.video_segment_extension();
                                            let _ = std::fs::write(
                                                out_dir.join(format!("{text_prefix}_segment_{segment_number}{seg_ext}")),
                                                seg.segment_data,
                                            );
                                            if let Some(ref manifest_str) = seg.manifest {
                                                let ext = seg.format.manifest_extension();
                                                let rewritten = rewrite_text_manifest(
                                                    manifest_str, cid_ref, text_idx,
                                                    seg.format, &seg.scheme, *container_fmt_ref,
                                                );
                                                let _ = std::fs::write(
                                                    out_dir.join(format!("{text_prefix}.{ext}")),
                                                    &rewritten,
                                                );
                                                // Signal text track manifest available
                                                if let Ok(mut ctx) = manifest_ctx_ref.lock() {
                                                    ctx.signal_text_manifest(text_idx);
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                            result.map_err(|e| format!("text track {text_idx} failed: {e}"))
                        })));
                    }
                }

                // ── Join all threads and collect results ──────────────
                handles.into_iter()
                    .map(|(kind, idx, handle)| {
                        let result = handle.join().unwrap_or_else(|_| Err(format!("{kind} {idx} panicked")));
                        (kind, idx, result)
                    })
                    .collect()
            });

            // Stop the progress flusher — all processing threads have completed
            progress_stop.store(true, Ordering::Relaxed);

            // Categorize results from all threads.
            // Signal failures to the progressive manifest context so it doesn't
            // wait for variants that will never produce output.
            for (kind, idx, result) in all_results {
                match kind {
                    "video" => {
                        match result {
                            Ok(outputs) if !outputs.is_empty() => {
                                let seg_count = outputs.first()
                                    .map(|(_, _, o)| o.manifest_state().segments.len())
                                    .unwrap_or(0);
                                eprintln!("  Video v{idx} complete: {seg_count} segments");
                                if seg_count > segments_total_count {
                                    segments_total_count = seg_count;
                                }
                                all_variant_outputs.push((idx, outputs));
                            }
                            Ok(_) => {
                                eprintln!("  Warning: video v{idx} produced no output");
                                if let Ok(mut ctx) = manifest_ctx.lock() {
                                    ctx.signal_video_failed();
                                }
                            }
                            Err(e) => {
                                eprintln!("  Warning: video v{idx} failed: {e}");
                                if let Ok(mut ctx) = manifest_ctx.lock() {
                                    ctx.signal_video_failed();
                                }
                            }
                        }
                    }
                    "audio" => {
                        match result {
                            Ok(outputs) => {
                                let seg_count = outputs.first()
                                    .map(|(_, _, o)| o.manifest_state().segments.len())
                                    .unwrap_or(0);
                                eprintln!("  Audio complete: {seg_count} segments");
                                audio_outputs_final = Some(outputs);
                            }
                            Err(e) => {
                                eprintln!("  Warning: audio failed: {e}");
                                if let Ok(mut ctx) = manifest_ctx.lock() {
                                    ctx.signal_audio_failed();
                                }
                            }
                        }
                    }
                    "text" | "text_vtt" => {
                        // Ensure the vec is large enough (text results may arrive out of order)
                        while text_outputs_final.len() <= idx {
                            text_outputs_final.push(vec![]);
                        }
                        match result {
                            Ok(outputs) => {
                                eprintln!("  Text track {idx} complete");
                                text_outputs_final[idx] = outputs;
                            }
                            Err(e) => {
                                eprintln!("  Warning: text track {idx} failed: {e}");
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // Select primary video outputs for finalization
        if all_variant_outputs.is_empty() {
            // All video failed
            let failed_state = serde_json::json!({
                "state": "Failed",
                "segments_completed": 0,
                "segments_total": null,
                "error": "all video variants failed",
            });
            let _ = cache.set(
                &state_key,
                &serde_json::to_vec(&failed_state).unwrap(),
                3600,
            );
            return;
        }

        // Write final per-variant manifests and select primary outputs.
        // Filter video_variants_for_task to only include variants that succeeded.
        let successful_variant_indices: Vec<usize> = all_variant_outputs.iter()
            .map(|(vid, _)| *vid)
            .collect();
        let successful_variants: Vec<VideoVariantInfo> = if is_multi_variant {
            successful_variant_indices.iter()
                .filter_map(|&vid| video_variants_for_task.get(vid).map(|v| VideoVariantInfo {
                    url: String::new(),
                    bandwidth: v.bandwidth,
                    width: v.width,
                    height: v.height,
                    codecs: v.codecs.clone(),
                    frame_rate: v.frame_rate.clone(),
                    original_index: Some(vid),
                }))
                .collect()
        } else {
            video_variants_for_task.iter().enumerate().map(|(idx, v)| VideoVariantInfo {
                url: String::new(),
                bandwidth: v.bandwidth,
                width: v.width,
                height: v.height,
                codecs: v.codecs.clone(),
                frame_rate: v.frame_rate.clone(),
                original_index: Some(idx),
            }).collect()
        };

        if is_multi_variant {
            for (vid, variant_outputs) in &all_variant_outputs {
                let prefix = format!("v{vid}");
                for (out_format, scheme, output) in variant_outputs {
                    if let Ok(rendered) = manifest::render_manifest(output.manifest_state()) {
                        let ext = out_format.manifest_extension();
                        let scheme_str = scheme.scheme_type_str();
                        let fmt_label = match out_format {
                            OutputFormat::Hls => "hls",
                            OutputFormat::Dash => "dash",
                        };
                        let out_dir = PathBuf::from(format!(
                            "sandbox/output/{cid}/{fmt_label}_{scheme_str}"
                        ));
                        let rewritten = rewrite_variant_manifest(
                            &rendered, &cid, *vid, *out_format, scheme, container_fmt,
                        );
                        let _ = std::fs::write(
                            out_dir.join(format!("{prefix}_video.{ext}")),
                            &rewritten,
                        );
                    }
                }
            }
        }
        video_outputs = all_variant_outputs.into_iter()
            .map(|(_, outputs)| outputs)
            .find(|v| !v.is_empty())
            .unwrap_or_default();

        // Update progressive manifest context with only successful variants
        // so the combined manifest doesn't reference failed variants.
        if is_multi_variant {
            if let Ok(mut ctx) = manifest_ctx.lock() {
                ctx.finalize_with_successful_variants(successful_variants.clone());
            }
        }

        let first_segment_ms = Some(pipeline_start.elapsed().as_millis() as u64);

        // ──────────────────────────────────────────────────────────────
        // Phase 2: Finalize — write final manifests with all tracks,
        // compute timing statistics, run validation.
        // ──────────────────────────────────────────────────────────────
        let pipeline_elapsed = pipeline_start.elapsed();
        let total_segments = segments_total_count as u32;

        let text_track_summary = if has_text {
            format!(" +{} text track(s)", text_source_tracks.len())
        } else {
            String::new()
        };
        eprintln!(
            "  Pipeline complete: {}/{} — {} output(s) in {:.1}s{}{}",
            cid, fmt_str, video_outputs.len(), pipeline_elapsed.as_secs_f64(),
            if has_audio { " +audio" } else { "" },
            text_track_summary,
        );

        let mut validation_results = Vec::new();

        // Write final combined manifests
        for (out_format, scheme, output) in &video_outputs {
            let scheme_str = scheme.scheme_type_str();
            let fmt_label = match out_format {
                OutputFormat::Hls => "hls",
                OutputFormat::Dash => "dash",
            };
            let out_dir = PathBuf::from(format!(
                "sandbox/output/{cid}/{fmt_label}_{scheme_str}"
            ));

            // For DASH multi-variant: build combined ManifestState with all variants
            // and per-Representation segment path prefixes.
            let video_rendered = if is_multi_variant && *out_format == OutputFormat::Dash {
                let mut combined_state = output.manifest_state().clone();
                combined_state.variants = build_dash_variant_infos(&successful_variants);
                manifest::render_manifest(&combined_state).ok()
            } else {
                manifest::render_manifest(output.manifest_state()).ok()
            };
            if let Some(video_rendered) = video_rendered {
                let ext = out_format.manifest_extension();
                let _ = std::fs::write(out_dir.join(format!("video.{ext}")), &video_rendered);

                // Write final audio manifest if available
                let final_audio_manifest = if let Some(ref audio_outs) = audio_outputs_final {
                    if let Some((_, _, ref audio_output)) = audio_outs.iter()
                        .find(|(f, s, _)| f == out_format && s == scheme)
                    {
                        let audio_rendered = manifest::render_manifest(audio_output.manifest_state()).ok();
                        if let Some(ref arm) = audio_rendered {
                            let rewritten = rewrite_audio_manifest(arm, &cid, *out_format, scheme, container_fmt);
                            let _ = std::fs::write(out_dir.join(format!("audio.{ext}")), &rewritten);
                        }
                        audio_rendered
                    } else {
                        None
                    }
                } else {
                    None
                };

                // Write final text manifests
                let mut text_manifest_infos: Vec<TextManifestInfo> = Vec::new();
                for (text_idx, text_outs) in text_outputs_final.iter().enumerate() {
                    if let Some((_, _, ref text_output)) = text_outs.iter()
                        .find(|(f, s, _)| f == out_format && s == scheme)
                    {
                        if let Ok(text_rendered) = manifest::render_manifest(text_output.manifest_state()) {
                            let rewritten = rewrite_text_manifest(
                                &text_rendered, &cid, text_idx, *out_format, scheme, container_fmt,
                            );
                            let text_prefix = format!("text_{text_idx}");
                            let _ = std::fs::write(out_dir.join(format!("{text_prefix}.{ext}")), &rewritten);

                            let track_info = &text_source_tracks[text_idx];
                            text_manifest_infos.push(TextManifestInfo {
                                index: text_idx,
                                name: track_info.name.clone(),
                                language: track_info.language.clone(),
                            });
                        }
                    }
                }

                // Also add raw VTT text tracks to the manifest info
                // (they don't have pipeline outputs but are still in text_source_tracks)
                for (text_idx, text_track) in text_source_tracks.iter().enumerate() {
                    if text_track.is_raw_vtt && !text_manifest_infos.iter().any(|t| t.index == text_idx) {
                        text_manifest_infos.push(TextManifestInfo {
                            index: text_idx,
                            name: text_track.name.clone(),
                            language: text_track.language.clone(),
                        });
                    }
                }
                // Sort by index to maintain order
                text_manifest_infos.sort_by_key(|t| t.index);

                // Build final combined manifest with all tracks
                if has_audio || has_text || is_multi_variant {
                    let combined = if let Some(ref raw_audio) = final_audio_manifest {
                        build_progressive_combined_manifest(
                            *out_format, &video_rendered, raw_audio,
                            &cid, scheme, container_fmt,
                            &text_manifest_infos,
                            &successful_variants,
                            true, // complete — final manifest
                        )
                    } else if !text_manifest_infos.is_empty() || is_multi_variant {
                        build_progressive_combined_manifest(
                            *out_format, &video_rendered, "",
                            &cid, scheme, container_fmt,
                            &text_manifest_infos,
                            &successful_variants,
                            true, // complete — final manifest
                        )
                    } else {
                        video_rendered.clone()
                    };
                    let final_manifest = if *out_format == OutputFormat::Dash {
                        fixup_dash_for_sandbox(&combined)
                    } else {
                        combined
                    };
                    let _ = std::fs::write(out_dir.join(format!("manifest.{ext}")), &final_manifest);
                } else {
                    // No separate audio/text — write video manifest as final
                    let final_manifest = if *out_format == OutputFormat::Dash {
                        fixup_dash_for_sandbox(&video_rendered)
                    } else {
                        video_rendered.clone()
                    };
                    let _ = std::fs::write(out_dir.join(format!("manifest.{ext}")), &final_manifest);
                }
            }

            // I-frame playlist (HLS only)
            if let Ok(Some(iframe_playlist)) = manifest::render_iframe_manifest(output.manifest_state()) {
                let _ = std::fs::write(out_dir.join("iframes.m3u8"), iframe_playlist);
            }

            eprintln!("  Wrote {} segments to {}", total_segments, out_dir.display());

            // Run compliance validation
            let validation = validate_output(&cid, *out_format, scheme_str, output);
            validation_results.push(validation);
        }

        // Validate audio output if present
        if let Some(ref audio_outs) = audio_outputs_final {
            for (out_format, scheme, audio_output) in audio_outs {
                let scheme_str = scheme.scheme_type_str();
                let mut audio_validation = validate_output(&cid, *out_format, scheme_str, audio_output);
                if let Some(obj) = audio_validation.as_object_mut() {
                    let label = obj.get("output").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    obj.insert("output".to_string(), serde_json::json!(format!("{label} (audio)")));
                }
                validation_results.push(audio_validation);
            }
        }

        // Calculate total output bytes (video + audio + text)
        let mut total_bytes: u64 = video_outputs
            .iter()
            .map(|(_, _, output)| {
                let init_bytes = output.init_segment_data().map(|d| d.len() as u64).unwrap_or(0);
                let seg_bytes: u64 = output
                    .manifest_state()
                    .segments
                    .iter()
                    .filter_map(|s| output.segment_data(s.number).map(|d| d.len() as u64))
                    .sum();
                init_bytes + seg_bytes
            })
            .sum();
        if let Some(ref audio_outs) = audio_outputs_final {
            total_bytes += audio_outs.iter().map(|(_, _, output)| {
                let init_bytes = output.init_segment_data().map(|d| d.len() as u64).unwrap_or(0);
                let seg_bytes: u64 = output
                    .manifest_state()
                    .segments
                    .iter()
                    .filter_map(|s| output.segment_data(s.number).map(|d| d.len() as u64))
                    .sum();
                init_bytes + seg_bytes
            }).sum::<u64>();
        }
        for text_outs in &text_outputs_final {
            total_bytes += text_outs.iter().map(|(_, _, output)| {
                let init_bytes = output.init_segment_data().map(|d| d.len() as u64).unwrap_or(0);
                let seg_bytes: u64 = output
                    .manifest_state()
                    .segments
                    .iter()
                    .filter_map(|s| output.segment_data(s.number).map(|d| d.len() as u64))
                    .sum();
                init_bytes + seg_bytes
            }).sum::<u64>();
        }

        // Estimate WASM cold start from binary size on disk
        let wasm_binary_size = std::fs::metadata("target/wasm32-wasip2/release/edgepack.wasm")
            .map(|m| m.len())
            .unwrap_or(628_000); // fallback: known ~628KB
        let cold_start_us = (wasm_binary_size as f64 / 500_000.0 * 500.0) as u64;

        // Write "Complete" state
        let skipped_json: serde_json::Value = if skipped_variants.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::to_value(&skipped_variants).unwrap_or(serde_json::Value::Null)
        };
        let complete_state = serde_json::json!({
            "state": "Complete",
            "segments_completed": total_segments,
            "segments_total": total_segments,
            "schemes": scheme_list,
            "validation": validation_results,
            "skipped_variants": skipped_json,
            "timing": {
                "total_pipeline_ms": pipeline_elapsed.as_millis() as u64,
                "first_segment_ms": first_segment_ms.unwrap_or(0),
                "per_segment_ms": [],
                "cold_start_us": cold_start_us,
                "wasm_binary_kb": wasm_binary_size / 1024,
                "total_segments": total_segments,
                "total_bytes": total_bytes,
                "avg_segment_ms": if total_segments > 0 {
                    pipeline_elapsed.as_millis() as f64 / total_segments as f64
                } else { 0.0 },
                "throughput_mbps": if pipeline_elapsed.as_secs_f64() > 0.0 {
                    (total_bytes as f64 * 8.0) / (pipeline_elapsed.as_secs_f64() * 1_000_000.0)
                } else { 0.0 },
            },
        });
        let _ = cache.set(
            &state_key,
            &serde_json::to_vec(&complete_state).unwrap(),
            3600,
        );
    });

    (
        StatusCode::OK,
        Json(RepackageResponse {
            content_id,
            format: fmt_str.into(),
            message: "repackaging started".into(),
            container_format: container_format_str,
        }),
    )
        .into_response()
}

async fn handle_status(
    State(_state): State<Arc<AppState>>,
    Path((content_id, format)): Path<(String, String)>,
) -> Response {
    let fmt = match format.as_str() {
        "hls" | "dash" => format.as_str(),
        _ => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "format must be 'hls' or 'dash'".into(),
                }),
            )
                .into_response();
        }
    };

    let cache = edgepack::cache::global_cache();
    let key = CacheKeys::job_state(&content_id, fmt);
    match cache.get(&key) {
        Ok(Some(data)) => match serde_json::from_slice::<serde_json::Value>(&data) {
            Ok(status) => {
                let state_str = status.get("state").and_then(|v| v.as_str()).unwrap_or("Unknown").to_string();
                let segments_completed = status.get("segments_completed").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let segments_total = status.get("segments_total").and_then(|v| v.as_u64()).map(|v| v as u32);
                let error = status.get("error").and_then(|v| v.as_str()).map(|s| s.to_string());
                let schemes = status.get("schemes").and_then(|v| {
                    v.as_array().map(|a| {
                        a.iter().filter_map(|s| s.as_str().map(String::from)).collect()
                    })
                });
                let validation = status.get("validation").cloned();
                let timing = status.get("timing").cloned();
                let playback_ready = status.get("playback_ready").and_then(|v| v.as_bool()).unwrap_or(false);
                let skipped_variants = status.get("skipped_variants").cloned();
                let output_dir = if state_str == "Complete" || playback_ready {
                    Some(format!("sandbox/output/{content_id}/{fmt}_*/"))
                } else {
                    None
                };
                (
                    StatusCode::OK,
                    Json(StatusResponse {
                        state: state_str,
                        segments_completed,
                        segments_total,
                        output_dir,
                        error,
                        schemes,
                        validation,
                        timing,
                        playback_ready,
                        skipped_variants,
                    }),
                )
                    .into_response()
            }
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: "corrupt job state".into(),
                }),
            )
                .into_response(),
        },
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "no job found".into(),
            }),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("cache error: {e}"),
            }),
        )
            .into_response(),
    }
}

async fn handle_output(
    Path((content_id, format_scheme, file)): Path<(String, String, String)>,
) -> Response {
    // format_scheme can be "hls_cenc", "dash_cbcs", etc.
    let valid = format_scheme.starts_with("hls") || format_scheme.starts_with("dash");
    if !valid {
        return (StatusCode::BAD_REQUEST, "invalid format").into_response();
    }

    let out_dir = PathBuf::from(format!("sandbox/output/{content_id}/{format_scheme}"));
    let file_path = out_dir.join(&file);

    // For bare "manifest" request, try both extensions
    let path = if file == "manifest" {
        let m3u8 = out_dir.join("manifest.m3u8");
        let mpd = out_dir.join("manifest.mpd");
        if m3u8.exists() {
            m3u8
        } else if mpd.exists() {
            mpd
        } else {
            return (StatusCode::NOT_FOUND, "manifest not found").into_response();
        }
    } else {
        file_path
    };

    // Determine content type from the resolved path
    let content_type = match path.extension().and_then(|e| e.to_str()) {
        Some("m3u8") => "application/vnd.apple.mpegurl",
        Some("mpd") => "application/dash+xml",
        Some("ts") => "video/mp2t",
        _ => "video/mp4",
    };

    match std::fs::read(&path) {
        Ok(data) => ([(axum::http::header::CONTENT_TYPE, content_type)], data).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

async fn handle_manifest_preview(
    Path((content_id, format_scheme)): Path<(String, String)>,
) -> Response {
    let valid = format_scheme.starts_with("hls") || format_scheme.starts_with("dash");
    if !valid {
        return (StatusCode::BAD_REQUEST, "invalid format").into_response();
    }

    let out_dir = PathBuf::from(format!("sandbox/output/{content_id}/{format_scheme}"));
    let m3u8 = out_dir.join("manifest.m3u8");
    let mpd = out_dir.join("manifest.mpd");
    let path = if m3u8.exists() {
        m3u8
    } else if mpd.exists() {
        mpd
    } else {
        return (StatusCode::NOT_FOUND, "manifest not found").into_response();
    };

    match std::fs::read_to_string(&path) {
        Ok(content) => (
            [(axum::http::header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            content,
        )
            .into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

// ─── JIT Source Config ───────────────────────────────────────────────────

#[derive(Deserialize)]
struct SourceConfigPayload {
    content_id: String,
    source_url: String,
    #[serde(default)]
    target_schemes: Vec<String>,
    #[serde(default = "default_container_format")]
    container_format: String,
}

async fn handle_source_config(
    State(_state): State<Arc<AppState>>,
    Json(payload): Json<SourceConfigPayload>,
) -> Response {
    if payload.content_id.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "content_id is required".into(),
            }),
        )
            .into_response();
    }

    if payload.source_url.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse {
                error: "source_url is required".into(),
            }),
        )
            .into_response();
    }

    // Parse target schemes
    let target_schemes = if payload.target_schemes.is_empty() {
        vec![EncryptionScheme::Cenc]
    } else {
        let mut schemes = Vec::with_capacity(payload.target_schemes.len());
        for s in &payload.target_schemes {
            let scheme = match s.as_str() {
                "cenc" => EncryptionScheme::Cenc,
                "cbcs" => EncryptionScheme::Cbcs,
                "none" => EncryptionScheme::None,
                _ => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(ErrorResponse {
                            error: format!("invalid target_scheme: {s}"),
                        }),
                    )
                        .into_response();
                }
            };
            schemes.push(scheme);
        }
        schemes
    };

    let container_format = match payload.container_format.as_str() {
        "cmaf" => edgepack::media::container::ContainerFormat::Cmaf,
        "fmp4" => edgepack::media::container::ContainerFormat::Fmp4,
        "iso" => edgepack::media::container::ContainerFormat::Iso,
        #[cfg(feature = "ts")]
        "ts" => edgepack::media::container::ContainerFormat::Ts,
        _ => edgepack::media::container::ContainerFormat::Cmaf,
    };

    let source_config = SourceConfig {
        source_url: payload.source_url,
        target_schemes,
        container_format,
    };

    let data = match serde_json::to_vec(&source_config) {
        Ok(d) => d,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("serialize error: {e}"),
                }),
            )
                .into_response();
        }
    };

    let cache = edgepack::cache::global_cache();
    let cache_key = CacheKeys::source_config(&payload.content_id);
    if let Err(e) = cache.set(&cache_key, &data, 172_800) {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ErrorResponse {
                error: format!("cache error: {e}"),
            }),
        )
            .into_response();
    }

    eprintln!(
        "  Source config stored for {} -> {}",
        payload.content_id,
        source_config.source_url
    );

    (
        StatusCode::OK,
        Json(serde_json::json!({
            "status": "ok",
            "content_id": payload.content_id,
        })),
    )
        .into_response()
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn generate_content_id(source_url: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    source_url.hash(&mut hasher);
    let hash = hasher.finish();
    format!("sb-{hash:016x}")
}

fn is_local_path(s: &str) -> bool {
    s.starts_with('/')
        || s.starts_with("./")
        || s.starts_with("../")
        || (!s.contains("://") && !s.starts_with("http"))
}

async fn start_local_file_server(path: &str) -> Result<String, String> {
    let abs_path = std::fs::canonicalize(path).map_err(|e| format!("path not found: {e}"))?;
    let parent = abs_path
        .parent()
        .ok_or("cannot determine parent directory")?
        .to_path_buf();
    let filename = abs_path
        .file_name()
        .ok_or("cannot determine filename")?
        .to_str()
        .ok_or("filename is not valid UTF-8")?
        .to_string();

    let serve_dir = tower_http::services::ServeDir::new(&parent);
    let app = Router::new().fallback_service(serve_dir);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind failed: {e}"))?;
    let port = listener
        .local_addr()
        .map_err(|e| format!("addr error: {e}"))?
        .port();

    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let url = format!("http://127.0.0.1:{port}/{filename}");
    eprintln!("  Serving local files from {} on port {port}", parent.display());
    Ok(url)
}

/// Resolve a source URL to separate video and audio sources.
///
/// - **HLS master playlists** (multivariant): picks the highest-bandwidth variant
///   as the video source and extracts the first `#EXT-X-MEDIA:TYPE=AUDIO` URI.
/// - **DASH MPDs**: parses for separate audio `<AdaptationSet>` elements with
///   `mimeType="audio/mp4"`. Builds a synthetic audio-only MPD, writes it to a
///   temp file, and serves it via a local HTTP server so the audio pipeline can
///   fetch it independently.
/// - **Other URLs** (media playlists, non-manifest): returned unchanged with no audio.
async fn resolve_master_playlist(url: &str) -> Result<ResolvedSource, String> {
    let lower = url.to_lowercase();

    // DASH MPD — check for separate audio/text AdaptationSets
    if lower.contains(".mpd") {
        return resolve_dash_tracks(url).await;
    }

    // Only attempt HLS resolution for .m3u8 URLs
    if !lower.contains(".m3u8") {
        return Ok(ResolvedSource { video_url: url.to_string(), audio_url: None, text_tracks: vec![], video_variants: vec![], skipped_variants: vec![] });
    }

    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("fetch failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("read body failed: {e}"))?;

    // Check if this is a master playlist (contains #EXT-X-STREAM-INF)
    if !body.contains("#EXT-X-STREAM-INF") {
        // Already a media playlist — use as-is
        return Ok(ResolvedSource { video_url: url.to_string(), audio_url: None, text_tracks: vec![], video_variants: vec![], skipped_variants: vec![] });
    }

    eprintln!("  Detected HLS master playlist — resolving to media playlist...");

    // Use the core parser to extract all variants and renditions
    let master_info = edgepack::manifest::hls_input::parse_hls_master_playlist(&body, url)
        .map_err(|e| format!("parse HLS master playlist: {e}"))?;

    // Build video variants from parsed data
    let video_variants: Vec<VideoVariantInfo> = master_info.variants.iter()
        .zip(master_info.variant_uris.iter())
        .enumerate()
        .map(|(idx, (variant, uri))| VideoVariantInfo {
            url: uri.clone(),
            bandwidth: variant.bandwidth,
            width: variant.width,
            height: variant.height,
            codecs: variant.codecs.clone(),
            frame_rate: variant.frame_rate.clone(),
            original_index: Some(idx),
        })
        .collect();

    // Pick the highest-bandwidth variant as the primary video URL (backward compat)
    let (video_url, best_bandwidth) = video_variants.iter()
        .max_by_key(|v| v.bandwidth)
        .map(|v| (v.url.clone(), v.bandwidth))
        .ok_or("no variant streams found in master playlist")?;

    // Extract audio URL from the first audio rendition with a URI
    let audio_url = master_info.audio_renditions.iter()
        .find_map(|r| r.uri.clone());

    // Extract text/subtitle tracks from renditions.
    // HLS subtitle playlists typically reference raw WebVTT segments (not fMP4),
    // so we mark them as is_raw_vtt = true and store the playlist URL.
    // The text track handler will fetch the playlist, parse segments, and concatenate.
    let text_tracks_resolved: Vec<TextTrackInfo> = master_info.subtitle_renditions.iter()
        .filter_map(|r| {
            r.uri.as_ref().map(|uri| TextTrackInfo {
                url: uri.clone(),
                name: r.name.clone(),
                language: r.language.clone(),
                // HLS subtitle playlists are WebVTT-based (not fMP4) unless
                // the playlist URL explicitly indicates fMP4 (extremely rare).
                // Mark as raw VTT so we download + concatenate instead of pipeline.
                is_raw_vtt: true,
            })
        })
        .collect();

    if video_variants.len() > 1 {
        eprintln!("  Detected {} HLS variants:", video_variants.len());
        for (i, v) in video_variants.iter().enumerate() {
            eprintln!(
                "    v{i}: {}x{} @ {} bps (codecs={:?})",
                v.width.unwrap_or(0), v.height.unwrap_or(0), v.bandwidth, v.codecs
            );
        }
    }
    eprintln!(
        "  Selected primary variant: {} (bandwidth: {})",
        video_url, best_bandwidth
    );
    if let Some(ref audio) = audio_url {
        eprintln!("  Audio rendition: {}", audio);
    } else {
        eprintln!("  Audio: muxed with video (no separate rendition)");
    }
    if !text_tracks_resolved.is_empty() {
        eprintln!("  Text tracks: {} track(s)", text_tracks_resolved.len());
        for tt in &text_tracks_resolved {
            eprintln!("    - {} (lang={:?})", tt.name, tt.language);
        }
    }

    Ok(ResolvedSource { video_url, audio_url, text_tracks: text_tracks_resolved, video_variants, skipped_variants: vec![] })
}

/// Resolve audio and text tracks from a DASH MPD.
///
/// If the MPD has a separate audio `<AdaptationSet>` with `mimeType="audio/mp4"`,
/// builds a synthetic audio-only DASH MPD, writes it to a temp file, serves it
/// via a local HTTP server, and returns the URL as the audio source.
/// Also extracts text `<AdaptationSet>` elements for subtitle tracks.
async fn resolve_dash_tracks(url: &str) -> Result<ResolvedSource, String> {
    let resp = reqwest::get(url)
        .await
        .map_err(|e| format!("fetch DASH MPD failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    let body = resp
        .text()
        .await
        .map_err(|e| format!("read DASH MPD body failed: {e}"))?;

    if !body.contains("<MPD") {
        // Not a DASH MPD — return as-is
        return Ok(ResolvedSource { video_url: url.to_string(), audio_url: None, text_tracks: vec![], video_variants: vec![], skipped_variants: vec![] });
    }

    // URL base for resolving relative BaseURL references
    let base = url.rfind('/').map(|i| &url[..=i]).unwrap_or(url);

    // Extract MPD-level attributes for the synthetic MPD
    let mpd_type = extract_xml_attr(&body, "MPD", "type").unwrap_or_else(|| "static".to_string());
    let mpd_duration = extract_xml_attr(&body, "MPD", "mediaPresentationDuration")
        .unwrap_or_else(|| "PT0S".to_string());
    let mpd_min_buffer = extract_xml_attr(&body, "MPD", "minBufferTime")
        .unwrap_or_else(|| "PT2S".to_string());
    let mpd_profiles = extract_xml_attr(&body, "MPD", "profiles")
        .unwrap_or_else(|| "urn:mpeg:dash:profile:isoff-on-demand:2011".to_string());

    // ── Extract all video Representations ───────────────────────────────
    // For each video AdaptationSet, extract individual Representations and build
    // synthetic single-Representation MPDs so each can be processed independently.
    // Non-ISOBMFF containers (e.g., WebM) are detected and skipped at this stage
    // rather than allowed to fail during pipeline processing.
    let mut video_variants: Vec<VideoVariantInfo> = Vec::new();
    let mut skipped_variants_list: Vec<SkippedVariant> = Vec::new();
    {
        let mut vs = 0;
        while let Some(as_start) = body[vs..].find("<AdaptationSet") {
            let abs_start = vs + as_start;
            let as_end = if let Some(end) = body[abs_start..].find("</AdaptationSet>") {
                abs_start + end + "</AdaptationSet>".len()
            } else {
                vs = abs_start + 1;
                continue;
            };
            let as_block = &body[abs_start..as_end];

            // Detect video AdaptationSet (by contentType or mimeType on any element)
            let is_video = as_block.contains("contentType=\"video\"")
                || as_block.contains("contentType='video'")
                || as_block.contains("mimeType=\"video/mp4\"")
                || as_block.contains("mimeType='video/mp4'")
                || as_block.contains("mimeType=\"video/webm\"")
                || as_block.contains("mimeType='video/webm'");

            if is_video {
                // Check AdaptationSet-level mimeType (may be inherited by all Representations)
                let as_open_end = as_block.find('>').unwrap_or(as_block.len());
                let as_open_tag = &as_block[..=as_open_end];
                let as_mime = extract_xml_attr(as_open_tag, "AdaptationSet", "mimeType");

                // Find all <Representation> elements within this AdaptationSet
                let mut rep_search = 0;
                while let Some(rep_start) = as_block[rep_search..].find("<Representation") {
                    let rep_abs = rep_search + rep_start;
                    // Find end of this Representation (could be </Representation> or self-closing)
                    let rep_end = if let Some(end) = as_block[rep_abs..].find("</Representation>") {
                        rep_abs + end + "</Representation>".len()
                    } else if let Some(end) = as_block[rep_abs..].find("/>") {
                        rep_abs + end + 2
                    } else {
                        rep_search = rep_abs + 1;
                        continue;
                    };
                    let rep_block = &as_block[rep_abs..rep_end];

                    // Extract variant metadata from Representation attributes
                    let bandwidth = extract_xml_attr(rep_block, "Representation", "bandwidth")
                        .and_then(|v| v.parse::<u64>().ok())
                        .unwrap_or(0);
                    let width = extract_xml_attr(rep_block, "Representation", "width")
                        .and_then(|v| v.parse::<u32>().ok());
                    let height = extract_xml_attr(rep_block, "Representation", "height")
                        .and_then(|v| v.parse::<u32>().ok());
                    let codecs = extract_xml_attr(rep_block, "Representation", "codecs");
                    let frame_rate = extract_xml_attr(rep_block, "Representation", "frameRate");

                    // Determine effective mimeType: Representation-level overrides AdaptationSet-level
                    let rep_mime = extract_xml_attr(rep_block, "Representation", "mimeType");
                    let effective_mime = rep_mime.as_deref().or(as_mime.as_deref());

                    // Skip non-ISOBMFF containers (WebM, etc.) — edgepack only processes ISOBMFF/CMAF
                    let is_isobmff = match effective_mime {
                        Some(m) => m == "video/mp4" || m == "video/iso.segment",
                        // If no mimeType, assume ISOBMFF (the common case for DASH)
                        None => true,
                    };

                    if !is_isobmff {
                        let res_label = match (width, height) {
                            (Some(w), Some(h)) => format!("{w}x{h}"),
                            _ => "unknown".to_string(),
                        };
                        let codec_label = codecs.as_deref().unwrap_or("unknown");
                        let mime_label = effective_mime.unwrap_or("unknown");
                        eprintln!(
                            "  ⚠ Skipping Representation: {res_label} @ {bandwidth} bps \
                             (codecs={codec_label}, mimeType={mime_label}) — \
                             non-ISOBMFF container not supported"
                        );
                        skipped_variants_list.push(SkippedVariant {
                            reason: format!(
                                "Container '{mime_label}' not supported — edgepack processes ISOBMFF (video/mp4) only"
                            ),
                            bandwidth,
                            width,
                            height,
                            codecs,
                            mime_type: effective_mime.map(|s| s.to_string()),
                        });
                        rep_search = rep_end;
                        continue;
                    }

                    // Resolve BaseURL references in the Representation to absolute URLs
                    let mut resolved_rep = rep_block.to_string();
                    let mut bu_search = 0;
                    while let Some(bu_start) = resolved_rep[bu_search..].find("<BaseURL>") {
                        let tag_start = bu_search + bu_start;
                        let value_start = tag_start + "<BaseURL>".len();
                        if let Some(bu_end) = resolved_rep[value_start..].find("</BaseURL>") {
                            let value_end = value_start + bu_end;
                            let relative_url = resolved_rep[value_start..value_end].to_string();
                            if !relative_url.starts_with("http://") && !relative_url.starts_with("https://") {
                                let absolute_url = format!("{base}{relative_url}");
                                resolved_rep = format!(
                                    "{}{}{}",
                                    &resolved_rep[..value_start],
                                    absolute_url,
                                    &resolved_rep[value_end..],
                                );
                            }
                            bu_search = value_start + 1;
                        } else {
                            break;
                        }
                    }

                    // Also check for any shared content outside Representations that
                    // should be included (e.g., SegmentTemplate at AdaptationSet level).
                    // Extract content between the opening tag and first <Representation>
                    let shared_content = if rep_start > as_open_end + 1 - 0 {
                        let shared_start = as_open_end + 1;
                        let first_rep_in_block = as_block.find("<Representation").unwrap_or(shared_start);
                        let shared = as_block[shared_start..first_rep_in_block].trim();
                        if shared.is_empty() { String::new() } else { format!("    {shared}\n") }
                    } else {
                        String::new()
                    };

                    // Build a synthetic single-Representation video MPD
                    let synthetic_video_mpd = format!(
                        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="{mpd_profiles}" minBufferTime="{mpd_min_buffer}" type="{mpd_type}" mediaPresentationDuration="{mpd_duration}">
  <BaseURL>{base}</BaseURL>
  <Period id="0">
    {as_open_tag}
{shared_content}      {resolved_rep}
    </AdaptationSet>
  </Period>
</MPD>"#
                    );

                    // Write synthetic MPD to temp file and serve it
                    let vid = video_variants.len();
                    let tmp_dir = std::env::temp_dir().join("edgepack_video");
                    let _ = std::fs::create_dir_all(&tmp_dir);
                    let file_hash = {
                        use std::collections::hash_map::DefaultHasher;
                        use std::hash::{Hash, Hasher};
                        let mut hasher = DefaultHasher::new();
                        format!("{url}_video_{vid}").hash(&mut hasher);
                        hasher.finish()
                    };
                    let tmp_path = tmp_dir.join(format!("video_{vid}_{file_hash:016x}.mpd"));
                    if std::fs::write(&tmp_path, &synthetic_video_mpd).is_ok() {
                        if let Ok(video_url) = start_local_file_server(
                            tmp_path.to_str().unwrap_or("")
                        ).await {
                            video_variants.push(VideoVariantInfo {
                                url: video_url,
                                bandwidth,
                                width,
                                height,
                                codecs,
                                frame_rate,
                                original_index: Some(vid),
                            });
                        }
                    }

                    rep_search = rep_end;
                }
            }
            vs = as_end;
        }
    }

    // Set video_url to highest-bandwidth variant for backward compatibility
    let video_url_resolved = if let Some(best) = video_variants.iter().max_by_key(|v| v.bandwidth) {
        best.url.clone()
    } else {
        url.to_string()
    };

    if video_variants.len() > 1 {
        eprintln!("  Detected {} DASH video Representations (ISOBMFF):", video_variants.len());
        for (i, v) in video_variants.iter().enumerate() {
            eprintln!(
                "    v{i}: {}x{} @ {} bps (codecs={:?})",
                v.width.unwrap_or(0), v.height.unwrap_or(0), v.bandwidth, v.codecs
            );
        }
    } else if video_variants.len() == 1 {
        eprintln!("  Single DASH video Representation: {} bps", video_variants[0].bandwidth);
    }

    if !skipped_variants_list.is_empty() {
        eprintln!(
            "  ⚠ Skipped {} non-ISOBMFF Representation(s):",
            skipped_variants_list.len()
        );
        for sv in &skipped_variants_list {
            eprintln!(
                "    - {}x{} @ {} bps (codecs={:?}, mime={:?}): {}",
                sv.width.unwrap_or(0),
                sv.height.unwrap_or(0),
                sv.bandwidth,
                sv.codecs,
                sv.mime_type,
                sv.reason,
            );
        }
    }

    // Find the first audio AdaptationSet with mimeType="audio/mp4"
    // We look for <AdaptationSet ... contentType="audio" ...> blocks that contain audio/mp4
    let mut audio_adaptation_set: Option<String> = None;
    let mut search_start = 0;

    while let Some(as_start) = body[search_start..].find("<AdaptationSet") {
        let abs_start = search_start + as_start;
        // Find the end of this AdaptationSet (could be </AdaptationSet> or self-closing)
        let as_end = if let Some(end) = body[abs_start..].find("</AdaptationSet>") {
            abs_start + end + "</AdaptationSet>".len()
        } else {
            // Self-closing or malformed — skip
            search_start = abs_start + 1;
            continue;
        };

        let as_block = &body[abs_start..as_end];

        // Check if this is an audio AdaptationSet with mp4 content.
        // Accept if EITHER:
        //   - contentType="audio" is present (on AdaptationSet or Representation), OR
        //   - mimeType="audio/mp4" is present (common on Representations)
        // BUT exclude non-mp4 audio (e.g. audio/webm).
        let has_content_type_audio = as_block.contains("contentType=\"audio\"")
            || as_block.contains("contentType='audio'");
        let has_mime_audio_mp4 = as_block.contains("mimeType=\"audio/mp4\"")
            || as_block.contains("mimeType='audio/mp4'");
        let has_mime_video = as_block.contains("contentType=\"video\"")
            || as_block.contains("contentType='video'")
            || as_block.contains("mimeType=\"video/");
        let has_non_mp4_audio = as_block.contains("mimeType=\"audio/webm\"")
            || as_block.contains("mimeType='audio/webm'");

        // Match: (contentType=audio OR mimeType=audio/mp4) AND NOT (video or webm audio)
        let is_audio_mp4 = (has_content_type_audio || has_mime_audio_mp4)
            && !has_mime_video
            && !has_non_mp4_audio;

        if is_audio_mp4 {
            // Resolve relative BaseURL references to absolute URLs
            let mut resolved_block = as_block.to_string();
            // Find <BaseURL>relative_path</BaseURL> and make absolute
            let mut base_url_search = 0;
            while let Some(bu_start) = resolved_block[base_url_search..].find("<BaseURL>") {
                let tag_start = base_url_search + bu_start; // position of '<' in <BaseURL>
                let value_start = tag_start + "<BaseURL>".len(); // position after '>'
                if let Some(bu_end) = resolved_block[value_start..].find("</BaseURL>") {
                    let value_end = value_start + bu_end;
                    let relative_url = resolved_block[value_start..value_end].to_string();
                    if !relative_url.starts_with("http://") && !relative_url.starts_with("https://") {
                        let absolute_url = format!("{base}{relative_url}");
                        // Replace just the URL value between <BaseURL> and </BaseURL>
                        resolved_block = format!(
                            "{}{}{}",
                            &resolved_block[..value_start],
                            absolute_url,
                            &resolved_block[value_end..]
                        );
                    }
                    base_url_search = value_start + 1;
                } else {
                    break;
                }
            }

            audio_adaptation_set = Some(resolved_block);
            break;
        }

        search_start = as_end;
    }

    // Also find text AdaptationSets (contentType="text" or mimeType containing ttml/wvtt/text)
    let mut text_tracks_found: Vec<TextTrackInfo> = Vec::new();
    let mut text_search_start = 0;
    while let Some(as_start) = body[text_search_start..].find("<AdaptationSet") {
        let abs_start = text_search_start + as_start;
        let as_end = if let Some(end) = body[abs_start..].find("</AdaptationSet>") {
            abs_start + end + "</AdaptationSet>".len()
        } else {
            text_search_start = abs_start + 1;
            continue;
        };
        let as_block = &body[abs_start..as_end];

        // Detect raw WebVTT (mimeType="text/vtt") — not wrapped in fMP4
        let is_raw_vtt = as_block.contains("mimeType=\"text/vtt\"")
            || as_block.contains("mimeType='text/vtt'");

        let is_text = is_raw_vtt
            || as_block.contains("contentType=\"text\"")
            || as_block.contains("contentType='text'")
            || as_block.contains("mimeType=\"application/ttml+xml\"")
            || (as_block.contains("mimeType=\"application/mp4\"")
                && (as_block.contains("codecs=\"stpp") || as_block.contains("codecs=\"wvtt")));

        if is_text {
            let lang = extract_xml_attr(as_block, "AdaptationSet", "lang");
            let name = lang.clone().unwrap_or_else(|| "subtitles".to_string());

            if is_raw_vtt {
                // Raw WebVTT: extract the BaseURL content (the .vtt file URL) directly.
                // No synthetic MPD needed — just store the URL for pass-through.
                if let Some(bu_start) = as_block.find("<BaseURL>") {
                    let value_start = bu_start + "<BaseURL>".len();
                    if let Some(bu_end) = as_block[value_start..].find("</BaseURL>") {
                        let vtt_url_raw = &as_block[value_start..value_start + bu_end];
                        let vtt_url = if vtt_url_raw.starts_with("http://") || vtt_url_raw.starts_with("https://") {
                            vtt_url_raw.to_string()
                        } else {
                            format!("{base}{vtt_url_raw}")
                        };
                        text_tracks_found.push(TextTrackInfo {
                            url: vtt_url,
                            name,
                            language: lang,
                            is_raw_vtt: true,
                        });
                    }
                }
            } else {
                // fMP4-wrapped text: build a synthetic text-only MPD
                let mut resolved_block = as_block.to_string();
                let mut bu_search = 0;
                while let Some(bu_start) = resolved_block[bu_search..].find("<BaseURL>") {
                    let tag_start = bu_search + bu_start;
                    let value_start = tag_start + "<BaseURL>".len();
                    if let Some(bu_end) = resolved_block[value_start..].find("</BaseURL>") {
                        let value_end = value_start + bu_end;
                        let relative_url = resolved_block[value_start..value_end].to_string();
                        if !relative_url.starts_with("http://") && !relative_url.starts_with("https://") {
                            let absolute_url = format!("{base}{relative_url}");
                            resolved_block = format!(
                                "{}{}{}",
                                &resolved_block[..value_start],
                                absolute_url,
                                &resolved_block[value_end..],
                            );
                        }
                        bu_search = value_start + 1;
                    } else {
                        break;
                    }
                }

                let text_idx = text_tracks_found.len();
                let synthetic_text_mpd = format!(
                    r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="{mpd_profiles}" minBufferTime="{mpd_min_buffer}" type="{mpd_type}" mediaPresentationDuration="{mpd_duration}">
  <BaseURL>{base}</BaseURL>
  <Period id="0">
    {resolved_block}
  </Period>
</MPD>"#
                );

                let tmp_dir = std::env::temp_dir().join("edgepack_text");
                let _ = std::fs::create_dir_all(&tmp_dir);
                let file_hash = {
                    use std::collections::hash_map::DefaultHasher;
                    use std::hash::{Hash, Hasher};
                    let mut hasher = DefaultHasher::new();
                    format!("{url}_text_{text_idx}").hash(&mut hasher);
                    hasher.finish()
                };
                let tmp_path = tmp_dir.join(format!("text_{text_idx}_{file_hash:016x}.mpd"));
                if std::fs::write(&tmp_path, &synthetic_text_mpd).is_ok() {
                    if let Ok(text_url) = start_local_file_server(
                        tmp_path.to_str().unwrap_or("")
                    ).await {
                        text_tracks_found.push(TextTrackInfo { url: text_url, name, language: lang, is_raw_vtt: false });
                    }
                }
            }
        }
        text_search_start = as_end;
    }

    // Handle audio
    let audio_url = if let Some(audio_as) = audio_adaptation_set {
        eprintln!("  Detected DASH MPD with separate audio AdaptationSet");

        // Build a synthetic audio-only DASH MPD
        let synthetic_mpd = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" profiles="{mpd_profiles}" minBufferTime="{mpd_min_buffer}" type="{mpd_type}" mediaPresentationDuration="{mpd_duration}">
  <BaseURL>{base}</BaseURL>
  <Period id="0">
    {audio_as}
  </Period>
</MPD>"#
        );

        let tmp_dir = std::env::temp_dir().join("edgepack_audio");
        std::fs::create_dir_all(&tmp_dir)
            .map_err(|e| format!("create temp dir: {e}"))?;

        let file_hash = {
            use std::collections::hash_map::DefaultHasher;
            use std::hash::{Hash, Hasher};
            let mut hasher = DefaultHasher::new();
            url.hash(&mut hasher);
            hasher.finish()
        };
        let tmp_path = tmp_dir.join(format!("audio_{file_hash:016x}.mpd"));
        std::fs::write(&tmp_path, &synthetic_mpd)
            .map_err(|e| format!("write synthetic audio MPD: {e}"))?;

        let audio_url = start_local_file_server(tmp_path.to_str().ok_or("invalid temp path")?)
            .await?;

        eprintln!("  Audio DASH MPD served at: {}", audio_url);
        Some(audio_url)
    } else {
        eprintln!("  DASH MPD: no separate audio/mp4 AdaptationSet found");
        None
    };

    if !text_tracks_found.is_empty() {
        eprintln!("  DASH MPD: {} text track(s) found", text_tracks_found.len());
    }

    Ok(ResolvedSource {
        video_url: video_url_resolved,
        audio_url,
        text_tracks: text_tracks_found,
        video_variants,
        skipped_variants: skipped_variants_list,
    })
}

/// Extract an XML attribute value from the first occurrence of a given element.
/// Uses word-boundary matching to avoid substring collisions (e.g. `width` inside `bandwidth`).
/// Extract the full content of an XML element (opening tag through closing tag) as a string.
/// Returns the inner block e.g. for `<SegmentTimeline>...\n</SegmentTimeline>`
/// returns `<SegmentTimeline>...\n</SegmentTimeline>`.
fn extract_xml_block(xml: &str, element: &str) -> Option<String> {
    let open_tag = format!("<{element}");
    let close_tag = format!("</{element}>");
    let start = xml.find(&open_tag)?;
    let end = xml[start..].find(&close_tag)?;
    Some(xml[start..start + end + close_tag.len()].to_string())
}

fn extract_xml_attr(xml: &str, element: &str, attr: &str) -> Option<String> {
    let tag_start = xml.find(&format!("<{element}"))?;
    let tag_content = &xml[tag_start..];
    let tag_end = tag_content.find('>')?;
    let tag = &tag_content[..=tag_end];

    // Try both quote styles, with leading space to ensure word boundary
    // (prevents "width" matching inside "bandwidth")
    for quote in ['"', '\''] {
        let pattern = format!(" {attr}={quote}");
        if let Some(attr_start) = tag.find(&pattern) {
            let value_start = attr_start + pattern.len();
            if let Some(value_end) = tag[value_start..].find(quote) {
                return Some(tag[value_start..value_start + value_end].to_string());
            }
        }
    }
    None
}

/// Merge audio AdaptationSet(s) from an audio-only MPD into a video MPD.
///
/// Extracts `<AdaptationSet contentType="audio" ...>` elements from the audio MPD,
/// rewrites segment URIs to use `audio_segment_` / `audio_init` prefixes, fixes
/// segment extensions (e.g. `.cmfv` → `.cmfa` for CMAF), and injects them into
/// the video MPD's `<Period>` element.
fn merge_dash_audio(
    video_mpd: &str,
    audio_mpd: &str,
    audio_base_url: &str,
    container_format: edgepack::media::container::ContainerFormat,
) -> String {
    // Extract audio AdaptationSet(s) from audio MPD
    let mut audio_adaptation_sets = Vec::new();
    let mut in_adaptation = false;
    let mut depth = 0i32;
    let mut current_block = String::new();

    for line in audio_mpd.lines() {
        let trimmed = line.trim();
        if trimmed.contains("<AdaptationSet") {
            in_adaptation = true;
            depth = 1;
            current_block.clear();
            current_block.push_str(line);
            current_block.push('\n');
            if trimmed.contains("/>") {
                // Self-closing — unlikely for AdaptationSet but handle it
                audio_adaptation_sets.push(current_block.clone());
                in_adaptation = false;
            }
        } else if in_adaptation {
            current_block.push_str(line);
            current_block.push('\n');
            if trimmed.contains("<AdaptationSet") {
                depth += 1;
            }
            if trimmed.contains("</AdaptationSet") {
                depth -= 1;
                if depth == 0 {
                    audio_adaptation_sets.push(current_block.clone());
                    in_adaptation = false;
                }
            }
        }
    }

    if audio_adaptation_sets.is_empty() {
        return video_mpd.to_string();
    }

    // Rewrite segment names in audio AdaptationSets to use audio_ prefix.
    // Strip the audio pipeline's base_url so paths are relative to the output dir.
    // Also fix segment extensions (e.g. CMAF uses .cmfv for video but .cmfa for audio).
    let video_ext = container_format.video_segment_extension();
    let audio_ext = container_format.audio_segment_extension();
    let rewritten_audio: Vec<String> = audio_adaptation_sets
        .iter()
        .map(|block| {
            let mut rewritten = block
                .replace(audio_base_url, "")
                // Force contentType to audio (in case it was detected differently)
                .replace("contentType=\"video\"", "contentType=\"audio\"")
                .replace("mimeType=\"video/mp4\"", "mimeType=\"audio/mp4\"");
            // Only add audio_ prefix if not already present (the audio pipeline
            // may already write audio_init.mp4 / audio_segment_N filenames).
            if !rewritten.contains("audio_init.mp4") {
                rewritten = rewritten.replace("init.mp4", "audio_init.mp4");
            }
            if !rewritten.contains("audio_segment_") {
                rewritten = rewritten.replace("segment_$Number$", "audio_segment_$Number$");
                rewritten = rewritten.replace("segment_$Number%", "audio_segment_$Number%");
            }
            // Fix segment extensions for formats where video ≠ audio extension (CMAF)
            if video_ext != audio_ext {
                rewritten = rewritten.replace(video_ext, audio_ext);
            }
            rewritten
        })
        .collect();

    // Inject audio AdaptationSets into the video MPD before </Period>
    let audio_block = rewritten_audio.join("");
    if let Some(pos) = video_mpd.find("</Period>") {
        let mut merged = String::with_capacity(video_mpd.len() + audio_block.len());
        merged.push_str(&video_mpd[..pos]);
        merged.push_str(&audio_block);
        merged.push_str(&video_mpd[pos..]);
        merged
    } else {
        // Fallback: just append (shouldn't happen for valid MPD)
        video_mpd.to_string()
    }
}

/// Build a multi-variant DASH MPD from a single-variant base MPD.
///
/// Takes the first variant's MPD (with SegmentTimeline) and replaces its single
/// Representation with multiple Representations, each with per-variant
/// SegmentTemplate init/media paths (v{vid}_ prefix).
fn build_dash_multi_variant_mpd(
    base_mpd: &str,
    video_variants: &[VideoVariantInfo],
    container_format: edgepack::media::container::ContainerFormat,
) -> String {
    // Extract the SegmentTimeline from the base MPD
    let timeline_block = extract_xml_block(base_mpd, "SegmentTimeline");
    let timescale = extract_xml_attr(base_mpd, "SegmentTemplate", "timescale")
        .unwrap_or_else(|| "1000".to_string());

    if timeline_block.is_none() {
        // Fallback: return base as-is if no SegmentTimeline
        return base_mpd.to_string();
    }
    let timeline = timeline_block.unwrap();
    let seg_ext = container_format.video_segment_extension();

    // Build per-Representation blocks with variant-specific SegmentTemplate
    let mut representations = String::new();
    for variant in video_variants {
        let file_vid = variant.original_index.unwrap_or(0);
        let prefix = format!("v{file_vid}_");
        // Only include video codec (strip audio codec if present)
        let video_codec = variant.codecs.as_deref()
            .map(|c| c.split(',').next().unwrap_or(c))
            .unwrap_or("avc1.64001f");

        representations.push_str(&format!(
            "      <Representation id=\"v{file_vid}\" bandwidth=\"{}\"",
            variant.bandwidth
        ));
        representations.push_str(&format!(" codecs=\"{video_codec}\""));
        if let (Some(w), Some(h)) = (variant.width, variant.height) {
            representations.push_str(&format!(" width=\"{w}\" height=\"{h}\""));
        }
        if let Some(ref fr) = variant.frame_rate {
            // Parse frame rate string to float for DASH
            let fps_str = if let Some((n, d)) = fr.split_once('/') {
                let n: f64 = n.parse().unwrap_or(30.0);
                let d: f64 = d.parse().unwrap_or(1.0);
                if d > 0.0 { format!("{:.3}", n / d) } else { "30".to_string() }
            } else {
                fr.clone()
            };
            representations.push_str(&format!(" frameRate=\"{fps_str}\""));
        }
        representations.push_str(">\n");
        representations.push_str(&format!(
            "        <SegmentTemplate timescale=\"{timescale}\" initialization=\"{prefix}init.mp4\" media=\"{prefix}segment_$Number${seg_ext}\" startNumber=\"0\">\n"
        ));
        representations.push_str(&format!("          {timeline}\n"));
        representations.push_str("        </SegmentTemplate>\n");
        representations.push_str("      </Representation>\n");
    }

    // Replace the existing AdaptationSet content with multi-variant Representations.
    // Find the video AdaptationSet and replace its content.
    let mut result = String::new();
    let mut skip_until_close = false;
    let mut found_close = false;

    for line in base_mpd.lines() {
        let trimmed = line.trim();
        if trimmed.contains("<AdaptationSet") && trimmed.contains("contentType=\"video\"") {
            result.push_str(line);
            result.push('\n');
            // Inject all Representations
            result.push_str(&representations);
            skip_until_close = true;
            continue;
        }
        if skip_until_close {
            if trimmed.contains("</AdaptationSet") {
                result.push_str(line);
                result.push('\n');
                skip_until_close = false;
                found_close = true;
            }
            // Skip original Representation/SegmentTemplate content
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }

    if !found_close {
        // Fallback: return base if we couldn't find the AdaptationSet structure
        return base_mpd.to_string();
    }

    result
}

/// Merge a text track into a DASH MPD as a subtitle AdaptationSet.
fn merge_dash_text_track(
    mpd: &str,
    text_track: &TextManifestInfo,
    _container_format: edgepack::media::container::ContainerFormat,
) -> String {
    let lang_attr = text_track.language.as_ref()
        .map(|l| format!(" lang=\"{l}\""))
        .unwrap_or_default();

    // Build a simple text AdaptationSet referencing the VTT file
    let text_as = format!(
        "    <AdaptationSet contentType=\"text\" mimeType=\"text/vtt\" segmentAlignment=\"true\"{lang_attr}>\n\
         \x20     <Representation id=\"text_{}\" bandwidth=\"1000\">\n\
         \x20       <BaseURL>text_{}.vtt</BaseURL>\n\
         \x20     </Representation>\n\
         \x20   </AdaptationSet>\n",
        text_track.index, text_track.index,
    );

    // Inject before </Period>
    if let Some(pos) = mpd.find("</Period>") {
        let mut merged = String::with_capacity(mpd.len() + text_as.len());
        merged.push_str(&mpd[..pos]);
        merged.push_str(&text_as);
        merged.push_str(&mpd[pos..]);
        merged
    } else {
        mpd.to_string()
    }
}

/// Information about a text track for combined manifest building.
struct TextManifestInfo {
    index: usize,
    name: String,
    language: Option<String>,
}

/// Rewrite an audio manifest from the audio pipeline to use audio-specific
/// segment names and relative paths.
fn rewrite_audio_manifest(
    manifest: &str,
    content_id: &str,
    format: OutputFormat,
    scheme: &EncryptionScheme,
    container_format: edgepack::media::container::ContainerFormat,
) -> String {
    let scheme_str = scheme.scheme_type_str();
    let fmt_label = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };
    let video_ext = container_format.video_segment_extension();
    let audio_ext = container_format.audio_segment_extension();

    // Strip the audio pipeline's base URL so paths become relative
    let base_url = format!("/repackage/{content_id}_audio/{fmt_label}_{scheme_str}/");
    let mut rewritten = manifest
        .replace(&base_url, "")
        .replace("init.mp4", "audio_init.mp4")
        .replace("segment_", "audio_segment_");
    // Fix segment extensions for formats where video ≠ audio (CMAF)
    if video_ext != audio_ext {
        rewritten = rewritten.replace(video_ext, audio_ext);
    }
    rewritten
}

/// Build VariantInfo entries for all video variants with DASH per-Representation segment path prefixes.
///
/// Each variant gets `segment_path_prefix: Some("v{vid}_")` so the DASH renderer generates
/// per-Representation `<SegmentTemplate>` elements with variant-specific init/media segment paths
/// (e.g., `initialization="v0_init.mp4"`, `media="v0_segment_$Number$.cmfv"`).
fn build_dash_variant_infos(
    video_variants: &[VideoVariantInfo],
) -> Vec<edgepack::manifest::types::VariantInfo> {
    use edgepack::manifest::types::{TrackMediaType, VariantInfo};
    video_variants
        .iter()
        .enumerate()
        .map(|(vid, variant)| {
            // Use original_index for file paths to match the v{N}_ prefix on disk
            let file_vid = variant.original_index.unwrap_or(vid);
            let resolution = match (variant.width, variant.height) {
                (Some(w), Some(h)) => Some((w, h)),
                _ => None,
            };
            let frame_rate = variant.frame_rate.as_ref().and_then(|fr| {
                if let Some((n, d)) = fr.split_once('/') {
                    let n: f64 = n.parse().ok()?;
                    let d: f64 = d.parse().ok()?;
                    if d > 0.0 { Some(n / d) } else { None }
                } else {
                    fr.parse::<f64>().ok()
                }
            });
            VariantInfo {
                id: format!("v{file_vid}"),
                bandwidth: variant.bandwidth,
                // Strip audio codec from video-only Representation
                // (HLS CODECS combines video+audio, but DASH has separate AdaptationSets)
                codecs: variant.codecs.as_deref()
                    .map(|c| c.split(',').next().unwrap_or(c).to_string())
                    .unwrap_or_else(|| "avc1.64001f".to_string()),
                resolution,
                frame_rate,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some(format!("v{file_vid}_")),
            }
        })
        .collect()
}

/// Download and concatenate all WebVTT segments from an HLS subtitle media playlist.
///
/// Fetches the .m3u8 media playlist, parses segment URIs, downloads each .vtt segment,
/// and concatenates them into a single WebVTT file. Handles WEBVTT headers in subsequent
/// segments (strips them to avoid duplicates).
fn download_hls_vtt_segments(playlist_url: &str) -> Result<Vec<u8>, String> {
    let client = shared_reqwest_client();

    // Fetch the playlist
    let playlist_resp = client.get(playlist_url)
        .send()
        .map_err(|e| format!("fetch subtitle playlist failed: {e}"))?;
    if !playlist_resp.status().is_success() {
        return Err(format!("subtitle playlist HTTP {}", playlist_resp.status()));
    }
    let playlist_body = playlist_resp.text()
        .map_err(|e| format!("read subtitle playlist failed: {e}"))?;

    // Compute base URL for resolving relative segment URIs
    let base_url = if let Some(last_slash) = playlist_url.rfind('/') {
        &playlist_url[..=last_slash]
    } else {
        playlist_url
    };

    // Parse segment URIs from the playlist
    let segment_urls: Vec<String> = playlist_body.lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#')
        })
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                trimmed.to_string()
            } else {
                format!("{base_url}{trimmed}")
            }
        })
        .collect();

    if segment_urls.is_empty() {
        return Err("no segments found in subtitle playlist".to_string());
    }

    eprintln!("  Downloading {} VTT subtitle segments...", segment_urls.len());

    // Download and concatenate all segments
    let mut combined = Vec::new();
    let mut is_first = true;

    for seg_url in &segment_urls {
        match client.get(seg_url).send() {
            Ok(resp) if resp.status().is_success() => {
                match resp.text() {
                    Ok(text) => {
                        if is_first {
                            combined.extend_from_slice(text.as_bytes());
                            is_first = false;
                        } else {
                            // Strip the WEBVTT header from subsequent segments
                            // to avoid duplicate headers in the concatenated output.
                            let content = text.trim_start();
                            let stripped = if content.starts_with("WEBVTT") {
                                // Skip past the header line (and optional blank line after)
                                let after_header = content.find('\n')
                                    .map(|i| &content[i + 1..])
                                    .unwrap_or("");
                                let after_blank = after_header.strip_prefix('\n')
                                    .or_else(|| after_header.strip_prefix("\r\n"))
                                    .unwrap_or(after_header);
                                after_blank
                            } else {
                                content
                            };
                            if !stripped.is_empty() {
                                combined.push(b'\n');
                                combined.extend_from_slice(stripped.as_bytes());
                            }
                        }
                    }
                    Err(e) => eprintln!("  Warning: failed to read VTT segment {seg_url}: {e}"),
                }
            }
            Ok(resp) => eprintln!("  Warning: VTT segment HTTP {} for {seg_url}", resp.status()),
            Err(e) => eprintln!("  Warning: failed to fetch VTT segment {seg_url}: {e}"),
        }
    }

    if combined.is_empty() {
        return Err("all VTT segments failed to download".to_string());
    }

    Ok(combined)
}

/// Rewrite a video variant manifest from a per-variant pipeline to use variant-specific
/// segment names and relative paths (v{vid}_ prefix).
fn rewrite_variant_manifest(
    manifest: &str,
    content_id: &str,
    vid: usize,
    format: OutputFormat,
    scheme: &EncryptionScheme,
    container_format: edgepack::media::container::ContainerFormat,
) -> String {
    let scheme_str = scheme.scheme_type_str();
    let fmt_label = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };
    let _video_ext = container_format.video_segment_extension();
    let prefix = format!("v{vid}");

    // Strip the variant pipeline's base URL so paths become relative
    let base_url = format!("/repackage/{content_id}_v{vid}/{fmt_label}_{scheme_str}/");
    let rewritten = manifest
        .replace(&base_url, "")
        .replace("init.mp4", &format!("{prefix}_init.mp4"))
        .replace("segment_", &format!("{prefix}_segment_"));
    rewritten
}

/// Rewrite a text track manifest from the text pipeline to use track-specific
/// segment names and relative paths.
fn rewrite_text_manifest(
    manifest: &str,
    content_id: &str,
    text_idx: usize,
    format: OutputFormat,
    scheme: &EncryptionScheme,
    container_format: edgepack::media::container::ContainerFormat,
) -> String {
    let scheme_str = scheme.scheme_type_str();
    let fmt_label = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };
    let video_ext = container_format.video_segment_extension();
    let text_ext = container_format.video_segment_extension(); // text in fMP4 uses same ext

    let base_url = format!("/repackage/{content_id}_text_{text_idx}/{fmt_label}_{scheme_str}/");
    let prefix = format!("text_{text_idx}");
    let mut rewritten = manifest
        .replace(&base_url, "")
        .replace("init.mp4", &format!("{prefix}_init.mp4"))
        .replace("segment_", &format!("{prefix}_segment_"));
    if video_ext != text_ext {
        rewritten = rewritten.replace(video_ext, text_ext);
    }
    rewritten
}

/// Build a combined manifest that includes video + audio + text tracks.
///
/// For HLS: renders a master playlist referencing video.m3u8, audio.m3u8, and text_N.m3u8.
///   Multi-variant: one `#EXT-X-STREAM-INF` per variant with real metadata (v{vid}_video.m3u8).
///   Single-variant: single `#EXT-X-STREAM-INF` with metadata from source (video.m3u8).
/// For DASH: merges audio and text AdaptationSets into the video MPD.
fn build_progressive_combined_manifest(
    format: OutputFormat,
    video_manifest: &str,
    raw_audio_manifest: &str,
    content_id: &str,
    scheme: &EncryptionScheme,
    container_format: edgepack::media::container::ContainerFormat,
    text_tracks: &[TextManifestInfo],
    video_variants: &[VideoVariantInfo],
    is_complete: bool,
) -> String {
    match format {
        OutputFormat::Hls => {
            let audio_codec = "mp4a.40.2".to_string();
            let has_audio = !raw_audio_manifest.is_empty();

            let mut master = String::new();
            master.push_str("#EXTM3U\n");
            master.push_str("#EXT-X-VERSION:7\n");
            master.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
            // For progressive (not yet complete) content, tell the player to start
            // from the beginning rather than seeking to the live edge.
            // Per RFC 8216 §4.4.2.1.3, #EXT-X-START in the master playlist
            // indicates the preferred start point. VOD playlists already start
            // from the beginning so this is only needed during progressive processing.
            if !is_complete {
                master.push_str("#EXT-X-START:TIME-OFFSET=0,PRECISE=YES\n");
            }

            // Audio rendition group
            if has_audio {
                master.push_str(
                    "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n"
                );
            }

            // Text rendition groups
            for tt in text_tracks {
                let lang_attr = tt.language.as_ref()
                    .map(|l| format!(",LANGUAGE=\"{l}\""))
                    .unwrap_or_default();
                master.push_str(&format!(
                    "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"{}\",DEFAULT=NO,AUTOSELECT=YES{lang_attr},URI=\"text_{}.m3u8\"\n",
                    tt.name, tt.index,
                ));
            }

            if video_variants.len() > 1 {
                // Multi-variant: emit one #EXT-X-STREAM-INF per variant with real metadata
                for (vid, variant) in video_variants.iter().enumerate() {
                    // Use original_index for filenames if set (preserves v{N}_ names after filtering)
                    let file_vid = variant.original_index.unwrap_or(vid);
                    // Strip audio codecs from variant codecs — source HLS may include
                    // combined video+audio (e.g. "avc1.4d401f,mp4a.40.2"). We add the
                    // audio codec explicitly below when has_audio is true.
                    let video_codec = strip_audio_codecs(
                        variant.codecs.as_deref().unwrap_or("avc1.64001f")
                    );
                    let mut stream_inf = format!("#EXT-X-STREAM-INF:BANDWIDTH={}", variant.bandwidth);

                    // RESOLUTION
                    if let (Some(w), Some(h)) = (variant.width, variant.height) {
                        stream_inf.push_str(&format!(",RESOLUTION={w}x{h}"));
                    }

                    // FRAME-RATE
                    if let Some(ref fr) = variant.frame_rate {
                        stream_inf.push_str(&format!(",FRAME-RATE={fr}"));
                    }

                    // CODECS
                    stream_inf.push_str(&format!(",CODECS=\"{video_codec}"));
                    if has_audio {
                        stream_inf.push_str(&format!(",{audio_codec}"));
                    }
                    stream_inf.push('"');

                    // Group references
                    if has_audio {
                        stream_inf.push_str(",AUDIO=\"audio\"");
                    }
                    if !text_tracks.is_empty() {
                        stream_inf.push_str(",SUBTITLES=\"subs\"");
                    }

                    master.push_str(&stream_inf);
                    master.push('\n');
                    master.push_str(&format!("v{file_vid}_video.m3u8\n"));
                }
            } else {
                // Single variant: use metadata from source if available
                let (bandwidth, video_codec, resolution, frame_rate) = if let Some(v) = video_variants.first() {
                    (
                        v.bandwidth,
                        // Strip audio codecs — source may include combined video+audio
                        strip_audio_codecs(v.codecs.as_deref().unwrap_or("avc1.64001f")),
                        if let (Some(w), Some(h)) = (v.width, v.height) {
                            Some(format!("{w}x{h}"))
                        } else {
                            None
                        },
                        v.frame_rate.clone(),
                    )
                } else {
                    (2_000_000, "avc1.64001f".to_string(), None, None)
                };

                let mut stream_inf = format!("#EXT-X-STREAM-INF:BANDWIDTH={bandwidth}");

                if let Some(ref res) = resolution {
                    stream_inf.push_str(&format!(",RESOLUTION={res}"));
                }
                if let Some(ref fr) = frame_rate {
                    stream_inf.push_str(&format!(",FRAME-RATE={fr}"));
                }

                stream_inf.push_str(&format!(",CODECS=\"{video_codec}"));
                if has_audio {
                    stream_inf.push_str(&format!(",{audio_codec}"));
                }
                stream_inf.push('"');
                if has_audio {
                    stream_inf.push_str(",AUDIO=\"audio\"");
                }
                if !text_tracks.is_empty() {
                    stream_inf.push_str(",SUBTITLES=\"subs\"");
                }
                master.push_str(&stream_inf);
                master.push('\n');
                master.push_str("video.m3u8\n");
            }
            master
        }
        OutputFormat::Dash => {
            let mut merged = if video_variants.len() > 1 && !video_manifest.is_empty() {
                // Multi-variant DASH: replace the single Representation in the base MPD
                // with multiple per-variant Representations, each with its own
                // SegmentTemplate using variant-specific path prefixes.
                build_dash_multi_variant_mpd(video_manifest, video_variants, container_format)
            } else {
                video_manifest.to_string()
            };

            // Merge audio AdaptationSet into the video MPD
            if !raw_audio_manifest.is_empty() {
                let audio_base = format!("/repackage/{content_id}_audio/dash_{}/",
                    scheme.scheme_type_str());
                merged = merge_dash_audio(&merged, raw_audio_manifest, &audio_base, container_format);
            }

            // Merge text track AdaptationSets into the MPD
            for tt in text_tracks {
                merged = merge_dash_text_track(&merged, tt, container_format);
            }

            merged
        }
    }
}

/// Strip audio codec strings from a combined codec string.
/// Source HLS master playlists often include combined video+audio codecs
/// (e.g. "avc1.4d401f,mp4a.40.2"). This strips audio codecs so the caller
/// can add the audio codec explicitly and avoid duplicates.
fn strip_audio_codecs(codecs: &str) -> String {
    codecs
        .split(',')
        .filter(|c| {
            let c = c.trim();
            !c.starts_with("mp4a.")
                && !c.starts_with("ac-3")
                && !c.starts_with("ec-3")
                && !c.starts_with("opus")
                && !c.starts_with("flac")
                && !c.starts_with("dtsc")
                && !c.starts_with("dtse")
                && !c.starts_with("dtsx")
        })
        .collect::<Vec<_>>()
        .join(",")
}

/// Extract an attribute value from an HLS #EXT-X-MEDIA tag.
#[allow(dead_code)]
fn extract_hls_media_attr(tag: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    let start = tag.find(&pattern)?;
    let value_start = start + pattern.len();
    let value_end = tag[value_start..].find('"')?;
    Some(tag[value_start..value_start + value_end].to_string())
}

/// Fix DASH MPD for sandbox playback.
///
/// During progressive output, the pipeline renders `type="dynamic"` (live) manifests.
/// For sandbox progressive playback, keep `type="dynamic"` so DASH.js refreshes the
/// MPD and picks up new segments. Adds attributes to make the player start from the
/// beginning of content rather than seeking to the live edge:
///
/// - `suggestedPresentationDelay` set to total content duration (pushes start to beginning)
/// - `publishTime` with current UTC timestamp (required for dynamic MPDs)
/// - `minimumUpdatePeriod="PT2S"` (give sandbox time between disk writes)
/// - Removes `mediaPresentationDuration` (invalid for in-progress dynamic MPDs)
fn fixup_dash_progressive(mpd: &str) -> String {
    let mut fixed = mpd.to_string();

    // Ensure type="dynamic" is present (core library sets this during Live phase)
    if !fixed.contains("type=\"dynamic\"") {
        // If it's already static (shouldn't happen during progressive), convert to dynamic
        fixed = fixed.replace("type=\"static\"", "type=\"dynamic\"");
    }

    // Replace minimumUpdatePeriod with a longer interval for disk-based serving
    fixed = fixed.replace("minimumUpdatePeriod=\"PT1S\"", "minimumUpdatePeriod=\"PT2S\"");

    // Compute total duration and set suggestedPresentationDelay to push playback to beginning
    let total_duration = compute_dash_duration_from_timeline(&fixed);
    if total_duration > 0.0 {
        // Add a small buffer to ensure we're fully at the beginning
        let delay_str = format_seconds_as_iso8601(total_duration + 10.0);

        // Remove existing suggestedPresentationDelay if present
        if let Some(start) = fixed.find(" suggestedPresentationDelay=\"") {
            let attr_start = start;
            let after_first_quote = attr_start + " suggestedPresentationDelay=\"".len();
            if let Some(close_quote) = fixed[after_first_quote..].find('"') {
                fixed = format!("{}{}", &fixed[..attr_start], &fixed[after_first_quote + close_quote + 1..]);
            }
        }

        // Insert suggestedPresentationDelay after minimumUpdatePeriod
        if let Some(pos) = fixed.find("minimumUpdatePeriod=\"PT2S\"") {
            let insert_at = pos + "minimumUpdatePeriod=\"PT2S\"".len();
            fixed.insert_str(insert_at, &format!(" suggestedPresentationDelay=\"{delay_str}\""));
        }
    }

    // Add publishTime (required for dynamic MPDs) — current UTC timestamp
    if !fixed.contains("publishTime=\"") {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default();
        let secs = now.as_secs();
        // Simple UTC timestamp: compute year/month/day/hour/min/sec from epoch
        let (year, month, day, hour, min, sec) = epoch_to_utc(secs);
        let publish_time = format!("{year:04}-{month:02}-{day:02}T{hour:02}:{min:02}:{sec:02}Z");
        // Insert after type="dynamic"
        if let Some(pos) = fixed.find("type=\"dynamic\"") {
            let insert_at = pos + "type=\"dynamic\"".len();
            fixed.insert_str(insert_at, &format!(" publishTime=\"{publish_time}\""));
        }
    }

    // Remove mediaPresentationDuration if present (invalid for in-progress dynamic MPDs)
    if let Some(start) = fixed.find(" mediaPresentationDuration=\"") {
        let after_first_quote = start + " mediaPresentationDuration=\"".len();
        if let Some(close_quote) = fixed[after_first_quote..].find('"') {
            fixed = format!("{}{}", &fixed[..start], &fixed[after_first_quote + close_quote + 1..]);
        }
    }

    fixed
}

/// Convert epoch seconds to UTC (year, month, day, hour, minute, second).
fn epoch_to_utc(epoch_secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let secs_per_day = 86400u64;
    let mut days = epoch_secs / secs_per_day;
    let day_secs = epoch_secs % secs_per_day;
    let hour = day_secs / 3600;
    let min = (day_secs % 3600) / 60;
    let sec = day_secs % 60;

    // Days since 1970-01-01
    let mut year = 1970u64;
    loop {
        let days_in_year = if is_leap_year(year) { 366 } else { 365 };
        if days < days_in_year {
            break;
        }
        days -= days_in_year;
        year += 1;
    }

    let month_days: [u64; 12] = if is_leap_year(year) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };

    let mut month = 1u64;
    for &md in &month_days {
        if days < md {
            break;
        }
        days -= md;
        month += 1;
    }

    (year, month, days + 1, hour, min, sec)
}

fn is_leap_year(y: u64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// For sandbox playback where all segments are immediately available on disk, convert
/// to `type="static"` (VOD) so DASH.js doesn't try to calculate segment availability
/// windows based on wall clock time.
///
/// Also removes `minimumUpdatePeriod` and `availabilityStartTime` which are live-only
/// attributes, and adds `mediaPresentationDuration` computed from segment durations.
fn fixup_dash_for_sandbox(mpd: &str) -> String {
    let mut fixed = mpd.to_string();

    // If already static, no fixup needed
    if !fixed.contains("type=\"dynamic\"") {
        return fixed;
    }

    // Convert type="dynamic" to type="static"
    fixed = fixed.replace("type=\"dynamic\"", "type=\"static\"");
    // Remove live-only attributes
    fixed = fixed.replace(" minimumUpdatePeriod=\"PT1S\"", "");
    // Remove availabilityStartTime (generic pattern)
    if let Some(start) = fixed.find(" availabilityStartTime=\"") {
        let after_first_quote = start + " availabilityStartTime=\"".len();
        if let Some(close_quote) = fixed[after_first_quote..].find('"') {
            fixed = format!("{}{}", &fixed[..start], &fixed[after_first_quote + close_quote + 1..]);
        }
    }

    // If no mediaPresentationDuration exists, calculate from SegmentTimeline
    // and add it to the MPD element
    if !fixed.contains("mediaPresentationDuration=") {
        // Sum all segment durations from the <SegmentTimeline> <S d="..." r="..."/> entries
        let total_duration = compute_dash_duration_from_timeline(&fixed);
        if total_duration > 0.0 {
            let dur_str = format_seconds_as_iso8601(total_duration);
            // Insert after type="static"
            fixed = fixed.replace(
                "type=\"static\"",
                &format!("type=\"static\" mediaPresentationDuration=\"{dur_str}\""),
            );
        }
    }

    fixed
}

/// Compute total duration from DASH SegmentTimeline entries.
/// Parses `<S d="..." r="..."/>` elements and the timeline's timescale.
fn compute_dash_duration_from_timeline(mpd: &str) -> f64 {
    // Extract timescale from SegmentTemplate
    let timescale = extract_xml_attr(mpd, "SegmentTemplate", "timescale")
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1.0);

    let mut total_ticks: f64 = 0.0;
    let mut search_start = 0;
    while let Some(s_start) = mpd[search_start..].find("<S ") {
        let abs_start = search_start + s_start;
        let s_end = mpd[abs_start..].find("/>").unwrap_or(0) + abs_start + 2;
        let s_tag = &mpd[abs_start..s_end];

        // Extract d (duration) attribute
        if let Some(d_str) = extract_xml_attr(s_tag, "S", "d") {
            if let Ok(d) = d_str.parse::<f64>() {
                // Extract r (repeat) attribute, default 0
                let repeat = extract_xml_attr(s_tag, "S", "r")
                    .and_then(|r| r.parse::<i64>().ok())
                    .unwrap_or(0);
                total_ticks += d * (1 + repeat) as f64;
            }
        }
        search_start = s_end;
    }

    if timescale > 0.0 {
        total_ticks / timescale
    } else {
        0.0
    }
}

/// Format seconds as ISO 8601 duration (e.g., PT60.5S).
fn format_seconds_as_iso8601(seconds: f64) -> String {
    let hours = (seconds / 3600.0).floor() as u64;
    let remaining = seconds - (hours as f64 * 3600.0);
    let minutes = (remaining / 60.0).floor() as u64;
    let secs = remaining - (minutes as f64 * 60.0);

    if hours > 0 {
        format!("PT{hours}H{minutes}M{secs:.1}S")
    } else if minutes > 0 {
        format!("PT{minutes}M{secs:.1}S")
    } else {
        format!("PT{secs:.1}S")
    }
}

fn validate_output(
    content_id: &str,
    format: OutputFormat,
    scheme: &str,
    output: &ProgressiveOutput,
) -> serde_json::Value {
    let fmt_str = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };
    let label = format!("{fmt_str}_{scheme}");
    let state = output.manifest_state();

    let mut checks: Vec<serde_json::Value> = Vec::new();
    let mut all_pass = true;

    // 1. Manifest render check
    match manifest::render_manifest(state) {
        Ok(rendered) => {
            let has_content = !rendered.trim().is_empty();
            if has_content {
                // HLS structural checks
                if format == OutputFormat::Hls {
                    let has_extm3u = rendered.starts_with("#EXTM3U");
                    let has_endlist = rendered.contains("#EXT-X-ENDLIST");
                    let has_version = rendered.contains("#EXT-X-VERSION:");
                    let has_targetduration = rendered.contains("#EXT-X-TARGETDURATION:");
                    let has_map = rendered.contains("#EXT-X-MAP:");
                    let has_extinf = rendered.contains("#EXTINF:");
                    let segment_count = rendered.matches("#EXTINF:").count();

                    checks.push(serde_json::json!({
                        "name": "HLS #EXTM3U header",
                        "pass": has_extm3u,
                    }));
                    checks.push(serde_json::json!({
                        "name": "HLS #EXT-X-VERSION tag",
                        "pass": has_version,
                    }));
                    checks.push(serde_json::json!({
                        "name": "HLS #EXT-X-TARGETDURATION tag",
                        "pass": has_targetduration,
                    }));
                    checks.push(serde_json::json!({
                        "name": "HLS #EXT-X-MAP init segment reference",
                        "pass": has_map,
                    }));
                    checks.push(serde_json::json!({
                        "name": format!("HLS #EXTINF segment entries ({segment_count} segments)"),
                        "pass": has_extinf,
                    }));
                    checks.push(serde_json::json!({
                        "name": "HLS #EXT-X-ENDLIST (VOD finalization)",
                        "pass": has_endlist,
                    }));

                    if !has_extm3u || !has_version || !has_targetduration || !has_endlist {
                        all_pass = false;
                    }
                }

                // DASH structural checks
                if format == OutputFormat::Dash {
                    let has_mpd = rendered.contains("<MPD");
                    let has_period = rendered.contains("<Period");
                    let has_adaptation = rendered.contains("<AdaptationSet");
                    let has_representation = rendered.contains("<Representation");
                    let has_segment_template = rendered.contains("<SegmentTemplate");
                    let has_segment_timeline = rendered.contains("<SegmentTimeline");
                    let is_static = rendered.contains("type=\"static\"");
                    let has_profiles = rendered.contains("profiles=");

                    checks.push(serde_json::json!({
                        "name": "DASH <MPD> root element",
                        "pass": has_mpd,
                    }));
                    checks.push(serde_json::json!({
                        "name": "DASH profiles attribute",
                        "pass": has_profiles,
                    }));
                    checks.push(serde_json::json!({
                        "name": "DASH type=\"static\" (VOD)",
                        "pass": is_static,
                    }));
                    checks.push(serde_json::json!({
                        "name": "DASH <Period> element",
                        "pass": has_period,
                    }));
                    checks.push(serde_json::json!({
                        "name": "DASH <AdaptationSet> element",
                        "pass": has_adaptation,
                    }));
                    checks.push(serde_json::json!({
                        "name": "DASH <Representation> element",
                        "pass": has_representation,
                    }));
                    checks.push(serde_json::json!({
                        "name": "DASH <SegmentTemplate> element",
                        "pass": has_segment_template,
                    }));
                    checks.push(serde_json::json!({
                        "name": "DASH <SegmentTimeline> element",
                        "pass": has_segment_timeline,
                    }));

                    if !has_mpd || !has_period || !has_adaptation || !has_representation {
                        all_pass = false;
                    }
                }
            } else {
                checks.push(serde_json::json!({
                    "name": "Manifest has content",
                    "pass": false,
                }));
                all_pass = false;
            }
        }
        Err(e) => {
            checks.push(serde_json::json!({
                "name": "Manifest renders without error",
                "pass": false,
                "detail": format!("{e}"),
            }));
            all_pass = false;
        }
    }

    // 2. Init segment validation (ISOBMFF structure)
    if let Some(init_data) = output.init_segment_data() {
        let result = compat::validate_init_segment(init_data);
        checks.push(serde_json::json!({
            "name": format!("Init segment structure ({} bytes)", init_data.len()),
            "pass": result.valid,
            "warnings": result.warnings,
            "errors": result.errors,
        }));
        if !result.valid {
            all_pass = false;
        }

        // 2b. Track analysis — detect audio/video presence
        if let Ok(tracks) = codec::extract_tracks(init_data) {
            let has_video = tracks.iter().any(|t| matches!(t.track_type, TrackType::Video));
            let has_audio = tracks.iter().any(|t| matches!(t.track_type, TrackType::Audio));
            let track_summary: Vec<String> = tracks.iter().map(|t| {
                let type_str = match t.track_type {
                    TrackType::Video => "video",
                    TrackType::Audio => "audio",
                    TrackType::Subtitle => "subtitle",
                    _ => "other",
                };
                let codec_str = if t.codec_string.is_empty() { "unknown" } else { &t.codec_string };
                format!("{type_str}({codec_str})")
            }).collect();

            checks.push(serde_json::json!({
                "name": format!("Tracks: {}", track_summary.join(", ")),
                "pass": has_video || has_audio,
            }));

            if has_audio && has_video {
                checks.push(serde_json::json!({
                    "name": "Audio track present (muxed with video)",
                    "pass": true,
                }));
            } else if has_audio && !has_video {
                checks.push(serde_json::json!({
                    "name": "Audio-only track (demuxed rendition)",
                    "pass": true,
                }));
            } else if has_video && !has_audio {
                checks.push(serde_json::json!({
                    "name": "Audio: not in this init segment",
                    "pass": true,
                    "warnings": ["Audio may be in a separate rendition — check output for audio_* files"],
                }));
            }
        }
    } else {
        // TS output has no init segment — not an error
        let is_ts = state.container_format.to_string() == "ts";
        checks.push(serde_json::json!({
            "name": "Init segment present",
            "pass": is_ts,
            "detail": if is_ts { "TS output: no init segment (expected)" } else { "Missing init segment" },
        }));
        if !is_ts {
            all_pass = false;
        }
    }

    // 3. Media segment validation (spot-check first and last)
    let segments = &state.segments;
    let is_encrypted = scheme != "none";
    if !segments.is_empty() {
        // Validate first segment
        if let Some(data) = output.segment_data(segments[0].number) {
            let result = compat::validate_media_segment(data, is_encrypted);
            checks.push(serde_json::json!({
                "name": format!("Segment {} structure ({} bytes)", segments[0].number, data.len()),
                "pass": result.valid,
                "warnings": result.warnings,
                "errors": result.errors,
            }));
            if !result.valid {
                all_pass = false;
            }
        }

        // Validate last segment (if different from first)
        if segments.len() > 1 {
            let last = &segments[segments.len() - 1];
            if let Some(data) = output.segment_data(last.number) {
                let result = compat::validate_media_segment(data, is_encrypted);
                checks.push(serde_json::json!({
                    "name": format!("Segment {} structure ({} bytes)", last.number, data.len()),
                    "pass": result.valid,
                    "warnings": result.warnings,
                    "errors": result.errors,
                }));
                if !result.valid {
                    all_pass = false;
                }
            }
        }
    } else {
        checks.push(serde_json::json!({
            "name": "At least one media segment",
            "pass": false,
        }));
        all_pass = false;
    }

    // 4. Segment count consistency
    let manifest_seg_count = segments.len();
    let disk_seg_count = segments
        .iter()
        .filter(|s| output.segment_data(s.number).is_some())
        .count();
    checks.push(serde_json::json!({
        "name": format!("Segment count consistency (manifest: {manifest_seg_count}, data: {disk_seg_count})"),
        "pass": manifest_seg_count == disk_seg_count,
    }));
    if manifest_seg_count != disk_seg_count {
        all_pass = false;
    }

    serde_json::json!({
        "output": label,
        "content_id": content_id,
        "pass": all_pass,
        "checks": checks,
    })
}

// ─── Main ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let state = Arc::new(AppState {
        jobs: Mutex::new(HashMap::new()),
    });

    #[allow(unused_mut)]
    let mut app = Router::new()
        .route("/", get(serve_ui))
        .route("/api/repackage", post(handle_repackage))
        .route("/api/status/{content_id}/{format}", get(handle_status))
        .route(
            "/api/output/{content_id}/{format}/{file}",
            get(handle_output),
        )
        // Mirror the production /repackage/ paths so manifest-embedded URLs
        // (e.g. /repackage/{id}/hls_cenc/init.mp4) resolve in the sandbox.
        .route(
            "/repackage/{content_id}/{format}/{file}",
            get(handle_output),
        )
        .route(
            "/api/manifest/{content_id}/{format_scheme}",
            get(handle_manifest_preview),
        );

    // JIT source config endpoint
    app = app.route("/api/config/source", post(handle_source_config));

    let app = app.with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3333")
        .await
        .expect("failed to bind to port 3333");

    eprintln!();
    eprintln!("  edgepack sandbox running on http://localhost:3333");
    eprintln!();

    axum::serve(listener, app)
        .await
        .expect("server error");
}

// ─── Embedded HTML UI ───────────────────────────────────────────────────

const SANDBOX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>edgepack sandbox</title>
<style>
  :root {
    --bg: #0f1117;
    --surface: #1a1d27;
    --border: #2a2d3a;
    --text: #e4e4e7;
    --text-muted: #8b8d98;
    --accent: #6366f1;
    --accent-hover: #818cf8;
    --success: #22c55e;
    --error: #ef4444;
    --warning: #f59e0b;
  }
  * { box-sizing: border-box; margin: 0; padding: 0; }
  body {
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, monospace;
    background: var(--bg);
    color: var(--text);
    min-height: 100vh;
    padding: 2rem;
  }
  .container { max-width: 640px; margin: 0 auto; }
  h1 {
    font-size: 1.5rem;
    font-weight: 600;
    margin-bottom: 0.25rem;
  }
  .subtitle {
    color: var(--text-muted);
    font-size: 0.875rem;
    margin-bottom: 2rem;
  }
  .card {
    background: var(--surface);
    border: 1px solid var(--border);
    border-radius: 0.75rem;
    padding: 1.5rem;
    margin-bottom: 1.5rem;
  }
  label {
    display: block;
    font-size: 0.8rem;
    color: var(--text-muted);
    margin-bottom: 0.375rem;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  input[type="text"], select {
    width: 100%;
    padding: 0.625rem 0.75rem;
    background: var(--bg);
    border: 1px solid var(--border);
    border-radius: 0.5rem;
    color: var(--text);
    font-size: 0.875rem;
    font-family: inherit;
    margin-bottom: 1rem;
    outline: none;
    transition: border-color 0.2s;
  }
  input[type="text"]:focus, select:focus {
    border-color: var(--accent);
  }
  input[type="text"]::placeholder {
    color: var(--text-muted);
    opacity: 0.6;
  }
  .radio-group {
    display: flex;
    gap: 1rem;
    margin-bottom: 1rem;
  }
  .radio-group label {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    cursor: pointer;
    text-transform: none;
    letter-spacing: 0;
    font-size: 0.875rem;
    color: var(--text);
  }
  input[type="radio"] {
    accent-color: var(--accent);
  }
  .format-hint {
    display: inline-block;
    font-size: 0.75rem;
    padding: 0.125rem 0.5rem;
    border-radius: 0.25rem;
    background: var(--accent);
    color: white;
    margin-left: 0.5rem;
    opacity: 0;
    transition: opacity 0.2s;
  }
  .format-hint.visible { opacity: 1; }
  button {
    width: 100%;
    padding: 0.75rem;
    background: var(--accent);
    color: white;
    border: none;
    border-radius: 0.5rem;
    font-size: 0.9rem;
    font-weight: 600;
    cursor: pointer;
    transition: background 0.2s;
    font-family: inherit;
  }
  button:hover { background: var(--accent-hover); }
  button:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  #status-panel {
    display: none;
  }
  .progress-bar {
    width: 100%;
    height: 6px;
    background: var(--bg);
    border-radius: 3px;
    overflow: hidden;
    margin: 0.75rem 0;
  }
  .progress-fill {
    height: 100%;
    background: var(--accent);
    border-radius: 3px;
    transition: width 0.3s ease;
    width: 0%;
  }
  .progress-fill.complete { background: var(--success); }
  .progress-fill.error { background: var(--error); }
  .status-text {
    font-size: 0.8rem;
    color: var(--text-muted);
  }
  .status-text .state {
    color: var(--warning);
    font-weight: 600;
  }
  .status-text .state.complete { color: var(--success); }
  .status-text .state.failed { color: var(--error); }
  .output-info {
    margin-top: 1rem;
    padding: 0.75rem;
    background: var(--bg);
    border-radius: 0.5rem;
    font-size: 0.8rem;
  }
  .output-info code {
    color: var(--accent);
    font-family: inherit;
  }
  .output-links {
    margin-top: 0.75rem;
    display: flex;
    flex-wrap: wrap;
    gap: 0.5rem;
  }
  .output-links a {
    color: var(--accent);
    text-decoration: none;
    font-size: 0.8rem;
    padding: 0.25rem 0.625rem;
    border: 1px solid var(--border);
    border-radius: 0.375rem;
    transition: border-color 0.2s;
  }
  .output-links a:hover {
    border-color: var(--accent);
  }
  .hidden { display: none; }
  .validation-output { margin-bottom: 1rem; }
  .validation-output:last-child { margin-bottom: 0; }
  .validation-header {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    font-size: 0.85rem;
    font-weight: 600;
    margin-bottom: 0.5rem;
  }
  .validation-header .badge {
    font-size: 0.65rem;
    padding: 0.1rem 0.4rem;
    border-radius: 0.25rem;
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .badge.pass { background: rgba(34,197,94,0.15); color: var(--success); }
  .badge.fail { background: rgba(239,68,68,0.15); color: var(--error); }
  .check-item {
    display: flex;
    align-items: flex-start;
    gap: 0.5rem;
    font-size: 0.75rem;
    padding: 0.25rem 0;
    color: var(--text-muted);
  }
  .check-icon { font-size: 0.8rem; flex-shrink: 0; line-height: 1.2; }
  .check-icon.pass { color: var(--success); }
  .check-icon.fail { color: var(--error); }
  .check-warnings {
    font-size: 0.7rem;
    color: var(--warning);
    margin-left: 1.3rem;
  }
  #player-panel { display: none; }
  #player-panel video {
    width: 100%;
    border-radius: 0.5rem;
    background: #000;
    max-height: 360px;
  }
  .player-status {
    font-size: 0.75rem;
    color: var(--text-muted);
    margin-top: 0.5rem;
  }
  .player-status .live-badge {
    display: inline-block;
    font-size: 0.6rem;
    padding: 0.1rem 0.4rem;
    border-radius: 0.25rem;
    background: rgba(239,68,68,0.2);
    color: var(--error);
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .player-status .vod-badge {
    display: inline-block;
    font-size: 0.6rem;
    padding: 0.1rem 0.4rem;
    border-radius: 0.25rem;
    background: rgba(34,197,94,0.2);
    color: var(--success);
    font-weight: 700;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
</style>
</head>
<body>
<div class="container">
  <h1>edgepack sandbox</h1>
  <p class="subtitle">Local repackaging tool &mdash; video, audio, subtitles &amp; metadata across all encryption schemes &amp; container formats</p>

  <div class="card">
    <label for="source-preset">Test Stream Presets</label>
    <select id="source-preset">
      <option value="">Custom URL...</option>
      <optgroup label="HLS — Clear, fMP4/CMAF">
        <option value="https://storage.googleapis.com/shaka-demo-assets/angel-one-hls/hls.m3u8">Shaka Angel One (master, multi-audio)</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/bbb-dark-truths-hls/hls.m3u8">Shaka Big Buck Bunny (master)</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/tos/hls.m3u8">Shaka Tears of Steel (master)</option>
        <option value="https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_fmp4/master.m3u8">Apple Advanced fMP4 (master, I-frames, subs)</option>
        <option value="https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_adv_example_hevc/master.m3u8">Apple HEVC + H.264 fMP4 (master, dual codec)</option>
        <option value="https://demo.unified-streaming.com/k8s/features/stable/video/tears-of-steel/tears-of-steel.ism/.m3u8">Unified Tears of Steel (master, 5 variants)</option>
        <option value="https://bitdash-a.akamaihd.net/content/MI201109210084_1/m3u8s-fmp4/f08e80da-bf1d-4e3d-8899-f0f6155f6efa.m3u8">Bitmovin Art of Motion fMP4 (master)</option>
        <option value="https://cdn.bitmovin.com/content/assets/MI201109210084/m3u8s/f08e80da-bf1d-4e3d-8899-f0f6155f6efa.m3u8">Bitmovin Art of Motion (master)</option>
      </optgroup>
      <optgroup label="HLS — Clear, TS">
        <option value="https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8">Mux Big Buck Bunny TS (master, ABR)</option>
        <option value="https://test-streams.mux.dev/x36xhzz/url_6/193039199_mp4_h264_aac_hq_7.m3u8">Mux Big Buck Bunny 480p TS (media)</option>
        <option value="https://devstreaming-cdn.apple.com/videos/streaming/examples/img_bipbop_adv_example_ts/master.m3u8">Apple Advanced TS (master)</option>
        <option value="https://bitdash-a.akamaihd.net/content/sintel/hls/playlist.m3u8">Bitmovin Sintel TS (master)</option>
        <option value="https://playertest.longtailvideo.com/adaptive/elephants_dream_v4/index.m3u8">Elephants Dream TS (alt audio + VTT)</option>
      </optgroup>
      <optgroup label="HLS — Encrypted">
        <option value="https://playertest.longtailvideo.com/adaptive/oceans_aes/oceans_aes.m3u8">Oceans AES-128 TS (master)</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/sintel-fmp4-aes/master.m3u8">Shaka Sintel AES-128 fMP4 (master)</option>
      </optgroup>
      <optgroup label="DASH — Clear, VOD">
        <option value="https://storage.googleapis.com/shaka-demo-assets/angel-one/dash.mpd">Shaka Angel One (H.264 + VP9)</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/bbb-dark-truths/dash.mpd">Shaka Big Buck Bunny</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/sintel-mp4-only/dash.mpd">Shaka Sintel 4K (SegmentBase, on-demand)</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/sintel-trickplay/dash.mpd">Shaka Sintel Trickplay</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/sintel-mp4-wvtt/dash.mpd">Shaka Sintel + WebVTT subs</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/tos-ttml/dash.mpd">Shaka Tears of Steel + TTML</option>
        <option value="https://storage.googleapis.com/shaka-demo-assets/tos-surround/dash.mpd">Shaka Tears of Steel surround audio</option>
        <option value="https://demo.unified-streaming.com/k8s/features/stable/video/tears-of-steel/tears-of-steel.ism/.mpd">Unified Tears of Steel (5 variants)</option>
        <option value="https://cdn.bitmovin.com/content/assets/MI201109210084/mpds/f08e80da-bf1d-4e3d-8899-f0f6155f6efa.mpd">Bitmovin Art of Motion</option>
        <option value="https://media.axprod.net/TestVectors/v7-Clear/Manifest.mpd">Axinom Clear (single period)</option>
        <option value="https://media.axprod.net/TestVectors/v7-Clear/Manifest_MultiPeriod.mpd">Axinom Clear (multi-period)</option>
        <option value="https://rdmedia.bbc.co.uk/dash/ondemand/testcard/1/client_manifest-events.mpd">BBC Test Card (in-band events)</option>
        <option value="https://dash.akamaized.net/akamai/bbb_30fps/bbb_with_tiled_thumbnails.mpd">Akamai BBB (tiled thumbnails)</option>
      </optgroup>
      <optgroup label="DASH — Multi-Codec">
        <option value="https://bitmovin-a.akamaihd.net/content/dataset/multi-codec/stream.mpd">Bitmovin H.264 + HEVC + VP9</option>
        <option value="https://bitmovin-a.akamaihd.net/content/dataset/multi-codec/hevc/stream.mpd">Bitmovin HEVC only</option>
        <option value="https://bitmovin-a.akamaihd.net/content/dataset/multi-codec/stream_vp9.mpd">Bitmovin VP9 only</option>
      </optgroup>
      <optgroup label="DASH — Multi-Period / Features">
        <option value="https://storage.googleapis.com/shaka-demo-assets/heliocentrism/heliocentrism.mpd">Shaka Heliocentrism (multi-period)</option>
        <option value="https://livesim2.dashif.org/vod/testpic_2s/Manifest.mpd">DASH-IF Testpic VOD (2s segments)</option>
        <option value="https://livesim2.dashif.org/vod/testpic_2s/Manifest_trickmode.mpd">DASH-IF Testpic Trickmode</option>
        <option value="https://livesim2.dashif.org/vod/testpic_2s/cea608.mpd">DASH-IF CEA-608 Captions</option>
      </optgroup>
      <optgroup label="DASH — Encrypted">
        <option value="https://storage.googleapis.com/shaka-demo-assets/angel-one-clearkey/dash.mpd">Shaka Angel One ClearKey</option>
        <option value="https://media.axprod.net/TestVectors/v7-MultiDRM-SingleKey/Manifest_1080p_ClearKey.mpd">Axinom ClearKey (single key)</option>
        <option value="https://media.axprod.net/TestVectors/v7-MultiDRM-MultiKey/Manifest_1080p_ClearKey.mpd">Axinom ClearKey (multi-key)</option>
      </optgroup>
      <optgroup label="Live / LL-HLS / LL-DASH">
        <option value="https://stream.mux.com/v69RSHhFelSm4701snP22dYz2jICy4E4FUyk02rW4gxRM.m3u8">Mux LL-HLS BBB (fMP4 loop)</option>
        <option value="https://livesim2.dashif.org/livesim2/utc_head/testpic_2s/Manifest.mpd">DASH-IF Live Sim (2s segments)</option>
        <option value="https://livesim2.dashif.org/livesim2/chunkdur_1/ato_7/testpic4_8s/Manifest300.mpd">DASH-IF LL-DASH (chunked)</option>
      </optgroup>
    </select>

    <label for="source-url">Source Manifest URL or Local Path
      <span id="format-hint" class="format-hint">HLS</span>
    </label>
    <input type="text" id="source-url" placeholder="https://cdn.example.com/master.m3u8 or ./path/to/manifest.mpd">

    <div id="speke-section">
      <label for="speke-url">SPEKE License Server URL</label>
      <input type="text" id="speke-url" placeholder="https://drm-provider.example.com/speke/v2">

      <label for="speke-auth-type">SPEKE Authentication</label>
      <select id="speke-auth-type">
        <option value="bearer">Bearer Token</option>
        <option value="api_key">API Key</option>
        <option value="basic">Basic Auth</option>
      </select>

      <label id="auth-value-label" for="speke-auth-value">Bearer Token</label>
      <input type="text" id="speke-auth-value" placeholder="your-token-here">

      <div id="api-key-header-row" class="hidden">
        <label for="api-key-header">API Key Header Name</label>
        <input type="text" id="api-key-header" placeholder="x-api-key" value="x-api-key">
      </div>
    </div>
    <p id="clear-hint" class="hidden" style="font-size:0.8rem;color:var(--text-muted);margin:0.25rem 0;">Source encryption is auto-detected. SPEKE fields hidden because target is clear (no encryption).</p>

    <label>Output Format</label>
    <div class="radio-group">
      <label><input type="radio" name="output-format" value="hls" checked> HLS (.m3u8)</label>
      <label><input type="radio" name="output-format" value="dash"> DASH (.mpd)</label>
    </div>

    <label>Target Encryption Scheme</label>
    <div class="radio-group">
      <label><input type="radio" name="target-scheme" value="cenc" checked> CENC (AES-CTR)</label>
      <label><input type="radio" name="target-scheme" value="cbcs"> CBCS (AES-CBC)</label>
      <label><input type="radio" name="target-scheme" value="both"> Both (Dual-Scheme)</label>
      <label><input type="radio" name="target-scheme" value="none"> None (Clear)</label>
    </div>

    <label>Container Format</label>
    <div class="radio-group">
      <label><input type="radio" name="container-format" value="cmaf" checked> CMAF (.cmfv)</label>
      <label><input type="radio" name="container-format" value="fmp4"> fMP4 (.m4s)</label>
      <label><input type="radio" name="container-format" value="iso"> ISO BMFF (.mp4)</label>
      <label><input type="radio" name="container-format" value="ts"> MPEG-TS (.ts)</label>
    </div>

    <details style="margin-bottom:1rem;">
      <summary style="cursor:pointer;font-size:0.8rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">Cache-Control Overrides</summary>
      <div style="margin-top:0.75rem;">
        <label for="cc-seg-max-age">Segment max-age (seconds)</label>
        <input type="text" id="cc-seg-max-age" placeholder="31536000 (default)">

        <label for="cc-final-max-age">Final Manifest max-age (seconds)</label>
        <input type="text" id="cc-final-max-age" placeholder="31536000 (default)">

        <label for="cc-live-max-age">Live Manifest max-age (seconds)</label>
        <input type="text" id="cc-live-max-age" placeholder="1 (default)">

        <label for="cc-live-s-maxage">Live Manifest s-maxage (seconds)</label>
        <input type="text" id="cc-live-s-maxage" placeholder="same as max-age (default)">

        <label style="display:flex;align-items:center;gap:0.5rem;text-transform:none;letter-spacing:0;font-size:0.875rem;color:var(--text);cursor:pointer;">
          <input type="checkbox" id="cc-immutable" checked style="accent-color:var(--accent);"> Include immutable directive
        </label>
      </div>
    </details>

    <button id="submit-btn" onclick="startRepackage()">Repackage</button>
  </div>
  
  <div id="player-panel" class="card">
    <h3 style="font-size:0.9rem;font-weight:600;margin-bottom:0.75rem;">Progressive Playback</h3>
    <video id="player-video" controls playsinline></video>
    <div class="player-status">
      <span id="player-badge"></span>
      <span id="player-info"></span>
    </div>
  </div>

  <div id="status-panel" class="card">
    <div class="status-text">
      <span id="status-state" class="state">Pending</span>
      <span id="status-segments"></span>
    </div>
    <div class="progress-bar">
      <div id="progress-fill" class="progress-fill"></div>
    </div>
    <div id="skipped-variants" class="hidden" style="margin-top:0.75rem;"></div>
    <div id="error-section" class="hidden" style="margin-top:0.75rem;padding:0.75rem;background:rgba(239,68,68,0.1);border:1px solid var(--error);border-radius:0.5rem;">
      <div style="font-size:0.8rem;color:var(--error);font-weight:600;margin-bottom:0.25rem;">Pipeline Error</div>
      <pre id="error-detail" style="font-size:0.75rem;color:var(--text-muted);white-space:pre-wrap;word-break:break-all;margin:0;"></pre>
    </div>
    <div id="output-section" class="hidden">
      <div class="output-info">
        Output written to <code id="output-dir"></code>
      </div>
      <div class="output-links" id="output-links"></div>
    </div>
  </div>

  <div id="validation-panel" class="card hidden">
    <h3 style="font-size:0.9rem;font-weight:600;margin-bottom:0.75rem;">Spec Compliance</h3>
    <div id="validation-results"></div>
  </div>

  <div id="manifest-panel" class="card hidden">
    <div style="display:flex;justify-content:space-between;align-items:center;margin-bottom:0.75rem;">
      <h3 style="font-size:0.9rem;font-weight:600;">Manifest Preview</h3>
      <select id="manifest-selector" style="width:auto;margin:0;padding:0.25rem 0.5rem;font-size:0.75rem;"></select>
    </div>
    <pre id="manifest-content" style="font-size:0.7rem;color:var(--text-muted);background:var(--bg);padding:0.75rem;border-radius:0.5rem;overflow-x:auto;max-height:400px;overflow-y:auto;white-space:pre;margin:0;line-height:1.4;"></pre>
  </div>
</div>

<script>
const sourceInput = document.getElementById('source-url');
const sourcePreset = document.getElementById('source-preset');
const formatHint = document.getElementById('format-hint');
const authType = document.getElementById('speke-auth-type');
const authLabel = document.getElementById('auth-value-label');
const authValue = document.getElementById('speke-auth-value');
const apiKeyRow = document.getElementById('api-key-header-row');

sourcePreset.addEventListener('change', () => {
  const url = sourcePreset.value;
  if (url) {
    sourceInput.value = url;
    sourceInput.dispatchEvent(new Event('input'));
    // Auto-select output format based on source type
    if (url.includes('.mpd')) {
      document.querySelector('input[name="output-format"][value="dash"]').checked = true;
    } else if (url.includes('.m3u8')) {
      document.querySelector('input[name="output-format"][value="hls"]').checked = true;
    }
  }
});

sourceInput.addEventListener('input', () => {
  const v = sourceInput.value.toLowerCase();
  if (v.includes('.m3u8')) {
    formatHint.textContent = 'HLS';
    formatHint.classList.add('visible');
  } else if (v.includes('.mpd')) {
    formatHint.textContent = 'DASH';
    formatHint.classList.add('visible');
  } else {
    formatHint.classList.remove('visible');
  }
});

authType.addEventListener('change', () => {
  const t = authType.value;
  if (t === 'bearer') {
    authLabel.textContent = 'Bearer Token';
    authValue.placeholder = 'your-token-here';
    apiKeyRow.classList.add('hidden');
  } else if (t === 'api_key') {
    authLabel.textContent = 'API Key Value';
    authValue.placeholder = 'your-api-key';
    apiKeyRow.classList.remove('hidden');
  } else {
    authLabel.textContent = 'Credentials (username:password)';
    authValue.placeholder = 'user:pass';
    apiKeyRow.classList.add('hidden');
  }
});

// Toggle SPEKE section visibility based on target encryption scheme
const spekeSection = document.getElementById('speke-section');
const clearHint = document.getElementById('clear-hint');
document.querySelectorAll('input[name="target-scheme"]').forEach(radio => {
  radio.addEventListener('change', () => {
    const scheme = document.querySelector('input[name="target-scheme"]:checked').value;
    if (scheme === 'none') {
      spekeSection.classList.add('hidden');
      clearHint.classList.remove('hidden');
    } else {
      spekeSection.classList.remove('hidden');
      clearHint.classList.add('hidden');
    }
  });
});

let pollTimer = null;
let playerInitialized = false;
let hlsPlayer = null;
let dashPlayer = null;

// Lazy-load a script from CDN (deduplicates)
const loadedScripts = {};
function loadScript(src) {
  if (loadedScripts[src]) return loadedScripts[src];
  loadedScripts[src] = new Promise((resolve, reject) => {
    const s = document.createElement('script');
    s.src = src;
    s.onload = resolve;
    s.onerror = reject;
    document.head.appendChild(s);
  });
  return loadedScripts[src];
}

function destroyPlayer() {
  if (hlsPlayer) { hlsPlayer.destroy(); hlsPlayer = null; }
  if (dashPlayer) { dashPlayer.reset(); dashPlayer = null; }
  const video = document.getElementById('player-video');
  video.pause();
  video.removeAttribute('src');
  video.load();
  playerInitialized = false;
  document.getElementById('player-panel').style.display = 'none';
  document.getElementById('player-badge').innerHTML = '';
  document.getElementById('player-info').textContent = '';
}

async function initPlayer(contentId, format, scheme, containerFormat) {
  if (playerInitialized) return;
  playerInitialized = true;

  const panel = document.getElementById('player-panel');
  const video = document.getElementById('player-video');
  const badge = document.getElementById('player-badge');
  const info = document.getElementById('player-info');
  panel.style.display = 'block';

  // Encrypted content cannot play without a license server
  if (scheme !== 'none') {
    badge.innerHTML = '';
    info.textContent = 'Encrypted (' + scheme.toUpperCase() + ') \u2014 playback requires a license server';
    video.style.display = 'none';
    return;
  }
  video.style.display = 'block';

  const fmtScheme = format + '_' + scheme;
  const ext = format === 'hls' ? 'm3u8' : 'mpd';
  const manifestUrl = '/api/output/' + contentId + '/' + fmtScheme + '/manifest.' + ext;

  badge.innerHTML = '<span class="live-badge">LIVE</span>';
  info.textContent = ' Playing progressive stream \u2014 segments arriving...';

  try {
    if (format === 'hls') {
      await loadScript('https://cdn.jsdelivr.net/npm/hls.js@latest');
      if (!Hls.isSupported()) {
        // Safari native HLS
        video.src = manifestUrl;
        video.play().catch(function(){});
        return;
      }
      hlsPlayer = new Hls({
        liveSyncDurationCount: 1,
        manifestLoadingMaxRetry: 60,
        manifestLoadingRetryDelay: 500,
        levelLoadingMaxRetry: 60,
        levelLoadingRetryDelay: 500,
        fragLoadingMaxRetry: 10,
      });
      hlsPlayer.loadSource(manifestUrl);
      hlsPlayer.attachMedia(video);
      hlsPlayer.on(Hls.Events.MANIFEST_PARSED, function() {
        video.play().catch(function(){});
      });
    } else {
      await loadScript('https://cdn.dashjs.org/latest/dash.all.min.js');
      dashPlayer = dashjs.MediaPlayer().create();
      dashPlayer.initialize(video, manifestUrl, true);
      dashPlayer.updateSettings({
        streaming: {
          delay: { liveDelay: 0, useSuggestedPresentationDelay: true },
          retryAttempts: { MPD: 60, MediaSegment: 10 },
          retryIntervals: { MPD: 500, MediaSegment: 500 },
        }
      });
    }
  } catch (e) {
    info.textContent = 'Player error: ' + e.message;
  }
}

function updatePlayerBadge(isComplete) {
  const badge = document.getElementById('player-badge');
  const info = document.getElementById('player-info');
  if (!playerInitialized) return;
  // Only update for clear content (encrypted shows its own message)
  const video = document.getElementById('player-video');
  if (video.style.display === 'none') return;
  if (isComplete) {
    badge.innerHTML = '<span class="vod-badge">VOD</span>';
    info.textContent = ' All segments processed \u2014 playback complete';
  }
}

function resetPanels() {
  destroyPlayer();
  document.getElementById('status-panel').style.display = 'block';
  document.getElementById('status-state').textContent = 'Processing';
  document.getElementById('status-state').className = 'state';
  document.getElementById('status-segments').textContent = '';
  document.getElementById('progress-fill').style.width = '10%';
  document.getElementById('progress-fill').className = 'progress-fill';
  document.getElementById('output-section').classList.add('hidden');
  document.getElementById('error-section').classList.add('hidden');
  document.getElementById('validation-panel').classList.add('hidden');
  document.getElementById('manifest-panel').classList.add('hidden');
}

async function startRepackage() {
  const btn = document.getElementById('submit-btn');
  btn.disabled = true;
  btn.textContent = 'Processing...';

  const outputFormat = document.querySelector('input[name="output-format"]:checked').value;
  const targetSchemeValue = document.querySelector('input[name="target-scheme"]:checked').value;
  const containerFormat = document.querySelector('input[name="container-format"]:checked').value;
  const targetSchemes = targetSchemeValue === 'both' ? ['cenc', 'cbcs'] : [targetSchemeValue];

  // Build cache_control overrides
  const ccSegMaxAge = document.getElementById('cc-seg-max-age').value;
  const ccFinalMaxAge = document.getElementById('cc-final-max-age').value;
  const ccLiveMaxAge = document.getElementById('cc-live-max-age').value;
  const ccLiveSMaxAge = document.getElementById('cc-live-s-maxage').value;
  const ccImmutable = document.getElementById('cc-immutable').checked;
  let cacheControl = null;
  if (ccSegMaxAge || ccFinalMaxAge || ccLiveMaxAge || ccLiveSMaxAge || !ccImmutable) {
    cacheControl = {};
    if (ccSegMaxAge) cacheControl.segment_max_age = parseInt(ccSegMaxAge, 10);
    if (ccFinalMaxAge) cacheControl.final_manifest_max_age = parseInt(ccFinalMaxAge, 10);
    if (ccLiveMaxAge) cacheControl.live_manifest_max_age = parseInt(ccLiveMaxAge, 10);
    if (ccLiveSMaxAge) cacheControl.live_manifest_s_maxage = parseInt(ccLiveSMaxAge, 10);
    if (!ccImmutable) cacheControl.immutable = false;
  }

  // Pre-load the player library now so it's ready before the first segment arrives.
  // This ensures the most accurate JITP demo — the player is waiting for segments,
  // not the other way around.
  const isEncrypted = targetSchemes.length === 1 && targetSchemes[0] === 'none' ? false : true;
  if (!isEncrypted) {
    if (outputFormat === 'hls') {
      loadScript('https://cdn.jsdelivr.net/npm/hls.js@latest');
    } else {
      loadScript('https://cdn.dashjs.org/latest/dash.all.min.js');
    }
  }

  const body = {
    source_url: sourceInput.value,
    speke_url: document.getElementById('speke-url').value,
    speke_auth_type: authType.value,
    speke_auth_value: authValue.value,
    speke_api_key_header: document.getElementById('api-key-header').value,
    output_format: outputFormat,
    target_schemes: targetSchemes,
    container_format: containerFormat,
    cache_control: cacheControl,
  };

  try {
    const resp = await fetch('/api/repackage', {
      method: 'POST',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    const data = await resp.json();

    if (!resp.ok) {
      alert('Error: ' + (data.error || 'unknown error'));
      btn.disabled = false;
      btn.textContent = 'Repackage';
      return;
    }

    resetPanels();

    if (pollTimer) clearInterval(pollTimer);
    const storedContainerFormat = data.container_format || 'cmaf';
    pollTimer = setInterval(() => pollStatus(data.content_id, data.format, storedContainerFormat, targetSchemes), 500);
  } catch (e) {
    alert('Request failed: ' + e.message);
    btn.disabled = false;
    btn.textContent = 'Repackage';
  }
}

function formatBytes(bytes) {
  if (bytes < 1024) return bytes + ' B';
  if (bytes < 1048576) return (bytes / 1024).toFixed(1) + ' KB';
  return (bytes / 1048576).toFixed(2) + ' MB';
}

async function pollStatus(contentId, format, containerFormat, targetSchemes) {
  try {
    const resp = await fetch(`/api/status/${contentId}/${format}`);
    if (!resp.ok) return;
    const data = await resp.json();

    const stateEl = document.getElementById('status-state');
    const segEl = document.getElementById('status-segments');
    const fill = document.getElementById('progress-fill');

    stateEl.textContent = data.state;
    stateEl.className = 'state';

    if (data.state === 'Processing') {
      if (data.segments_total) {
        const pct = Math.round((data.segments_completed / data.segments_total) * 100);
        segEl.textContent = ` \u2014 ${data.segments_completed}/${data.segments_total} segments`;
        fill.style.width = pct + '%';
      } else if (data.segments_completed > 0) {
        segEl.textContent = ` \u2014 ${data.segments_completed} segments processed`;
        // Animate progress bar without total (pulse between 30-70%)
        const pulse = 30 + Math.min(40, data.segments_completed / 50);
        fill.style.width = pulse + '%';
      } else {
        fill.style.width = '15%';
        segEl.textContent = ' \u2014 fetching and repackaging...';
      }
    }

    // Show skipped variants if any
    const skippedEl = document.getElementById('skipped-variants');
    if (skippedEl && data.skipped_variants && data.skipped_variants.length > 0) {
      let html = `<div class="check-warnings">\u26a0 ${data.skipped_variants.length} variant(s) skipped:</div>`;
      for (const sv of data.skipped_variants) {
        const res = (sv.width && sv.height) ? `${sv.width}x${sv.height}` : '?';
        html += `<div class="check-warnings" style="margin-left:1rem;font-size:0.65rem;">${res} @ ${Math.round(sv.bandwidth/1000)}k (${sv.codecs || '?'}, ${sv.mime_type || '?'})</div>`;
      }
      skippedEl.innerHTML = html;
      skippedEl.classList.remove('hidden');
    }

    // Start player as soon as playback is ready (init + first segment on disk)
    if (data.playback_ready && !playerInitialized && data.schemes && data.schemes.length > 0) {
      // Prefer 'none' scheme for playback (clear content plays without DRM)
      const playScheme = data.schemes.includes('none') ? 'none' : data.schemes[0];
      initPlayer(contentId, format, playScheme, containerFormat);
    }

    if (data.state === 'Complete') {
      stateEl.classList.add('complete');
      fill.classList.add('complete');
      fill.style.width = '100%';
      clearInterval(pollTimer);
      pollTimer = null;

      const btn = document.getElementById('submit-btn');
      btn.disabled = false;
      btn.textContent = 'Repackage';

      // Initialize player on Complete if it wasn't started during processing
      if (!playerInitialized && data.schemes && data.schemes.length > 0) {
        const playScheme = data.schemes.includes('none') ? 'none' : data.schemes[0];
        initPlayer(contentId, format, playScheme, containerFormat);
      }
      updatePlayerBadge(true);

      // Show output links
      if (data.output_dir) {
        document.getElementById('output-dir').textContent = data.output_dir;
        const linksEl = document.getElementById('output-links');
        linksEl.innerHTML = '';

        const ext = format === 'hls' ? 'm3u8' : 'mpd';
        const videoExt = containerFormat === 'ts' ? '.ts' : containerFormat === 'fmp4' ? '.m4s' : containerFormat === 'iso' ? '.mp4' : '.cmfv';
        const audioExt = containerFormat === 'ts' ? '.ts' : containerFormat === 'fmp4' ? '.m4s' : containerFormat === 'iso' ? '.mp4' : '.cmfa';
        const isTs = containerFormat === 'ts';
        const schemes = data.schemes || targetSchemes;
        for (const scheme of schemes) {
          const fmtScheme = `${format}_${scheme}`;
          const base = `/api/output/${contentId}/${fmtScheme}`;
          linksEl.innerHTML += `<a href="${base}/manifest.${ext}" target="_blank">${fmtScheme}/manifest.${ext}</a>`;
          if (!isTs) {
            linksEl.innerHTML += `<a href="${base}/init.mp4" target="_blank">${fmtScheme}/init.mp4</a>`;
          }
          for (let i = 0; i < data.segments_completed; i++) {
            linksEl.innerHTML += `<a href="${base}/segment_${i}${videoExt}" target="_blank">${fmtScheme}/segment_${i}${videoExt}</a>`;
          }
          // Check for audio output files (separate rendition — prefixed with audio_)
          const audioInit = await fetch(`${base}/audio_init.mp4`, {method:'HEAD'}).catch(()=>null);
          if (audioInit && audioInit.ok) {
            // manifest.m3u8 is now a master playlist — also show video + audio media playlists
            linksEl.innerHTML += `<a href="${base}/video.${ext}" target="_blank" style="border-color:var(--accent);">${fmtScheme}/video.${ext}</a>`;
            linksEl.innerHTML += `<a href="${base}/audio.${ext}" target="_blank" style="border-color:var(--success);">${fmtScheme}/audio.${ext}</a>`;
            linksEl.innerHTML += `<a href="${base}/audio_init.mp4" target="_blank" style="border-color:var(--success);">${fmtScheme}/audio_init.mp4</a>`;
            for (let i = 0; i < data.segments_completed; i++) {
              const segResp = await fetch(`${base}/audio_segment_${i}${audioExt}`, {method:'HEAD'}).catch(()=>null);
              if (segResp && segResp.ok) {
                linksEl.innerHTML += `<a href="${base}/audio_segment_${i}${audioExt}" target="_blank" style="border-color:var(--success);">${fmtScheme}/audio_segment_${i}${audioExt}</a>`;
              }
            }
          }
        }
        document.getElementById('output-section').classList.remove('hidden');
      }

      // Show validation results
      if (data.validation) {
        renderValidation(data.validation, data.timing);
      }

      // Show manifest preview
      const schemes = data.schemes || targetSchemes;
      if (schemes.length > 0) {
        loadManifestPreview(contentId, format, schemes);
      }
    } else if (data.state === 'Failed') {
      stateEl.classList.add('failed');
      fill.classList.add('error');
      fill.style.width = '100%';
      clearInterval(pollTimer);
      pollTimer = null;

      const btn = document.getElementById('submit-btn');
      btn.disabled = false;
      btn.textContent = 'Repackage';

      if (data.error) {
        document.getElementById('error-detail').textContent = data.error;
        document.getElementById('error-section').classList.remove('hidden');
      }
    }
  } catch (e) {
    // Ignore polling errors
  }
}

function renderValidation(validations, timing) {
  const panel = document.getElementById('validation-panel');
  const container = document.getElementById('validation-results');
  container.innerHTML = '';

  // Timing stats
  if (timing) {
    const coldStartMs = timing.cold_start_us ? (timing.cold_start_us / 1000).toFixed(2) : '< 1';
    const pipelineMs = timing.total_pipeline_ms || 0;
    const firstSegMs = timing.first_segment_ms || 0;
    const totalCacheMiss = firstSegMs + (timing.cold_start_us ? timing.cold_start_us / 1000 : 0);
    const totalComplete = pipelineMs + (timing.cold_start_us ? timing.cold_start_us / 1000 : 0);
    const perSegMs = timing.per_segment_ms || [];
    const statsHtml = `<div style="display:grid;grid-template-columns:repeat(4,1fr);gap:0.75rem;margin-bottom:1rem;padding:0.75rem;background:var(--bg);border-radius:0.5rem;">
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--success);">${coldStartMs}ms</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">WASM Cold Start</div>
        <div style="font-size:0.55rem;color:var(--text-muted);">${timing.wasm_binary_kb || '~628'} KB binary</div>
      </div>
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--success);">${firstSegMs}ms</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">First Segment</div>
        <div style="font-size:0.55rem;color:var(--text-muted);">playback-ready</div>
      </div>
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--accent);">${pipelineMs}ms</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">All Segments</div>
        <div style="font-size:0.55rem;color:var(--text-muted);">${timing.total_segments} segments</div>
      </div>
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--accent);">${timing.throughput_mbps ? timing.throughput_mbps.toFixed(1) : 0} Mbps</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">Throughput</div>
      </div>
    </div>
    <div style="font-size:0.7rem;color:var(--text-muted);margin-bottom:0.75rem;padding:0 0.25rem;">
      <strong style="color:var(--text);">Cache miss \u2192 first viewer: ${totalCacheMiss.toFixed(1)}ms</strong> (${coldStartMs}ms cold start + ${firstSegMs}ms to first segment).
      All ${timing.total_segments} segments complete in <strong style="color:var(--text);">${totalComplete.toFixed(1)}ms</strong> (${formatBytes(timing.total_bytes)}).
    </div>` +
    // Progressive timeline bar
    (perSegMs.length > 1 ? `<div style="margin-bottom:1rem;padding:0 0.25rem;">
      <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;margin-bottom:0.375rem;">Repackaging Timeline</div>
      <div style="display:flex;height:20px;border-radius:0.25rem;overflow:hidden;background:var(--bg);border:1px solid var(--border);">${perSegMs.map((ms, i) => {
        const pct = (ms / pipelineMs) * 100;
        const hue = i === 0 ? 142 : 235;
        const opacity = 0.5 + 0.5 * (1 - i / perSegMs.length);
        return '<div title="Seg ' + i + ': ' + ms + 'ms" style="width:' + Math.max(pct, 0.5) + '%;background:hsl(' + hue + ',60%,' + Math.round(50 * opacity) + '%);border-right:1px solid var(--bg);"></div>';
      }).join('')}</div>
      <div style="display:flex;justify-content:space-between;font-size:0.55rem;color:var(--text-muted);margin-top:0.25rem;">
        <span>Seg 0: ${perSegMs[0]}ms (fetch + init + repackage)</span>
        <span>Avg remaining: ${perSegMs.length > 1 ? Math.round(perSegMs.slice(1).reduce((a,b)=>a+b,0) / (perSegMs.length-1)) : 0}ms/seg</span>
      </div>
    </div>` : '');
    container.innerHTML += statsHtml;
  }

  // Validation checks per output
  for (const v of validations) {
    let html = `<div class="validation-output">`;
    html += `<div class="validation-header">
      <span class="badge ${v.pass ? 'pass' : 'fail'}">${v.pass ? 'PASS' : 'FAIL'}</span>
      <span>${v.output}</span>
    </div>`;

    for (const check of v.checks) {
      const icon = check.pass ? '\u2713' : '\u2717';
      const cls = check.pass ? 'pass' : 'fail';
      html += `<div class="check-item">
        <span class="check-icon ${cls}">${icon}</span>
        <span>${check.name}</span>
      </div>`;
      if (check.warnings && check.warnings.length > 0) {
        html += `<div class="check-warnings">\u26a0 ${check.warnings.join(', ')}</div>`;
      }
      if (check.errors && check.errors.length > 0) {
        html += `<div class="check-warnings" style="color:var(--error);">\u2717 ${check.errors.join(', ')}</div>`;
      }
    }
    html += `</div>`;
    container.innerHTML += html;
  }

  panel.classList.remove('hidden');
}

async function loadManifestPreview(contentId, format, schemes) {
  const panel = document.getElementById('manifest-panel');
  const selector = document.getElementById('manifest-selector');
  const content = document.getElementById('manifest-content');
  selector.innerHTML = '';

  const manifests = {};
  for (const scheme of schemes) {
    const fmtScheme = `${format}_${scheme}`;
    try {
      const resp = await fetch(`/api/manifest/${contentId}/${fmtScheme}`);
      if (resp.ok) {
        manifests[fmtScheme] = await resp.text();
        const opt = document.createElement('option');
        opt.value = fmtScheme;
        opt.textContent = fmtScheme;
        selector.appendChild(opt);
      }
    } catch (e) { /* skip */ }
  }

  if (Object.keys(manifests).length > 0) {
    const first = Object.keys(manifests)[0];
    content.textContent = manifests[first];
    selector.addEventListener('change', () => {
      content.textContent = manifests[selector.value] || '';
    });
    panel.classList.remove('hidden');
  }
}
</script>
</body>
</html>
"#;
