//! Integration tests: HTTP handler routing and request/response cycle.
//!
//! Tests the full HTTP request flow:
//! - Route matching for all endpoints
//! - Webhook payload validation
//! - Error responses for invalid inputs
//! - Cache-Control header correctness

mod common;

use edge_packager::handler::{route, HttpMethod, HttpRequest, HttpResponse};

// ─── Health Check ───────────────────────────────────────────────────

#[test]
fn health_check_returns_200_ok() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/health".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"ok");
}

#[test]
fn health_check_with_trailing_slash() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/health/".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 200);
}

// ─── Manifest Routing ───────────────────────────────────────────────

#[test]
fn manifest_request_routes_correctly_hls() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    // Should return an error since no content is cached, but the route should match
    let result = route(&req);
    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("manifest not found") || err.contains("not found"),
        "error should indicate manifest not found, got: {err}"
    );
}

#[test]
fn manifest_request_routes_correctly_dash() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/dash/manifest".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req);
    assert!(result.is_err());
}

#[test]
fn manifest_request_invalid_format() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/mp4/manifest".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("unknown format"),
        "should reject invalid format"
    );
}

// ─── Init Segment Routing ───────────────────────────────────────────

#[test]
fn init_segment_request_routes_correctly() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/init.mp4".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("init segment"),
        "should indicate init segment not found"
    );
}

// ─── Media Segment Routing ──────────────────────────────────────────

#[test]
fn media_segment_request_segment_0() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/segment_0.cmfv".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("segment 0"),
        "should indicate segment 0 not found"
    );
}

#[test]
fn media_segment_request_segment_42() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/dash/segment_42.cmfv".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req);
    assert!(result.is_err());
    assert!(
        result.unwrap_err().to_string().contains("segment 42"),
        "should indicate segment 42 not found"
    );
}

#[test]
fn media_segment_request_invalid_filename() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/movie-123/hls/invalid_file.xyz".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 404, "invalid segment filename should return 404");
}

// ─── Status Routing ─────────────────────────────────────────────────

#[test]
fn status_request_routes_correctly() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/status/movie-123/hls".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req);
    assert!(result.is_err());
    let err_msg = result.unwrap_err().to_string();
    assert!(
        err_msg.contains("no job found") || err_msg.contains("not found"),
        "should indicate no job found, got: {err_msg}"
    );
}

// ─── Webhook Routing ────────────────────────────────────────────────

#[test]
fn webhook_repackage_valid_hls_payload() {
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

    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 202, "valid webhook should return 202 Accepted");

    let resp_body: serde_json::Value = serde_json::from_slice(&resp.body)
        .expect("response body should be valid JSON");
    assert!(
        resp_body.get("content_id").is_some(),
        "response should contain content_id"
    );
}

#[test]
fn webhook_repackage_valid_dash_payload() {
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

    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 202);
}

#[test]
fn webhook_repackage_with_key_ids() {
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

    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 202);
}

#[test]
fn webhook_repackage_missing_body() {
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: None,
    };

    // The handler returns Err(InvalidInput) for missing body
    let result = route(&req);
    assert!(result.is_err(), "missing body should return an error");
    assert!(
        result.unwrap_err().to_string().contains("missing request body"),
        "error should mention missing body"
    );
}

#[test]
fn webhook_repackage_invalid_json() {
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/webhook/repackage".into(),
        headers: vec![],
        body: Some(b"not json{".to_vec()),
    };

    // The handler returns Err(InvalidInput) for invalid JSON
    let result = route(&req);
    assert!(result.is_err(), "invalid JSON should return an error");
    assert!(
        result.unwrap_err().to_string().contains("invalid JSON"),
        "error should mention invalid JSON"
    );
}

#[test]
fn webhook_repackage_missing_required_fields() {
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

    // The handler returns Err(InvalidInput) for missing required fields
    let result = route(&req);
    assert!(result.is_err(), "missing content_id should return an error");
    assert!(
        result.unwrap_err().to_string().contains("content_id"),
        "error should mention missing content_id"
    );
}

// ─── Unknown Routes ─────────────────────────────────────────────────

#[test]
fn unknown_path_returns_404() {
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/unknown/path".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 404);
    assert_eq!(resp.body, b"not found");
}

#[test]
fn wrong_method_returns_404() {
    // POST to health should not match
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/health".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn options_method_returns_404() {
    let req = HttpRequest {
        method: HttpMethod::Options,
        path: "/repackage/movie-123/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req).unwrap();
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
    // Content IDs with hyphens should work
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/my-movie-123-hd/hls/manifest".into(),
        headers: vec![],
        body: None,
    };
    let result = route(&req);
    assert!(
        result.is_err(),
        "should route correctly even with hyphenated content IDs"
    );
}

#[test]
fn multiple_format_requests_for_same_content() {
    // Verify both HLS and DASH routes work for the same content
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

    let hls_result = route(&hls_req);
    let dash_result = route(&dash_req);

    // Both should route correctly (both return errors since no content exists)
    assert!(hls_result.is_err());
    assert!(dash_result.is_err());
}

#[test]
fn sequential_segment_requests() {
    // Simulate a player requesting segments in order
    for i in 0..5 {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: format!("/repackage/movie-1/hls/segment_{i}.cmfv"),
            headers: vec![],
            body: None,
        };
        let result = route(&req);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains(&format!("segment {i}")),
            "should indicate segment {i} not found"
        );
    }
}
