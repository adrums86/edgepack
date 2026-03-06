//! Integration tests for Phase 19: Configurable Cache-Control Headers.
//!
//! Tests cache-control header generation with system defaults, per-request overrides,
//! immutable flag toggling, s-maxage split, backward compatibility, and serde roundtrips.

mod common;

use edgepack::config::{CacheConfig, CacheControlConfig};
use edgepack::manifest::types::{ManifestPhase, ManifestState, OutputFormat};
use edgepack::media::container::ContainerFormat;
use edgepack::repackager::progressive::ProgressiveOutput;
use edgepack::repackager::RepackageRequest;
use edgepack::drm::scheme::EncryptionScheme;

// ─── System Defaults ────────────────────────────────────────────────

#[test]
fn hls_manifest_system_defaults_awaiting() {
    let state = common::make_hls_manifest_state(0, ManifestPhase::AwaitingFirstSegment);
    let system = CacheConfig::default();
    assert_eq!(state.manifest_cache_header(&system), "no-cache");
}

#[test]
fn hls_manifest_system_defaults_live() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    let system = CacheConfig::default();
    assert_eq!(state.manifest_cache_header(&system), "public, max-age=1, s-maxage=1");
}

#[test]
fn hls_manifest_system_defaults_complete() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=31536000, immutable"
    );
}

#[test]
fn dash_manifest_system_defaults_live() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Live);
    let system = CacheConfig::default();
    assert_eq!(state.manifest_cache_header(&system), "public, max-age=1, s-maxage=1");
}

#[test]
fn dash_manifest_system_defaults_complete() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=31536000, immutable"
    );
}

#[test]
fn segment_system_defaults() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    let system = CacheConfig::default();
    assert_eq!(
        state.segment_cache_header(&system),
        "public, max-age=31536000, immutable"
    );
}

// ─── Per-Request Overrides ──────────────────────────────────────────

#[test]
fn live_manifest_max_age_override() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        live_manifest_max_age: Some(5),
        ..Default::default()
    });
    let system = CacheConfig::default();
    // s-maxage should default to the overridden max-age when not explicitly set
    assert_eq!(state.manifest_cache_header(&system), "public, max-age=5, s-maxage=5");
}

#[test]
fn live_manifest_s_maxage_split() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        live_manifest_max_age: Some(2),
        live_manifest_s_maxage: Some(10),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(state.manifest_cache_header(&system), "public, max-age=2, s-maxage=10");
}

#[test]
fn final_manifest_max_age_override() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    state.cache_control = Some(CacheControlConfig {
        final_manifest_max_age: Some(3600),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=3600, immutable"
    );
}

#[test]
fn segment_max_age_override() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        segment_max_age: Some(86400),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.segment_cache_header(&system),
        "public, max-age=86400, immutable"
    );
}

// ─── Immutable Flag ─────────────────────────────────────────────────

#[test]
fn immutable_off_complete_manifest() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    state.cache_control = Some(CacheControlConfig {
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=31536000"
    );
}

#[test]
fn immutable_off_segment() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.segment_cache_header(&system),
        "public, max-age=31536000"
    );
}

#[test]
fn immutable_on_explicitly() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    state.cache_control = Some(CacheControlConfig {
        immutable: Some(true),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=31536000, immutable"
    );
}

// ─── Safety Invariants ──────────────────────────────────────────────

