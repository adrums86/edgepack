//! Integration tests for Phase 13: DVR Window & Time-Shift
//!
//! Tests cover:
//! - HLS DVR window rendering (sliding window, media sequence, playlist type)
//! - DASH DVR window rendering (timeShiftBufferDepth, startNumber, SegmentTimeline)
//! - DVR with DRM, I-frame playlists, ad breaks, and LL-HLS parts
//! - Live-to-VOD transition (Complete phase renders all segments)
//! - Serde backward compatibility
//! - Webhook validation (positive duration required)
//! - Container format variations

mod common;

use edgepack::manifest::hls;
use edgepack::manifest::dash;
use edgepack::manifest::types::*;
use edgepack::repackager::RepackageRequest;
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::media::container::ContainerFormat;

// ─── HLS DVR Window Rendering ───────────────────────────────────

#[test]
fn hls_dvr_window_live_omits_event_type() {
    // DVR window active → no PLAYLIST-TYPE:EVENT (allows sliding window)
    let state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    let m3u8 = hls::render(&state).unwrap();
    assert!(!m3u8.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    assert!(!m3u8.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
}

#[test]
fn hls_dvr_window_live_media_sequence() {
    // 10 segments * 6s = 60s. Window = 30s → 5 segments visible, starting at segment 5.
    let state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    let m3u8 = hls::render(&state).unwrap();
    assert!(m3u8.contains("#EXT-X-MEDIA-SEQUENCE:5"));
}

#[test]
fn hls_dvr_window_live_renders_only_windowed_segments() {
    let state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    let m3u8 = hls::render(&state).unwrap();

    // Segments 0-4 should not appear, segments 5-9 should
    assert!(!m3u8.contains("segment_0.cmfv"));
    assert!(!m3u8.contains("segment_4.cmfv"));
    assert!(m3u8.contains("segment_5.cmfv"));
    assert!(m3u8.contains("segment_9.cmfv"));

    // Count EXTINF entries — should be 5
    let extinf_count = m3u8.matches("#EXTINF:").count();
    assert_eq!(extinf_count, 5);
}

#[test]
fn hls_dvr_window_complete_renders_all_segments() {
    // Complete phase ignores window — renders all segments for VOD
    let state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Complete, 30.0);
    let m3u8 = hls::render(&state).unwrap();

    assert!(m3u8.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(m3u8.contains("#EXT-X-ENDLIST"));
    assert!(m3u8.contains("segment_0.cmfv"));
    assert!(m3u8.contains("segment_9.cmfv"));
    assert!(m3u8.contains("#EXT-X-MEDIA-SEQUENCE:0"));

    let extinf_count = m3u8.matches("#EXTINF:").count();
    assert_eq!(extinf_count, 10);
}

#[test]
fn hls_dvr_window_larger_than_total_renders_all() {
    // Window (3600s) larger than total content (60s) → all segments rendered
    let state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 3600.0);
    let m3u8 = hls::render(&state).unwrap();

    assert!(m3u8.contains("segment_0.cmfv"));
    assert!(m3u8.contains("segment_9.cmfv"));
    assert!(m3u8.contains("#EXT-X-MEDIA-SEQUENCE:0"));

    let extinf_count = m3u8.matches("#EXTINF:").count();
    assert_eq!(extinf_count, 10);
}

#[test]
fn hls_dvr_window_no_window_event_type() {
    // No DVR window → EVENT playlist type (all segments stay)
    let state = common::make_hls_manifest_state(10, ManifestPhase::Live);
    let m3u8 = hls::render(&state).unwrap();
    assert!(m3u8.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    assert!(m3u8.contains("#EXT-X-MEDIA-SEQUENCE:0"));
}

#[test]
fn hls_dvr_window_with_drm() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
        widevine_pssh: Some("WV_PSSH_DVR".into()),
        playready_pssh: None,
        playready_pro: None,
        fairplay_key_uri: None,
        default_kid: "00112233445566778899aabbccddeeff".into(),
        clearkey_pssh: None,
    });

    let m3u8 = hls::render(&state).unwrap();
    // Should have DRM KEY tags but only windowed segments
    assert!(m3u8.contains("#EXT-X-KEY:METHOD=SAMPLE-AES-CTR"));
    assert!(!m3u8.contains("segment_0.cmfv"));
    assert!(m3u8.contains("segment_5.cmfv"));
}

