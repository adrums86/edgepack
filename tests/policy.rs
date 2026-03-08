//! Integration tests: Runtime Policy Controls.
//!
//! Tests fail-closed allowlist enforcement at route level for
//! output formats, encryption schemes, and container formats.

mod common;

use edgepack::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, JitConfig, PolicyConfig, SpekeAuth,
};
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::handler::{route, HandlerContext, HttpMethod, HttpRequest};
use edgepack::manifest::types::OutputFormat;
use edgepack::media::container::ContainerFormat;

fn test_config_with_policy(policy: PolicyConfig) -> AppConfig {
    AppConfig {
        drm: DrmConfig {
            speke_url: edgepack::url::Url::parse("https://drm.example.com/speke").unwrap(),
            speke_auth: SpekeAuth::Bearer("test-bearer-token".into()),
            system_ids: DrmSystemIds::default(),
        },
        cache: CacheConfig::default(),
        jit: JitConfig::default(),
        policy,
    }
}

fn test_context_with_policy(policy: PolicyConfig) -> HandlerContext {
    HandlerContext {
        config: test_config_with_policy(policy),
    }
}

fn get_request(path: &str) -> HttpRequest {
    HttpRequest {
        method: HttpMethod::Get,
        path: path.to_string(),
        headers: vec![],
        body: None,
    }
}

// ─── Default Policy (backward compat) ──────────────────────────────────

#[test]
fn default_policy_allows_hls() {
    let ctx = test_context_with_policy(PolicyConfig::default());
    let req = get_request("/repackage/pol-dflt-hls/hls/manifest");
    // 404 = format was allowed, just content not found
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn default_policy_allows_dash() {
    let ctx = test_context_with_policy(PolicyConfig::default());
    let req = get_request("/repackage/pol-dflt-dash/dash/manifest");
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn default_policy_allows_all_schemes() {
    let ctx = test_context_with_policy(PolicyConfig::default());
    for scheme in &["cenc", "cbcs", "none"] {
        let req = get_request(&format!("/repackage/pol-dflt-sch/hls_{scheme}/manifest"));
        let resp = route(&req, &ctx).unwrap();
        assert_eq!(resp.status, 404, "expected 404 for scheme {scheme}");
    }
}

// ─── Format Policy ─────────────────────────────────────────────────────

#[test]
fn format_policy_hls_only_allows_hls() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Hls]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-hls/hls/manifest");
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn format_policy_hls_only_denies_dash() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Hls]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-dash/dash/manifest");
    let result = route(&req, &ctx);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("forbidden"));
    assert!(err.to_string().contains("dash"));
}

#[test]
fn format_policy_dash_only_allows_dash() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Dash]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-dash2/dash/manifest");
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn format_policy_dash_only_denies_hls() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Dash]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-hls2/hls/manifest");
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("forbidden"));
}

#[test]
fn format_policy_both_allows_both() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Hls, OutputFormat::Dash]),
        ..Default::default()
    });
    let req_hls = get_request("/repackage/pol-fmt-both-h/hls/manifest");
    let req_dash = get_request("/repackage/pol-fmt-both-d/dash/manifest");
    assert_eq!(route(&req_hls, &ctx).unwrap().status, 404);
    assert_eq!(route(&req_dash, &ctx).unwrap().status, 404);
}

#[test]
fn format_policy_denies_init_segment() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Dash]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-init/hls/init.mp4");
    assert!(route(&req, &ctx).is_err());
}

#[test]
fn format_policy_denies_media_segment() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Dash]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-seg/hls/segment_0.cmfv");
    assert!(route(&req, &ctx).is_err());
}

#[test]
fn format_policy_denies_iframes() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Dash]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-if/hls/iframes");
    assert!(route(&req, &ctx).is_err());
}

#[test]
fn format_policy_denies_key() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Dash]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-fmt-key/hls/key");
    assert!(route(&req, &ctx).is_err());
}

// ─── Scheme Policy ──────────────────────────────────────────────────────

#[test]
fn scheme_policy_cenc_only_allows_cenc() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cenc]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-sch-cenc/hls_cenc/manifest");
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn scheme_policy_cenc_only_denies_cbcs() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cenc]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-sch-cbcs/hls_cbcs/manifest");
    let result = route(&req, &ctx);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("forbidden"));
    assert!(err.to_string().contains("cbcs"));
}

#[test]
fn scheme_policy_cenc_only_denies_none() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cenc]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-sch-none/dash_none/manifest");
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("none"));
}

#[test]
fn scheme_policy_denies_clear_content() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-sch-noclr/hls_none/manifest");
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("forbidden"));
}

#[test]
fn scheme_policy_allows_unqualified_format() {
    // Unqualified format (no scheme in URL) should pass route-level check
    // because the scheme isn't determined yet at route level.
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cenc]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-sch-unq/hls/manifest");
    // Passes route level (scheme not in URL), 404 because no content
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn scheme_policy_denies_init_segment() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cbcs]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-sch-init/dash_cenc/init.mp4");
    assert!(route(&req, &ctx).is_err());
}

#[test]
fn scheme_policy_denies_media_segment() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cbcs]),
        ..Default::default()
    });
    let req = get_request("/repackage/pol-sch-seg/hls_cenc/segment_5.cmfv");
    assert!(route(&req, &ctx).is_err());
}

// ─── Combined Policy ───────────────────────────────────────────────────

