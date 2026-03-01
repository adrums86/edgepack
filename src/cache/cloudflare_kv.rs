//! Cloudflare Workers KV cache backend.
//!
//! Uses the Cloudflare Workers KV REST API for cache storage.
//! This backend is only available when the `cloudflare` feature is enabled.
//!
//! API pattern:
//! - GET value: `GET {base}/accounts/{account_id}/storage/kv/namespaces/{namespace_id}/values/{key}`
//! - SET value: `PUT {base}/accounts/{account_id}/storage/kv/namespaces/{namespace_id}/values/{key}?expiration_ttl={ttl}`
//! - DELETE value: `DELETE {base}/accounts/{account_id}/storage/kv/namespaces/{namespace_id}/values/{key}`
//! - Auth: `Authorization: Bearer {api_token}`

use crate::cache::CacheBackend;
use crate::config::CloudflareKvConfig;
use crate::error::{EdgepackError, Result};
use crate::http_client;

/// Cloudflare Workers KV cache backend.
///
/// Stores and retrieves values via the Cloudflare KV REST API.
/// Binary values are stored directly (CF KV handles binary natively).
pub struct CloudflareKvBackend {
    account_id: String,
    namespace_id: String,
    api_token: String,
    base_url: String,
}

impl CloudflareKvBackend {
    pub fn new(config: &CloudflareKvConfig) -> Self {
        Self {
            account_id: config.account_id.clone(),
            namespace_id: config.namespace_id.clone(),
            api_token: config.api_token.clone(),
            base_url: config.api_base_url.trim_end_matches('/').to_string(),
        }
    }

    /// Build the KV API URL for a given key.
    fn kv_url(&self, key: &str) -> String {
        let encoded_key = encode_key(key);
        format!(
            "{}/accounts/{}/storage/kv/namespaces/{}/values/{}",
            self.base_url, self.account_id, self.namespace_id, encoded_key
        )
    }

    /// Build authorization headers.
    fn auth_headers(&self) -> Vec<(String, String)> {
        vec![("Authorization".to_string(), format!("Bearer {}", self.api_token))]
    }
}

impl CacheBackend for CloudflareKvBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let url = self.kv_url(key);
        let resp = http_client::get(&url, &self.auth_headers())?;

        match resp.status {
            200 => Ok(Some(resp.body)),
            404 => Ok(None),
            status => Err(EdgepackError::Cache(format!(
                "Cloudflare KV GET failed (status {}): {}",
                status,
                String::from_utf8_lossy(&resp.body)
            ))),
        }
    }

    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()> {
        let url = format!("{}?expiration_ttl={}", self.kv_url(key), ttl_seconds);
        let mut headers = self.auth_headers();
        headers.push(("Content-Type".to_string(), "application/octet-stream".to_string()));

        let resp = http_client::put(&url, &headers, value.to_vec())?;

        match resp.status {
            200 => Ok(()),
            status => Err(EdgepackError::Cache(format!(
                "Cloudflare KV PUT failed (status {}): {}",
                status,
                String::from_utf8_lossy(&resp.body)
            ))),
        }
    }

    fn set_nx(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<bool> {
        // CF KV doesn't support atomic compare-and-set.
        // GET-then-PUT is acceptable for distributed locks with TTL expiry.
        if self.exists(key)? {
            return Ok(false);
        }
        self.set(key, value, ttl_seconds)?;
        Ok(true)
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let url = self.kv_url(key);
        let resp = http_client::get(&url, &self.auth_headers())?;
        Ok(resp.status == 200)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let url = self.kv_url(key);
        let resp = http_client::delete_request(&url, &self.auth_headers())?;

        match resp.status {
            200 | 404 => Ok(()), // 404 = already deleted, treat as success
            status => Err(EdgepackError::Cache(format!(
                "Cloudflare KV DELETE failed (status {}): {}",
                status,
                String::from_utf8_lossy(&resp.body)
            ))),
        }
    }
}

/// URL-encode a cache key for safe use in URL paths.
///
/// Cache keys contain `:` (which is URL-safe) but we encode other special
/// characters to be safe across all HTTP implementations.
fn encode_key(key: &str) -> String {
    let mut encoded = String::with_capacity(key.len());
    for byte in key.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b':' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push_str(&format!("%{:02X}", byte));
            }
        }
    }
    encoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_url_construction() {
        let config = CloudflareKvConfig {
            account_id: "acc123".into(),
            namespace_id: "ns456".into(),
            api_token: "tok789".into(),
            api_base_url: "https://api.cloudflare.com/client/v4".into(),
        };
        let backend = CloudflareKvBackend::new(&config);
        let url = backend.kv_url("ep:content-1:keys");
        assert_eq!(
            url,
            "https://api.cloudflare.com/client/v4/accounts/acc123/storage/kv/namespaces/ns456/values/ep:content-1:keys"
        );
    }

    #[test]
    fn kv_url_key_encoding() {
        // Cache keys with colons should pass through
        let encoded = encode_key("ep:content-1:hls:init");
        assert_eq!(encoded, "ep:content-1:hls:init");
    }

    #[test]
    fn kv_url_key_encoding_special_chars() {
        // Spaces and other chars should be percent-encoded
        let encoded = encode_key("ep:content 1:keys");
        assert_eq!(encoded, "ep:content%201:keys");
    }

    #[test]
    fn auth_headers_format() {
        let config = CloudflareKvConfig {
            account_id: "acc".into(),
            namespace_id: "ns".into(),
            api_token: "my-secret-token".into(),
            api_base_url: "https://api.cloudflare.com/client/v4".into(),
        };
        let backend = CloudflareKvBackend::new(&config);
        let headers = backend.auth_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer my-secret-token");
    }

    #[test]
    fn kv_url_trailing_slash_normalized() {
        let config = CloudflareKvConfig {
            account_id: "acc".into(),
            namespace_id: "ns".into(),
            api_token: "tok".into(),
            api_base_url: "https://api.cloudflare.com/client/v4/".into(),
        };
        let backend = CloudflareKvBackend::new(&config);
        let url = backend.kv_url("key");
        assert!(!url.contains("v4//"), "URL should not have double slashes");
    }

    #[test]
    fn encode_key_preserves_safe_chars() {
        let key = "ep:my-content_v2.0:hls:seg:42";
        let encoded = encode_key(key);
        assert_eq!(encoded, key); // all chars are safe
    }

    #[test]
    fn encode_key_encodes_unsafe_chars() {
        let encoded = encode_key("key with spaces/and slashes");
        assert!(encoded.contains("%20"));
        assert!(encoded.contains("%2F"));
    }
}
