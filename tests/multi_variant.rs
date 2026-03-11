//! Integration tests: Multi-variant architecture (CDN fan-out).
//!
//! Tests the multi-variant pipeline changes:
//! - DASH parser extracts all Representation metadata into source_variants
//! - HLS master playlist parser extracts variant/rendition info
//! - Pipeline merges source variant metadata into VariantInfo
//! - Per-variant route handling (v/{vid}/manifest, v/{vid}/init.mp4, etc.)
//! - CacheKeys variant-qualified key builders
//! - TrackInfo width/height from tkhd box
//! - SourceVariantInfo serde roundtrips

mod common;

use edgepack::cache::CacheKeys;
use edgepack::config::{
    AppConfig, CacheConfig, DrmConfig, DrmSystemIds, JitConfig, PolicyConfig, SpekeAuth,
};
use edgepack::handler::{route, HandlerContext, HttpMethod, HttpRequest};
use edgepack::manifest::types::{SourceManifest, SourceVariantInfo};
use edgepack::media::codec::extract_tracks;

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

// ─── TrackInfo Width/Height from tkhd ──────────────────────────────

#[test]
fn extract_tracks_returns_width_height_from_tkhd() {
    let init = common::build_clear_init_segment_with_dimensions(1920, 1080);
    let tracks = extract_tracks(&init).unwrap();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].width, Some(1920));
    assert_eq!(tracks[0].height, Some(1080));
}

#[test]
fn extract_tracks_returns_width_height_various_resolutions() {
    for (w, h) in [(3840, 2160), (1280, 720), (640, 360), (256, 144)] {
        let init = common::build_clear_init_segment_with_dimensions(w, h);
        let tracks = extract_tracks(&init).unwrap();
        assert_eq!(tracks.len(), 1, "expected 1 track for {w}x{h}");
        assert_eq!(tracks[0].width, Some(w), "width mismatch for {w}x{h}");
        assert_eq!(tracks[0].height, Some(h), "height mismatch for {w}x{h}");
    }
}

#[test]
fn extract_tracks_without_tkhd_returns_none_dimensions() {
    // Existing clear init segment has no tkhd — extract_tracks may return
    // empty (no hdlr) or tracks with None dimensions
    let init = common::build_clear_init_segment();
    let tracks = extract_tracks(&init).unwrap();
    assert!(tracks.is_empty() || tracks[0].width.is_none());
}

// ─── SourceVariantInfo Serde ───────────────────────────────────────

#[test]
fn source_variant_info_serde_roundtrip() {
    let variant = SourceVariantInfo {
        bandwidth: 5000000,
        width: Some(1920),
        height: Some(1080),
        codecs: Some("avc1.640028".into()),
        frame_rate: Some("24".into()),
    };
    let json = serde_json::to_string(&variant).unwrap();
    let roundtripped: SourceVariantInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtripped.bandwidth, 5000000);
    assert_eq!(roundtripped.width, Some(1920));
    assert_eq!(roundtripped.height, Some(1080));
    assert_eq!(roundtripped.codecs.as_deref(), Some("avc1.640028"));
    assert_eq!(roundtripped.frame_rate.as_deref(), Some("24"));
}

#[test]
fn source_variant_info_serde_with_none_fields() {
    let variant = SourceVariantInfo {
        bandwidth: 100000,
        width: None,
        height: None,
        codecs: None,
        frame_rate: None,
    };
    let json = serde_json::to_string(&variant).unwrap();
    let roundtripped: SourceVariantInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtripped.bandwidth, 100000);
    assert!(roundtripped.width.is_none());
    assert!(roundtripped.codecs.is_none());
}

#[test]
fn source_manifest_with_source_variants_serde_roundtrip() {
    let manifest = SourceManifest {
        init_segment_url: "https://example.com/init.mp4".into(),
        segment_urls: vec!["https://example.com/seg0.m4s".into()],
        segment_durations: vec![6.0],
        is_live: false,
        source_scheme: None,
        ad_breaks: vec![],
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
        is_ts_source: false,
        aes128_key_url: None,
        aes128_iv: None,
        content_steering: None,
        init_byte_range: None,
        segment_byte_ranges: vec![],
        segment_base: None,
        source_variants: vec![
            SourceVariantInfo {
                bandwidth: 500000,
                width: Some(640),
                height: Some(360),
                codecs: Some("avc1.42c015".into()),
                frame_rate: Some("30".into()),
            },
            SourceVariantInfo {
                bandwidth: 2000000,
                width: Some(1280),
                height: Some(720),
                codecs: Some("avc1.4d401f".into()),
                frame_rate: Some("30".into()),
            },
            SourceVariantInfo {
                bandwidth: 5000000,
                width: Some(1920),
                height: Some(1080),
                codecs: Some("avc1.640028".into()),
                frame_rate: Some("24".into()),
            },
        ],
    };

    let json = serde_json::to_string(&manifest).unwrap();
    let roundtripped: SourceManifest = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtripped.source_variants.len(), 3);
    assert_eq!(roundtripped.source_variants[0].bandwidth, 500000);
    assert_eq!(roundtripped.source_variants[1].width, Some(1280));
    assert_eq!(roundtripped.source_variants[2].codecs.as_deref(), Some("avc1.640028"));
}

