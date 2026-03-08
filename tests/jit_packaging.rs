//! Integration tests: JIT Packaging.
//!
//! Tests source config storage/retrieval, JIT route handling,
//! request coalescing lock behavior, and backward compatibility.

mod common;

use edgepack::cache::{CacheBackend, CacheKeys};
use edgepack::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, JitConfig, PolicyConfig, SpekeAuth,
};
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::handler::{route, HandlerContext, HttpMethod, HttpRequest};
use edgepack::media::container::ContainerFormat;
use edgepack::repackager::SourceConfig;

fn test_config() -> AppConfig {
    AppConfig {
        drm: DrmConfig {
            speke_url: edgepack::url::Url::parse("https://drm.example.com/speke").unwrap(),
            speke_auth: SpekeAuth::Bearer("test-bearer-token".into()),
            system_ids: DrmSystemIds::default(),
        },
        cache: CacheConfig::default(),
        jit: JitConfig::default(),
        policy: PolicyConfig::default(),
    }
}

fn jit_config() -> AppConfig {
    let mut config = test_config();
    config.jit.source_url_pattern =
        Some("https://origin.example.com/{content_id}/manifest.m3u8".into());
    config.jit.default_target_scheme = EncryptionScheme::Cenc;
    config.jit.default_container_format = ContainerFormat::Cmaf;
    config.jit.lock_ttl_seconds = 30;
    config
}

fn test_context(config: AppConfig) -> HandlerContext {
    HandlerContext { config }
}

// ─── Source Config Store and Retrieve ─────────────────────────────────────

#[test]
fn source_config_store_and_retrieve_roundtrip() {
    let cache = edgepack::cache::global_cache();

    let source = SourceConfig {
        source_url: "https://origin.example.com/manifest.m3u8".into(),
        target_schemes: vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
        container_format: ContainerFormat::Cmaf,
    };

    let key = CacheKeys::source_config("jit-roundtrip-1");
    let data = serde_json::to_vec(&source).unwrap();
    cache.set(&key, &data, 3600).unwrap();

    let retrieved = cache.get(&key).unwrap();
    assert!(retrieved.is_some());

    let parsed: SourceConfig = serde_json::from_slice(&retrieved.unwrap()).unwrap();
    assert_eq!(parsed.source_url, source.source_url);
    assert_eq!(parsed.target_schemes, source.target_schemes);
    assert_eq!(parsed.container_format, source.container_format);

    // Clean up
    let _ = cache.delete(&key);
}

#[test]
fn source_config_api_stores_in_cache() {
    let ctx = test_context(test_config());

    let payload = serde_json::json!({
        "content_id": "jit-api-store-1",
        "source_url": "https://origin.example.com/movie-1/manifest.m3u8",
        "target_schemes": ["cenc", "cbcs"],
        "container_format": "fmp4"
    });
    let body = serde_json::to_vec(&payload).unwrap();

    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/config/source".to_string(),
        headers: vec![],
        body: Some(body),
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 200);

    // Verify it's stored in global cache
    let cache = edgepack::cache::global_cache();
    let stored = cache
        .get(&CacheKeys::source_config("jit-api-store-1"))
        .unwrap();
    assert!(stored.is_some());
    let config: SourceConfig = serde_json::from_slice(&stored.unwrap()).unwrap();
    assert_eq!(
        config.source_url,
        "https://origin.example.com/movie-1/manifest.m3u8"
    );
    assert_eq!(config.target_schemes.len(), 2);

    // Clean up
    let _ = cache.delete(&CacheKeys::source_config("jit-api-store-1"));
}

#[test]
fn source_config_api_rejects_empty_content_id() {
    let ctx = test_context(test_config());

    let payload = serde_json::json!({
        "content_id": "",
        "source_url": "https://example.com"
    });
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/config/source".to_string(),
        headers: vec![],
        body: Some(serde_json::to_vec(&payload).unwrap()),
    };
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("content_id"));
}

#[test]
fn source_config_api_rejects_empty_source_url() {
    let ctx = test_context(test_config());

    let payload = serde_json::json!({
        "content_id": "test",
        "source_url": ""
    });
    let req = HttpRequest {
        method: HttpMethod::Post,
        path: "/config/source".to_string(),
        headers: vec![],
        body: Some(serde_json::to_vec(&payload).unwrap()),
    };
    let result = route(&req, &ctx);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("source_url"));
}

