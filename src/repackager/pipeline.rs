use crate::cache::{CacheBackend, CacheKeys};
use crate::config::AppConfig;
use crate::drm::scheme::EncryptionScheme;
use crate::drm::speke::SpekeClient;
use crate::drm::{ContentKey, DrmKeySet};
use crate::error::{EdgePackagerError, Result};
use crate::manifest::types::{
    ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat, SegmentInfo, SourceManifest,
};
use crate::media::init;
use crate::media::segment::{self, SegmentRewriteParams};
use crate::repackager::progressive::ProgressiveOutput;
use crate::repackager::{JobState, JobStatus, RepackageRequest};

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
    /// This is the original entry point. For WASI environments with request timeouts,
    /// prefer `execute_first()` + `execute_remaining()` for chunked processing.
    pub fn execute(&self, request: &RepackageRequest) -> Result<JobStatus> {
        let content_id = &request.content_id;
        let format = request.output_format;

        // Update job state: FetchingKeys
        self.update_job_state(content_id, format, JobState::FetchingKeys, 0, None)?;

        // Step 1: Fetch the source manifest to discover segments and encryption info
        let source = self.fetch_source_manifest(&request.source_url)?;

        // Step 2: Fetch the init segment and parse protection info
        let init_data = self.fetch_segment(&source.init_segment_url)?;
        let protection_info = init::parse_protection_info(&init_data)?
            .ok_or_else(|| EdgePackagerError::Drm("source content is not encrypted".into()))?;

        // Detect source encryption scheme
        let source_scheme = EncryptionScheme::from_scheme_type(&protection_info.scheme_type)
            .ok_or_else(|| EdgePackagerError::Drm(format!(
                "unsupported encryption scheme: {:?}",
                std::str::from_utf8(&protection_info.scheme_type)
            )))?;

        let target_scheme = request.target_scheme;
        let target_iv_size = target_scheme.default_iv_size();
        let target_pattern = target_scheme.default_video_pattern();
        let source_pattern = (
            protection_info.tenc.default_crypt_byte_block,
            protection_info.tenc.default_skip_byte_block,
        );

        // Step 3: Get content keys via SPEKE 2.0
        let key_ids = vec![protection_info.tenc.default_kid];
        let key_set = self.get_or_fetch_keys(content_id, &key_ids)?;

        // Step 4: Find source and target keys
        let source_key = find_key_for_kid(&key_set, &protection_info.tenc.default_kid)?;
        let target_key = source_key.clone(); // Same key, different encryption scheme

        // Step 5: Rewrite init segment for target scheme
        let new_init = init::rewrite_init_segment(&init_data, &key_set, target_scheme, target_iv_size, target_pattern)?;

        // Step 6: Set up progressive output
        let base_url = format!("/repackage/{content_id}/{}/", format_str(format));
        let drm_info = build_manifest_drm_info(&key_set, &protection_info.tenc.default_kid, target_scheme);
        let mut progressive =
            ProgressiveOutput::new(content_id.clone(), format, base_url, drm_info);

        // Register init segment
        progressive.set_init_segment(new_init);

        // Step 7: Process each media segment
        self.update_job_state(
            content_id,
            format,
            JobState::Processing,
            0,
            Some(source.segment_urls.len() as u32),
        )?;

        for (i, segment_url) in source.segment_urls.iter().enumerate() {
            let seg_data = self.fetch_segment(segment_url)?;

            let params = SegmentRewriteParams {
                source_key: source_key.clone(),
                target_key: target_key.clone(),
                source_scheme,
                target_scheme,
                source_iv_size: protection_info.tenc.default_per_sample_iv_size,
                target_iv_size,
                source_pattern,
                target_pattern,
                constant_iv: protection_info.tenc.default_constant_iv.clone(),
                segment_number: i as u32,
            };

            let new_segment = segment::rewrite_segment(&seg_data, &params)?;

            let duration = source
                .segment_durations
                .get(i)
                .copied()
                .unwrap_or(6.0);

            let is_last = i == source.segment_urls.len() - 1 && !source.is_live;

            // Add to progressive output
            progressive.add_segment(i as u32, new_segment, duration);

            if is_last {
                progressive.finalize();
            }

            // Save manifest state to Redis for coordination
            let manifest_state = progressive.manifest_state();
            let state_json = serde_json::to_vec(manifest_state)
                .map_err(|e| EdgePackagerError::Cache(format!("serialize state: {e}")))?;
            let key = CacheKeys::manifest_state(content_id, &format_str(format));
            self.cache.set(&key, &state_json, self.config.cache.job_state_ttl)?;

            self.update_job_state(
                content_id,
                format,
                if is_last { JobState::Complete } else { JobState::Processing },
                (i + 1) as u32,
                Some(source.segment_urls.len() as u32),
            )?;
        }

        // Clean up sensitive cache entries now that processing is complete
        self.cleanup_sensitive_data(content_id, format);

        Ok(JobStatus {
            content_id: content_id.clone(),
            format,
            state: JobState::Complete,
            segments_completed: source.segment_urls.len() as u32,
            segments_total: Some(source.segment_urls.len() as u32),
        })
    }

    /// Execute the pipeline through the first segment, producing a live manifest.
    ///
    /// This is the first half of the split execution model for WASI environments.
    /// After this returns, the manifest URL is immediately usable. The caller should
    /// chain `execute_remaining()` via self-invocation for the rest.
    pub fn execute_first(&self, request: &RepackageRequest) -> Result<JobStatus> {
        let content_id = &request.content_id;
        let format = request.output_format;
        let fmt = format_str(format);
        let ttl = self.config.cache.job_state_ttl;

        // Step 1: Fetch source manifest
        self.update_job_state(content_id, format, JobState::FetchingKeys, 0, None)?;
        let source = self.fetch_source_manifest(&request.source_url)?;
        let total = source.segment_urls.len() as u32;

        // Store source manifest in Redis for continuation
        let source_json = serde_json::to_vec(&source)
            .map_err(|e| EdgePackagerError::Cache(format!("serialize source manifest: {e}")))?;
        self.cache
            .set(&CacheKeys::source_manifest(content_id, &fmt), &source_json, ttl)?;

        // Step 2: Fetch init segment and parse protection info
        let init_data = self.fetch_segment(&source.init_segment_url)?;
        let protection_info = init::parse_protection_info(&init_data)?
            .ok_or_else(|| EdgePackagerError::Drm("source content is not encrypted".into()))?;

        // Detect source encryption scheme
        let source_scheme = EncryptionScheme::from_scheme_type(&protection_info.scheme_type)
            .ok_or_else(|| EdgePackagerError::Drm(format!(
                "unsupported encryption scheme: {:?}",
                std::str::from_utf8(&protection_info.scheme_type)
            )))?;

        let target_scheme = request.target_scheme;
        let target_iv_size = target_scheme.default_iv_size();
        let target_pattern = target_scheme.default_video_pattern();
        let source_pattern = (
            protection_info.tenc.default_crypt_byte_block,
            protection_info.tenc.default_skip_byte_block,
        );

        // Step 3: Get DRM keys
        let key_ids = vec![protection_info.tenc.default_kid];
        let key_set = self.get_or_fetch_keys(content_id, &key_ids)?;

        // Step 4: Build and store rewrite parameters for continuation
        let source_key = find_key_for_kid(&key_set, &protection_info.tenc.default_kid)?;
        let target_key = source_key.clone();

        let continuation = ContinuationParams {
            source_key: CachedKey {
                kid: source_key.kid.to_vec(),
                key: source_key.key.clone(),
                iv: source_key.iv.clone(),
            },
            target_key: CachedKey {
                kid: target_key.kid.to_vec(),
                key: target_key.key.clone(),
                iv: target_key.iv.clone(),
            },
            source_scheme,
            target_scheme,
            source_iv_size: protection_info.tenc.default_per_sample_iv_size,
            target_iv_size,
            source_pattern,
            target_pattern,
            constant_iv: protection_info.tenc.default_constant_iv.clone(),
        };
        let cont_json = serde_json::to_vec(&continuation)
            .map_err(|e| EdgePackagerError::Cache(format!("serialize rewrite params: {e}")))?;
        self.cache
            .set(&CacheKeys::rewrite_params(content_id, &fmt), &cont_json, ttl)?;

        // Step 5: Rewrite init segment for target scheme
        let new_init = init::rewrite_init_segment(&init_data, &key_set, target_scheme, target_iv_size, target_pattern)?;

        // Store init segment in Redis
        self.cache
            .set(&CacheKeys::init_segment(content_id, &fmt), &new_init, ttl)?;

        // Step 6: Set up progressive output
        let base_url = format!("/repackage/{content_id}/{fmt}/");
        let drm_info = build_manifest_drm_info(&key_set, &protection_info.tenc.default_kid, target_scheme);
        let mut progressive =
            ProgressiveOutput::new(content_id.clone(), format, base_url, drm_info);
        progressive.set_init_segment(new_init);

        // Step 7: Process first media segment
        self.update_job_state(content_id, format, JobState::Processing, 0, Some(total))?;

        let seg_data = self.fetch_segment(&source.segment_urls[0])?;
        let params = SegmentRewriteParams {
            source_key,
            target_key,
            source_scheme,
            target_scheme,
            source_iv_size: protection_info.tenc.default_per_sample_iv_size,
            target_iv_size,
            source_pattern,
            target_pattern,
            constant_iv: protection_info.tenc.default_constant_iv.clone(),
            segment_number: 0,
        };
        let new_segment = segment::rewrite_segment(&seg_data, &params)?;

        // Store segment in Redis
        self.cache.set(
            &CacheKeys::media_segment(content_id, &fmt, 0),
            &new_segment,
            ttl,
        )?;

        let is_last = source.segment_urls.len() == 1 && !source.is_live;

        progressive.add_segment(0, new_segment, source.segment_durations.first().copied().unwrap_or(6.0));
        if is_last {
            progressive.finalize();
        }

        // Save manifest state
        let manifest_state = progressive.manifest_state();
        let state_json = serde_json::to_vec(manifest_state)
            .map_err(|e| EdgePackagerError::Cache(format!("serialize manifest state: {e}")))?;
        self.cache
            .set(&CacheKeys::manifest_state(content_id, &fmt), &state_json, ttl)?;

        let state = if is_last {
            // Single-segment content: clean up immediately since execute_remaining() won't run
            self.cleanup_sensitive_data(content_id, format);
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
    /// Loads source manifest, rewrite params, and manifest state from Redis,
    /// processes the next segment, and updates state. Returns the updated status.
    pub fn execute_remaining(&self, content_id: &str, format: OutputFormat) -> Result<JobStatus> {
        let fmt = format_str(format);
        let ttl = self.config.cache.job_state_ttl;

        // Load source manifest from Redis
        let source_data = self
            .cache
            .get(&CacheKeys::source_manifest(content_id, &fmt))?
            .ok_or_else(|| {
                EdgePackagerError::Cache(format!(
                    "source manifest not found in cache for {content_id}/{fmt}"
                ))
            })?;
        let source: SourceManifest = serde_json::from_slice(&source_data)
            .map_err(|e| EdgePackagerError::Cache(format!("deserialize source manifest: {e}")))?;

        // Load rewrite params
        let params_data = self
            .cache
            .get(&CacheKeys::rewrite_params(content_id, &fmt))?
            .ok_or_else(|| {
                EdgePackagerError::Cache(format!(
                    "rewrite params not found in cache for {content_id}/{fmt}"
                ))
            })?;
        let continuation: ContinuationParams = serde_json::from_slice(&params_data)
            .map_err(|e| EdgePackagerError::Cache(format!("deserialize rewrite params: {e}")))?;

        // Load current manifest state
        let state_data = self
            .cache
            .get(&CacheKeys::manifest_state(content_id, &fmt))?
            .ok_or_else(|| {
                EdgePackagerError::Cache(format!(
                    "manifest state not found in cache for {content_id}/{fmt}"
                ))
            })?;
        let mut manifest_state: ManifestState = serde_json::from_slice(&state_data)
            .map_err(|e| EdgePackagerError::Cache(format!("deserialize manifest state: {e}")))?;

        let segments_done = manifest_state.segments.len();
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

        // Process next segment
        let i = segments_done;
        let seg_data = self.fetch_segment(&source.segment_urls[i])?;

        let source_key = restore_content_key(&continuation.source_key);
        let target_key = restore_content_key(&continuation.target_key);

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

        // Store in Redis
        self.cache.set(
            &CacheKeys::media_segment(content_id, &fmt, i as u32),
            &new_segment,
            ttl,
        )?;

        // Update manifest state
        let uri = format!("{}segment_{i}.cmfv", manifest_state.base_url);
        let duration = source.segment_durations.get(i).copied().unwrap_or(6.0);

        manifest_state.segments.push(SegmentInfo {
            number: i as u32,
            duration,
            uri,
            byte_size: new_segment.len() as u64,
        });
        if duration > manifest_state.target_duration {
            manifest_state.target_duration = duration;
        }

        let is_last = i == total - 1 && !source.is_live;
        if is_last {
            manifest_state.phase = ManifestPhase::Complete;
        }

        // Save updated manifest state
        let state_json = serde_json::to_vec(&manifest_state)
            .map_err(|e| EdgePackagerError::Cache(format!("serialize manifest state: {e}")))?;
        self.cache
            .set(&CacheKeys::manifest_state(content_id, &fmt), &state_json, ttl)?;

        let completed = (i + 1) as u32;
        let state = if is_last {
            // Final segment: clean up all sensitive cache data
            self.cleanup_sensitive_data(content_id, format);
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
            return Err(EdgePackagerError::Http {
                status: response.status,
                message: format!("failed to fetch source manifest: HTTP {}", response.status),
            });
        }

        let text = String::from_utf8(response.body).map_err(|e| EdgePackagerError::Http {
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
            return Err(EdgePackagerError::Http {
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
            .map_err(|e| EdgePackagerError::Cache(format!("serialize job state: {e}")))?;
        let key = CacheKeys::job_state(content_id, &format_str(format));
        self.cache.set(&key, &json, self.config.cache.job_state_ttl)
    }

    /// Delete all sensitive cache entries for a completed job.
    ///
    /// Removes DRM keys, SPEKE response, rewrite params, and source manifest
    /// metadata. Non-sensitive data (job state, manifest state, init/media
    /// segments) is left for CDN serving.
    ///
    /// Cleanup errors are intentionally swallowed — they must not prevent
    /// the pipeline from reporting success to the caller.
    fn cleanup_sensitive_data(&self, content_id: &str, format: OutputFormat) {
        let fmt = format_str(format);
        let _ = self.cache.delete(&CacheKeys::drm_keys(content_id));
        let _ = self.cache.delete(&CacheKeys::speke_response(content_id));
        let _ = self.cache.delete(&CacheKeys::rewrite_params(content_id, &fmt));
        let _ = self.cache.delete(&CacheKeys::source_manifest(content_id, &fmt));
    }
}

fn find_key_for_kid(key_set: &DrmKeySet, kid: &[u8; 16]) -> Result<ContentKey> {
    key_set
        .keys
        .iter()
        .find(|k| &k.kid == kid)
        .cloned()
        .ok_or_else(|| {
            EdgePackagerError::Drm(format!(
                "no key found for KID {:?}",
                crate::drm::cpix::format_uuid(kid)
            ))
        })
}

fn build_manifest_drm_info(
    key_set: &DrmKeySet,
    kid: &[u8; 16],
    target_scheme: EncryptionScheme,
) -> ManifestDrmInfo {
    let b64 = &base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    let kid_hex: String = kid.iter().map(|b| format!("{b:02x}")).collect();

    let widevine_pssh = key_set
        .drm_systems
        .iter()
        .find(|d| d.system_id == crate::drm::system_ids::WIDEVINE)
        .map(|d| {
            let pssh_box = crate::media::cmaf::build_pssh_box(&crate::media::cmaf::PsshBox {
                version: 1,
                system_id: d.system_id,
                key_ids: vec![d.kid],
                data: d.pssh_data.clone(),
            });
            b64.encode(&pssh_box)
        });

    let playready_pssh = key_set
        .drm_systems
        .iter()
        .find(|d| d.system_id == crate::drm::system_ids::PLAYREADY)
        .map(|d| {
            let pssh_box = crate::media::cmaf::build_pssh_box(&crate::media::cmaf::PsshBox {
                version: 1,
                system_id: d.system_id,
                key_ids: vec![d.kid],
                data: d.pssh_data.clone(),
            });
            b64.encode(&pssh_box)
        });

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

fn format_str(format: OutputFormat) -> String {
    match format {
        OutputFormat::Hls => "hls".to_string(),
        OutputFormat::Dash => "dash".to_string(),
    }
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
    source_key: CachedKey,
    target_key: CachedKey,
    source_scheme: EncryptionScheme,
    target_scheme: EncryptionScheme,
    source_iv_size: u8,
    target_iv_size: u8,
    source_pattern: (u8, u8),
    target_pattern: (u8, u8),
    constant_iv: Option<Vec<u8>>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::CacheBackend;
    use crate::drm::{system_ids, DrmSystemData};
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
        let info = build_manifest_drm_info(&key_set, &kid, EncryptionScheme::Cenc);

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
        let info = build_manifest_drm_info(&key_set, &kid, EncryptionScheme::Cenc);

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
        let info = build_manifest_drm_info(&key_set, &kid, EncryptionScheme::Cenc);
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
        let info = build_manifest_drm_info(&key_set, &kid, EncryptionScheme::Cbcs);

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
        let info = build_manifest_drm_info(&key_set, &kid, EncryptionScheme::Cenc);

        assert_eq!(info.encryption_scheme, EncryptionScheme::Cenc);
        assert!(info.fairplay_key_uri.is_none());
    }

    #[test]
    fn continuation_params_serde_roundtrip() {
        let params = ContinuationParams {
            source_key: CachedKey {
                kid: vec![0x01; 16],
                key: vec![0xAA; 16],
                iv: Some(vec![0xBB; 8]),
            },
            target_key: CachedKey {
                kid: vec![0x01; 16],
                key: vec![0xAA; 16],
                iv: None,
            },
            source_scheme: EncryptionScheme::Cbcs,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 8,
            target_iv_size: 8,
            source_pattern: (1, 9),
            target_pattern: (0, 0),
            constant_iv: Some(vec![0xCC; 16]),
        };

        let json = serde_json::to_string(&params).unwrap();
        let parsed: ContinuationParams = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_key.kid.len(), 16);
        assert_eq!(parsed.source_scheme, EncryptionScheme::Cbcs);
        assert_eq!(parsed.target_scheme, EncryptionScheme::Cenc);
        assert_eq!(parsed.source_iv_size, 8);
        assert_eq!(parsed.source_pattern, (1, 9));
        assert_eq!(parsed.target_pattern, (0, 0));
        assert!(parsed.constant_iv.is_some());
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
            redis: RedisConfig {
                url: "unused://localhost".into(),
                token: "test-token".into(),
                backend: RedisBackendType::Http,
            },
            drm: DrmConfig {
                speke_url: url::Url::parse("https://speke.test/v2").unwrap(),
                speke_auth: SpekeAuth::Bearer("test".into()),
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
        }
    }

    #[test]
    fn cleanup_deletes_all_sensitive_keys_hls() {
        let (cache, deleted) = SpyCacheBackend::new();
        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(cache));

        pipeline.cleanup_sensitive_data("my-content", OutputFormat::Hls);

        let keys = deleted.lock().unwrap();
        assert_eq!(keys.len(), 4);
        assert!(keys.contains(&"ep:my-content:keys".to_string()));
        assert!(keys.contains(&"ep:my-content:speke".to_string()));
        assert!(keys.contains(&"ep:my-content:hls:rewrite_params".to_string()));
        assert!(keys.contains(&"ep:my-content:hls:source".to_string()));
    }

    #[test]
    fn cleanup_deletes_all_sensitive_keys_dash() {
        let (cache, deleted) = SpyCacheBackend::new();
        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(cache));

        pipeline.cleanup_sensitive_data("content-42", OutputFormat::Dash);

        let keys = deleted.lock().unwrap();
        assert_eq!(keys.len(), 4);
        assert!(keys.contains(&"ep:content-42:keys".to_string()));
        assert!(keys.contains(&"ep:content-42:speke".to_string()));
        assert!(keys.contains(&"ep:content-42:dash:rewrite_params".to_string()));
        assert!(keys.contains(&"ep:content-42:dash:source".to_string()));
    }

    #[test]
    fn cleanup_does_not_delete_non_sensitive_keys() {
        let (cache, deleted) = SpyCacheBackend::new();
        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(cache));

        pipeline.cleanup_sensitive_data("abc", OutputFormat::Hls);

        let keys = deleted.lock().unwrap();
        // Should NOT contain state, manifest_state, init, or segment keys
        for key in keys.iter() {
            assert!(!key.contains(":state"), "should not delete job state: {key}");
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
        use crate::error::EdgePackagerError;

        /// Cache backend where delete() always fails.
        struct FailingDeleteCache;
        impl CacheBackend for FailingDeleteCache {
            fn get(&self, _: &str) -> crate::error::Result<Option<Vec<u8>>> {
                Ok(None)
            }
            fn set(&self, _: &str, _: &[u8], _: u64) -> crate::error::Result<()> {
                Ok(())
            }
            fn exists(&self, _: &str) -> crate::error::Result<bool> {
                Ok(false)
            }
            fn delete(&self, _: &str) -> crate::error::Result<()> {
                Err(EdgePackagerError::Cache("connection refused".into()))
            }
        }

        let pipeline = RepackagePipeline::new(make_test_config(), Box::new(FailingDeleteCache));

        // Should not panic — errors are swallowed with `let _ =`
        pipeline.cleanup_sensitive_data("test", OutputFormat::Hls);
    }
}
