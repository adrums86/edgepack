//! Integration tests: Phase 4 — Dual-Scheme Output.
//!
//! Tests scheme-qualified routing, cache key generation, webhook payload
//! parsing with multiple target schemes, and backward compatibility.

mod common;

use edgepack::cache::CacheKeys;
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::handler::{route, HandlerContext, HttpMethod, HttpRequest};
use edgepack::handler::webhook::{WebhookPayload, WebhookResponse};

use edgepack::cache::CacheBackend;
use edgepack::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, RedisBackendType, RedisConfig, SpekeAuth,
};

/// A stub cache backend for integration tests that always returns None/Ok.
struct StubCacheBackend;

impl CacheBackend for StubCacheBackend {
    fn get(&self, _key: &str) -> edgepack::error::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn set(&self, _key: &str, _value: &[u8], _ttl: u64) -> edgepack::error::Result<()> {
        Ok(())
    }
    fn exists(&self, _key: &str) -> edgepack::error::Result<bool> {
        Ok(false)
    }
    fn delete(&self, _key: &str) -> edgepack::error::Result<()> {
        Ok(())
    }
}

fn test_context() -> HandlerContext {
    HandlerContext {
        cache: Box::new(StubCacheBackend),
        config: AppConfig {
            redis: RedisConfig {
                url: "https://test-redis.example.com".into(),
                token: "test-token".into(),
                backend: RedisBackendType::Http,
            },
            drm: DrmConfig {
                speke_url: edgepack::url::Url::parse("https://drm.example.com/speke").unwrap(),
                speke_auth: SpekeAuth::Bearer("test-bearer-token".into()),
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
        },
    }
}

// ─── Scheme-Qualified Route Parsing ───────────────────────────────────

#[test]
fn route_manifest_hls_cenc() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/hls_cenc/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404); // stub cache → not found
}

#[test]
fn route_manifest_dash_cbcs() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/dash_cbcs/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn route_init_segment_hls_cenc() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/hls_cenc/init.mp4".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn route_media_segment_dash_cbcs() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/dash_cbcs/segment_0.cmfv".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn route_media_segment_hls_none() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/hls_none/segment_3.m4s".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn route_invalid_scheme_in_format() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/hls_aes256/manifest".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req, &ctx);
    assert!(result.is_err());
}

#[test]
fn route_backward_compat_plain_hls() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404); // routes correctly, no data in cache
}

#[test]
fn route_backward_compat_plain_dash() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/dash/segment_0.cmfv".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

// ─── Scheme-Qualified Cache Keys ──────────────────────────────────────

#[test]
fn cache_keys_scheme_qualified_different_from_plain() {
    let plain_init = CacheKeys::init_segment("abc", "hls");
    let cenc_init = CacheKeys::init_segment_for_scheme("abc", "hls", "cenc");
    let cbcs_init = CacheKeys::init_segment_for_scheme("abc", "hls", "cbcs");
    assert_ne!(plain_init, cenc_init);
    assert_ne!(plain_init, cbcs_init);
    assert_ne!(cenc_init, cbcs_init);
}

#[test]
fn cache_keys_manifest_state_per_scheme() {
    let cenc = CacheKeys::manifest_state_for_scheme("movie", "hls", "cenc");
    let cbcs = CacheKeys::manifest_state_for_scheme("movie", "hls", "cbcs");
    assert_ne!(cenc, cbcs);
    assert!(cenc.contains("hls_cenc"));
    assert!(cbcs.contains("hls_cbcs"));
}

#[test]
fn cache_keys_media_segment_per_scheme() {
    let cenc = CacheKeys::media_segment_for_scheme("movie", "dash", "cenc", 5);
    let cbcs = CacheKeys::media_segment_for_scheme("movie", "dash", "cbcs", 5);
    assert_ne!(cenc, cbcs);
    assert!(cenc.contains("dash_cenc:seg:5"));
    assert!(cbcs.contains("dash_cbcs:seg:5"));
}

#[test]
fn cache_keys_rewrite_params_per_scheme() {
    let cenc = CacheKeys::rewrite_params_for_scheme("id", "hls", "cenc");
    let cbcs = CacheKeys::rewrite_params_for_scheme("id", "hls", "cbcs");
    assert_ne!(cenc, cbcs);
    assert!(cenc.contains("hls_cenc"));
    assert!(cbcs.contains("hls_cbcs"));
}