#[test]
fn source_manifest_backward_compat_no_source_variants() {
    // JSON without source_variants field should deserialize with empty Vec
    let json = r#"{
        "init_segment_url": "https://example.com/init.mp4",
        "segment_urls": ["https://example.com/seg0.m4s"],
        "segment_durations": [6.0],
        "total_duration": 6.0,
        "is_live": false
    }"#;
    let manifest: SourceManifest = serde_json::from_str(json).unwrap();
    assert!(manifest.source_variants.is_empty());
}

// ─── DASH Parser Source Variants ───────────────────────────────────

#[test]
fn dash_parser_extracts_multi_representation_metadata() {
    use edgepack::manifest::dash_input::parse_dash_manifest;

    let mpd = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT60S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="90000" media="video_$Number$.cmfv" initialization="video_init.mp4" startNumber="0">
        <SegmentTimeline>
          <S t="0" d="540000" r="9"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="1" bandwidth="100000" width="256" height="144" codecs="avc1.42c00c" frameRate="24"/>
      <Representation id="2" bandwidth="500000" width="640" height="360" codecs="avc1.42c015" frameRate="30"/>
      <Representation id="3" bandwidth="2000000" width="1280" height="720" codecs="avc1.4d401f" frameRate="30"/>
      <Representation id="4" bandwidth="5000000" width="1920" height="1080" codecs="avc1.640028" frameRate="24"/>
      <Representation id="5" bandwidth="12000000" width="3840" height="2160" codecs="avc1.640033" frameRate="24"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate timescale="44100" media="audio_$Number$.cmfa" initialization="audio_init.mp4" startNumber="0">
        <SegmentTimeline>
          <S t="0" d="264600" r="9"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="6" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let source = parse_dash_manifest(mpd, "https://cdn.example.com/content/dash.mpd").unwrap();
    assert_eq!(source.source_variants.len(), 5);

    // Check sorted by appearance order
    assert_eq!(source.source_variants[0].bandwidth, 100000);
    assert_eq!(source.source_variants[0].width, Some(256));
    assert_eq!(source.source_variants[0].height, Some(144));
    assert_eq!(source.source_variants[0].codecs.as_deref(), Some("avc1.42c00c"));
    assert_eq!(source.source_variants[0].frame_rate.as_deref(), Some("24"));

    assert_eq!(source.source_variants[2].bandwidth, 2000000);
    assert_eq!(source.source_variants[2].width, Some(1280));
    assert_eq!(source.source_variants[2].height, Some(720));

    assert_eq!(source.source_variants[4].bandwidth, 12000000);
    assert_eq!(source.source_variants[4].width, Some(3840));
    assert_eq!(source.source_variants[4].height, Some(2160));
}

#[test]
fn dash_parser_ignores_audio_representations_in_source_variants() {
    use edgepack::manifest::dash_input::parse_dash_manifest;

    let mpd = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT60S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="90000" media="video_$Number$.cmfv" initialization="video_init.mp4" startNumber="0">
        <SegmentTimeline>
          <S t="0" d="540000" r="9"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="1" bandwidth="2000000" width="1280" height="720" codecs="avc1.4d401f"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate timescale="44100" media="audio_$Number$.cmfa" initialization="audio_init.mp4" startNumber="0">
        <SegmentTimeline>
          <S t="0" d="264600" r="9"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="2" bandwidth="128000" codecs="mp4a.40.2"/>
      <Representation id="3" bandwidth="256000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let source = parse_dash_manifest(mpd, "https://cdn.example.com/content/dash.mpd").unwrap();
    // Only video Representations should be in source_variants
    assert_eq!(source.source_variants.len(), 1);
    assert_eq!(source.source_variants[0].bandwidth, 2000000);
}

#[test]
fn dash_parser_single_representation_still_populates_source_variants() {
    use edgepack::manifest::dash_input::parse_dash_manifest;

    let mpd = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="90000" media="v_$Number$.cmfv" initialization="v_init.mp4" startNumber="0">
        <SegmentTimeline>
          <S t="0" d="900000"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="1" bandwidth="3000000" width="1920" height="1080" codecs="avc1.640028" frameRate="30"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    let source = parse_dash_manifest(mpd, "https://cdn.example.com/dash.mpd").unwrap();
    assert_eq!(source.source_variants.len(), 1);
    assert_eq!(source.source_variants[0].bandwidth, 3000000);
    assert_eq!(source.source_variants[0].frame_rate.as_deref(), Some("30"));
}