#[test]
fn awaiting_always_no_cache_even_with_overrides() {
    let mut state = common::make_hls_manifest_state(0, ManifestPhase::AwaitingFirstSegment);
    state.cache_control = Some(CacheControlConfig {
        live_manifest_max_age: Some(3600),
        final_manifest_max_age: Some(86400),
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(state.manifest_cache_header(&system), "no-cache");
}

// ─── Progressive Output Integration ─────────────────────────────────

#[test]
fn progressive_output_manifest_cache_control_with_override() {
    let mut po = ProgressiveOutput::new(
        "cc-prog-test".into(),
        OutputFormat::Hls,
        "/repackage/cc-prog-test/hls/".into(),
        None,
        ContainerFormat::default(),
    );
    po.set_cache_control(CacheControlConfig {
        live_manifest_max_age: Some(5),
        live_manifest_s_maxage: Some(30),
        ..Default::default()
    });

    // Add a segment to transition to Live
    po.set_init_segment(vec![0x00; 256]);
    po.add_segment(0, vec![0xAA; 1000], 6.0);
    assert_eq!(po.manifest_state().phase, ManifestPhase::Live);

    let system = CacheConfig::default();
    assert_eq!(po.manifest_cache_control(&system), "public, max-age=5, s-maxage=30");
}

#[test]
fn progressive_output_segment_cache_control_with_override() {
    let mut po = ProgressiveOutput::new(
        "cc-seg-test".into(),
        OutputFormat::Hls,
        "/repackage/cc-seg-test/hls/".into(),
        None,
        ContainerFormat::default(),
    );
    po.set_cache_control(CacheControlConfig {
        segment_max_age: Some(600),
        immutable: Some(false),
        ..Default::default()
    });

    let system = CacheConfig::default();
    assert_eq!(po.segment_cache_control(&system), "public, max-age=600");
}

#[test]
fn progressive_output_finalize_with_override() {
    let mut po = ProgressiveOutput::new(
        "cc-final-test".into(),
        OutputFormat::Hls,
        "/repackage/cc-final-test/hls/".into(),
        None,
        ContainerFormat::default(),
    );
    po.set_cache_control(CacheControlConfig {
        final_manifest_max_age: Some(7200),
        immutable: Some(false),
        ..Default::default()
    });

    po.set_init_segment(vec![0x00; 256]);
    po.add_segment(0, vec![0xAA; 1000], 6.0);
    po.finalize();
    assert_eq!(po.manifest_state().phase, ManifestPhase::Complete);

    let system = CacheConfig::default();
    assert_eq!(po.manifest_cache_control(&system), "public, max-age=7200");
}

// ─── Backward Compatibility ─────────────────────────────────────────

#[test]
fn manifest_state_without_cache_control_deserializes() {
    // Simulates ManifestState from Redis without cache_control field
    let json = r#"{"content_id":"c","format":"Hls","phase":"Complete","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
    let parsed: ManifestState = serde_json::from_str(json).unwrap();
    assert!(parsed.cache_control.is_none());
    let system = CacheConfig::default();
    assert_eq!(
        parsed.manifest_cache_header(&system),
        "public, max-age=31536000, immutable"
    );
}

#[test]
fn manifest_state_with_cache_control_serde_roundtrip() {
    let mut state = common::make_hls_manifest_state(2, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        segment_max_age: Some(3600),
        live_manifest_max_age: Some(5),
        live_manifest_s_maxage: Some(10),
        final_manifest_max_age: Some(86400),
        immutable: Some(false),
    });
    let json = serde_json::to_string(&state).unwrap();
    let parsed: ManifestState = serde_json::from_str(&json).unwrap();
    let cc = parsed.cache_control.unwrap();
    assert_eq!(cc.segment_max_age, Some(3600));
    assert_eq!(cc.live_manifest_max_age, Some(5));
    assert_eq!(cc.live_manifest_s_maxage, Some(10));
    assert_eq!(cc.final_manifest_max_age, Some(86400));
    assert_eq!(cc.immutable, Some(false));
}

#[test]
fn repackage_request_cache_control_serde_roundtrip() {
    let req = RepackageRequest {
        content_id: "cc-test".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_formats: vec![OutputFormat::Hls],
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::default(),
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: Some(CacheControlConfig {
            segment_max_age: Some(7200),
            final_manifest_max_age: None,
            live_manifest_max_age: Some(3),
            live_manifest_s_maxage: None,
            immutable: Some(false),
        }),
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
    let cc = parsed.cache_control.unwrap();
    assert_eq!(cc.segment_max_age, Some(7200));
    assert!(cc.final_manifest_max_age.is_none());
    assert_eq!(cc.live_manifest_max_age, Some(3));
    assert!(cc.live_manifest_s_maxage.is_none());
    assert_eq!(cc.immutable, Some(false));
}

#[test]
fn repackage_request_no_cache_control_backward_compat() {
    let json = r#"{"content_id":"test","source_url":"https://example.com","output_formats":["Hls"],"key_ids":[]}"#;
    let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
    assert!(parsed.cache_control.is_none());
}

// ─── CacheControlConfig Behavior ────────────────────────────────────

#[test]
fn cache_control_config_default_is_all_none() {
    let cc = CacheControlConfig::default();
    assert!(cc.segment_max_age.is_none());
    assert!(cc.final_manifest_max_age.is_none());
    assert!(cc.live_manifest_max_age.is_none());
    assert!(cc.live_manifest_s_maxage.is_none());
    assert!(cc.immutable.is_none());
}

#[test]
fn cache_control_config_is_immutable_default_true() {
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
fn cache_control_config_serde_roundtrip() {
    let cc = CacheControlConfig {
        segment_max_age: Some(86400),
        final_manifest_max_age: Some(604800),
        live_manifest_max_age: Some(2),
        live_manifest_s_maxage: Some(15),
        immutable: Some(false),
    };
    let json = serde_json::to_string(&cc).unwrap();
    let parsed: CacheControlConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, cc);
}

#[test]
fn cache_control_config_partial_serde() {
    // JSON with only some fields set
    let json = r#"{"segment_max_age":3600}"#;
    let parsed: CacheControlConfig = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.segment_max_age, Some(3600));
    assert!(parsed.final_manifest_max_age.is_none());
    assert!(parsed.live_manifest_max_age.is_none());
    assert!(parsed.live_manifest_s_maxage.is_none());
    assert!(parsed.immutable.is_none());
    assert!(parsed.is_immutable()); // default true when None
}

// ─── DVR + Cache Control Interaction ────────────────────────────────

#[test]
fn dvr_live_manifest_with_cache_control_override() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.cache_control = Some(CacheControlConfig {
        live_manifest_max_age: Some(3),
        live_manifest_s_maxage: Some(15),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(state.manifest_cache_header(&system), "public, max-age=3, s-maxage=15");
    assert!(state.is_dvr_active());
}

#[test]
fn dvr_complete_manifest_with_cache_control_override() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Complete, 30.0);
    state.cache_control = Some(CacheControlConfig {
        final_manifest_max_age: Some(86400),
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(state.manifest_cache_header(&system), "public, max-age=86400");
    // DVR is not active in Complete phase
    assert!(!state.is_dvr_active());
}

// ─── Container Format + Cache Control ───────────────────────────────

#[test]
fn fmp4_manifest_cache_control_override() {
    let mut state = ManifestState::new(
        "fmp4-cc-test".into(),
        OutputFormat::Hls,
        "/repackage/fmp4-cc-test/hls/".into(),
        ContainerFormat::Fmp4,
    );
    state.phase = ManifestPhase::Complete;
    state.cache_control = Some(CacheControlConfig {
        final_manifest_max_age: Some(7200),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=7200, immutable"
    );
}

#[test]
fn iso_segment_cache_control_override() {
    let mut state = ManifestState::new(
        "iso-cc-test".into(),
        OutputFormat::Dash,
        "/repackage/iso-cc-test/dash/".into(),
        ContainerFormat::Iso,
    );
    state.phase = ManifestPhase::Live;
    state.cache_control = Some(CacheControlConfig {
        segment_max_age: Some(300),
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(state.segment_cache_header(&system), "public, max-age=300");
}

// ─── System CacheConfig Overrides ───────────────────────────────────

#[test]
fn system_config_final_manifest_max_age() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let mut system = CacheConfig::default();
    system.final_manifest_max_age = 86400;
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=86400, immutable"
    );
}

#[test]
fn system_config_live_manifest_max_age() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    let mut system = CacheConfig::default();
    system.live_manifest_max_age = 5;
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=5, s-maxage=5"
    );
}

#[test]
fn system_config_segment_vod_max_age() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    let mut system = CacheConfig::default();
    system.vod_max_age = 600;
    assert_eq!(
        state.segment_cache_header(&system),
        "public, max-age=600, immutable"
    );
}

