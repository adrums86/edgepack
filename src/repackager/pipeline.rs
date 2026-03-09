use crate::cache::{CacheBackend, CacheKeys};
use crate::config::AppConfig;
use crate::drm::scheme::EncryptionScheme;
use crate::drm::speke::SpekeClient;
use crate::drm::{ContentKey, DrmKeySet};
use crate::error::{EdgepackError, Result};
use crate::manifest::types::{
    IFrameSegmentInfo, ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat, SegmentInfo,
    SourceManifest, TrackMediaType, VariantInfo,
};
use crate::media::codec::{extract_tracks, TrackInfo, TrackKeyMapping};
use crate::media::compat;
use crate::media::container::ContainerFormat;
use crate::media::init;
use crate::media::scte35;
use crate::media::segment::{self, SegmentRewriteParams};
use crate::repackager::progressive::ProgressiveOutput;
use crate::repackager::RepackageRequest;

/// Result of a JIT setup operation.
///
/// Contains the manifest state (ready to render) and rewritten init segment.
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
    speke: SpekeClient,
}

impl RepackagePipeline {
    pub fn new(config: AppConfig) -> Self {
        let speke = SpekeClient::new(&config.drm);
        Self {
            config,
            speke,
        }
    }

    /// Execute the full repackaging pipeline (processes all segments in one invocation).
    ///
    /// Produces one `ProgressiveOutput` per (format, scheme) combination.
    pub fn execute(&self, request: &RepackageRequest) -> Result<Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)>> {
        let content_id = &request.content_id;
        let target_schemes = &request.target_schemes;
        let container_format = request.container_format;

        // Step 1: Fetch the source manifest to discover segments and encryption info
        let source = self.fetch_source_manifest(&request.source_url)?;

        // TS source: reject if feature not enabled, set up pre-processing state
        #[cfg(not(feature = "ts"))]
        if source.is_ts_source {
            return Err(EdgepackError::InvalidInput(
                "TS input requires the 'ts' feature to be enabled".to_string(),
            ));
        }
        #[cfg(feature = "ts")]
        let mut ts_state: Option<(
            Option<crate::media::transmux::VideoConfig>,
            Option<crate::media::transmux::AudioConfig>,
        )> = None;

        // Step 2: Fetch the init segment and parse protection info
        // For TS sources, we synthesize the init from the first segment instead
        let init_data = if source.is_ts_source {
            #[cfg(feature = "ts")]
            {
                // Fetch the first TS segment to extract codec config
                let first_ts = self.fetch_segment(&source.segment_urls[0])?;
                let (_, init) = process_ts_segment(&first_ts, &source, &mut ts_state, 0)?;
                init.ok_or_else(|| {
                    EdgepackError::MediaParse("failed to synthesize init from TS segment".to_string())
                })?
            }
            #[cfg(not(feature = "ts"))]
            {
                return Err(EdgepackError::InvalidInput(
                    "TS input requires the 'ts' feature".to_string(),
                ));
            }
        } else {
            self.fetch_segment_with_range(&source.init_segment_url, source.init_byte_range)?
        };
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
            let ks = if !request.raw_keys.is_empty() {
                build_key_set_from_raw_keys(&request.raw_keys)
            } else {
                let key_ids = key_mapping.all_kids();
                self.get_or_fetch_keys(content_id, &key_ids)?
            };
            let key = find_key_for_kid(&ks, &primary_kid)?;
            let src = if source_scheme.is_encrypted() { Some(key.clone()) } else { None };
            (Some(ks), src, Some(key))
        } else {
            (None, None, None)
        };

        // Step 5: Per-scheme init rewriting, then per-(format, scheme) progressive output setup

        // TS output: extract mux config, skip init rewriting
        #[cfg(feature = "ts")]
        let ts_mux_config = if !container_format.is_isobmff() {
            Some(crate::media::ts_mux::extract_mux_config(&init_data)?)
        } else {
            None
        };
        #[cfg(feature = "ts")]
        let skip_init = !container_format.is_isobmff();
        #[cfg(not(feature = "ts"))]
        let skip_init = false;

        // Rewrite init segments once per scheme (format-agnostic)
        let mut scheme_inits: Vec<(EncryptionScheme, Vec<u8>)> = Vec::with_capacity(target_schemes.len());
        if skip_init {
            // TS output has no init segments (PAT/PMT are embedded in each segment)
            for &target_scheme in target_schemes {
                scheme_inits.push((target_scheme, Vec::new()));
            }
        } else {
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
                scheme_inits.push((target_scheme, new_init));
            }
        }

        // Build one ProgressiveOutput per (format, scheme) pair
        let mut outputs: Vec<(OutputFormat, EncryptionScheme, ProgressiveOutput)> =
            Vec::with_capacity(request.output_formats.len() * target_schemes.len());
        for &out_format in &request.output_formats {
            let out_fmt = format_str(out_format);
            for &(target_scheme, ref new_init) in &scheme_inits {
                let scheme_str = target_scheme.scheme_type_str();
                let base_url = format!("/repackage/{content_id}/{out_fmt}_{scheme_str}/");
                let drm_info = if target_scheme.is_encrypted() {
                    let ks = key_set.as_ref().unwrap();
                    Some(build_manifest_drm_info(ks, &primary_kid, &key_mapping, target_scheme, &request.drm_systems))
                } else {
                    None
                };
                let mut progressive =
                    ProgressiveOutput::new(content_id.clone(), out_format, base_url, drm_info, container_format);
                progressive.set_variants(build_variants_from_tracks(&tracks));
                if !skip_init {
                    progressive.set_init_segment(new_init.clone());
                }

                // Thread LL-HLS/LL-DASH parameters from source manifest
                if let Some(ptd) = source.part_target_duration {
                    progressive.set_part_target_duration(ptd);
                }
                if let Some(ref sc) = source.server_control {
                    progressive.set_server_control(sc.clone());
                }
                if let Some(ref ll) = source.ll_dash_info {
                    progressive.set_ll_dash_info(ll.clone());
                }

                // Thread I-frame playlist flag
                if request.enable_iframe_playlist {
                    progressive.set_enable_iframe_playlist(true);
                }

                // Thread DVR window duration
                if let Some(dvr_window) = request.dvr_window_duration {
                    progressive.set_dvr_window_duration(dvr_window);
                }

                // Thread content steering (request override > source)
                let effective_steering = request.content_steering.clone()
                    .or_else(|| source.content_steering.clone());
                if let Some(cs) = effective_steering {
                    progressive.set_content_steering(cs);
                }

                // Thread cache control overrides
                if let Some(ref cc) = request.cache_control {
                    progressive.set_cache_control(cc.clone());
                }

                outputs.push((out_format, target_scheme, progressive));
            }
        }

        // Step 6: Process each media segment
        let source_iv_size = protection_info.as_ref()
            .map(|info| info.tenc.default_per_sample_iv_size)
            .unwrap_or(0);
        let constant_iv = protection_info.as_ref()
            .and_then(|info| info.tenc.default_constant_iv.clone());

        let mut elapsed_time = 0.0f64;
        for (i, segment_url) in source.segment_urls.iter().enumerate() {
            // For TS sources, transmux to CMAF before further processing
            let seg_data = if source.is_ts_source {
                #[cfg(feature = "ts")]
                {
                    if i == 0 {
                        // First segment was already processed for init synthesis.
                        // Re-fetch and re-transmux (the cost is acceptable for correctness).
                        let raw_ts = self.fetch_segment(segment_url)?;
                        let (cmaf_seg, _) = process_ts_segment(&raw_ts, &source, &mut ts_state, i as u32)?;
                        cmaf_seg
                    } else {
                        let raw_ts = self.fetch_segment(segment_url)?;
                        let (cmaf_seg, _) = process_ts_segment(&raw_ts, &source, &mut ts_state, i as u32)?;
                        cmaf_seg
                    }
                }
                #[cfg(not(feature = "ts"))]
                {
                    return Err(EdgepackError::InvalidInput(
                        "TS input requires the 'ts' feature".to_string(),
                    ));
                }
            } else {
                let byte_range = source.segment_byte_ranges.get(i).copied();
                self.fetch_segment_with_range(segment_url, byte_range)?
            };
            let duration = source.segment_durations.get(i).copied().unwrap_or(6.0);
            let is_last = i == source.segment_urls.len() - 1 && !source.is_live;

            // Extract SCTE-35 ad breaks from emsg boxes (once per source segment)
            let ad_breaks = extract_ad_breaks_from_segment(&seg_data, i as u32, elapsed_time);
            elapsed_time += duration;

            // Re-encrypt once per scheme (format-agnostic), then distribute to all outputs
            let mut rewritten_segments: Vec<(EncryptionScheme, Vec<u8>, Vec<crate::media::chunk::ChunkBoundary>)> = Vec::new();
            for &target_scheme in target_schemes {
                #[cfg(feature = "ts")]
                if !container_format.is_isobmff() {
                    // TS output path: decrypt to clear CMAF, then mux to TS
                    let ts_config = ts_mux_config.as_ref().unwrap();

                    let clear_cmaf = if source_scheme.is_encrypted() {
                        let clear_params = SegmentRewriteParams {
                            source_key: source_key.clone(),
                            target_key: None,
                            source_scheme,
                            target_scheme: EncryptionScheme::None,
                            source_iv_size,
                            target_iv_size: 0,
                            source_pattern,
                            target_pattern: (0, 0),
                            constant_iv: constant_iv.clone(),
                            segment_number: i as u32,
                        };
                        segment::rewrite_segment(&seg_data, &clear_params)?
                    } else {
                        seg_data.clone()
                    };

                    let ts_bytes = crate::media::ts_mux::mux_to_ts(&clear_cmaf, ts_config, i as u32)?;

                    let output = if target_scheme.is_encrypted() {
                        let key = content_key.as_ref().unwrap();
                        let key_bytes: &[u8; 16] = key.key.as_slice().try_into()
                            .map_err(|_| EdgepackError::Encryption("content key must be 16 bytes for TS AES-128".into()))?;
                        let iv = generate_ts_iv(i as u32);
                        crate::media::ts_mux::encrypt_ts_segment(&ts_bytes, key_bytes, &iv)?
                    } else {
                        ts_bytes
                    };

                    rewritten_segments.push((target_scheme, output, Vec::new()));
                    continue;
                }

                // CMAF/fMP4 output path
                let target_iv_size = target_scheme.default_iv_size();
                let target_pattern = target_scheme.default_video_pattern();
                let target_key = if target_scheme.is_encrypted() { content_key.clone() } else { None };

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
                    segment_number: i as u32,
                };

                let new_segment = segment::rewrite_segment(&seg_data, &params)?;

                // Detect chunk boundaries (needed for LL-HLS parts and/or I-frame playlists)
                let need_chunks = !source.parts.is_empty() || request.enable_iframe_playlist;
                let boundaries = if need_chunks {
                    crate::media::chunk::detect_chunk_boundaries(&new_segment)
                } else {
                    Vec::new()
                };

                rewritten_segments.push((target_scheme, new_segment, boundaries));
            }

            // Distribute rewritten segments to all (format, scheme) outputs
            for (_, ref target_scheme, ref mut progressive) in outputs.iter_mut() {
                let (_, ref new_segment, ref boundaries) = rewritten_segments.iter()
                    .find(|(s, _, _)| s == target_scheme)
                    .expect("scheme must exist in rewritten_segments");

                // LL-HLS: extract parts from multi-chunk segments
                if !source.parts.is_empty() && boundaries.len() > 1 {
                    let part_duration = duration / boundaries.len() as f64;
                    for (pi, boundary) in boundaries.iter().enumerate() {
                        if let Some(chunk_data) = crate::media::chunk::extract_chunk(new_segment, boundary) {
                            progressive.add_part(
                                i as u32,
                                pi as u32,
                                chunk_data,
                                part_duration,
                                boundary.independent,
                            );
                        }
                    }
                }

                // I-frame playlist: record byte range of first IDR chunk
                if request.enable_iframe_playlist {
                    if let Some(idr) = boundaries.iter().find(|b| b.independent) {
                        let ext = request.container_format.video_segment_extension();
                        let seg_uri = format!("{}segment_{}{ext}", progressive.manifest_state().base_url, i);
                        progressive.add_iframe_info(IFrameSegmentInfo {
                            segment_number: i as u32,
                            byte_offset: idr.offset as u64,
                            byte_length: idr.size as u64,
                            duration,
                            segment_uri: seg_uri,
                        });
                    }
                }

                progressive.add_segment(i as u32, new_segment.clone(), duration);

                // Add ad breaks to this output
                for ab in &ad_breaks {
                    progressive.add_ad_break(ab.clone());
                }

                if is_last {
                    progressive.finalize();
                }
            }

        }

        // Clean up sensitive DRM data — keys are no longer needed after all segments are processed
        self.cleanup_sensitive_data(content_id);

        Ok(outputs)
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
        let mut source = if url.contains(".m3u8") || text.starts_with("#EXTM3U") {
            crate::manifest::hls_input::parse_hls_manifest(&text, url)?
        } else {
            crate::manifest::dash_input::parse_dash_manifest(&text, url)?
        };

        // Resolve SegmentBase (on-demand DASH) by fetching sidx and populating segments
        if source.segment_base.is_some() {
            source = self.resolve_segment_base(source)?;
        }

        Ok(source)
    }

    /// Resolve a DASH SegmentBase manifest by fetching the sidx (Segment Index) box.
    ///
    /// For on-demand DASH profiles, the manifest specifies a single file per representation
    /// with byte ranges for the init segment and sidx box. This method fetches the sidx,
    /// parses it to discover subsegment byte ranges and durations, and populates the
    /// `segment_urls`, `segment_durations`, and `segment_byte_ranges` fields.
    fn resolve_segment_base(&self, mut source: SourceManifest) -> Result<SourceManifest> {
        let sb = match source.segment_base.take() {
            Some(sb) => sb,
            None => return Ok(source),
        };

        // Fetch the sidx box using a byte-range request
        let sidx_data = self.fetch_segment_with_range(&sb.file_url, Some(sb.index_range))?;

        // Parse the sidx box
        let sidx = crate::media::cmaf::parse_sidx(&sidx_data)?;

        // Use the sidx timescale if available, falling back to the SegmentBase timescale
        let effective_timescale = if sidx.timescale > 0 {
            sidx.timescale as u64
        } else if sb.timescale > 0 {
            sb.timescale
        } else {
            1
        };

        // Calculate byte offsets for each subsegment.
        // The first subsegment starts at: end_of_sidx_box + first_offset
        // where end_of_sidx_box = index_range.end + 1
        let mut offset = sb.index_range.1 + 1 + sidx.first_offset;
        for reference in &sidx.references {
            if reference.reference_type {
                // This references another sidx box — skip (hierarchical sidx not yet supported)
                continue;
            }
            let end = offset + reference.referenced_size as u64 - 1;
            source.segment_urls.push(sb.file_url.clone());
            source.segment_byte_ranges.push((offset, end));
            source.segment_durations.push(
                reference.subsegment_duration as f64 / effective_timescale as f64,
            );
            offset = end + 1;
        }

        log::info!(
            "resolved SegmentBase: {} subsegments from sidx ({} references)",
            source.segment_urls.len(),
            sidx.references.len(),
        );

        Ok(source)
    }

    /// Fetch a single segment (init or media) from origin.
    fn fetch_segment(&self, url: &str) -> Result<Vec<u8>> {
        self.fetch_segment_with_range(url, None)
    }

    /// Fetch a segment with an optional byte range.
    ///
    /// When `byte_range` is `Some((start, end))`, sends an HTTP Range header
    /// to fetch only the specified byte range (inclusive).
    fn fetch_segment_with_range(&self, url: &str, byte_range: Option<(u64, u64)>) -> Result<Vec<u8>> {
        let headers = if let Some((start, end)) = byte_range {
            vec![("Range".to_string(), format!("bytes={start}-{end}"))]
        } else {
            vec![]
        };

        let response = crate::http_client::get(url, &headers)?;

        // Accept both 200 (full) and 206 (partial content) as success
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
        if let Some(cached) = crate::cache::global_cache().get(&cache_key)? {
            if let Ok(key_set) = serde_json::from_slice::<CachedKeySet>(&cached) {
                return Ok(key_set.into());
            }
        }

        // Fetch from SPEKE
        let key_set = self.speke.request_keys(content_id, key_ids)?;

        // Cache the keys
        let cacheable = CachedKeySet::from(&key_set);
        if let Ok(json) = serde_json::to_vec(&cacheable) {
            let _ = crate::cache::global_cache().set(&cache_key, &json, self.config.cache.vod_max_age);
        }

        Ok(key_set)
    }

    /// Delete sensitive DRM data from cache after it is no longer needed.
    ///
    /// Removes the raw DRM content keys and SPEKE response cache entries.
    /// Errors are intentionally swallowed — cleanup failure must not prevent
    /// the pipeline from returning a successful result.
    fn cleanup_sensitive_data(&self, content_id: &str) {
        let cache = crate::cache::global_cache();
        let _ = cache.delete(&CacheKeys::drm_keys(content_id));
        let _ = cache.delete(&CacheKeys::speke_response(content_id));
    }

    // --- JIT Packaging Methods (Phase 8) ---

    /// JIT setup: fetch source manifest, init segment, and DRM keys, then cache
    /// everything needed for subsequent per-segment JIT requests.
    ///
    /// This is the expensive initial operation triggered on the first GET for content.
    /// After setup, manifests and init segments are immediately available in cache.
    /// Media segments are processed individually on demand via `jit_segment()`.
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
        let ttl = self.config.cache.vod_max_age;

        // Check if setup is already done (idempotency)
        let setup_key = CacheKeys::jit_setup(content_id, &fmt);
        if crate::cache::global_cache().exists(&setup_key)? {
            // Load existing manifest state and init segment
            let state_data = crate::cache::global_cache().get(&CacheKeys::manifest_state_for_scheme(content_id, &fmt, scheme_str))?;
            let init_data = crate::cache::global_cache().get(&CacheKeys::init_segment_for_scheme(content_id, &fmt, scheme_str))?;
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
        crate::cache::global_cache().set(&CacheKeys::source_manifest(content_id, &fmt), &source_json, ttl)?;

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
            #[cfg(feature = "ts")]
            ts_mux_config: if !container_format.is_isobmff() {
                Some(crate::media::ts_mux::extract_mux_config(&init_data)?)
            } else {
                None
            },
        };
        let cont_json = serde_json::to_vec(&continuation)
            .map_err(|e| EdgepackError::Cache(format!("serialize rewrite params: {e}")))?;
        crate::cache::global_cache().set(
            &CacheKeys::rewrite_params_for_scheme(content_id, &fmt, scheme_str),
            &cont_json,
            ttl,
        )?;

        // Step 8: Cache init segment
        crate::cache::global_cache().set(
            &CacheKeys::init_segment_for_scheme(content_id, &fmt, scheme_str),
            &new_init,
            ttl,
        )?;

        // Step 9: Build manifest state with all segment entries (but not yet processed)
        let drm_info = if target_scheme.is_encrypted() {
            let ks = key_set.as_ref().unwrap();
            Some(build_manifest_drm_info(ks, &primary_kid, &key_mapping, target_scheme, &[]))
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
                key_period: None,
            }
        }).collect();

        let target_duration = source.segment_durations.iter().copied()
            .fold(0.0_f64, f64::max)
            .max(6.0);

        use crate::manifest::types::InitSegmentInfo;
        #[cfg(feature = "ts")]
        let jit_init_segment = if !container_format.is_isobmff() {
            None // TS output has no init segment
        } else {
            Some(InitSegmentInfo {
                uri: format!("{base_url}init.mp4"),
                byte_size: new_init.len() as u64,
            })
        };
        #[cfg(not(feature = "ts"))]
        let jit_init_segment = Some(InitSegmentInfo {
            uri: format!("{base_url}init.mp4"),
            byte_size: new_init.len() as u64,
        });

        let manifest_state = ManifestState {
            content_id: content_id.to_string(),
            format: output_format,
            phase: ManifestPhase::Complete,
            init_segment: jit_init_segment,
            segments: manifest_segments,
            target_duration,
            variants: build_variants_from_tracks(&tracks),
            drm_info,
            media_sequence: 0,
            base_url: base_url.to_string(),
            container_format,
            cea_captions: Vec::new(),
            ad_breaks: Vec::new(),
            rotation_drm_info: Vec::new(),
            clear_lead_boundary: None,
            parts: Vec::new(),
            part_target_duration: None,
            server_control: None,
            ll_dash_info: None,
            iframe_segments: Vec::new(),
            enable_iframe_playlist: false,
            dvr_window_duration: None,
            content_steering: None,
            cache_control: None,
        };

        let state_json = serde_json::to_vec(&manifest_state)
            .map_err(|e| EdgepackError::Cache(format!("serialize manifest state: {e}")))?;
        crate::cache::global_cache().set(
            &CacheKeys::manifest_state_for_scheme(content_id, &fmt, scheme_str),
            &state_json,
            ttl,
        )?;

        // Step 10: Set JIT setup marker
        crate::cache::global_cache().set(&setup_key, b"1", ttl)?;

        // Clean up raw DRM keys — they're now embedded in encrypted rewrite_params
        self.cleanup_sensitive_data(content_id);

        Ok(JitSetupResult {
            manifest_state,
            init_segment_data: new_init,
        })
    }

    /// JIT segment: fetch, decrypt, re-encrypt, and cache a single media segment on demand.
    ///
    /// Requires `jit_setup()` to have been called first (loads source manifest and
    /// rewrite params from cache). Returns the rewritten segment bytes.
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
        if let Some(cached) = crate::cache::global_cache().get(&seg_cache_key)? {
            return Ok(cached);
        }

        // Load source manifest
        let source_data = crate::cache::global_cache().get(&CacheKeys::source_manifest(content_id, &fmt))?
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
        let params_data = crate::cache::global_cache().get(&CacheKeys::rewrite_params_for_scheme(content_id, &fmt, scheme_str))?
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

        #[cfg(feature = "ts")]
        let is_ts_output = !continuation.container_format.is_isobmff();
        #[cfg(not(feature = "ts"))]
        let is_ts_output = false;

        // Rewrite segment
        let new_segment = if is_ts_output {
            #[cfg(feature = "ts")]
            {
                let ts_config = continuation.ts_mux_config.as_ref().ok_or_else(|| {
                    EdgepackError::MediaParse("TS mux config missing in continuation params".into())
                })?;

                let clear_cmaf = if continuation.source_scheme.is_encrypted() {
                    let clear_params = SegmentRewriteParams {
                        source_key: source_key.clone(),
                        target_key: None,
                        source_scheme: continuation.source_scheme,
                        target_scheme: EncryptionScheme::None,
                        source_iv_size: continuation.source_iv_size,
                        target_iv_size: 0,
                        source_pattern: continuation.source_pattern,
                        target_pattern: (0, 0),
                        constant_iv: continuation.constant_iv,
                        segment_number,
                    };
                    segment::rewrite_segment(&seg_data, &clear_params)?
                } else {
                    seg_data
                };

                let ts_bytes = crate::media::ts_mux::mux_to_ts(&clear_cmaf, ts_config, segment_number)?;

                if continuation.target_scheme.is_encrypted() {
                    let key = target_key.as_ref()
                        .or(source_key.as_ref())
                        .ok_or_else(|| EdgepackError::Encryption("key required for TS encryption".into()))?;
                    let key_bytes: &[u8; 16] = key.key.as_slice().try_into()
                        .map_err(|_| EdgepackError::Encryption("content key must be 16 bytes for TS AES-128".into()))?;
                    let iv = generate_ts_iv(segment_number);
                    crate::media::ts_mux::encrypt_ts_segment(&ts_bytes, key_bytes, &iv)?
                } else {
                    ts_bytes
                }
            }
            #[cfg(not(feature = "ts"))]
            {
                return Err(EdgepackError::InvalidInput("TS output requires the 'ts' feature".into()));
            }
        } else {
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
            segment::rewrite_segment(&seg_data, &params)?
        };

        // Cache the result
        let ttl = self.config.cache.vod_max_age;
        crate::cache::global_cache().set(&seg_cache_key, &new_segment, ttl)?;

        Ok(new_segment)
    }
}