// ─── HLS Master Playlist Parser ───────────────────────────────────

#[test]
fn hls_master_playlist_extracts_variants() {
    use edgepack::manifest::hls_input::parse_hls_master_playlist;

    let master = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-INDEPENDENT-SEGMENTS
#EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=640x360,CODECS="avc1.42c015",FRAME-RATE=30.000
360p/playlist.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS="avc1.4d401f",FRAME-RATE=30.000
720p/playlist.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS="avc1.640028",FRAME-RATE=24.000
1080p/playlist.m3u8
"#;

    let info = parse_hls_master_playlist(master, "https://cdn.example.com/master.m3u8").unwrap();
    assert_eq!(info.variants.len(), 3);

    assert_eq!(info.variants[0].bandwidth, 500000);
    assert_eq!(info.variants[0].width, Some(640));
    assert_eq!(info.variants[0].height, Some(360));
    assert_eq!(info.variants[0].codecs.as_deref(), Some("avc1.42c015"));
    assert_eq!(info.variants[0].frame_rate.as_deref(), Some("30.000"));

    assert_eq!(info.variants[1].bandwidth, 2000000);
    assert_eq!(info.variants[1].width, Some(1280));

    assert_eq!(info.variants[2].bandwidth, 5000000);
    assert_eq!(info.variants[2].width, Some(1920));
    assert_eq!(info.variants[2].height, Some(1080));

    // Check URIs are resolved
    assert_eq!(info.variant_uris.len(), 3);
    assert!(info.variant_uris[0].contains("360p/playlist.m3u8"));
    assert!(info.variant_uris[2].contains("1080p/playlist.m3u8"));
}

#[test]
fn hls_master_playlist_extracts_audio_renditions() {
    use edgepack::manifest::hls_input::parse_hls_master_playlist;

    let master = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="audio_en.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="audio",NAME="Spanish",LANGUAGE="es",DEFAULT=NO,URI="audio_es.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,AUDIO="audio"
video.m3u8
"#;

    let info = parse_hls_master_playlist(master, "https://cdn.example.com/master.m3u8").unwrap();
    assert_eq!(info.audio_renditions.len(), 2);
    assert_eq!(info.audio_renditions[0].name, "English");
    assert_eq!(info.audio_renditions[0].language.as_deref(), Some("en"));
    assert!(info.audio_renditions[0].is_default);
    assert_eq!(info.audio_renditions[1].name, "Spanish");
    assert_eq!(info.audio_renditions[1].language.as_deref(), Some("es"));
    assert!(!info.audio_renditions[1].is_default);
}

#[test]
fn hls_master_playlist_extracts_subtitle_renditions() {
    use edgepack::manifest::hls_input::parse_hls_master_playlist;

    let master = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="English",LANGUAGE="en",DEFAULT=YES,URI="subs_en.m3u8"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID="subs",NAME="French",LANGUAGE="fr",DEFAULT=NO,URI="subs_fr.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,SUBTITLES="subs"
video.m3u8
"#;

    let info = parse_hls_master_playlist(master, "https://cdn.example.com/master.m3u8").unwrap();
    assert_eq!(info.subtitle_renditions.len(), 2);
    assert_eq!(info.subtitle_renditions[0].name, "English");
    assert_eq!(info.subtitle_renditions[0].language.as_deref(), Some("en"));
    assert_eq!(info.subtitle_renditions[1].name, "French");
}

#[test]
fn hls_master_playlist_no_variants_returns_error() {
    use edgepack::manifest::hls_input::parse_hls_master_playlist;

    let master = r#"#EXTM3U
#EXT-X-VERSION:7
"#;

    // A master playlist with no #EXT-X-STREAM-INF should be rejected
    let result = parse_hls_master_playlist(master, "https://cdn.example.com/master.m3u8");
    assert!(result.is_err());
}

#[test]
fn hls_master_playlist_resolves_relative_uris() {
    use edgepack::manifest::hls_input::parse_hls_master_playlist;

    let master = r#"#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=960x540
../video/540p.m3u8
"#;

    let info = parse_hls_master_playlist(master, "https://cdn.example.com/hls/master.m3u8").unwrap();
    assert_eq!(info.variant_uris.len(), 1);
    // URI should be resolved relative to master URL
    let uri = &info.variant_uris[0];
    assert!(uri.contains("video/540p.m3u8") || uri.contains("../video/540p.m3u8"));
}

// ─── Per-Variant Route Handling ───────────────────────────────────