#[test]
fn per_request_overrides_system_config() {
    // Per-request should take precedence over system config
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        live_manifest_max_age: Some(10),
        ..Default::default()
    });
    let mut system = CacheConfig::default();
    system.live_manifest_max_age = 5;
    // Per-request (10) should override system (5)
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=10, s-maxage=10"
    );
}

// ─── DASH Per-Request Override Tests ────────────────────────────────

#[test]
fn dash_manifest_live_max_age_override() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        live_manifest_max_age: Some(5),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=5, s-maxage=5"
    );
}

#[test]
fn dash_manifest_live_s_maxage_split() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        live_manifest_max_age: Some(2),
        live_manifest_s_maxage: Some(15),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=2, s-maxage=15"
    );
}

#[test]
fn dash_manifest_complete_max_age_override() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    state.cache_control = Some(CacheControlConfig {
        final_manifest_max_age: Some(7200),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=7200, immutable"
    );
}

#[test]
fn dash_manifest_complete_immutable_off() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    state.cache_control = Some(CacheControlConfig {
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=31536000"
    );
}

#[test]
fn dash_segment_uses_system_defaults_not_per_request() {
    // Documents intentional design: segment handlers use system config only,
    // not per-request overrides. This avoids an extra Redis GET per segment request.
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Live);
    state.cache_control = Some(CacheControlConfig {
        segment_max_age: Some(300),
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    // segment_cache_header respects per-request on ManifestState (available in ProgressiveOutput),
    // but GET segment handlers don't load ManifestState — they use system config directly.
    // This test documents the ManifestState-level behavior.
    assert_eq!(
        state.segment_cache_header(&system),
        "public, max-age=300"
    );
}

// ─── Segment Handler Design Documentation Tests ─────────────────────

#[test]
fn segment_handler_uses_system_defaults_by_design() {
    // This test documents an intentional design decision:
    // GET init/media segment handlers hardcode ctx.config.cache.vod_max_age
    // and do NOT load ManifestState for cache_control overrides.
    //
    // Rationale: loading ManifestState from Redis for every segment request
    // would add latency. Segments are immutable, so the system default is
    // almost always correct. Per-request overrides are available through
    // ProgressiveOutput for the sandbox execute() path.
    let system = CacheConfig::default();
    let expected = format!("public, max-age={}, immutable", system.vod_max_age);
    assert_eq!(expected, "public, max-age=31536000, immutable");
}

#[test]
fn jit_manifest_state_has_no_cache_control_by_design() {
    // Documents that JIT-created ManifestState always has cache_control: None.
    // JIT requests come from GET cache misses — there's no webhook payload
    // to carry per-request overrides. JIT always uses system defaults.
    //
    // The pipeline.rs JIT ManifestState constructor (struct literal at ~line 1239)
    // explicitly sets cache_control: None. This test ensures that the default
    // ManifestState fixtures reflect this JIT behavior.
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    assert!(
        state.cache_control.is_none(),
        "Default ManifestState should have cache_control: None (JIT default)"
    );
    // With cache_control: None, manifest_cache_header falls through to system defaults
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "public, max-age=31536000, immutable"
    );
}