/// Build a DrmKeySet from raw key entries (bypass SPEKE).
fn build_key_set_from_raw_keys(raw_keys: &[crate::repackager::RawKeyEntry]) -> DrmKeySet {
    let keys = raw_keys
        .iter()
        .map(|rk| ContentKey {
            kid: rk.kid,
            key: rk.key.to_vec(),
            iv: rk.iv.map(|iv| iv.to_vec()),
        })
        .collect();
    DrmKeySet {
        keys,
        drm_systems: vec![],
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
    drm_systems_override: &[String],
) -> ManifestDrmInfo {
    let b64 = &base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let kid_hex: String = kid.iter().map(|b| format!("{b:02x}")).collect();

    // DRM systems filtering
    let include_widevine = drm_systems_override.is_empty() || drm_systems_override.iter().any(|s| s == "widevine");
    let include_playready = drm_systems_override.is_empty() || drm_systems_override.iter().any(|s| s == "playready");
    let include_fairplay = drm_systems_override.is_empty() || drm_systems_override.iter().any(|s| s == "fairplay");
    let include_clearkey = drm_systems_override.iter().any(|s| s == "clearkey");

    let widevine_pssh = if include_widevine {
        build_manifest_pssh_for_system(key_set, key_mapping, crate::drm::system_ids::WIDEVINE)
            .map(|pssh_box| b64.encode(&pssh_box))
    } else {
        None
    };

    let playready_pssh = if include_playready {
        build_manifest_pssh_for_system(key_set, key_mapping, crate::drm::system_ids::PLAYREADY)
            .map(|pssh_box| b64.encode(&pssh_box))
    } else {
        None
    };

    let playready_pro = if include_playready {
        key_set.drm_systems.iter()
            .find(|d| d.system_id == crate::drm::system_ids::PLAYREADY)
            .and_then(|d| d.content_protection_data.clone())
    } else {
        None
    };

    let fairplay_key_uri = if include_fairplay && target_scheme == EncryptionScheme::Cbcs {
        key_set.drm_systems.iter()
            .find(|d| d.system_id == crate::drm::system_ids::FAIRPLAY)
            .and_then(|d| d.content_protection_data.clone())
    } else {
        None
    };

    // ClearKey: build PSSH locally from KIDs
    let clearkey_pssh = if include_clearkey {
        let all_kids = key_mapping.all_kids();
        let kids_owned: Vec<[u8; 16]> = all_kids;
        let pssh_data = crate::drm::build_clearkey_pssh_data(&kids_owned);
        let pssh_box = crate::media::cmaf::build_pssh_box(&crate::media::cmaf::PsshBox {
            version: 1,
            system_id: crate::drm::system_ids::CLEARKEY,
            key_ids: kids_owned,
            data: pssh_data,
        });
        Some(b64.encode(&pssh_box))
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
        clearkey_pssh,
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

/// Generate a 16-byte IV for TS AES-128-CBC encryption from a segment number.
///
/// Per HLS spec, the IV is the segment sequence number as a 128-bit big-endian integer.
#[cfg(feature = "ts")]
fn generate_ts_iv(segment_number: u32) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[12..16].copy_from_slice(&segment_number.to_be_bytes());
    iv
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
    /// TS mux configuration for CMAF→TS conversion (TS output only).
    #[cfg(feature = "ts")]
    #[serde(default)]
    ts_mux_config: Option<crate::media::ts_mux::TsMuxConfig>,
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
/// 1. In-process cache (pre-populated source config)
/// 2. URL pattern from `JitConfig.source_url_pattern` (replaces `{content_id}`)
/// 3. Error if neither is available
///
/// The `scheme_from_url` parameter allows the URL path scheme to override
/// the default target scheme from the source config.
pub fn resolve_source_config(
    content_id: &str,
    config: &AppConfig,
    scheme_from_url: Option<&str>,
) -> Result<crate::repackager::SourceConfig> {
    use crate::repackager::SourceConfig;

    let cache = crate::cache::global_cache();

    // 1. Check cache for per-content config
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
fn parse_scheme_str(s: &str) -> Option<EncryptionScheme> {
    match s {
        "cenc" => Some(EncryptionScheme::Cenc),
        "cbcs" => Some(EncryptionScheme::Cbcs),
        "none" => Some(EncryptionScheme::None),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// MPEG-TS processing (feature-gated)
// ---------------------------------------------------------------------------

/// Process a TS segment: optionally decrypt, demux, and transmux to CMAF.
///
/// On the first segment, extracts codec config and synthesizes a CMAF init segment.
/// Returns `(cmaf_segment_data, Option<init_segment_data>)`.
#[cfg(feature = "ts")]
fn process_ts_segment(
    segment_data: &[u8],
    _source_manifest: &SourceManifest,
    ts_state: &mut Option<(
        Option<crate::media::transmux::VideoConfig>,
        Option<crate::media::transmux::AudioConfig>,
    )>,
    sequence_number: u32,
) -> Result<(Vec<u8>, Option<Vec<u8>>)> {
    use crate::media::ts;
    use crate::media::transmux;

    // Step 1: AES-128 decryption is handled at a higher level when keys are available.
    // For now, the pipeline passes through the raw data. TS segment-level AES-128
    // decryption support will be completed when key fetching is integrated.
    let data = segment_data;

    // Step 2: Demux TS to PES packets
    let demuxed = ts::demux_segment(data)?;

    // Step 3: Extract config from first segment if not already done
    let init_data = if ts_state.is_none() {
        let video_config = if !demuxed.video_packets.is_empty() {
            Some(transmux::extract_video_config(&demuxed.video_packets[0])?)
        } else {
            None
        };
        let audio_config = if !demuxed.audio_packets.is_empty() {
            Some(transmux::extract_audio_config(&demuxed.audio_packets[0])?)
        } else {
            None
        };

        let init = transmux::synthesize_init_segment(
            video_config.as_ref(),
            audio_config.as_ref(),
        )?;

        *ts_state = Some((video_config, audio_config));
        Some(init)
    } else {
        None
    };

    // Step 4: Transmux PES to CMAF moof+mdat
    let (ref video_config, ref audio_config) = ts_state.as_ref().unwrap();
    let cmaf_segment = transmux::transmux_to_cmaf(
        &demuxed,
        video_config.as_ref(),
        audio_config.as_ref(),
        sequence_number,
    )?;

    Ok((cmaf_segment, init_data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::{system_ids, DrmSystemData};
    use crate::media::container::ContainerFormat;

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
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc, &[]);

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
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc, &[]);

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
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc, &[]);
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
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cbcs, &[]);

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
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc, &[]);

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
        let info = build_manifest_drm_info(&key_set, &video_kid, &mapping, EncryptionScheme::Cenc, &[]);

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
            #[cfg(feature = "ts")]
            ts_mux_config: None,
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
            #[cfg(feature = "ts")]
            ts_mux_config: None,
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

    // --- resolve_source_config tests ---

    fn make_test_config() -> AppConfig {
        use crate::config::*;
        AppConfig {
            drm: DrmConfig {
                speke_url: crate::url::Url::parse("https://speke.test/v2").unwrap(),
                speke_auth: SpekeAuth::Bearer("test".into()),
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
            jit: JitConfig::default(),
            policy: PolicyConfig::default(),
        }
    }

    #[test]
    fn resolve_source_config_from_cache() {
        let cache = crate::cache::global_cache();
        let config = make_test_config();

        // Store a SourceConfig in cache
        let source = crate::repackager::SourceConfig {
            source_url: "https://origin.example.com/manifest.m3u8".into(),
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::Cmaf,
        };
        let data = serde_json::to_vec(&source).unwrap();
        cache.set(&CacheKeys::source_config("test-id"), &data, 3600).unwrap();

        let resolved = resolve_source_config("test-id", &config, None).unwrap();
        assert_eq!(resolved.source_url, "https://origin.example.com/manifest.m3u8");
        assert_eq!(resolved.target_schemes, vec![EncryptionScheme::Cenc]);

        // Clean up
        let _ = cache.delete(&CacheKeys::source_config("test-id"));
    }

    #[test]
    fn resolve_source_config_from_pattern() {
        let mut config = make_test_config();
        config.jit.source_url_pattern = Some("https://origin.example.com/{content_id}/master.m3u8".into());

        let resolved = resolve_source_config("movie-123-pattern", &config, None).unwrap();
        assert_eq!(resolved.source_url, "https://origin.example.com/movie-123-pattern/master.m3u8");
    }

    #[test]
    fn resolve_source_config_missing() {
        let config = make_test_config();

        let result = resolve_source_config("nonexistent-resolve", &config, None);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no source configuration"));
    }

    #[test]
    fn resolve_source_config_scheme_override() {
        let cache = crate::cache::global_cache();
        let config = make_test_config();

        // Store a SourceConfig with cenc
        let source = crate::repackager::SourceConfig {
            source_url: "https://origin.example.com/manifest.m3u8".into(),
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::Cmaf,
        };
        let data = serde_json::to_vec(&source).unwrap();
        cache.set(&CacheKeys::source_config("test-id-override"), &data, 3600).unwrap();

        // Override with cbcs from URL
        let resolved = resolve_source_config("test-id-override", &config, Some("cbcs")).unwrap();
        assert_eq!(resolved.target_schemes, vec![EncryptionScheme::Cbcs]);

        // Clean up
        let _ = cache.delete(&CacheKeys::source_config("test-id-override"));
    }

    #[test]
    fn resolve_source_config_pattern_with_scheme() {
        let mut config = make_test_config();
        config.jit.source_url_pattern = Some("https://cdn.test/{content_id}/index.m3u8".into());
        config.jit.default_target_scheme = EncryptionScheme::Cenc;

        let resolved = resolve_source_config("vid-1-scheme", &config, Some("cbcs")).unwrap();
        assert_eq!(resolved.source_url, "https://cdn.test/vid-1-scheme/index.m3u8");
        assert_eq!(resolved.target_schemes, vec![EncryptionScheme::Cbcs]);
    }

    // --- build_key_set_from_raw_keys tests ---

    #[test]
    fn build_key_set_from_raw_keys_single() {
        let raw = vec![crate::repackager::RawKeyEntry {
            kid: [0xAA; 16],
            key: [0xBB; 16],
            iv: Some([0xCC; 16]),
        }];
        let ks = build_key_set_from_raw_keys(&raw);
        assert_eq!(ks.keys.len(), 1);
        assert_eq!(ks.keys[0].kid, [0xAA; 16]);
        assert_eq!(ks.keys[0].key, vec![0xBB; 16]);
        assert_eq!(ks.keys[0].iv, Some(vec![0xCC; 16]));
        assert!(ks.drm_systems.is_empty());
    }

    #[test]
    fn build_key_set_from_raw_keys_multi() {
        let raw = vec![
            crate::repackager::RawKeyEntry { kid: [0x01; 16], key: [0x11; 16], iv: None },
            crate::repackager::RawKeyEntry { kid: [0x02; 16], key: [0x22; 16], iv: None },
        ];
        let ks = build_key_set_from_raw_keys(&raw);
        assert_eq!(ks.keys.len(), 2);
        assert_eq!(ks.keys[0].kid, [0x01; 16]);
        assert_eq!(ks.keys[1].kid, [0x02; 16]);
    }

    #[test]
    fn build_key_set_from_raw_keys_empty() {
        let ks = build_key_set_from_raw_keys(&[]);
        assert!(ks.keys.is_empty());
        assert!(ks.drm_systems.is_empty());
    }

    #[test]
    fn build_key_set_from_raw_keys_finds_key_by_kid() {
        let raw = vec![
            crate::repackager::RawKeyEntry { kid: [0xAA; 16], key: [0x11; 16], iv: None },
            crate::repackager::RawKeyEntry { kid: [0xBB; 16], key: [0x22; 16], iv: None },
        ];
        let ks = build_key_set_from_raw_keys(&raw);
        let found = find_key_for_kid(&ks, &[0xBB; 16]).unwrap();
        assert_eq!(found.key, vec![0x22; 16]);
    }

    // --- build_manifest_drm_info with drm_systems_override tests ---

    #[test]
    fn build_manifest_drm_info_drm_systems_filter_widevine_only() {
        let key_set = make_key_set();
        let kid = [0x01; 16];
        let mapping = TrackKeyMapping::single(kid);
        let override_list = vec!["widevine".to_string()];
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc, &override_list);
        assert!(info.widevine_pssh.is_some());
        assert!(info.playready_pssh.is_none());
        assert!(info.playready_pro.is_none());
    }

    #[test]
    fn build_manifest_drm_info_drm_systems_clearkey() {
        let key_set = DrmKeySet {
            keys: vec![],
            drm_systems: vec![],
        };
        let kid = [0x01; 16];
        let mapping = TrackKeyMapping::single(kid);
        let override_list = vec!["clearkey".to_string()];
        let info = build_manifest_drm_info(&key_set, &kid, &mapping, EncryptionScheme::Cenc, &override_list);
        assert!(info.clearkey_pssh.is_some());
        // Widevine and PlayReady should be excluded
        assert!(info.widevine_pssh.is_none());
        assert!(info.playready_pssh.is_none());
    }
}
