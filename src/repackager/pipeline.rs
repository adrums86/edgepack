use crate::cache::{CacheBackend, CacheKeys};
use crate::config::AppConfig;
use crate::drm::scheme::EncryptionScheme;
use crate::drm::speke::SpekeClient;
use crate::drm::{ContentKey, DrmKeySet};
use crate::error::{EdgepackError, Result};
use crate::manifest::types::{
    ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat, SegmentInfo, SourceManifest,
    TrackMediaType, VariantInfo,
};
use crate::media::codec::{extract_tracks, TrackInfo, TrackKeyMapping};
use crate::media::compat;
use crate::media::container::ContainerFormat;
use crate::media::init;
use crate::media::scte35;
use crate::media::segment::{self, SegmentRewriteParams};
use crate::repackager::progressive::ProgressiveOutput;
use crate::repackager::{JobState, JobStatus, RepackageRequest};

/// Result of a JIT setup operation.
///
/// Contains the manifest state (ready to render) and rewritten init segment.
#[cfg(feature = "jit")]
pub struct JitSetupResult {
    pub manifest_state: ManifestState,
    pub init_segment_data: Vec<u8>,
}

/// The main repackaging pipeline.
///
/// Orchestrates: fetch source -> get DRM keys -> repackage init segment ->
/// repackage media segments (progressively) -> finalize manifest.
pub struct RepackagePipeline {
    config: AppConfig,
    cache: Box<dyn CacheBackend>,
    speke: SpekeClient,
}

impl RepackagePipeline {
    pub fn new(config: AppConfig, cache: Box<dyn CacheBackend>) -> Self {
        let speke = SpekeClient::new(&config.drm);
        Self {
            config,
            cache,
            speke,
        }
    }

    /// Execute the full repackaging pipeline (processes all segments in one invocation).
    ///
    /// Produces one `ProgressiveOutput` per target scheme. For WASI environments with
    /// request timeouts, prefer `execute_first()` + `execute_remaining()` for chunked processing.
    pub fn execute(&self, request: &RepackageRequest) -> Result<(JobStatus, Vec<(EncryptionScheme, ProgressiveOutput)>)> {
        let content_id = &request.content_id;
        let format = request.output_format;
        let target_schemes = &request.target_schemes;
        let container_format = request.container_format;

        // Update job state: FetchingKeys
        self.update_job_state(content_id, format, JobState::FetchingKeys, 0, None)?;

        // Step 1: Fetch the source manifest to discover segments and encryption info
        let source = self.fetch_source_manifest(&request.source_url)?;

        // Step 2: Fetch the init segment and parse protection info
        let init_data = self.fetch_segment(&source.init_segment_url)?;
        let protection_info = init::parse_protection_info(&init_data)?;

        // Detect source encryption scheme
        let source_scheme = if let Some(ref info) = protection_info {
            EncryptionScheme::from_scheme_type(&info.scheme_type)
                .ok_or_else(|| EdgepackError::Drm(format!(
                    "unsupported encryption scheme: {:?}",
                    std::str::from_utf8(&info.scheme_type)
                )))?
        } else {
            EncryptionScheme::None
        };

        let source_pattern = protection_info.as_ref().map(|info| (
            info.tenc.default_crypt_byte_block,
            info.tenc.default_skip_byte_block,
        )).unwrap_or((0, 0));

        // Step 3: Extract track info for codec strings and per-track KIDs
        let tracks = extract_tracks(&init_data).unwrap_or_default();

        // Step 3a: Validate codec/scheme compatibility (pre-flight check)
        let validation = compat::validate_repackage_request(
            source_scheme,
            target_schemes,
            container_format,
            &tracks,
        );
        if !validation.errors.is_empty() {
            return Err(EdgepackError::InvalidInput(format!(
                "validation failed: {}",
                validation.errors.join("; ")
            )));
        }
        for warning in &validation.warnings {
            log::warn!("validation warning: {warning}");
        }

        let key_mapping = build_track_key_mapping(&tracks, &protection_info, content_id);
        let primary_kid = key_mapping.all_kids().into_iter().next()
            .unwrap_or_else(|| derive_kid_from_content_id(content_id));

        // Step 4: Conditional SPEKE — only needed when either side is encrypted
        let any_target_encrypted = target_schemes.iter().any(|s| s.is_encrypted());
        let needs_keys = source_scheme.is_encrypted() || any_target_encrypted;
        let (key_set, source_key, content_key) = if needs_keys {
            let key_ids = key_mapping.all_kids();
            let ks = self.get_or_fetch_keys(content_id, &key_ids)?;
            let key = find_key_for_kid(&ks, &primary_kid)?;
            let src = if source_scheme.is_encrypted() { Some(key.clone()) } else { None };
            (Some(ks), src, Some(key))
        } else {
            (None, None, None)
        };

        // Step 5: Per-scheme init rewriting and progressive output setup
        let fmt = format_str(format);

        let mut outputs: Vec<(EncryptionScheme, ProgressiveOutput)> = Vec::with_capacity(target_schemes.len());
        for &target_scheme in target_schemes {
            let target_iv_size = target_scheme.default_iv_size();
            let target_pattern = target_scheme.default_video_pattern();

            let new_init = match (source_scheme.is_encrypted(), target_scheme.is_encrypted()) {
                (true, true) => {
                    let ks = key_set.as_ref().unwrap();
                    init::rewrite_init_segment(&init_data, ks, &key_mapping, target_scheme, target_iv_size, target_pattern, container_format)?
                }
                (false, true) => {
                    let ks = key_set.as_ref().unwrap();
                    init::create_protection_info(&init_data, ks, &key_mapping, target_scheme, target_iv_size, target_pattern, container_format)?
                }
                (true, false) => {
                    init::strip_protection_info(&init_data, container_format)?
                }
                (false, false) => {
                    init::rewrite_ftyp_only(&init_data, container_format)?
                }
            };

            let scheme_str = target_scheme.scheme_type_str();
            let base_url = format!("/repackage/{content_id}/{fmt}_{scheme_str}/");
            let drm_info = if target_scheme.is_encrypted() {
                let ks = key_set.as_ref().unwrap();
                Some(build_manifest_drm_info(ks, &primary_kid, &key_mapping, target_scheme))
            } else {
                None
            };
            let mut progressive =
                ProgressiveOutput::new(content_id.clone(), format, base_url, drm_info, container_format);
            progressive.set_variants(build_variants_from_tracks(&tracks));
            progressive.set_init_segment(new_init);
            outputs.push((target_scheme, progressive));
        }

        // Step 6: Process each media segment
        let source_iv_size = protection_info.as_ref()
            .map(|info| info.tenc.default_per_sample_iv_size)
            .unwrap_or(0);
        let constant_iv = protection_info.as_ref()
            .and_then(|info| info.tenc.default_constant_iv.clone());

        self.update_job_state(
            content_id,
            format,
            JobState::Processing,
            0,
            Some(source.segment_urls.len() as u32),
        )?;

        let mut elapsed_time = 0.0f64;
        for (i, segment_url) in source.segment_urls.iter().enumerate() {
            let seg_data = self.fetch_segment(segment_url)?;
            let duration = source.segment_durations.get(i).copied().unwrap_or(6.0);
            let is_last = i == source.segment_urls.len() - 1 && !source.is_live;

            // Extract SCTE-35 ad breaks from emsg boxes (once per source segment)
            let ad_breaks = extract_ad_breaks_from_segment(&seg_data, i as u32, elapsed_time);
            elapsed_time += duration;

            // Re-encrypt for each target scheme
            for (target_scheme, progressive) in outputs.iter_mut() {
                let target_iv_size = target_scheme.default_iv_size();
                let target_pattern = target_scheme.default_video_pattern();
                let target_key = if target_scheme.is_encrypted() { content_key.clone() } else { None };

                let params = SegmentRewriteParams {
                    source_key: source_key.clone(),
                    target_key,
                    source_scheme,
                    target_scheme: *target_scheme,
                    source_iv_size,
                    target_iv_size,
                    source_pattern,
                    target_pattern,
                    constant_iv: constant_iv.clone(),
                    segment_number: i as u32,
                };

                let new_segment = segment::rewrite_segment(&seg_data, &params)?;
                progressive.add_segment(i as u32, new_segment, duration);

                // Add ad breaks to this scheme's progressive output
                for ab in &ad_breaks {
                    progressive.add_ad_break(ab.clone());
                }

                if is_last {
                    progressive.finalize();
                }
            }

            self.update_job_state(
                content_id,
                format,
                if is_last { JobState::Complete } else { JobState::Processing },
                (i + 1) as u32,
                Some(source.segment_urls.len() as u32),
            )?;
        }

        // Clean up sensitive cache entries
        if needs_keys {
            self.cleanup_sensitive_data(content_id, format, target_schemes);
        }

        let status = JobStatus {
            content_id: content_id.clone(),
            format,
            state: JobState::Complete,
            segments_completed: source.segment_urls.len() as u32,
            segments_total: Some(source.segment_urls.len() as u32),
        };
        Ok((status, outputs))
    }