// ─── DASH Progressive Output Integration ────────────────────────────

#[test]
fn progressive_output_dash_manifest_with_override() {
    let mut po = ProgressiveOutput::new(
        "dash-cc".into(),
        OutputFormat::Dash,
        "/repackage/dash-cc/dash".into(),
        None,
        ContainerFormat::default(),
    );
    po.set_init_segment(vec![0x00]);
    po.add_segment(0, vec![0xAA; 50], 6.0);
    po.set_cache_control(CacheControlConfig {
        live_manifest_max_age: Some(3),
        live_manifest_s_maxage: Some(10),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        po.manifest_cache_control(&system),
        "public, max-age=3, s-maxage=10"
    );
}

#[test]
fn progressive_output_dash_finalize_with_override() {
    let mut po = ProgressiveOutput::new(
        "dash-cc".into(),
        OutputFormat::Dash,
        "/repackage/dash-cc/dash".into(),
        None,
        ContainerFormat::default(),
    );
    po.set_init_segment(vec![0x00]);
    po.add_segment(0, vec![0xAA; 50], 6.0);
    po.finalize();
    po.set_cache_control(CacheControlConfig {
        final_manifest_max_age: Some(1800),
        immutable: Some(false),
        ..Default::default()
    });
    let system = CacheConfig::default();
    assert_eq!(
        po.manifest_cache_control(&system),
        "public, max-age=1800"
    );
}
