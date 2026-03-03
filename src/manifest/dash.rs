use crate::error::Result;
use crate::manifest::types::{ManifestPhase, ManifestState, TrackMediaType};

/// Render a DASH MPD manifest from the current state.
///
/// - During `Live` phase: `type="dynamic"` with `minimumUpdatePeriod`
/// - During `Complete` phase: `type="static"` with `mediaPresentationDuration`
pub fn render(state: &ManifestState) -> Result<String> {
    let mut mpd = String::new();

    mpd.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");

    // MPD element
    let mpd_type = match state.phase {
        ManifestPhase::Complete => "static",
        ManifestPhase::Live => "dynamic",
        ManifestPhase::AwaitingFirstSegment => return Ok(mpd),
    };

    let total_duration = state.segments.iter().map(|s| s.duration).sum::<f64>();
    let duration_str = format_iso8601_duration(total_duration);

    mpd.push_str(&format!(
        "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" \
         xmlns:cenc=\"urn:mpeg:cenc:2013\" \
         xmlns:mspr=\"urn:microsoft:playready\" \
         type=\"{mpd_type}\" \
         profiles=\"{profiles}\"",
        profiles = state.container_format.dash_profiles()
    ));

    match state.phase {
        ManifestPhase::Complete => {
            mpd.push_str(&format!(
                " mediaPresentationDuration=\"{duration_str}\""
            ));
        }
        ManifestPhase::Live => {
            mpd.push_str(" minimumUpdatePeriod=\"PT1S\"");
            mpd.push_str(" availabilityStartTime=\"1970-01-01T00:00:00Z\"");
        }
        _ => {}
    }

    let target_dur = format_iso8601_duration(state.target_duration);
    mpd.push_str(&format!(
        " minBufferTime=\"{target_dur}\">\n"
    ));

    // Period
    mpd.push_str("  <Period id=\"0\">\n");

    // SCTE-35 EventStream (ad break signaling)
    if !state.ad_breaks.is_empty() {
        mpd.push_str(
            "    <EventStream schemeIdUri=\"urn:scte:scte35:2013:bin\" timescale=\"90000\">\n",
        );
        for ab in &state.ad_breaks {
            let pts = (ab.presentation_time * 90000.0) as u64;
            let dur = ab
                .duration
                .map(|d| (d * 90000.0) as u64)
                .unwrap_or(0);
            mpd.push_str(&format!(
                "      <Event presentationTime=\"{pts}\" duration=\"{dur}\" id=\"{}\"",
                ab.id
            ));
            if let Some(ref cmd) = ab.scte35_cmd {
                mpd.push_str(&format!(">{cmd}</Event>\n"));
            } else {
                mpd.push_str("/>\n");
            }
        }
        mpd.push_str("    </EventStream>\n");
    }

    // Content Protection at AdaptationSet level
    let cp_xml = build_content_protection_xml(state);

    // Group variants by type
    let video_variants: Vec<_> = state
        .variants
        .iter()
        .filter(|v| v.track_type == TrackMediaType::Video)
        .collect();
    let audio_variants: Vec<_> = state
        .variants
        .iter()
        .filter(|v| v.track_type == TrackMediaType::Audio)
        .collect();
    let subtitle_variants: Vec<_> = state
        .variants
        .iter()
        .filter(|v| v.track_type == TrackMediaType::Subtitle)
        .collect();

    // Video AdaptationSet
    let has_trick_play = state.enable_iframe_playlist && !state.iframe_segments.is_empty();
    if !video_variants.is_empty() || state.variants.is_empty() {
        if has_trick_play {
            mpd.push_str(
                "    <AdaptationSet id=\"1\" contentType=\"video\" mimeType=\"video/mp4\" segmentAlignment=\"true\">\n",
            );
        } else {
            mpd.push_str(
                "    <AdaptationSet contentType=\"video\" mimeType=\"video/mp4\" segmentAlignment=\"true\">\n",
            );
        }
        mpd.push_str(&cp_xml);

        // CEA-608/708 closed caption Accessibility descriptors (inside video AdaptationSet)
        for caption in &state.cea_captions {
            let scheme = if caption.is_608 {
                "urn:scte:dash:cc:cea-608:2015"
            } else {
                "urn:scte:dash:cc:cea-708:2015"
            };
            mpd.push_str(&format!(
                "      <Accessibility schemeIdUri=\"{scheme}\" value=\"{}={}\"/>\n",
                caption.service_name, caption.language
            ));
        }

        // SegmentTemplate
        mpd.push_str(&build_segment_template(state));

        if video_variants.is_empty() {
            // Single representation (no variant info available)
            mpd.push_str("      <Representation id=\"video\" bandwidth=\"2000000\">\n");
            mpd.push_str("      </Representation>\n");
        } else {
            for variant in &video_variants {
                mpd.push_str(&format!(
                    "      <Representation id=\"{}\" bandwidth=\"{}\"",
                    variant.id, variant.bandwidth
                ));
                if !variant.codecs.is_empty() {
                    mpd.push_str(&format!(" codecs=\"{}\"", variant.codecs));
                }
                if let Some((w, h)) = variant.resolution {
                    mpd.push_str(&format!(" width=\"{w}\" height=\"{h}\""));
                }
                if let Some(fps) = variant.frame_rate {
                    mpd.push_str(&format!(" frameRate=\"{fps}\""));
                }
                mpd.push_str(">\n");
                mpd.push_str("      </Representation>\n");
            }
        }

        mpd.push_str("    </AdaptationSet>\n");

        // Trick play AdaptationSet (references main video via EssentialProperty)
        if state.enable_iframe_playlist && !state.iframe_segments.is_empty() {
            mpd.push_str("    <AdaptationSet contentType=\"video\" mimeType=\"video/mp4\" segmentAlignment=\"true\">\n");
            mpd.push_str("      <EssentialProperty schemeIdUri=\"http://dashif.org/guidelines/trickmode\" value=\"1\"/>\n");
            mpd.push_str(&build_segment_template(state));
            if video_variants.is_empty() {
                mpd.push_str("      <Representation id=\"video_trick\" bandwidth=\"200000\">\n");
                mpd.push_str("      </Representation>\n");
            } else {
                for variant in &video_variants {
                    mpd.push_str(&format!(
                        "      <Representation id=\"{}_trick\" bandwidth=\"{}\"",
                        variant.id,
                        variant.bandwidth / 10
                    ));
                    if !variant.codecs.is_empty() {
                        mpd.push_str(&format!(" codecs=\"{}\"", variant.codecs));
                    }
                    if let Some((w, h)) = variant.resolution {
                        mpd.push_str(&format!(" width=\"{w}\" height=\"{h}\""));
                    }
                    mpd.push_str(">\n");
                    mpd.push_str("      </Representation>\n");
                }
            }
            mpd.push_str("    </AdaptationSet>\n");
        }
    }

    // Audio AdaptationSet
    if !audio_variants.is_empty() {
        mpd.push_str(
            "    <AdaptationSet contentType=\"audio\" mimeType=\"audio/mp4\" segmentAlignment=\"true\"",
        );
        // Add lang attribute from first audio variant
        if let Some(lang) = audio_variants.first().and_then(|v| v.language.as_deref()) {
            mpd.push_str(&format!(" lang=\"{lang}\""));
        }
        mpd.push_str(">\n");
        mpd.push_str(&cp_xml);
        mpd.push_str(&build_segment_template(state));

        for variant in &audio_variants {
            mpd.push_str(&format!(
                "      <Representation id=\"{}\" bandwidth=\"{}\"",
                variant.id, variant.bandwidth
            ));
            if !variant.codecs.is_empty() {
                mpd.push_str(&format!(" codecs=\"{}\"", variant.codecs));
            }
            mpd.push_str(">\n");
            mpd.push_str("      </Representation>\n");
        }

        mpd.push_str("    </AdaptationSet>\n");
    }

    // Subtitle AdaptationSet
    if !subtitle_variants.is_empty() {
        mpd.push_str(
            "    <AdaptationSet contentType=\"text\" mimeType=\"application/mp4\" segmentAlignment=\"true\"",
        );
        // Add lang attribute from first subtitle variant
        if let Some(lang) = subtitle_variants.first().and_then(|v| v.language.as_deref()) {
            mpd.push_str(&format!(" lang=\"{lang}\""));
        }
        mpd.push_str(">\n");
        // No content protection for subtitles (they pass through unencrypted)
        mpd.push_str(&build_segment_template(state));

        for variant in &subtitle_variants {
            mpd.push_str(&format!(
                "      <Representation id=\"{}\" bandwidth=\"{}\"",
                variant.id, variant.bandwidth
            ));
            if !variant.codecs.is_empty() {
                mpd.push_str(&format!(" codecs=\"{}\"", variant.codecs));
            }
            mpd.push_str(">\n");
            mpd.push_str("      </Representation>\n");
        }

        mpd.push_str("    </AdaptationSet>\n");
    }

    mpd.push_str("  </Period>\n");
    mpd.push_str("</MPD>\n");

    Ok(mpd)
}

