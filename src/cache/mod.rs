pub mod encrypted;
pub mod http_kv;
pub mod redis_http;
pub mod redis_tcp;

#[cfg(feature = "cloudflare")]
pub mod cloudflare_kv;

#[cfg(feature = "sandbox")]
pub mod memory;

use crate::config::{AppConfig, CacheBackendType};
use crate::error::{EdgepackError, Result};

/// Abstract cache backend for application state storage.
///
/// Used for DRM keys, repackaging job state, and manifest progress tracking.
/// Media segments/manifests themselves are cached at the CDN level via HTTP headers.
pub trait CacheBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()>;
    /// Atomic set-if-not-exists. Returns `true` if the key was set (did not exist),
    /// `false` if the key already existed. Used for distributed locking / request coalescing.
    fn set_nx(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<bool>;
    fn exists(&self, key: &str) -> Result<bool>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// Create a cache backend from application configuration.
///
/// The returned backend automatically encrypts sensitive cache entries
/// (DRM keys, SPEKE responses, rewrite parameters) using AES-256-GCM
/// with a key derived from the store token.
///
/// Dispatches to the appropriate backend based on `config.store.backend`:
/// - `RedisHttp` → Upstash-compatible HTTP Redis
/// - `RedisTcp` → TCP Redis (stub)
/// - `CloudflareKv` → Cloudflare Workers KV REST API (requires `cloudflare` feature)
/// - `HttpKv` → Generic HTTP KV (for DynamoDB via API GW, EdgeKV via proxy, etc.)
pub fn create_backend(config: &AppConfig) -> Result<Box<dyn CacheBackend>> {
    let inner: Box<dyn CacheBackend> = match config.store.backend {
        CacheBackendType::RedisHttp => Box::new(redis_http::RedisHttpBackend::new(
            &config.store.url,
            &config.store.token,
        )),
        CacheBackendType::RedisTcp => Box::new(redis_tcp::RedisTcpBackend::new(
            &config.store.url,
            &config.store.token,
        )?),
        #[cfg(feature = "cloudflare")]
        CacheBackendType::CloudflareKv => {
            let cf = config.cloudflare_kv.as_ref()
                .ok_or_else(|| EdgepackError::Config("cloudflare_kv config required for CloudflareKv backend".into()))?;
            Box::new(cloudflare_kv::CloudflareKvBackend::new(cf))
        }
        CacheBackendType::HttpKv => {
            let hkv = config.http_kv.as_ref()
                .ok_or_else(|| EdgepackError::Config("http_kv config required for HttpKv backend".into()))?;
            Box::new(http_kv::HttpKvBackend::new(hkv))
        }
    };
    let enc_key = encrypted::derive_key(config.cache_encryption_token());
    Ok(Box::new(encrypted::EncryptedCacheBackend::new(inner, &enc_key)))
}

/// Cache key builders for consistent key naming.
pub struct CacheKeys;

impl CacheKeys {
    /// DRM content keys for a given content ID.
    pub fn drm_keys(content_id: &str) -> String {
        format!("ep:{content_id}:keys")
    }

    /// Repackaging job state (progress, status).
    pub fn job_state(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:state")
    }

    /// Progressive manifest state (segment list, live/complete).
    pub fn manifest_state(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:manifest_state")
    }

    /// SPEKE response cache to avoid duplicate license server calls.
    pub fn speke_response(content_id: &str) -> String {
        format!("ep:{content_id}:speke")
    }

    /// Rewritten init segment binary data.
    pub fn init_segment(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:init")
    }

    /// Rewritten media segment binary data.
    pub fn media_segment(content_id: &str, format: &str, number: u32) -> String {
        format!("ep:{content_id}:{format}:seg:{number}")
    }

    /// Serialized source manifest metadata (for continuation chaining).
    pub fn source_manifest(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:source")
    }

    /// Serialized segment rewrite parameters (for continuation chaining).
    pub fn rewrite_params(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:rewrite_params")
    }

    /// Target schemes list for continuation chaining (stored during execute_first).
    pub fn target_schemes(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:target_schemes")
    }

    // --- Format-agnostic key builders (Phase 21: Generic HLS/DASH Pipeline) ---
    // Segments are identical for HLS and DASH — only scheme affects bytes.

    /// Rewritten init segment for a specific scheme (format-agnostic, shared across HLS/DASH).
    pub fn init_segment_for_scheme_only(content_id: &str, scheme: &str) -> String {
        format!("ep:{content_id}:{scheme}:init")
    }

    /// Rewritten media segment for a specific scheme (format-agnostic, shared across HLS/DASH).
    pub fn media_segment_for_scheme_only(content_id: &str, scheme: &str, number: u32) -> String {
        format!("ep:{content_id}:{scheme}:seg:{number}")
    }

    /// Target output formats list for continuation chaining (stored during execute_first).
    pub fn target_formats(content_id: &str) -> String {
        format!("ep:{content_id}:target_formats")
    }

    // --- Scheme-qualified key builders (Phase 4: Dual-Scheme Output) ---

    /// Build a scheme-qualified format string: e.g. "hls_cenc", "dash_cbcs".
    fn scheme_fmt(format: &str, scheme: &str) -> String {
        format!("{format}_{scheme}")
    }

    /// Progressive manifest state for a specific scheme.
    pub fn manifest_state_for_scheme(content_id: &str, format: &str, scheme: &str) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:manifest_state")
    }

    /// Rewritten init segment for a specific scheme.
    pub fn init_segment_for_scheme(content_id: &str, format: &str, scheme: &str) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:init")
    }

    /// Rewritten media segment for a specific scheme.
    pub fn media_segment_for_scheme(content_id: &str, format: &str, scheme: &str, number: u32) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:seg:{number}")
    }

    /// Rewrite parameters for a specific scheme (continuation chaining).
    pub fn rewrite_params_for_scheme(content_id: &str, format: &str, scheme: &str) -> String {
        let sf = Self::scheme_fmt(format, scheme);
        format!("ep:{content_id}:{sf}:rewrite_params")
    }

    // --- JIT Packaging key builders (Phase 8) ---

    /// Per-content source configuration (source URL, target schemes, container format).
    pub fn source_config(content_id: &str) -> String {
        format!("ep:{content_id}:source_config")
    }

    /// Distributed processing lock for a specific resource.
    /// Prevents duplicate JIT work when multiple requests arrive simultaneously.
    pub fn processing_lock(content_id: &str, format: &str, resource: &str) -> String {
        format!("ep:{content_id}:{format}:lock:{resource}")
    }

    /// Marker that JIT setup (manifest + init + keys) is complete for this content/format.
    pub fn jit_setup(content_id: &str, format: &str) -> String {
        format!("ep:{content_id}:{format}:jit_setup")
    }

    /// Part data (LL-HLS chunk).
    pub fn part(content_id: &str, format: &str, segment_number: u32, part_index: u32) -> String {
        format!("ep:{content_id}:{format}:part:{segment_number}:{part_index}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_keys_drm_keys() {
        assert_eq!(CacheKeys::drm_keys("abc123"), "ep:abc123:keys");
    }

    #[test]
    fn cache_keys_drm_keys_special_chars() {
        assert_eq!(CacheKeys::drm_keys("my-content_v2"), "ep:my-content_v2:keys");
    }

    #[test]
    fn cache_keys_job_state() {
        assert_eq!(CacheKeys::job_state("abc", "hls"), "ep:abc:hls:state");
        assert_eq!(CacheKeys::job_state("abc", "dash"), "ep:abc:dash:state");
    }

    #[test]
    fn cache_keys_manifest_state() {
        assert_eq!(
            CacheKeys::manifest_state("abc", "hls"),
            "ep:abc:hls:manifest_state"
        );
    }

    #[test]
    fn cache_keys_speke_response() {
        assert_eq!(CacheKeys::speke_response("abc"), "ep:abc:speke");
    }

    #[test]
    fn cache_keys_init_segment() {
        assert_eq!(CacheKeys::init_segment("abc", "hls"), "ep:abc:hls:init");
        assert_eq!(CacheKeys::init_segment("abc", "dash"), "ep:abc:dash:init");
    }

    #[test]
    fn cache_keys_media_segment() {
        assert_eq!(CacheKeys::media_segment("abc", "hls", 0), "ep:abc:hls:seg:0");
        assert_eq!(CacheKeys::media_segment("abc", "dash", 42), "ep:abc:dash:seg:42");
    }

    #[test]
    fn cache_keys_source_manifest() {
        assert_eq!(CacheKeys::source_manifest("abc", "hls"), "ep:abc:hls:source");
    }

    #[test]
    fn cache_keys_rewrite_params() {
        assert_eq!(CacheKeys::rewrite_params("abc", "dash"), "ep:abc:dash:rewrite_params");
    }

    #[test]
    fn cache_keys_target_schemes() {
        assert_eq!(CacheKeys::target_schemes("abc", "hls"), "ep:abc:hls:target_schemes");
        assert_eq!(CacheKeys::target_schemes("abc", "dash"), "ep:abc:dash:target_schemes");
    }

    // --- Format-agnostic key tests (Phase 21) ---

    #[test]
    fn cache_keys_init_segment_for_scheme_only() {
        assert_eq!(
            CacheKeys::init_segment_for_scheme_only("abc", "cenc"),
            "ep:abc:cenc:init"
        );
        assert_eq!(
            CacheKeys::init_segment_for_scheme_only("abc", "cbcs"),
            "ep:abc:cbcs:init"
        );
    }

    #[test]
    fn cache_keys_media_segment_for_scheme_only() {
        assert_eq!(
            CacheKeys::media_segment_for_scheme_only("abc", "cenc", 0),
            "ep:abc:cenc:seg:0"
        );
        assert_eq!(
            CacheKeys::media_segment_for_scheme_only("abc", "cbcs", 42),
            "ep:abc:cbcs:seg:42"
        );
    }

    #[test]
    fn cache_keys_target_formats() {
        assert_eq!(CacheKeys::target_formats("abc"), "ep:abc:target_formats");
    }

    #[test]
    fn cache_keys_format_agnostic_differs_from_format_qualified() {
        // Format-agnostic keys should differ from format-qualified keys
        let agnostic = CacheKeys::init_segment_for_scheme_only("abc", "cenc");
        let qualified = CacheKeys::init_segment_for_scheme("abc", "hls", "cenc");
        assert_ne!(agnostic, qualified);
        assert_eq!(agnostic, "ep:abc:cenc:init");
        assert_eq!(qualified, "ep:abc:hls_cenc:init");
    }

    // --- Scheme-qualified key tests ---

    #[test]
    fn cache_keys_manifest_state_for_scheme() {
        assert_eq!(
            CacheKeys::manifest_state_for_scheme("abc", "hls", "cenc"),
            "ep:abc:hls_cenc:manifest_state"
        );
        assert_eq!(
            CacheKeys::manifest_state_for_scheme("abc", "dash", "cbcs"),
            "ep:abc:dash_cbcs:manifest_state"
        );
    }

    #[test]
    fn cache_keys_init_segment_for_scheme() {
        assert_eq!(
            CacheKeys::init_segment_for_scheme("abc", "hls", "cenc"),
            "ep:abc:hls_cenc:init"
        );
        assert_eq!(
            CacheKeys::init_segment_for_scheme("abc", "dash", "cbcs"),
            "ep:abc:dash_cbcs:init"
        );
    }

    #[test]
    fn cache_keys_media_segment_for_scheme() {
        assert_eq!(
            CacheKeys::media_segment_for_scheme("abc", "hls", "cenc", 0),
            "ep:abc:hls_cenc:seg:0"
        );
        assert_eq!(
            CacheKeys::media_segment_for_scheme("abc", "dash", "cbcs", 42),
            "ep:abc:dash_cbcs:seg:42"
        );
    }

    #[test]
    fn cache_keys_rewrite_params_for_scheme() {
        assert_eq!(
            CacheKeys::rewrite_params_for_scheme("abc", "hls", "cenc"),
            "ep:abc:hls_cenc:rewrite_params"
        );
        assert_eq!(
            CacheKeys::rewrite_params_for_scheme("abc", "dash", "cbcs"),
            "ep:abc:dash_cbcs:rewrite_params"
        );
    }

    // --- JIT key tests ---

    #[test]
    fn cache_keys_source_config() {
        assert_eq!(CacheKeys::source_config("abc"), "ep:abc:source_config");
        assert_eq!(CacheKeys::source_config("my-video"), "ep:my-video:source_config");
    }

    #[test]
    fn cache_keys_processing_lock() {
        assert_eq!(
            CacheKeys::processing_lock("abc", "hls", "setup"),
            "ep:abc:hls:lock:setup"
        );
        assert_eq!(
            CacheKeys::processing_lock("abc", "dash", "seg:5"),
            "ep:abc:dash:lock:seg:5"
        );
    }

    #[test]
    fn cache_keys_jit_setup() {
        assert_eq!(CacheKeys::jit_setup("abc", "hls"), "ep:abc:hls:jit_setup");
        assert_eq!(CacheKeys::jit_setup("abc", "dash"), "ep:abc:dash:jit_setup");
    }

    #[test]
    fn cache_keys_part() {
        assert_eq!(
            CacheKeys::part("abc", "hls_cenc", 3, 2),
            "ep:abc:hls_cenc:part:3:2"
        );
        assert_eq!(
            CacheKeys::part("abc", "dash_cbcs", 0, 0),
            "ep:abc:dash_cbcs:part:0:0"
        );
    }

    #[test]
    fn cache_keys_scheme_fmt_different_from_unqualified() {
        // Scheme-qualified keys should differ from unqualified keys
        let unqualified = CacheKeys::init_segment("abc", "hls");
        let qualified = CacheKeys::init_segment_for_scheme("abc", "hls", "cenc");
        assert_ne!(unqualified, qualified);
    }

    #[test]
    fn create_backend_http() {
        use crate::config::*;
        let config = AppConfig {
            store: StoreConfig {
                url: "https://redis.example.com".into(),
                token: "token123".into(),
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
        };
        let backend = create_backend(&config);
        assert!(backend.is_ok());
    }

    #[test]
    fn create_backend_tcp() {
        use crate::config::*;
        let config = AppConfig {
            store: StoreConfig {
                url: "redis://localhost:6379".into(),
                token: "token123".into(),
                backend: CacheBackendType::RedisTcp,
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
        };
        let backend = create_backend(&config);
        // TCP backend constructor should succeed (it's a stub)
        assert!(backend.is_ok());
    }
}
