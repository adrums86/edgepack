pub mod encrypted;
pub mod redis_http;
pub mod redis_tcp;

#[cfg(feature = "sandbox")]
pub mod memory;

use crate::config::{RedisBackendType, RedisConfig};
use crate::error::Result;

/// Abstract cache backend for application state storage.
///
/// Used for DRM keys, repackaging job state, and manifest progress tracking.
/// Media segments/manifests themselves are cached at the CDN level via HTTP headers.
pub trait CacheBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()>;
    fn exists(&self, key: &str) -> Result<bool>;
    fn delete(&self, key: &str) -> Result<()>;
}

/// Create a cache backend from configuration.
///
/// The returned backend automatically encrypts sensitive cache entries
/// (DRM keys, SPEKE responses, rewrite parameters) using AES-256-GCM
/// with a key derived from the Redis token.
pub fn create_backend(config: &RedisConfig) -> Result<Box<dyn CacheBackend>> {
    let inner: Box<dyn CacheBackend> = match config.backend {
        RedisBackendType::Http => Box::new(redis_http::RedisHttpBackend::new(
            &config.url,
            &config.token,
        )),
        RedisBackendType::Tcp => Box::new(redis_tcp::RedisTcpBackend::new(
            &config.url,
            &config.token,
        )?),
    };
    let enc_key = encrypted::derive_key(&config.token);
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

    #[test]
    fn cache_keys_scheme_fmt_different_from_unqualified() {
        // Scheme-qualified keys should differ from unqualified keys
        let unqualified = CacheKeys::init_segment("abc", "hls");
        let qualified = CacheKeys::init_segment_for_scheme("abc", "hls", "cenc");
        assert_ne!(unqualified, qualified);
    }

    #[test]
    fn create_backend_http() {
        let config = RedisConfig {
            url: "https://redis.example.com".into(),
            token: "token123".into(),
            backend: RedisBackendType::Http,
        };
        let backend = create_backend(&config);
        assert!(backend.is_ok());
    }

    #[test]
    fn create_backend_tcp() {
        let config = RedisConfig {
            url: "redis://localhost:6379".into(),
            token: "token123".into(),
            backend: RedisBackendType::Tcp,
        };
        let backend = create_backend(&config);
        // TCP backend constructor should succeed (it's a stub)
        assert!(backend.is_ok());
    }
}