/// Build ContentProtection XML elements for DASH.
fn build_content_protection_xml(state: &ManifestState) -> String {
    let mut xml = String::new();

    if let Some(ref drm) = state.drm_info {
        // mp4protection with scheme-specific value
        let scheme_value = drm.encryption_scheme.scheme_type_str();
        xml.push_str(&format!(
            "      <ContentProtection schemeIdUri=\"urn:mpeg:dash:mp4protection:2011\" \
             value=\"{scheme_value}\" cenc:default_KID=\"{}\"/>\n",
            format_kid_with_hyphens(&drm.default_kid)
        ));

        // Widevine
        if let Some(ref pssh) = drm.widevine_pssh {
            xml.push_str(
                "      <ContentProtection \
                 schemeIdUri=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\">\n",
            );
            xml.push_str(&format!("        <cenc:pssh>{pssh}</cenc:pssh>\n"));
            xml.push_str("      </ContentProtection>\n");
        }

        // PlayReady
        if drm.playready_pssh.is_some() || drm.playready_pro.is_some() {
            xml.push_str(
                "      <ContentProtection \
                 schemeIdUri=\"urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95\">\n",
            );
            if let Some(ref pssh) = drm.playready_pssh {
                xml.push_str(&format!("        <cenc:pssh>{pssh}</cenc:pssh>\n"));
            }
            if let Some(ref pro) = drm.playready_pro {
                xml.push_str(&format!("        <mspr:pro>{pro}</mspr:pro>\n"));
            }
            xml.push_str("      </ContentProtection>\n");
        }

        // ClearKey
        if let Some(ref pssh) = drm.clearkey_pssh {
            xml.push_str(
                "      <ContentProtection \
                 schemeIdUri=\"urn:uuid:e2719d58-a985-b3c9-781a-b030af78d30e\">\n",
            );
            xml.push_str(&format!("        <cenc:pssh>{pssh}</cenc:pssh>\n"));
            xml.push_str("      </ContentProtection>\n");
        }
    }

    xml
}

