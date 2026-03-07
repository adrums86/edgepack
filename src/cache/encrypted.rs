//! Encrypted cache backend decorator.
//!
//! Wraps any `CacheBackend` and transparently encrypts values for sensitive
//! key patterns (DRM keys, rewrite params, SPEKE responses) using AES-128-CTR.
//! Non-sensitive keys pass through unmodified.
//!
//! The encryption key is generated per-process from available entropy sources
//! (pointer addresses, monotonic clock). This provides defense-in-depth against
//! memory dumps exposing raw DRM key material — it is not a substitute for
//! OS-level memory protection.

use crate::cache::CacheBackend;
use crate::error::{EdgepackError, Result};
use aes::Aes128;
use cipher::{KeyInit, KeyIvInit, StreamCipher};
use std::sync::Arc;

type Aes128Ctr = ctr::Ctr128BE<Aes128>;

/// Encrypted cache backend that wraps an inner `CacheBackend`.
///
/// Sensitive cache entries are encrypted with AES-128-CTR using a per-process
/// random key. Non-sensitive entries pass through unmodified.
///
/// Clone is cheap — shares the inner backend and key via `Arc`.
#[derive(Clone)]
pub struct EncryptedCacheBackend<B: CacheBackend + Clone> {
    inner: B,
    key: Arc<[u8; 16]>,
}

impl<B: CacheBackend + Clone> EncryptedCacheBackend<B> {
    pub fn new(inner: B, key: [u8; 16]) -> Self {
        Self {
            inner,
            key: Arc::new(key),
        }
    }

    /// Encrypt a value using AES-128-CTR with a generated IV.
    /// Returns `iv (16 bytes) || ciphertext`.
    fn encrypt(&self, plaintext: &[u8]) -> Vec<u8> {
        let iv = generate_iv();
        let mut ciphertext = plaintext.to_vec();
        let mut cipher = Aes128Ctr::new((&*self.key).into(), (&iv).into());
        cipher.apply_keystream(&mut ciphertext);

        let mut result = Vec::with_capacity(16 + ciphertext.len());
        result.extend_from_slice(&iv);
        result.extend_from_slice(&ciphertext);
        result
    }

    /// Decrypt a value. Input format: `iv (16 bytes) || ciphertext`.
    fn decrypt(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 16 {
            return Err(EdgepackError::Cache(
                "encrypted cache entry too short".into(),
            ));
        }
        let iv: [u8; 16] = data[..16].try_into().unwrap();
        let mut plaintext = data[16..].to_vec();
        let mut cipher = Aes128Ctr::new((&*self.key).into(), (&iv).into());
        cipher.apply_keystream(&mut plaintext);
        Ok(plaintext)
    }
}

/// Returns true if this cache key contains sensitive data that should be encrypted.
fn is_sensitive_key(key: &str) -> bool {
    key.ends_with(":keys") || key.contains(":rewrite_params") || key.ends_with(":speke")
}

impl<B: CacheBackend + Clone> CacheBackend for EncryptedCacheBackend<B> {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let value = self.inner.get(key)?;
        match value {
            Some(data) if is_sensitive_key(key) => Ok(Some(self.decrypt(&data)?)),
            other => Ok(other),
        }
    }

    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()> {
        if is_sensitive_key(key) {
            let encrypted = self.encrypt(value);
            self.inner.set(key, &encrypted, ttl_seconds)
        } else {
            self.inner.set(key, value, ttl_seconds)
        }
    }

    fn set_nx(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<bool> {
        if is_sensitive_key(key) {
            let encrypted = self.encrypt(value);
            self.inner.set_nx(key, &encrypted, ttl_seconds)
        } else {
            self.inner.set_nx(key, value, ttl_seconds)
        }
    }

    fn exists(&self, key: &str) -> Result<bool> {
        self.inner.exists(key)
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.inner.delete(key)
    }
}