#[test]
fn cache_keys_target_schemes() {
    let key = CacheKeys::target_schemes("abc", "hls");
    assert_eq!(key, "ep:abc:hls:target_schemes");
}

// ─── Webhook Payload Parsing ──────────────────────────────────────────

#[test]
fn webhook_payload_multi_scheme_parsing() {
    let json = r#"{
        "content_id": "movie-1",
        "source_url": "https://cdn.example.com/manifest.m3u8",
        "format": "hls",
        "target_schemes": ["cenc", "cbcs"]
    }"#;
    let payload: WebhookPayload = serde_json::from_str(json).unwrap();
    assert_eq!(payload.resolved_target_schemes(), vec!["cenc", "cbcs"]);
}

#[test]
fn webhook_payload_single_scheme_backward_compat() {
    let json = r#"{
        "content_id": "movie-1",
        "source_url": "https://cdn.example.com/manifest.m3u8",
        "format": "hls",
        "target_scheme": "cbcs"
    }"#;
    let payload: WebhookPayload = serde_json::from_str(json).unwrap();
    assert_eq!(payload.resolved_target_schemes(), vec!["cbcs"]);
}

#[test]
fn webhook_payload_default_scheme() {
    let json = r#"{
        "content_id": "movie-1",
        "source_url": "https://cdn.example.com/manifest.m3u8",
        "format": "hls"
    }"#;
    let payload: WebhookPayload = serde_json::from_str(json).unwrap();
    assert_eq!(payload.resolved_target_schemes(), vec!["cenc"]);
}

#[test]
fn webhook_payload_target_schemes_takes_precedence() {
    let json = r#"{
        "content_id": "movie-1",
        "source_url": "https://cdn.example.com/manifest.m3u8",
        "format": "hls",
        "target_scheme": "none",
        "target_schemes": ["cenc", "cbcs"]
    }"#;
    let payload: WebhookPayload = serde_json::from_str(json).unwrap();
    assert_eq!(payload.resolved_target_schemes(), vec!["cenc", "cbcs"]);
}

#[test]
fn webhook_response_manifest_urls_per_scheme() {
    let mut manifest_urls = std::collections::HashMap::new();
    manifest_urls.insert("cenc".into(), "/repackage/movie-1/hls_cenc/manifest".into());
    manifest_urls.insert("cbcs".into(), "/repackage/movie-1/hls_cbcs/manifest".into());

    let resp = WebhookResponse {
        status: "processing".into(),
        content_id: "movie-1".into(),
        format: "hls".into(),
        manifest_urls,
        segments_completed: 1,
        segments_total: Some(10),
    };

    let json = serde_json::to_string(&resp).unwrap();
    let parsed: WebhookResponse = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.manifest_urls.len(), 2);
    assert!(parsed.manifest_urls.get("cenc").unwrap().contains("hls_cenc"));
    assert!(parsed.manifest_urls.get("cbcs").unwrap().contains("hls_cbcs"));
}

#[test]
fn webhook_duplicate_scheme_rejected() {
    let ctx = test_context();
    let payload = serde_json::json!({
        "content_id": "test",
        "source_url": "https://example.com/source.m3u8",
        "format": "hls",
        "target_schemes": ["cenc", "cenc"]
    });
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: Some(serde_json::to_vec(&payload).unwrap()),
    };
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("duplicate"));
}

#[test]
fn webhook_invalid_scheme_in_array_rejected() {
    let ctx = test_context();
    let payload = serde_json::json!({
        "content_id": "test",
        "source_url": "https://example.com/source.m3u8",
        "format": "hls",
        "target_schemes": ["cenc", "aes256"]
    });
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: Some(serde_json::to_vec(&payload).unwrap()),
    };
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("invalid target_scheme"));
}

// ─── EncryptionScheme from_str_value ──────────────────────────────────

#[test]
fn encryption_scheme_from_str_value_roundtrip() {
    for scheme in [EncryptionScheme::Cenc, EncryptionScheme::Cbcs, EncryptionScheme::None] {
        let s = scheme.scheme_type_str();
        let parsed = EncryptionScheme::from_str_value(s).unwrap();
        assert_eq!(parsed, scheme);
    }
}

#[test]
fn encryption_scheme_from_str_value_invalid() {
    assert!(EncryptionScheme::from_str_value("aes256").is_none());
    assert!(EncryptionScheme::from_str_value("CENC").is_none());
    assert!(EncryptionScheme::from_str_value("").is_none());
}