#[test]
fn hls_dvr_window_iframe_playlist_windowed() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.enable_iframe_playlist = true;
    for i in 0..10u32 {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 5000 + (i as u64 * 100),
            duration: 6.006,
            segment_uri: format!("/repackage/integration-test/hls/segment_{i}.cmfv"),
        });
    }

    let iframe_m3u8 = hls::render_iframe_playlist(&state).unwrap().unwrap();
    // Should only include I-frames for windowed segments (5-9)
    assert!(iframe_m3u8.contains("#EXT-X-I-FRAMES-ONLY"));
    assert!(!iframe_m3u8.contains("segment_0.cmfv"));
    assert!(!iframe_m3u8.contains("segment_4.cmfv"));
    assert!(iframe_m3u8.contains("segment_5.cmfv"));
    assert!(iframe_m3u8.contains("segment_9.cmfv"));
    // DVR active → no EVENT type
    assert!(!iframe_m3u8.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    // Media sequence should match window start
    assert!(iframe_m3u8.contains("#EXT-X-MEDIA-SEQUENCE:5"));
}

#[test]
fn hls_dvr_window_with_ad_breaks() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    // Add ad breaks: one inside window (seg 7), one outside (seg 2)
    state.ad_breaks.push(AdBreakInfo {
        id: 1,
        presentation_time: 12.0,
        duration: Some(15.0),
        scte35_cmd: None,
        segment_number: 2,
    });
    state.ad_breaks.push(AdBreakInfo {
        id: 2,
        presentation_time: 42.0,
        duration: Some(10.0),
        scte35_cmd: None,
        segment_number: 7,
    });

    let m3u8 = hls::render(&state).unwrap();
    // Ad break at segment 2 should not appear (outside window)
    assert!(!m3u8.contains("splice-1"));
    // Ad break at segment 7 should appear (inside window)
    assert!(m3u8.contains("splice-2"));
}

// ─── DASH DVR Window Rendering ──────────────────────────────────

#[test]
fn dash_dvr_window_live_time_shift_buffer_depth() {
    let state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Live, 3600.0);
    let mpd = dash::render(&state).unwrap();
    assert!(mpd.contains("timeShiftBufferDepth=\"PT1H\""));
}

#[test]
fn dash_dvr_window_live_renders_windowed_segments() {
    let state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    let mpd = dash::render(&state).unwrap();

    // Should only have 5 <S entries (first has @t, rest are d-only)
    let s_entries = mpd.matches("<S ").count();
    assert_eq!(s_entries, 5);
}

#[test]
fn dash_dvr_window_live_start_number() {
    let state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    let mpd = dash::render(&state).unwrap();

    // startNumber should be 5 (first segment in the window)
    assert!(mpd.contains("startNumber=\"5\""));
}

#[test]
fn dash_dvr_window_first_s_has_t_attribute() {
    let state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    let mpd = dash::render(&state).unwrap();

    // DVR window: segments 5-9. Cumulative duration of segments 0-4 = 5 × 6000ms = 30000ms.
    // Per ISO 23009-1, first <S> must have @t to avoid implicit t=0 mismatch.
    assert!(
        mpd.contains("<S t=\"30000\" d=\"6000\"/>"),
        "first <S> must have @t for DVR windowed timeline"
    );
    // Only the first <S> should have @t
    assert_eq!(mpd.matches("<S t=").count(), 1);
}

#[test]
fn dash_dvr_window_complete_no_t_attribute() {
    let state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Complete, 30.0);
    let mpd = dash::render(&state).unwrap();
    // Complete phase: startNumber=0, no @t needed
    assert!(!mpd.contains("<S t="), "Complete phase should not have @t");
}

#[test]
fn dash_dvr_window_complete_no_tsbd_all_segments() {
    let state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Complete, 30.0);
    let mpd = dash::render(&state).unwrap();

    // Complete phase: no timeShiftBufferDepth, type=static, all segments
    assert!(!mpd.contains("timeShiftBufferDepth"));
    assert!(mpd.contains("type=\"static\""));
    assert!(mpd.contains("startNumber=\"0\""));

    let s_entries = mpd.matches("<S d=\"").count();
    assert_eq!(s_entries, 10);
}

#[test]
fn dash_dvr_window_no_window_no_tsbd() {
    let state = common::make_dash_manifest_state(10, ManifestPhase::Live);
    let mpd = dash::render(&state).unwrap();

    assert!(!mpd.contains("timeShiftBufferDepth"));
    assert!(mpd.contains("type=\"dynamic\""));
    assert!(mpd.contains("startNumber=\"0\""));
}

#[test]
fn dash_dvr_window_larger_than_total() {
    let state = common::make_dash_dvr_manifest_state(5, ManifestPhase::Live, 3600.0);
    let mpd = dash::render(&state).unwrap();

    assert!(mpd.contains("startNumber=\"0\""));
    let s_entries = mpd.matches("<S d=\"").count();
    assert_eq!(s_entries, 5);
}

#[test]
fn dash_dvr_window_with_ad_breaks() {
    let mut state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.ad_breaks.push(AdBreakInfo {
        id: 1,
        presentation_time: 12.0,
        duration: Some(15.0),
        scte35_cmd: None,
        segment_number: 2,
    });
    state.ad_breaks.push(AdBreakInfo {
        id: 2,
        presentation_time: 42.0,
        duration: Some(10.0),
        scte35_cmd: None,
        segment_number: 7,
    });

    let mpd = dash::render(&state).unwrap();
    // Only ad break at seg 7 (inside window) should appear
    assert!(!mpd.contains("id=\"1\""));
    assert!(mpd.contains("id=\"2\""));
}