    /// Execute the pipeline through the first segment, producing a live manifest.
    ///
    /// This is the first half of the split execution model for WASI environments.
    /// After this returns, per-scheme manifest URLs are immediately usable. The caller
    /// should chain `execute_remaining()` via self-invocation for the rest.
    pub fn execute_first(&self, request: &RepackageRequest) -> Result<JobStatus> {
        let content_id = &request.content_id;
        let format = request.output_format;
        let target_schemes = &request.target_schemes;
        let container_format = request.container_format;
        let fmt = format_str(format);
        let ttl = self.config.cache.job_state_ttl;

        // Step 1: Fetch source manifest
        self.update_job_state(content_id, format, JobState::FetchingKeys, 0, None)?;
        let source = self.fetch_source_manifest(&request.source_url)?;
        let total = source.segment_urls.len() as u32;

        // Store source manifest in Redis for continuation
        let source_json = serde_json::to_vec(&source)
            .map_err(|e| EdgepackError::Cache(format!("serialize source manifest: {e}")))?;
        self.cache
            .set(&CacheKeys::source_manifest(content_id, &fmt), &source_json, ttl)?;

        // Store target schemes list for continuation
        let schemes_json = serde_json::to_vec(target_schemes)
            .map_err(|e| EdgepackError::Cache(format!("serialize target schemes: {e}")))?;
        self.cache
            .set(&CacheKeys::target_schemes(content_id, &fmt), &schemes_json, ttl)?;

        // Step 2: Fetch init segment and parse protection info
        let init_data = self.fetch_segment(&source.init_segment_url)?;
        let protection_info = init::parse_protection_info(&init_data)?;

        // Detect source encryption scheme
        let source_scheme = if let Some(ref info) = protection_info {
            EncryptionScheme::from_scheme_type(&info.scheme_type)
                .ok_or_else(|| EdgepackError::Drm(format!(
                    "unsupported encryption scheme: {:?}",
                    std::str::from_utf8(&info.scheme_type)
                )))?
        } else {
            EncryptionScheme::None
        };

        let source_pattern = protection_info.as_ref().map(|info| (
            info.tenc.default_crypt_byte_block,
            info.tenc.default_skip_byte_block,
        )).unwrap_or((0, 0));

        // Step 3: Extract track info and build key mapping
        let tracks = extract_tracks(&init_data).unwrap_or_default();

        // Step 3a: Validate codec/scheme compatibility (pre-flight check)
        let validation = compat::validate_repackage_request(
            source_scheme,
            target_schemes,
            container_format,
            &tracks,
        );
        if !validation.errors.is_empty() {
            return Err(EdgepackError::InvalidInput(format!(
                "validation failed: {}",
                validation.errors.join("; ")
            )));
        }
        for warning in &validation.warnings {
            log::warn!("validation warning: {warning}");
        }

        let key_mapping = build_track_key_mapping(&tracks, &protection_info, content_id);
        let primary_kid = key_mapping.all_kids().into_iter().next()
            .unwrap_or_else(|| derive_kid_from_content_id(content_id));

        // Step 4: Conditional SPEKE
        let any_target_encrypted = target_schemes.iter().any(|s| s.is_encrypted());
        let needs_keys = source_scheme.is_encrypted() || any_target_encrypted;
        let (key_set, source_key, content_key) = if needs_keys {
            let key_ids = key_mapping.all_kids();
            let ks = self.get_or_fetch_keys(content_id, &key_ids)?;
            let key = find_key_for_kid(&ks, &primary_kid)?;
            let src = if source_scheme.is_encrypted() { Some(key.clone()) } else { None };
            (Some(ks), src, Some(key))
        } else {
            (None, None, None)
        };

        // Step 5: Source IV info
        let source_iv_size = protection_info.as_ref()
            .map(|info| info.tenc.default_per_sample_iv_size)
            .unwrap_or(0);
        let constant_iv = protection_info.as_ref()
            .and_then(|info| info.tenc.default_constant_iv.clone());

        // Step 5: Fetch first segment (once, shared across all schemes)
        self.update_job_state(content_id, format, JobState::Processing, 0, Some(total))?;
        let seg_data = self.fetch_segment(&source.segment_urls[0])?;
        let first_duration = source.segment_durations.first().copied().unwrap_or(6.0);
        let is_last = source.segment_urls.len() == 1 && !source.is_live;

        // Extract SCTE-35 ad breaks from first segment's emsg boxes
        let ad_breaks = extract_ad_breaks_from_segment(&seg_data, 0, 0.0);

        // Step 6: Per-scheme — continuation params, init rewrite, first segment, manifest state
        for &target_scheme in target_schemes {
            let target_iv_size = target_scheme.default_iv_size();
            let target_pattern = target_scheme.default_video_pattern();
            let target_key = if target_scheme.is_encrypted() { content_key.clone() } else { None };
            let scheme_str = target_scheme.scheme_type_str();

            // Build and store rewrite parameters for continuation
            let continuation = ContinuationParams {
                source_key: source_key.as_ref().map(|k| CachedKey {
                    kid: k.kid.to_vec(),
                    key: k.key.clone(),
                    iv: k.iv.clone(),
                }),
                target_key: target_key.as_ref().map(|k| CachedKey {
                    kid: k.kid.to_vec(),
                    key: k.key.clone(),
                    iv: k.iv.clone(),
                }),
                source_scheme,
                target_scheme,
                source_iv_size,
                target_iv_size,
                source_pattern,
                target_pattern,
                constant_iv: constant_iv.clone(),
                container_format,
                track_key_mapping: if key_mapping.is_multi_key() {
                    Some(key_mapping.clone())
                } else {
                    None
                },
            };
            let cont_json = serde_json::to_vec(&continuation)
                .map_err(|e| EdgepackError::Cache(format!("serialize rewrite params: {e}")))?;
            self.cache.set(
                &CacheKeys::rewrite_params_for_scheme(content_id, &fmt, scheme_str),
                &cont_json,
                ttl,
            )?;

            // Rewrite init segment
            let new_init = match (source_scheme.is_encrypted(), target_scheme.is_encrypted()) {
                (true, true) => {
                    let ks = key_set.as_ref().unwrap();
                    init::rewrite_init_segment(&init_data, ks, &key_mapping, target_scheme, target_iv_size, target_pattern, container_format)?
                }
                (false, true) => {
                    let ks = key_set.as_ref().unwrap();
                    init::create_protection_info(&init_data, ks, &key_mapping, target_scheme, target_iv_size, target_pattern, container_format)?
                }
                (true, false) => {
                    init::strip_protection_info(&init_data, container_format)?
                }
                (false, false) => {
                    init::rewrite_ftyp_only(&init_data, container_format)?
                }
            };

            // Store init segment
            self.cache.set(
                &CacheKeys::init_segment_for_scheme(content_id, &fmt, scheme_str),
                &new_init,
                ttl,
            )?;

            // Set up progressive output
            let base_url = format!("/repackage/{content_id}/{fmt}_{scheme_str}/");
            let drm_info = if target_scheme.is_encrypted() {
                let ks = key_set.as_ref().unwrap();
                Some(build_manifest_drm_info(ks, &primary_kid, &key_mapping, target_scheme))
            } else {
                None
            };
            let mut progressive = ProgressiveOutput::new(
                content_id.clone(),
                format,
                base_url,
                drm_info,
                container_format,
            );
            progressive.set_variants(build_variants_from_tracks(&tracks));
            progressive.set_init_segment(new_init);

            // Process first segment for this scheme
            let params = SegmentRewriteParams {
                source_key: source_key.clone(),
                target_key,
                source_scheme,
                target_scheme,
                source_iv_size,
                target_iv_size,
                source_pattern,
                target_pattern,
                constant_iv: constant_iv.clone(),
                segment_number: 0,
            };
            let new_segment = segment::rewrite_segment(&seg_data, &params)?;

            // Store segment
            self.cache.set(
                &CacheKeys::media_segment_for_scheme(content_id, &fmt, scheme_str, 0),
                &new_segment,
                ttl,
            )?;

            progressive.add_segment(0, new_segment, first_duration);
            for ab in &ad_breaks {
                progressive.add_ad_break(ab.clone());
            }
            if is_last {
                progressive.finalize();
            }

            // Save manifest state
            let manifest_state = progressive.manifest_state();
            let state_json = serde_json::to_vec(manifest_state)
                .map_err(|e| EdgepackError::Cache(format!("serialize manifest state: {e}")))?;
            self.cache.set(
                &CacheKeys::manifest_state_for_scheme(content_id, &fmt, scheme_str),
                &state_json,
                ttl,
            )?;
        }

        let state = if is_last {
            if needs_keys {
                self.cleanup_sensitive_data(content_id, format, target_schemes);
            }
            JobState::Complete
        } else {
            JobState::Processing
        };
        self.update_job_state(content_id, format, state, 1, Some(total))?;

        Ok(JobStatus {
            content_id: content_id.clone(),
            format,
            state,
            segments_completed: 1,
            segments_total: Some(total),
        })
    }

