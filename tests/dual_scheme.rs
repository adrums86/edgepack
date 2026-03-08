//! Integration tests: Phase 4 — Dual-Scheme Output.
//!
//! Tests scheme-qualified routing, cache key generation, source config payload
//! parsing with multiple target schemes, and backward compatibility.

mod common;

use edgepack::cache::CacheKeys;
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::handler::{route, HandlerContext, HttpMethod, HttpRequest};
use edgepack::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, JitConfig, PolicyConfig, SpekeAuth,
};

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
            policy: PolicyConfig::default(),
        },
    }
}

// ─── Scheme-Qualified Route Parsing ───────────────────────────────────

#[test]
fn route_manifest_hls_cenc() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/ds-mfst-hls-cenc/hls_cenc/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404); // not found in cache
}

#[test]
fn route_manifest_dash_cbcs() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/ds-mfst-dash-cbcs/dash_cbcs/manifest".into(),
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
        path: "/repackage/ds-init-hls-cenc/hls_cenc/init.mp4".into(),
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
        path: "/repackage/ds-seg-dash-cbcs/dash_cbcs/segment_0.cmfv".into(),
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
        path: "/repackage/ds-seg-hls-none/hls_none/segment_3.m4s".into(),
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
        path: "/repackage/ds-invalid-scheme/hls_aes256/manifest".into(),
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
        path: "/repackage/ds-compat-hls/hls/manifest".into(),
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
        path: "/repackage/ds-compat-dash/dash/segment_0.cmfv".into(),
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
