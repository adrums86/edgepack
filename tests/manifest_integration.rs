//! Integration tests: Manifest generation lifecycle (HLS + DASH).
//!
//! Tests the full manifest lifecycle using the ProgressiveOutput state machine:
//! 1. AwaitingFirstSegment → no manifest available
//! 2. Add first segment → Live manifest with one segment
//! 3. Add subsequent segments → Live manifest with multiple segments
//! 4. Finalize → Complete manifest (HLS: ENDLIST, DASH: static)
//!
//! Also validates DRM signaling (Widevine + PlayReady) in both formats.

mod common;

use edge_packager::manifest::types::{
    ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat,
};
use edge_packager::manifest;
use edge_packager::repackager::progressive::ProgressiveOutput;

// ─── ProgressiveOutput State Machine ────────────────────────────────

#[test]
fn progressive_output_hls_full_lifecycle() {
    let drm_info = ManifestDrmInfo {
        encryption_scheme: edge_packager::drm::scheme::EncryptionScheme::Cenc,
        widevine_pssh: Some("AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgIARIQ".into()),
        playready_pssh: Some("AAAARHBzc2gBAAAAmgTweZhAQoarkuZb4IhflQAAAAE=".into()),
        playready_pro: None,
        fairplay_key_uri: None,
        default_kid: "00112233445566778899aabbccddeeff".into(),
    };

    let mut po = ProgressiveOutput::new(
        "lifecycle-test".into(),
        OutputFormat::Hls,
        "/repackage/lifecycle-test/hls/".into(),
        drm_info,
    );

    // Phase 1: AwaitingFirstSegment
    assert_eq!(
        po.manifest_state().phase,
        ManifestPhase::AwaitingFirstSegment
    );
    assert!(po.current_manifest().is_none());
    assert_eq!(
        po.manifest_cache_control(31536000, 1),
        "no-cache"
    );

    // Set init segment
    po.set_init_segment(vec![0x00; 256]);
    assert!(po.init_segment_data().is_some());
    assert_eq!(po.init_segment_data().unwrap().len(), 256);

    // Phase 2: Add first segment → transition to Live
    let manifest = po.add_segment(0, vec![0xAA; 50_000], 6.006);
    assert!(manifest.is_some());
    assert_eq!(po.manifest_state().phase, ManifestPhase::Live);

    let m3u8 = manifest.unwrap();
    assert!(m3u8.contains("#EXTM3U"));
    assert!(m3u8.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    assert!(m3u8.contains("#EXTINF:6.006000,"));
    assert!(m3u8.contains("/repackage/lifecycle-test/hls/segment_0.cmfv"));
    assert!(!m3u8.contains("#EXT-X-ENDLIST"));

    // Verify Live cache control
    assert_eq!(
        po.manifest_cache_control(31536000, 1),
        "public, max-age=1, s-maxage=1"
    );

    // Phase 3: Add more segments → stay Live
    let manifest2 = po.add_segment(1, vec![0xBB; 48_000], 6.006);
    assert!(manifest2.is_some());
    assert_eq!(po.manifest_state().phase, ManifestPhase::Live);
    let m3u8_2 = manifest2.unwrap();
    assert!(m3u8_2.contains("/repackage/lifecycle-test/hls/segment_1.cmfv"));

    let manifest3 = po.add_segment(2, vec![0xCC; 52_000], 6.006);
    assert!(manifest3.is_some());
    assert_eq!(po.manifest_state().segments.len(), 3);

    // Phase 4: Finalize → transition to Complete
    let final_manifest = po.finalize();
    assert!(final_manifest.is_some());
    assert_eq!(po.manifest_state().phase, ManifestPhase::Complete);
    assert!(po.manifest_state().is_complete());

    let final_m3u8 = final_manifest.unwrap();
    assert!(final_m3u8.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(final_m3u8.contains("#EXT-X-ENDLIST"));
    assert!(final_m3u8.contains("segment_0.cmfv"));
    assert!(final_m3u8.contains("segment_1.cmfv"));
    assert!(final_m3u8.contains("segment_2.cmfv"));

    // Verify Complete cache control
    assert_eq!(
        po.manifest_cache_control(31536000, 1),
        "public, max-age=31536000, immutable"
    );
}

#[test]
fn progressive_output_dash_full_lifecycle() {
    let drm_info = ManifestDrmInfo {
        encryption_scheme: edge_packager::drm::scheme::EncryptionScheme::Cenc,
        widevine_pssh: Some("WVPSSH".into()),
        playready_pssh: Some("PRPSSH".into()),
        playready_pro: Some("<WRMHEADER></WRMHEADER>".into()),
        fairplay_key_uri: None,
        default_kid: "00112233445566778899aabbccddeeff".into(),
    };

    let mut po = ProgressiveOutput::new(
        "dash-lifecycle".into(),
        OutputFormat::Dash,
        "/repackage/dash-lifecycle/dash/".into(),
        drm_info,
    );

    po.set_init_segment(vec![0x00; 512]);

    // Add segments
    po.add_segment(0, vec![0xAA; 60_000], 6.0);
    assert_eq!(po.manifest_state().phase, ManifestPhase::Live);

    let live_manifest = po.current_manifest().unwrap();
    assert!(live_manifest.contains("type=\"dynamic\""));
    assert!(live_manifest.contains("minimumUpdatePeriod=\"PT1S\""));
    assert!(live_manifest.contains("<SegmentTimeline>"));

    po.add_segment(1, vec![0xBB; 58_000], 6.0);
    po.add_segment(2, vec![0xCC; 62_000], 6.0);

    // Finalize
    let final_manifest = po.finalize().unwrap();
    assert!(final_manifest.contains("type=\"static\""));
    assert!(final_manifest.contains("mediaPresentationDuration="));
    assert!(!final_manifest.contains("minimumUpdatePeriod"));
    assert!(final_manifest.contains("</MPD>"));
}

// ─── HLS Manifest DRM Signaling ─────────────────────────────────────

#[test]
fn hls_manifest_widevine_drm_signaling() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let m3u8 = manifest::render_manifest(&state).unwrap();

    // Widevine key format
    assert!(
        m3u8.contains("KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\""),
        "should contain Widevine key format UUID"
    );
    assert!(
        m3u8.contains("METHOD=SAMPLE-AES-CTR"),
        "should use SAMPLE-AES-CTR method for CENC"
    );
    assert!(
        m3u8.contains("KEYID=0x00112233445566778899aabbccddeeff"),
        "should include the default KID"
    );
}

#[test]
fn hls_manifest_playready_drm_signaling() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let m3u8 = manifest::render_manifest(&state).unwrap();

    // PlayReady key format
    assert!(
        m3u8.contains("KEYFORMAT=\"urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95\""),
        "should contain PlayReady key format UUID"
    );
}

