//! DASH MPD source manifest input parser.
//!
//! Parses a DASH MPD to extract init segment URL, media segment URLs,
//! durations, and live/VOD status. This is the *input* side — the output
//! renderer is in `dash.rs`.

use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgePackagerError, Result};
use crate::manifest::types::SourceManifest;
use quick_xml::events::Event;
use quick_xml::Reader;
use url::Url;

/// Parse a DASH MPD manifest into a `SourceManifest`.
///
/// Supports the most common CMAF DASH patterns:
/// - `<SegmentTemplate>` with `<SegmentTimeline>` and `$Number$` substitution
/// - `<SegmentTemplate>` with `duration` attribute (uniform segments)
///
/// The `manifest_url` is used as the base for resolving relative segment URIs.
pub fn parse_dash_manifest(manifest_text: &str, manifest_url: &str) -> Result<SourceManifest> {
    let base_url = Url::parse(manifest_url).map_err(|e| {
        EdgePackagerError::Manifest(format!("invalid manifest URL: {e}"))
    })?;

    let mut reader = Reader::from_str(manifest_text);
    reader.config_mut().trim_text(true);

    let mut is_live = false;
    let mut source_scheme: Option<EncryptionScheme> = None;
    let mut init_template: Option<String> = None;
    let mut media_template: Option<String> = None;
    let mut timescale: u64 = 1;
    let mut start_number: u32 = 0;
    let mut timeline_entries: Vec<(u64, u32)> = Vec::new(); // (duration_ticks, repeat_count)
    let mut uniform_duration: Option<u64> = None; // for SegmentTemplate@duration
    let mut base_url_override: Option<String> = None;
    let mut total_duration_secs: Option<f64> = None;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let name = e.local_name();
                match name.as_ref() {
                    b"MPD" => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"type" => {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    is_live = val == "dynamic";
                                }
                                b"mediaPresentationDuration" => {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    total_duration_secs =
                                        parse_iso8601_duration(&val);
                                }
                                _ => {}
                            }
                        }
                    }
                    b"BaseURL" => {
                        // BaseURL content comes in a Text event; we'll read it next
                    }
                    b"SegmentTemplate" => {
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"initialization" => {
                                    init_template = Some(
                                        String::from_utf8_lossy(&attr.value).to_string(),
                                    );
                                }
                                b"media" => {
                                    media_template = Some(
                                        String::from_utf8_lossy(&attr.value).to_string(),
                                    );
                                }
                                b"timescale" => {
                                    timescale = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(1);
                                }
                                b"startNumber" => {
                                    start_number = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(0);
                                }
                                b"duration" => {
                                    uniform_duration = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .ok();
                                }
                                _ => {}
                            }
                        }
                    }
                    b"S" => {
                        let mut d = 0u64;
                        let mut r = 0u32;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"d" => {
                                    d = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(0);
                                }
                                b"r" => {
                                    r = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(0);
                                }
                                _ => {}
                            }
                        }
                        if d > 0 {
                            timeline_entries.push((d, r));
                        }
                    }
                    b"ContentProtection" => {
                        // Detect source encryption scheme from ContentProtection elements.
                        // Look for the MPEG-DASH mp4protection scheme which carries the
                        // encryption scheme in its value attribute.
                        let mut scheme_uri: Option<String> = None;
                        let mut value: Option<String> = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"schemeIdUri" => {
                                    scheme_uri = Some(
                                        String::from_utf8_lossy(&attr.value).to_string(),
                                    );
                                }
                                b"value" => {
                                    value = Some(
                                        String::from_utf8_lossy(&attr.value).to_string(),
                                    );
                                }
                                _ => {}
                            }
                        }
                        // The mp4protection scheme signals which encryption scheme is in use
                        if scheme_uri.as_deref() == Some("urn:mpeg:dash:mp4protection:2011") {
                            if let Some(val) = value {
                                match val.as_str() {
                                    "cenc" => source_scheme = Some(EncryptionScheme::Cenc),
                                    "cbcs" => source_scheme = Some(EncryptionScheme::Cbcs),
                                    _ => {}
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                // Check if we're inside a <BaseURL> element
                // quick-xml delivers text after Start("BaseURL")
                let text = e.unescape().unwrap_or_default().to_string();
                let text = text.trim();
                if !text.is_empty() && base_url_override.is_none() {
                    // Only capture first BaseURL
                    if text.starts_with("http://") || text.starts_with("https://") {
                        base_url_override = Some(text.to_string());
                    }
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(EdgePackagerError::Manifest(format!(
                    "MPD XML parse error: {e}"
                )));
            }
            _ => {}
        }
        buf.clear();
    }

    // Determine the effective base URL
    let effective_base = if let Some(ref override_url) = base_url_override {
        Url::parse(override_url).unwrap_or(base_url.clone())
    } else {
        base_url
    };

    // Build segment list
    let media_pattern = media_template.ok_or_else(|| {
        EdgePackagerError::Manifest("DASH MPD missing SegmentTemplate@media".into())
    })?;

    let mut segment_urls = Vec::new();
    let mut segment_durations = Vec::new();

    if !timeline_entries.is_empty() {
        // SegmentTimeline mode
        let mut number = start_number;
        for (d, r) in &timeline_entries {
            let duration_secs = *d as f64 / timescale as f64;
            // r+1 segments with this duration (r=0 means 1 segment)
            for _ in 0..=*r {
                let url = media_pattern.replace("$Number$", &number.to_string());
                segment_urls.push(resolve_url(&effective_base, &url)?);
                segment_durations.push(duration_secs);
                number += 1;
            }
        }
    } else if let Some(dur) = uniform_duration {
        // Uniform duration mode — need total_duration to compute segment count
        let duration_secs = dur as f64 / timescale as f64;
        if let Some(total_secs) = total_duration_secs {
            let segment_count = (total_secs / duration_secs).ceil() as u32;
            for i in 0..segment_count {
                let number = start_number + i;
                let url = media_pattern.replace("$Number$", &number.to_string());
                segment_urls.push(resolve_url(&effective_base, &url)?);
                // Last segment may be shorter
                let remaining = total_secs - (i as f64 * duration_secs);
                segment_durations.push(remaining.min(duration_secs));
            }
        }
    }

    // Build init segment URL
    let init_url = init_template
        .map(|t| resolve_url(&effective_base, &t))
        .transpose()?
        .ok_or_else(|| {
            EdgePackagerError::Manifest(
                "DASH MPD missing SegmentTemplate@initialization".into(),
            )
        })?;

    Ok(SourceManifest {
        init_segment_url: init_url,
        segment_urls,
        segment_durations,
        is_live,
        source_scheme,
    })
}

