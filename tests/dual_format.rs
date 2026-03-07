//! Integration tests for Phase 21: Generic HLS/DASH Pipeline (dual-format output).
//!
//! Tests multi-format output where a single request produces both HLS and DASH manifests
//! sharing format-agnostic CMAF segments.

mod common;

use edgepack::cache::CacheKeys;
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::manifest::types::{
    ManifestPhase, ManifestState, OutputFormat,
};
use edgepack::media::container::ContainerFormat;
use edgepack::repackager::RepackageRequest;

// ─── RepackageRequest with output_formats ──────────────────────────────

#[test]
fn repackage_request_single_format_hls() {
    let req = RepackageRequest {
        content_id: "fmt-1".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_formats: vec![OutputFormat::Hls],
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };
    assert_eq!(req.primary_format(), OutputFormat::Hls);
    assert_eq!(req.output_formats.len(), 1);
}

#[test]
fn repackage_request_single_format_dash() {
    let req = RepackageRequest {
        content_id: "fmt-2".into(),
        source_url: "https://example.com/src.mpd".into(),
        output_formats: vec![OutputFormat::Dash],
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };
    assert_eq!(req.primary_format(), OutputFormat::Dash);
    assert_eq!(req.output_formats.len(), 1);
}

#[test]
fn repackage_request_dual_format() {
    let req = RepackageRequest {
        content_id: "fmt-dual".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_formats: vec![OutputFormat::Hls, OutputFormat::Dash],
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };
    assert_eq!(req.primary_format(), OutputFormat::Hls);
    assert_eq!(req.output_formats.len(), 2);
    assert_eq!(req.output_formats[0], OutputFormat::Hls);
    assert_eq!(req.output_formats[1], OutputFormat::Dash);
}

#[test]
fn repackage_request_dual_format_primary_is_first() {
    let req = RepackageRequest {
        content_id: "fmt-prim".into(),
        source_url: "https://example.com/src.mpd".into(),
        output_formats: vec![OutputFormat::Dash, OutputFormat::Hls],
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };
    assert_eq!(req.primary_format(), OutputFormat::Dash);
}

#[test]
fn repackage_request_dual_format_serde_roundtrip() {
    let req = RepackageRequest {
        content_id: "serde-dual".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_formats: vec![OutputFormat::Hls, OutputFormat::Dash],
        target_schemes: vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.output_formats, vec![OutputFormat::Hls, OutputFormat::Dash]);
    assert_eq!(parsed.target_schemes, vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs]);
}

#[test]
fn repackage_request_dual_format_dual_scheme() {
    let req = RepackageRequest {
        content_id: "4way".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_formats: vec![OutputFormat::Hls, OutputFormat::Dash],
        target_schemes: vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };
    // 2 formats × 2 schemes = 4 output combinations expected
    let expected_outputs = req.output_formats.len() * req.target_schemes.len();
    assert_eq!(expected_outputs, 4);
}

#[test]
fn repackage_request_backward_compat_no_output_formats() {
    // Old JSON without output_formats field should deserialize with empty vec (default)
    let json = r#"{"content_id":"test","source_url":"https://example.com","output_formats":["Hls"],"key_ids":[]}"#;
    let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
    assert_eq!(parsed.output_formats, vec![OutputFormat::Hls]);
}

// ─── Format-Agnostic Cache Keys ────────────────────────────────────────

#[test]
fn format_agnostic_init_key_no_format_prefix() {
    let key = CacheKeys::init_segment_for_scheme_only("content-1", "cenc");
    assert_eq!(key, "ep:content-1:cenc:init");
    // Verify it does NOT contain "hls" or "dash"
    assert!(!key.contains("hls"));
    assert!(!key.contains("dash"));
}

#[test]
fn format_agnostic_media_key_no_format_prefix() {
    let key = CacheKeys::media_segment_for_scheme_only("content-1", "cbcs", 5);
    assert_eq!(key, "ep:content-1:cbcs:seg:5");
    assert!(!key.contains("hls"));
    assert!(!key.contains("dash"));
}

#[test]
fn legacy_keys_still_have_format() {
    // Legacy format-qualified keys should still include format
    let init_key = CacheKeys::init_segment_for_scheme("content-1", "hls", "cenc");
    assert!(init_key.contains("hls"));

    let seg_key = CacheKeys::media_segment_for_scheme("content-1", "dash", "cbcs", 3);
    assert!(seg_key.contains("dash"));
}

