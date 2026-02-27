use crate::cache::CacheBackend;
use crate::error::{EdgePackagerError, Result};

/// HTTP-based Redis backend compatible with Upstash REST API.
///
/// Uses simple HTTP GET/POST requests instead of TCP sockets,
/// making it compatible with all edge/WASM environments.
pub struct RedisHttpBackend {
    url: String,
    token: String,
}

impl RedisHttpBackend {
    pub fn new(url: &str, token: &str) -> Self {
        Self {
            url: url.trim_end_matches('/').to_string(),
            token: token.to_string(),
        }
    }

    /// Execute a Redis command via the Upstash HTTP API.
    ///
    /// Uses the shared HTTP client (WASI outgoing-handler on wasm32,
    /// stub error on native) to call the Upstash REST API.
    fn execute_command(&self, args: &[&str]) -> Result<Option<Vec<u8>>> {
        let endpoint = format!("{}/{}", self.url, args.join("/"));
        let headers = vec![
            ("Authorization".to_string(), format!("Bearer {}", self.token)),
        ];

        let response = crate::http_client::get(&endpoint, &headers)
            .map_err(|e| EdgePackagerError::Cache(format!("Redis HTTP request failed: {e}")))?;

        if response.status >= 400 {
            return Err(EdgePackagerError::Cache(format!(
                "Redis HTTP error: status {}",
                response.status
            )));
        }

        parse_upstash_response(response.status, &response.body)
    }
}

/// Parse an Upstash REST API JSON response.
///
/// Upstash returns JSON like `{ "result": "value" }` for string results,
/// `{ "result": null }` when a key doesn't exist, and `{ "result": 1 }` for
/// integer results (EXISTS, DEL). This function is extracted for testability
/// on native targets.
fn parse_upstash_response(_status: u16, body: &[u8]) -> Result<Option<Vec<u8>>> {
    let parsed: serde_json::Value = serde_json::from_slice(body).map_err(|e| {
        EdgePackagerError::Cache(format!("failed to parse Upstash response: {e}"))
    })?;

    match parsed.get("result") {
        Some(serde_json::Value::String(s)) => Ok(Some(s.as_bytes().to_vec())),
        Some(serde_json::Value::Number(n)) => Ok(Some(n.to_string().into_bytes())),
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(b.to_string().into_bytes())),
        Some(other) => Ok(Some(other.to_string().into_bytes())),
        None => {
            // Check for error field
            if let Some(serde_json::Value::String(err)) = parsed.get("error") {
                Err(EdgePackagerError::Cache(format!("Upstash error: {err}")))
            } else {
                Err(EdgePackagerError::Cache(
                    "unexpected Upstash response format".into(),
                ))
            }
        }
    }
}

impl CacheBackend for RedisHttpBackend {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.execute_command(&["GET", key])
    }

    fn set(&self, key: &str, value: &[u8], ttl_seconds: u64) -> Result<()> {
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            value,
        );
        let ttl = ttl_seconds.to_string();
        self.execute_command(&["SET", key, &encoded, "EX", &ttl])?;
        Ok(())
    }

    fn exists(&self, key: &str) -> Result<bool> {
        match self.execute_command(&["EXISTS", key]) {
            Ok(Some(data)) => {
                // Upstash returns "1" for exists, "0" for not
                let s = String::from_utf8_lossy(&data);
                Ok(s.trim() == "1")
            }
            Ok(None) => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn delete(&self, key: &str) -> Result<()> {
        self.execute_command(&["DEL", key])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_trims_trailing_slash() {
        let backend = RedisHttpBackend::new("https://example.com/", "token");
        assert_eq!(backend.url, "https://example.com");
    }

    #[test]
    fn new_no_trailing_slash() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        assert_eq!(backend.url, "https://example.com");
    }

    #[test]
    fn new_preserves_token() {
        let backend = RedisHttpBackend::new("https://example.com", "my-secret-token");
        assert_eq!(backend.token, "my-secret-token");
    }

    // On native targets, the HTTP client returns an error (no WASI transport),
    // which gets wrapped as a Cache error by execute_command.
    #[test]
    fn get_returns_error_on_native() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.get("test-key");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Redis HTTP request failed") || err.contains("only available in WASI"));
    }

    #[test]
    fn set_returns_error_on_native() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.set("key", b"value", 3600);
        assert!(result.is_err());
    }

    #[test]
    fn exists_returns_error_on_native() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.exists("key");
        assert!(result.is_err());
    }

    #[test]
    fn delete_returns_error_on_native() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.delete("key");
        assert!(result.is_err());
    }

    // --- Upstash response parsing tests (testable on native) ---

    #[test]
    fn parse_upstash_string_result() {
        let body = br#"{"result":"hello world"}"#;
        let result = parse_upstash_response(200, body).unwrap();
        assert_eq!(result, Some(b"hello world".to_vec()));
    }

    #[test]
    fn parse_upstash_null_result() {
        let body = br#"{"result":null}"#;
        let result = parse_upstash_response(200, body).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_upstash_integer_result() {
        let body = br#"{"result":1}"#;
        let result = parse_upstash_response(200, body).unwrap();
        assert_eq!(result, Some(b"1".to_vec()));
    }

    #[test]
    fn parse_upstash_zero_result() {
        let body = br#"{"result":0}"#;
        let result = parse_upstash_response(200, body).unwrap();
        assert_eq!(result, Some(b"0".to_vec()));
    }

    #[test]
    fn parse_upstash_ok_result() {
        let body = br#"{"result":"OK"}"#;
        let result = parse_upstash_response(200, body).unwrap();
        assert_eq!(result, Some(b"OK".to_vec()));
    }

    #[test]
    fn parse_upstash_error_response() {
        let body = br#"{"error":"WRONGTYPE Operation against a key"}"#;
        let result = parse_upstash_response(200, body);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Upstash error"));
    }

    #[test]
    fn parse_upstash_invalid_json() {
        let body = b"not json";
        let result = parse_upstash_response(200, body);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("parse Upstash response"));
    }

    #[test]
    fn parse_upstash_base64_encoded_value() {
        // The SET command sends base64-encoded values; GET returns them as-is
        let encoded = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            b"binary data",
        );
        let body = format!(r#"{{"result":"{}"}}"#, encoded);
        let result = parse_upstash_response(200, body.as_bytes()).unwrap();
        let value = result.unwrap();
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            &value,
        ).unwrap();
        assert_eq!(decoded, b"binary data");
    }

    #[test]
    fn parse_upstash_unexpected_format() {
        let body = br#"{"something":"else"}"#;
        let result = parse_upstash_response(200, body);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unexpected"));
    }
}