/// Build SegmentTemplate element for DASH.
fn build_segment_template(state: &ManifestState) -> String {
    let init_uri = state
        .init_segment
        .as_ref()
        .map(|i| i.uri.as_str())
        .unwrap_or("init.mp4");

    let timescale = 1000u32; // millisecond timescale

    let seg_ext = state.container_format.video_segment_extension();
    let mut xml = format!(
        "      <SegmentTemplate timescale=\"{timescale}\" \
         initialization=\"{init_uri}\" \
         media=\"{base}segment_$Number${seg_ext}\" \
         startNumber=\"0\"",
        base = state.base_url
    );

    // LL-DASH: add availabilityTimeOffset and availabilityTimeComplete
    if let Some(ref ll) = state.ll_dash_info {
        xml.push_str(&format!(
            " availabilityTimeOffset=\"{:.3}\"",
            ll.availability_time_offset
        ));
        if !ll.availability_time_complete {
            xml.push_str(" availabilityTimeComplete=\"false\"");
        }
    }

    xml.push_str(">\n");

    xml.push_str("        <SegmentTimeline>\n");
    for segment in &state.segments {
        let duration_ms = (segment.duration * timescale as f64) as u64;
        xml.push_str(&format!(
            "          <S d=\"{duration_ms}\"/>\n"
        ));
    }
    xml.push_str("        </SegmentTimeline>\n");
    xml.push_str("      </SegmentTemplate>\n");
    xml
}