/// Resolve a possibly-relative URI against a base URL.
fn resolve_url(base: &Url, relative: &str) -> Result<String> {
    if relative.starts_with("http://") || relative.starts_with("https://") {
        return Ok(relative.to_string());
    }
    base.join(relative)
        .map(|u| u.to_string())
        .map_err(|e| EdgePackagerError::Manifest(format!("resolve URL: {e}")))
}

/// Parse an ISO 8601 duration (e.g., "PT1H30M0S") into seconds.
fn parse_iso8601_duration(s: &str) -> Option<f64> {
    let s = s.strip_prefix("PT")?;
    let mut total = 0.0;
    let mut num_buf = String::new();

    for c in s.chars() {
        match c {
            'H' => {
                total += num_buf.parse::<f64>().unwrap_or(0.0) * 3600.0;
                num_buf.clear();
            }
            'M' => {
                total += num_buf.parse::<f64>().unwrap_or(0.0) * 60.0;
                num_buf.clear();
            }
            'S' => {
                total += num_buf.parse::<f64>().unwrap_or(0.0);
                num_buf.clear();
            }
            _ => num_buf.push(c),
        }
    }

    if total > 0.0 {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_URL: &str = "https://cdn.example.com/content/manifest.mpd";

    fn minimal_static_mpd() -> String {
        r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD type="static" mediaPresentationDuration="PT18.018S">
  <Period>
    <AdaptationSet mimeType="video/mp4">
      <Representation bandwidth="5000000">
        <SegmentTemplate initialization="init.mp4" media="segment_$Number$.cmfv" startNumber="0" timescale="90000">
          <SegmentTimeline>
            <S d="540540" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#.to_string()
    }

    #[test]
    fn parse_minimal_static_mpd() {
        let result = parse_dash_manifest(&minimal_static_mpd(), BASE_URL).unwrap();
        assert!(!result.is_live);
        assert_eq!(
            result.init_segment_url,
            "https://cdn.example.com/content/init.mp4"
        );
        assert_eq!(result.segment_urls.len(), 3); // r=2 means 3 segments
        assert_eq!(result.segment_durations.len(), 3);
    }

    #[test]
    fn parse_segment_durations_from_timeline() {
        let result = parse_dash_manifest(&minimal_static_mpd(), BASE_URL).unwrap();
        // 540540 / 90000 = 6.006 seconds
        for dur in &result.segment_durations {
            assert!((dur - 6.006).abs() < 0.001);
        }
    }

    #[test]
    fn parse_segment_urls_with_number_substitution() {
        let result = parse_dash_manifest(&minimal_static_mpd(), BASE_URL).unwrap();
        assert_eq!(
            result.segment_urls[0],
            "https://cdn.example.com/content/segment_0.cmfv"
        );
        assert_eq!(
            result.segment_urls[1],
            "https://cdn.example.com/content/segment_1.cmfv"
        );
        assert_eq!(
            result.segment_urls[2],
            "https://cdn.example.com/content/segment_2.cmfv"
        );
    }

    #[test]
    fn parse_dynamic_mpd_is_live() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000">
          <SegmentTimeline>
            <S d="6000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert!(result.is_live);
    }

    #[test]
    fn parse_start_number_nonzero() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" startNumber="5" timescale="1000">
          <SegmentTimeline>
            <S d="6000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.segment_urls.len(), 2);
        assert!(result.segment_urls[0].contains("seg_5"));
        assert!(result.segment_urls[1].contains("seg_6"));
    }

    #[test]
    fn parse_multiple_timeline_entries() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT18S">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" startNumber="0">
          <SegmentTimeline>
            <S d="6000" r="1"/>
            <S d="3000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.segment_urls.len(), 4); // 2 + 2
        assert!((result.segment_durations[0] - 6.0).abs() < 0.001);
        assert!((result.segment_durations[1] - 6.0).abs() < 0.001);
        assert!((result.segment_durations[2] - 3.0).abs() < 0.001);
        assert!((result.segment_durations[3] - 3.0).abs() < 0.001);
    }

    #[test]
    fn parse_missing_segment_template_returns_error() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet>
      <Representation bandwidth="1000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("SegmentTemplate@media"));
    }

    #[test]
    fn parse_missing_initialization_returns_error() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate media="seg_$Number$.cmfv" timescale="1000">
          <SegmentTimeline>
            <S d="6000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("initialization"));
    }

    #[test]
    fn parse_invalid_manifest_url() {
        let result = parse_dash_manifest(&minimal_static_mpd(), "not-a-url");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid manifest URL"));
    }

    #[test]
    fn parse_uniform_duration_mode() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12.012S">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" duration="6006" timescale="1000" startNumber="0"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.segment_urls.len(), 2);
        assert!((result.segment_durations[0] - 6.006).abs() < 0.001);
        assert!((result.segment_durations[1] - 6.006).abs() < 0.001);
    }

    #[test]
    fn parse_detects_cenc_from_content_protection() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet>
      <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
      <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000">
          <SegmentTimeline>
            <S d="6000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.source_scheme, Some(EncryptionScheme::Cenc));
    }

    #[test]
    fn parse_detects_cbcs_from_content_protection() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet>
      <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cbcs"/>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000">
          <SegmentTimeline>
            <S d="6000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.source_scheme, Some(EncryptionScheme::Cbcs));
    }

    #[test]
    fn parse_no_content_protection_source_scheme_is_none() {
        let result = parse_dash_manifest(&minimal_static_mpd(), BASE_URL).unwrap();
        assert_eq!(result.source_scheme, None);
    }

    #[test]
    fn parse_ignores_non_mp4protection_content_protection() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet>
      <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed" value="Widevine"/>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000">
          <SegmentTimeline>
            <S d="6000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.source_scheme, None);
    }

    #[test]
    fn parse_iso8601_duration_full() {
        assert!((parse_iso8601_duration("PT1H30M0S").unwrap() - 5400.0).abs() < 0.001);
    }

    #[test]
    fn parse_iso8601_duration_minutes_seconds() {
        assert!((parse_iso8601_duration("PT2M30S").unwrap() - 150.0).abs() < 0.001);
    }

    #[test]
    fn parse_iso8601_duration_seconds_only() {
        assert!((parse_iso8601_duration("PT18.018S").unwrap() - 18.018).abs() < 0.001);
    }

    #[test]
    fn parse_iso8601_duration_invalid() {
        assert!(parse_iso8601_duration("invalid").is_none());
        assert!(parse_iso8601_duration("P1D").is_none()); // days not supported
    }
}