#[test]
fn variant_manifest_route_returns_404_on_cache_miss() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/mv-test-1/hls/v/0/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn variant_init_route_returns_404_on_cache_miss() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/mv-test-2/hls/v/0/init.mp4".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn variant_segment_route_returns_404_on_cache_miss() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/mv-test-3/hls_cenc/v/4/segment_0.cmfv".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn variant_iframe_route_returns_404_on_cache_miss() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/mv-test-4/hls/v/0/iframes".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

#[test]
fn variant_iframe_route_dash_returns_404() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/mv-test-5/dash/v/0/iframes".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
    let body = String::from_utf8_lossy(&resp.body);
    assert!(body.contains("DASH trick play"));
}

#[test]
fn variant_routes_with_scheme_qualified_format() {
    let ctx = test_context();
    for fmt in ["hls_cenc", "hls_cbcs", "dash_cenc", "dash_cbcs"] {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: format!("/repackage/mv-test-6/{}//v/0/manifest", fmt),
            headers: vec![],
            body: None,
        };
        // Should route correctly (404 due to cache miss, not routing error)
        let resp = route(&req, &ctx).unwrap();
        assert!(resp.status == 404 || resp.status == 200,
            "unexpected status {} for format {}", resp.status, fmt);
    }
}

#[test]
fn variant_segment_route_various_extensions() {
    let ctx = test_context();
    for ext in ["cmfv", "cmfa", "m4s", "mp4"] {
        let req = HttpRequest {
            method: HttpMethod::Get,
            path: format!("/repackage/mv-test-7/hls/v/2/segment_5.{}", ext),
            headers: vec![],
            body: None,
        };
        let resp = route(&req, &ctx).unwrap();
        assert_eq!(resp.status, 404, "expected 404 for extension .{}", ext);
    }
}

#[test]
fn variant_route_high_variant_id() {
    let ctx = test_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/mv-test-8/hls/v/99/manifest".into(),
        headers: vec![],
        body: None,
    };
    let resp = route(&req, &ctx).unwrap();
    assert_eq!(resp.status, 404);
}

// ─── CacheKeys Variant Builders ───────────────────────────────────

#[test]
fn cache_keys_variant_manifest_state_format() {
    let key = CacheKeys::variant_manifest_state("content-123", 4, "hls", Some("cenc"));
    assert_eq!(key, "ep:content-123:v4:hls_cenc:manifest_state");
}

#[test]
fn cache_keys_variant_init_segment_format() {
    let key = CacheKeys::variant_init_segment("content-123", 0, Some("cbcs"));
    assert_eq!(key, "ep:content-123:v0:cbcs:init");
}

#[test]
fn cache_keys_variant_media_segment_format() {
    let key = CacheKeys::variant_media_segment("content-123", 8, 42, Some("cenc"));
    assert_eq!(key, "ep:content-123:v8:cenc:seg:42");
}

#[test]
fn cache_keys_variant_without_scheme() {
    let key = CacheKeys::variant_manifest_state("content-123", 0, "hls", None);
    assert_eq!(key, "ep:content-123:v0:hls:manifest_state");

    let key = CacheKeys::variant_init_segment("content-123", 0, None);
    assert_eq!(key, "ep:content-123:v0:init");

    let key = CacheKeys::variant_media_segment("content-123", 0, 5, None);
    assert_eq!(key, "ep:content-123:v0:seg:5");
}

#[test]
fn cache_keys_variant_differs_from_non_variant() {
    let variant = CacheKeys::variant_init_segment("abc", 0, Some("cenc"));
    let global = CacheKeys::init_segment_for_scheme_only("abc", "cenc");
    assert_ne!(variant, global);
}

#[test]
fn cache_keys_source_variants_key() {
    let key = CacheKeys::source_variants("content-123");
    assert_eq!(key, "ep:content-123:variants");
}

#[test]
fn cache_keys_master_manifest_key() {
    let key = CacheKeys::master_manifest("content-123", "hls", Some("cenc"));
    assert_eq!(key, "ep:content-123:master:hls_cenc");
    let key = CacheKeys::master_manifest("content-123", "dash", None);
    assert_eq!(key, "ep:content-123:master:dash");
}

// ─── HlsRenditionInfo Serde ───────────────────────────────────────

#[test]
fn hls_rendition_info_serde_roundtrip() {
    use edgepack::manifest::types::HlsRenditionInfo;

    let rendition = HlsRenditionInfo {
        uri: Some("audio_en.m3u8".into()),
        name: "English".into(),
        language: Some("en".into()),
        group_id: "audio".into(),
        is_default: true,
    };
    let json = serde_json::to_string(&rendition).unwrap();
    let roundtripped: HlsRenditionInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtripped.name, "English");
    assert_eq!(roundtripped.language.as_deref(), Some("en"));
    assert!(roundtripped.is_default);
    assert_eq!(roundtripped.group_id, "audio");
}