    /// Execute the next segment in the pipeline, continuing from stored state.
    ///
    /// Loads source manifest, target schemes, per-scheme rewrite params and manifest
    /// state from Redis, processes the next segment for each scheme, and updates state.
    pub fn execute_remaining(&self, content_id: &str, format: OutputFormat) -> Result<JobStatus> {
        let fmt = format_str(format);
        let ttl = self.config.cache.job_state_ttl;

        // Load source manifest from Redis
        let source_data = self
            .cache
            .get(&CacheKeys::source_manifest(content_id, &fmt))?
            .ok_or_else(|| {
                EdgepackError::Cache(format!(
                    "source manifest not found in cache for {content_id}/{fmt}"
                ))
            })?;
        let source: SourceManifest = serde_json::from_slice(&source_data)
            .map_err(|e| EdgepackError::Cache(format!("deserialize source manifest: {e}")))?;

        // Load target schemes list
        let schemes_data = self
            .cache
            .get(&CacheKeys::target_schemes(content_id, &fmt))?
            .ok_or_else(|| {
                EdgepackError::Cache(format!(
                    "target schemes not found in cache for {content_id}/{fmt}"
                ))
            })?;
        let target_schemes: Vec<EncryptionScheme> = serde_json::from_slice(&schemes_data)
            .map_err(|e| EdgepackError::Cache(format!("deserialize target schemes: {e}")))?;

        // Use first scheme's manifest state to determine progress (all schemes are in sync)
        let first_scheme_str = target_schemes[0].scheme_type_str();
        let first_state_data = self
            .cache
            .get(&CacheKeys::manifest_state_for_scheme(content_id, &fmt, first_scheme_str))?
            .ok_or_else(|| {
                EdgepackError::Cache(format!(
                    "manifest state not found in cache for {content_id}/{fmt}_{first_scheme_str}"
                ))
            })?;
        let first_manifest_state: ManifestState = serde_json::from_slice(&first_state_data)
            .map_err(|e| EdgepackError::Cache(format!("deserialize manifest state: {e}")))?;

        let segments_done = first_manifest_state.segments.len();
        let total = source.segment_urls.len();

        if segments_done >= total {
            // Already complete
            return Ok(JobStatus {
                content_id: content_id.to_string(),
                format,
                state: JobState::Complete,
                segments_completed: total as u32,
                segments_total: Some(total as u32),
            });
        }

        // Fetch next source segment (once, shared across all schemes)
        let i = segments_done;
        let seg_data = self.fetch_segment(&source.segment_urls[i])?;
        let duration = source.segment_durations.get(i).copied().unwrap_or(6.0);
        let is_last = i == total - 1 && !source.is_live;

        // Calculate elapsed time from prior segment durations
        let elapsed_time: f64 = source.segment_durations[..i].iter().sum();

        // Extract SCTE-35 ad breaks from emsg boxes
        let ad_breaks = extract_ad_breaks_from_segment(&seg_data, i as u32, elapsed_time);

        // Process for each target scheme
        let mut needs_keys = false;
        for target_scheme in &target_schemes {
            let scheme_str = target_scheme.scheme_type_str();

            // Load continuation params for this scheme
            let params_data = self
                .cache
                .get(&CacheKeys::rewrite_params_for_scheme(content_id, &fmt, scheme_str))?
                .ok_or_else(|| {
                    EdgepackError::Cache(format!(
                        "rewrite params not found in cache for {content_id}/{fmt}_{scheme_str}"
                    ))
                })?;
            let continuation: ContinuationParams = serde_json::from_slice(&params_data)
                .map_err(|e| EdgepackError::Cache(format!("deserialize rewrite params: {e}")))?;

            if continuation.source_scheme.is_encrypted() || continuation.target_scheme.is_encrypted() {
                needs_keys = true;
            }

            let source_key = continuation.source_key.as_ref().map(restore_content_key);
            let target_key = continuation.target_key.as_ref().map(restore_content_key);

            let params = SegmentRewriteParams {
                source_key,
                target_key,
                source_scheme: continuation.source_scheme,
                target_scheme: continuation.target_scheme,
                source_iv_size: continuation.source_iv_size,
                target_iv_size: continuation.target_iv_size,
                source_pattern: continuation.source_pattern,
                target_pattern: continuation.target_pattern,
                constant_iv: continuation.constant_iv.clone(),
                segment_number: i as u32,
            };

            let new_segment = segment::rewrite_segment(&seg_data, &params)?;

            // Store segment
            self.cache.set(
                &CacheKeys::media_segment_for_scheme(content_id, &fmt, scheme_str, i as u32),
                &new_segment,
                ttl,
            )?;

            // Load and update manifest state for this scheme
            let state_data = self
                .cache
                .get(&CacheKeys::manifest_state_for_scheme(content_id, &fmt, scheme_str))?
                .ok_or_else(|| {
                    EdgepackError::Cache(format!(
                        "manifest state not found in cache for {content_id}/{fmt}_{scheme_str}"
                    ))
                })?;
            let mut manifest_state: ManifestState = serde_json::from_slice(&state_data)
                .map_err(|e| EdgepackError::Cache(format!("deserialize manifest state: {e}")))?;

            let ext = continuation.container_format.video_segment_extension();
            let uri = format!("{}segment_{i}{ext}", manifest_state.base_url);

            manifest_state.segments.push(SegmentInfo {
                number: i as u32,
                duration,
                uri,
                byte_size: new_segment.len() as u64,
            });
            if duration > manifest_state.target_duration {
                manifest_state.target_duration = duration;
            }

            // Add ad breaks from this segment
            for ab in &ad_breaks {
                manifest_state.ad_breaks.push(ab.clone());
            }

            if is_last {
                manifest_state.phase = ManifestPhase::Complete;
            }

            // Save updated manifest state
            let state_json = serde_json::to_vec(&manifest_state)
                .map_err(|e| EdgepackError::Cache(format!("serialize manifest state: {e}")))?;
            self.cache.set(
                &CacheKeys::manifest_state_for_scheme(content_id, &fmt, scheme_str),
                &state_json,
                ttl,
            )?;
        }

        let completed = (i + 1) as u32;
        let state = if is_last {
            if needs_keys {
                self.cleanup_sensitive_data(content_id, format, &target_schemes);
            }
            JobState::Complete
        } else {
            JobState::Processing
        };
        self.update_job_state(content_id, format, state, completed, Some(total as u32))?;

        Ok(JobStatus {
            content_id: content_id.to_string(),
            format,
            state,
            segments_completed: completed,
            segments_total: Some(total as u32),
        })
    }

    /// Fetch source manifest and parse segment URLs.
    ///
    /// Fetches the manifest via HTTP, auto-detects HLS vs DASH,
    /// and parses it into a `SourceManifest` with segment URLs.
    fn fetch_source_manifest(&self, url: &str) -> Result<SourceManifest> {
        let response = crate::http_client::get(url, &[])?;

        if response.status >= 400 {
            return Err(EdgepackError::Http {
                status: response.status,
                message: format!("failed to fetch source manifest: HTTP {}", response.status),
            });
        }

        let text = String::from_utf8(response.body).map_err(|e| EdgepackError::Http {
            status: 0,
            message: format!("source manifest is not valid UTF-8: {e}"),
        })?;

        // Auto-detect format: HLS if URL ends in .m3u8 or content starts with #EXTM3U
        if url.contains(".m3u8") || text.starts_with("#EXTM3U") {
            crate::manifest::hls_input::parse_hls_manifest(&text, url)
        } else {
            crate::manifest::dash_input::parse_dash_manifest(&text, url)
        }
    }

    /// Fetch a single segment (init or media) from origin.
    fn fetch_segment(&self, url: &str) -> Result<Vec<u8>> {
        let response = crate::http_client::get(url, &[])?;

        if response.status >= 400 {
            return Err(EdgepackError::Http {
                status: response.status,
                message: format!("failed to fetch segment: HTTP {}", response.status),
            });
        }

        Ok(response.body)
    }

    /// Get cached DRM keys or fetch new ones via SPEKE.
    fn get_or_fetch_keys(
        &self,
        content_id: &str,
        key_ids: &[[u8; 16]],
    ) -> Result<DrmKeySet> {
        let cache_key = CacheKeys::drm_keys(content_id);

        // Check cache first
        if let Some(cached) = self.cache.get(&cache_key)? {
            if let Ok(key_set) = serde_json::from_slice::<CachedKeySet>(&cached) {
                return Ok(key_set.into());
            }
        }

        // Fetch from SPEKE
        let key_set = self.speke.request_keys(content_id, key_ids)?;

        // Cache the keys
        let cacheable = CachedKeySet::from(&key_set);
        if let Ok(json) = serde_json::to_vec(&cacheable) {
            let _ = self.cache.set(&cache_key, &json, self.config.cache.drm_key_ttl);
        }

        Ok(key_set)
    }

    fn update_job_state(
        &self,
        content_id: &str,
        format: OutputFormat,
        state: JobState,
        completed: u32,
        total: Option<u32>,
    ) -> Result<()> {
        let status = JobStatus {
            content_id: content_id.to_string(),
            format,
            state,
            segments_completed: completed,
            segments_total: total,
        };
        let json = serde_json::to_vec(&status)
            .map_err(|e| EdgepackError::Cache(format!("serialize job state: {e}")))?;
        let key = CacheKeys::job_state(content_id, &format_str(format));
        self.cache.set(&key, &json, self.config.cache.job_state_ttl)
    }

    // --- JIT Packaging Methods (Phase 8) ---

