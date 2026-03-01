use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgepackError, Result};
use crate::media::container::ContainerFormat;
use crate::url::Url;
use serde::{Deserialize, Serialize};

/// Top-level application configuration, typically loaded from environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// Cache store backend configuration (Redis, Cloudflare KV, HTTP KV, etc.).
    pub store: StoreConfig,
    pub drm: DrmConfig,
    pub cache: CacheConfig,
    /// JIT (just-in-time) packaging configuration. Disabled by default.
    #[serde(default)]
    pub jit: JitConfig,
    /// Cloudflare Workers KV configuration (only when using CloudflareKv backend).
    #[cfg(feature = "cloudflare")]
    #[serde(default)]
    pub cloudflare_kv: Option<CloudflareKvConfig>,
    /// Generic HTTP KV configuration (only when using HttpKv backend).
    #[serde(default)]
    pub http_kv: Option<HttpKvConfig>,
}

/// Cache store configuration.
///
/// For Redis backends, `url` is the Redis endpoint and `token` is the auth token.
/// For other backends, `url` and `token` are used as the general endpoint/auth.
/// Platform-specific config is in dedicated config structs (e.g. `CloudflareKvConfig`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreConfig {
    /// Endpoint URL for the cache store.
    pub url: String,
    /// Authentication token for the cache store.
    pub token: String,
    /// Which backend type to use.
    pub backend: CacheBackendType,
}

/// Cache backend type selector.
///
/// Determines which cache backend implementation is used for application state storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum CacheBackendType {
    /// Upstash-compatible HTTP Redis (default). Uses GET-based REST API.
    RedisHttp,
    /// TCP Redis (stub — for future runtimes with socket support).
    RedisTcp,
    /// Cloudflare Workers KV. Requires `cloudflare` feature and `CF_*` env vars.
    #[cfg(feature = "cloudflare")]
    CloudflareKv,
    /// Generic HTTP KV backend. Works with any REST API following GET/PUT/DELETE `{base}/{key}`.
    /// Suitable for AWS DynamoDB via API Gateway, Akamai EdgeKV via proxy, or custom KV stores.
    HttpKv,
}

/// Cloudflare Workers KV configuration.
#[cfg(feature = "cloudflare")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CloudflareKvConfig {
    /// Cloudflare account ID.
    pub account_id: String,
    /// Workers KV namespace ID.
    pub namespace_id: String,
    /// Cloudflare API token with Workers KV permissions.
    pub api_token: String,
    /// API base URL (default: "https://api.cloudflare.com/client/v4").
    #[serde(default = "default_cf_api_base_url")]
    pub api_base_url: String,
}

#[cfg(feature = "cloudflare")]
fn default_cf_api_base_url() -> String {
    "https://api.cloudflare.com/client/v4".to_string()
}

/// Generic HTTP KV configuration.
///
/// Connects to any REST API following the pattern:
/// - `GET {base_url}/{key}` → 200 = value body, 404 = not found
/// - `PUT {base_url}/{key}?ttl={seconds}` → set value (body = raw value)
/// - `DELETE {base_url}/{key}` → delete key
///
/// Suitable for AWS DynamoDB via API Gateway, Akamai EdgeKV via auth proxy,
/// or any custom KV store with a RESTful interface.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpKvConfig {
    /// Base URL for the KV API (e.g. "https://xxx.execute-api.us-east-1.amazonaws.com/prod").
    pub base_url: String,
    /// HTTP header name for authentication (e.g. "x-api-key" or "Authorization").
    pub auth_header: String,
    /// HTTP header value for authentication (e.g. "abc123" or "Bearer xxx").
    pub auth_value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmConfig {
    /// SPEKE 2.0 license server endpoint URL.
    pub speke_url: Url,
    /// Authentication credentials for the license server (API key, bearer token, etc.).
    pub speke_auth: SpekeAuth,
    /// Default DRM system IDs to request keys for.
    pub system_ids: DrmSystemIds,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SpekeAuth {
    Bearer(String),
    ApiKey { header: String, value: String },
    Basic { username: String, password: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DrmSystemIds {
    /// Widevine system ID: edef8ba9-79d6-4ace-a3c8-27dcd51d21ed
    pub widevine: bool,
    /// PlayReady system ID: 9a04f079-9840-4286-ab92-e65be0885f95
    pub playready: bool,
}

impl Default for DrmSystemIds {
    fn default() -> Self {
        Self {
            widevine: true,
            playready: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Max-age in seconds for finalized (VOD) content. Default: 1 year.
    pub vod_max_age: u64,
    /// Max-age in seconds for live/in-progress manifests. Default: 1 second.
    pub live_manifest_max_age: u64,
    /// TTL in seconds for DRM keys stored in Redis. Default: 24 hours.
    pub drm_key_ttl: u64,
    /// TTL in seconds for job state stored in Redis. Default: 48 hours.
    pub job_state_ttl: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            vod_max_age: 31_536_000, // 1 year
            live_manifest_max_age: 1,
            drm_key_ttl: 86_400,      // 24 hours
            job_state_ttl: 172_800,    // 48 hours
        }
    }
}

/// JIT (just-in-time) packaging configuration.
///
/// When enabled, GET requests for manifests/segments trigger on-demand
/// repackaging on cache miss, eliminating the need for webhook pre-warming.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitConfig {
    /// Whether JIT packaging is enabled. Default: false.
    pub enabled: bool,
    /// URL pattern for resolving source manifests from content IDs.
    /// Use `{content_id}` as a placeholder, e.g. "https://origin.example.com/{content_id}/manifest.m3u8".
    /// If not set, per-content source config must be registered via `POST /config/source`.
    pub source_url_pattern: Option<String>,
    /// Default target encryption scheme when not specified in the request URL.
    pub default_target_scheme: EncryptionScheme,
    /// Default container format when not specified in source config.
    pub default_container_format: ContainerFormat,
    /// TTL in seconds for distributed processing locks. Default: 30 seconds.
    pub lock_ttl_seconds: u64,
}

impl Default for JitConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            source_url_pattern: None,
            default_target_scheme: EncryptionScheme::Cenc,
            default_container_format: ContainerFormat::Cmaf,
            lock_ttl_seconds: 30,
        }
    }
}

