//! In-memory cache backend for the local sandbox.
//!
//! Stores all data in a `HashMap` behind `Arc<RwLock<...>>` for thread-safe
//! shared access between the pipeline processing thread and the API server.
//! TTL values are accepted but ignored (sandbox lifetime is short enough
//! that expiration is irrelevant).

use crate::cache::CacheBackend;
use crate::error::{EdgepackError, Result};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// In-memory cache backend for sandbox use.
///
/// Clone is cheap — it shares the underlying `Arc`. This allows the same
/// cache instance to be shared between the pipeline thread (writer) and
/// the Axum API server (reader for status polling).
#[derive(Clone)]
pub struct InMemoryCacheBackend {
    store: Arc<RwLock<HashMap<String, Vec<u8>>>>,
}

impl InMemoryCacheBackend {
    pub fn new() -> Self {
        Self {
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }
}

impl CacheBackend for InMemoryCacheBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let store = self
            .store
            .read()
            .map_err(|e| EdgepackError::Cache(format!("lock poisoned: {e}")))?;
        Ok(store.get(key).cloned())
    }

    fn set(&self, key: &str, value: &[u8], _ttl_seconds: u64) -> Result<()> {
        let mut store = self
            .store
            .write()
            .map_err(|e| EdgepackError::Cache(format!("lock poisoned: {e}")))?;
        store.insert(key.to_string(), value.to_vec());
        Ok(())
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let store = self
            .store
            .read()
            .map_err(|e| EdgepackError::Cache(format!("lock poisoned: {e}")))?;
        Ok(store.contains_key(key))
    }

    fn delete(&self, key: &str) -> Result<()> {
        let mut store = self
            .store
            .write()
            .map_err(|e| EdgepackError::Cache(format!("lock poisoned: {e}")))?;
        store.remove(key);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_returns_none_for_missing_key() {
        let cache = InMemoryCacheBackend::new();
        assert!(cache.get("missing").unwrap().is_none());
    }

    #[test]
    fn set_and_get_roundtrip() {
        let cache = InMemoryCacheBackend::new();
        cache.set("key1", b"value1", 3600).unwrap();
        assert_eq!(cache.get("key1").unwrap(), Some(b"value1".to_vec()));
    }

    #[test]
    fn set_overwrites_existing() {
        let cache = InMemoryCacheBackend::new();
        cache.set("key", b"old", 60).unwrap();
        cache.set("key", b"new", 60).unwrap();
        assert_eq!(cache.get("key").unwrap(), Some(b"new".to_vec()));
    }

    #[test]
    fn exists_returns_false_for_missing() {
        let cache = InMemoryCacheBackend::new();
        assert!(!cache.exists("missing").unwrap());
    }

    #[test]
    fn exists_returns_true_after_set() {
        let cache = InMemoryCacheBackend::new();
        cache.set("key", b"val", 60).unwrap();
        assert!(cache.exists("key").unwrap());
    }

    #[test]
    fn delete_removes_key() {
        let cache = InMemoryCacheBackend::new();
        cache.set("key", b"val", 60).unwrap();
        cache.delete("key").unwrap();
        assert!(cache.get("key").unwrap().is_none());
    }

    #[test]
    fn delete_nonexistent_is_ok() {
        let cache = InMemoryCacheBackend::new();
        assert!(cache.delete("nonexistent").is_ok());
    }

    #[test]
    fn clone_shares_state() {
        let cache1 = InMemoryCacheBackend::new();
        let cache2 = cache1.clone();
        cache1.set("shared", b"data", 60).unwrap();
        assert_eq!(cache2.get("shared").unwrap(), Some(b"data".to_vec()));
    }

    #[test]
    fn ttl_is_ignored() {
        let cache = InMemoryCacheBackend::new();
        cache.set("key", b"val", 0).unwrap();
        assert_eq!(cache.get("key").unwrap(), Some(b"val".to_vec()));
    }

    #[test]
    fn stores_binary_data() {
        let cache = InMemoryCacheBackend::new();
        let binary = vec![0x00, 0xFF, 0x80, 0x01];
        cache.set("bin", &binary, 60).unwrap();
        assert_eq!(cache.get("bin").unwrap(), Some(binary));
    }
}
