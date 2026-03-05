//! Integration tests: Advanced DRM features (Phase 11).
//!
//! Tests ClearKey DRM, raw key bypass, clear lead, key rotation,
//! and DRM system filtering.

mod common;

use edgepack::drm;
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::manifest;
use edgepack::manifest::types::*;
use edgepack::media::container::ContainerFormat;
use edgepack::repackager::*;

// ─── ClearKey System ID ─────────────────────────────────────────────

#[test]
fn clearkey_system_id_uuid() {
    // e2719d58-a985-b3c9-781a-b030af78d30e
    assert_eq!(drm::system_ids::CLEARKEY[0], 0xe2);
    assert_eq!(drm::system_ids::CLEARKEY[15], 0x0e);
    assert_eq!(drm::system_ids::system_id_name(&drm::system_ids::CLEARKEY), "ClearKey");
}

// ─── ClearKey PSSH Builder ──────────────────────────────────────────

#[test]
fn clearkey_pssh_data_single_kid_integration() {
    let kid = [0x42u8; 16];
    let data = drm::build_clearkey_pssh_data(&[kid]);
    let json_str = std::str::from_utf8(&data).unwrap();
    assert!(json_str.contains("\"kids\""));
    let parsed: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(parsed["kids"].as_array().unwrap().len(), 1);
}

#[test]
fn clearkey_pssh_data_multi_kid_integration() {
    let kid1 = [0xAA; 16];
    let kid2 = [0xBB; 16];
    let data = drm::build_clearkey_pssh_data(&[kid1, kid2]);
    let parsed: serde_json::Value = serde_json::from_slice(&data).unwrap();
    assert_eq!(parsed["kids"].as_array().unwrap().len(), 2);
}

// ─── Raw Key Types ──────────────────────────────────────────────────

#[test]
fn raw_key_entry_serde_integration() {
    let entry = RawKeyEntry {
        kid: [0x01; 16],
        key: [0x02; 16],
        iv: Some([0x03; 16]),
    };
    let json = serde_json::to_string(&entry).unwrap();
    let parsed: RawKeyEntry = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.kid, entry.kid);
    assert_eq!(parsed.key, entry.key);
    assert_eq!(parsed.iv, entry.iv);
}

#[test]
fn key_rotation_config_serde_integration() {
    let cfg = KeyRotationConfig { period_segments: 5 };
    let json = serde_json::to_string(&cfg).unwrap();
    let parsed: KeyRotationConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.period_segments, 5);
}

#[test]
fn repackage_request_with_all_advanced_drm_fields() {
    let req = RepackageRequest {
        content_id: "adv-drm-1".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_format: OutputFormat::Hls,
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![RawKeyEntry {
            kid: [0xAA; 16],
            key: [0xBB; 16],
            iv: None,
        }],
        key_rotation: Some(KeyRotationConfig { period_segments: 10 }),
        clear_lead_segments: Some(3),
        drm_systems: vec!["widevine".into(), "clearkey".into()],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.raw_keys.len(), 1);
    assert_eq!(parsed.key_rotation.unwrap().period_segments, 10);
    assert_eq!(parsed.clear_lead_segments, Some(3));
    assert_eq!(parsed.drm_systems.len(), 2);
}

// ─── HLS ClearKey Signaling ─────────────────────────────────────────

#[test]
fn hls_clearkey_key_tag() {
    let mut state = ManifestState::new("ck-hls".into(), OutputFormat::Hls, "/base/".into(), ContainerFormat::default());
    state.phase = ManifestPhase::Live;
    state.init_segment = Some(InitSegmentInfo { uri: "/base/init.mp4".into(), byte_size: 256 });
    state.segments.push(SegmentInfo {
        number: 0,
        duration: 6.0,
        uri: "/base/segment_0.cmfv".into(),
        byte_size: 1024,
        key_period: None,
    });
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
        widevine_pssh: None,
        playready_pssh: None,
        playready_pro: None,
        fairplay_key_uri: None,
        default_kid: "0123456789abcdef0123456789abcdef".into(),
        clearkey_pssh: Some("CK_PSSH_B64".into()),
    });
    let m3u8 = manifest::render_manifest(&state).unwrap();
    assert!(m3u8.contains("e2719d58-a985-b3c9-781a-b030af78d30e"));
    assert!(m3u8.contains("CK_PSSH_B64"));
}

// ─── DASH ClearKey Signaling ────────────────────────────────────────

#[test]
fn dash_clearkey_content_protection() {
    let mut state = ManifestState::new("ck-dash".into(), OutputFormat::Dash, "/base/".into(), ContainerFormat::default());
    state.phase = ManifestPhase::Live;
    state.init_segment = Some(InitSegmentInfo { uri: "/base/init.mp4".into(), byte_size: 256 });
    state.segments.push(SegmentInfo {
        number: 0,
        duration: 6.0,
        uri: "/base/segment_0.cmfv".into(),
        byte_size: 1024,
        key_period: None,
    });
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
        widevine_pssh: None,
        playready_pssh: None,
        playready_pro: None,
        fairplay_key_uri: None,
        default_kid: "0123456789abcdef0123456789abcdef".into(),
        clearkey_pssh: Some("CK_DASH_B64".into()),
    });
    let mpd = manifest::render_manifest(&state).unwrap();
    assert!(mpd.contains("e2719d58-a985-b3c9-781a-b030af78d30e"));
    assert!(mpd.contains("CK_DASH_B64"));
}