    /// JIT setup: fetch source manifest, init segment, and DRM keys, then cache
    /// everything needed for subsequent per-segment JIT requests.
    ///
    /// This is the expensive initial operation triggered on the first GET for content.
    /// After setup, manifests and init segments are immediately available in cache.
    /// Media segments are processed individually on demand via `jit_segment()`.
    #[cfg(feature = "jit")]
    pub fn jit_setup(
        &self,
        content_id: &str,
        source_config: &crate::repackager::SourceConfig,
        output_format: OutputFormat,
        target_scheme: EncryptionScheme,
        base_url: &str,
    ) -> Result<JitSetupResult> {
        let fmt = format_str(output_format);
        let scheme_str = target_scheme.scheme_type_str();
        let ttl = self.config.cache.job_state_ttl;

        // Check if setup is already done (idempotency)
        let setup_key = CacheKeys::jit_setup(content_id, &fmt);
        if self.cache.exists(&setup_key)? {
            // Load existing manifest state and init segment
            let state_data = self.cache.get(&CacheKeys::manifest_state_for_scheme(content_id, &fmt, scheme_str))?;
            let init_data = self.cache.get(&CacheKeys::init_segment_for_scheme(content_id, &fmt, scheme_str))?;
            if let (Some(state_bytes), Some(init_bytes)) = (state_data, init_data) {
                let manifest_state: ManifestState = serde_json::from_slice(&state_bytes)
                    .map_err(|e| EdgepackError::Cache(format!("deserialize manifest state: {e}")))?;
                return Ok(JitSetupResult {
                    manifest_state,
                    init_segment_data: init_bytes,
                });
            }
        }

        // Step 1: Fetch source manifest
        let source = self.fetch_source_manifest(&source_config.source_url)?;
        let _total_segments = source.segment_urls.len() as u32;

        // Cache source manifest for jit_segment() to use later
        let source_json = serde_json::to_vec(&source)
            .map_err(|e| EdgepackError::Cache(format!("serialize source manifest: {e}")))?;
        self.cache.set(&CacheKeys::source_manifest(content_id, &fmt), &source_json, ttl)?;

        // Step 2: Fetch init segment and parse protection info
        let init_data = self.fetch_segment(&source.init_segment_url)?;
        let protection_info = init::parse_protection_info(&init_data)?;

        // Detect source encryption scheme
        let source_scheme = if let Some(ref info) = protection_info {
            EncryptionScheme::from_scheme_type(&info.scheme_type)
                .ok_or_else(|| EdgepackError::Drm(format!(
                    "unsupported encryption scheme: {:?}",
                    std::str::from_utf8(&info.scheme_type)
                )))?
        } else {
            EncryptionScheme::None
        };

        let source_pattern = protection_info.as_ref().map(|info| (
            info.tenc.default_crypt_byte_block,
            info.tenc.default_skip_byte_block,
        )).unwrap_or((0, 0));

        // Step 3: Extract tracks and build key mapping
        let tracks = extract_tracks(&init_data).unwrap_or_default();

        // Step 3a: Validate codec/scheme compatibility (pre-flight check)
        let validation = compat::validate_repackage_request(
            source_scheme,
            &[target_scheme],
            source_config.container_format,
            &tracks,
        );
        if !validation.errors.is_empty() {
            return Err(EdgepackError::InvalidInput(format!(
                "validation failed: {}",
                validation.errors.join("; ")
            )));
        }
        for warning in &validation.warnings {
            log::warn!("validation warning: {warning}");
        }

        let key_mapping = build_track_key_mapping(&tracks, &protection_info, content_id);
        let primary_kid = key_mapping.all_kids().into_iter().next()
            .unwrap_or_else(|| derive_kid_from_content_id(content_id));

        // Step 4: Conditional SPEKE
        let needs_keys = source_scheme.is_encrypted() || target_scheme.is_encrypted();
        let (key_set, source_key, content_key) = if needs_keys {
            let key_ids = key_mapping.all_kids();
            let ks = self.get_or_fetch_keys(content_id, &key_ids)?;
            let key = find_key_for_kid(&ks, &primary_kid)?;
            let src = if source_scheme.is_encrypted() { Some(key.clone()) } else { None };
            (Some(ks), src, Some(key))
        } else {
            (None, None, None)
        };

        // Step 5: Source IV info
        let source_iv_size = protection_info.as_ref()
            .map(|info| info.tenc.default_per_sample_iv_size)
            .unwrap_or(0);
        let constant_iv = protection_info.as_ref()
            .and_then(|info| info.tenc.default_constant_iv.clone());

        // Step 6: Rewrite init segment
        let target_iv_size = target_scheme.default_iv_size();
        let target_pattern = target_scheme.default_video_pattern();
        let container_format = source_config.container_format;
        let target_key = if target_scheme.is_encrypted() { content_key.clone() } else { None };

        let new_init = match (source_scheme.is_encrypted(), target_scheme.is_encrypted()) {
            (true, true) => {
                let ks = key_set.as_ref().unwrap();
                init::rewrite_init_segment(&init_data, ks, &key_mapping, target_scheme, target_iv_size, target_pattern, container_format)?
            }
            (false, true) => {
                let ks = key_set.as_ref().unwrap();
                init::create_protection_info(&init_data, ks, &key_mapping, target_scheme, target_iv_size, target_pattern, container_format)?
            }
            (true, false) => {
                init::strip_protection_info(&init_data, container_format)?
            }
            (false, false) => {
                init::rewrite_ftyp_only(&init_data, container_format)?
            }
        };

        // Step 7: Build and cache rewrite params (for jit_segment)
        let continuation = ContinuationParams {
            source_key: source_key.as_ref().map(|k| CachedKey {
                kid: k.kid.to_vec(),
                key: k.key.clone(),
                iv: k.iv.clone(),
            }),
            target_key: target_key.as_ref().map(|k| CachedKey {
                kid: k.kid.to_vec(),
                key: k.key.clone(),
                iv: k.iv.clone(),
            }),
            source_scheme,
            target_scheme,
            source_iv_size,
            target_iv_size,
            source_pattern,
            target_pattern,
            constant_iv,
            container_format,
            track_key_mapping: if key_mapping.is_multi_key() {
                Some(key_mapping.clone())
            } else {
                None
            },
        };
        let cont_json = serde_json::to_vec(&continuation)
            .map_err(|e| EdgepackError::Cache(format!("serialize rewrite params: {e}")))?;
        self.cache.set(
            &CacheKeys::rewrite_params_for_scheme(content_id, &fmt, scheme_str),
            &cont_json,
            ttl,
        )?;

        // Step 8: Cache init segment
        self.cache.set(
            &CacheKeys::init_segment_for_scheme(content_id, &fmt, scheme_str),
            &new_init,
            ttl,
        )?;

        // Step 9: Build manifest state with all segment entries (but not yet processed)
        let drm_info = if target_scheme.is_encrypted() {
            let ks = key_set.as_ref().unwrap();
            Some(build_manifest_drm_info(ks, &primary_kid, &key_mapping, target_scheme))
        } else {
            None
        };

        let ext = container_format.video_segment_extension();
        let manifest_segments: Vec<SegmentInfo> = source.segment_urls.iter().enumerate().map(|(i, _)| {
            let duration = source.segment_durations.get(i).copied().unwrap_or(6.0);
            SegmentInfo {
                number: i as u32,
                duration,
                uri: format!("{base_url}segment_{i}{ext}"),
                byte_size: 0, // Not yet processed — size will be updated when segment is processed
            }
        }).collect();

        let target_duration = source.segment_durations.iter().copied()
            .fold(0.0_f64, f64::max)
            .max(6.0);

        use crate::manifest::types::InitSegmentInfo;
        let manifest_state = ManifestState {
            content_id: content_id.to_string(),
            format: output_format,
            phase: ManifestPhase::Complete,
            init_segment: Some(InitSegmentInfo {
                uri: format!("{base_url}init.mp4"),
                byte_size: new_init.len() as u64,
            }),
            segments: manifest_segments,
            target_duration,
            variants: build_variants_from_tracks(&tracks),
            drm_info,
            media_sequence: 0,
            base_url: base_url.to_string(),
            container_format,
            cea_captions: Vec::new(),
            ad_breaks: Vec::new(),
        };

        let state_json = serde_json::to_vec(&manifest_state)
            .map_err(|e| EdgepackError::Cache(format!("serialize manifest state: {e}")))?;
        self.cache.set(
            &CacheKeys::manifest_state_for_scheme(content_id, &fmt, scheme_str),
            &state_json,
            ttl,
        )?;

        // Step 10: Set JIT setup marker
        self.cache.set(&setup_key, b"1", ttl)?;

        Ok(JitSetupResult {
            manifest_state,
            init_segment_data: new_init,
        })
    }

    /// JIT segment: fetch, decrypt, re-encrypt, and cache a single media segment on demand.
    ///
    /// Requires `jit_setup()` to have been called first (loads source manifest and
    /// rewrite params from cache). Returns the rewritten segment bytes.
    #[cfg(feature = "jit")]
    pub fn jit_segment(
        &self,
        content_id: &str,
        output_format: OutputFormat,
        target_scheme: EncryptionScheme,
        segment_number: u32,
    ) -> Result<Vec<u8>> {
        let fmt = format_str(output_format);
        let scheme_str = target_scheme.scheme_type_str();

        // Check if segment is already cached
        let seg_cache_key = CacheKeys::media_segment_for_scheme(content_id, &fmt, scheme_str, segment_number);
        if let Some(cached) = self.cache.get(&seg_cache_key)? {
            return Ok(cached);
        }

        // Load source manifest
        let source_data = self.cache.get(&CacheKeys::source_manifest(content_id, &fmt))?
            .ok_or_else(|| EdgepackError::Cache(format!(
                "source manifest not found for {content_id}/{fmt} — call jit_setup first"
            )))?;
        let source: SourceManifest = serde_json::from_slice(&source_data)
            .map_err(|e| EdgepackError::Cache(format!("deserialize source manifest: {e}")))?;

        // Validate segment number
        if segment_number as usize >= source.segment_urls.len() {
            return Err(EdgepackError::InvalidInput(format!(
                "segment {segment_number} out of bounds (total: {})",
                source.segment_urls.len()
            )));
        }

        // Load rewrite params
        let params_data = self.cache.get(&CacheKeys::rewrite_params_for_scheme(content_id, &fmt, scheme_str))?
            .ok_or_else(|| EdgepackError::Cache(format!(
                "rewrite params not found for {content_id}/{fmt}_{scheme_str} — call jit_setup first"
            )))?;
        let continuation: ContinuationParams = serde_json::from_slice(&params_data)
            .map_err(|e| EdgepackError::Cache(format!("deserialize rewrite params: {e}")))?;

        // Fetch source segment
        let seg_data = self.fetch_segment(&source.segment_urls[segment_number as usize])?;

        // Build rewrite params
        let source_key = continuation.source_key.as_ref().map(restore_content_key);
        let target_key = continuation.target_key.as_ref().map(restore_content_key);

        let params = SegmentRewriteParams {
            source_key,
            target_key,
            source_scheme: continuation.source_scheme,
            target_scheme: continuation.target_scheme,
            source_iv_size: continuation.source_iv_size,
            target_iv_size: continuation.target_iv_size,
            source_pattern: continuation.source_pattern,
            target_pattern: continuation.target_pattern,
            constant_iv: continuation.constant_iv,
            segment_number,
        };

        // Rewrite segment
        let new_segment = segment::rewrite_segment(&seg_data, &params)?;

        // Cache the result
        let ttl = self.config.cache.job_state_ttl;
        self.cache.set(&seg_cache_key, &new_segment, ttl)?;

        Ok(new_segment)
    }

    /// Delete all sensitive cache entries for a completed job.
    ///
    /// Removes DRM keys, SPEKE response, per-scheme rewrite params, target schemes
    /// list, and source manifest metadata. Non-sensitive data (job state, manifest
    /// state, init/media segments) is left for CDN serving.
    ///
    /// Cleanup errors are intentionally swallowed — they must not prevent
    /// the pipeline from reporting success to the caller.
    fn cleanup_sensitive_data(&self, content_id: &str, format: OutputFormat, target_schemes: &[EncryptionScheme]) {
        let fmt = format_str(format);
        let _ = self.cache.delete(&CacheKeys::drm_keys(content_id));
        let _ = self.cache.delete(&CacheKeys::speke_response(content_id));
        let _ = self.cache.delete(&CacheKeys::source_manifest(content_id, &fmt));
        let _ = self.cache.delete(&CacheKeys::target_schemes(content_id, &fmt));
        // Delete per-scheme rewrite params
        for scheme in target_schemes {
            let scheme_str = scheme.scheme_type_str();
            let _ = self.cache.delete(&CacheKeys::rewrite_params_for_scheme(content_id, &fmt, scheme_str));
        }
    }
}

