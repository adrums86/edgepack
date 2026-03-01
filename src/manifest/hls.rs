use crate::error::Result;
use crate::manifest::types::{ManifestPhase, ManifestState};

/// Render an HLS M3U8 manifest from the current state.
///
/// - During `Live` phase: produces a live playlist (no `#EXT-X-ENDLIST`)
/// - During `Complete` phase: produces a VOD playlist (with `#EXT-X-ENDLIST`)
pub fn render(state: &ManifestState) -> Result<String> {
    let mut m3u8 = String::new();

    // Header
    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:7\n");

    // Target duration (rounded up to nearest integer)
    let target_dur = state.target_duration.ceil() as u64;
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));

    // Media sequence
    m3u8.push_str(&format!(
        "#EXT-X-MEDIA-SEQUENCE:{}\n",
        state.media_sequence
    ));

    // Playlist type
    match state.phase {
        ManifestPhase::Complete => {
            m3u8.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        }
        ManifestPhase::Live => {
            m3u8.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        ManifestPhase::AwaitingFirstSegment => {
            // Shouldn't render in this state, but handle gracefully
            return Ok(m3u8);
        }
    }

    // Independent segments (CMAF guarantees this)
    m3u8.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    // DRM signaling — dynamic method based on encryption scheme
    if let Some(ref drm) = state.drm_info {
        let method = drm.encryption_scheme.hls_method_string();

        // FairPlay (CBCS only)
        if let Some(ref key_uri) = drm.fairplay_key_uri {
            m3u8.push_str(&format!(
                "#EXT-X-KEY:METHOD={method},\
                 URI=\"{key_uri}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"com.apple.streamingkeydelivery\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }

        // Widevine
        if let Some(ref pssh) = drm.widevine_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-KEY:METHOD={method},\
                 URI=\"data:text/plain;base64,{pssh}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }

        // PlayReady
        if let Some(ref pssh) = drm.playready_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-KEY:METHOD={method},\
                 URI=\"data:text/plain;base64,{pssh}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }
    }

    // Init segment (EXT-X-MAP)
    if let Some(ref init) = state.init_segment {
        m3u8.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init.uri));
    }

    // Segments
    for segment in &state.segments {
        m3u8.push_str(&format!("#EXTINF:{:.6},\n", segment.duration));
        m3u8.push_str(&format!("{}\n", segment.uri));
    }

    // End list for completed manifests
    if state.phase == ManifestPhase::Complete {
        m3u8.push_str("#EXT-X-ENDLIST\n");
    }

    Ok(m3u8)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::scheme::EncryptionScheme;
    use crate::manifest::types::*;
    use crate::media::container::ContainerFormat;

    fn make_state(phase: ManifestPhase) -> ManifestState {
        let mut s = ManifestState::new("test".into(), OutputFormat::Hls, "/base/".into(), ContainerFormat::default());
        s.phase = phase;
        s
    }

    fn make_live_state_with_segments(count: u32) -> ManifestState {
        let mut s = make_state(ManifestPhase::Live);
        s.init_segment = Some(InitSegmentInfo {
            uri: "/base/init.mp4".into(),
            byte_size: 256,
        });
        for i in 0..count {
            s.segments.push(SegmentInfo {
                number: i,
                duration: 6.006,
                uri: format!("/base/segment_{i}.cmfv"),
                byte_size: 1024,
            });
        }
        s
    }

    #[test]
    fn render_awaiting_returns_minimal() {
        let state = make_state(ManifestPhase::AwaitingFirstSegment);
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXTM3U"));
        assert!(m3u8.contains("#EXT-X-VERSION:7"));
        assert!(!m3u8.contains("#EXT-X-PLAYLIST-TYPE"));
        assert!(!m3u8.contains("#EXTINF"));
    }

    #[test]
    fn render_live_no_endlist() {
        let state = make_live_state_with_segments(2);
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        assert!(m3u8.contains("#EXT-X-INDEPENDENT-SEGMENTS"));
        assert!(m3u8.contains("#EXT-X-MAP:URI=\"/base/init.mp4\""));
        assert!(m3u8.contains("#EXTINF:6.006000,"));
        assert!(m3u8.contains("/base/segment_0.cmfv"));
        assert!(m3u8.contains("/base/segment_1.cmfv"));
        assert!(!m3u8.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn render_complete_has_endlist() {
        let mut state = make_live_state_with_segments(3);
        state.phase = ManifestPhase::Complete;
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(m3u8.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn render_target_duration_rounded_up() {
        let mut state = make_live_state_with_segments(1);
        state.target_duration = 6.006;
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-TARGETDURATION:7"));
    }

    #[test]
    fn render_media_sequence() {
        let mut state = make_live_state_with_segments(1);
        state.media_sequence = 42;
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-MEDIA-SEQUENCE:42"));
    }

    #[test]
    fn render_with_drm_widevine() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("AAAA".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("METHOD=SAMPLE-AES-CTR"));
        assert!(m3u8.contains("KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\""));
        assert!(m3u8.contains("KEYID=0x0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn render_with_drm_cbcs_uses_sample_aes() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cbcs,
            widevine_pssh: Some("AAAA".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("METHOD=SAMPLE-AES"));
        assert!(!m3u8.contains("METHOD=SAMPLE-AES-CTR"));
    }

    #[test]
    fn render_with_drm_playready() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: None,
            playready_pssh: Some("BBBB".into()),
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "abcdef01234567890123456789abcdef".into(),
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("KEYFORMAT=\"urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95\""));
    }

    #[test]
    fn render_with_both_drm_systems() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV".into()),
            playready_pssh: Some("PR".into()),
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "00000000000000000000000000000001".into(),
        });
        let m3u8 = render(&state).unwrap();
        let key_count = m3u8.matches("#EXT-X-KEY:").count();
        assert_eq!(key_count, 2);
    }

    #[test]
    fn render_with_fairplay() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cbcs,
            widevine_pssh: None,
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: Some("skd://key-server/key-id".into()),
            default_kid: "0123456789abcdef0123456789abcdef".into(),
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("METHOD=SAMPLE-AES"));
        assert!(m3u8.contains("KEYFORMAT=\"com.apple.streamingkeydelivery\""));
        assert!(m3u8.contains("skd://key-server/key-id"));
    }

    #[test]
    fn render_no_init_segment() {
        let mut state = make_state(ManifestPhase::Live);
        state.segments.push(SegmentInfo {
            number: 0,
            duration: 6.0,
            uri: "seg.cmfv".into(),
            byte_size: 100,
        });
        let m3u8 = render(&state).unwrap();
        assert!(!m3u8.contains("#EXT-X-MAP"));
    }

    #[test]
    fn render_master_video_variant() {
        let mut state = make_state(ManifestPhase::Live);
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
        let m3u8 = render_master(&state, &uris).unwrap();
        assert!(m3u8.contains("#EXTM3U"));
        assert!(m3u8.contains("#EXT-X-STREAM-INF:BANDWIDTH=3000000"));
        assert!(m3u8.contains("CODECS=\"avc1.64001f\""));
        assert!(m3u8.contains("RESOLUTION=1280x720"));
        assert!(m3u8.contains("FRAME-RATE=29.970"));
        assert!(m3u8.contains("v720.m3u8"));
    }

    #[test]
    fn render_master_audio_variant() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "audio_en".into(),
            bandwidth: 128_000,
            codecs: "mp4a.40.2".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Audio,
            language: None,
        });
        let uris = vec!["audio.m3u8".to_string()];
        let m3u8 = render_master(&state, &uris).unwrap();
        assert!(m3u8.contains("#EXT-X-MEDIA:TYPE=AUDIO"));
        assert!(m3u8.contains("GROUP-ID=\"audio\""));
        assert!(m3u8.contains("NAME=\"audio_en\""));
    }

    #[test]
    fn render_master_with_session_key() {
        let mut state = make_state(ManifestPhase::Live);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WVPSSH".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "00000000000000000000000000000001".into(),
        });
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
        let m3u8 = render_master(&state, &uris).unwrap();
        assert!(m3u8.contains("#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES-CTR"));
    }

    #[test]
    fn render_master_missing_uri_falls_back() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 1_000_000,
            codecs: "avc1.42c01e".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        let m3u8 = render_master(&state, &[]).unwrap();
        assert!(m3u8.contains("variant.m3u8"));
    }

    #[test]
    fn render_master_subtitle_rendition() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: Some((1280, 720)),
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.variants.push(VariantInfo {
            id: "sub_eng".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("eng".into()),
        });
        let uris = vec!["v1.m3u8".into(), "subs_eng.m3u8".into()];
        let m3u8 = render_master(&state, &uris).unwrap();
        // Should have subtitle rendition group
        assert!(m3u8.contains("#EXT-X-MEDIA:TYPE=SUBTITLES"));
        assert!(m3u8.contains("GROUP-ID=\"subs\""));
        assert!(m3u8.contains("LANGUAGE=\"eng\""));
        assert!(m3u8.contains("NAME=\"sub_eng\""));
        assert!(m3u8.contains("URI=\"subs_eng.m3u8\""));
        // Video STREAM-INF should reference subs group
        assert!(m3u8.contains("SUBTITLES=\"subs\""));
    }

    #[test]
    fn render_master_subtitle_stpp() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.variants.push(VariantInfo {
            id: "sub_spa".into(),
            bandwidth: 0,
            codecs: "stpp".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("spa".into()),
        });
        let uris = vec!["v1.m3u8".into(), "subs_spa.m3u8".into()];
        let m3u8 = render_master(&state, &uris).unwrap();
        assert!(m3u8.contains("LANGUAGE=\"spa\""));
        assert!(m3u8.contains("NAME=\"sub_spa\""));
        assert!(m3u8.contains("SUBTITLES=\"subs\""));
    }

    #[test]
    fn render_master_subtitle_no_language_defaults_und() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.variants.push(VariantInfo {
            id: "sub1".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: None,
        });
        let uris = vec!["v1.m3u8".into(), "subs.m3u8".into()];
        let m3u8 = render_master(&state, &uris).unwrap();
        assert!(m3u8.contains("LANGUAGE=\"und\""));
    }

    #[test]
    fn render_master_cea_608_captions() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.cea_captions.push(CeaCaptionInfo {
            service_name: "CC1".into(),
            language: "eng".into(),
            is_608: true,
        });
        let uris = vec!["v1.m3u8".into()];
        let m3u8 = render_master(&state, &uris).unwrap();
        // Should have closed captions signaling
        assert!(m3u8.contains("#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS"));
        assert!(m3u8.contains("GROUP-ID=\"cc\""));
        assert!(m3u8.contains("LANGUAGE=\"eng\""));
        assert!(m3u8.contains("INSTREAM-ID=\"CC1\""));
        // Video STREAM-INF should reference cc group
        assert!(m3u8.contains("CLOSED-CAPTIONS=\"cc\""));
    }

    #[test]
    fn render_master_cea_708_captions() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.cea_captions.push(CeaCaptionInfo {
            service_name: "SERVICE1".into(),
            language: "eng".into(),
            is_608: false,
        });
        let uris = vec!["v1.m3u8".into()];
        let m3u8 = render_master(&state, &uris).unwrap();
        assert!(m3u8.contains("INSTREAM-ID=\"SERVICE1\""));
    }

    #[test]
    fn render_master_multiple_captions_and_subs() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.variants.push(VariantInfo {
            id: "sub_eng".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("eng".into()),
        });
        state.variants.push(VariantInfo {
            id: "sub_spa".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("spa".into()),
        });
        state.cea_captions.push(CeaCaptionInfo {
            service_name: "CC1".into(),
            language: "eng".into(),
            is_608: true,
        });
        state.cea_captions.push(CeaCaptionInfo {
            service_name: "CC3".into(),
            language: "spa".into(),
            is_608: true,
        });
        let uris = vec!["v1.m3u8".into(), "subs_eng.m3u8".into(), "subs_spa.m3u8".into()];
        let m3u8 = render_master(&state, &uris).unwrap();
        // Two subtitle renditions
        assert_eq!(m3u8.matches("TYPE=SUBTITLES").count(), 2);
        // Two closed caption entries
        assert_eq!(m3u8.matches("TYPE=CLOSED-CAPTIONS").count(), 2);
        // Video has both group references
        assert!(m3u8.contains("SUBTITLES=\"subs\""));
        assert!(m3u8.contains("CLOSED-CAPTIONS=\"cc\""));
    }

    #[test]
    fn render_master_audio_with_language() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.variants.push(VariantInfo {
            id: "audio_eng".into(),
            bandwidth: 128_000,
            codecs: "mp4a.40.2".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Audio,
            language: Some("eng".into()),
        });
        let uris = vec!["v1.m3u8".into(), "audio.m3u8".into()];
        let m3u8 = render_master(&state, &uris).unwrap();
        assert!(m3u8.contains("LANGUAGE=\"eng\""));
        assert!(m3u8.contains("AUDIO=\"audio\""));
    }
}

