use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgepackError, Result};
use crate::media::container::ContainerFormat;
use crate::url::Url;
use serde::{Deserialize, Serialize};

/// Top-level application configuration, typically loaded from environment variables.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub drm: DrmConfig,
    pub cache: CacheConfig,
    /// JIT (just-in-time) packaging configuration.
    #[serde(default)]
    pub jit: JitConfig,
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
    /// ClearKey system. Not sent to SPEKE — PSSH is built locally.
    #[serde(default)]
    pub clearkey: bool,
}

impl Default for DrmSystemIds {
    fn default() -> Self {
        Self {
            widevine: true,
            playready: true,
            clearkey: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Max-age in seconds for segments and init segments. Default: 1 year.
    pub vod_max_age: u64,
    /// Max-age in seconds for live/in-progress manifests. Default: 1 second.
    pub live_manifest_max_age: u64,
    /// Max-age in seconds for finalized (VOD) manifests. Default: 1 year.
    /// When not present in JSON, defaults to vod_max_age.
    #[serde(default = "default_vod_max_age")]
    pub final_manifest_max_age: u64,
}

fn default_vod_max_age() -> u64 {
    31_536_000
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            vod_max_age: 31_536_000,          // 1 year
            live_manifest_max_age: 1,
            final_manifest_max_age: 31_536_000, // 1 year
        }
    }
}

/// Per-request cache-control header configuration.
///
/// Controls the `Cache-Control` headers on HTTP responses for manifests.
/// All fields are optional; when absent, the system-wide defaults from
/// `CacheConfig` (env vars) are used.
///
/// Safety invariants (not overridable):
/// - `AwaitingFirstSegment` manifests always use `no-cache`
/// - Status endpoint always uses `no-cache`
/// - All cacheable responses include `public`
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
pub struct CacheControlConfig {
    /// Max-age for segments and init segments (seconds). Default: 31536000 (1 year).
    #[serde(default)]
    pub segment_max_age: Option<u64>,
    /// Max-age for finalized/VOD manifests (seconds). Default: 31536000 (1 year).
    #[serde(default)]
    pub final_manifest_max_age: Option<u64>,
    /// Max-age (browser cache) for live manifests (seconds). Default: 1.
    #[serde(default)]
    pub live_manifest_max_age: Option<u64>,
    /// s-maxage (CDN/shared cache) for live manifests (seconds).
    /// When None, uses `live_manifest_max_age` value. Default: None (same as max-age).
    #[serde(default)]
    pub live_manifest_s_maxage: Option<u64>,
    /// Whether to include `immutable` on segments and finalized manifests.
    /// Default: true.
    #[serde(default)]
    pub immutable: Option<bool>,
}

impl CacheControlConfig {
    /// Whether to include the `immutable` directive. Defaults to true.
    pub fn is_immutable(&self) -> bool {
        self.immutable.unwrap_or(true)
    }
}

/// JIT (just-in-time) packaging configuration.
///
/// Controls on-demand repackaging behavior where GET requests for
/// manifests/segments trigger repackaging on cache miss.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JitConfig {
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
            source_url_pattern: None,
            default_target_scheme: EncryptionScheme::Cenc,
            default_container_format: ContainerFormat::Cmaf,
            lock_ttl_seconds: 30,
        }
    }
}