/// Build a TrackKeyMapping from extracted tracks and protection info.
///
/// Priority:
/// 1. Per-track KIDs from extracted track info (multi-key encrypted)
/// 2. Single KID from protection info tenc box (single-key encrypted)
/// 3. Derived KID from content_id (clear source)
fn build_track_key_mapping(
    tracks: &[TrackInfo],
    protection_info: &Option<crate::media::cmaf::ProtectionSchemeInfo>,
    content_id: &str,
) -> TrackKeyMapping {
    // Try building from track-level KIDs first (multi-track encrypted)
    let mapping = TrackKeyMapping::from_tracks(tracks);
    if !mapping.all_kids().is_empty() {
        return mapping;
    }

    // Fall back to protection_info KID (single-key encrypted)
    if let Some(info) = protection_info {
        return TrackKeyMapping::single(info.tenc.default_kid);
    }

    // Clear source: derive deterministic KID
    TrackKeyMapping::single(derive_kid_from_content_id(content_id))
}

/// Build VariantInfo from extracted track metadata for manifest population.
///
/// Converts each video/audio track into a VariantInfo with codec string.
/// Tracks with unknown types are skipped. Bandwidth defaults to 0
/// (will be updated from actual segment sizes if needed).
fn build_variants_from_tracks(tracks: &[TrackInfo]) -> Vec<VariantInfo> {
    tracks
        .iter()
        .filter_map(|t| {
            let track_type = match t.track_type {
                crate::media::TrackType::Video => TrackMediaType::Video,
                crate::media::TrackType::Audio => TrackMediaType::Audio,
                crate::media::TrackType::Subtitle => TrackMediaType::Subtitle,
                _ => return None,
            };
            Some(VariantInfo {
                id: t.track_id.to_string(),
                bandwidth: 0,
                codecs: t.codec_string.clone(),
                resolution: None,
                frame_rate: None,
                track_type,
                language: t.language.clone(),
            })
        })
        .collect()
}

fn find_key_for_kid(key_set: &DrmKeySet, kid: &[u8; 16]) -> Result<ContentKey> {
    key_set
        .keys
        .iter()
        .find(|k| &k.kid == kid)
        .cloned()
        .ok_or_else(|| {
            EdgepackError::Drm(format!(
                "no key found for KID {:?}",
                crate::drm::cpix::format_uuid(kid)
            ))
        })
}

fn build_manifest_drm_info(
    key_set: &DrmKeySet,
    kid: &[u8; 16],
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
) -> ManifestDrmInfo {
    let b64 = &base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let kid_hex: String = kid.iter().map(|b| format!("{b:02x}")).collect();

    let widevine_pssh = build_manifest_pssh_for_system(key_set, key_mapping, crate::drm::system_ids::WIDEVINE)
        .map(|pssh_box| b64.encode(&pssh_box));

    let playready_pssh = build_manifest_pssh_for_system(key_set, key_mapping, crate::drm::system_ids::PLAYREADY)
        .map(|pssh_box| b64.encode(&pssh_box));

    let playready_pro = key_set
        .drm_systems
        .iter()
        .find(|d| d.system_id == crate::drm::system_ids::PLAYREADY)
        .and_then(|d| d.content_protection_data.clone());

    // For CBCS output, include FairPlay key URI if available
    let fairplay_key_uri = if target_scheme == EncryptionScheme::Cbcs {
        key_set
            .drm_systems
            .iter()
            .find(|d| d.system_id == crate::drm::system_ids::FAIRPLAY)
            .and_then(|d| d.content_protection_data.clone())
    } else {
        None
    };

    ManifestDrmInfo {
        encryption_scheme: target_scheme,
        widevine_pssh,
        playready_pssh,
        playready_pro,
        fairplay_key_uri,
        default_kid: kid_hex,
    }
}

/// Build a PSSH box for a DRM system, merging all unique KIDs.
///
/// Groups DRM system entries by system_id and collects all unique KIDs
/// from both the entries and the key_mapping. Produces one PSSH v1 box
/// per system with all KIDs, matching the multi-KID PSSH in the init segment.
fn build_manifest_pssh_for_system(
    key_set: &DrmKeySet,
    key_mapping: &TrackKeyMapping,
    system_id: [u8; 16],
) -> Option<Vec<u8>> {
    let system_entries: Vec<_> = key_set
        .drm_systems
        .iter()
        .filter(|d| d.system_id == system_id)
        .collect();

    if system_entries.is_empty() {
        return None;
    }

    // Collect all unique KIDs from DRM system entries
    let mut kid_set: Vec<[u8; 16]> = Vec::new();
    for entry in &system_entries {
        if !kid_set.contains(&entry.kid) {
            kid_set.push(entry.kid);
        }
    }

    // When multi-key, merge KIDs from key_mapping
    if key_mapping.is_multi_key() {
        for k in key_mapping.all_kids() {
            if !kid_set.contains(&k) {
                kid_set.push(k);
            }
        }
    }

    let pssh_box = crate::media::cmaf::build_pssh_box(&crate::media::cmaf::PsshBox {
        version: 1,
        system_id,
        key_ids: kid_set,
        data: system_entries[0].pssh_data.clone(),
    });

    Some(pssh_box)
}

fn format_str(format: OutputFormat) -> String {
    match format {
        OutputFormat::Hls => "hls".to_string(),
        OutputFormat::Dash => "dash".to_string(),
    }
}

/// Derive a deterministic KID from a content_id for clear-to-encrypted transforms.
///
/// Uses the first 16 bytes of the content_id (or zero-pads if shorter).
/// This provides a stable, deterministic KID without requiring SPEKE key IDs upfront.
fn derive_kid_from_content_id(content_id: &str) -> [u8; 16] {
    let bytes = content_id.as_bytes();
    let mut kid = [0u8; 16];
    let len = bytes.len().min(16);
    kid[..len].copy_from_slice(&bytes[..len]);
    kid
}

fn restore_content_key(cached: &CachedKey) -> ContentKey {
    let mut kid = [0u8; 16];
    let len = cached.kid.len().min(16);
    kid[..len].copy_from_slice(&cached.kid[..len]);
    ContentKey {
        kid,
        key: cached.key.clone(),
        iv: cached.iv.clone(),
    }
}

/// Extract SCTE-35 ad breaks from emsg boxes in a source segment.
fn extract_ad_breaks_from_segment(
    segment_data: &[u8],
    segment_number: u32,
    elapsed_time: f64,
) -> Vec<crate::manifest::types::AdBreakInfo> {
    use base64::Engine;
    let emsg_boxes = segment::extract_emsg_boxes(segment_data);
    let mut ad_breaks = Vec::new();

    for emsg in &emsg_boxes {
        if !scte35::is_scte35_emsg(emsg) {
            continue;
        }
        if let Ok(splice) = scte35::parse_splice_info(&emsg.message_data) {
            let presentation_time = if let Some(pts) = splice.pts_time {
                pts as f64 / 90000.0
            } else {
                elapsed_time
            };
            let scte35_cmd =
                Some(base64::engine::general_purpose::STANDARD.encode(&emsg.message_data));
            ad_breaks.push(crate::manifest::types::AdBreakInfo {
                id: splice.splice_event_id,
                presentation_time,
                duration: splice.break_duration,
                scte35_cmd,
                segment_number,
            });
        }
    }

    ad_breaks
}

// ---------------------------------------------------------------------------
// Serializable types for Redis caching
// ---------------------------------------------------------------------------

