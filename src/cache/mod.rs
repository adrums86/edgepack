pub mod redis_http;
pub mod redis_tcp;

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
}