#[test]
fn hls_manifest_two_key_entries() {
    let state = common::make_hls_manifest_state(2, ManifestPhase::Live);
    let m3u8 = manifest::render_manifest(&state).unwrap();

    let key_count = m3u8.matches("#EXT-X-KEY:").count();
    assert_eq!(
        key_count, 2,
        "should have two #EXT-X-KEY entries (Widevine + PlayReady)"
    );
}

#[test]
fn hls_manifest_init_segment_map() {
    let state = common::make_hls_manifest_state(1, ManifestPhase::Live);
    let m3u8 = manifest::render_manifest(&state).unwrap();

    assert!(
        m3u8.contains("#EXT-X-MAP:URI=\"/repackage/integration-test/hls/init.mp4\""),
        "should contain init segment map"
    );
}

#[test]
fn hls_target_duration_rounds_up() {
    let state = common::make_hls_manifest_state(1, ManifestPhase::Live);
    let m3u8 = manifest::render_manifest(&state).unwrap();

    // 6.006 should round up to 7
    assert!(
        m3u8.contains("#EXT-X-TARGETDURATION:7"),
        "target duration 6.006 should round up to 7"
    );
}

#[test]
fn hls_complete_manifest_has_all_segments() {
    let state = common::make_hls_manifest_state(5, ManifestPhase::Complete);
    let m3u8 = manifest::render_manifest(&state).unwrap();

    for i in 0..5 {
        assert!(
            m3u8.contains(&format!("segment_{i}.cmfv")),
            "should contain segment_{i}"
        );
    }

    let extinf_count = m3u8.matches("#EXTINF:").count();
    assert_eq!(extinf_count, 5, "should have 5 EXTINF entries");
}

// ─── DASH Manifest DRM Signaling ────────────────────────────────────

#[test]
fn dash_manifest_content_protection_elements() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    let mpd = manifest::render_manifest(&state).unwrap();

    // CENC default_KID (with hyphens in UUID format)
    assert!(
        mpd.contains("urn:mpeg:dash:mp4protection:2011"),
        "should contain DASH mp4protection URI"
    );
    assert!(
        mpd.contains("value=\"cenc\""),
        "should specify cenc scheme"
    );
    assert!(
        mpd.contains("cenc:default_KID=\"00112233-4455-6677-8899-aabbccddeeff\""),
        "should contain hyphenated KID"
    );
}

#[test]
fn dash_manifest_widevine_content_protection() {
    let state = common::make_dash_manifest_state(2, ManifestPhase::Live);
    let mpd = manifest::render_manifest(&state).unwrap();

    assert!(
        mpd.contains("urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"),
        "should contain Widevine scheme URI"
    );
    assert!(
        mpd.contains("<cenc:pssh>"),
        "should contain PSSH element"
    );
}