/// Serializable version of DrmKeySet for Redis caching.
#[derive(serde::Serialize, serde::Deserialize)]
struct CachedKeySet {
    keys: Vec<CachedKey>,
    drm_systems: Vec<CachedDrmSystem>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedKey {
    kid: Vec<u8>,
    key: Vec<u8>,
    iv: Option<Vec<u8>>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CachedDrmSystem {
    system_id: Vec<u8>,
    kid: Vec<u8>,
    pssh_data: Vec<u8>,
    content_protection_data: Option<String>,
}

/// Serializable segment rewrite parameters stored in Redis for continuation chaining.
#[derive(serde::Serialize, serde::Deserialize)]
struct ContinuationParams {
    source_key: Option<CachedKey>,
    target_key: Option<CachedKey>,
    source_scheme: EncryptionScheme,
    target_scheme: EncryptionScheme,
    source_iv_size: u8,
    target_iv_size: u8,
    source_pattern: (u8, u8),
    target_pattern: (u8, u8),
    constant_iv: Option<Vec<u8>>,
    #[serde(default)]
    container_format: ContainerFormat,
    /// Per-track key mapping for multi-key support.
    /// When `None`, falls back to single source_key/target_key (backward compat).
    #[serde(default)]
    track_key_mapping: Option<TrackKeyMapping>,
}

impl From<&DrmKeySet> for CachedKeySet {
    fn from(ks: &DrmKeySet) -> Self {
        Self {
            keys: ks
                .keys
                .iter()
                .map(|k| CachedKey {
                    kid: k.kid.to_vec(),
                    key: k.key.clone(),
                    iv: k.iv.clone(),
                })
                .collect(),
            drm_systems: ks
                .drm_systems
                .iter()
                .map(|d| CachedDrmSystem {
                    system_id: d.system_id.to_vec(),
                    kid: d.kid.to_vec(),
                    pssh_data: d.pssh_data.clone(),
                    content_protection_data: d.content_protection_data.clone(),
                })
                .collect(),
        }
    }
}

impl From<CachedKeySet> for DrmKeySet {
    fn from(ck: CachedKeySet) -> Self {
        Self {
            keys: ck
                .keys
                .into_iter()
                .map(|k| restore_content_key(&k))
                .collect(),
            drm_systems: ck
                .drm_systems
                .into_iter()
                .map(|d| {
                    let mut system_id = [0u8; 16];
                    system_id.copy_from_slice(&d.system_id[..16.min(d.system_id.len())]);
                    let mut kid = [0u8; 16];
                    kid.copy_from_slice(&d.kid[..16.min(d.kid.len())]);
                    crate::drm::DrmSystemData {
                        system_id,
                        kid,
                        pssh_data: d.pssh_data,
                        content_protection_data: d.content_protection_data,
                    }
                })
                .collect(),
        }
    }
}

/// Resolve source configuration for JIT packaging.
///
/// Priority:
/// 1. Redis cache (previously stored via `POST /config/source`)
/// 2. URL pattern from `JitConfig.source_url_pattern` (replaces `{content_id}`)
/// 3. Error if neither is available
///
/// The `scheme_from_url` parameter allows the URL path scheme to override
/// the default target scheme from the source config.
#[cfg(feature = "jit")]
pub fn resolve_source_config(
    cache: &dyn CacheBackend,
    content_id: &str,
    config: &AppConfig,
    scheme_from_url: Option<&str>,
) -> Result<crate::repackager::SourceConfig> {
    use crate::repackager::SourceConfig;

    // 1. Check Redis for per-content config
    let cache_key = CacheKeys::source_config(content_id);
    if let Some(data) = cache.get(&cache_key)? {
        let mut source_config: SourceConfig = serde_json::from_slice(&data).map_err(|e| {
            EdgepackError::Cache(format!("deserialize source config: {e}"))
        })?;

        // Override scheme from URL if provided
        if let Some(scheme_str) = scheme_from_url {
            if let Some(scheme) = parse_scheme_str(scheme_str) {
                source_config.target_schemes = vec![scheme];
            }
        }
        return Ok(source_config);
    }

    // 2. Try URL pattern from config
    if let Some(ref pattern) = config.jit.source_url_pattern {
        let source_url = pattern.replace("{content_id}", content_id);

        let target_schemes = if let Some(scheme_str) = scheme_from_url {
            vec![parse_scheme_str(scheme_str).unwrap_or(config.jit.default_target_scheme)]
        } else {
            vec![config.jit.default_target_scheme]
        };

        return Ok(SourceConfig {
            source_url,
            target_schemes,
            container_format: config.jit.default_container_format,
        });
    }

    // 3. No source configuration available
    Err(EdgepackError::InvalidInput(format!(
        "no source configuration for content_id: {content_id}"
    )))
}

/// Parse a scheme string into an EncryptionScheme.
#[cfg(feature = "jit")]
fn parse_scheme_str(s: &str) -> Option<EncryptionScheme> {
    match s {
        "cenc" => Some(EncryptionScheme::Cenc),
        "cbcs" => Some(EncryptionScheme::Cbcs),
        "none" => Some(EncryptionScheme::None),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheBackend;
    use crate::drm::{system_ids, DrmSystemData};
    use crate::media::container::ContainerFormat;
    use std::sync::{Arc, Mutex};

    /// Mock cache backend that records all `delete()` calls for verification.
    struct SpyCacheBackend {
        inner: std::collections::HashMap<String, Vec<u8>>,
        deleted_keys: Arc<Mutex<Vec<String>>>,
    }

    impl SpyCacheBackend {
        fn new() -> (Self, Arc<Mutex<Vec<String>>>) {
            let deleted = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    inner: std::collections::HashMap::new(),
                    deleted_keys: Arc::clone(&deleted),
                },
                deleted,
            )
        }
    }

    impl CacheBackend for SpyCacheBackend {
        fn get(&self, key: &str) -> crate::error::Result<Option<Vec<u8>>> {
            Ok(self.inner.get(key).cloned())
        }
        fn set(&self, _key: &str, _value: &[u8], _ttl: u64) -> crate::error::Result<()> {
            Ok(())
        }
        fn set_nx(&self, _key: &str, _value: &[u8], _ttl: u64) -> crate::error::Result<bool> {
            Ok(true)
        }
        fn exists(&self, key: &str) -> crate::error::Result<bool> {
            Ok(self.inner.contains_key(key))
        }
        fn delete(&self, key: &str) -> crate::error::Result<()> {
            self.deleted_keys.lock().unwrap().push(key.to_string());
            Ok(())
        }
    }

    fn make_key_set() -> DrmKeySet {
        DrmKeySet {
            keys: vec![ContentKey {
                kid: [0x01; 16],
                key: vec![0xAA; 16],
                iv: Some(vec![0xBB; 8]),
            }],
            drm_systems: vec![
                DrmSystemData {
                    system_id: system_ids::WIDEVINE,
                    kid: [0x01; 16],
                    pssh_data: vec![0x10, 0x20],
                    content_protection_data: None,
                },
                DrmSystemData {
                    system_id: system_ids::PLAYREADY,
                    kid: [0x01; 16],
                    pssh_data: vec![0x30, 0x40],
                    content_protection_data: Some("<pro>test</pro>".into()),
                },
            ],
        }
    }

    #[test]
    fn format_str_hls() {
        assert_eq!(format_str(OutputFormat::Hls), "hls");
    }

    #[test]
    fn format_str_dash() {
        assert_eq!(format_str(OutputFormat::Dash), "dash");
    }

    #[test]
    fn find_key_for_kid_found() {
        let key_set = make_key_set();
        let kid = [0x01; 16];
        let key = find_key_for_kid(&key_set, &kid).unwrap();
        assert_eq!(key.kid, kid);
        assert_eq!(key.key, vec![0xAA; 16]);
    }

    #[test]
    fn find_key_for_kid_not_found() {
        let key_set = make_key_set();
        let kid = [0xFF; 16];
        let result = find_key_for_kid(&key_set, &kid);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no key found"));
    }

    #[test]
    fn cached_key_set_roundtrip() {
        let original = make_key_set();
        let cached = CachedKeySet::from(&original);

        // Serialize and deserialize
        let json = serde_json::to_string(&cached).unwrap();
        let deserialized: CachedKeySet = serde_json::from_str(&json).unwrap();

        // Convert back
        let restored: DrmKeySet = deserialized.into();
        assert_eq!(restored.keys.len(), 1);
        assert_eq!(restored.keys[0].kid, [0x01; 16]);
        assert_eq!(restored.keys[0].key, vec![0xAA; 16]);
        assert_eq!(restored.keys[0].iv, Some(vec![0xBB; 8]));
        assert_eq!(restored.drm_systems.len(), 2);
    }

    #[test]
    fn cached_key_set_preserves_drm_systems() {
        let original = make_key_set();
        let cached = CachedKeySet::from(&original);
        let json = serde_json::to_string(&cached).unwrap();
        let deserialized: CachedKeySet = serde_json::from_str(&json).unwrap();
        let restored: DrmKeySet = deserialized.into();

        assert_eq!(restored.drm_systems[0].system_id, system_ids::WIDEVINE);
        assert_eq!(restored.drm_systems[0].pssh_data, vec![0x10, 0x20]);
        assert!(restored.drm_systems[0].content_protection_data.is_none());

        assert_eq!(restored.drm_systems[1].system_id, system_ids::PLAYREADY);
        assert_eq!(restored.drm_systems[1].pssh_data, vec![0x30, 0x40]);
        assert_eq!(
            restored.drm_systems[1].content_protection_data,
            Some("<pro>test</pro>".into())
        );
    }

    #[test]
    fn cached_key_set_no_iv() {
        let key_set = DrmKeySet {
            keys: vec![ContentKey {
                kid: [0x02; 16],
                key: vec![0xCC; 16],
                iv: None,
            }],
            drm_systems: vec![],
        };
        let cached = CachedKeySet::from(&key_set);
        let json = serde_json::to_string(&cached).unwrap();
        let deserialized: CachedKeySet = serde_json::from_str(&json).unwrap();
        let restored: DrmKeySet = deserialized.into();
        assert!(restored.keys[0].iv.is_none());
    }

    #[test]
    fn build_manifest_drm_info_widevine_and_playready() {
        let key_set = make_key_set();
        let kid = [0x01; 16];
        let mapping = TrackKeyMapping::single(kid);
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc);

        assert!(info.widevine_pssh.is_some());
        assert!(info.playready_pssh.is_some());
        assert!(info.playready_pro.is_some());
        assert_eq!(info.encryption_scheme, EncryptionScheme::Cenc);
        assert!(info.fairplay_key_uri.is_none());
        assert_eq!(info.default_kid.len(), 32);
        assert!(info.default_kid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn build_manifest_drm_info_no_drm_systems() {
        let key_set = DrmKeySet {
            keys: vec![],
            drm_systems: vec![],
        };
        let kid = [0x01; 16];
        let mapping = TrackKeyMapping::single(kid);
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc);

        assert!(info.widevine_pssh.is_none());
        assert!(info.playready_pssh.is_none());
        assert!(info.playready_pro.is_none());
        assert!(info.fairplay_key_uri.is_none());
    }

    #[test]
    fn build_manifest_drm_info_kid_hex_format() {
        let key_set = DrmKeySet {
            keys: vec![],
            drm_systems: vec![],
        };
        let kid = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
                   0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        let mapping = TrackKeyMapping::single(kid);
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc);
        assert_eq!(info.default_kid, "0123456789abcdef0123456789abcdef");
    }

    #[test]
    fn build_manifest_drm_info_cbcs_includes_fairplay() {
        let key_set = DrmKeySet {
            keys: vec![],
            drm_systems: vec![
                DrmSystemData {
                    system_id: system_ids::FAIRPLAY,
                    kid: [0x01; 16],
                    pssh_data: vec![],
                    content_protection_data: Some("skd://fairplay-key-uri".into()),
                },
            ],
        };
        let kid = [0x01; 16];
        let mapping = TrackKeyMapping::single(kid);
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cbcs);

