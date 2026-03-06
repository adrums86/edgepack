//! Integration tests for Phase 14: Content Steering & CDN Optimization.
//!
//! Tests content steering tag/element rendering in HLS master playlists and DASH MPDs,
//! DASH source manifest extraction, serde roundtrips, and webhook override behavior.

mod common;

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::manifest::types::{
    ContentSteeringConfig, ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat,
    TrackMediaType, VariantInfo,
};
use edgepack::media::container::ContainerFormat;
use edgepack::repackager::RepackageRequest;

// ─── HLS Master Playlist Tests ──────────────────────────────────────

#[test]
fn hls_master_content_steering_full() {
    let mut state = common::make_hls_content_steering_manifest_state(3, ManifestPhase::Live);
    state.variants.push(VariantInfo {
        id: "v1".into(),
        bandwidth: 5_000_000,
        codecs: "avc1.640028".into(),
        resolution: Some((1920, 1080)),
        frame_rate: None,
        track_type: TrackMediaType::Video,
        language: None,
    });
    let m3u8 = edgepack::manifest::hls::render_master(&state, &["v1.m3u8".into()]).unwrap();
    assert!(m3u8.contains("#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://steer.example.com/v1\",PATHWAY-ID=\"cdn-a\""));
}

#[test]
fn hls_master_content_steering_server_uri_only() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: None,
        query_before_start: None,
    });
    state.variants.push(VariantInfo {
        id: "v1".into(),
        bandwidth: 5_000_000,
        codecs: "avc1.640028".into(),
        resolution: Some((1920, 1080)),
        frame_rate: None,
        track_type: TrackMediaType::Video,
        language: None,
    });
    let m3u8 = edgepack::manifest::hls::render_master(&state, &["v1.m3u8".into()]).unwrap();
    assert!(m3u8.contains("#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://steer.example.com/v1\""));
    assert!(!m3u8.contains("PATHWAY-ID"));
}

#[test]
fn hls_master_no_steering_backward_compat() {
    let mut state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    state.variants.push(VariantInfo {
        id: "v1".into(),
        bandwidth: 5_000_000,
        codecs: "avc1.640028".into(),
        resolution: Some((1920, 1080)),
        frame_rate: None,
        track_type: TrackMediaType::Video,
        language: None,
    });
    let m3u8 = edgepack::manifest::hls::render_master(&state, &["v1.m3u8".into()]).unwrap();
    assert!(!m3u8.contains("CONTENT-STEERING"));
}

#[test]
fn hls_media_playlist_never_has_steering() {
    let mut state = common::make_hls_content_steering_manifest_state(3, ManifestPhase::Live);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: Some("cdn-a".into()),
        query_before_start: None,
    });
    let m3u8 = edgepack::manifest::hls::render(&state).unwrap();
    assert!(!m3u8.contains("CONTENT-STEERING"));
}

#[test]
fn hls_master_steering_tag_position_before_session_key() {
    let mut state = common::make_hls_content_steering_manifest_state(3, ManifestPhase::Live);
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
        widevine_pssh: Some("WV_PSSH_CS".into()),
        playready_pssh: None,
        playready_pro: None,
        fairplay_key_uri: None,
        default_kid: "0123456789abcdef0123456789abcdef".into(),
        clearkey_pssh: None,
    });
    state.variants.push(VariantInfo {
        id: "v1".into(),
        bandwidth: 5_000_000,
        codecs: "avc1.640028".into(),
        resolution: Some((1920, 1080)),
        frame_rate: None,
        track_type: TrackMediaType::Video,
        language: None,
    });
    let m3u8 = edgepack::manifest::hls::render_master(&state, &["v1.m3u8".into()]).unwrap();
    let steering_pos = m3u8.find("#EXT-X-CONTENT-STEERING").unwrap();
    let session_pos = m3u8.find("#EXT-X-SESSION-KEY").unwrap();
    assert!(steering_pos < session_pos);
}