/// Render an HLS master playlist referencing variant streams.
pub fn render_master(state: &ManifestState, variant_playlist_uris: &[String]) -> Result<String> {
    use crate::manifest::types::TrackMediaType;

    let mut m3u8 = String::new();

    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:7\n");
    m3u8.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    // Content protection at master level
    if let Some(ref drm) = state.drm_info {
        let method = drm.encryption_scheme.hls_method_string();
        if let Some(ref pssh) = drm.widevine_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-SESSION-KEY:METHOD={method},\
                 URI=\"data:text/plain;base64,{pssh}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }
    }

    // Check if we have subtitle or audio renditions for STREAM-INF attributes
    let has_subtitles = state
        .variants
        .iter()
        .any(|v| v.track_type == TrackMediaType::Subtitle);
    let has_audio = state
        .variants
        .iter()
        .any(|v| v.track_type == TrackMediaType::Audio);
    let has_cea_captions = !state.cea_captions.is_empty();

    // Emit audio rendition groups first
    for (i, variant) in state.variants.iter().enumerate() {
        if variant.track_type != TrackMediaType::Audio {
            continue;
        }
        let uri = variant_playlist_uris
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("variant.m3u8");

        let mut attrs = String::from("TYPE=AUDIO,GROUP-ID=\"audio\"");
        if let Some(ref lang) = variant.language {
            attrs.push_str(&format!(",LANGUAGE=\"{lang}\""));
        }
        attrs.push_str(&format!(",NAME=\"{}\"", variant.id));
        attrs.push_str(",DEFAULT=YES,AUTOSELECT=YES");
        attrs.push_str(&format!(",URI=\"{uri}\""));
        m3u8.push_str(&format!("#EXT-X-MEDIA:{attrs}\n"));
    }

    // Emit subtitle rendition groups
    for (i, variant) in state.variants.iter().enumerate() {
        if variant.track_type != TrackMediaType::Subtitle {
            continue;
        }
        let uri = variant_playlist_uris
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("variant.m3u8");

        let lang = variant.language.as_deref().unwrap_or("und");
        let name = if variant.id.is_empty() {
            lang
        } else {
            &variant.id
        };
        m3u8.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",\
             LANGUAGE=\"{lang}\",NAME=\"{name}\",\
             DEFAULT=NO,AUTOSELECT=YES,URI=\"{uri}\"\n"
        ));
    }

    // CEA-608/708 closed caption signaling
    // In HLS, CEA captions are signaled via EXT-X-MEDIA TYPE=CLOSED-CAPTIONS (no URI).
    // The INSTREAM-ID identifies the CC channel.
    for caption in &state.cea_captions {
        let instream_id = if caption.is_608 {
            // CEA-608: CC1-CC4
            format!("\"{}\"", caption.service_name)
        } else {
            // CEA-708: SERVICE1-SERVICE63
            format!("\"{}\"", caption.service_name)
        };
        m3u8.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=CLOSED-CAPTIONS,GROUP-ID=\"cc\",\
             LANGUAGE=\"{}\",NAME=\"{}\",\
             INSTREAM-ID={instream_id},DEFAULT=NO,AUTOSELECT=YES\n",
            caption.language, caption.service_name
        ));
    }

    // Emit video variant STREAM-INF lines
    for (i, variant) in state.variants.iter().enumerate() {
        if variant.track_type != TrackMediaType::Video {
            continue;
        }
        let uri = variant_playlist_uris
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("variant.m3u8");

        let mut attrs = format!("BANDWIDTH={}", variant.bandwidth);
        attrs.push_str(&format!(",CODECS=\"{}\"", variant.codecs));
        if let Some((w, h)) = variant.resolution {
            attrs.push_str(&format!(",RESOLUTION={w}x{h}"));
        }
        if let Some(fps) = variant.frame_rate {
            attrs.push_str(&format!(",FRAME-RATE={fps:.3}"));
        }
        if has_audio {
            attrs.push_str(",AUDIO=\"audio\"");
        }
        if has_subtitles {
            attrs.push_str(",SUBTITLES=\"subs\"");
        }
        if has_cea_captions {
            attrs.push_str(",CLOSED-CAPTIONS=\"cc\"");
        }
        m3u8.push_str(&format!("#EXT-X-STREAM-INF:{attrs}\n"));
        m3u8.push_str(&format!("{uri}\n"));
    }

    Ok(m3u8)
}
