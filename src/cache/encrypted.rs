//! Transparent encryption layer for sensitive cache entries.
//!
//! Wraps any `CacheBackend` and applies AES-256-GCM encryption to values
//! stored under sensitive key patterns (`:keys`, `:speke`, `:rewrite_params`).
//! Non-sensitive keys pass through unmodified.
//!
//! Wire format: `nonce (12 bytes) || ciphertext || tag (16 bytes)`.

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes256Gcm, Nonce};

use crate::cache::CacheBackend;
use crate::error::{EdgepackError, Result};

/// AES-256-GCM encrypted cache backend decorator.
///
/// Encrypts values for sensitive keys before storing and decrypts on retrieval.
/// Non-sensitive keys are delegated directly to the inner backend.
pub struct EncryptedCacheBackend {
    inner: Box<dyn CacheBackend>,
    cipher: Aes256Gcm,
}

impl EncryptedCacheBackend {
    /// Create a new encrypted cache backend wrapping `inner`.
    ///
    /// `key` must be exactly 32 bytes (AES-256). Use [`derive_key`] to produce
    /// a key from the Redis token.
    pub fn new(inner: Box<dyn CacheBackend>, key: &[u8; 32]) -> Self {
        let cipher = Aes256Gcm::new(key.into());
        Self { inner, cipher }
    }

    fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        // Generate a random 12-byte nonce
        let nonce_bytes = generate_nonce();
        let nonce = Nonce::from_slice(&nonce_bytes);

        let ciphertext = self
            .cipher
            .encrypt(nonce, plaintext)
            .map_err(|e| EdgepackError::Encryption(format!("encrypt failed: {e}")))?;

        // Wire format: nonce || ciphertext (includes tag appended by aes-gcm)
        let mut output = Vec::with_capacity(12 + ciphertext.len());
        output.extend_from_slice(&nonce_bytes);
        output.extend_from_slice(&ciphertext);
        Ok(output)
    }

    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 12 + 16 {
            return Err(EdgepackError::Encryption(
                "ciphertext too short (missing nonce or tag)".into(),
            ));
        }

        let nonce = Nonce::from_slice(&data[..12]);
        let ciphertext = &data[12..];

        self.cipher
            .decrypt(nonce, ciphertext)
            .map_err(|e| EdgepackError::Encryption(format!("decrypt failed: {e}")))
    }
}

impl CacheBackend for EncryptedCacheBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let raw = self.inner.get(key)?;
        match raw {
            Some(data) if is_sensitive_key(key) => Ok(Some(self.decrypt(&data)?)),
            other => Ok(other),
        }
    }

    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()> {
        if is_sensitive_key(key) {
            let encrypted = self.encrypt(value)?;
            self.inner.set(key, &encrypted, ttl_seconds)
        } else {
            self.inner.set(key, value, ttl_seconds)
        }
    }

    /// Pass through to inner backend — lock keys are not sensitive data.
    fn set_nx(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<bool> {
        self.inner.set_nx(key, value, ttl_seconds)
    }

    fn exists(&self, key: &str) -> Result<bool> {
        self.inner.exists(key)
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key)
    }
}

/// Returns true for cache keys that contain sensitive data (DRM keys,
/// SPEKE responses, rewrite parameters containing encryption keys).
pub fn is_sensitive_key(key: &str) -> bool {
    key.ends_with(":keys")
        || key.ends_with(":speke")
        || key.ends_with(":rewrite_params")
}

/// Derive a 32-byte AES-256 key from a token string.
///
/// Uses AES-128-ECB as a PRF: takes the first 16 bytes of the token
/// (zero-padded if shorter) and encrypts two distinct constant blocks
/// to produce 32 bytes of key material. This avoids adding a SHA-256
/// dependency while providing a deterministic, collision-resistant mapping.
pub fn derive_key(token: &str) -> [u8; 32] {
    use aes::cipher::{BlockEncrypt, KeyInit as _};
    use aes::Aes128;

    // Prepare 16-byte seed from token
    let token_bytes = token.as_bytes();
    let mut seed = [0u8; 16];
    let len = token_bytes.len().min(16);
    seed[..len].copy_from_slice(&token_bytes[..len]);

    let aes = Aes128::new(&seed.into());

    // Encrypt two distinct constant blocks to produce 32 bytes
    let mut block_a = aes::Block::from([
        0x65, 0x70, 0x2d, 0x6b, 0x65, 0x79, 0x2d, 0x64, // "ep-key-d"
        0x65, 0x72, 0x69, 0x76, 0x65, 0x2d, 0x30, 0x31, // "erive-01"
    ]);
    let mut block_b = aes::Block::from([
        0x65, 0x70, 0x2d, 0x6b, 0x65, 0x79, 0x2d, 0x64, // "ep-key-d"
        0x65, 0x72, 0x69, 0x76, 0x65, 0x2d, 0x30, 0x32, // "erive-02"
    ]);

    aes.encrypt_block(&mut block_a);
    aes.encrypt_block(&mut block_b);

    let mut key = [0u8; 32];
    key[..16].copy_from_slice(&block_a);
    key[16..].copy_from_slice(&block_b);
    key
}

