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
use edgepack::repackager::RepackageRequest;
use edgepack::repackager::SourceConfig;

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
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

/// Result of resolving a master playlist to media playlist(s).
struct ResolvedSource {
    video_url: String,
    audio_url: Option<String>,
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
                    error: "container_format must be 'cmaf', 'fmp4', or 'iso'".into(),
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
    tokio::task::spawn_blocking(move || {
        let cache = edgepack::cache::global_cache();
        let state_key = CacheKeys::job_state(&cid, fmt_str);

        // Write initial "Processing" state
        let initial_state = serde_json::json!({
            "state": "Processing",
            "segments_completed": 0,
            "segments_total": null,
        });
        let _ = cache.set(
            &state_key,
            &serde_json::to_vec(&initial_state).unwrap(),
            3600,
        );

        let pipeline_start = Instant::now();
        // Clone config for audio pipeline if we have a separate audio rendition
        let audio_config = audio_source.as_ref().map(|_| config.clone());
        let pipeline = RepackagePipeline::new(config);
        match pipeline.execute(&request) {
            Ok(outputs) => {
                let pipeline_elapsed = pipeline_start.elapsed();
                eprintln!(
                    "  Pipeline complete: {}/{} — {} output(s) in {:.1}s",
                    cid,
                    fmt_str,
                    outputs.len(),
                    pipeline_elapsed.as_secs_f64()
                );

                let mut total_segments = 0u32;
                let mut scheme_list = Vec::new();
                let mut validation_results = Vec::new();

                // Write output per (format, scheme) pair
                for (out_format, scheme, output) in &outputs {
                    let scheme_str = scheme.scheme_type_str();
                    scheme_list.push(scheme_str.to_string());
                    let seg_count = output.manifest_state().segments.len() as u32;
                    total_segments = total_segments.max(seg_count);

                    // Clean previous output before writing
                    let out_dir = PathBuf::from(format!(
                        "sandbox/output/{cid}/{}_{scheme_str}",
                        match out_format {
                            OutputFormat::Hls => "hls",
                            OutputFormat::Dash => "dash",
                        }
                    ));
                    if out_dir.exists() {
                        let _ = std::fs::remove_dir_all(&out_dir);
                    }

                    if let Err(e) = write_output_to_disk(&cid, *out_format, scheme_str, output, false) {
                        let fmt_label = match out_format {
                            OutputFormat::Hls => "hls",
                            OutputFormat::Dash => "dash",
                        };
                        eprintln!("  Warning: failed to write {fmt_label}_{scheme_str} output to disk: {e}");
                    }

                    // Run compliance validation (includes audio track detection)
                    let validation = validate_output(&cid, *out_format, scheme_str, output);
                    validation_results.push(validation);
                }

                // Process separate audio rendition if present
                let mut audio_segments = 0u32;
                if let (Some(audio_source), Some(audio_cfg)) = (audio_source, audio_config) {
                    eprintln!("  Processing separate audio rendition: {}", audio_source);
                    let audio_request = RepackageRequest {
                        content_id: format!("{cid}_audio"),
                        source_url: audio_source,
                        output_formats: request.output_formats.clone(),
                        target_schemes: request.target_schemes.clone(),
                        container_format: request.container_format,
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
                    let audio_pipeline = RepackagePipeline::new(audio_cfg);
                    match audio_pipeline.execute(&audio_request) {
                        Ok(audio_outputs) => {
                            for (out_format, scheme, output) in &audio_outputs {
                                let scheme_str = scheme.scheme_type_str();
                                let audio_seg_count = output.manifest_state().segments.len() as u32;
                                audio_segments = audio_segments.max(audio_seg_count);
                                if let Err(e) = write_output_to_disk(&cid, *out_format, scheme_str, output, true) {
                                    eprintln!("  Warning: failed to write audio output: {e}");
                                }
                                // Validate audio output
                                let mut audio_validation = validate_output(&cid, *out_format, scheme_str, output);
                                // Tag as audio output
                                if let Some(obj) = audio_validation.as_object_mut() {
                                    let label = obj.get("output").and_then(|v| v.as_str()).unwrap_or("").to_string();
                                    obj.insert("output".to_string(), serde_json::json!(format!("{label} (audio)")));
                                }
                                validation_results.push(audio_validation);
                            }
                            eprintln!("  Audio pipeline complete: {} audio segments", audio_segments);
                        }
                        Err(e) => {
                            eprintln!("  Warning: audio pipeline failed: {e}");
                            validation_results.push(serde_json::json!({
                                "output": "audio",
                                "pass": false,
                                "checks": [{
                                    "name": "Audio pipeline execution",
                                    "pass": false,
                                    "detail": format!("{e}"),
                                }],
                            }));
                        }
                    }
                }

                // Calculate total output bytes
                let total_bytes: u64 = outputs
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

                // Estimate WASM cold start from binary size on disk
                let wasm_binary_size = std::fs::metadata("target/wasm32-wasip2/release/edgepack.wasm")
                    .map(|m| m.len())
                    .unwrap_or(628_000); // fallback: known ~628KB
                // Empirical: ~0.5ms per 500KB on modern runtimes (wasmtime, V8)
                let cold_start_us = (wasm_binary_size as f64 / 500_000.0 * 500.0) as u64;

                // Write "Complete" state with segment count, validation, and timing
                let complete_state = serde_json::json!({
                    "state": "Complete",
                    "segments_completed": total_segments,
                    "segments_total": total_segments,
                    "schemes": scheme_list,
                    "validation": validation_results,
                    "timing": {
                        "total_pipeline_ms": pipeline_elapsed.as_millis() as u64,
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
            }
            Err(e) => {
                let pipeline_elapsed = pipeline_start.elapsed();
                eprintln!("  Pipeline failed after {:.1}s: {e}", pipeline_elapsed.as_secs_f64());

                // Write "Failed" state with error
                let failed_state = serde_json::json!({
                    "state": "Failed",
                    "segments_completed": 0,
                    "segments_total": null,
                    "error": format!("{e}"),
                });
                let _ = cache.set(
                    &state_key,
                    &serde_json::to_vec(&failed_state).unwrap(),
                    3600,
                );
            }
        }
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
                let output_dir = if state_str == "Complete" {
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

/// If the given URL points to an HLS master playlist (multivariant), fetch it,
/// pick the highest-bandwidth variant, and return the resolved media playlist URL(s).
/// Also extracts separate audio rendition URLs when present.
/// If it's already a media playlist (or not HLS), return the original URL unchanged.
async fn resolve_master_playlist(url: &str) -> Result<ResolvedSource, String> {
    // Only attempt resolution for .m3u8 URLs
    if !url.to_lowercase().contains(".m3u8") {
        return Ok(ResolvedSource { video_url: url.to_string(), audio_url: None });
    }

    // Also handle .mpd URLs that might be passed with .m3u8 in path
    if url.to_lowercase().ends_with(".mpd") {
        return Ok(ResolvedSource { video_url: url.to_string(), audio_url: None });
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
        return Ok(ResolvedSource { video_url: url.to_string(), audio_url: None });
    }

    eprintln!("  Detected HLS master playlist — resolving to media playlist...");

    let base = url.rfind('/').map(|i| &url[..=i]).unwrap_or(url);

    // Parse audio renditions (#EXT-X-MEDIA:TYPE=AUDIO with URI)
    let mut audio_uri: Option<String> = None;
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#EXT-X-MEDIA:") && trimmed.contains("TYPE=AUDIO") {
            if let Some(uri_start) = trimmed.find("URI=\"") {
                let after_uri = &trimmed[uri_start + 5..];
                if let Some(uri_end) = after_uri.find('"') {
                    let uri = &after_uri[..uri_end];
                    if !uri.is_empty() {
                        audio_uri = Some(uri.to_string());
                        break; // Take the first audio rendition
                    }
                }
            }
        }
    }

    // Parse variant streams: extract (bandwidth, relative_url) pairs
    let mut best_bandwidth: u64 = 0;
    let mut best_url: Option<String> = None;
    let mut next_is_url = false;

    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("#EXT-X-STREAM-INF:") {
            // Extract bandwidth
            if let Some(bw_str) = trimmed
                .split(',')
                .find(|s| s.contains("BANDWIDTH="))
                .and_then(|s| s.split('=').nth(1))
            {
                if let Ok(bw) = bw_str.trim().parse::<u64>() {
                    if bw > best_bandwidth {
                        best_bandwidth = bw;
                        next_is_url = true;
                        continue;
                    }
                }
            }
            next_is_url = true;
        } else if next_is_url && !trimmed.is_empty() && !trimmed.starts_with('#') {
            if best_bandwidth > 0 || best_url.is_none() {
                best_url = Some(trimmed.to_string());
            }
            next_is_url = false;
        }
    }

    let variant_path = best_url.ok_or("no variant streams found in master playlist")?;

    // Resolve relative URLs against master URL base
    let video_url = if variant_path.starts_with("http://") || variant_path.starts_with("https://") {
        variant_path
    } else {
        format!("{base}{variant_path}")
    };

    let audio_url = audio_uri.map(|uri| {
        if uri.starts_with("http://") || uri.starts_with("https://") {
            uri
        } else {
            format!("{base}{uri}")
        }
    });

    eprintln!(
        "  Selected variant: {} (bandwidth: {})",
        video_url, best_bandwidth
    );
    if let Some(ref audio) = audio_url {
        eprintln!("  Audio rendition: {}", audio);
    } else {
        eprintln!("  Audio: muxed with video (no separate rendition)");
    }

    Ok(ResolvedSource { video_url, audio_url })
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
                    "warnings": ["Audio may be in a separate rendition — check output for .cmfa segments"],
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

fn write_output_to_disk(
    content_id: &str,
    format: OutputFormat,
    scheme: &str,
    output: &ProgressiveOutput,
    is_audio: bool,
) -> Result<(), String> {
    let fmt_str = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };

    let out_dir = PathBuf::from(format!("sandbox/output/{content_id}/{fmt_str}_{scheme}"));
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| format!("create output dir: {e}"))?;

    let state = output.manifest_state();
    let track_label = if is_audio { "audio" } else { "video" };

    // Write manifest
    if let Ok(rendered) = manifest::render_manifest(state) {
        let ext = format.manifest_extension();
        if is_audio {
            // For audio output, write a separate audio manifest with corrected extensions
            let video_ext = state.container_format.video_segment_extension();
            let audio_ext = state.container_format.audio_segment_extension();
            let audio_rendered = rendered
                .replace(video_ext, audio_ext)
                .replace("init.mp4", "audio_init.mp4");
            let manifest_path = out_dir.join(format!("audio_manifest.{ext}"));
            std::fs::write(&manifest_path, audio_rendered)
                .map_err(|e| format!("write audio manifest: {e}"))?;
            eprintln!("  Wrote {}", manifest_path.display());
        } else {
            let manifest_path = out_dir.join(format!("manifest.{ext}"));
            std::fs::write(&manifest_path, rendered)
                .map_err(|e| format!("write manifest: {e}"))?;
            eprintln!("  Wrote {}", manifest_path.display());
        }
    }

    // Write init segment
    if let Some(data) = output.init_segment_data() {
        let init_name = if is_audio { "audio_init.mp4" } else { "init.mp4" };
        let init_path = out_dir.join(init_name);
        std::fs::write(&init_path, data).map_err(|e| format!("write {track_label} init segment: {e}"))?;
        eprintln!("  Wrote {} ({} bytes)", init_path.display(), data.len());
    }

    // Write media segments with track-appropriate extensions
    let seg_ext = if is_audio {
        state.container_format.audio_segment_extension()
    } else {
        state.container_format.video_segment_extension()
    };
    let mut seg_num = 0u32;
    for seg in &state.segments {
        if let Some(data) = output.segment_data(seg.number) {
            let seg_path = out_dir.join(format!("segment_{}{seg_ext}", seg.number));
            std::fs::write(&seg_path, data)
                .map_err(|e| format!("write {track_label} segment {}: {e}", seg.number))?;
            seg_num += 1;
        }
    }
    if seg_num > 0 {
        eprintln!("  Wrote {seg_num} {track_label} segments to {}", out_dir.display());
    }

    // Write I-frame playlist (HLS only, video track only)
    if !is_audio {
        if let Ok(Some(iframe_playlist)) = manifest::render_iframe_manifest(state) {
            let iframe_path = out_dir.join("iframes.m3u8");
            std::fs::write(&iframe_path, iframe_playlist)
                .map_err(|e| format!("write I-frame playlist: {e}"))?;
            eprintln!("  Wrote {}", iframe_path.display());
        }
    }

    Ok(())
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
</style>
</head>
<body>
<div class="container">
  <h1>edgepack sandbox</h1>
  <p class="subtitle">Local repackaging tool &mdash; configurable encryption &amp; container format</p>

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

  <div id="status-panel" class="card">
    <div class="status-text">
      <span id="status-state" class="state">Pending</span>
      <span id="status-segments"></span>
    </div>
    <div class="progress-bar">
      <div id="progress-fill" class="progress-fill"></div>
    </div>
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

function resetPanels() {
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
      fill.style.width = '30%';
      segEl.textContent = ' \u2014 fetching and repackaging...';
    }

    if (data.segments_total) {
      const pct = Math.round((data.segments_completed / data.segments_total) * 100);
      segEl.textContent = ` \u2014 ${data.segments_completed}/${data.segments_total} segments`;
      fill.style.width = pct + '%';
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
          // Check for audio output files (separate rendition)
          const audioInit = await fetch(`${base}/audio_init.mp4`, {method:'HEAD'}).catch(()=>null);
          if (audioInit && audioInit.ok) {
            linksEl.innerHTML += `<a href="${base}/audio_manifest.${ext}" target="_blank" style="border-color:var(--success);">${fmtScheme}/audio_manifest.${ext}</a>`;
            linksEl.innerHTML += `<a href="${base}/audio_init.mp4" target="_blank" style="border-color:var(--success);">${fmtScheme}/audio_init.mp4</a>`;
            for (let i = 0; i < data.segments_completed; i++) {
              const segResp = await fetch(`${base}/segment_${i}${audioExt}`, {method:'HEAD'}).catch(()=>null);
              if (segResp && segResp.ok) {
                linksEl.innerHTML += `<a href="${base}/segment_${i}${audioExt}" target="_blank" style="border-color:var(--success);">${fmtScheme}/segment_${i}${audioExt}</a>`;
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
    const totalCacheMiss = pipelineMs + (timing.cold_start_us ? timing.cold_start_us / 1000 : 0);
    const statsHtml = `<div style="display:grid;grid-template-columns:repeat(4,1fr);gap:0.75rem;margin-bottom:1rem;padding:0.75rem;background:var(--bg);border-radius:0.5rem;">
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--success);">${coldStartMs}ms</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">WASM Cold Start</div>
        <div style="font-size:0.55rem;color:var(--text-muted);">${timing.wasm_binary_kb || '~628'} KB binary</div>
      </div>
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--accent);">${pipelineMs}ms</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">Pipeline</div>
        <div style="font-size:0.55rem;color:var(--text-muted);">${timing.total_segments} segments</div>
      </div>
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--accent);">${timing.avg_segment_ms ? timing.avg_segment_ms.toFixed(0) : 0}ms</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">Avg/Segment</div>
      </div>
      <div style="text-align:center;">
        <div style="font-size:1.1rem;font-weight:700;color:var(--accent);">${timing.throughput_mbps ? timing.throughput_mbps.toFixed(1) : 0} Mbps</div>
        <div style="font-size:0.6rem;color:var(--text-muted);text-transform:uppercase;letter-spacing:0.05em;">Throughput</div>
      </div>
    </div>
    <div style="font-size:0.7rem;color:var(--text-muted);margin-bottom:1rem;padding:0 0.25rem;">
      <strong style="color:var(--text);">Cache miss \u2192 CDN-ready: ${totalCacheMiss.toFixed(1)}ms</strong> (${coldStartMs}ms cold start + ${pipelineMs}ms pipeline) for ${timing.total_segments} segments (${formatBytes(timing.total_bytes)}).
      Manifest + first segment available in <strong style="color:var(--text);">&lt;${Math.max(1, Math.round(pipelineMs / timing.total_segments))}ms</strong> via progressive output.
    </div>`;
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