// ─── JIT Route Handling — No Source Config ────────────────────────────────

#[test]
fn jit_no_source_config_manifest_returns_404() {
    let ctx = test_context(test_config());

    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/jit-nosrc-mfst/hls_cenc/manifest".to_string(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn jit_no_source_config_init_returns_404() {
    let ctx = test_context(test_config());

    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/jit-nosrc-init/hls_cenc/init.mp4".to_string(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn jit_no_source_config_segment_returns_404() {
    let ctx = test_context(test_config());

    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/jit-nosrc-seg/hls_cenc/segment_0.cmfv".to_string(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn jit_no_source_config_no_pattern_returns_404() {
    // No source config and no URL pattern → falls through to 404
    let config = test_config();
    // No source_url_pattern set
    let ctx = test_context(config);

    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/nonexistent/hls_cenc/manifest".to_string(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

// ─── JIT With Source Config (Pipeline Fails on Native) ───────────────────

#[test]
fn jit_with_source_config_triggers_pipeline() {
    // JIT with source config → triggers pipeline (which fails on native
    // because there's no real HTTP server at the source URL).
    // The key assertion: it doesn't crash.
    let cache = edgepack::cache::global_cache();
    let config = jit_config();

    // Store source config
    let source = SourceConfig {
        source_url: "https://nonexistent.example.com/manifest.m3u8".into(),
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Cmaf,
    };
    let data = serde_json::to_vec(&source).unwrap();
    cache
        .set(&CacheKeys::source_config("jit-trigger-1"), &data, 3600)
        .unwrap();

    let ctx = test_context(config);

    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/jit-trigger-1/hls_cenc/manifest".to_string(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    // On native: pipeline fails (no HTTP client / unreachable URL) → JIT error → 404
    // The important thing is it didn't crash
    assert!(resp.status == 404 || resp.status == 500 || resp.status == 200);

    // Clean up
    let _ = cache.delete(&CacheKeys::source_config("jit-trigger-1"));
}

// ─── JIT Cache Key Patterns ──────────────────────────────────────────────

#[test]
fn jit_cache_keys_source_config() {
    assert_eq!(
        CacheKeys::source_config("movie-1"),
        "ep:movie-1:source_config"
    );
}

#[test]
fn jit_cache_keys_processing_lock() {
    assert_eq!(
        CacheKeys::processing_lock("movie-1", "hls", "setup"),
        "ep:movie-1:hls:lock:setup"
    );
    assert_eq!(
        CacheKeys::processing_lock("movie-1", "hls", "seg:5"),
        "ep:movie-1:hls:lock:seg:5"
    );
}

#[test]
fn jit_cache_keys_jit_setup() {
    assert_eq!(
        CacheKeys::jit_setup("movie-1", "hls"),
        "ep:movie-1:hls:jit_setup"
    );
}

// ─── Request Coalescing Lock Behavior ────────────────────────────────────

#[test]
fn processing_lock_acquired_first_time() {
    let cache = edgepack::cache::global_cache();
    let lock_key = CacheKeys::processing_lock("jit-lock-1", "hls", "setup");

    let acquired = cache.set_nx(&lock_key, b"1", 30).unwrap();
    assert!(acquired);

    // Clean up
    let _ = cache.delete(&lock_key);
}

#[test]
fn processing_lock_fails_on_second_attempt() {
    let cache = edgepack::cache::global_cache();
    let lock_key = CacheKeys::processing_lock("jit-lock-2", "hls", "setup");

    let first = cache.set_nx(&lock_key, b"1", 30).unwrap();
    assert!(first);

    let second = cache.set_nx(&lock_key, b"1", 30).unwrap();
    assert!(!second);

    // Clean up
    let _ = cache.delete(&lock_key);
}

#[test]
fn processing_lock_released_after_delete() {
    let cache = edgepack::cache::global_cache();
    let lock_key = CacheKeys::processing_lock("jit-lock-3", "hls", "setup");

    cache.set_nx(&lock_key, b"1", 30).unwrap();
    cache.delete(&lock_key).unwrap();

    let reacquired = cache.set_nx(&lock_key, b"1", 30).unwrap();
    assert!(reacquired);

    // Clean up
    let _ = cache.delete(&lock_key);
}

#[test]
fn segment_locks_are_independent() {
    let cache = edgepack::cache::global_cache();

    let lock0 = CacheKeys::processing_lock("jit-lock-4", "hls", "seg:0");
    let lock1 = CacheKeys::processing_lock("jit-lock-4", "hls", "seg:1");

    let a = cache.set_nx(&lock0, b"1", 30).unwrap();
    let b = cache.set_nx(&lock1, b"1", 30).unwrap();
    assert!(a);
    assert!(b);

    // But same lock fails
    let c = cache.set_nx(&lock0, b"1", 30).unwrap();
    assert!(!c);

    // Clean up
    let _ = cache.delete(&lock0);
    let _ = cache.delete(&lock1);
}

// ─── JIT Setup Marker (Idempotency) ─────────────────────────────────────

#[test]
fn jit_setup_marker_stored_and_checked() {
    let cache = edgepack::cache::global_cache();

    let setup_key = CacheKeys::jit_setup("jit-marker-1", "hls");

    assert!(!cache.exists(&setup_key).unwrap());

    cache.set(&setup_key, b"1", 172_800).unwrap();

    assert!(cache.exists(&setup_key).unwrap());

    // Clean up
    let _ = cache.delete(&setup_key);
}

// ─── JIT Config Defaults ─────────────────────────────────────────────────

#[test]
fn jit_config_defaults_are_sane() {
    let config = JitConfig::default();
    assert!(config.source_url_pattern.is_none());
    assert_eq!(config.default_target_scheme, EncryptionScheme::Cenc);
    assert_eq!(config.default_container_format, ContainerFormat::Cmaf);
    assert_eq!(config.lock_ttl_seconds, 30);
}

#[test]
fn jit_config_url_pattern_via_field() {
    let mut config = JitConfig::default();
    config.source_url_pattern = Some("https://example.com/{content_id}/manifest.m3u8".into());
    assert!(config.source_url_pattern.is_some());
}

// ─── Source URL Pattern Resolution ───────────────────────────────────────

#[test]
fn source_url_pattern_replaces_content_id() {
    let pattern = "https://origin.example.com/{content_id}/master.m3u8";
    let resolved = pattern.replace("{content_id}", "movie-123");
    assert_eq!(
        resolved,
        "https://origin.example.com/movie-123/master.m3u8"
    );
}

#[test]
fn source_url_pattern_with_complex_content_id() {
    let pattern = "https://cdn.example.com/vod/{content_id}/index.m3u8";
    let resolved = pattern.replace("{content_id}", "show/season-1/ep-3");
    assert_eq!(
        resolved,
        "https://cdn.example.com/vod/show/season-1/ep-3/index.m3u8"
    );
}

// ─── JIT Route Handling (Scheme-Qualified) ────────────────────────────────

#[test]
fn jit_plain_format_returns_404_no_source() {
    // Plain format (no scheme qualifier) with no source config
    let ctx = test_context(test_config());

    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/jit-plain-nosrc/hls/manifest".to_string(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    // No source config → JIT falls through to 404
    assert_eq!(resp.status, 404);
}

#[test]
fn config_source_get_method_returns_404() {
    // GET on /config/source should return 404 (only POST is valid)
    let ctx = test_context(test_config());

    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/config/source".to_string(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

// ─── SourceConfig Serde ──────────────────────────────────────────────────

#[test]
fn source_config_default_schemes() {
    let json = r#"{"source_url":"https://example.com/source.m3u8"}"#;
    let parsed: SourceConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.target_schemes, vec![EncryptionScheme::Cenc]);
    assert_eq!(parsed.container_format, ContainerFormat::Cmaf);
}

#[test]
fn source_config_multi_scheme_serde() {
    let config = SourceConfig {
        source_url: "https://example.com/source.mpd".into(),
        target_schemes: vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
        container_format: ContainerFormat::Fmp4,
    };
    let json = serde_json::to_string(&config).unwrap();
    let parsed: SourceConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.target_schemes.len(), 2);
    assert_eq!(parsed.container_format, ContainerFormat::Fmp4);
}

// ─── HttpResponse Accepted Retry-After ───────────────────────────────────

#[test]
fn accepted_retry_after_response() {
    use edgepack::handler::HttpResponse;

    let resp = HttpResponse::accepted_retry_after(b"{}".to_vec(), 1);
    assert_eq!(resp.status, 202);
    assert!(resp
        .headers
        .iter()
        .any(|(k, v)| k == "Retry-After" && v == "1"));
    assert!(resp
        .headers
        .iter()
        .any(|(k, v)| k == "Content-Type" && v == "application/json"));
}
