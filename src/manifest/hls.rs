use crate::error::Result;
use crate::manifest::types::{ManifestPhase, ManifestState};

/// Decode base64 SCTE-35 command and return hex-encoded string.
fn hex_encode_base64(b64: &str) -> String {
    use base64::Engine;
    match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(bytes) => bytes.iter().map(|b| format!("{b:02x}")).collect(),
        Err(_) => b64.to_string(), // fallback: pass through as-is
    }
}

/// Emit HLS DRM KEY tags for a given ManifestDrmInfo.
fn emit_hls_drm_keys(m3u8: &mut String, drm: &crate::manifest::types::ManifestDrmInfo) {
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

    // ClearKey
    if let Some(ref pssh) = drm.clearkey_pssh {
        m3u8.push_str(&format!(
            "#EXT-X-KEY:METHOD={method},\
             URI=\"data:text/plain;base64,{pssh}\",\
             KEYID=0x{},\
             KEYFORMAT=\"urn:uuid:e2719d58-a985-b3c9-781a-b030af78d30e\",\
             KEYFORMATVERSIONS=\"1\"\n",
            drm.default_kid
        ));
    }
}

/// Render an HLS M3U8 manifest from the current state.
///
/// - During `Live` phase: produces a live playlist (no `#EXT-X-ENDLIST`)
/// - During `Complete` phase: produces a VOD playlist (with `#EXT-X-ENDLIST`)
pub fn render(state: &ManifestState) -> Result<String> {
    let mut m3u8 = String::new();

    // Determine if this is an LL-HLS playlist (version 9)
    let is_ll_hls = state.part_target_duration.is_some() || !state.parts.is_empty();

    // Header
    m3u8.push_str("#EXTM3U\n");
    if is_ll_hls {
        m3u8.push_str("#EXT-X-VERSION:9\n");
    } else {
        m3u8.push_str("#EXT-X-VERSION:7\n");
    }

    // Target duration (rounded up to nearest integer)
    let target_dur = state.target_duration.ceil() as u64;
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));

    // Media sequence
    m3u8.push_str(&format!(
        "#EXT-X-MEDIA-SEQUENCE:{}\n",
        state.media_sequence
    ));

    // LL-HLS: Server Control
    if let Some(ref sc) = state.server_control {
        let mut attrs = Vec::new();
        if sc.can_block_reload {
            attrs.push("CAN-BLOCK-RELOAD=YES".to_string());
        }
        if let Some(phb) = sc.part_hold_back {
            attrs.push(format!("PART-HOLD-BACK={phb:.5}"));
        }
        if let Some(hb) = sc.hold_back {
            attrs.push(format!("HOLD-BACK={hb:.5}"));
        }
        if let Some(csu) = sc.can_skip_until {
            attrs.push(format!("CAN-SKIP-UNTIL={csu:.5}"));
        }
        if !attrs.is_empty() {
            m3u8.push_str(&format!("#EXT-X-SERVER-CONTROL:{}\n", attrs.join(",")));
        }
    }

    // LL-HLS: Part Target Duration
    if let Some(ptd) = state.part_target_duration {
        m3u8.push_str(&format!("#EXT-X-PART-INF:PART-TARGET={ptd:.5}\n"));
    }

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
    // Clear lead: first N segments are unencrypted
    let has_clear_lead = state.clear_lead_boundary.is_some() && state.drm_info.is_some();
    let clear_lead_boundary = state.clear_lead_boundary.unwrap_or(0);

    if has_clear_lead {
        m3u8.push_str("#EXT-X-KEY:METHOD=NONE\n");
    } else if let Some(ref drm) = state.drm_info {
        emit_hls_drm_keys(&mut m3u8, drm);
    }

    // Init segment (EXT-X-MAP)
    if let Some(ref init) = state.init_segment {
        m3u8.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init.uri));
    }

    // Key rotation state tracking
    let mut last_key_period: Option<u32> = None;

    // Segments
    for segment in &state.segments {
        // Clear lead transition
        if has_clear_lead && segment.number == clear_lead_boundary {
            if let Some(ref drm) = state.drm_info {
                emit_hls_drm_keys(&mut m3u8, drm);
            }
        }

        // Key rotation: emit new KEY tag when period changes
        if let Some(period) = segment.key_period {
            if last_key_period != Some(period) && !state.rotation_drm_info.is_empty() {
                let drm_idx = period as usize % state.rotation_drm_info.len();
                emit_hls_drm_keys(&mut m3u8, &state.rotation_drm_info[drm_idx]);
                last_key_period = Some(period);
            }
        }

        // SCTE-35 ad break markers for this segment
        for ab in &state.ad_breaks {
            if ab.segment_number == segment.number {
                let mut daterange = format!(
                    "#EXT-X-DATERANGE:ID=\"splice-{}\"",
                    ab.id
                );
                // ISO 8601 date from presentation time (epoch-relative)
                let secs = ab.presentation_time as u64;
                let frac = ab.presentation_time - secs as f64;
                daterange.push_str(&format!(
                    ",START-DATE=\"1970-01-01T{:02}:{:02}:{:02}.{:03}Z\"",
                    (secs / 3600) % 24,
                    (secs / 60) % 60,
                    secs % 60,
                    (frac * 1000.0) as u32
                ));
                if let Some(dur) = ab.duration {
                    daterange.push_str(&format!(",PLANNED-DURATION={dur:.3}"));
                }
                if let Some(ref cmd) = ab.scte35_cmd {
                    daterange.push_str(&format!(",SCTE35-CMD=0x{}", hex_encode_base64(cmd)));
                }
                m3u8.push_str(&daterange);
                m3u8.push('\n');
            }
        }
        m3u8.push_str(&format!("#EXTINF:{:.6},\n", segment.duration));
        m3u8.push_str(&format!("{}\n", segment.uri));

        // LL-HLS: emit EXT-X-PART tags for parts belonging to this segment
        if is_ll_hls {
            for part in &state.parts {
                if part.segment_number == segment.number {
                    let mut part_attrs = format!(
                        "DURATION={:.5},URI=\"{}\"",
                        part.duration, part.uri
                    );
                    if part.independent {
                        part_attrs.push_str(",INDEPENDENT=YES");
                    }
                    m3u8.push_str(&format!("#EXT-X-PART:{part_attrs}\n"));
                }
            }
        }
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
                key_period: None,
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
            clearkey_pssh: None,
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
            clearkey_pssh: None,
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
            clearkey_pssh: None,
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
            clearkey_pssh: None,
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
            clearkey_pssh: None,
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("METHOD=SAMPLE-AES"));
        assert!(m3u8.contains("KEYFORMAT=\"com.apple.streamingkeydelivery\""));
        assert!(m3u8.contains("skd://key-server/key-id"));
    }

    #[test]
    fn render_with_ad_break() {
        let mut state = make_live_state_with_segments(3);
        state.ad_breaks.push(AdBreakInfo {
            id: 42,
            presentation_time: 12.0,
            duration: Some(30.0),
            scte35_cmd: None,
            segment_number: 2,
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-DATERANGE:ID=\"splice-42\""));
        assert!(m3u8.contains("PLANNED-DURATION=30.000"));
        // Should appear before segment 2's EXTINF
        let daterange_pos = m3u8.find("splice-42").unwrap();
        let seg2_pos = m3u8.find("/base/segment_2.cmfv").unwrap();
        assert!(daterange_pos < seg2_pos);
    }

    #[test]
    fn render_with_ad_break_no_duration() {
        let mut state = make_live_state_with_segments(2);
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 6.0,
            duration: None,
            scte35_cmd: None,
            segment_number: 1,
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-DATERANGE:ID=\"splice-1\""));
        assert!(!m3u8.contains("PLANNED-DURATION"));
    }

    #[test]
    fn render_with_ad_break_scte35_cmd() {
        let mut state = make_live_state_with_segments(1);
        // Base64 of [0xFC, 0x30] → "/DA="
        state.ad_breaks.push(AdBreakInfo {
            id: 5,
            presentation_time: 0.0,
            duration: None,
            scte35_cmd: Some("/DA=".to_string()),
            segment_number: 0,
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("SCTE35-CMD=0x"));
    }

    #[test]
    fn render_no_ad_breaks_unchanged() {
        let state = make_live_state_with_segments(2);
        let m3u8 = render(&state).unwrap();
        assert!(!m3u8.contains("EXT-X-DATERANGE"));
    }

    #[test]
    fn render_multiple_ad_breaks() {
        let mut state = make_live_state_with_segments(3);
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 6.0,
            duration: Some(15.0),
            scte35_cmd: None,
            segment_number: 1,
        });
        state.ad_breaks.push(AdBreakInfo {
            id: 2,
            presentation_time: 12.0,
            duration: Some(30.0),
            scte35_cmd: None,
            segment_number: 2,
        });
        let m3u8 = render(&state).unwrap();
        assert_eq!(m3u8.matches("EXT-X-DATERANGE").count(), 2);
    }

    #[test]
    fn render_no_init_segment() {
        let mut state = make_state(ManifestPhase::Live);
        state.segments.push(SegmentInfo {
            number: 0,
            duration: 6.0,
            uri: "seg.cmfv".into(),
            byte_size: 100,
            key_period: None,
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
            clearkey_pssh: None,
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

    #[test]
    fn render_with_clearkey() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: None,
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
            clearkey_pssh: Some("CKDATA".into()),
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("KEYFORMAT=\"urn:uuid:e2719d58-a985-b3c9-781a-b030af78d30e\""));
        assert!(m3u8.contains("CKDATA"));
    }

    #[test]
    fn render_with_clear_lead() {
        let mut state = make_live_state_with_segments(4);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "00000000000000000000000000000001".into(),
            clearkey_pssh: None,
        });
        state.clear_lead_boundary = Some(2);
        let m3u8 = render(&state).unwrap();
        // Should have METHOD=NONE at start
        assert!(m3u8.contains("METHOD=NONE"));
        // Should have DRM key at boundary
        let none_pos = m3u8.find("METHOD=NONE").unwrap();
        let drm_pos = m3u8.rfind("SAMPLE-AES-CTR").unwrap();
        assert!(drm_pos > none_pos);
    }

    // --- LL-HLS rendering tests ---

    #[test]
    fn render_ll_hls_version_9() {
        let mut state = make_live_state_with_segments(1);
        state.part_target_duration = Some(0.33334);
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-VERSION:9"));
        assert!(!m3u8.contains("#EXT-X-VERSION:7"));
    }

    #[test]
    fn render_ll_hls_part_inf() {
        let mut state = make_live_state_with_segments(1);
        state.part_target_duration = Some(0.33334);
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-PART-INF:PART-TARGET=0.33334"));
    }

    #[test]
    fn render_ll_hls_server_control() {
        let mut state = make_live_state_with_segments(1);
        state.part_target_duration = Some(0.33334);
        state.server_control = Some(crate::manifest::types::ServerControl {
            can_skip_until: Some(12.0),
            hold_back: None,
            part_hold_back: Some(1.0),
            can_block_reload: true,
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-SERVER-CONTROL:"));
        assert!(m3u8.contains("CAN-BLOCK-RELOAD=YES"));
        assert!(m3u8.contains("PART-HOLD-BACK="));
        assert!(m3u8.contains("CAN-SKIP-UNTIL="));
    }

    #[test]
    fn render_ll_hls_parts_after_segments() {
        let mut state = make_live_state_with_segments(2);
        state.part_target_duration = Some(0.33334);
        state.parts.push(crate::manifest::types::PartInfo {
            segment_number: 0,
            part_index: 0,
            duration: 0.33334,
            independent: true,
            uri: "/base/part_0.0.cmfv".into(),
            byte_size: 5000,
        });
        state.parts.push(crate::manifest::types::PartInfo {
            segment_number: 0,
            part_index: 1,
            duration: 0.33334,
            independent: false,
            uri: "/base/part_0.1.cmfv".into(),
            byte_size: 4000,
        });
        state.parts.push(crate::manifest::types::PartInfo {
            segment_number: 1,
            part_index: 0,
            duration: 0.33334,
            independent: true,
            uri: "/base/part_1.0.cmfv".into(),
            byte_size: 5000,
        });
        let m3u8 = render(&state).unwrap();
        // Parts should appear after their segment's URI
        let seg0_pos = m3u8.find("/base/segment_0.cmfv").unwrap();
        let part00_pos = m3u8.find("/base/part_0.0.cmfv").unwrap();
        let part01_pos = m3u8.find("/base/part_0.1.cmfv").unwrap();
        assert!(part00_pos > seg0_pos);
        assert!(part01_pos > part00_pos);
        // Independent flag
        assert!(m3u8.contains("INDEPENDENT=YES"));
        // Part count
        assert_eq!(m3u8.matches("#EXT-X-PART:").count(), 3);
    }

    #[test]
    fn render_ll_hls_independent_flag() {
        let mut state = make_live_state_with_segments(1);
        state.part_target_duration = Some(0.5);
        state.parts.push(crate::manifest::types::PartInfo {
            segment_number: 0,
            part_index: 0,
            duration: 0.5,
            independent: true,
            uri: "/base/part.cmfv".into(),
            byte_size: 1000,
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("INDEPENDENT=YES"));
    }

    #[test]
    fn render_backward_compat_no_parts_version_7() {
        let state = make_live_state_with_segments(2);
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("#EXT-X-VERSION:7"));
        assert!(!m3u8.contains("#EXT-X-VERSION:9"));
        assert!(!m3u8.contains("#EXT-X-PART-INF"));
        assert!(!m3u8.contains("#EXT-X-SERVER-CONTROL"));
        assert!(!m3u8.contains("#EXT-X-PART:"));
    }

    #[test]
    fn render_with_key_rotation() {
        let mut state = make_state(ManifestPhase::Live);
        state.init_segment = Some(InitSegmentInfo { uri: "/base/init.mp4".into(), byte_size: 256 });
        // Add segments with different key periods
        for i in 0..6u32 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("/base/segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: Some(i / 3), // period changes at segment 3
            });
        }
        state.rotation_drm_info.push(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV_P0".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "00000000000000000000000000000001".into(),
            clearkey_pssh: None,
        });
        state.rotation_drm_info.push(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV_P1".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "00000000000000000000000000000002".into(),
            clearkey_pssh: None,
        });
        let m3u8 = render(&state).unwrap();
        // Should have KEY tags for both periods
        assert!(m3u8.contains("WV_P0"));
        assert!(m3u8.contains("WV_P1"));
    }
}