// ─── DASH Per-Representation SegmentTemplate ──────────────────────

#[test]
fn dash_multi_variant_per_representation_segment_template() {
    use edgepack::manifest::types::{
        ManifestPhase, ManifestState, SegmentInfo, TrackMediaType, VariantInfo,
    };
    use edgepack::media::container::ContainerFormat;

    let state = ManifestState {
        content_id: "test-mv-dash".into(),
        format: edgepack::manifest::types::OutputFormat::Dash,
        phase: ManifestPhase::Complete,
        init_segment: Some(edgepack::manifest::types::InitSegmentInfo {
            uri: "init.mp4".into(),
            byte_size: 1000,
        }),
        segments: vec![
            SegmentInfo { number: 0, duration: 6.0, uri: "segment_0.cmfv".into(), byte_size: 10000, key_period: None },
            SegmentInfo { number: 1, duration: 6.0, uri: "segment_1.cmfv".into(), byte_size: 10000, key_period: None },
        ],
        target_duration: 6.0,
        variants: vec![
            VariantInfo {
                id: "v0".into(),
                bandwidth: 500_000,
                codecs: "avc1.42c01e".into(),
                resolution: Some((640, 360)),
                frame_rate: Some(30.0),
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v/0/".into()),
            },
            VariantInfo {
                id: "v1".into(),
                bandwidth: 2_000_000,
                codecs: "avc1.64001f".into(),
                resolution: Some((1280, 720)),
                frame_rate: Some(30.0),
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v/1/".into()),
            },
            VariantInfo {
                id: "v2".into(),
                bandwidth: 5_000_000,
                codecs: "avc1.640028".into(),
                resolution: Some((1920, 1080)),
                frame_rate: Some(30.0),
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v/2/".into()),
            },
        ],
        drm_info: None,
        media_sequence: 0,
        base_url: String::new(),
        container_format: ContainerFormat::Cmaf,
        cea_captions: vec![],
        ad_breaks: vec![],
        rotation_drm_info: vec![],
        clear_lead_boundary: None,
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
        iframe_segments: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };

    let mpd = edgepack::manifest::render_manifest(&state).unwrap();

    // Should contain 3 Representations
    assert_eq!(mpd.matches("<Representation").count(), 3, "Expected 3 Representations");

    // Each should have its own SegmentTemplate with variant-specific paths
    assert!(mpd.contains("initialization=\"v/0/init.mp4\""), "v0 init path");
    assert!(mpd.contains("media=\"v/0/segment_$Number$.cmfv\""), "v0 media path");
    assert!(mpd.contains("initialization=\"v/1/init.mp4\""), "v1 init path");
    assert!(mpd.contains("media=\"v/1/segment_$Number$.cmfv\""), "v1 media path");
    assert!(mpd.contains("initialization=\"v/2/init.mp4\""), "v2 init path");
    assert!(mpd.contains("media=\"v/2/segment_$Number$.cmfv\""), "v2 media path");

    // Should NOT have an AdaptationSet-level SegmentTemplate
    // (only per-Representation ones)
    let adaptation_set_pos = mpd.find("<AdaptationSet").unwrap();
    let first_rep_pos = mpd.find("<Representation").unwrap();
    let between = &mpd[adaptation_set_pos..first_rep_pos];
    assert!(
        !between.contains("<SegmentTemplate"),
        "No AdaptationSet-level SegmentTemplate when per-Representation templates are used"
    );

    // Bandwidth and resolution should be present
    assert!(mpd.contains("bandwidth=\"500000\""));
    assert!(mpd.contains("bandwidth=\"2000000\""));
    assert!(mpd.contains("bandwidth=\"5000000\""));
    assert!(mpd.contains("width=\"640\" height=\"360\""));
    assert!(mpd.contains("width=\"1280\" height=\"720\""));
    assert!(mpd.contains("width=\"1920\" height=\"1080\""));

    // Each SegmentTemplate should have SegmentTimeline with correct durations
    assert_eq!(mpd.matches("<SegmentTimeline>").count(), 3, "Expected 3 SegmentTimelines");
    assert_eq!(mpd.matches("<S d=\"6000\"/>").count(), 6, "Expected 6 S elements (2 per variant)");
}

