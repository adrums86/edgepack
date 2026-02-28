use crate::error::{EdgepackError, Result};
use crate::url::Url;
use serde::{Deserialize, Serialize};

/// Top-level application configuration, typically loaded from environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub redis: RedisConfig,
    pub drm: DrmConfig,
    pub cache: CacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RedisConfig {
    /// Redis endpoint URL (e.g. "https://us1-xxxx.upstash.io" for HTTP, or "redis://host:6379" for TCP).
    pub url: String,
    /// Authentication token or password.
    pub token: String,
    /// Which backend type to use.
    pub backend: RedisBackendType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RedisBackendType {
    Http,
    Tcp,
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

impl AppConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Result<Self> {
        let redis_url = env_var("REDIS_URL")?;
        let redis_token = env_var("REDIS_TOKEN")?;
        let redis_backend = match env_var_or("REDIS_BACKEND", "http").as_str() {
            "tcp" => RedisBackendType::Tcp,
            _ => RedisBackendType::Http,
        };

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

        Ok(Self {
            redis: RedisConfig {
                url: redis_url,
                token: redis_token,
                backend: redis_backend,
            },
            drm: DrmConfig {
                speke_url,
                speke_auth,
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
        })
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
    fn redis_backend_type_serializes() {
        let http = RedisBackendType::Http;
        let json = serde_json::to_string(&http).unwrap();
        assert!(json.contains("Http"));

        let tcp = RedisBackendType::Tcp;
        let json = serde_json::to_string(&tcp).unwrap();
        assert!(json.contains("Tcp"));
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
    fn from_env_fails_without_required_vars() {
        // Make sure the required vars are NOT set
        std::env::remove_var("REDIS_URL");
        let result = AppConfig::from_env();
        assert!(result.is_err());
    }
}
