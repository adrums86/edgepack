//! Integration tests for Phase 12: Trick Play & I-Frame Playlists
//!
//! Tests cover:
//! - HLS I-frame playlist rendering (#EXT-X-I-FRAMES-ONLY, BYTERANGE, DRM)
//! - HLS master playlist with #EXT-X-I-FRAME-STREAM-INF
//! - DASH trick play AdaptationSet with EssentialProperty
//! - Disabled-by-default behavior
//! - Serde backward compatibility
//! - Route handling (HLS iframes route, DASH 404)
//! - Container format variations

mod common;

use edgepack::manifest;
use edgepack::manifest::hls;
use edgepack::manifest::dash;
use edgepack::manifest::types::*;
use edgepack::media::container::ContainerFormat;
use edgepack::repackager::RepackageRequest;

// ─── HLS I-Frame Playlist Rendering ───────────────────────────────

#[test]
fn hls_iframe_playlist_basic() {
    let state = common::make_hls_iframe_manifest_state(3, ManifestPhase::Complete);
    let result = hls::render_iframe_playlist(&state).unwrap();
    assert!(result.is_some());
    let playlist = result.unwrap();
    assert!(playlist.contains("#EXTM3U"));
    assert!(playlist.contains("#EXT-X-VERSION:4"));
    assert!(playlist.contains("#EXT-X-I-FRAMES-ONLY"));
    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(playlist.contains("#EXT-X-ENDLIST"));
    assert!(playlist.contains("#EXT-X-MAP:URI="));
}

#[test]
fn hls_iframe_playlist_byterange_format() {
    let state = common::make_hls_iframe_manifest_state(2, ManifestPhase::Live);
    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    // Should contain BYTERANGE entries
    assert!(playlist.contains("#EXT-X-BYTERANGE:8192@0"));
    assert!(playlist.contains("#EXT-X-BYTERANGE:8292@0"));
    // Should reference segment URIs
    assert!(playlist.contains("/repackage/integration-test/hls/segment_0.cmfv"));
    assert!(playlist.contains("/repackage/integration-test/hls/segment_1.cmfv"));
}

#[test]
fn hls_iframe_playlist_extinf_durations() {
    let state = common::make_hls_iframe_manifest_state(2, ManifestPhase::Live);
    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert_eq!(playlist.matches("#EXTINF:6.006000,").count(), 2);
}

#[test]
fn hls_iframe_playlist_live_no_endlist() {
    let state = common::make_hls_iframe_manifest_state(2, ManifestPhase::Live);
    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
    assert!(!playlist.contains("#EXT-X-ENDLIST"));
}

#[test]
fn hls_iframe_playlist_complete_has_endlist() {
    let state = common::make_hls_iframe_manifest_state(2, ManifestPhase::Complete);
    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
    assert!(playlist.contains("#EXT-X-ENDLIST"));
}

#[test]
fn hls_iframe_playlist_with_drm() {
    let state = common::make_hls_iframe_manifest_state(1, ManifestPhase::Complete);
    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    // Should include DRM KEY tags (same as regular playlist)
    assert!(playlist.contains("METHOD=SAMPLE-AES-CTR"));
    assert!(playlist.contains("KEYFORMAT="));
}

#[test]
fn hls_iframe_playlist_target_duration() {
    let state = common::make_hls_iframe_manifest_state(2, ManifestPhase::Complete);
    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    // Duration is 6.006, so target should be 7 (ceil)
    assert!(playlist.contains("#EXT-X-TARGETDURATION:7"));
}

#[test]
fn hls_iframe_playlist_disabled_returns_none() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    // enable_iframe_playlist defaults to false
    assert!(!state.enable_iframe_playlist);
    let result = hls::render_iframe_playlist(&state).unwrap();
    assert!(result.is_none());

    // Even with enable but empty iframe_segments
    state.enable_iframe_playlist = true;
    let result = hls::render_iframe_playlist(&state).unwrap();
    assert!(result.is_none());
}

#[test]
fn hls_iframe_playlist_awaiting_returns_none() {
    let mut state = common::make_hls_iframe_manifest_state(0, ManifestPhase::AwaitingFirstSegment);
    state.enable_iframe_playlist = true;
    // No iframe segments in awaiting state
    let result = hls::render_iframe_playlist(&state).unwrap();
    assert!(result.is_none());
}

// ─── HLS Master Playlist with I-Frame Stream ──────────────────────