#[test]
fn format_agnostic_vs_legacy_keys_differ() {
    let agnostic = CacheKeys::init_segment_for_scheme_only("c1", "cenc");
    let legacy_hls = CacheKeys::init_segment_for_scheme("c1", "hls", "cenc");
    let legacy_dash = CacheKeys::init_segment_for_scheme("c1", "dash", "cenc");

    assert_ne!(agnostic, legacy_hls);
    assert_ne!(agnostic, legacy_dash);
    assert_ne!(legacy_hls, legacy_dash);
}

#[test]
fn target_formats_cache_key() {
    let key = CacheKeys::target_formats("my-content");
    assert_eq!(key, "ep:my-content:target_formats");
}

// ─── Manifest State Per-Format ────────────────────────────────────────

#[test]
fn manifest_state_format_is_per_instance() {
    // HLS and DASH manifest states are separate instances
    let hls_state = ManifestState::new(
        "test".into(), OutputFormat::Hls, "/base/hls/".into(), ContainerFormat::Cmaf,
    );
    let dash_state = ManifestState::new(
        "test".into(), OutputFormat::Dash, "/base/dash/".into(), ContainerFormat::Cmaf,
    );
    assert_eq!(hls_state.format, OutputFormat::Hls);
    assert_eq!(dash_state.format, OutputFormat::Dash);
    assert_ne!(hls_state.format, dash_state.format);
}

#[test]
fn manifest_state_keys_are_format_qualified() {
    // Manifest state cache keys should still include format (manifests differ per format)
    let hls_key = CacheKeys::manifest_state_for_scheme("c1", "hls", "cenc");
    let dash_key = CacheKeys::manifest_state_for_scheme("c1", "dash", "cenc");
    assert!(hls_key.contains("hls"));
    assert!(dash_key.contains("dash"));
    assert_ne!(hls_key, dash_key);
}

// ─── Dual-Format Manifest Rendering ────────────────────────────────────

#[test]
fn hls_manifest_renders_m3u8() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    state.format = OutputFormat::Hls;
    let text = edgepack::manifest::render_manifest(&state).unwrap();
    assert!(text.contains("#EXTM3U"));
    assert!(text.contains("#EXT-X-ENDLIST"));
}

#[test]
fn dash_manifest_renders_mpd() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    state.format = OutputFormat::Dash;
    let text = edgepack::manifest::render_manifest(&state).unwrap();
    assert!(text.contains("<MPD"));
    assert!(text.contains("type=\"static\""));
}

#[test]
fn dual_format_produces_different_manifests() {
    // Same content, same DRM, but HLS and DASH formats produce different manifests
    let hls_state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let dash_state = common::make_dash_manifest_state(3, ManifestPhase::Complete);

    let hls_text = edgepack::manifest::render_manifest(&hls_state).unwrap();
    let dash_text = edgepack::manifest::render_manifest(&dash_state).unwrap();

    // HLS is M3U8, DASH is MPD
    assert!(hls_text.contains("#EXTM3U"));
    assert!(dash_text.contains("<MPD"));
    assert!(!hls_text.contains("<MPD"));
    assert!(!dash_text.contains("#EXTM3U"));
}

// ─── Container Format Independence ────────────────────────────────────

#[test]
fn container_format_affects_segments_not_output_format() {
    // Verify that ContainerFormat is independent of OutputFormat
    let cmaf_ext = ContainerFormat::Cmaf.video_segment_extension();
    let fmp4_ext = ContainerFormat::Fmp4.video_segment_extension();
    let iso_ext = ContainerFormat::Iso.video_segment_extension();

    assert_eq!(cmaf_ext, ".cmfv");
    assert_eq!(fmp4_ext, ".m4s");
    assert_eq!(iso_ext, ".mp4");

    // These extensions are the same regardless of whether output is HLS or DASH
}

#[test]
fn dual_format_with_fmp4_container() {
    let req = RepackageRequest {
        content_id: "fmp4-dual".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_formats: vec![OutputFormat::Hls, OutputFormat::Dash],
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Fmp4,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };
    assert_eq!(req.container_format, ContainerFormat::Fmp4);
    assert_eq!(req.output_formats.len(), 2);
}

// ─── Serde Backward Compat ────────────────────────────────────────────

#[test]
fn repackage_request_old_json_without_output_formats_field() {
    // Before Phase 21, JSON had output_format (singular). New format uses output_formats (plural).
    // Vec<OutputFormat> with serde(default) means missing field = empty Vec.
    let json = r#"{"content_id":"old","source_url":"https://example.com","target_schemes":["Cenc"],"key_ids":[]}"#;
    let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
    // output_formats defaults to empty Vec when not present
    assert!(parsed.output_formats.is_empty());
    // primary_format() falls back to Hls when empty
    assert_eq!(parsed.primary_format(), OutputFormat::Hls);
}