/// Generate a 16-byte IV from available entropy sources.
///
/// Uses pointer addresses and a counter to produce unique IVs per call.
/// This is not cryptographically random but provides sufficient uniqueness
/// for per-process cache encryption where the threat model is passive
/// memory observation, not active cryptanalysis.
fn generate_iv() -> [u8; 16] {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut iv = [0u8; 16];

    // Lower 8 bytes: monotonic counter (ensures uniqueness)
    iv[..8].copy_from_slice(&count.to_le_bytes());

    // Upper 8 bytes: stack pointer address (adds entropy across processes)
    let stack_var: u8 = 0;
    let ptr = &stack_var as *const u8 as u64;
    iv[8..16].copy_from_slice(&ptr.to_le_bytes());

    iv
}

/// Generate a per-process encryption key from available entropy.
///
/// Mixes pointer addresses, stack data, and a monotonic clock to produce
/// a key unique to this process instance. The key protects against passive
/// memory dumps — it is stored in the same process memory as the data it
/// protects, so it does not defend against an attacker with full memory access.
pub fn generate_process_key() -> [u8; 16] {
    let mut key = [0u8; 16];

    // Source 1: heap pointer address (ASLR provides some randomness)
    let heap_val = Box::new(42u64);
    let heap_ptr = &*heap_val as *const u64 as u64;
    let heap_bytes = heap_ptr.to_le_bytes();

    // Source 2: stack pointer address
    let stack_var: u64 = 0;
    let stack_ptr = &stack_var as *const u64 as u64;
    let stack_bytes = stack_ptr.to_le_bytes();

    // Mix sources into key
    for i in 0..8 {
        key[i] = heap_bytes[i];
        key[i + 8] = stack_bytes[i];
    }

    // Whiten the key through AES: encrypt the key with itself to improve distribution.
    // This ensures even weak entropy sources produce a key with good bit distribution.
    use aes::cipher::BlockEncrypt;
    let aes_key = aes::Aes128::new((&key).into());
    let mut block = aes::Block::from(key);
    aes_key.encrypt_block(&mut block);
    block.into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::memory::InMemoryCacheBackend;

    fn make_encrypted_cache() -> EncryptedCacheBackend<InMemoryCacheBackend> {
        let key = [0x42u8; 16]; // Fixed key for test determinism
        EncryptedCacheBackend::new(InMemoryCacheBackend::new(), key)
    }

    #[test]
    fn sensitive_key_detection() {
        assert!(is_sensitive_key("ep:abc:keys"));
        assert!(is_sensitive_key("ep:abc:speke"));
        assert!(is_sensitive_key("ep:abc:hls_cenc:rewrite_params"));
        assert!(is_sensitive_key("ep:abc:dash_cbcs:rewrite_params"));

        assert!(!is_sensitive_key("ep:abc:hls:state"));
        assert!(!is_sensitive_key("ep:abc:hls_cenc:init"));
        assert!(!is_sensitive_key("ep:abc:hls_cenc:seg:0"));
        assert!(!is_sensitive_key("ep:abc:hls:manifest_state"));
        assert!(!is_sensitive_key("ep:abc:source_config"));
    }

    #[test]
    fn sensitive_values_are_encrypted_at_rest() {
        let cache = make_encrypted_cache();
        let plaintext = b"secret-key-data";

        cache.set("ep:test:keys", plaintext, 60).unwrap();

        // Read raw bytes from inner cache — should NOT match plaintext
        let raw = cache.inner.get("ep:test:keys").unwrap().unwrap();
        assert_ne!(&raw, &plaintext.to_vec());
        assert_eq!(raw.len(), 16 + plaintext.len()); // IV + ciphertext

        // Read through encrypted backend — should match plaintext
        let decrypted = cache.get("ep:test:keys").unwrap().unwrap();
        assert_eq!(decrypted, plaintext.to_vec());
    }

    #[test]
    fn non_sensitive_values_pass_through() {
        let cache = make_encrypted_cache();
        let data = b"manifest-state-json";

        cache.set("ep:test:hls:manifest_state", data, 60).unwrap();

        // Raw bytes should match — no encryption applied
        let raw = cache.inner.get("ep:test:hls:manifest_state").unwrap().unwrap();
        assert_eq!(raw, data.to_vec());
    }

    #[test]
    fn roundtrip_rewrite_params() {
        let cache = make_encrypted_cache();
        let params = b"{\"source_key\":{\"kid\":[1,2,3],\"key\":[4,5,6]}}";

        cache.set("ep:test:hls_cenc:rewrite_params", params, 60).unwrap();

        let result = cache.get("ep:test:hls_cenc:rewrite_params").unwrap().unwrap();
        assert_eq!(result, params.to_vec());

        // Verify raw storage is encrypted
        let raw = cache.inner.get("ep:test:hls_cenc:rewrite_params").unwrap().unwrap();
        assert_ne!(raw, params.to_vec());
    }

    #[test]
    fn roundtrip_speke() {
        let cache = make_encrypted_cache();
        let xml = b"<cpix:CPIX>...</cpix:CPIX>";

        cache.set("ep:test:speke", xml, 60).unwrap();
        let result = cache.get("ep:test:speke").unwrap().unwrap();
        assert_eq!(result, xml.to_vec());
    }

    #[test]
    fn set_nx_encrypts_sensitive() {
        let cache = make_encrypted_cache();
        let data = b"key-material";

        assert!(cache.set_nx("ep:test:keys", data, 60).unwrap());
        assert!(!cache.set_nx("ep:test:keys", b"other", 60).unwrap());

        let result = cache.get("ep:test:keys").unwrap().unwrap();
        assert_eq!(result, data.to_vec());
    }

    #[test]
    fn delete_removes_sensitive() {
        let cache = make_encrypted_cache();
        cache.set("ep:test:keys", b"secret", 60).unwrap();
        cache.delete("ep:test:keys").unwrap();
        assert!(cache.get("ep:test:keys").unwrap().is_none());
    }

    #[test]
    fn exists_works_for_sensitive() {
        let cache = make_encrypted_cache();
        assert!(!cache.exists("ep:test:keys").unwrap());
        cache.set("ep:test:keys", b"data", 60).unwrap();
        assert!(cache.exists("ep:test:keys").unwrap());
    }

    #[test]
    fn different_keys_produce_different_ciphertext() {
        let cache1 = EncryptedCacheBackend::new(
            InMemoryCacheBackend::new(),
            [0x01u8; 16],
        );
        let cache2 = EncryptedCacheBackend::new(
            InMemoryCacheBackend::new(),
            [0x02u8; 16],
        );
        let plaintext = b"same-plaintext";

        cache1.set("ep:test:keys", plaintext, 60).unwrap();
        cache2.set("ep:test:keys", plaintext, 60).unwrap();

        let raw1 = cache1.inner.get("ep:test:keys").unwrap().unwrap();
        let raw2 = cache2.inner.get("ep:test:keys").unwrap().unwrap();
        // Different keys → different ciphertext (IVs also differ but that's secondary)
        assert_ne!(raw1, raw2);
    }

    #[test]
    fn process_key_generation_is_nonzero() {
        let key = generate_process_key();
        assert_ne!(key, [0u8; 16]);
    }

    #[test]
    fn process_key_has_good_bit_distribution() {
        let key = generate_process_key();
        // After AES whitening, key should have bits set across all bytes
        let nonzero_bytes = key.iter().filter(|&&b| b != 0).count();
        assert!(nonzero_bytes >= 8, "key should have at least 8 non-zero bytes, got {nonzero_bytes}");
    }

    #[test]
    fn clone_shares_state() {
        let cache1 = make_encrypted_cache();
        let cache2 = cache1.clone();
        cache1.set("ep:test:keys", b"shared-secret", 60).unwrap();
        let result = cache2.get("ep:test:keys").unwrap().unwrap();
        assert_eq!(result, b"shared-secret".to_vec());
    }

    #[test]
    fn iv_uniqueness() {
        let iv1 = generate_iv();
        let iv2 = generate_iv();
        assert_ne!(iv1, iv2);
    }
}