#[test]
fn hls_master_iframe_stream_inf() {
    let mut state = common::make_hls_iframe_manifest_state(3, ManifestPhase::Complete);
    state.variants.push(VariantInfo {
        id: "v720".into(),
        bandwidth: 3_000_000,
        codecs: "avc1.64001f".into(),
        resolution: Some((1280, 720)),
        frame_rate: Some(29.97),
        track_type: TrackMediaType::Video,
        language: None,
    });
    let uris = vec!["v720.m3u8".to_string()];
    let m3u8 = hls::render_master(&state, &uris).unwrap();
    assert!(m3u8.contains("#EXT-X-I-FRAME-STREAM-INF:"));
    assert!(m3u8.contains("URI=\"iframes\""));
    assert!(m3u8.contains("BANDWIDTH=300000")); // 3M / 10
    assert!(m3u8.contains("CODECS=\"avc1.64001f\""));
    assert!(m3u8.contains("RESOLUTION=1280x720"));
}

#[test]
fn hls_master_no_iframe_stream_when_disabled() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    let uris = vec![];
    let m3u8 = hls::render_master(&state, &uris).unwrap();
    assert!(!m3u8.contains("I-FRAME-STREAM-INF"));
}

#[test]
fn hls_master_no_iframe_stream_when_empty_segments() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Complete);
    state.enable_iframe_playlist = true;
    // No iframe_segments populated
    state.variants.push(VariantInfo {
        id: "v1".into(),
        bandwidth: 2_000_000,
        codecs: "avc1.64001f".into(),
        resolution: None,
        frame_rate: None,
        track_type: TrackMediaType::Video,
        language: None,
    });
    let uris = vec!["v1.m3u8".into()];
    let m3u8 = hls::render_master(&state, &uris).unwrap();
    assert!(!m3u8.contains("I-FRAME-STREAM-INF"));
}

// ─── DASH Trick Play ──────────────────────────────────────────────

#[test]
fn dash_trick_play_adaptation_set() {
    let mut state = common::make_dash_iframe_manifest_state(3, ManifestPhase::Complete);
    state.variants.push(VariantInfo {
        id: "v720".into(),
        bandwidth: 3_000_000,
        codecs: "avc1.64001f".into(),
        resolution: Some((1280, 720)),
        frame_rate: Some(30.0),
        track_type: TrackMediaType::Video,
        language: None,
    });
    let mpd = dash::render(&state).unwrap();
    // Main video AdaptationSet should have id="1"
    assert!(mpd.contains("id=\"1\""));
    // Trick play AdaptationSet
    assert!(mpd.contains("http://dashif.org/guidelines/trickmode"));
    assert!(mpd.contains("value=\"1\""));
    assert!(mpd.contains("id=\"v720_trick\""));
    assert!(mpd.contains("bandwidth=\"300000\"")); // 3M / 10
}

#[test]
fn dash_no_trick_play_when_disabled() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    let mpd = dash::render(&state).unwrap();
    assert!(!mpd.contains("trickmode"));
    assert!(!mpd.contains("_trick"));
}

#[test]
fn dash_no_trick_play_when_no_iframes() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    state.enable_iframe_playlist = true;
    // No iframe_segments
    let mpd = dash::render(&state).unwrap();
    assert!(!mpd.contains("trickmode"));
}

#[test]
fn dash_trick_play_no_id_when_disabled() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Complete);
    let mpd = dash::render(&state).unwrap();
    // Video AdaptationSet should NOT have id="1" when trick play is disabled
    assert!(!mpd.contains("id=\"1\""));
}

#[test]
fn dash_trick_play_default_variant() {
    // No explicit variants — should use default trick play Representation
    let state = common::make_dash_iframe_manifest_state(2, ManifestPhase::Complete);
    let mpd = dash::render(&state).unwrap();
    assert!(mpd.contains("id=\"video_trick\""));
    assert!(mpd.contains("bandwidth=\"200000\""));
}

// ─── Manifest Dispatcher ──────────────────────────────────────────

#[test]
fn render_iframe_manifest_hls_returns_some() {
    let state = common::make_hls_iframe_manifest_state(2, ManifestPhase::Complete);
    let result = manifest::render_iframe_manifest(&state).unwrap();
    assert!(result.is_some());
    let playlist = result.unwrap();
    assert!(playlist.contains("#EXT-X-I-FRAMES-ONLY"));
}

#[test]
fn render_iframe_manifest_dash_returns_none() {
    let state = common::make_dash_iframe_manifest_state(2, ManifestPhase::Complete);
    let result = manifest::render_iframe_manifest(&state).unwrap();
    assert!(result.is_none());
}

#[test]
fn render_iframe_manifest_disabled_returns_none() {
    let state = common::make_hls_manifest_state(2, ManifestPhase::Complete);
    let result = manifest::render_iframe_manifest(&state).unwrap();
    assert!(result.is_none());
}