        assert_eq!(info.encryption_scheme, EncryptionScheme::Cbcs);
        assert_eq!(info.fairplay_key_uri, Some("skd://fairplay-key-uri".into()));
    }

    #[test]
    fn build_manifest_drm_info_cenc_excludes_fairplay() {
        let key_set = DrmKeySet {
            keys: vec![],
            drm_systems: vec![
                DrmSystemData {
                    system_id: system_ids::FAIRPLAY,
                    kid: [0x01; 16],
                    pssh_data: vec![],
                    content_protection_data: Some("skd://fairplay-key-uri".into()),
                },
            ],
        };
        let kid = [0x01; 16];
        let mapping = TrackKeyMapping::single(kid);
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc);

        assert_eq!(info.encryption_scheme, EncryptionScheme::Cenc);
        assert!(info.fairplay_key_uri.is_none());
    }

    #[test]
    fn build_manifest_drm_info_multi_kid_pssh() {
        // Multi-key: video KID 0xAA, audio KID 0xBB
        let key_set = DrmKeySet {
            keys: vec![
                ContentKey { kid: [0xAA; 16], key: vec![0x11; 16], iv: None },
                ContentKey { kid: [0xBB; 16], key: vec![0x22; 16], iv: None },
            ],
            drm_systems: vec![
                DrmSystemData {
                    system_id: system_ids::WIDEVINE,
                    kid: [0xAA; 16],
                    pssh_data: vec![0x10],
                    content_protection_data: None,
                },
                DrmSystemData {
                    system_id: system_ids::WIDEVINE,
                    kid: [0xBB; 16],
                    pssh_data: vec![0x10],
                    content_protection_data: None,
                },
            ],
        };
        let video_kid = [0xAA; 16];
        let mapping = TrackKeyMapping::per_type([0xAA; 16], [0xBB; 16]);
        let info = build_manifest_drm_info(&key_set, &video_kid, &mapping, EncryptionScheme::Cenc);

        // Should have Widevine PSSH with both KIDs
        assert!(info.widevine_pssh.is_some());
        let pssh_b64 = info.widevine_pssh.unwrap();
        let pssh_bytes = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &pssh_b64,
        ).unwrap();

        // PSSH v1 box should contain both KIDs (each 16 bytes)
        // The key_ids count field is at a known offset in the PSSH box
        // Just verify the bytes contain both KID patterns
        assert!(pssh_bytes.windows(16).any(|w| w == [0xAA; 16]),
            "PSSH should contain video KID");
        assert!(pssh_bytes.windows(16).any(|w| w == [0xBB; 16]),
            "PSSH should contain audio KID");

        // Default KID should be the primary (video) KID
        assert_eq!(info.default_kid, "aa".repeat(16));
    }

    #[test]
    fn continuation_params_serde_roundtrip() {
        let params = ContinuationParams {
            source_key: Some(CachedKey {
                kid: vec![0x01; 16],
                key: vec![0xAA; 16],
                iv: Some(vec![0xBB; 8]),
            }),
            target_key: Some(CachedKey {
                kid: vec![0x01; 16],
                key: vec![0xAA; 16],
                iv: None,
            }),
            source_scheme: EncryptionScheme::Cbcs,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 8,
            target_iv_size: 8,
            source_pattern: (1, 9),
            target_pattern: (0, 0),
            constant_iv: Some(vec![0xCC; 16]),
            container_format: ContainerFormat::Fmp4,
            track_key_mapping: None,
        };

        let json = serde_json::to_string(&params).unwrap();
        let parsed: ContinuationParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_key.as_ref().unwrap().kid.len(), 16);
        assert_eq!(parsed.source_scheme, EncryptionScheme::Cbcs);
        assert_eq!(parsed.target_scheme, EncryptionScheme::Cenc);
        assert_eq!(parsed.source_iv_size, 8);
        assert_eq!(parsed.source_pattern, (1, 9));
        assert_eq!(parsed.target_pattern, (0, 0));
        assert!(parsed.constant_iv.is_some());
        assert_eq!(parsed.container_format, ContainerFormat::Fmp4);
        assert!(parsed.track_key_mapping.is_none());
    }

    #[test]
    fn continuation_params_default_container_format() {
        // When container_format is missing from JSON, should default to Cmaf
        let json = r#"{
            "source_key":{"kid":[1],"key":[2],"iv":null},
            "target_key":{"kid":[1],"key":[2],"iv":null},
            "source_scheme":"Cbcs","target_scheme":"Cenc",
            "source_iv_size":8,"target_iv_size":8,
            "source_pattern":[1,9],"target_pattern":[0,0],
            "constant_iv":null
        }"#;
        let parsed: ContinuationParams = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.container_format, ContainerFormat::Cmaf);
        // track_key_mapping should also default to None
        assert!(parsed.track_key_mapping.is_none());
    }

    #[test]
    fn continuation_params_with_track_key_mapping() {
        let mapping = TrackKeyMapping::per_type([0xAA; 16], [0xBB; 16]);
        let params = ContinuationParams {
            source_key: None,
            target_key: None,
            source_scheme: EncryptionScheme::Cenc,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 8,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            container_format: ContainerFormat::Cmaf,
            track_key_mapping: Some(mapping),
        };

        let json = serde_json::to_string(&params).unwrap();
        let parsed: ContinuationParams = serde_json::from_str(&json).unwrap();
        let mapping = parsed.track_key_mapping.unwrap();
        assert!(mapping.is_multi_key());
        let kids = mapping.all_kids();
        assert_eq!(kids.len(), 2);
        assert!(kids.contains(&[0xAA; 16]));
        assert!(kids.contains(&[0xBB; 16]));
    }

    // --- build_track_key_mapping tests ---

    #[test]
    fn build_track_key_mapping_from_encrypted_tracks() {
        use crate::media::codec::TrackInfo;
        use crate::media::TrackType;

        let tracks = vec![
            TrackInfo {
                track_type: TrackType::Video,
                track_id: 1,
                codec_string: "avc1.64001f".to_string(),
                timescale: 90000,
                kid: Some([0xAA; 16]),
                language: None,
            },
            TrackInfo {
                track_type: TrackType::Audio,
                track_id: 2,
                codec_string: "mp4a.40.2".to_string(),
                timescale: 44100,
                kid: Some([0xBB; 16]),
                language: None,
            },
        ];

        let mapping = build_track_key_mapping(&tracks, &None, "content-1");
        assert!(mapping.is_multi_key());
        let kids = mapping.all_kids();
        assert_eq!(kids.len(), 2);
        assert!(kids.contains(&[0xAA; 16]));
        assert!(kids.contains(&[0xBB; 16]));
    }

    #[test]
    fn build_track_key_mapping_from_protection_info() {
        use crate::media::cmaf::ProtectionSchemeInfo;
        use crate::media::cmaf::TrackEncryptionBox;

        let info = ProtectionSchemeInfo {
            original_format: *b"avc1",
            scheme_type: *b"cenc",
            scheme_version: 0x00010000,
            tenc: TrackEncryptionBox {
                is_protected: 1,
                default_per_sample_iv_size: 8,
                default_kid: [0xCC; 16],
                default_constant_iv: None,
                default_crypt_byte_block: 0,
                default_skip_byte_block: 0,
            },
        };

        let mapping = build_track_key_mapping(&[], &Some(info), "content-1");
        assert!(!mapping.is_multi_key());
        assert_eq!(mapping.all_kids(), vec![[0xCC; 16]]);
    }

    #[test]
    fn build_track_key_mapping_clear_source_derives_kid() {
        let mapping = build_track_key_mapping(&[], &None, "test-content-id");
        assert!(!mapping.is_multi_key());
        let kids = mapping.all_kids();
        assert_eq!(kids.len(), 1);
        // Derived KID should be first 16 bytes of content_id
        let expected = derive_kid_from_content_id("test-content-id");
        assert_eq!(kids[0], expected);
    }

    #[test]
    fn build_track_key_mapping_prefers_track_kids_over_protection_info() {
        use crate::media::cmaf::ProtectionSchemeInfo;
        use crate::media::cmaf::TrackEncryptionBox;
        use crate::media::codec::TrackInfo;
        use crate::media::TrackType;

        let tracks = vec![TrackInfo {
            track_type: TrackType::Video,
            track_id: 1,
            codec_string: "avc1.64001f".to_string(),
            timescale: 90000,
            kid: Some([0xAA; 16]),
            language: None,
        }];

        let info = ProtectionSchemeInfo {
            original_format: *b"avc1",
            scheme_type: *b"cenc",
            scheme_version: 0x00010000,
            tenc: TrackEncryptionBox {
                is_protected: 1,
                default_per_sample_iv_size: 8,
                default_kid: [0xBB; 16], // Different KID
                default_constant_iv: None,
                default_crypt_byte_block: 0,
                default_skip_byte_block: 0,
            },
        };

        let mapping = build_track_key_mapping(&tracks, &Some(info), "content-1");
        // Should use track KID, not protection_info KID
        assert_eq!(mapping.all_kids(), vec![[0xAA; 16]]);
    }

    // --- build_variants_from_tracks tests ---

    #[test]
    fn build_variants_from_tracks_video() {
        use crate::media::codec::TrackInfo;
        use crate::media::TrackType;

        let tracks = vec![TrackInfo {
            track_type: TrackType::Video,
            track_id: 1,
            codec_string: "avc1.64001f".to_string(),
            timescale: 90000,
            kid: None,
            language: None,
        }];

        let variants = build_variants_from_tracks(&tracks);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].id, "1");
        assert_eq!(variants[0].codecs, "avc1.64001f");
        assert_eq!(variants[0].track_type, TrackMediaType::Video);
        assert_eq!(variants[0].bandwidth, 0);
    }

    #[test]
    fn build_variants_from_tracks_audio() {
        use crate::media::codec::TrackInfo;
        use crate::media::TrackType;

        let tracks = vec![TrackInfo {
            track_type: TrackType::Audio,
            track_id: 2,
            codec_string: "mp4a.40.2".to_string(),
            timescale: 44100,
            kid: None,
            language: None,
        }];

        let variants = build_variants_from_tracks(&tracks);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].codecs, "mp4a.40.2");
        assert_eq!(variants[0].track_type, TrackMediaType::Audio);
    }

    #[test]
    fn build_variants_from_tracks_multi_track() {
        use crate::media::codec::TrackInfo;
        use crate::media::TrackType;

        let tracks = vec![
            TrackInfo {
                track_type: TrackType::Video,
                track_id: 1,
                codec_string: "avc1.64001f".to_string(),
                timescale: 90000,
                kid: None,
                language: None,
            },
            TrackInfo {
                track_type: TrackType::Audio,
                track_id: 2,
                codec_string: "mp4a.40.2".to_string(),
                timescale: 44100,
                kid: None,
                language: None,
            },
        ];

        let variants = build_variants_from_tracks(&tracks);
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[0].track_type, TrackMediaType::Video);
        assert_eq!(variants[1].track_type, TrackMediaType::Audio);
    }

    #[test]
    fn build_variants_from_tracks_subtitle() {
        use crate::media::codec::TrackInfo;
        use crate::media::TrackType;

        let tracks = vec![TrackInfo {
            track_type: TrackType::Subtitle,
            track_id: 3,
            codec_string: "wvtt".to_string(),
            timescale: 1000,
            kid: None,
            language: Some("eng".to_string()),
        }];

        let variants = build_variants_from_tracks(&tracks);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].track_type, TrackMediaType::Subtitle);
        assert_eq!(variants[0].codecs, "wvtt");
        assert_eq!(variants[0].language.as_deref(), Some("eng"));
    }

    #[test]
    fn build_variants_from_tracks_skips_unknown() {
        use crate::media::codec::TrackInfo;
        use crate::media::TrackType;

        let tracks = vec![
            TrackInfo {
                track_type: TrackType::Unknown,
                track_id: 3,
                codec_string: "???".to_string(),
                timescale: 0,
                kid: None,
                language: None,
            },
        ];

        let variants = build_variants_from_tracks(&tracks);
        assert!(variants.is_empty());
    }

    #[test]
    fn build_variants_from_tracks_empty() {
        let variants = build_variants_from_tracks(&[]);
        assert!(variants.is_empty());
    }

    #[test]
    fn restore_content_key_roundtrip() {
        let original = ContentKey {
            kid: [0x42; 16],
            key: vec![0xDE; 16],
            iv: Some(vec![0x99; 8]),
        };
        let cached = CachedKey {
            kid: original.kid.to_vec(),
            key: original.key.clone(),
            iv: original.iv.clone(),
        };
        let restored = restore_content_key(&cached);
        assert_eq!(restored.kid, original.kid);
        assert_eq!(restored.key, original.key);
        assert_eq!(restored.iv, original.iv);
    }

    #[test]
    fn restore_content_key_no_iv() {
        let cached = CachedKey {
            kid: vec![0x11; 16],
            key: vec![0x22; 16],
            iv: None,
        };
        let restored = restore_content_key(&cached);
        assert!(restored.iv.is_none());
    }

    // --- cleanup_sensitive_data tests ---

    fn make_test_config() -> AppConfig {
        use crate::config::*;
        AppConfig {
            store: StoreConfig {
                url: "unused://localhost".into(),
                token: "test-token".into(),
                backend: CacheBackendType::RedisHttp,
            },
            drm: DrmConfig {
                speke_url: crate::url::Url::parse("https://speke.test/v2").unwrap(),
                speke_auth: SpekeAuth::Bearer("test".into()),
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
            jit: JitConfig::default(),
            #[cfg(feature = "cloudflare")]
            cloudflare_kv: None,
            http_kv: None,
        }
    }

    #[test]
    fn cleanup_deletes_all_sensitive_keys_hls() {
        let (cache, deleted) = SpyCacheBackend::new();
        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(cache));

        pipeline.cleanup_sensitive_data("my-content", OutputFormat::Hls, &[EncryptionScheme::Cenc]);

        let keys = deleted.lock().unwrap();
        assert_eq!(keys.len(), 5);
        assert!(keys.contains(&"ep:my-content:keys".to_string()));
        assert!(keys.contains(&"ep:my-content:speke".to_string()));
        assert!(keys.contains(&"ep:my-content:hls:source".to_string()));
        assert!(keys.contains(&"ep:my-content:hls:target_schemes".to_string()));
        assert!(keys.contains(&"ep:my-content:hls_cenc:rewrite_params".to_string()));
    }

    #[test]
    fn cleanup_deletes_all_sensitive_keys_dash() {
        let (cache, deleted) = SpyCacheBackend::new();
        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(cache));

        pipeline.cleanup_sensitive_data("content-42", OutputFormat::Dash, &[EncryptionScheme::Cbcs]);

        let keys = deleted.lock().unwrap();
        assert_eq!(keys.len(), 5);
        assert!(keys.contains(&"ep:content-42:keys".to_string()));
        assert!(keys.contains(&"ep:content-42:speke".to_string()));
        assert!(keys.contains(&"ep:content-42:dash:source".to_string()));
        assert!(keys.contains(&"ep:content-42:dash:target_schemes".to_string()));
        assert!(keys.contains(&"ep:content-42:dash_cbcs:rewrite_params".to_string()));
    }

    #[test]
    fn cleanup_deletes_per_scheme_rewrite_params_dual() {
        let (cache, deleted) = SpyCacheBackend::new();
        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(cache));

        pipeline.cleanup_sensitive_data(
            "dual-content",
            OutputFormat::Hls,
            &[EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
        );

        let keys = deleted.lock().unwrap();
        assert_eq!(keys.len(), 6);
        assert!(keys.contains(&"ep:dual-content:hls_cenc:rewrite_params".to_string()));
        assert!(keys.contains(&"ep:dual-content:hls_cbcs:rewrite_params".to_string()));
    }

    #[test]
    fn cleanup_does_not_delete_non_sensitive_keys() {
        let (cache, deleted) = SpyCacheBackend::new();
        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(cache));

        pipeline.cleanup_sensitive_data("abc", OutputFormat::Hls, &[EncryptionScheme::Cenc]);

        let keys = deleted.lock().unwrap();
        // Should NOT contain state, manifest_state, init, or segment keys
        for key in keys.iter() {
            assert!(
                !key.ends_with(":state"),
                "should not delete job state: {key}"
            );
            assert!(
                !key.contains(":manifest_state"),
                "should not delete manifest state: {key}"
            );
            assert!(
                !key.ends_with(":init"),
                "should not delete init segment: {key}"
            );
            assert!(!key.contains(":seg:"), "should not delete segments: {key}");
        }
    }

    #[test]
    fn cleanup_swallows_delete_errors() {
        use crate::error::EdgepackError;

        /// Cache backend where delete() always fails.
        struct FailingDeleteCache;
        impl CacheBackend for FailingDeleteCache {
            fn get(&self, _: &str) -> crate::error::Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn set(&self, _: &str, _: &[u8], _: u64) -> crate::error::Result<()> {
                Ok(())
            }
            fn set_nx(&self, _: &str, _: &[u8], _: u64) -> crate::error::Result<bool> {
                Ok(true)
            }
            fn exists(&self, _: &str) -> crate::error::Result<bool> {
                Ok(false)
            }
            fn delete(&self, _: &str) -> crate::error::Result<()> {
                Err(EdgepackError::Cache("connection refused".into()))
            }
        }

        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(FailingDeleteCache));

        // Should not panic — errors are swallowed with `let _ =`
        pipeline.cleanup_sensitive_data("test", OutputFormat::Hls, &[EncryptionScheme::Cenc]);
    }

    // --- resolve_source_config tests ---

    /// In-memory cache for resolve_source_config tests.
    #[cfg(feature = "jit")]
    struct MemCache(std::sync::RwLock<std::collections::HashMap<String, Vec<u8>>>);
    #[cfg(feature = "jit")]
    impl MemCache {
        fn new() -> Self {
            Self(std::sync::RwLock::new(std::collections::HashMap::new()))
        }
    }
    #[cfg(feature = "jit")]
    impl CacheBackend for MemCache {
        fn get(&self, key: &str) -> crate::error::Result<Option<Vec<u8>>> {
            Ok(self.0.read().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &[u8], _ttl: u64) -> crate::error::Result<()> {
            self.0.write().unwrap().insert(key.to_string(), value.to_vec());
            Ok(())
        }
        fn set_nx(&self, key: &str, value: &[u8], _ttl: u64) -> crate::error::Result<bool> {
            let mut store = self.0.write().unwrap();
            if store.contains_key(key) {
                Ok(false)
            } else {
                store.insert(key.to_string(), value.to_vec());
                Ok(true)
            }
        }
        fn exists(&self, key: &str) -> crate::error::Result<bool> {
            Ok(self.0.read().unwrap().contains_key(key))
        }
        fn delete(&self, key: &str) -> crate::error::Result<()> {
            self.0.write().unwrap().remove(key);
            Ok(())
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn resolve_source_config_from_redis() {
        let cache = MemCache::new();
        let config = make_test_config();

        // Store a SourceConfig in Redis
        let source = crate::repackager::SourceConfig {
            source_url: "https://origin.example.com/manifest.m3u8".into(),
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::Cmaf,
        };
        let data = serde_json::to_vec(&source).unwrap();
        cache.set(&CacheKeys::source_config("test-id"), &data, 3600).unwrap();

        let resolved = resolve_source_config(&cache, "test-id", &config, None).unwrap();
        assert_eq!(resolved.source_url, "https://origin.example.com/manifest.m3u8");
        assert_eq!(resolved.target_schemes, vec![EncryptionScheme::Cenc]);
    }

    #[cfg(feature = "jit")]
    #[test]
    fn resolve_source_config_from_pattern() {
        let cache = MemCache::new();
        let mut config = make_test_config();
        config.jit.source_url_pattern = Some("https://origin.example.com/{content_id}/master.m3u8".into());

        let resolved = resolve_source_config(&cache, "movie-123", &config, None).unwrap();
        assert_eq!(resolved.source_url, "https://origin.example.com/movie-123/master.m3u8");
    }

    #[cfg(feature = "jit")]
    #[test]
    fn resolve_source_config_missing() {
        let cache = MemCache::new();
        let config = make_test_config();

        let result = resolve_source_config(&cache, "nonexistent", &config, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no source configuration"));
    }

    #[cfg(feature = "jit")]
    #[test]
    fn resolve_source_config_scheme_override() {
        let cache = MemCache::new();
        let config = make_test_config();

        // Store a SourceConfig with cenc
        let source = crate::repackager::SourceConfig {
            source_url: "https://origin.example.com/manifest.m3u8".into(),
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::Cmaf,
        };
        let data = serde_json::to_vec(&source).unwrap();
        cache.set(&CacheKeys::source_config("test-id"), &data, 3600).unwrap();

        // Override with cbcs from URL
        let resolved = resolve_source_config(&cache, "test-id", &config, Some("cbcs")).unwrap();
        assert_eq!(resolved.target_schemes, vec![EncryptionScheme::Cbcs]);
    }

    #[cfg(feature = "jit")]
    #[test]
    fn resolve_source_config_pattern_with_scheme() {
        let cache = MemCache::new();
        let mut config = make_test_config();
        config.jit.source_url_pattern = Some("https://cdn.test/{content_id}/index.m3u8".into());
        config.jit.default_target_scheme = EncryptionScheme::Cenc;

        let resolved = resolve_source_config(&cache, "vid-1", &config, Some("cbcs")).unwrap();
        assert_eq!(resolved.source_url, "https://cdn.test/vid-1/index.m3u8");
        assert_eq!(resolved.target_schemes, vec![EncryptionScheme::Cbcs]);
    }
}
