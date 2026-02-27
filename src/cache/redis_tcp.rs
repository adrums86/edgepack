use crate::cache::CacheBackend;
use crate::error::{EdgePackagerError, Result};

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
        Err(EdgePackagerError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }

    fn set(&self, _key: &str, _value: &[u8], _ttl_seconds: u64) -> Result<()> {
        Err(EdgePackagerError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }

    fn exists(&self, _key: &str) -> Result<bool> {
        Err(EdgePackagerError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }

    fn delete(&self, _key: &str) -> Result<()> {
        Err(EdgePackagerError::Cache(
            "TCP Redis backend not yet implemented — use HTTP backend".into(),
        ))
    }
}