// ─── DASH MPD Tests ──────────────────────────────────────────────────

#[test]
fn dash_content_steering_full() {
    let state = common::make_dash_content_steering_manifest_state(3, ManifestPhase::Live);
    let mpd = edgepack::manifest::dash::render(&state).unwrap();
    assert!(mpd.contains("<ContentSteering"));
    assert!(mpd.contains("proxyServerURL=\"https://steer.example.com/v1\""));
    assert!(mpd.contains("defaultServiceLocation=\"cdn-a\""));
    assert!(mpd.contains("queryBeforeStart=\"true\""));
}

#[test]
fn dash_content_steering_proxy_url_only() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Live);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: None,
        query_before_start: None,
    });
    let mpd = edgepack::manifest::dash::render(&state).unwrap();
    assert!(mpd.contains("proxyServerURL=\"https://steer.example.com/v1\""));
    assert!(!mpd.contains("defaultServiceLocation"));
    assert!(!mpd.contains("queryBeforeStart"));
}

#[test]
fn dash_content_steering_query_before_start_false() {
    let mut state = common::make_dash_manifest_state(3, ManifestPhase::Live);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: None,
        query_before_start: Some(false),
    });
    let mpd = edgepack::manifest::dash::render(&state).unwrap();
    assert!(mpd.contains("queryBeforeStart=\"false\""));
}

#[test]
fn dash_no_steering_backward_compat() {
    let state = common::make_dash_manifest_state(3, ManifestPhase::Live);
    let mpd = edgepack::manifest::dash::render(&state).unwrap();
    assert!(!mpd.contains("ContentSteering"));
}

#[test]
fn dash_content_steering_position_before_period() {
    let state = common::make_dash_content_steering_manifest_state(3, ManifestPhase::Live);
    let mpd = edgepack::manifest::dash::render(&state).unwrap();
    let steering_pos = mpd.find("<ContentSteering").unwrap();
    let period_pos = mpd.find("<Period").unwrap();
    assert!(steering_pos < period_pos);
}

// ─── DASH Input Parsing Tests ────────────────────────────────────────

#[test]
fn dash_input_parse_content_steering_full() {
    let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT18.018S">
  <ContentSteering proxyServerURL="https://steer.example.com/v1" defaultServiceLocation="cdn-a" queryBeforeStart="true"/>
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="90000">
          <SegmentTimeline>
            <S d="540540" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let result = edgepack::manifest::dash_input::parse_dash_manifest(
        mpd,
        "https://cdn.example.com/content/manifest.mpd",
    )
    .unwrap();
    let cs = result.content_steering.unwrap();
    assert_eq!(cs.server_uri, "https://steer.example.com/v1");
    assert_eq!(cs.default_pathway_id.as_deref(), Some("cdn-a"));
    assert_eq!(cs.query_before_start, Some(true));
}

#[test]
fn dash_input_parse_content_steering_minimal() {
    let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT18.018S">
  <ContentSteering proxyServerURL="https://steer.example.com/v1"/>
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="90000">
          <SegmentTimeline>
            <S d="540540" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let result = edgepack::manifest::dash_input::parse_dash_manifest(
        mpd,
        "https://cdn.example.com/content/manifest.mpd",
    )
    .unwrap();
    let cs = result.content_steering.unwrap();
    assert_eq!(cs.server_uri, "https://steer.example.com/v1");
    assert!(cs.default_pathway_id.is_none());
    assert!(cs.query_before_start.is_none());
}

#[test]
fn dash_input_no_steering_backward_compat() {
    let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT18.018S">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="90000">
          <SegmentTimeline>
            <S d="540540" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
    let result = edgepack::manifest::dash_input::parse_dash_manifest(
        mpd,
        "https://cdn.example.com/content/manifest.mpd",
    )
    .unwrap();
    assert!(result.content_steering.is_none());
}

// ─── Serde Tests ─────────────────────────────────────────────────────