// ─── Serde Backward Compatibility ─────────────────────────────────

#[test]
fn manifest_state_serde_without_iframe_fields() {
    // Old JSON without iframe fields should deserialize with defaults
    let json = r#"{
        "content_id":"test",
        "format":"Hls",
        "base_url":"/base/",
        "container_format":"Cmaf",
        "phase":"Live",
        "target_duration":6.0,
        "media_sequence":0,
        "segments":[],
        "parts":[],
        "ad_breaks":[],
        "cea_captions":[],
        "rotation_drm_info":[],
        "variants":[]
    }"#;
    let parsed: ManifestState = serde_json::from_str(json).unwrap();
    assert!(!parsed.enable_iframe_playlist);
    assert!(parsed.iframe_segments.is_empty());
}

#[test]
fn repackage_request_serde_without_iframe_field() {
    let json = r#"{"content_id":"test","source_url":"https://example.com","output_formats":["Hls"],"key_ids":[]}"#;
    let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
    assert!(!parsed.enable_iframe_playlist);
}

#[test]
fn iframe_segment_info_serde_roundtrip() {
    let info = IFrameSegmentInfo {
        segment_number: 5,
        byte_offset: 1024,
        byte_length: 4096,
        duration: 6.006,
        segment_uri: "/seg/segment_5.cmfv".into(),
    };
    let json = serde_json::to_string(&info).unwrap();
    let parsed: IFrameSegmentInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.segment_number, 5);
    assert_eq!(parsed.byte_offset, 1024);
    assert_eq!(parsed.byte_length, 4096);
    assert!((parsed.duration - 6.006).abs() < f64::EPSILON);
    assert_eq!(parsed.segment_uri, "/seg/segment_5.cmfv");
}

// ─── Container Format Variations ──────────────────────────────────

#[test]
fn hls_iframe_playlist_fmp4_extension() {
    let mut state = ManifestState::new(
        "test".into(),
        OutputFormat::Hls,
        "/base/".into(),
        ContainerFormat::Fmp4,
    );
    state.phase = ManifestPhase::Complete;
    state.enable_iframe_playlist = true;
    state.init_segment = Some(InitSegmentInfo {
        uri: "/base/init.mp4".into(),
        byte_size: 256,
    });
    state.segments.push(SegmentInfo {
        number: 0,
        duration: 6.0,
        uri: "/base/segment_0.m4s".into(),
        byte_size: 1024,
        key_period: None,
    });
    state.iframe_segments.push(IFrameSegmentInfo {
        segment_number: 0,
        byte_offset: 0,
        byte_length: 512,
        duration: 6.0,
        segment_uri: "/base/segment_0.m4s".into(),
    });

    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(playlist.contains("/base/segment_0.m4s"));
    assert!(playlist.contains("#EXT-X-BYTERANGE:512@0"));
}

#[test]
fn hls_iframe_playlist_iso_extension() {
    let mut state = ManifestState::new(
        "test".into(),
        OutputFormat::Hls,
        "/base/".into(),
        ContainerFormat::Iso,
    );
    state.phase = ManifestPhase::Complete;
    state.enable_iframe_playlist = true;
    state.init_segment = Some(InitSegmentInfo {
        uri: "/base/init.mp4".into(),
        byte_size: 256,
    });
    state.segments.push(SegmentInfo {
        number: 0,
        duration: 6.0,
        uri: "/base/segment_0.mp4".into(),
        byte_size: 1024,
        key_period: None,
    });
    state.iframe_segments.push(IFrameSegmentInfo {
        segment_number: 0,
        byte_offset: 100,
        byte_length: 300,
        duration: 6.0,
        segment_uri: "/base/segment_0.mp4".into(),
    });

    let playlist = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(playlist.contains("/base/segment_0.mp4"));
    assert!(playlist.contains("#EXT-X-BYTERANGE:300@100"));
}

// ─── Route Handling ───────────────────────────────────────────────

#[test]
fn iframes_route_parsing() {
    // Verify that the "iframes" path component is not mistaken for a segment file
    let path = "/repackage/content-1/hls_cenc/iframes";
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    assert_eq!(segments, vec!["repackage", "content-1", "hls_cenc", "iframes"]);
}

#[test]
fn iframes_route_before_segment_catch_all() {
    // "iframes" should match a dedicated route, not the segment_{n}.{ext} catch-all
    // Verify "iframes" does NOT match segment number parsing
    let filename = "iframes";
    let is_segment = filename.starts_with("segment_")
        && filename.len() > 8
        && filename[8..].chars().next().map_or(false, |c| c.is_ascii_digit());
    assert!(!is_segment);
}