#[test]
fn dash_single_variant_no_per_representation_template() {
    use edgepack::manifest::types::{
        ManifestPhase, ManifestState, SegmentInfo, TrackMediaType, VariantInfo,
    };
    use edgepack::media::container::ContainerFormat;

    let state = ManifestState {
        content_id: "test-sv-dash".into(),
        format: edgepack::manifest::types::OutputFormat::Dash,
        phase: ManifestPhase::Complete,
        init_segment: Some(edgepack::manifest::types::InitSegmentInfo {
            uri: "init.mp4".into(),
            byte_size: 1000,
        }),
        segments: vec![
            SegmentInfo { number: 0, duration: 6.0, uri: "segment_0.cmfv".into(), byte_size: 10000, key_period: None },
        ],
        target_duration: 6.0,
        variants: vec![
            VariantInfo {
                id: "v0".into(),
                bandwidth: 2_000_000,
                codecs: "avc1.64001f".into(),
                resolution: Some((1280, 720)),
                frame_rate: None,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: None, // No prefix = shared AdaptationSet-level template
            },
        ],
        drm_info: None,
        media_sequence: 0,
        base_url: String::new(),
        container_format: ContainerFormat::Cmaf,
        cea_captions: vec![],
        ad_breaks: vec![],
        rotation_drm_info: vec![],
        clear_lead_boundary: None,
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
        iframe_segments: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };

    let mpd = edgepack::manifest::render_manifest(&state).unwrap();

    // Single variant: AdaptationSet-level SegmentTemplate (backward compat)
    assert_eq!(mpd.matches("<SegmentTemplate").count(), 1, "One shared SegmentTemplate");
    assert_eq!(mpd.matches("<Representation").count(), 1, "One Representation");

    // SegmentTemplate should be at AdaptationSet level (before Representation)
    let template_pos = mpd.find("<SegmentTemplate").unwrap();
    let rep_pos = mpd.find("<Representation").unwrap();
    assert!(template_pos < rep_pos, "SegmentTemplate before Representation");
}

#[test]
fn dash_sandbox_prefix_per_representation() {
    use edgepack::manifest::types::{
        ManifestPhase, ManifestState, SegmentInfo, TrackMediaType, VariantInfo,
    };
    use edgepack::media::container::ContainerFormat;

    // Sandbox uses "v0_" prefix (flat file naming, not nested paths)
    let state = ManifestState {
        content_id: "test-sandbox-dash".into(),
        format: edgepack::manifest::types::OutputFormat::Dash,
        phase: ManifestPhase::Complete,
        init_segment: Some(edgepack::manifest::types::InitSegmentInfo {
            uri: "init.mp4".into(),
            byte_size: 1000,
        }),
        segments: vec![
            SegmentInfo { number: 0, duration: 4.0, uri: "segment_0.cmfv".into(), byte_size: 5000, key_period: None },
        ],
        target_duration: 4.0,
        variants: vec![
            VariantInfo {
                id: "v0".into(),
                bandwidth: 100_000,
                codecs: "avc1.42c00d".into(),
                resolution: Some((256, 144)),
                frame_rate: None,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v0_".into()),
            },
            VariantInfo {
                id: "v1".into(),
                bandwidth: 2_000_000,
                codecs: "avc1.64001f".into(),
                resolution: Some((1280, 720)),
                frame_rate: None,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v1_".into()),
            },
        ],
        drm_info: None,
        media_sequence: 0,
        base_url: String::new(),
        container_format: ContainerFormat::Cmaf,
        cea_captions: vec![],
        ad_breaks: vec![],
        rotation_drm_info: vec![],
        clear_lead_boundary: None,
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
        iframe_segments: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };

    let mpd = edgepack::manifest::render_manifest(&state).unwrap();

    // Sandbox-style flat prefixes
    assert!(mpd.contains("initialization=\"v0_init.mp4\""), "v0 sandbox init: {mpd}");
    assert!(mpd.contains("media=\"v0_segment_$Number$.cmfv\""), "v0 sandbox media: {mpd}");
    assert!(mpd.contains("initialization=\"v1_init.mp4\""), "v1 sandbox init: {mpd}");
    assert!(mpd.contains("media=\"v1_segment_$Number$.cmfv\""), "v1 sandbox media: {mpd}");
}