impl AppConfig {
    /// Load configuration from environment variables.
    pub fn from_env() -> Result<Self> {
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

        // JIT configuration
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

        Ok(Self {
            drm: DrmConfig {
                speke_url,
                speke_auth,
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig {
                vod_max_age: env_var_or("CACHE_MAX_AGE_SEGMENTS", "31536000")
                    .parse()
                    .unwrap_or(31_536_000),
                live_manifest_max_age: env_var_or("CACHE_MAX_AGE_MANIFEST_LIVE", "1")
                    .parse()
                    .unwrap_or(1),
                final_manifest_max_age: env_var_or("CACHE_MAX_AGE_MANIFEST_FINAL", "31536000")
                    .parse()
                    .unwrap_or(31_536_000),
            },
            jit: JitConfig {
                source_url_pattern: jit_source_url_pattern,
                default_target_scheme: jit_default_target_scheme,
                default_container_format: jit_default_container_format,
                lock_ttl_seconds: jit_lock_ttl,
            },
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
        assert_eq!(c.final_manifest_max_age, 31_536_000);
    }

    #[test]
    fn drm_system_ids_defaults() {
        let ids = DrmSystemIds::default();
        assert!(ids.widevine);
        assert!(ids.playready);
        assert!(!ids.clearkey);
    }

    #[test]
    fn drm_system_ids_clearkey_default() {
        let ids = DrmSystemIds::default();
        assert!(!ids.clearkey);
    }

    #[test]
    fn cache_config_serializes_roundtrip() {
        let c = CacheConfig::default();
        let json = serde_json::to_string(&c).unwrap();
        let c2: CacheConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(c2.vod_max_age, c.vod_max_age);
        assert_eq!(c2.live_manifest_max_age, c.live_manifest_max_age);
        assert_eq!(c2.final_manifest_max_age, c.final_manifest_max_age);
    }

    #[test]
    fn cache_config_backward_compat_without_final_manifest() {
        // Old JSON without final_manifest_max_age should deserialize with default
        let json = r#"{"vod_max_age":31536000,"live_manifest_max_age":1}"#;
        let c: CacheConfig = serde_json::from_str(json).unwrap();
        assert_eq!(c.final_manifest_max_age, 31_536_000);
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
        assert!(jit.source_url_pattern.is_none());
        assert_eq!(jit.default_target_scheme, EncryptionScheme::Cenc);
        assert_eq!(jit.default_container_format, ContainerFormat::Cmaf);
        assert_eq!(jit.lock_ttl_seconds, 30);
    }

    #[test]
    fn jit_config_serde_roundtrip() {
        let jit = JitConfig {
            source_url_pattern: Some("https://origin.example.com/{content_id}/manifest.m3u8".into()),
            default_target_scheme: EncryptionScheme::Cbcs,
            default_container_format: ContainerFormat::Fmp4,
            lock_ttl_seconds: 60,
        };
        let json = serde_json::to_string(&jit).unwrap();
        let parsed: JitConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_url_pattern.as_deref(), Some("https://origin.example.com/{content_id}/manifest.m3u8"));
        assert_eq!(parsed.default_target_scheme, EncryptionScheme::Cbcs);
        assert_eq!(parsed.default_container_format, ContainerFormat::Fmp4);
        assert_eq!(parsed.lock_ttl_seconds, 60);
    }

    #[test]
    fn app_config_jit_defaults_when_missing() {
        // When jit field is missing from JSON, it should use defaults
        let json = r#"{
            "drm": {"speke_url": "https://speke.test/v2", "speke_auth": {"Bearer": "test"}, "system_ids": {"widevine": true, "playready": true}},
            "cache": {"vod_max_age": 31536000, "live_manifest_max_age": 1}
        }"#;
        let parsed: AppConfig = serde_json::from_str(json).unwrap();
        assert!(parsed.jit.source_url_pattern.is_none());
    }

    #[test]
    fn from_env_fails_without_required_vars() {
        // Make sure the required vars are NOT set
        std::env::remove_var("SPEKE_URL");
        let result = AppConfig::from_env();
        assert!(result.is_err());
    }

    // --- CacheControlConfig tests ---

    #[test]
    fn cache_control_config_default_is_all_none() {
        let cc = CacheControlConfig::default();
        assert_eq!(cc.segment_max_age, None);
        assert_eq!(cc.final_manifest_max_age, None);
        assert_eq!(cc.live_manifest_max_age, None);
        assert_eq!(cc.live_manifest_s_maxage, None);
        assert_eq!(cc.immutable, None);
    }

    #[test]
    fn cache_control_config_is_immutable_defaults_true() {
        let cc = CacheControlConfig::default();
        assert!(cc.is_immutable());
    }

    #[test]
    fn cache_control_config_is_immutable_explicit_false() {
        let cc = CacheControlConfig {
            immutable: Some(false),
            ..Default::default()
        };
        assert!(!cc.is_immutable());
    }

    #[test]
    fn cache_control_config_is_immutable_explicit_true() {
        let cc = CacheControlConfig {
            immutable: Some(true),
            ..Default::default()
        };
        assert!(cc.is_immutable());
    }

    #[test]
    fn cache_control_config_serde_roundtrip_full() {
        let cc = CacheControlConfig {
            segment_max_age: Some(86400),
            final_manifest_max_age: Some(3600),
            live_manifest_max_age: Some(2),
            live_manifest_s_maxage: Some(1),
            immutable: Some(false),
        };
        let json = serde_json::to_string(&cc).unwrap();
        let parsed: CacheControlConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, cc);
    }

    #[test]
    fn cache_control_config_serde_roundtrip_partial() {
        let cc = CacheControlConfig {
            segment_max_age: Some(86400),
            ..Default::default()
        };
        let json = serde_json::to_string(&cc).unwrap();
        let parsed: CacheControlConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.segment_max_age, Some(86400));
        assert_eq!(parsed.final_manifest_max_age, None);
        assert_eq!(parsed.immutable, None);
    }

    #[test]
    fn cache_control_config_serde_empty_json() {
        let cc: CacheControlConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(cc, CacheControlConfig::default());
    }
}