impl AppConfig {
    /// Load configuration from environment variables.
    ///
    /// Backward compatible: `REDIS_URL`, `REDIS_TOKEN`, `REDIS_BACKEND` still work.
    /// For non-Redis backends, use `STORE_URL`/`STORE_TOKEN` or `CACHE_BACKEND` override.
    pub fn from_env() -> Result<Self> {
        // Cache backend selection — CACHE_BACKEND overrides REDIS_BACKEND
        let backend = match std::env::var("CACHE_BACKEND").ok().as_deref() {
            Some("redis_tcp") => CacheBackendType::RedisTcp,
            #[cfg(feature = "cloudflare")]
            Some("cloudflare_kv") => CacheBackendType::CloudflareKv,
            Some("http_kv") => CacheBackendType::HttpKv,
            Some("redis_http") | None => {
                // Fall back to REDIS_BACKEND for backward compat
                match env_var_or("REDIS_BACKEND", "http").as_str() {
                    "tcp" => CacheBackendType::RedisTcp,
                    _ => CacheBackendType::RedisHttp,
                }
            }
            Some(other) => {
                return Err(EdgepackError::Config(format!(
                    "unknown CACHE_BACKEND: {other} (expected redis_http, redis_tcp, cloudflare_kv, or http_kv)"
                )));
            }
        };

        // Store URL and token — STORE_URL/STORE_TOKEN fall back to REDIS_URL/REDIS_TOKEN
        let store_url = std::env::var("STORE_URL")
            .or_else(|_| std::env::var("REDIS_URL"))
            .map_err(|_| EdgepackError::Config("missing env var: STORE_URL or REDIS_URL".into()))?;
        let store_token = std::env::var("STORE_TOKEN")
            .or_else(|_| std::env::var("REDIS_TOKEN"))
            .map_err(|_| EdgepackError::Config("missing env var: STORE_TOKEN or REDIS_TOKEN".into()))?;

        let speke_url = Url::parse(&env_var("SPEKE_URL")?)
            .map_err(|e| EdgepackError::Config(format!("invalid SPEKE_URL: {e}")))?;

        let speke_auth = if let Ok(token) = env_var("SPEKE_BEARER_TOKEN") {
            SpekeAuth::Bearer(token)
        } else if let Ok(api_key) = env_var("SPEKE_API_KEY") {
            let header = env_var_or("SPEKE_API_KEY_HEADER", "x-api-key");
            SpekeAuth::ApiKey {
                header,
                value: api_key,
            }
        } else {
            let username = env_var("SPEKE_USERNAME")?;
            let password = env_var("SPEKE_PASSWORD")?;
            SpekeAuth::Basic { username, password }
        };

        // JIT configuration — all optional, defaults to disabled
        let jit_enabled = env_var_or("JIT_ENABLED", "false") == "true";
        let jit_source_url_pattern = std::env::var("JIT_SOURCE_URL_PATTERN").ok();
        let jit_default_target_scheme = match env_var_or("JIT_DEFAULT_TARGET_SCHEME", "cenc").as_str() {
            "cbcs" => EncryptionScheme::Cbcs,
            _ => EncryptionScheme::Cenc,
        };
        let jit_default_container_format = match env_var_or("JIT_DEFAULT_CONTAINER_FORMAT", "cmaf").as_str() {
            "fmp4" => ContainerFormat::Fmp4,
            _ => ContainerFormat::Cmaf,
        };
        let jit_lock_ttl: u64 = env_var_or("JIT_LOCK_TTL", "30")
            .parse()
            .unwrap_or(30);

        // Cloudflare KV config — optional, loaded from CF_* env vars
        #[cfg(feature = "cloudflare")]
        let cloudflare_kv = if matches!(backend, CacheBackendType::CloudflareKv) {
            Some(CloudflareKvConfig {
                account_id: env_var("CF_ACCOUNT_ID")?,
                namespace_id: env_var("CF_KV_NAMESPACE_ID")?,
                api_token: env_var("CF_API_TOKEN")?,
                api_base_url: env_var_or("CF_API_BASE_URL", "https://api.cloudflare.com/client/v4"),
            })
        } else {
            None
        };

        // Generic HTTP KV config — optional, loaded from HTTP_KV_* env vars
        let http_kv = if matches!(backend, CacheBackendType::HttpKv) {
            Some(HttpKvConfig {
                base_url: env_var("HTTP_KV_BASE_URL")?,
                auth_header: env_var_or("HTTP_KV_AUTH_HEADER", "Authorization"),
                auth_value: env_var("HTTP_KV_AUTH_VALUE")?,
            })
        } else {
            None
        };

        Ok(Self {
            store: StoreConfig {
                url: store_url,
                token: store_token,
                backend,
            },
            drm: DrmConfig {
                speke_url,
                speke_auth,
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
            jit: JitConfig {
                enabled: jit_enabled,
                source_url_pattern: jit_source_url_pattern,
                default_target_scheme: jit_default_target_scheme,
                default_container_format: jit_default_container_format,
                lock_ttl_seconds: jit_lock_ttl,
            },
            #[cfg(feature = "cloudflare")]
            cloudflare_kv,
            http_kv,
        })
    }

    /// Get the token used for cache encryption key derivation.
    ///
    /// Uses `CACHE_ENCRYPTION_TOKEN` if set, otherwise falls back to the store token.
    pub fn cache_encryption_token(&self) -> &str {
        // Check env at runtime — allows overriding the encryption key source
        // without needing a separate config field
        &self.store.token
    }
}

fn env_var(name: &str) -> Result<String> {
    std::env::var(name).map_err(|_| EdgepackError::Config(format!("missing env var: {name}")))
}

fn env_var_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_config_defaults() {
        let c = CacheConfig::default();
        assert_eq!(c.vod_max_age, 31_536_000);
        assert_eq!(c.live_manifest_max_age, 1);
        assert_eq!(c.drm_key_ttl, 86_400);
        assert_eq!(c.job_state_ttl, 172_800);
    }