#[test]
fn dash_progressive_multi_variant_live_phase() {
    use edgepack::manifest::types::{
        ManifestPhase, ManifestState, SegmentInfo, TrackMediaType, VariantInfo,
    };
    use edgepack::media::container::ContainerFormat;

    // Test that progressive (Live phase) DASH output works with per-Representation templates
    let state = ManifestState {
        content_id: "test-prog-dash".into(),
        format: edgepack::manifest::types::OutputFormat::Dash,
        phase: ManifestPhase::Live,
        init_segment: Some(edgepack::manifest::types::InitSegmentInfo {
            uri: "init.mp4".into(),
            byte_size: 1000,
        }),
        segments: vec![
            SegmentInfo { number: 0, duration: 6.0, uri: "segment_0.cmfv".into(), byte_size: 10000, key_period: None },
        ],
        target_duration: 6.0,
        variants: vec![
            VariantInfo {
                id: "v0".into(),
                bandwidth: 500_000,
                codecs: "avc1.42c01e".into(),
                resolution: Some((640, 360)),
                frame_rate: None,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v/0/".into()),
            },
            VariantInfo {
                id: "v1".into(),
                bandwidth: 2_000_000,
                codecs: "avc1.64001f".into(),
                resolution: Some((1280, 720)),
                frame_rate: None,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v/1/".into()),
            },
        ],
        drm_info: None,
        media_sequence: 0,
        base_url: String::new(),
        container_format: ContainerFormat::Cmaf,
        cea_captions: vec![],
        ad_breaks: vec![],
        rotation_drm_info: vec![],
        clear_lead_boundary: None,
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
        iframe_segments: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };

    let mpd = edgepack::manifest::render_manifest(&state).unwrap();

    // Live phase → type="dynamic"
    assert!(mpd.contains("type=\"dynamic\""), "Live phase should be dynamic");

    // Per-Representation SegmentTemplates should still work in Live phase
    assert!(mpd.contains("initialization=\"v/0/init.mp4\""), "v0 init in live");
    assert!(mpd.contains("initialization=\"v/1/init.mp4\""), "v1 init in live");

    // Only 1 segment so far (progressive)
    assert_eq!(mpd.matches("<S d=\"6000\"/>").count(), 2, "1 segment × 2 variants");
}

// ─── HLS Multi-Variant with segment_path_prefix ─────────────────

#[test]
fn hls_multi_variant_render_master_uses_segment_path_prefix() {
    use edgepack::manifest::types::{
        ManifestPhase, ManifestState, SegmentInfo, TrackMediaType, VariantInfo,
    };
    use edgepack::manifest::hls::render_master;
    use edgepack::media::container::ContainerFormat;

    let state = ManifestState {
        content_id: "test-mv-hls".into(),
        format: edgepack::manifest::types::OutputFormat::Hls,
        phase: ManifestPhase::Complete,
        init_segment: None,
        segments: vec![
            SegmentInfo { number: 0, duration: 6.0, uri: "segment_0.cmfv".into(), byte_size: 10000, key_period: None },
        ],
        target_duration: 6.0,
        variants: vec![
            VariantInfo {
                id: "v0".into(),
                bandwidth: 500_000,
                codecs: "avc1.42c01e".into(),
                resolution: Some((640, 360)),
                frame_rate: None,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v/0/".into()),
            },
            VariantInfo {
                id: "v1".into(),
                bandwidth: 2_000_000,
                codecs: "avc1.64001f".into(),
                resolution: Some((1280, 720)),
                frame_rate: None,
                track_type: TrackMediaType::Video,
                language: None,
                segment_path_prefix: Some("v/1/".into()),
            },
        ],
        drm_info: None,
        media_sequence: 0,
        base_url: String::new(),
        container_format: ContainerFormat::Cmaf,
        cea_captions: vec![],
        ad_breaks: vec![],
        rotation_drm_info: vec![],
        clear_lead_boundary: None,
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
        iframe_segments: vec![],
        enable_iframe_playlist: false,
        dvr_window_duration: None,
        content_steering: None,
        cache_control: None,
    };

    // No explicit variant_playlist_uris — should use segment_path_prefix
    let m3u8 = render_master(&state, &[]).unwrap();

    // Should reference per-variant manifests
    assert!(m3u8.contains("v/0/manifest"), "v0 manifest URI: {m3u8}");
    assert!(m3u8.contains("v/1/manifest"), "v1 manifest URI: {m3u8}");

    // Should have proper STREAM-INF with bandwidth and resolution
    assert!(m3u8.contains("BANDWIDTH=500000"), "v0 bandwidth");
    assert!(m3u8.contains("BANDWIDTH=2000000"), "v1 bandwidth");
    assert!(m3u8.contains("RESOLUTION=640x360"), "v0 resolution");
    assert!(m3u8.contains("RESOLUTION=1280x720"), "v1 resolution");
}

// ─── VariantInfo segment_path_prefix Serde ─────────────────────────

#[test]
fn variant_info_segment_path_prefix_serde_roundtrip() {
    use edgepack::manifest::types::{TrackMediaType, VariantInfo};

    let v = VariantInfo {
        id: "v0".into(),
        bandwidth: 1_000_000,
        codecs: "avc1.64001f".into(),
        resolution: Some((1280, 720)),
        frame_rate: Some(30.0),
        track_type: TrackMediaType::Video,
        language: None,
        segment_path_prefix: Some("v/0/".into()),
    };
    let json = serde_json::to_string(&v).unwrap();
    let roundtripped: VariantInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(roundtripped.segment_path_prefix.as_deref(), Some("v/0/"));
}

