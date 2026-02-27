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

    // DRM signaling — CENC (Widevine + PlayReady)
    if let Some(ref drm) = state.drm_info {
        // Widevine
        if let Some(ref pssh) = drm.widevine_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,\
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
                "#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,\
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
    use crate::manifest::types::*;

    fn make_state(phase: ManifestPhase) -> ManifestState {
        let mut s = ManifestState::new("test".into(), OutputFormat::Hls, "/base/".into());
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
            widevine_pssh: Some("AAAA".into()),
            playready_pssh: None,
            playready_pro: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("METHOD=SAMPLE-AES-CTR"));
        assert!(m3u8.contains("KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\""));
        assert!(m3u8.contains("KEYID=0x0123456789abcdef0123456789abcdef"));
    }

    #[test]
    fn render_with_drm_playready() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            widevine_pssh: None,
            playready_pssh: Some("BBBB".into()),
            playready_pro: None,
            default_kid: "abcdef01234567890123456789abcdef".into(),
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("KEYFORMAT=\"urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95\""));
    }

    #[test]
    fn render_with_both_drm_systems() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            widevine_pssh: Some("WV".into()),
            playready_pssh: Some("PR".into()),
            playready_pro: None,
            default_kid: "00000000000000000000000000000001".into(),
        });
        let m3u8 = render(&state).unwrap();
        let key_count = m3u8.matches("#EXT-X-KEY:").count();
        assert_eq!(key_count, 2);
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
            widevine_pssh: Some("WVPSSH".into()),
            playready_pssh: None,
            playready_pro: None,
            default_kid: "00000000000000000000000000000001".into(),
        });
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Video,
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
        });
        let m3u8 = render_master(&state, &[]).unwrap();
        assert!(m3u8.contains("variant.m3u8"));
    }
}

/// Render an HLS master playlist referencing variant streams.
pub fn render_master(state: &ManifestState, variant_playlist_uris: &[String]) -> Result<String> {
    let mut m3u8 = String::new();

    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:7\n");
    m3u8.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    // Content protection at master level
    if let Some(ref drm) = state.drm_info {
        if let Some(ref pssh) = drm.widevine_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES-CTR,\
                 URI=\"data:text/plain;base64,{pssh}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }
    }

    for (i, variant) in state.variants.iter().enumerate() {
        let uri = variant_playlist_uris
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("variant.m3u8");

        match variant.track_type {
            crate::manifest::types::TrackMediaType::Video => {
                let mut attrs = format!("BANDWIDTH={}", variant.bandwidth);
                attrs.push_str(&format!(",CODECS=\"{}\"", variant.codecs));
                if let Some((w, h)) = variant.resolution {
                    attrs.push_str(&format!(",RESOLUTION={w}x{h}"));
                }
                if let Some(fps) = variant.frame_rate {
                    attrs.push_str(&format!(",FRAME-RATE={fps:.3}"));
                }
                m3u8.push_str(&format!("#EXT-X-STREAM-INF:{attrs}\n"));
                m3u8.push_str(&format!("{uri}\n"));
            }
            crate::manifest::types::TrackMediaType::Audio => {
                m3u8.push_str(&format!(
                    "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{}\",\
                     DEFAULT=YES,AUTOSELECT=YES,URI=\"{uri}\"\n",
                    variant.id
                ));
            }
        }
    }

    Ok(m3u8)
}
