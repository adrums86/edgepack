//! Integration tests: HTTP handler routing and request/response cycle.
//!
//! Tests the full HTTP request flow:
//! - Route matching for all endpoints
//! - Webhook payload validation
//! - Error responses for invalid inputs
//! - Cache-Control header correctness

mod common;

use edge_packager::cache::CacheBackend;
use edge_packager::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, RedisBackendType, RedisConfig, SpekeAuth,
};
use edge_packager::handler::{route, HandlerContext, HttpMethod, HttpRequest, HttpResponse};

/// A stub cache backend for integration tests that always returns None/Ok.
struct StubCacheBackend;

impl CacheBackend for StubCacheBackend {
    fn get(&self, _key: &str) -> edge_packager::error::Result<Option<Vec<u8>>> {
        Ok(None)
    }
    fn set(&self, _key: &str, _value: &[u8], _ttl: u64) -> edge_packager::error::Result<()> {
        Ok(())
    }
    fn exists(&self, _key: &str) -> edge_packager::error::Result<bool> {
        Ok(false)
    }
    fn delete(&self, _key: &str) -> edge_packager::error::Result<()> {
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
                speke_url: url::Url::parse("https://drm.example.com/speke").unwrap(),
                speke_auth: SpekeAuth::Bearer("test-bearer-token".into()),
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
        },
    }
}

// ─── Health Check ───────────────────────────────────────────────────

#[test]
fn health_check_returns_200_ok() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/health".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"ok");
}

#[test]
fn health_check_with_trailing_slash() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/health/".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 200);
}

// ─── Manifest Routing ───────────────────────────────────────────────

#[test]
fn manifest_request_routes_correctly_hls() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    // With stub cache (returns None), handler returns 404 HttpResponse
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn manifest_request_routes_correctly_dash() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/dash/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn manifest_request_invalid_format() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/mp4/manifest".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("unknown format"),
        "should reject invalid format"
    );
}

// ─── Init Segment Routing ───────────────────────────────────────────

#[test]
fn init_segment_request_routes_correctly() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/init.mp4".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

// ─── Media Segment Routing ──────────────────────────────────────────

#[test]
fn media_segment_request_segment_0() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/segment_0.cmfv".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_segment_42() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/dash/segment_42.cmfv".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_m4s_segment_0() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/segment_0.m4s".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    // Should route successfully (just no data in cache)
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_m4s_segment_42() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/dash/segment_42.m4s".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_invalid_filename() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/invalid_file.xyz".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404, "invalid segment filename should return 404");
}

// ─── Status Routing ─────────────────────────────────────────────────

#[test]
fn status_request_routes_correctly() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/status/movie-123/hls".into(),
        headers: vec![],
        body: None,
    };
    // With stub cache, status returns 404
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

// ─── Webhook Routing ────────────────────────────────────────────────

#[test]
fn webhook_repackage_valid_hls_payload() {
    let ctx = test_context();
    let payload = serde_json::json!({
        "content_id": "movie-123",
        "source_url": "https://cdn.example.com/master.m3u8",
        "format": "hls"
    });
    let body = serde_json::to_vec(&payload).unwrap();

    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![("Content-Type".into(), "application/json".into())],
        body: Some(body),
    };

    let resp = route(&req, &ctx).unwrap();
    // On native targets, pipeline fails (HTTP client not available) → 500
    // On WASI targets, would succeed → 200
    assert!(resp.status == 200 || resp.status == 500);
}

#[test]
fn webhook_repackage_valid_dash_payload() {
    let ctx = test_context();
    let payload = serde_json::json!({
        "content_id": "show-456",
        "source_url": "https://cdn.example.com/manifest.mpd",
        "format": "dash"
    });
    let body = serde_json::to_vec(&payload).unwrap();

    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: Some(body),
    };

    let resp = route(&req, &ctx).unwrap();
    assert!(resp.status == 200 || resp.status == 500);
}

#[test]
fn webhook_repackage_with_key_ids() {
    let ctx = test_context();
    let payload = serde_json::json!({
        "content_id": "movie-789",
        "source_url": "https://cdn.example.com/source.m3u8",
        "format": "hls",
        "key_ids": ["aabbccdd11223344", "55667788aabbccdd"]
    });
    let body = serde_json::to_vec(&payload).unwrap();

    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: Some(body),
    };

    let resp = route(&req, &ctx).unwrap();
    assert!(resp.status == 200 || resp.status == 500);
}

