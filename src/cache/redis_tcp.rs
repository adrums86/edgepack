use crate::cache::CacheBackend;
use crate::error::{EdgepackError, Result};

/// TCP-based Redis backend for runtimes that support socket connections.
///
/// This is a stub implementation. Most edge/WASM environments do not support
/// raw TCP sockets, so the HTTP backend (`redis_http`) is preferred.
/// This exists for future runtimes that add socket support (e.g., WASI sockets proposal).
pub struct RedisTcpBackend {
    _url: String,
    _token: String,
}

impl RedisTcpBackend {
    pub fn new(url: &str, token: &str) -> Result<Self> {
        // TCP sockets are not available in most WASM/edge environments.
        // This backend is provided for forward compatibility.
        Ok(Self {
            _url: url.to_string(),
            _token: token.to_string(),
        })
    }
}

impl CacheBackend for RedisTcpBackend {
    fn get(&self, _key: &str) -> Result<Option<Vec<u8>>> {
        Err(EdgepackError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }

    fn set(&self, _key: &str, _value: &[u8], _ttl_seconds: u64) -> Result<()> {
        Err(EdgepackError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }

    fn set_nx(&self, _key: &str, _value: &[u8], _ttl_seconds: u64) -> Result<bool> {
        Err(EdgepackError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }

    fn exists(&self, _key: &str) -> Result<bool> {
        Err(EdgepackError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }

    fn delete(&self, _key: &str) -> Result<()> {
        Err(EdgepackError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_succeeds() {
        let backend = RedisTcpBackend::new("redis://localhost:6379", "password");
        assert!(backend.is_ok());
    }

    #[test]
    fn all_operations_return_not_implemented() {
        let backend = RedisTcpBackend::new("redis://localhost:6379", "password").unwrap();

        assert!(backend.get("key").is_err());
        assert!(backend.set("key", b"value", 60).is_err());
        assert!(backend.set_nx("key", b"value", 30).is_err());
        assert!(backend.exists("key").is_err());
        assert!(backend.delete("key").is_err());
    }

    #[test]
    fn error_message_suggests_http_backend() {
        let backend = RedisTcpBackend::new("redis://localhost:6379", "password").unwrap();
        let err = backend.get("key").unwrap_err();
        assert!(err.to_string().contains("use HTTP backend"));
    }
}
