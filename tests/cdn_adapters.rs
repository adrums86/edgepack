//! Integration tests: Phase 17 — CDN Provider Adapters.
//!
//! Tests cache backend type selection, configuration loading, backward
//! compatibility (REDIS_URL/REDIS_TOKEN still work), and encryption token
//! derivation across different backend types.

mod common;

use edgepack::config::{
    AppConfig, CacheBackendType, CacheConfig, DrmConfig, DrmSystemIds, HttpKvConfig, JitConfig,
    SpekeAuth, StoreConfig,
};

fn make_config(backend: CacheBackendType) -> AppConfig {
    AppConfig {
        store: StoreConfig {
            url: "https://store.example.com".into(),
            token: "test-token".into(),
            backend,
        },
        drm: DrmConfig {
            speke_url: edgepack::url::Url::parse("https://drm.example.com/speke").unwrap(),
            speke_auth: SpekeAuth::Bearer("test-bearer-token".into()),
            system_ids: DrmSystemIds::default(),
        },
        cache: CacheConfig::default(),
        jit: JitConfig::default(),
        #[cfg(feature = "cloudflare")]
        cloudflare_kv: None,
        http_kv: None,
    }
}

// ─── Backend Type Selection ─────────────────────────────────────────────

#[test]
fn backend_type_redis_http_default() {
    let config = make_config(CacheBackendType::RedisHttp);
    assert_eq!(config.store.backend, CacheBackendType::RedisHttp);
}

#[test]
fn backend_type_redis_tcp() {
    let config = make_config(CacheBackendType::RedisTcp);
    assert_eq!(config.store.backend, CacheBackendType::RedisTcp);
}

#[test]
fn backend_type_http_kv() {
    let config = make_config(CacheBackendType::HttpKv);
    assert_eq!(config.store.backend, CacheBackendType::HttpKv);
}

#[cfg(feature = "cloudflare")]
#[test]
fn backend_type_cloudflare_kv() {
    let config = make_config(CacheBackendType::CloudflareKv);
    assert_eq!(config.store.backend, CacheBackendType::CloudflareKv);
}

// ─── Store Config ────────────────────────────────────────────────────────

#[test]
fn store_config_replaces_redis_config() {
    // Verify the new StoreConfig struct works with the same fields
    let config = StoreConfig {
        url: "https://redis.example.com".into(),
        token: "tok123".into(),
        backend: CacheBackendType::RedisHttp,
    };
    assert_eq!(config.url, "https://redis.example.com");
    assert_eq!(config.token, "tok123");
    assert_eq!(config.backend, CacheBackendType::RedisHttp);
}

#[test]
fn store_config_serde_roundtrip() {
    let config = StoreConfig {
        url: "https://store.example.com".into(),
        token: "secret-token".into(),
        backend: CacheBackendType::RedisHttp,
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: StoreConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.url, config.url);
    assert_eq!(parsed.token, config.token);
    assert_eq!(parsed.backend, CacheBackendType::RedisHttp);
}

// ─── HTTP KV Config ──────────────────────────────────────────────────────

#[test]
fn http_kv_config_construction() {
    let config = HttpKvConfig {
        base_url: "https://xxx.execute-api.us-east-1.amazonaws.com/prod".into(),
        auth_header: "x-api-key".into(),
        auth_value: "abc123".into(),
    };
    assert_eq!(
        config.base_url,
        "https://xxx.execute-api.us-east-1.amazonaws.com/prod"
    );
}

#[test]
fn http_kv_config_serde_roundtrip() {
    let config = HttpKvConfig {
        base_url: "https://api.example.com/kv".into(),
        auth_header: "Authorization".into(),
        auth_value: "Bearer my-token".into(),
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: HttpKvConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.base_url, "https://api.example.com/kv");
    assert_eq!(parsed.auth_header, "Authorization");
    assert_eq!(parsed.auth_value, "Bearer my-token");
}

// ─── Cloudflare KV Config ────────────────────────────────────────────────

#[cfg(feature = "cloudflare")]
#[test]
fn cloudflare_kv_config_construction() {
    use edgepack::config::CloudflareKvConfig;
    let config = CloudflareKvConfig {
        account_id: "abc123".into(),
        namespace_id: "ns-456".into(),
        api_token: "cf-token-789".into(),
        api_base_url: "https://api.cloudflare.com/client/v4".into(),
    };
    assert_eq!(config.account_id, "abc123");
    assert_eq!(config.namespace_id, "ns-456");
}

#[cfg(feature = "cloudflare")]
#[test]
fn cloudflare_kv_config_serde_roundtrip() {
    use edgepack::config::CloudflareKvConfig;
    let config = CloudflareKvConfig {
        account_id: "acc".into(),
        namespace_id: "ns".into(),
        api_token: "tok".into(),
        api_base_url: "https://api.cloudflare.com/client/v4".into(),
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: CloudflareKvConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.account_id, "acc");
    assert_eq!(parsed.api_base_url, "https://api.cloudflare.com/client/v4");
}

// ─── Cache Encryption Token ──────────────────────────────────────────────

#[test]
fn cache_encryption_token_defaults_to_store_token() {
    let config = make_config(CacheBackendType::RedisHttp);
    assert_eq!(config.cache_encryption_token(), "test-token");
}