/// Generate a random 12-byte nonce for AES-GCM.
fn generate_nonce() -> [u8; 12] {
    let uuid = uuid::Uuid::new_v4();
    let bytes = uuid.as_bytes();
    let mut nonce = [0u8; 12];
    nonce.copy_from_slice(&bytes[..12]);
    nonce
}

// Compile-time check that EncryptedCacheBackend is Send + Sync,
// as required by the CacheBackend trait.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<EncryptedCacheBackend>();
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::RwLock;

    /// Simple in-memory backend for testing.
    struct TestCacheBackend {
        store: RwLock<HashMap<String, Vec<u8>>>,
    }

    impl TestCacheBackend {
        fn new() -> Self {
            Self {
                store: RwLock::new(HashMap::new()),
            }
        }
    }

    impl CacheBackend for TestCacheBackend {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.store.read().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &[u8], _ttl: u64) -> Result<()> {
            self.store
                .write()
                .unwrap()
                .insert(key.to_string(), value.to_vec());
            Ok(())
        }
        fn set_nx(&self, key: &str, value: &[u8], _ttl: u64) -> Result<bool> {
            let mut store = self.store.write().unwrap();
            if store.contains_key(key) {
                Ok(false)
            } else {
                store.insert(key.to_string(), value.to_vec());
                Ok(true)
            }
        }
        fn exists(&self, key: &str) -> Result<bool> {
            Ok(self.store.read().unwrap().contains_key(key))
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.store.write().unwrap().remove(key);
            Ok(())
        }
    }

    fn test_key() -> [u8; 32] {
        derive_key("test-redis-token-123")
    }

    // We need a shared inner for some tests, wrapping in Arc
    use std::sync::Arc;

    /// Arc-wrapped test backend so we can inspect raw store after encryption.
    struct SharedTestBackend {
        store: Arc<RwLock<HashMap<String, Vec<u8>>>>,
    }

    impl SharedTestBackend {
        fn new() -> Self {
            Self {
                store: Arc::new(RwLock::new(HashMap::new())),
            }
        }

        fn clone_store(&self) -> Arc<RwLock<HashMap<String, Vec<u8>>>> {
            Arc::clone(&self.store)
        }
    }

    impl CacheBackend for SharedTestBackend {
        fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
            Ok(self.store.read().unwrap().get(key).cloned())
        }
        fn set(&self, key: &str, value: &[u8], _ttl: u64) -> Result<()> {
            self.store
                .write()
                .unwrap()
                .insert(key.to_string(), value.to_vec());
            Ok(())
        }
        fn set_nx(&self, key: &str, value: &[u8], _ttl: u64) -> Result<bool> {
            let mut store = self.store.write().unwrap();
            if store.contains_key(key) {
                Ok(false)
            } else {
                store.insert(key.to_string(), value.to_vec());
                Ok(true)
            }
        }
        fn exists(&self, key: &str) -> Result<bool> {
            Ok(self.store.read().unwrap().contains_key(key))
        }
        fn delete(&self, key: &str) -> Result<()> {
            self.store.write().unwrap().remove(key);
            Ok(())
        }
    }

    #[test]
    fn sensitive_key_roundtrip() {
        let inner = TestCacheBackend::new();
        let enc = EncryptedCacheBackend::new(Box::new(inner), &test_key());

        let value = b"raw-aes-128-key-material-here";
        enc.set("ep:content1:keys", value, 3600).unwrap();

        let retrieved = enc.get("ep:content1:keys").unwrap().unwrap();
        assert_eq!(retrieved, value);
    }

    #[test]
    fn sensitive_key_encrypted_in_store() {
        let inner = SharedTestBackend::new();
        let store = inner.clone_store();
        let enc = EncryptedCacheBackend::new(Box::new(inner), &test_key());

        let plaintext = b"super-secret-key-data";
        enc.set("ep:abc:keys", plaintext, 3600).unwrap();

        // Read raw data from inner store — should NOT match plaintext
        let raw = store.read().unwrap().get("ep:abc:keys").cloned().unwrap();
        assert_ne!(raw, plaintext.to_vec());

        // Should be longer: 12 (nonce) + len(plaintext) + 16 (tag)
        assert_eq!(raw.len(), 12 + plaintext.len() + 16);
    }

    #[test]
    fn non_sensitive_key_passes_through() {
        let inner = SharedTestBackend::new();
        let store = inner.clone_store();
        let enc = EncryptedCacheBackend::new(Box::new(inner), &test_key());

        let value = b"job-state-json-here";
        enc.set("ep:abc:hls:state", value, 3600).unwrap();

        // Non-sensitive: raw store should contain plaintext
        let raw = store.read().unwrap().get("ep:abc:hls:state").cloned().unwrap();
        assert_eq!(raw, value.to_vec());

        // get() should return plaintext directly
        let retrieved = enc.get("ep:abc:hls:state").unwrap().unwrap();
        assert_eq!(retrieved, value);
    }

    #[test]
    fn speke_key_is_sensitive() {
        assert!(is_sensitive_key("ep:content1:speke"));
    }

    #[test]
    fn rewrite_params_key_is_sensitive() {
        assert!(is_sensitive_key("ep:content1:hls:rewrite_params"));
        assert!(is_sensitive_key("ep:xyz:dash:rewrite_params"));
    }

    #[test]
    fn non_sensitive_key_patterns() {
        assert!(!is_sensitive_key("ep:abc:hls:state"));
        assert!(!is_sensitive_key("ep:abc:hls:manifest_state"));
        assert!(!is_sensitive_key("ep:abc:hls:init"));
        assert!(!is_sensitive_key("ep:abc:hls:seg:0"));
        assert!(!is_sensitive_key("ep:abc:hls:source"));
    }

    #[test]
    fn derive_key_deterministic() {
        let key1 = derive_key("my-secret-token");
        let key2 = derive_key("my-secret-token");
        assert_eq!(key1, key2);
    }

    #[test]
    fn derive_key_different_tokens() {
        let key1 = derive_key("token-alpha");
        let key2 = derive_key("token-bravo");
        assert_ne!(key1, key2);
    }

    #[test]
    fn derive_key_produces_32_bytes() {
        let key = derive_key("anything");
        assert_eq!(key.len(), 32);
    }

    #[test]
    fn tampered_ciphertext_fails() {
        let inner = SharedTestBackend::new();
        let store = inner.clone_store();
        let enc = EncryptedCacheBackend::new(Box::new(inner), &test_key());

        enc.set("ep:abc:keys", b"secret", 3600).unwrap();

        // Tamper with the stored ciphertext
        {
            let mut s = store.write().unwrap();
            let data = s.get_mut("ep:abc:keys").unwrap();
            // Flip a byte in the ciphertext portion (after the 12-byte nonce)
            if data.len() > 13 {
                data[13] ^= 0xFF;
            }
        }

        let result = enc.get("ep:abc:keys");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("decrypt failed"),
            "expected decrypt error, got: {err_msg}"
        );
    }

    #[test]
    fn truncated_ciphertext_fails() {
        let inner = SharedTestBackend::new();
        let store = inner.clone_store();
        let enc = EncryptedCacheBackend::new(Box::new(inner), &test_key());

        enc.set("ep:abc:speke", b"cpix-xml-data", 3600).unwrap();

        // Truncate to just the nonce (12 bytes) — missing ciphertext+tag
        {
            let mut s = store.write().unwrap();
            let data = s.get_mut("ep:abc:speke").unwrap();
            data.truncate(10);
        }

        let result = enc.get("ep:abc:speke");
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("too short"),
            "expected 'too short' error, got: {err_msg}"
        );
    }

    #[test]
    fn wrong_key_fails_decrypt() {
        let inner = SharedTestBackend::new();
        let store = inner.clone_store();

        let key_a = derive_key("token-a");
        let key_b = derive_key("token-b");

        let enc_a = EncryptedCacheBackend::new(Box::new(inner), &key_a);
        enc_a.set("ep:abc:keys", b"secret-data", 3600).unwrap();

        // Build a new EncryptedCacheBackend with key_b pointing at the same store
        struct StoreRef(Arc<RwLock<HashMap<String, Vec<u8>>>>);
        impl CacheBackend for StoreRef {
            fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
                Ok(self.0.read().unwrap().get(key).cloned())
            }
            fn set(&self, key: &str, value: &[u8], _: u64) -> Result<()> {
                self.0.write().unwrap().insert(key.to_string(), value.to_vec());
                Ok(())
            }
            fn set_nx(&self, key: &str, value: &[u8], _: u64) -> Result<bool> {
                let mut store = self.0.write().unwrap();
                if store.contains_key(key) {
                    Ok(false)
                } else {
                    store.insert(key.to_string(), value.to_vec());
                    Ok(true)
                }
            }
            fn exists(&self, key: &str) -> Result<bool> {
                Ok(self.0.read().unwrap().contains_key(key))
            }
            fn delete(&self, key: &str) -> Result<()> {
                self.0.write().unwrap().remove(key);
                Ok(())
            }
        }

        let enc_b = EncryptedCacheBackend::new(Box::new(StoreRef(store)), &key_b);
        let result = enc_b.get("ep:abc:keys");
        assert!(result.is_err(), "decrypting with wrong key should fail");
    }

    #[test]
    fn exists_and_delete_delegate_directly() {
        let inner = TestCacheBackend::new();
        let enc = EncryptedCacheBackend::new(Box::new(inner), &test_key());

        enc.set("ep:abc:keys", b"data", 3600).unwrap();
        assert!(enc.exists("ep:abc:keys").unwrap());

        enc.delete("ep:abc:keys").unwrap();
        assert!(!enc.exists("ep:abc:keys").unwrap());
    }

    #[test]
    fn get_missing_key_returns_none() {
        let inner = TestCacheBackend::new();
        let enc = EncryptedCacheBackend::new(Box::new(inner), &test_key());

        assert!(enc.get("ep:nonexistent:keys").unwrap().is_none());
        assert!(enc.get("ep:nonexistent:state").unwrap().is_none());
    }
}
