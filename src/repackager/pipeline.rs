use crate::cache::CacheBackend;
use crate::config::AppConfig;
use crate::drm::speke::SpekeClient;
use crate::drm::{ContentKey, DrmKeySet};
use crate::error::{EdgePackagerError, Result};
use crate::manifest::types::{ManifestDrmInfo, OutputFormat};
use crate::media::init;
use crate::media::segment::{self, SegmentRewriteParams};
use crate::repackager::progressive::ProgressiveOutput;
use crate::repackager::{JobState, JobStatus, RepackageRequest};

/// The main repackaging pipeline.
///
/// Orchestrates: fetch source → get DRM keys → repackage init segment →
/// repackage media segments (progressively) → finalize manifest.
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

    /// Execute the full repackaging pipeline for a content item.
    ///
    /// This is the main entry point called by both on-demand and webhook handlers.
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

        // Verify source is CBCS
        if &protection_info.scheme_type != b"cbcs" {
            return Err(EdgePackagerError::Drm(format!(
                "expected CBCS encryption, found {:?}",
                std::str::from_utf8(&protection_info.scheme_type)
            )));
        }

        // Step 3: Get content keys via SPEKE 2.0
        let key_ids = vec![protection_info.tenc.default_kid];
        let key_set = self.get_or_fetch_keys(content_id, &key_ids)?;

        // Step 4: Find source and target keys
        let source_key = find_key_for_kid(&key_set, &protection_info.tenc.default_kid)?;
        let target_key = source_key.clone(); // Same key, different encryption scheme

        // Step 5: Rewrite init segment (CBCS → CENC)
        let target_iv_size = 8u8;
        let new_init = init::rewrite_init_segment(&init_data, &key_set, target_iv_size)?;

        // Step 6: Set up progressive output
        let base_url = format!("/repackage/{content_id}/{}/", format_str(format));
        let drm_info = build_manifest_drm_info(&key_set, &protection_info.tenc.default_kid);
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
                source_iv_size: protection_info.tenc.default_per_sample_iv_size,
                target_iv_size,
                crypt_byte_block: protection_info.tenc.default_crypt_byte_block,
                skip_byte_block: protection_info.tenc.default_skip_byte_block,
                constant_iv: protection_info.tenc.default_constant_iv.clone(),
                segment_number: i as u32,
            };

            let new_segment = segment::rewrite_segment(&seg_data, &params)?;

            // Estimate segment duration (would come from source manifest in production)
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
            let key = crate::cache::CacheKeys::manifest_state(content_id, &format_str(format));
            self.cache.set(&key, &state_json, self.config.cache.job_state_ttl)?;

            self.update_job_state(
                content_id,
                format,
                if is_last { JobState::Complete } else { JobState::Processing },
                (i + 1) as u32,
                Some(source.segment_urls.len() as u32),
            )?;
        }

        Ok(JobStatus {
            content_id: content_id.clone(),
            format,
            state: JobState::Complete,
            segments_completed: source.segment_urls.len() as u32,
            segments_total: Some(source.segment_urls.len() as u32),
        })
    }

    /// Fetch source manifest and parse segment URLs.
    ///
    /// TODO: Implement using wasi:http/outgoing-handler
    fn fetch_source_manifest(&self, _url: &str) -> Result<SourceManifest> {
        // In production, this would:
        // 1. HTTP GET the source manifest URL
        // 2. Parse HLS M3U8 or DASH MPD
        // 3. Extract init segment URL, media segment URLs, durations
        // 4. Determine if source is live or VOD

        Err(EdgePackagerError::Http {
            status: 0,
            message: "WASI HTTP transport not yet implemented".into(),
        })
    }

    /// Fetch a single segment from origin.
    ///
    /// TODO: Implement using wasi:http/outgoing-handler
    fn fetch_segment(&self, _url: &str) -> Result<Vec<u8>> {
        Err(EdgePackagerError::Http {
            status: 0,
            message: "WASI HTTP transport not yet implemented".into(),
        })
    }

    /// Get cached DRM keys or fetch new ones via SPEKE.
    fn get_or_fetch_keys(
        &self,
        content_id: &str,
        key_ids: &[[u8; 16]],
    ) -> Result<DrmKeySet> {
        let cache_key = crate::cache::CacheKeys::drm_keys(content_id);

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
        let key = crate::cache::CacheKeys::job_state(content_id, &format_str(format));
        self.cache.set(&key, &json, self.config.cache.job_state_ttl)
    }
}

/// Parsed source manifest information.
struct SourceManifest {
    init_segment_url: String,
    segment_urls: Vec<String>,
    segment_durations: Vec<f64>,
    is_live: bool,
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

    ManifestDrmInfo {
        widevine_pssh,
        playready_pssh,
        playready_pro,
        default_kid: kid_hex,
    }
}

fn format_str(format: OutputFormat) -> String {
    match format {
        OutputFormat::Hls => "hls".to_string(),
        OutputFormat::Dash => "dash".to_string(),
    }
}

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
                .map(|k| {
                    let mut kid = [0u8; 16];
                    kid.copy_from_slice(&k.kid[..16.min(k.kid.len())]);
                    ContentKey {
                        kid,
                        key: k.key,
                        iv: k.iv,
                    }
                })
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