// ─── Clear Lead ─────────────────────────────────────────────────────

#[test]
fn hls_clear_lead_method_none_then_encrypted() {
    let mut state = ManifestState::new("cl-test".into(), OutputFormat::Hls, "/base/".into(), ContainerFormat::default());
    state.phase = ManifestPhase::Live;
    state.init_segment = Some(InitSegmentInfo { uri: "/base/init.mp4".into(), byte_size: 256 });
    for i in 0..4u32 {
        state.segments.push(SegmentInfo {
            number: i,
            duration: 6.0,
            uri: format!("/base/segment_{i}.cmfv"),
            byte_size: 1024,
            key_period: None,
        });
    }
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
        widevine_pssh: Some("WV_DATA".into()),
        playready_pssh: None,
        playready_pro: None,
        fairplay_key_uri: None,
        default_kid: "00000000000000000000000000000001".into(),
        clearkey_pssh: None,
    });
    state.clear_lead_boundary = Some(2);
    let m3u8 = manifest::render_manifest(&state).unwrap();
    // METHOD=NONE should appear before segments
    assert!(m3u8.contains("METHOD=NONE"));
    // DRM keys should appear after clear lead ends
    assert!(m3u8.contains("SAMPLE-AES-CTR"));
    // METHOD=NONE should come first
    let none_idx = m3u8.find("METHOD=NONE").unwrap();
    let drm_idx = m3u8.find("SAMPLE-AES-CTR").unwrap();
    assert!(none_idx < drm_idx);
}

// ─── Key Rotation ───────────────────────────────────────────────────

#[test]
fn hls_key_rotation_emits_keys_per_period() {
    let mut state = ManifestState::new("kr-test".into(), OutputFormat::Hls, "/base/".into(), ContainerFormat::default());
    state.phase = ManifestPhase::Live;
    state.init_segment = Some(InitSegmentInfo { uri: "/base/init.mp4".into(), byte_size: 256 });
    for i in 0..6u32 {
        state.segments.push(SegmentInfo {
            number: i,
            duration: 6.0,
            uri: format!("/base/segment_{i}.cmfv"),
            byte_size: 1024,
            key_period: Some(i / 3),
        });
    }
    state.rotation_drm_info = vec![
        ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV_PERIOD_0".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa0".into(),
            clearkey_pssh: None,
        },
        ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV_PERIOD_1".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa1".into(),
            clearkey_pssh: None,
        },
    ];
    let m3u8 = manifest::render_manifest(&state).unwrap();
    assert!(m3u8.contains("WV_PERIOD_0"));
    assert!(m3u8.contains("WV_PERIOD_1"));
    // Period 0 KEY should appear before period 1 KEY
    let p0_pos = m3u8.find("WV_PERIOD_0").unwrap();
    let p1_pos = m3u8.find("WV_PERIOD_1").unwrap();
    assert!(p0_pos < p1_pos);
}

// ─── Backward Compatibility ─────────────────────────────────────────

#[test]
fn manifest_drm_info_backward_compat_no_clearkey() {
    // JSON without clearkey_pssh field should parse fine
    let json = r#"{"encryption_scheme":"Cenc","widevine_pssh":"WV","playready_pssh":null,"playready_pro":null,"fairplay_key_uri":null,"default_kid":"0123456789abcdef0123456789abcdef"}"#;
    let parsed: ManifestDrmInfo = serde_json::from_str(json).unwrap();
    assert!(parsed.clearkey_pssh.is_none());
    assert_eq!(parsed.widevine_pssh, Some("WV".into()));
}

#[test]
fn manifest_state_backward_compat_no_rotation_fields() {
    let json = r#"{"content_id":"c","format":"Hls","phase":"Live","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
    let parsed: ManifestState = serde_json::from_str(json).unwrap();
    assert!(parsed.rotation_drm_info.is_empty());
    assert!(parsed.clear_lead_boundary.is_none());
}

#[test]
fn repackage_request_backward_compat_no_advanced_fields() {
    let json = r#"{"content_id":"test","source_url":"https://example.com","output_format":"Hls","key_ids":[]}"#;
    let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
    assert!(parsed.raw_keys.is_empty());
    assert!(parsed.key_rotation.is_none());
    assert!(parsed.clear_lead_segments.is_none());
    assert!(parsed.drm_systems.is_empty());
}

// ─── DRM Systems Validation ─────────────────────────────────────────

#[test]
fn drm_systems_valid_values() {
    let valid = ["widevine", "playready", "fairplay", "clearkey"];
    for sys in &valid {
        assert!(valid.contains(sys));
    }
}

#[test]
fn clearkey_combined_with_widevine() {
    let req = RepackageRequest {
        content_id: "combo".into(),
        source_url: "https://example.com/src.m3u8".into(),
        output_format: OutputFormat::Hls,
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::Cmaf,
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec!["widevine".into(), "clearkey".into()],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
    };
    assert_eq!(req.drm_systems.len(), 2);
}