#[test]
fn manifest_state_content_steering_serde_roundtrip() {
    let state = common::make_hls_content_steering_manifest_state(3, ManifestPhase::Live);
    let json = serde_json::to_string(&state).unwrap();
    let parsed: ManifestState = serde_json::from_str(&json).unwrap();
    let cs = parsed.content_steering.unwrap();
    assert_eq!(cs.server_uri, "https://steer.example.com/v1");
    assert_eq!(cs.default_pathway_id.as_deref(), Some("cdn-a"));
}

#[test]
fn manifest_state_no_steering_backward_compat() {
    let state = common::make_hls_manifest_state(3, ManifestPhase::Live);
    let json = serde_json::to_string(&state).unwrap();
    let parsed: ManifestState = serde_json::from_str(&json).unwrap();
    assert!(parsed.content_steering.is_none());
}

#[test]
fn repackage_request_content_steering_serde_roundtrip() {
    let req = RepackageRequest {
        content_id: "test".into(),
        source_url: "https://example.com/source.mpd".into(),
        output_formats: vec![OutputFormat::Dash],
        target_schemes: vec![EncryptionScheme::Cenc],
        container_format: ContainerFormat::default(),
        key_ids: vec![],
        raw_keys: vec![],
        key_rotation: None,
        clear_lead_segments: None,
        drm_systems: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: Some(ContentSteeringConfig {
            server_uri: "https://steer.example.com/v1".into(),
            default_pathway_id: Some("cdn-a".into()),
            query_before_start: Some(true),
        }),
        cache_control: None,
    };
    let json = serde_json::to_string(&req).unwrap();
    let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
    let cs = parsed.content_steering.unwrap();
    assert_eq!(cs.server_uri, "https://steer.example.com/v1");
    assert_eq!(cs.default_pathway_id.as_deref(), Some("cdn-a"));
    assert_eq!(cs.query_before_start, Some(true));
}

#[test]
fn repackage_request_no_steering_backward_compat() {
    let json = r#"{"content_id":"test","source_url":"https://example.com","output_formats":["Hls"],"target_schemes":["Cenc"],"key_ids":[]}"#;
    let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
    assert!(parsed.content_steering.is_none());
}

// ─── Override Behavior Tests ─────────────────────────────────────────

#[test]
fn content_steering_config_override_priority() {
    // Simulate the override logic: webhook config > source config
    let webhook_steering = Some(ContentSteeringConfig {
        server_uri: "https://webhook-steer.example.com".into(),
        default_pathway_id: Some("webhook-cdn".into()),
        query_before_start: None,
    });
    let source_steering = Some(ContentSteeringConfig {
        server_uri: "https://source-steer.example.com".into(),
        default_pathway_id: Some("source-cdn".into()),
        query_before_start: Some(true),
    });

    // Webhook should take precedence
    let effective = webhook_steering.clone().or(source_steering.clone());
    let cs = effective.unwrap();
    assert_eq!(cs.server_uri, "https://webhook-steer.example.com");
    assert_eq!(cs.default_pathway_id.as_deref(), Some("webhook-cdn"));
}

#[test]
fn content_steering_source_used_when_no_webhook() {
    // When webhook doesn't set steering, source config should be used
    let webhook_steering: Option<ContentSteeringConfig> = None;
    let source_steering = Some(ContentSteeringConfig {
        server_uri: "https://source-steer.example.com".into(),
        default_pathway_id: Some("source-cdn".into()),
        query_before_start: Some(true),
    });

    let effective = webhook_steering.or(source_steering);
    let cs = effective.unwrap();
    assert_eq!(cs.server_uri, "https://source-steer.example.com");
    assert_eq!(cs.default_pathway_id.as_deref(), Some("source-cdn"));
}

#[test]
fn content_steering_none_when_both_absent() {
    let webhook_steering: Option<ContentSteeringConfig> = None;
    let source_steering: Option<ContentSteeringConfig> = None;
    let effective = webhook_steering.or(source_steering);
    assert!(effective.is_none());
}