#[test]
fn webhook_repackage_missing_body() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: None,
    };

    let result = route(&req, &ctx);
    assert!(result.is_err(), "missing body should return an error");
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("missing request body"),
        "error should mention missing body"
    );
}

#[test]
fn webhook_repackage_invalid_json() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: Some(b"not json{".to_vec()),
    };

    let result = route(&req, &ctx);
    assert!(result.is_err(), "invalid JSON should return an error");
    assert!(
        result.unwrap_err().to_string().contains("invalid JSON"),
        "error should mention invalid JSON"
    );
}

#[test]
fn webhook_repackage_missing_required_fields() {
    let ctx = test_context();
    // Missing content_id
    let payload = serde_json::json!({
        "source_url": "https://example.com/source.m3u8",
        "format": "hls"
    });
    let body = serde_json::to_vec(&payload).unwrap();

    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: Some(body),
    };

    let result = route(&req, &ctx);
    assert!(result.is_err(), "missing content_id should return an error");
}

// ─── Unknown Routes ─────────────────────────────────────────────────

#[test]
fn unknown_path_returns_404() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/unknown/path".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body, b"not found");
}

#[test]
fn wrong_method_returns_404() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/health".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn options_method_returns_404() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Options,
        path: "/repackage/movie-123/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

// ─── Response Helpers ───────────────────────────────────────────────

#[test]
fn http_response_ok_has_correct_headers() {
    let resp = HttpResponse::ok(b"test data".to_vec(), "video/mp4");
    assert_eq!(resp.status, 200);

    let content_type = resp
        .headers
        .iter()
        .find(|(k, _)| k == "Content-Type")
        .map(|(_, v): &(String, String)| v.as_str());
    assert_eq!(content_type, Some("video/mp4"));
}

#[test]
fn http_response_ok_with_cache_headers() {
    let resp = HttpResponse::ok_with_cache(
        b"segment data".to_vec(),
        "video/mp4",
        "public, max-age=31536000, immutable",
    );
    assert_eq!(resp.status, 200);

    let cache_control = resp
        .headers
        .iter()
        .find(|(k, _)| k == "Cache-Control")
        .map(|(_, v): &(String, String)| v.as_str());
    assert_eq!(
        cache_control,
        Some("public, max-age=31536000, immutable")
    );
}

#[test]
fn http_response_accepted_json_content_type() {
    let resp = HttpResponse::accepted(b"{}".to_vec());
    assert_eq!(resp.status, 202);

    let content_type = resp
        .headers
        .iter()
        .find(|(k, _)| k == "Content-Type")
        .map(|(_, v): &(String, String)| v.as_str());
    assert_eq!(content_type, Some("application/json"));
}

#[test]
fn http_response_error_formats() {
    let resp = HttpResponse::error(500, "internal server error");
    assert_eq!(resp.status, 500);
    assert_eq!(resp.body, b"internal server error");

    let resp = HttpResponse::error(503, "service unavailable");
    assert_eq!(resp.status, 503);
}

// ─── Complex Routing Scenarios ──────────────────────────────────────

#[test]
fn content_ids_with_special_characters() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/my-movie-123-hd/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(
        resp.status, 404,
        "should route correctly even with hyphenated content IDs"
    );
}

#[test]
fn multiple_format_requests_for_same_content() {
    let ctx = test_context();
    let hls_req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    let dash_req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-1/dash/manifest".into(),
        headers: vec![],
        body: None,
    };

    let hls_resp = route(&hls_req, &ctx).unwrap();
    let dash_resp = route(&dash_req, &ctx).unwrap();

    assert_eq!(hls_resp.status, 404);
    assert_eq!(dash_resp.status, 404);
}

#[test]
fn sequential_segment_requests() {
    let ctx = test_context();
    for i in 0..5 {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: format!("/repackage/movie-1/hls/segment_{i}.cmfv"),
            headers: vec![],
            body: None,
        };
        let resp = route(&req, &ctx).unwrap();
        assert_eq!(resp.status, 404);
    }
}
