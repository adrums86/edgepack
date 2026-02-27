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
pub fn create_backend(config: &RedisConfig) -> Result<Box<dyn CacheBackend>> {
    match config.backend {
        RedisBackendType::Http => Ok(Box::new(redis_http::RedisHttpBackend::new(
            &config.url,
            &config.token,
        ))),
        RedisBackendType::Tcp => Ok(Box::new(redis_tcp::RedisTcpBackend::new(
            &config.url,
            &config.token,
        )?)),
    }
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