#[test]
fn combined_policy_hls_cenc_only() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Hls]),
        allowed_schemes: Some(vec![EncryptionScheme::Cenc]),
        ..Default::default()
    });

    // HLS + CENC = allowed
    let req = get_request("/repackage/pol-cmb-ok/hls_cenc/manifest");
    assert_eq!(route(&req, &ctx).unwrap().status, 404);

    // DASH + CENC = format denied
    let req = get_request("/repackage/pol-cmb-d1/dash_cenc/manifest");
    assert!(route(&req, &ctx).is_err());

    // HLS + CBCS = scheme denied
    let req = get_request("/repackage/pol-cmb-d2/hls_cbcs/manifest");
    assert!(route(&req, &ctx).is_err());

    // DASH + CBCS = both denied (format checked first)
    let req = get_request("/repackage/pol-cmb-d3/dash_cbcs/manifest");
    assert!(route(&req, &ctx).is_err());
}

// ─── Full Lockdown ─────────────────────────────────────────────────────

#[test]
fn empty_allowlists_deny_everything() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![]),
        allowed_schemes: Some(vec![]),
        allowed_containers: Some(vec![]),
    });

    let req = get_request("/repackage/pol-lock-1/hls/manifest");
    assert!(route(&req, &ctx).is_err());

    let req = get_request("/repackage/pol-lock-2/dash_cenc/manifest");
    assert!(route(&req, &ctx).is_err());

    let req = get_request("/repackage/pol-lock-3/hls_cbcs/init.mp4");
    assert!(route(&req, &ctx).is_err());
}

// ─── Health check is never blocked by policy ────────────────────────────

#[test]
fn health_check_unaffected_by_policy() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![]),
        allowed_schemes: Some(vec![]),
        allowed_containers: Some(vec![]),
    });
    let req = get_request("/health");
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 200);
    assert_eq!(resp.body, b"ok");
}

// ─── Source config registration unaffected by policy ────────────────────

#[test]
fn source_config_registration_unaffected_by_policy() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![]),
        allowed_schemes: Some(vec![]),
        allowed_containers: Some(vec![]),
    });
    let payload = serde_json::json!({
        "content_id": "pol-src-reg",
        "source_url": "https://origin.example.com/manifest.m3u8"
    });
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/config/source".to_string(),
        headers: vec![],
        body: Some(serde_json::to_vec(&payload).unwrap()),
    };
    // Source config POST should succeed even under full lockdown
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 200);
}

// ─── Serde Backward Compatibility ───────────────────────────────────────

#[test]
fn app_config_without_policy_deserializes() {
    let json = r#"{
        "drm": {"speke_url": "https://speke.test/v2", "speke_auth": {"Bearer": "t"}, "system_ids": {"widevine": true, "playready": true}},
        "cache": {"vod_max_age": 31536000, "live_manifest_max_age": 1}
    }"#;
    let config: AppConfig = serde_json::from_str(json).unwrap();
    assert!(config.policy.allowed_schemes.is_none());
    assert!(config.policy.allowed_formats.is_none());
    assert!(config.policy.allowed_containers.is_none());
}

#[test]
fn policy_config_serde_roundtrip() {
    let policy = PolicyConfig {
        allowed_schemes: Some(vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs]),
        allowed_formats: Some(vec![OutputFormat::Hls]),
        allowed_containers: Some(vec![ContainerFormat::Cmaf]),
    };
    let json = serde_json::to_string(&policy).unwrap();
    let parsed: PolicyConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.allowed_schemes.as_ref().unwrap().len(), 2);
    assert_eq!(parsed.allowed_formats.as_ref().unwrap().len(), 1);
    assert_eq!(parsed.allowed_containers.as_ref().unwrap().len(), 1);
}

#[test]
fn policy_config_none_vs_empty_semantics() {
    // None = no restriction
    let none_policy = PolicyConfig::default();
    assert!(none_policy.check_scheme(&EncryptionScheme::Cenc).is_ok());
    assert!(none_policy.check_format(&OutputFormat::Dash).is_ok());
    assert!(none_policy.check_container(&ContainerFormat::Fmp4).is_ok());

    // Empty = full lockdown
    let empty_policy = PolicyConfig {
        allowed_schemes: Some(vec![]),
        allowed_formats: Some(vec![]),
        allowed_containers: Some(vec![]),
    };
    assert!(empty_policy.check_scheme(&EncryptionScheme::Cenc).is_err());
    assert!(empty_policy.check_format(&OutputFormat::Dash).is_err());
    assert!(empty_policy.check_container(&ContainerFormat::Fmp4).is_err());
}

// ─── Container Format Policy ────────────────────────────────────────────

#[test]
fn container_policy_check_methods() {
    let policy = PolicyConfig {
        allowed_containers: Some(vec![ContainerFormat::Cmaf]),
        ..Default::default()
    };
    assert!(policy.check_container(&ContainerFormat::Cmaf).is_ok());
    assert!(policy.check_container(&ContainerFormat::Fmp4).is_err());
    assert!(policy.check_container(&ContainerFormat::Iso).is_err());
}

// ─── Multiple segment extensions are equally enforced ────────────────────

#[test]
fn format_policy_denies_all_segment_extensions() {
    let ctx = test_context_with_policy(PolicyConfig {
        allowed_formats: Some(vec![OutputFormat::Dash]),
        ..Default::default()
    });

    for ext in &["cmfv", "cmfa", "cmft", "cmfm", "m4s", "mp4", "m4a"] {
        let path = format!("/repackage/pol-ext-{ext}/hls/segment_0.{ext}");
        let req = get_request(&path);
        let result = route(&req, &ctx);
        assert!(result.is_err(), "expected error for extension .{ext}");
    }
}
