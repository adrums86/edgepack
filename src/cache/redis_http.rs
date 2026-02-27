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
    /// In a WASI environment, this will use `wasi:http/outgoing-handler`
    /// to make the HTTP request. For now, the structure is in place
    /// and the actual HTTP call is abstracted.
    fn execute_command(&self, args: &[&str]) -> Result<Option<Vec<u8>>> {
        let _endpoint = format!("{}/{}", self.url, args.join("/"));
        let _auth = &self.token;

        // TODO: Implement using wasi:http/outgoing-handler
        // For now, return a placeholder that compiles.
        // The actual implementation will use the WASI HTTP API:
        //
        // 1. Build outgoing request to Upstash REST endpoint
        // 2. Set Authorization header: "Bearer {token}"
        // 3. Send request via wasi:http/outgoing-handler
        // 4. Parse JSON response: { "result": <value> }
        //
        // Example Upstash REST API:
        //   GET https://<endpoint>/GET/<key>
        //   GET https://<endpoint>/SET/<key>/<value>/EX/<ttl>
        //   GET https://<endpoint>/EXISTS/<key>
        //   GET https://<endpoint>/DEL/<key>

        Err(EdgePackagerError::Cache(
            "WASI HTTP transport not yet implemented".into(),
        ))
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

    #[test]
    fn get_returns_not_implemented_error() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.get("test-key");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not yet implemented"));
    }

    #[test]
    fn set_returns_not_implemented_error() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.set("key", b"value", 3600);
        assert!(result.is_err());
    }

    #[test]
    fn exists_returns_not_implemented_error() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.exists("key");
        assert!(result.is_err());
    }

    #[test]
    fn delete_returns_not_implemented_error() {
        let backend = RedisHttpBackend::new("https://example.com", "token");
        let result = backend.delete("key");
        assert!(result.is_err());
    }
}