/// Format a duration in seconds as ISO 8601 duration (e.g., "PT120.500S").
fn format_iso8601_duration(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "PT0S".to_string();
    }

    let hours = (seconds / 3600.0).floor() as u64;
    let minutes = ((seconds % 3600.0) / 60.0).floor() as u64;
    let secs = seconds % 60.0;

    let mut s = "PT".to_string();
    if hours > 0 {
        s.push_str(&format!("{hours}H"));
    }
    if minutes > 0 {
        s.push_str(&format!("{minutes}M"));
    }
    if secs > 0.0 || (hours == 0 && minutes == 0) {
        s.push_str(&format!("{secs:.3}S"));
    }
    s
}

/// Format a hex KID string as a UUID with hyphens.
fn format_kid_with_hyphens(kid_hex: &str) -> String {
    if kid_hex.len() != 32 {
        return kid_hex.to_string();
    }
    format!(
        "{}-{}-{}-{}-{}",
        &kid_hex[0..8],
        &kid_hex[8..12],
        &kid_hex[12..16],
        &kid_hex[16..20],
        &kid_hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::scheme::EncryptionScheme;
    use crate::manifest::types::*;
    use crate::media::container::ContainerFormat;

    fn make_state(phase: ManifestPhase) -> ManifestState {
        let mut s = ManifestState::new("content-1".into(), OutputFormat::Dash, "/base/".into(), ContainerFormat::default());
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
                duration: 6.0,
                uri: format!("/base/segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        s
    }

    #[test]
    fn format_iso8601_duration_zero() {
        assert_eq!(format_iso8601_duration(0.0), "PT0S");
        assert_eq!(format_iso8601_duration(-1.0), "PT0S");
    }

    #[test]
    fn format_iso8601_duration_seconds_only() {
        let result = format_iso8601_duration(30.5);
        assert!(result.starts_with("PT"));
        assert!(result.contains("S"));
        assert!(!result.contains("H"));
        assert!(!result.contains("M"));
    }

    #[test]
    fn format_iso8601_duration_minutes_and_seconds() {
        let result = format_iso8601_duration(90.0);
        assert!(result.starts_with("PT"));
        assert!(result.contains("1M"));
        assert!(result.contains("S"));
    }

    #[test]
    fn format_iso8601_duration_hours() {
        let result = format_iso8601_duration(3661.0);
        assert!(result.contains("1H"));
        assert!(result.contains("1M"));
        assert!(result.contains("S"));
    }

    #[test]
    fn format_kid_with_hyphens_valid() {
        let kid = "0123456789abcdef0123456789abcdef";
        let result = format_kid_with_hyphens(kid);
        assert_eq!(result, "01234567-89ab-cdef-0123-456789abcdef");
    }

    #[test]
    fn format_kid_with_hyphens_short() {
        let kid = "0123";
        let result = format_kid_with_hyphens(kid);
        assert_eq!(result, "0123"); // returned unchanged
    }

    #[test]
    fn format_kid_with_hyphens_long() {
        let kid = "0123456789abcdef0123456789abcdef00";
        let result = format_kid_with_hyphens(kid);
        assert_eq!(result, kid); // returned unchanged (34 chars)
    }

    #[test]
    fn render_awaiting_returns_minimal() {
        let state = make_state(ManifestPhase::AwaitingFirstSegment);
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("<?xml"));
        assert!(!mpd.contains("<MPD"));
    }

    #[test]
    fn render_live_dynamic_type() {
        let state = make_live_state_with_segments(2);
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("type=\"dynamic\""));
        assert!(mpd.contains("minimumUpdatePeriod=\"PT1S\""));
        assert!(mpd.contains("availabilityStartTime="));
        assert!(mpd.contains("<Period"));
        assert!(mpd.contains("<AdaptationSet"));
        assert!(mpd.contains("<SegmentTemplate"));
        assert!(mpd.contains("<SegmentTimeline"));
        assert!(mpd.contains("</MPD>"));
    }

    #[test]
    fn render_complete_static_type() {
        let mut state = make_live_state_with_segments(3);
        state.phase = ManifestPhase::Complete;
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("type=\"static\""));
        assert!(mpd.contains("mediaPresentationDuration="));
        assert!(!mpd.contains("minimumUpdatePeriod"));
    }

    #[test]
    fn render_with_drm_content_protection_cenc() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WVDATA".into()),
            playready_pssh: Some("PRDATA".into()),
            playready_pro: Some("<pro>data</pro>".into()),
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
            clearkey_pssh: None,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("urn:mpeg:dash:mp4protection:2011"));
        assert!(mpd.contains("value=\"cenc\""));
        assert!(mpd.contains("cenc:default_KID=\"01234567-89ab-cdef-0123-456789abcdef\""));
        assert!(mpd.contains("urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"));
        assert!(mpd.contains("<cenc:pssh>WVDATA</cenc:pssh>"));
        assert!(mpd.contains("urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95"));
        assert!(mpd.contains("<cenc:pssh>PRDATA</cenc:pssh>"));
        assert!(mpd.contains("<mspr:pro><pro>data</pro></mspr:pro>"));
    }

    #[test]
    fn render_with_drm_content_protection_cbcs() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cbcs,
            widevine_pssh: Some("WVDATA".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
            clearkey_pssh: None,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("value=\"cbcs\""));
    }

    #[test]
    fn render_segment_timeline_durations() {
        let state = make_live_state_with_segments(3);
        let mpd = render(&state).unwrap();
        let s_count = mpd.matches("<S d=").count();
        assert_eq!(s_count, 3);
        assert!(mpd.contains("<S d=\"6000\"/>"));
    }

    #[test]
    fn render_with_video_variants() {
        let mut state = make_live_state_with_segments(1);
        state.variants.push(VariantInfo {
            id: "v720".into(),
            bandwidth: 3_000_000,
            codecs: "avc1.64001f".into(),
            resolution: Some((1280, 720)),
            frame_rate: Some(30.0),
            track_type: TrackMediaType::Video,
            language: None,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("contentType=\"video\""));
        assert!(mpd.contains("id=\"v720\""));
        assert!(mpd.contains("bandwidth=\"3000000\""));
        assert!(mpd.contains("codecs=\"avc1.64001f\""));
        assert!(mpd.contains("width=\"1280\""));
        assert!(mpd.contains("height=\"720\""));
    }

    #[test]
    fn render_with_audio_variants() {
        let mut state = make_live_state_with_segments(1);
        state.variants.push(VariantInfo {
            id: "a1".into(),
            bandwidth: 128_000,
            codecs: "mp4a.40.2".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Audio,
            language: None,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("contentType=\"audio\""));
        assert!(mpd.contains("id=\"a1\""));
        assert!(mpd.contains("bandwidth=\"128000\""));
    }

    #[test]
    fn render_default_video_adaptation_set_when_no_variants() {
        let state = make_live_state_with_segments(1);
        let mpd = render(&state).unwrap();
        // Should still have a video AdaptationSet with default representation
        assert!(mpd.contains("contentType=\"video\""));
        assert!(mpd.contains("bandwidth=\"2000000\""));
    }

    #[test]
    fn render_profiles() {
        let state = make_live_state_with_segments(1);
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("urn:mpeg:dash:profile:isoff-live:2011"));
        assert!(mpd.contains("urn:mpeg:dash:profile:cmaf:2019"));
    }

    #[test]
    fn render_namespaces() {
        let state = make_live_state_with_segments(1);
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("xmlns=\"urn:mpeg:dash:schema:mpd:2011\""));
        assert!(mpd.contains("xmlns:cenc=\"urn:mpeg:cenc:2013\""));
        assert!(mpd.contains("xmlns:mspr=\"urn:microsoft:playready\""));
    }

    #[test]
    fn render_with_subtitle_variants() {
        let mut state = make_live_state_with_segments(1);
        state.variants.push(VariantInfo {
            id: "sub_eng".into(),
            bandwidth: 0,
            codecs: "wvtt".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("eng".into()),
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("contentType=\"text\""));
        assert!(mpd.contains("mimeType=\"application/mp4\""));
        assert!(mpd.contains("lang=\"eng\""));
        assert!(mpd.contains("id=\"sub_eng\""));
        assert!(mpd.contains("codecs=\"wvtt\""));
    }

    #[test]
    fn render_with_subtitle_stpp() {
        let mut state = make_live_state_with_segments(1);
        state.variants.push(VariantInfo {
            id: "sub_spa".into(),
            bandwidth: 0,
            codecs: "stpp".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("spa".into()),
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("contentType=\"text\""));
        assert!(mpd.contains("lang=\"spa\""));
        assert!(mpd.contains("codecs=\"stpp\""));
    }

    #[test]
    fn render_subtitle_no_content_protection() {
        let mut state = make_live_state_with_segments(1);
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("WVDATA".into()),
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
        let mpd = render(&state).unwrap();
        // Video AdaptationSet should have ContentProtection
        assert!(mpd.contains("urn:mpeg:dash:mp4protection:2011"));
        // Count schemeIdUri occurrences — should only be in video AdaptationSet
        // (2: mp4protection + Widevine)
        let scheme_count = mpd.matches("schemeIdUri").count();
        assert_eq!(scheme_count, 2, "ContentProtection only in video AdaptationSet");
    }

    #[test]
    fn render_with_cea_608_captions() {
        let mut state = make_live_state_with_segments(1);
        state.cea_captions.push(CeaCaptionInfo {
            service_name: "CC1".into(),
            language: "eng".into(),
            is_608: true,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("urn:scte:dash:cc:cea-608:2015"));
        assert!(mpd.contains("value=\"CC1=eng\""));
    }

    #[test]
    fn render_with_cea_708_captions() {
        let mut state = make_live_state_with_segments(1);
        state.cea_captions.push(CeaCaptionInfo {
            service_name: "SERVICE1".into(),
            language: "eng".into(),
            is_608: false,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("urn:scte:dash:cc:cea-708:2015"));
        assert!(mpd.contains("value=\"SERVICE1=eng\""));
    }

    #[test]
    fn render_with_multiple_cea_captions() {
        let mut state = make_live_state_with_segments(1);
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
        let mpd = render(&state).unwrap();
        assert_eq!(mpd.matches("Accessibility").count(), 2);
        assert!(mpd.contains("value=\"CC1=eng\""));
        assert!(mpd.contains("value=\"CC3=spa\""));
    }

    #[test]
    fn render_audio_with_language() {
        let mut state = make_live_state_with_segments(1);
        state.variants.push(VariantInfo {
            id: "a1".into(),
            bandwidth: 128_000,
            codecs: "mp4a.40.2".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Audio,
            language: Some("eng".into()),
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("contentType=\"audio\""));
        assert!(mpd.contains("lang=\"eng\""));
    }

    #[test]
    fn render_combined_video_audio_subtitle_cea() {
        let mut state = make_live_state_with_segments(1);
        state.variants.push(VariantInfo {
            id: "v1".into(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".into(),
            resolution: Some((1280, 720)),
            frame_rate: Some(30.0),
            track_type: TrackMediaType::Video,
            language: None,
        });
        state.variants.push(VariantInfo {
            id: "a1".into(),
            bandwidth: 128_000,
            codecs: "mp4a.40.2".into(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Audio,
            language: Some("eng".into()),
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
        state.cea_captions.push(CeaCaptionInfo {
            service_name: "CC1".into(),
            language: "eng".into(),
            is_608: true,
        });
        let mpd = render(&state).unwrap();
        // Should have all three AdaptationSets
        assert!(mpd.contains("contentType=\"video\""));
        assert!(mpd.contains("contentType=\"audio\""));
        assert!(mpd.contains("contentType=\"text\""));
        // CEA inside video AdaptationSet
        assert!(mpd.contains("urn:scte:dash:cc:cea-608:2015"));
        // Count AdaptationSet elements
        assert_eq!(mpd.matches("<AdaptationSet").count(), 3);
    }

    // --- SCTE-35 EventStream tests ---

    #[test]
    fn render_with_ad_break_event_stream() {
        let mut state = make_live_state_with_segments(3);
        state.ad_breaks.push(AdBreakInfo {
            id: 42,
            presentation_time: 12.0,
            duration: Some(30.0),
            scte35_cmd: None,
            segment_number: 2,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("<EventStream schemeIdUri=\"urn:scte:scte35:2013:bin\" timescale=\"90000\">"));
        assert!(mpd.contains("id=\"42\""));
        // 12s * 90000 = 1080000
        assert!(mpd.contains("presentationTime=\"1080000\""));
        // 30s * 90000 = 2700000
        assert!(mpd.contains("duration=\"2700000\""));
    }

    #[test]
    fn render_with_ad_break_scte35_cmd() {
        let mut state = make_live_state_with_segments(1);
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 0.0,
            duration: None,
            scte35_cmd: Some("AABBCC".to_string()),
            segment_number: 0,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains(">AABBCC</Event>"));
    }

    #[test]
    fn render_no_ad_breaks_no_event_stream() {
        let state = make_live_state_with_segments(2);
        let mpd = render(&state).unwrap();
        assert!(!mpd.contains("EventStream"));
    }

    #[test]
    fn render_multiple_ad_breaks_single_event_stream() {
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
        let mpd = render(&state).unwrap();
        // One EventStream with two Events
        assert_eq!(mpd.matches("<EventStream").count(), 1);
        assert_eq!(mpd.matches("<Event ").count(), 2);
    }

    #[test]
    fn render_ad_break_no_duration() {
        let mut state = make_live_state_with_segments(1);
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 0.0,
            duration: None,
            scte35_cmd: None,
            segment_number: 0,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("duration=\"0\""));
    }

    #[test]
    fn render_ad_break_event_stream_in_period() {
        let mut state = make_live_state_with_segments(1);
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 0.0,
            duration: Some(15.0),
            scte35_cmd: None,
            segment_number: 0,
        });
        let mpd = render(&state).unwrap();
        // EventStream should be inside Period, before AdaptationSet
        let event_stream_pos = mpd.find("<EventStream").unwrap();
        let adaptation_set_pos = mpd.find("<AdaptationSet").unwrap();
        let period_pos = mpd.find("<Period").unwrap();
        assert!(event_stream_pos > period_pos);
        assert!(event_stream_pos < adaptation_set_pos);
    }

    // --- LL-DASH rendering tests ---

    #[test]
    fn render_ll_dash_availability_time_offset() {
        let mut state = make_live_state_with_segments(2);
        state.ll_dash_info = Some(crate::manifest::types::LowLatencyDashInfo {
            availability_time_offset: 5.0,
            availability_time_complete: false,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("availabilityTimeOffset=\"5.000\""));
        assert!(mpd.contains("availabilityTimeComplete=\"false\""));
    }

    #[test]
    fn render_ll_dash_atc_true_not_emitted() {
        let mut state = make_live_state_with_segments(1);
        state.ll_dash_info = Some(crate::manifest::types::LowLatencyDashInfo {
            availability_time_offset: 3.5,
            availability_time_complete: true,
        });
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("availabilityTimeOffset=\"3.500\""));
        // When ATC is true (default), it should NOT be emitted
        assert!(!mpd.contains("availabilityTimeComplete"));
    }

    #[test]
    fn render_no_ll_dash_backward_compat() {
        let state = make_live_state_with_segments(2);
        let mpd = render(&state).unwrap();
        assert!(!mpd.contains("availabilityTimeOffset"));
        assert!(!mpd.contains("availabilityTimeComplete"));
    }

    #[test]
    fn render_with_clearkey_content_protection() {
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
        let mpd = render(&state).unwrap();
        assert!(mpd.contains("urn:uuid:e2719d58-a985-b3c9-781a-b030af78d30e"));
        assert!(mpd.contains("<cenc:pssh>CKDATA</cenc:pssh>"));
    }
}