#[test]
fn dash_manifest_playready_content_protection() {
    let state = common::make_dash_manifest_state(2, ManifestPhase::Live);
    let mpd = manifest::render_manifest(&state).unwrap();

    assert!(
        mpd.contains("urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95"),
        "should contain PlayReady scheme URI"
    );
    assert!(
        mpd.contains("<mspr:pro>"),
        "should contain PlayReady PRO element"
    );
}

#[test]
fn dash_static_manifest_has_presentation_duration() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    let mpd = manifest::render_manifest(&state).unwrap();

    assert!(
        mpd.contains("type=\"static\""),
        "complete DASH manifest should be static"
    );
    assert!(
        mpd.contains("mediaPresentationDuration="),
        "static manifest should have presentation duration"
    );
    assert!(
        !mpd.contains("minimumUpdatePeriod"),
        "static manifest should not have update period"
    );
}

#[test]
fn dash_dynamic_manifest_has_update_period() {
    let state = common::make_dash_manifest_state(2, ManifestPhase::Live);
    let mpd = manifest::render_manifest(&state).unwrap();

    assert!(
        mpd.contains("type=\"dynamic\""),
        "live DASH manifest should be dynamic"
    );
    assert!(
        mpd.contains("minimumUpdatePeriod=\"PT1S\""),
        "dynamic manifest should have 1-second update period"
    );
}

#[test]
fn dash_manifest_segment_timeline() {
    let state = common::make_dash_manifest_state(4, ManifestPhase::Complete);
    let mpd = manifest::render_manifest(&state).unwrap();

    assert!(mpd.contains("<SegmentTimeline>"), "should have segment timeline");

    // Each segment is 6.0 seconds at 1000ms timescale = 6000
    let s_count = mpd.matches("<S d=\"6000\"/>").count();
    assert_eq!(s_count, 4, "should have 4 segment timeline entries");
}

#[test]
fn dash_manifest_namespaces() {
    let state = common::make_dash_manifest_state(1, ManifestPhase::Live);
    let mpd = manifest::render_manifest(&state).unwrap();

    assert!(mpd.contains("xmlns=\"urn:mpeg:dash:schema:mpd:2011\""));
    assert!(mpd.contains("xmlns:cenc=\"urn:mpeg:cenc:2013\""));
    assert!(mpd.contains("xmlns:mspr=\"urn:microsoft:playready\""));
}

#[test]
fn dash_manifest_profiles() {
    let state = common::make_dash_manifest_state(1, ManifestPhase::Live);
    let mpd = manifest::render_manifest(&state).unwrap();

    assert!(mpd.contains("urn:mpeg:dash:profile:isoff-live:2011"));
    assert!(mpd.contains("urn:mpeg:dash:profile:cmaf:2019"));
}

// ─── Cross-Format Consistency ───────────────────────────────────────

#[test]
fn same_content_renders_differently_for_hls_and_dash() {
    let hls_state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let dash_state = common::make_dash_manifest_state(3, ManifestPhase::Complete);

    let hls_manifest = manifest::render_manifest(&hls_state).unwrap();
    let dash_manifest = manifest::render_manifest(&dash_state).unwrap();

    // HLS markers
    assert!(hls_manifest.contains("#EXTM3U"));
    assert!(!hls_manifest.contains("<MPD"));

    // DASH markers
    assert!(dash_manifest.contains("<MPD"));
    assert!(!dash_manifest.contains("#EXTM3U"));
}

#[test]
fn manifest_state_serialization_roundtrip() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);

    let json = serde_json::to_string(&state).expect("should serialize");
    let parsed: ManifestState =
        serde_json::from_str(&json).expect("should deserialize");

    assert_eq!(parsed.content_id, state.content_id);
    assert_eq!(parsed.format, state.format);
    assert_eq!(parsed.phase, state.phase);
    assert_eq!(parsed.segments.len(), state.segments.len());
    assert!(parsed.drm_info.is_some());
    assert_eq!(
        parsed.drm_info.as_ref().unwrap().default_kid,
        state.drm_info.as_ref().unwrap().default_kid
    );
}

// ─── Cache-Control Headers ──────────────────────────────────────────

#[test]
fn segment_cache_control_is_immutable() {
    let cc = ProgressiveOutput::segment_cache_control(31536000);
    assert_eq!(cc, "public, max-age=31536000, immutable");
}

#[test]
fn segment_cache_control_custom_max_age() {
    let cc = ProgressiveOutput::segment_cache_control(86400);
    assert_eq!(cc, "public, max-age=86400, immutable");
}
