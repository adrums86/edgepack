use crate::error::Result;
use crate::manifest::types::{ManifestPhase, ManifestState};

/// Decode base64 SCTE-35 command and return hex-encoded string.
fn hex_encode_base64(b64: &str) -> String {
    use base64::Engine;
    use std::fmt::Write;
    match base64::engine::general_purpose::STANDARD.decode(b64) {
        Ok(bytes) => {
            let mut hex = String::with_capacity(bytes.len() * 2);
            for b in &bytes {
                let _ = write!(hex, "{b:02x}");
            }
            hex
        }
        Err(_) => b64.to_string(), // fallback: pass through as-is
    }
}

/// Emit HLS KEY tag for TS AES-128 whole-segment encryption.
///
/// For TS output, the method is always `AES-128` regardless of the target scheme.
/// The key URI points to a key delivery endpoint. IV defaults to the media sequence
/// number per the HLS spec (no explicit IV tag needed).
#[cfg(feature = "ts")]
fn emit_hls_ts_key(m3u8: &mut String, key_uri: &str) {
    m3u8.push_str(&format!(
        "#EXT-X-KEY:METHOD=AES-128,URI=\"{key_uri}\"\n"
    ));
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
        // TS segments are compatible with HLS version 3 (lower compat than CMAF's v7)
        #[cfg(feature = "ts")]
        let version = if !state.container_format.is_isobmff() { 3 } else { 7 };
        #[cfg(not(feature = "ts"))]
        let version = 7;
        m3u8.push_str(&format!("#EXT-X-VERSION:{version}\n"));
    }

    // Target duration (rounded up to nearest integer)
    let target_dur = state.target_duration.ceil() as u64;
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));

    // Media sequence (windowed for DVR)
    m3u8.push_str(&format!(
        "#EXT-X-MEDIA-SEQUENCE:{}\n",
        state.windowed_media_sequence()
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
            // When DVR window is active, omit PLAYLIST-TYPE to allow segments to slide out.
            // Without a DVR window, use EVENT (append-only, all segments stay).
            if !state.is_dvr_active() {
                m3u8.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
            }
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

    // TS output uses AES-128 whole-segment encryption instead of per-sample DRM
    #[cfg(feature = "ts")]
    let is_ts = !state.container_format.is_isobmff();
    #[cfg(not(feature = "ts"))]
    let is_ts = false;

    if has_clear_lead {
        m3u8.push_str("#EXT-X-KEY:METHOD=NONE\n");
    } else if let Some(ref _drm) = state.drm_info {
        if is_ts {
            #[cfg(feature = "ts")]
            {
                let key_uri = format!("{}key", state.base_url);
                emit_hls_ts_key(&mut m3u8, &key_uri);
            }
        } else {
            emit_hls_drm_keys(&mut m3u8, _drm);
        }
    }

    // Init segment (EXT-X-MAP)
    if let Some(ref init) = state.init_segment {
        m3u8.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init.uri));
    }

    // Key rotation state tracking
    let mut last_key_period: Option<u32> = None;

    // Segments (windowed for DVR)
    for segment in state.windowed_segments() {
        // Clear lead transition
        if has_clear_lead && segment.number == clear_lead_boundary {
            if let Some(ref _drm) = state.drm_info {
                if is_ts {
                    #[cfg(feature = "ts")]
                    {
                        let key_uri = format!("{}key", state.base_url);
                        emit_hls_ts_key(&mut m3u8, &key_uri);
                    }
                } else {
                    emit_hls_drm_keys(&mut m3u8, _drm);
                }
            }
        }

        // Key rotation: emit new KEY tag when period changes
        if let Some(period) = segment.key_period {
            if last_key_period != Some(period) && !state.rotation_drm_info.is_empty() {
                if is_ts {
                    #[cfg(feature = "ts")]
                    {
                        let key_uri = format!("{}key", state.base_url);
                        emit_hls_ts_key(&mut m3u8, &key_uri);
                    }
                } else {
                    let drm_idx = period as usize % state.rotation_drm_info.len();
                    emit_hls_drm_keys(&mut m3u8, &state.rotation_drm_info[drm_idx]);
                }
                last_key_period = Some(period);
            }
        }

        // SCTE-35 ad break markers for this segment (windowed for DVR)
        for ab in state.windowed_ad_breaks() {
            if ab.segment_number == segment.number {
                let mut daterange = format!(
                    "#EXT-X-DATERANGE:ID=\"splice-{}\"",
                    ab.id
                );
                // ISO 8601 date from presentation time (epoch-relative)
                let secs = ab.presentation_time as u64;
                let frac = ab.presentation_time - secs as f64;
                let days = secs / 86400;
                let day_secs = secs % 86400;
                daterange.push_str(&format!(
                    ",START-DATE=\"1970-01-{:02}T{:02}:{:02}:{:02}.{:03}Z\"",
                    days + 1, // day-of-month is 1-based
                    day_secs / 3600,
                    (day_secs / 60) % 60,
                    day_secs % 60,
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
        // LL-HLS: emit EXT-X-PART tags before the parent segment's EXTINF
        // per RFC 8216bis Section 4.4.4.9
        if is_ll_hls {
            for part in state.windowed_parts() {
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
            segment_path_prefix: None,
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
            segment_path_prefix: None,
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
            segment_path_prefix: None,
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
            segment_path_prefix: None,
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
            segment_path_prefix: None,
        });
        state.variants.push(VariantInfo {
            id: "sub_eng".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("eng".into()),
            segment_path_prefix: None,
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
            segment_path_prefix: None,
        });
        state.variants.push(VariantInfo {
            id: "sub_spa".into(),
            bandwidth: 0,
            codecs: "stpp".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("spa".into()),
            segment_path_prefix: None,
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
            segment_path_prefix: None,
        });
        state.variants.push(VariantInfo {
            id: "sub1".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: None,
            segment_path_prefix: None,
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
            segment_path_prefix: None,
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
            segment_path_prefix: None,
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
            segment_path_prefix: None,
        });
        state.variants.push(VariantInfo {
            id: "sub_eng".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("eng".into()),
            segment_path_prefix: None,
        });
        state.variants.push(VariantInfo {
            id: "sub_spa".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("spa".into()),
            segment_path_prefix: None,
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
            segment_path_prefix: None,
        });
        state.variants.push(VariantInfo {
            id: "audio_eng".into(),
            bandwidth: 128_000,
            codecs: "mp4a.40.2".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Audio,
            language: Some("eng".into()),
            segment_path_prefix: None,
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
    fn render_ll_hls_parts_before_extinf() {
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
        // RFC 8216bis 4.4.4.9: Parts MUST appear before the EXTINF of the parent segment
        let part00_pos = m3u8.find("/base/part_0.0.cmfv").unwrap();
        let part01_pos = m3u8.find("/base/part_0.1.cmfv").unwrap();
        let seg0_pos = m3u8.find("/base/segment_0.cmfv").unwrap();
        let part10_pos = m3u8.find("/base/part_1.0.cmfv").unwrap();
        let seg1_pos = m3u8.find("/base/segment_1.cmfv").unwrap();
        // Segment 0's parts appear before segment 0's URI
        assert!(part00_pos < seg0_pos, "part 0.0 must precede segment 0 URI");
        assert!(part01_pos < seg0_pos, "part 0.1 must precede segment 0 URI");
        assert!(part01_pos > part00_pos, "parts must be in order");
        // Segment 1's part appears before segment 1's URI
        assert!(part10_pos < seg1_pos, "part 1.0 must precede segment 1 URI");
        // Segment 0's parts appear before segment 1's parts
        assert!(part01_pos < part10_pos, "segment 0 parts before segment 1 parts");
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

    // --- Content steering tests ---

    #[test]
    fn render_master_content_steering_full() {
        let mut state = make_state(ManifestPhase::Live);
        state.content_steering = Some(ContentSteeringConfig {
            server_uri: "https://steer.example.com/v1".into(),
            default_pathway_id: Some("cdn-a".into()),
            query_before_start: None,
        });
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: Some((1280, 720)),
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
            segment_path_prefix: None,
        });
        let m3u8 = render_master(&state, &["v1.m3u8".into()]).unwrap();
        assert!(m3u8.contains("#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://steer.example.com/v1\",PATHWAY-ID=\"cdn-a\""));
    }

    #[test]
    fn render_master_content_steering_server_uri_only() {
        let mut state = make_state(ManifestPhase::Live);
        state.content_steering = Some(ContentSteeringConfig {
            server_uri: "https://steer.example.com/v1".into(),
            default_pathway_id: None,
            query_before_start: None,
        });
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: Some((1280, 720)),
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
            segment_path_prefix: None,
        });
        let m3u8 = render_master(&state, &["v1.m3u8".into()]).unwrap();
        assert!(m3u8.contains("#EXT-X-CONTENT-STEERING:SERVER-URI=\"https://steer.example.com/v1\""));
        assert!(!m3u8.contains("PATHWAY-ID"));
    }

    #[test]
    fn render_master_no_content_steering_backward_compat() {
        let mut state = make_state(ManifestPhase::Live);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: Some((1280, 720)),
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
            segment_path_prefix: None,
        });
        let m3u8 = render_master(&state, &["v1.m3u8".into()]).unwrap();
        assert!(!m3u8.contains("CONTENT-STEERING"));
    }

    #[test]
    fn render_master_content_steering_tag_position() {
        let mut state = make_state(ManifestPhase::Live);
        state.content_steering = Some(ContentSteeringConfig {
            server_uri: "https://steer.example.com/v1".into(),
            default_pathway_id: Some("cdn-a".into()),
            query_before_start: None,
        });
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WV_PSSH".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
            clearkey_pssh: None,
        });
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: Some((1280, 720)),
            frame_rate: None,
            track_type: TrackMediaType::Video,
            language: None,
            segment_path_prefix: None,
        });
        let m3u8 = render_master(&state, &["v1.m3u8".into()]).unwrap();
        let ind_pos = m3u8.find("#EXT-X-INDEPENDENT-SEGMENTS").unwrap();
        let steer_pos = m3u8.find("#EXT-X-CONTENT-STEERING").unwrap();
        let session_pos = m3u8.find("#EXT-X-SESSION-KEY").unwrap();
        assert!(steer_pos > ind_pos, "steering should be after INDEPENDENT-SEGMENTS");
        assert!(steer_pos < session_pos, "steering should be before SESSION-KEY");
    }

    #[test]
    fn render_media_playlist_never_has_content_steering() {
        let mut state = make_live_state_with_segments(2);
        state.content_steering = Some(ContentSteeringConfig {
            server_uri: "https://steer.example.com/v1".into(),
            default_pathway_id: Some("cdn-a".into()),
            query_before_start: None,
        });
        let m3u8 = render(&state).unwrap();
        assert!(!m3u8.contains("CONTENT-STEERING"));
    }

    // --- DATERANGE 24h fix tests ---

    #[test]
    fn render_ad_break_start_date_beyond_24h() {
        let mut state = make_live_state_with_segments(1);
        // presentation_time = 90061.5s = 25h1m1.5s → should be 1970-01-02T01:01:01.500Z
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 90061.5,
            duration: None,
            scte35_cmd: None,
            segment_number: 0,
        });
        let m3u8 = render(&state).unwrap();
        assert!(
            m3u8.contains("START-DATE=\"1970-01-02T01:01:01.500Z\""),
            "START-DATE must not wrap at 24h: {}",
            m3u8
        );
    }

    #[test]
    fn render_ad_break_start_date_within_24h() {
        let mut state = make_live_state_with_segments(1);
        // 12.0s = 0h0m12.0s → 1970-01-01T00:00:12.000Z (unchanged behavior)
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 12.0,
            duration: None,
            scte35_cmd: None,
            segment_number: 0,
        });
        let m3u8 = render(&state).unwrap();
        assert!(m3u8.contains("START-DATE=\"1970-01-01T00:00:12.000Z\""));
    }

    // --- DVR windowed ad breaks + parts tests ---

    #[test]
    fn render_dvr_windowed_ad_breaks() {
        let mut state = make_live_state_with_segments(10);
        state.dvr_window_duration = Some(30.0); // 5 segments × 6.006s ≈ 30s → segments 5-9

        // Ad break in segment 1 (outside window) — should NOT appear
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 6.0,
            duration: Some(15.0),
            scte35_cmd: None,
            segment_number: 1,
        });
        // Ad break in segment 7 (inside window) — should appear
        state.ad_breaks.push(AdBreakInfo {
            id: 2,
            presentation_time: 42.0,
            duration: Some(30.0),
            scte35_cmd: None,
            segment_number: 7,
        });
        let m3u8 = render(&state).unwrap();
        assert!(!m3u8.contains("splice-1"), "ad break outside DVR window should be excluded");
        assert!(m3u8.contains("splice-2"), "ad break inside DVR window should be included");
    }

    #[test]
    fn render_dvr_windowed_parts() {
        let mut state = make_live_state_with_segments(10);
        state.dvr_window_duration = Some(30.0); // segments 5-9 in window
        state.part_target_duration = Some(1.0); // enable LL-HLS

        // Part in segment 1 (outside window)
        state.parts.push(PartInfo {
            segment_number: 1,
            part_index: 0,
            duration: 1.0,
            independent: true,
            uri: "part_1_0.cmfv".into(),
            byte_size: 512,
        });
        // Part in segment 7 (inside window)
        state.parts.push(PartInfo {
            segment_number: 7,
            part_index: 0,
            duration: 1.0,
            independent: true,
            uri: "part_7_0.cmfv".into(),
            byte_size: 512,
        });
        let m3u8 = render(&state).unwrap();
        assert!(!m3u8.contains("part_1_0.cmfv"), "part outside DVR window should be excluded");
        assert!(m3u8.contains("part_7_0.cmfv"), "part inside DVR window should be included");
    }
}

