//! Generic HTTP KV cache backend.
//!
//! A configurable HTTP-based KV backend that works with any REST API following:
//! - `GET {base_url}/{key}` → 200 = value body, 404 = not found
//! - `PUT {base_url}/{key}?ttl={seconds}` → set value (body = raw value)
//! - `DELETE {base_url}/{key}` → delete key
//!
//! This covers:
//! - AWS DynamoDB via API Gateway (user deploys API GW + Lambda + DynamoDB)
//! - Akamai EdgeKV via auth proxy (user deploys proxy in front of EdgeKV)
//! - Any custom KV store with a REST interface

use crate::cache::CacheBackend;
use crate::config::HttpKvConfig;
use crate::error::{EdgepackError, Result};
use crate::http_client;

/// Generic HTTP KV cache backend.
///
/// Communicates with a REST API following a simple GET/PUT/DELETE pattern.
/// Authentication is handled via a configurable header (e.g. `Authorization`,
/// `x-api-key`).
pub struct HttpKvBackend {
    base_url: String,
    auth_header: String,
    auth_value: String,
}

impl HttpKvBackend {
    pub fn new(config: &HttpKvConfig) -> Self {
        Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            auth_header: config.auth_header.clone(),
            auth_value: config.auth_value.clone(),
        }
    }

    /// Build the item URL for a given key.
    fn item_url(&self, key: &str) -> String {
        format!("{}/{}", self.base_url, encode_key(key))
    }

    /// Build authentication headers.
    fn auth_headers(&self) -> Vec<(String, String)> {
        vec![(self.auth_header.clone(), self.auth_value.clone())]
    }
}

impl CacheBackend for HttpKvBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let url = self.item_url(key);
        let resp = http_client::get(&url, &self.auth_headers())?;

        match resp.status {
            200 => Ok(Some(resp.body)),
            404 => Ok(None),
            status => Err(EdgepackError::Cache(format!(
                "HTTP KV GET failed (status {}): {}",
                status,
                String::from_utf8_lossy(&resp.body)
            ))),
        }
    }

    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()> {
        let url = format!("{}?ttl={}", self.item_url(key), ttl_seconds);
        let mut headers = self.auth_headers();
        headers.push(("Content-Type".to_string(), "application/octet-stream".to_string()));

        let resp = http_client::put(&url, &headers, value.to_vec())?;

        match resp.status {
            200 | 201 | 204 => Ok(()),
            status => Err(EdgepackError::Cache(format!(
                "HTTP KV PUT failed (status {}): {}",
                status,
                String::from_utf8_lossy(&resp.body)
            ))),
        }
    }

    fn set_nx(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<bool> {
        // Generic HTTP KV doesn't support atomic compare-and-set.
        // GET-then-PUT is acceptable for distributed locks with TTL expiry.
        if self.exists(key)? {
            return Ok(false);
        }
        self.set(key, value, ttl_seconds)?;
        Ok(true)
    }

    fn exists(&self, key: &str) -> Result<bool> {
        let url = self.item_url(key);
        let resp = http_client::get(&url, &self.auth_headers())?;
        Ok(resp.status == 200)
    }

    fn delete(&self, key: &str) -> Result<()> {
        let url = self.item_url(key);
        let resp = http_client::delete_request(&url, &self.auth_headers())?;

        match resp.status {
            200 | 204 | 404 => Ok(()), // 404 = already deleted, treat as success
            status => Err(EdgepackError::Cache(format!(
                "HTTP KV DELETE failed (status {}): {}",
                status,
                String::from_utf8_lossy(&resp.body)
            ))),
        }
    }
}

/// URL-encode a cache key for safe use in URL paths.
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
    fn item_url_construction() {
        let config = HttpKvConfig {
            base_url: "https://api.example.com/kv".into(),
            auth_header: "x-api-key".into(),
            auth_value: "secret".into(),
        };
        let backend = HttpKvBackend::new(&config);
        let url = backend.item_url("ep:content-1:keys");
        assert_eq!(url, "https://api.example.com/kv/ep:content-1:keys");
    }

    #[test]
    fn item_url_key_encoding() {
        let encoded = encode_key("ep:content-1:hls:init");
        assert_eq!(encoded, "ep:content-1:hls:init");
    }

    #[test]
    fn item_url_key_encoding_special_chars() {
        let encoded = encode_key("ep:content 1:keys");
        assert_eq!(encoded, "ep:content%201:keys");
    }

    #[test]
    fn auth_headers_injection() {
        let config = HttpKvConfig {
            base_url: "https://api.example.com/kv".into(),
            auth_header: "x-api-key".into(),
            auth_value: "my-api-key-123".into(),
        };
        let backend = HttpKvBackend::new(&config);
        let headers = backend.auth_headers();
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].0, "x-api-key");
        assert_eq!(headers[0].1, "my-api-key-123");
    }

    #[test]
    fn auth_headers_authorization_bearer() {
        let config = HttpKvConfig {
            base_url: "https://api.example.com/kv".into(),
            auth_header: "Authorization".into(),
            auth_value: "Bearer tok123".into(),
        };
        let backend = HttpKvBackend::new(&config);
        let headers = backend.auth_headers();
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer tok123");
    }

    #[test]
    fn item_url_trailing_slash_normalized() {
        let config = HttpKvConfig {
            base_url: "https://api.example.com/kv/".into(),
            auth_header: "x-api-key".into(),
            auth_value: "key".into(),
        };
        let backend = HttpKvBackend::new(&config);
        let url = backend.item_url("key");
        assert!(!url.contains("kv//"), "URL should not have double slashes");
    }

    #[test]
    fn encode_key_preserves_safe_chars() {
        let key = "ep:my-content_v2.0:hls:seg:42";
        let encoded = encode_key(key);
        assert_eq!(encoded, key);
    }

    #[test]
    fn encode_key_encodes_unsafe_chars() {
        let encoded = encode_key("key with spaces/and slashes");
        assert!(encoded.contains("%20"));
        assert!(encoded.contains("%2F"));
    }
}