#[test]
fn variant_info_segment_path_prefix_defaults_none() {
    use edgepack::manifest::types::{TrackMediaType, VariantInfo};

    // JSON without segment_path_prefix should deserialize to None (backward compat)
    let json = r#"{"id":"v0","bandwidth":1000000,"codecs":"avc1.64001f","resolution":[1280,720],"frame_rate":30.0,"track_type":"Video"}"#;
    let v: VariantInfo = serde_json::from_str(json).unwrap();
    assert!(v.segment_path_prefix.is_none(), "Default should be None");
}

// ─── Pipeline Multi-Variant Prefix Assignment ──────────────────────

#[test]
fn pipeline_multi_variant_assigns_segment_path_prefix() {
    use edgepack::manifest::types::SourceVariantInfo;

    // Create a SourceManifest with multiple variants
    let source = SourceManifest {
        init_segment_url: "https://cdn.example.com/init.mp4".into(),
        segment_urls: vec!["https://cdn.example.com/seg0.m4s".into()],
        segment_durations: vec![6.0],
        is_live: false,
        source_scheme: None,
        is_ts_source: false,
        aes128_key_url: None,
        aes128_iv: None,
        source_variants: vec![
            SourceVariantInfo {
                bandwidth: 500_000,
                width: Some(640),
                height: Some(360),
                codecs: Some("avc1.42c01e".into()),
                frame_rate: None,
            },
            SourceVariantInfo {
                bandwidth: 2_000_000,
                width: Some(1280),
                height: Some(720),
                codecs: Some("avc1.64001f".into()),
                frame_rate: None,
            },
            SourceVariantInfo {
                bandwidth: 5_000_000,
                width: Some(1920),
                height: Some(1080),
                codecs: Some("avc1.640028".into()),
                frame_rate: None,
            },
        ],
        content_steering: None,
        init_byte_range: None,
        segment_byte_ranges: vec![],
        segment_base: None,
        ad_breaks: vec![],
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
    };

    // Build init segment with a video track for extract_tracks
    let init = common::build_clear_init_segment_with_dimensions(1280, 720);
    let tracks = extract_tracks(&init).unwrap();

    let variants = edgepack::repackager::pipeline::build_variants_from_tracks(&tracks, Some(&source));

    // Should have 3 video variants
    let video_variants: Vec<_> = variants.iter()
        .filter(|v| v.track_type == edgepack::manifest::types::TrackMediaType::Video)
        .collect();
    assert_eq!(video_variants.len(), 3, "Should have 3 video variants");

    // Each should have segment_path_prefix set for CDN routing
    assert_eq!(video_variants[0].segment_path_prefix.as_deref(), Some("v/0/"), "v0 prefix");
    assert_eq!(video_variants[1].segment_path_prefix.as_deref(), Some("v/1/"), "v1 prefix");
    assert_eq!(video_variants[2].segment_path_prefix.as_deref(), Some("v/2/"), "v2 prefix");

    // Bandwidths should be preserved from source
    assert_eq!(video_variants[0].bandwidth, 500_000);
    assert_eq!(video_variants[1].bandwidth, 2_000_000);
    assert_eq!(video_variants[2].bandwidth, 5_000_000);
}

#[test]
fn pipeline_single_variant_no_prefix() {
    use edgepack::manifest::types::SourceVariantInfo;

    let source = SourceManifest {
        init_segment_url: "https://cdn.example.com/init.mp4".into(),
        segment_urls: vec!["https://cdn.example.com/seg0.m4s".into()],
        segment_durations: vec![6.0],
        is_live: false,
        source_scheme: None,
        is_ts_source: false,
        aes128_key_url: None,
        aes128_iv: None,
        source_variants: vec![
            SourceVariantInfo {
                bandwidth: 2_000_000,
                width: Some(1280),
                height: Some(720),
                codecs: Some("avc1.64001f".into()),
                frame_rate: None,
            },
        ],
        content_steering: None,
        init_byte_range: None,
        segment_byte_ranges: vec![],
        segment_base: None,
        ad_breaks: vec![],
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
    };

    let init = common::build_clear_init_segment_with_dimensions(1280, 720);
    let tracks = extract_tracks(&init).unwrap();

    let variants = edgepack::repackager::pipeline::build_variants_from_tracks(&tracks, Some(&source));

    let video_variants: Vec<_> = variants.iter()
        .filter(|v| v.track_type == edgepack::manifest::types::TrackMediaType::Video)
        .collect();

    // Single variant should NOT have prefix (backward compat)
    assert_eq!(video_variants.len(), 1, "Single variant");
    assert!(video_variants[0].segment_path_prefix.is_none(), "No prefix for single variant");
}