#[test]
fn cache_encryption_token_same_for_all_backends() {
    let redis_config = make_config(CacheBackendType::RedisHttp);
    let http_kv_config = make_config(CacheBackendType::HttpKv);
    assert_eq!(
        redis_config.cache_encryption_token(),
        http_kv_config.cache_encryption_token()
    );
}

// ─── Create Backend ──────────────────────────────────────────────────────

#[test]
fn create_backend_redis_http() {
    let config = make_config(CacheBackendType::RedisHttp);
    let backend = edgepack::cache::create_backend(&config);
    assert!(backend.is_ok());
}

#[test]
fn create_backend_redis_tcp() {
    let config = make_config(CacheBackendType::RedisTcp);
    let backend = edgepack::cache::create_backend(&config);
    assert!(backend.is_ok());
}

#[test]
fn create_backend_http_kv_missing_config_errors() {
    // HttpKv backend requires http_kv config — None should error
    let config = make_config(CacheBackendType::HttpKv);
    let backend = edgepack::cache::create_backend(&config);
    assert!(backend.is_err());
    let err = match backend {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected error"),
    };
    assert!(err.contains("http_kv config required"));
}

#[test]
fn create_backend_http_kv_with_config() {
    let mut config = make_config(CacheBackendType::HttpKv);
    config.http_kv = Some(HttpKvConfig {
        base_url: "https://api.example.com/kv".into(),
        auth_header: "x-api-key".into(),
        auth_value: "secret".into(),
    });
    let backend = edgepack::cache::create_backend(&config);
    assert!(backend.is_ok());
}

#[cfg(feature = "cloudflare")]
#[test]
fn create_backend_cloudflare_kv_missing_config_errors() {
    let config = make_config(CacheBackendType::CloudflareKv);
    let backend = edgepack::cache::create_backend(&config);
    assert!(backend.is_err());
    let err = match backend {
        Err(e) => e.to_string(),
        Ok(_) => panic!("expected error"),
    };
    assert!(err.contains("cloudflare_kv config required"));
}

#[cfg(feature = "cloudflare")]
#[test]
fn create_backend_cloudflare_kv_with_config() {
    use edgepack::config::CloudflareKvConfig;
    let mut config = make_config(CacheBackendType::CloudflareKv);
    config.cloudflare_kv = Some(CloudflareKvConfig {
        account_id: "acc123".into(),
        namespace_id: "ns456".into(),
        api_token: "tok789".into(),
        api_base_url: "https://api.cloudflare.com/client/v4".into(),
    });
    let backend = edgepack::cache::create_backend(&config);
    assert!(backend.is_ok());
}

// ─── Backend Type Serialization ──────────────────────────────────────────

#[test]
fn cache_backend_type_serde_roundtrip_redis_http() {
    let t = CacheBackendType::RedisHttp;
    let json = serde_json::to_string(&t).unwrap();
    let parsed: CacheBackendType = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, CacheBackendType::RedisHttp);
}

#[test]
fn cache_backend_type_serde_roundtrip_redis_tcp() {
    let t = CacheBackendType::RedisTcp;
    let json = serde_json::to_string(&t).unwrap();
    let parsed: CacheBackendType = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, CacheBackendType::RedisTcp);
}

#[test]
fn cache_backend_type_serde_roundtrip_http_kv() {
    let t = CacheBackendType::HttpKv;
    let json = serde_json::to_string(&t).unwrap();
    let parsed: CacheBackendType = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, CacheBackendType::HttpKv);
}

#[cfg(feature = "cloudflare")]
#[test]
fn cache_backend_type_serde_roundtrip_cloudflare_kv() {
    let t = CacheBackendType::CloudflareKv;
    let json = serde_json::to_string(&t).unwrap();
    let parsed: CacheBackendType = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, CacheBackendType::CloudflareKv);
}

// ─── App Config with Backend Selection ───────────────────────────────────

#[test]
fn app_config_with_http_kv_serde_roundtrip() {
    let mut config = make_config(CacheBackendType::HttpKv);
    config.http_kv = Some(HttpKvConfig {
        base_url: "https://api.example.com/kv".into(),
        auth_header: "x-api-key".into(),
        auth_value: "secret123".into(),
    });
    let json = serde_json::to_string(&config).unwrap();
    let parsed: AppConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.store.backend, CacheBackendType::HttpKv);
    assert!(parsed.http_kv.is_some());
    assert_eq!(parsed.http_kv.unwrap().base_url, "https://api.example.com/kv");
}

#[test]
fn app_config_http_kv_optional_field_defaults_to_none() {
    // When http_kv field is missing from JSON, it should default to None
    let json = r#"{
        "store": {"url": "https://redis.test", "token": "tok", "backend": "RedisHttp"},
        "drm": {"speke_url": "https://speke.test/v2", "speke_auth": {"Bearer": "test"}, "system_ids": {"widevine": true, "playready": true}},
        "cache": {"vod_max_age": 31536000, "live_manifest_max_age": 1, "drm_key_ttl": 86400, "job_state_ttl": 172800}
    }"#;
    let parsed: AppConfig = serde_json::from_str(json).unwrap();
    assert!(parsed.http_kv.is_none());
}
