//! Integration tests: HTTP handler routing and request/response cycle.
//!
//! Tests the full HTTP request flow:
//! - Route matching for all endpoints
//! - Webhook payload validation
//! - Error responses for invalid inputs
//! - Cache-Control header correctness

mod common;

use edgepack::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, JitConfig, SpekeAuth,
};
use edgepack::handler::{route, HandlerContext, HttpMethod, HttpRequest, HttpResponse};

fn test_context() -> HandlerContext {
    HandlerContext {
        config: AppConfig {
            drm: DrmConfig {
                speke_url: edgepack::url::Url::parse("https://drm.example.com/speke").unwrap(),
                speke_auth: SpekeAuth::Bearer("test-bearer-token".into()),
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
            jit: JitConfig::default(),
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
        path: "/repackage/hi-hls-mfst/hls/manifest".into(),
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
        path: "/repackage/hi-dash-mfst/dash/manifest".into(),
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
        path: "/repackage/hi-mp4-mfst/mp4/manifest".into(),
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
        path: "/repackage/hi-hls-init/hls/init.mp4".into(),
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
        path: "/repackage/hi-hls-cmfv0/hls/segment_0.cmfv".into(),
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
        path: "/repackage/hi-dash-cmfv42/dash/segment_42.cmfv".into(),
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
        path: "/repackage/hi-hls-m4s0/hls/segment_0.m4s".into(),
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
        path: "/repackage/hi-dash-m4s42/dash/segment_42.m4s".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_mp4_segment_0() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/hi-hls-mp40/hls/segment_0.mp4".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    // Should route successfully (just no data in cache)
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_mp4_segment_42() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/hi-dash-mp442/dash/segment_42.mp4".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_cmfa_segment_0() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/hi-hls-cmfa0/hls/segment_0.cmfa".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    // CMAF audio segment routes correctly — just no data in cache
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_m4a_segment_0() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/hi-hls-m4a0/hls/segment_0.m4a".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    // ISOBMFF audio segment routes correctly — just no data in cache
    assert_eq!(resp.status, 404);
}

#[test]
fn media_segment_request_invalid_filename() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/hi-hls-invalid/hls/invalid_file.xyz".into(),
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
        path: "/status/hi-status/hls".into(),
        headers: vec![],
        body: None,
    };
    // With stub cache, status returns 404
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
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
        path: "/repackage/hi-hls-mfst/hls/manifest".into(),
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
        path: "/repackage/hi-hyphen-id/hls/manifest".into(),
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
        path: "/repackage/hi-multi-fmt/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    let dash_req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/hi-multi-fmt/dash/manifest".into(),
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
            path: format!("/repackage/hi-seq-seg-{i}/hls/segment_{i}.cmfv"),
            headers: vec![],
            body: None,
        };
        let resp = route(&req, &ctx).unwrap();
        assert_eq!(resp.status, 404);
    }
}