/// Render an HLS I-frame-only playlist from the manifest state.
///
/// Returns `Ok(Some(playlist))` if I-frame data is available and enabled,
/// `Ok(None)` if I-frame playlists are disabled or empty.
///
/// Uses `#EXT-X-I-FRAMES-ONLY` with `#EXT-X-BYTERANGE` pointing into regular segments.
pub fn render_iframe_playlist(state: &ManifestState) -> Result<Option<String>> {
    let iframes = state.windowed_iframe_segments();
    if !state.enable_iframe_playlist || iframes.is_empty() {
        return Ok(None);
    }

    let mut m3u8 = String::new();

    // Header — version 4 required for EXT-X-BYTERANGE
    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:4\n");

    // Target duration (from I-frame durations)
    let max_dur = iframes
        .iter()
        .map(|f| f.duration)
        .fold(0.0f64, f64::max);
    let target_dur = max_dur.ceil() as u64;
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));

    m3u8.push_str(&format!(
        "#EXT-X-MEDIA-SEQUENCE:{}\n",
        state.windowed_media_sequence()
    ));

    m3u8.push_str("#EXT-X-I-FRAMES-ONLY\n");

    // Playlist type
    match state.phase {
        ManifestPhase::Complete => {
            m3u8.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        }
        ManifestPhase::Live => {
            if !state.is_dvr_active() {
                m3u8.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
            }
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

    // I-frame entries (windowed for DVR)
    for iframe in &iframes {
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

    // Content steering
    if let Some(ref cs) = state.content_steering {
        m3u8.push_str(&format!(
            "#EXT-X-CONTENT-STEERING:SERVER-URI=\"{}\"",
            cs.server_uri
        ));
        if let Some(ref pathway) = cs.default_pathway_id {
            m3u8.push_str(&format!(",PATHWAY-ID=\"{pathway}\""));
        }
        m3u8.push('\n');
    }

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
        // URI priority: explicit variant_playlist_uris > segment_path_prefix-derived > fallback
        let uri = variant_playlist_uris
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or_else(|| {
                // For multi-variant with segment_path_prefix, derive URI from prefix
                // e.g., prefix "v/0/" → "v/0/manifest"
                "variant.m3u8"
            });
        let uri_owned;
        let uri = if uri == "variant.m3u8" {
            if let Some(ref prefix) = variant.segment_path_prefix {
                uri_owned = format!("{prefix}manifest");
                uri_owned.as_str()
            } else {
                uri
            }
        } else {
            uri
        };

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