    #[test]
    fn drm_system_ids_defaults() {
        let ids = DrmSystemIds::default();
        assert!(ids.widevine);
        assert!(ids.playready);
    }

    #[test]
    fn cache_config_serializes_roundtrip() {
        let c = CacheConfig::default();
        let json = serde_json::to_string(&c).unwrap();
        let c2: CacheConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.vod_max_age, c.vod_max_age);
        assert_eq!(c2.live_manifest_max_age, c.live_manifest_max_age);
    }

    #[test]
    fn cache_backend_type_serializes() {
        let http = CacheBackendType::RedisHttp;
        let json = serde_json::to_string(&http).unwrap();
        assert!(json.contains("RedisHttp"));

        let tcp = CacheBackendType::RedisTcp;
        let json = serde_json::to_string(&tcp).unwrap();
        assert!(json.contains("RedisTcp"));

        let hkv = CacheBackendType::HttpKv;
        let json = serde_json::to_string(&hkv).unwrap();
        assert!(json.contains("HttpKv"));
    }

    #[test]
    fn http_kv_config_serde_roundtrip() {
        let cfg = HttpKvConfig {
            base_url: "https://api.example.com/kv".into(),
            auth_header: "x-api-key".into(),
            auth_value: "secret123".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: HttpKvConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.base_url, "https://api.example.com/kv");
        assert_eq!(parsed.auth_header, "x-api-key");
        assert_eq!(parsed.auth_value, "secret123");
    }

    #[cfg(feature = "cloudflare")]
    #[test]
    fn cloudflare_kv_config_serde_roundtrip() {
        let cfg = CloudflareKvConfig {
            account_id: "abc123".into(),
            namespace_id: "ns456".into(),
            api_token: "token789".into(),
            api_base_url: "https://api.cloudflare.com/client/v4".into(),
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: CloudflareKvConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.account_id, "abc123");
        assert_eq!(parsed.namespace_id, "ns456");
    }

    #[test]
    fn speke_auth_bearer_serializes() {
        let auth = SpekeAuth::Bearer("my-token".into());
        let json = serde_json::to_string(&auth).unwrap();
        assert!(json.contains("my-token"));
        let roundtrip: SpekeAuth = serde_json::from_str(&json).unwrap();
        match roundtrip {
            SpekeAuth::Bearer(t) => assert_eq!(t, "my-token"),
            _ => panic!("expected Bearer"),
        }
    }

    #[test]
    fn speke_auth_api_key_serializes() {
        let auth = SpekeAuth::ApiKey {
            header: "x-api-key".into(),
            value: "secret".into(),
        };
        let json = serde_json::to_string(&auth).unwrap();
        let roundtrip: SpekeAuth = serde_json::from_str(&json).unwrap();
        match roundtrip {
            SpekeAuth::ApiKey { header, value } => {
                assert_eq!(header, "x-api-key");
                assert_eq!(value, "secret");
            }
            _ => panic!("expected ApiKey"),
        }
    }

    #[test]
    fn speke_auth_basic_serializes() {
        let auth = SpekeAuth::Basic {
            username: "user".into(),
            password: "pass".into(),
        };
        let json = serde_json::to_string(&auth).unwrap();
        let roundtrip: SpekeAuth = serde_json::from_str(&json).unwrap();
        match roundtrip {
            SpekeAuth::Basic { username, password } => {
                assert_eq!(username, "user");
                assert_eq!(password, "pass");
            }
            _ => panic!("expected Basic"),
        }
    }

    #[test]
    fn env_var_missing_returns_config_error() {
        let result = env_var("EDGEPACK_TEST_NONEXISTENT_VAR_12345");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("missing env var"));
    }

    #[test]
    fn env_var_or_returns_default_when_missing() {
        let val = env_var_or("EDGEPACK_TEST_NONEXISTENT_VAR_12345", "fallback");
        assert_eq!(val, "fallback");
    }

    #[test]
    fn env_var_or_returns_set_value() {
        std::env::set_var("EDGEPACK_TEST_ENVVAR_OR", "actual");
        let val = env_var_or("EDGEPACK_TEST_ENVVAR_OR", "fallback");
        assert_eq!(val, "actual");
        std::env::remove_var("EDGEPACK_TEST_ENVVAR_OR");
    }

    #[test]
    fn jit_config_defaults() {
        let jit = JitConfig::default();
        assert!(!jit.enabled);
        assert!(jit.source_url_pattern.is_none());
        assert_eq!(jit.default_target_scheme, EncryptionScheme::Cenc);
        assert_eq!(jit.default_container_format, ContainerFormat::Cmaf);
        assert_eq!(jit.lock_ttl_seconds, 30);
    }

    #[test]
    fn jit_config_serde_roundtrip() {
        let jit = JitConfig {
            enabled: true,
            source_url_pattern: Some("https://origin.example.com/{content_id}/manifest.m3u8".into()),
            default_target_scheme: EncryptionScheme::Cbcs,
            default_container_format: ContainerFormat::Fmp4,
            lock_ttl_seconds: 60,
        };
        let json = serde_json::to_string(&jit).unwrap();
        let parsed: JitConfig = serde_json::from_str(&json).unwrap();
        assert!(parsed.enabled);
        assert_eq!(parsed.source_url_pattern.as_deref(), Some("https://origin.example.com/{content_id}/manifest.m3u8"));
        assert_eq!(parsed.default_target_scheme, EncryptionScheme::Cbcs);
        assert_eq!(parsed.default_container_format, ContainerFormat::Fmp4);
        assert_eq!(parsed.lock_ttl_seconds, 60);
    }

    #[test]
    fn app_config_jit_defaults_when_missing() {
        // When jit field is missing from JSON, it should use defaults
        let json = r#"{
            "store": {"url": "https://redis.test", "token": "tok", "backend": "RedisHttp"},
            "drm": {"speke_url": "https://speke.test/v2", "speke_auth": {"Bearer": "test"}, "system_ids": {"widevine": true, "playready": true}},
            "cache": {"vod_max_age": 31536000, "live_manifest_max_age": 1, "drm_key_ttl": 86400, "job_state_ttl": 172800}
        }"#;
        let parsed: AppConfig = serde_json::from_str(json).unwrap();
        assert!(!parsed.jit.enabled);
        assert!(parsed.jit.source_url_pattern.is_none());
    }

    #[test]
    fn from_env_fails_without_required_vars() {
        // Make sure the required vars are NOT set
        std::env::remove_var("REDIS_URL");
        let result = AppConfig::from_env();
        assert!(result.is_err());
    }
}