#[test]
fn dash_dvr_window_tsbd_short_duration() {
    let state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    let mpd = dash::render(&state).unwrap();
    assert!(mpd.contains("timeShiftBufferDepth=\"PT30.000S\""));
}

// ─── Container Format Variations ────────────────────────────────

#[test]
fn hls_dvr_window_fmp4_format() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.container_format = ContainerFormat::Fmp4;
    // Re-build segment URIs for fmp4
    state.segments.clear();
    for i in 0..10 {
        state.segments.push(SegmentInfo {
            number: i,
            duration: 6.0,
            uri: format!("/repackage/integration-test/hls/segment_{i}.m4s"),
            byte_size: 1024,
            key_period: None,
        });
    }
    let m3u8 = hls::render(&state).unwrap();
    assert!(!m3u8.contains("segment_0.m4s"));
    assert!(m3u8.contains("segment_5.m4s"));
}

// ─── Serde & Backward Compat ────────────────────────────────────

#[test]
fn dvr_window_manifest_state_serde_roundtrip() {
    let state = common::make_hls_dvr_manifest_state(5, ManifestPhase::Live, 3600.0);
    let json = serde_json::to_string(&state).unwrap();
    let parsed: ManifestState = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.dvr_window_duration, Some(3600.0));
    assert_eq!(parsed.segments.len(), 5);
}

#[test]
fn dvr_window_manifest_state_backward_compat() {
    // Old JSON without dvr_window_duration should deserialize with None
    let json = r#"{"content_id":"c","format":"Hls","phase":"Live","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
    let parsed: ManifestState = serde_json::from_str(json).unwrap();
    assert!(parsed.dvr_window_duration.is_none());
    assert!(!parsed.is_dvr_active());
}

#[test]
fn dvr_window_repackage_request_serde_roundtrip() {
    let req = RepackageRequest {
        content_id: "dvr-test".into(),
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
        dvr_window_duration: Some(1800.0),
        content_steering: None,
        cache_control: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.dvr_window_duration, Some(1800.0));
}

#[test]
fn dvr_window_repackage_request_backward_compat() {
    let json = r#"{"content_id":"test","source_url":"https://example.com","output_formats":["Hls"],"key_ids":[]}"#;
    let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
    assert!(parsed.dvr_window_duration.is_none());
}

// ─── DVR with Other Features ────────────────────────────────────

#[test]
fn hls_dvr_window_with_clear_lead() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.clear_lead_boundary = Some(3);
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
        widevine_pssh: Some("WV_PSSH".into()),
        playready_pssh: None,
        playready_pro: None,
        fairplay_key_uri: None,
        default_kid: "00112233445566778899aabbccddeeff".into(),
        clearkey_pssh: None,
    });

    let m3u8 = hls::render(&state).unwrap();
    // Window starts at segment 5, which is past clear_lead_boundary (3).
    // Clear lead transition should NOT appear since it's before the window.
    assert!(!m3u8.contains("segment_2.cmfv"));
    assert!(m3u8.contains("segment_5.cmfv"));
}

#[test]
fn hls_dvr_window_with_key_rotation() {
    let mut state = common::make_hls_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.rotation_drm_info = vec![
        ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV_PSSH_0".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "00112233445566778899aabbccddeeff".into(),
            clearkey_pssh: None,
        },
    ];
    // Set key_period on windowed segments
    for seg in &mut state.segments {
        seg.key_period = Some(seg.number / 5);
    }

    let m3u8 = hls::render(&state).unwrap();
    // Should render KEY tags for the windowed segments' key periods
    assert!(m3u8.contains("#EXT-X-KEY:METHOD=SAMPLE-AES-CTR"));
    assert!(!m3u8.contains("segment_0.cmfv"));
}

#[test]
fn dash_dvr_window_with_trick_play() {
    let mut state = common::make_dash_dvr_manifest_state(10, ManifestPhase::Live, 30.0);
    state.enable_iframe_playlist = true;
    for i in 0..10u32 {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 5000,
            duration: 6.0,
            segment_uri: format!("/repackage/integration-test/dash/segment_{i}.cmfv"),
        });
    }

    let mpd = dash::render(&state).unwrap();
    // Trick play AdaptationSet should still appear
    assert!(mpd.contains("http://dashif.org/guidelines/trickmode"));
    // Main video timeline should only have 5 entries
    let s_entries = mpd.matches("<S ").count();
    // Two SegmentTemplates (main + trick) each with 5 entries = 10
    // (first <S> in each has @t attribute due to DVR)
    assert_eq!(s_entries, 10);
}