/// Render an HLS I-frame-only playlist from the manifest state.
///
/// Returns `Ok(Some(playlist))` if I-frame data is available and enabled,
/// `Ok(None)` if I-frame playlists are disabled or empty.
///
/// Uses `#EXT-X-I-FRAMES-ONLY` with `#EXT-X-BYTERANGE` pointing into regular segments.
pub fn render_iframe_playlist(state: &ManifestState) -> Result<Option<String>> {
    if !state.enable_iframe_playlist || state.iframe_segments.is_empty() {
        return Ok(None);
    }

    let mut m3u8 = String::new();

    // Header — version 4 required for EXT-X-BYTERANGE
    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:4\n");

    // Target duration (from I-frame durations)
    let max_dur = state
        .iframe_segments
        .iter()
        .map(|f| f.duration)
        .fold(0.0f64, f64::max);
    let target_dur = max_dur.ceil() as u64;
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));

    m3u8.push_str(&format!(
        "#EXT-X-MEDIA-SEQUENCE:{}\n",
        state.media_sequence
    ));

    m3u8.push_str("#EXT-X-I-FRAMES-ONLY\n");

    // Playlist type
    match state.phase {
        ManifestPhase::Complete => {
            m3u8.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        }
        ManifestPhase::Live => {
            m3u8.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        ManifestPhase::AwaitingFirstSegment => {
            return Ok(None);
        }
    }

    // DRM KEY tags (same encryption as regular playlist)
    if let Some(ref drm) = state.drm_info {
        emit_hls_drm_keys(&mut m3u8, drm);
    }

    // Init segment map (same init as regular playlist)
    if let Some(ref init) = state.init_segment {
        m3u8.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init.uri));
    }

    // I-frame entries
    for iframe in &state.iframe_segments {
        m3u8.push_str(&format!("#EXTINF:{:.6},\n", iframe.duration));
        m3u8.push_str(&format!(
            "#EXT-X-BYTERANGE:{}@{}\n",
            iframe.byte_length, iframe.byte_offset
        ));
        m3u8.push_str(&format!("{}\n", iframe.segment_uri));
    }

    // End list
    if state.phase == ManifestPhase::Complete {
        m3u8.push_str("#EXT-X-ENDLIST\n");
    }

    Ok(Some(m3u8))
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

    // I-Frame stream info for each video variant (when enabled and data available)
    if state.enable_iframe_playlist && !state.iframe_segments.is_empty() {
        for variant in state.variants.iter() {
            if variant.track_type != TrackMediaType::Video {
                continue;
            }
            let mut attrs = format!("BANDWIDTH={}", variant.bandwidth / 10);
            attrs.push_str(&format!(",CODECS=\"{}\"", variant.codecs));
            if let Some((w, h)) = variant.resolution {
                attrs.push_str(&format!(",RESOLUTION={w}x{h}"));
            }
            attrs.push_str(",URI=\"iframes\"");
            m3u8.push_str(&format!("#EXT-X-I-FRAME-STREAM-INF:{attrs}\n"));
        }
    }

    Ok(m3u8)
}
