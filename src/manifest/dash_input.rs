//! DASH MPD source manifest input parser.
//!
//! Parses a DASH MPD to extract init segment URL, media segment URLs,
//! durations, and live/VOD status. This is the *input* side — the output
//! renderer is in `dash.rs`.

use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgepackError, Result};
use crate::manifest::types::{
    AdBreakInfo, ContentSteeringConfig, LowLatencyDashInfo, SegmentBaseSource, SourceManifest,
};
use quick_xml::events::Event;
use quick_xml::Reader;
use crate::url::Url;

/// Parse a DASH MPD manifest into a `SourceManifest`.
///
/// Supports the most common CMAF DASH patterns:
/// - `<SegmentTemplate>` with `<SegmentTimeline>` and `$Number$` substitution
/// - `<SegmentTemplate>` with `duration` attribute (uniform segments)
/// - `<SegmentBase>` with `<BaseURL>` and byte-range indexing (on-demand profile)
///
/// For `SegmentBase` manifests, the returned `SourceManifest` will have a populated
/// `segment_base` field containing sidx resolution metadata. The caller (pipeline)
/// must fetch the sidx box and resolve segment byte ranges before processing.
///
/// The `manifest_url` is used as the base for resolving relative segment URIs.
pub fn parse_dash_manifest(manifest_text: &str, manifest_url: &str) -> Result<SourceManifest> {
    let base_url = Url::parse(manifest_url).map_err(|e| {
        EdgepackError::Manifest(format!("invalid manifest URL: {e}"))
    })?;

    let mut reader = Reader::from_str(manifest_text);
    reader.config_mut().trim_text(true);

    // --- MPD-level state ---
    let mut is_live = false;
    let mut source_scheme: Option<EncryptionScheme> = None;
    let mut init_template: Option<String> = None;
    let mut media_template: Option<String> = None;
    let mut timescale: u64 = 1;
    let mut start_number: u32 = 0;
    let mut timeline_entries: Vec<(u64, u32)> = Vec::new(); // (duration_ticks, repeat_count)
    let mut uniform_duration: Option<u64> = None; // for SegmentTemplate@duration
    let mut base_url_override: Option<String> = None; // absolute BaseURL at MPD/Period level
    let mut found_video_segment_template = false; // whether we've locked in on a video SegmentTemplate
    let mut capturing_current_template = false; // whether current AdaptationSet's template is being captured
    let mut ll_dash_info: Option<LowLatencyDashInfo> = None;
    let mut total_duration_secs: Option<f64> = None;
    let mut dash_content_steering: Option<ContentSteeringConfig> = None;
    let mut ad_breaks: Vec<AdBreakInfo> = Vec::new();
    let mut in_scte35_event_stream = false;
    let mut event_stream_timescale: u64 = 90000;
    let mut pending_event: Option<PendingDashEvent> = None;

    // --- SegmentBase support (on-demand profile) ---
    // We track AdaptationSet and Representation context to collect per-Representation
    // BaseURL + SegmentBase data. We pick the first video representation found.
    let mut in_adaptation_set = false;
    let mut adaptation_set_is_video = false;
    let mut in_representation = false;
    let mut awaiting_base_url_text = false; // true after Start("BaseURL"), cleared after Text
    // Per-Representation accumulator (reset on Representation start)
    let mut current_rep_base_url: Option<String> = None;
    let mut current_seg_base_index_range: Option<(u64, u64)> = None;
    let mut current_seg_base_timescale: u64 = 0;
    let mut current_seg_base_init_range: Option<(u64, u64)> = None;
    let mut in_segment_base = false;
    // Collected SegmentBase representation (first video one wins)
    let mut segment_base_rep: Option<SegmentBaseRepresentation> = None;

    let mut buf = Vec::new();
    loop {
        let event = reader.read_event_into(&mut buf);
        let is_empty = matches!(&event, Ok(Event::Empty(_)));

        match event {
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
                    b"AdaptationSet" => {
                        in_adaptation_set = true;
                        adaptation_set_is_video = false;
                        // Check contentType or mimeType to determine video/audio
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"contentType" => {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    adaptation_set_is_video = val == "video";
                                }
                                b"mimeType" => {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    if val.starts_with("video/") {
                                        adaptation_set_is_video = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    b"Representation" if in_adaptation_set => {
                        in_representation = true;
                        // Reset per-Representation accumulators
                        current_rep_base_url = None;
                        current_seg_base_index_range = None;
                        current_seg_base_timescale = 0;
                        current_seg_base_init_range = None;

                        // Check mimeType on Representation as fallback for video detection
                        if !adaptation_set_is_video {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"mimeType" {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    if val.starts_with("video/") {
                                        adaptation_set_is_video = true;
                                    }
                                }
                            }
                        }
                    }
                    b"BaseURL" => {
                        // BaseURL content comes in a Text event; flag to capture it
                        awaiting_base_url_text = true;
                    }
                    b"SegmentBase" => {
                        in_segment_base = true;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"indexRange" => {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    current_seg_base_index_range = parse_byte_range(&val);
                                }
                                b"timescale" => {
                                    current_seg_base_timescale =
                                        String::from_utf8_lossy(&attr.value)
                                            .parse()
                                            .unwrap_or(0);
                                }
                                _ => {}
                            }
                        }
                        if is_empty {
                            in_segment_base = false;
                        }
                    }
                    b"Initialization" if in_segment_base => {
                        for attr in e.attributes().flatten() {
                            if attr.key.as_ref() == b"range" {
                                let val = String::from_utf8_lossy(&attr.value);
                                current_seg_base_init_range = parse_byte_range(&val);
                            }
                        }
                    }
                    b"SegmentTemplate" => {
                        // Only capture SegmentTemplate data from the first video
                        // AdaptationSet. If no video has been seen yet, capture from
                        // any AdaptationSet as a fallback (will be replaced when a
                        // video AdaptationSet is found later). Once a video template
                        // is locked in, skip all subsequent AdaptationSets.
                        let should_capture = !found_video_segment_template;
                        capturing_current_template = should_capture;
                        if should_capture {
                            // If this is a video AdaptationSet and we previously captured
                            // non-video data, clear it first and lock in on video.
                            if adaptation_set_is_video {
                                timeline_entries.clear();
                                uniform_duration = None;
                                found_video_segment_template = true;
                            }
                            let mut ato: Option<f64> = None;
                            let mut atc: Option<bool> = None;
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
                                    b"availabilityTimeOffset" => {
                                        ato = String::from_utf8_lossy(&attr.value)
                                            .parse::<f64>()
                                            .ok();
                                    }
                                    b"availabilityTimeComplete" => {
                                        let val = String::from_utf8_lossy(&attr.value);
                                        atc = Some(val == "true");
                                    }
                                    _ => {}
                                }
                            }
                            // Detect LL-DASH parameters
                            if let Some(offset) = ato {
                                ll_dash_info = Some(LowLatencyDashInfo {
                                    availability_time_offset: offset,
                                    availability_time_complete: atc.unwrap_or(true),
                                });
                            }
                        }
                    }
                    b"S" => {
                        // Only capture timeline entries from the AdaptationSet
                        // whose SegmentTemplate we are currently capturing.
                        let should_capture = capturing_current_template;
                        if should_capture {
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
                    }
                    b"ContentSteering" => {
                        let mut proxy_url: Option<String> = None;
                        let mut default_sl: Option<String> = None;
                        let mut qbs: Option<bool> = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"proxyServerURL" => {
                                    proxy_url = Some(
                                        String::from_utf8_lossy(&attr.value).to_string(),
                                    );
                                }
                                b"defaultServiceLocation" => {
                                    default_sl = Some(
                                        String::from_utf8_lossy(&attr.value).to_string(),
                                    );
                                }
                                b"queryBeforeStart" => {
                                    let val = String::from_utf8_lossy(&attr.value);
                                    qbs = Some(val == "true");
                                }
                                _ => {}
                            }
                        }
                        if let Some(url) = proxy_url {
                            dash_content_steering = Some(ContentSteeringConfig {
                                server_uri: url,
                                default_pathway_id: default_sl,
                                query_before_start: qbs,
                            });
                        }
                    }
                    b"ContentProtection" => {
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
                    b"EventStream" => {
                        let mut scheme_uri = String::new();
                        let mut ts: u64 = 90000;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"schemeIdUri" => {
                                    scheme_uri = String::from_utf8_lossy(&attr.value).to_string();
                                }
                                b"timescale" => {
                                    ts = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(90000);
                                }
                                _ => {}
                            }
                        }
                        if scheme_uri.starts_with("urn:scte:scte35:") {
                            in_scte35_event_stream = true;
                            event_stream_timescale = ts;
                        }
                    }
                    b"Event" if in_scte35_event_stream => {
                        let mut evt_id: u32 = 0;
                        let mut pt: u64 = 0;
                        let mut dur: Option<u64> = None;
                        for attr in e.attributes().flatten() {
                            match attr.key.as_ref() {
                                b"id" => {
                                    evt_id = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(0);
                                }
                                b"presentationTime" => {
                                    pt = String::from_utf8_lossy(&attr.value)
                                        .parse()
                                        .unwrap_or(0);
                                }
                                b"duration" => {
                                    dur = String::from_utf8_lossy(&attr.value).parse().ok();
                                }
                                _ => {}
                            }
                        }

                        if is_empty {
                            let presentation_time_secs = pt as f64 / event_stream_timescale as f64;
                            let duration_secs = dur.map(|d| d as f64 / event_stream_timescale as f64);
                            ad_breaks.push(AdBreakInfo {
                                id: evt_id,
                                presentation_time: presentation_time_secs,
                                duration: duration_secs,
                                scte35_cmd: None,
                                segment_number: 0,
                            });
                        } else {
                            pending_event = Some(PendingDashEvent {
                                id: evt_id,
                                presentation_time: pt,
                                duration: dur,
                                text_content: String::new(),
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                let text_trimmed = text.trim();

                // Collect text into pending Event element
                if let Some(ref mut pe) = pending_event {
                    if !text_trimmed.is_empty() {
                        pe.text_content.push_str(text_trimmed);
                    }
                }

                // Capture BaseURL text content
                if awaiting_base_url_text && !text_trimmed.is_empty() {
                    if in_representation {
                        // Per-Representation BaseURL (may be relative)
                        current_rep_base_url = Some(text_trimmed.to_string());
                    } else if base_url_override.is_none() {
                        // MPD/Period-level BaseURL (only absolute URLs)
                        if text_trimmed.starts_with("http://") || text_trimmed.starts_with("https://") {
                            base_url_override = Some(text_trimmed.to_string());
                        }
                    }
                    awaiting_base_url_text = false;
                }
            }
            Ok(Event::End(ref e)) => {
                match e.local_name().as_ref() {
                    b"AdaptationSet" => {
                        in_adaptation_set = false;
                        adaptation_set_is_video = false;
                        capturing_current_template = false;
                    }
                    b"Representation" => {
                        // If this Representation had SegmentBase data, collect it
                        // (prefer video; take first one found)
                        // Prefer video Representation for SegmentBase. Take the
                        // first video one, or the first non-video one as fallback
                        // (will be replaced if a video Representation is found later).
                        let take_segment_base = if segment_base_rep.is_none() {
                            true // nothing captured yet — take any
                        } else if adaptation_set_is_video && !segment_base_rep.as_ref().unwrap().is_video {
                            true // upgrade from non-video to video
                        } else {
                            false
                        };
                        if in_representation
                            && take_segment_base
                            && current_seg_base_index_range.is_some()
                            && current_seg_base_init_range.is_some()
                            && current_rep_base_url.is_some()
                        {
                            segment_base_rep = Some(SegmentBaseRepresentation {
                                base_url: current_rep_base_url.take().unwrap(),
                                init_range: current_seg_base_init_range.unwrap(),
                                index_range: current_seg_base_index_range.unwrap(),
                                timescale: current_seg_base_timescale,
                                is_video: adaptation_set_is_video,
                            });
                        }
                        in_representation = false;
                        current_rep_base_url = None;
                        current_seg_base_index_range = None;
                        current_seg_base_timescale = 0;
                        current_seg_base_init_range = None;
                    }
                    b"SegmentBase" => {
                        in_segment_base = false;
                    }
                    b"BaseURL" => {
                        awaiting_base_url_text = false;
                    }
                    b"EventStream" => {
                        in_scte35_event_stream = false;
                    }
                    b"Event" if in_scte35_event_stream => {
                        if let Some(pe) = pending_event.take() {
                            let presentation_time_secs =
                                pe.presentation_time as f64 / event_stream_timescale as f64;
                            let duration_secs = pe.duration
                                .map(|d| d as f64 / event_stream_timescale as f64);
                            let scte35_cmd = if pe.text_content.is_empty() {
                                None
                            } else {
                                Some(pe.text_content)
                            };
                            ad_breaks.push(AdBreakInfo {
                                id: pe.id,
                                presentation_time: presentation_time_secs,
                                duration: duration_secs,
                                scte35_cmd,
                                segment_number: 0,
                            });
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(EdgepackError::Manifest(format!(
                    "MPD XML parse error: {e}"
                )));
            }
            _ => {}
        }
        buf.clear();
    }

    // Determine the effective base URL for resolving relative URIs
    let effective_base = if let Some(ref override_url) = base_url_override {
        Url::parse(override_url).unwrap_or(base_url.clone())
    } else {
        base_url.clone()
    };

    // --- Path A: SegmentTemplate-based manifests ---
    if media_template.is_some() {
        let media_pattern = media_template.unwrap();

        let mut segment_urls = Vec::new();
        let mut segment_durations = Vec::new();

        if !timeline_entries.is_empty() {
            // SegmentTimeline mode
            let mut number = start_number;
            for (d, r) in &timeline_entries {
                let duration_secs = *d as f64 / timescale as f64;
                for _ in 0..=*r {
                    let url = media_pattern.replace("$Number$", &number.to_string());
                    segment_urls.push(resolve_url(&effective_base, &url)?);
                    segment_durations.push(duration_secs);
                    number += 1;
                }
            }
        } else if let Some(dur) = uniform_duration {
            // Uniform duration mode
            let duration_secs = dur as f64 / timescale as f64;
            if let Some(total_secs) = total_duration_secs {
                let segment_count = (total_secs / duration_secs).ceil() as u32;
                for i in 0..segment_count {
                    let number = start_number + i;
                    let url = media_pattern.replace("$Number$", &number.to_string());
                    segment_urls.push(resolve_url(&effective_base, &url)?);
                    let remaining = total_secs - (i as f64 * duration_secs);
                    segment_durations.push(remaining.min(duration_secs));
                }
            }
        }

        let init_url = init_template
            .map(|t| resolve_url(&effective_base, &t))
            .transpose()?
            .ok_or_else(|| {
                EdgepackError::Manifest(
                    "DASH MPD missing SegmentTemplate@initialization".into(),
                )
            })?;

        return Ok(SourceManifest {
            init_segment_url: init_url,
            segment_urls,
            segment_durations,
            is_live,
            source_scheme,
            ad_breaks,
            parts: Vec::new(),
            part_target_duration: None,
            server_control: None,
            ll_dash_info,
            is_ts_source: false,
            aes128_key_url: None,
            aes128_iv: None,
            content_steering: dash_content_steering,
            init_byte_range: None,
            segment_byte_ranges: Vec::new(),
            segment_base: None,
        });
    }

    // --- Path B: SegmentBase-based manifests (on-demand profile) ---
    if let Some(rep) = segment_base_rep {
        // Resolve the representation's BaseURL against the manifest base
        let file_url = resolve_url(&effective_base, &rep.base_url)?;

        return Ok(SourceManifest {
            // The init URL is the same file — the pipeline will use byte range to fetch
            init_segment_url: file_url.clone(),
            // Segments will be populated after sidx resolution by the pipeline
            segment_urls: Vec::new(),
            segment_durations: Vec::new(),
            is_live,
            source_scheme,
            ad_breaks,
            parts: Vec::new(),
            part_target_duration: None,
            server_control: None,
            ll_dash_info,
            is_ts_source: false,
            aes128_key_url: None,
            aes128_iv: None,
            content_steering: dash_content_steering,
            init_byte_range: Some(rep.init_range),
            segment_byte_ranges: Vec::new(),
            segment_base: Some(SegmentBaseSource {
                file_url,
                init_range: rep.init_range,
                index_range: rep.index_range,
                timescale: rep.timescale,
            }),
        });
    }

    // --- Neither SegmentTemplate nor SegmentBase found ---
    Err(EdgepackError::Manifest(
        "DASH MPD missing both SegmentTemplate and SegmentBase — unsupported manifest format".into(),
    ))
}

/// Per-Representation SegmentBase data collected during parsing.
struct SegmentBaseRepresentation {
    /// BaseURL text content (may be relative to manifest URL).
    base_url: String,
    /// Initialization byte range (start, end inclusive).
    init_range: (u64, u64),
    /// Sidx (Segment Index) byte range (start, end inclusive).
    index_range: (u64, u64),
    /// Timescale from `<SegmentBase>` for duration conversion.
    timescale: u64,
    /// Whether this representation is in a video AdaptationSet.
    #[allow(dead_code)]
    is_video: bool,
}

/// Temporary state for collecting a DASH `<Event>` element's attributes and text content.
struct PendingDashEvent {
    id: u32,
    presentation_time: u64,
    duration: Option<u64>,
    text_content: String,
}

/// Parse a byte range string like "0-822" into (start, end) tuple.
fn parse_byte_range(s: &str) -> Option<(u64, u64)> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() == 2 {
        let start: u64 = parts[0].parse().ok()?;
        let end: u64 = parts[1].parse().ok()?;
        Some((start, end))
    } else {
        None
    }
}

/// Resolve a possibly-relative URI against a base URL.
fn resolve_url(base: &Url, relative: &str) -> Result<String> {
    if relative.starts_with("http://") || relative.starts_with("https://") {
        return Ok(relative.to_string());
    }
    base.join(relative)
        .map(|u| u.to_string())
        .map_err(|e| EdgepackError::Manifest(format!("resolve URL: {e}")))
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
    fn parse_missing_segment_template_and_base_returns_error() {
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
        assert!(err.contains("SegmentTemplate"));
        assert!(err.contains("SegmentBase"));
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
    fn parse_event_stream_with_scte35() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT18S">
  <Period>
    <EventStream schemeIdUri="urn:scte:scte35:2013:bin" timescale="90000">
      <Event presentationTime="540540" duration="2700000" id="42">AQIDBA==</Event>
    </EventStream>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" startNumber="0">
          <SegmentTimeline>
            <S d="6000" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.ad_breaks.len(), 1);
        let ab = &result.ad_breaks[0];
        assert_eq!(ab.id, 42);
        // 540540 / 90000 = 6.006
        assert!((ab.presentation_time - 6.006).abs() < 0.001);
        // 2700000 / 90000 = 30.0
        assert!((ab.duration.unwrap() - 30.0).abs() < 0.001);
        assert_eq!(ab.scte35_cmd.as_deref(), Some("AQIDBA=="));
    }

    #[test]
    fn parse_event_stream_empty_event() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <EventStream schemeIdUri="urn:scte:scte35:2013:bin" timescale="90000">
      <Event presentationTime="0" duration="900000" id="1"/>
    </EventStream>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" startNumber="0">
          <SegmentTimeline>
            <S d="6000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert_eq!(result.ad_breaks.len(), 1);
        let ab = &result.ad_breaks[0];
        assert_eq!(ab.id, 1);
        assert!((ab.presentation_time - 0.0).abs() < 0.001);
        assert!((ab.duration.unwrap() - 10.0).abs() < 0.001);
        assert!(ab.scte35_cmd.is_none());
    }

    #[test]
    fn parse_no_event_stream_empty_ad_breaks() {
        let result = parse_dash_manifest(&minimal_static_mpd(), BASE_URL).unwrap();
        assert!(result.ad_breaks.is_empty());
    }

    #[test]
    fn parse_non_scte35_event_stream_ignored() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <EventStream schemeIdUri="urn:mpeg:dash:event:2012" timescale="1000">
      <Event presentationTime="0" duration="1000" id="1">some data</Event>
    </EventStream>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" startNumber="0">
          <SegmentTimeline>
            <S d="6000" r="1"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        assert!(result.ad_breaks.is_empty());
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

    // --- LL-DASH parsing tests ---

    #[test]
    fn parse_availability_time_offset() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" availabilityTimeOffset="5.0" availabilityTimeComplete="false">
          <SegmentTimeline>
            <S d="6000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        let ll = result.ll_dash_info.unwrap();
        assert!((ll.availability_time_offset - 5.0).abs() < 0.001);
        assert!(!ll.availability_time_complete);
    }

    #[test]
    fn parse_availability_time_complete_true() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="dynamic">
  <Period>
    <AdaptationSet>
      <Representation>
        <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" availabilityTimeOffset="2.5" availabilityTimeComplete="true">
          <SegmentTimeline>
            <S d="6000"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        let ll = result.ll_dash_info.unwrap();
        assert!((ll.availability_time_offset - 2.5).abs() < 0.001);
        assert!(ll.availability_time_complete);
    }

    #[test]
    fn parse_no_ll_dash_attributes_backward_compat() {
        let result = parse_dash_manifest(&minimal_static_mpd(), BASE_URL).unwrap();
        assert!(result.ll_dash_info.is_none());
    }

    // --- Content steering tests ---

    #[test]
    fn parse_content_steering_full() {
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
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        let cs = result.content_steering.unwrap();
        assert_eq!(cs.server_uri, "https://steer.example.com/v1");
        assert_eq!(cs.default_pathway_id.as_deref(), Some("cdn-a"));
        assert_eq!(cs.query_before_start, Some(true));
    }

    #[test]
    fn parse_content_steering_minimal() {
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
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        let cs = result.content_steering.unwrap();
        assert_eq!(cs.server_uri, "https://steer.example.com/v1");
        assert!(cs.default_pathway_id.is_none());
        assert!(cs.query_before_start.is_none());
    }

    #[test]
    fn parse_no_content_steering_backward_compat() {
        let result = parse_dash_manifest(&minimal_static_mpd(), BASE_URL).unwrap();
        assert!(result.content_steering.is_none());
    }

    // --- SegmentBase (on-demand profile) tests ---

    #[test]
    fn parse_segment_base_basic() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT10M34.533S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="0" bandwidth="120158" codecs="avc1.42c01e" mimeType="video/mp4" width="256" height="144">
        <BaseURL>v-0144p-0100k-libx264.mp4</BaseURL>
        <SegmentBase indexRange="823-1982" timescale="15360">
          <Initialization range="0-822"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();

        // Should have SegmentBase info for sidx resolution
        assert!(result.segment_base.is_some());
        let sb = result.segment_base.unwrap();
        assert_eq!(sb.file_url, "https://cdn.example.com/content/v-0144p-0100k-libx264.mp4");
        assert_eq!(sb.init_range, (0, 822));
        assert_eq!(sb.index_range, (823, 1982));
        assert_eq!(sb.timescale, 15360);

        // Init byte range should be set
        assert_eq!(result.init_byte_range, Some((0, 822)));

        // Init URL points to the same file
        assert_eq!(result.init_segment_url, sb.file_url);

        // Segments are empty — need sidx resolution
        assert!(result.segment_urls.is_empty());
        assert!(result.segment_durations.is_empty());
    }

    #[test]
    fn parse_segment_base_with_absolute_base_url() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT60S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="1" bandwidth="500000">
        <BaseURL>https://other-cdn.example.com/video.mp4</BaseURL>
        <SegmentBase indexRange="1000-2000" timescale="90000">
          <Initialization range="0-999"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        let sb = result.segment_base.unwrap();
        assert_eq!(sb.file_url, "https://other-cdn.example.com/video.mp4");
        assert_eq!(sb.init_range, (0, 999));
        assert_eq!(sb.index_range, (1000, 2000));
    }

    #[test]
    fn parse_segment_base_multiple_representations_picks_first() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT60S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="0" bandwidth="100000">
        <BaseURL>low.mp4</BaseURL>
        <SegmentBase indexRange="100-200" timescale="10000">
          <Initialization range="0-99"/>
        </SegmentBase>
      </Representation>
      <Representation id="1" bandwidth="500000">
        <BaseURL>high.mp4</BaseURL>
        <SegmentBase indexRange="500-1000" timescale="10000">
          <Initialization range="0-499"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        let sb = result.segment_base.unwrap();
        // Should pick the first representation
        assert!(sb.file_url.contains("low.mp4"));
        assert_eq!(sb.index_range, (100, 200));
    }

    #[test]
    fn parse_segment_base_backward_compat_fields() {
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT60S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="0" bandwidth="100000">
        <BaseURL>video.mp4</BaseURL>
        <SegmentBase indexRange="100-200" timescale="10000">
          <Initialization range="0-99"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Standard fields should have sensible defaults
        assert!(!result.is_live);
        assert!(result.source_scheme.is_none());
        assert!(result.ad_breaks.is_empty());
        assert!(!result.is_ts_source);
    }

    #[test]
    fn parse_segment_template_preferred_over_segment_base() {
        // If a manifest has both SegmentTemplate and SegmentBase,
        // SegmentTemplate should win (it's the more common pattern)
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate initialization="init.mp4" media="seg_$Number$.cmfv" timescale="1000" startNumber="0">
        <SegmentTimeline>
          <S d="6000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="0" bandwidth="100000">
        <BaseURL>video.mp4</BaseURL>
        <SegmentBase indexRange="100-200" timescale="10000">
          <Initialization range="0-99"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Should use SegmentTemplate path, not SegmentBase
        assert!(result.segment_base.is_none());
        assert_eq!(result.segment_urls.len(), 2);
    }

    #[test]
    fn parse_byte_range_valid() {
        assert_eq!(parse_byte_range("0-822"), Some((0, 822)));
        assert_eq!(parse_byte_range("823-1982"), Some((823, 1982)));
        assert_eq!(parse_byte_range("0-0"), Some((0, 0)));
    }

    #[test]
    fn parse_byte_range_invalid() {
        assert_eq!(parse_byte_range("invalid"), None);
        assert_eq!(parse_byte_range(""), None);
        assert_eq!(parse_byte_range("0"), None);
        assert_eq!(parse_byte_range("0-abc"), None);
    }

    // ──────────────────────────────────────────────────────────────────
    // Video-preference tests: ensure the parser selects the video
    // AdaptationSet's SegmentTemplate data, not audio, regardless of
    // element ordering in the MPD.
    // ──────────────────────────────────────────────────────────────────

    #[test]
    fn parse_video_first_audio_second_selects_video_template() {
        // Video AdaptationSet appears before audio — should use video templates
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate initialization="video_init.mp4" media="video_$Number$.cmfv" timescale="1000" startNumber="0">
        <SegmentTimeline>
          <S d="6000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v0" bandwidth="2000000"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate initialization="audio_init.mp4" media="audio_$Number$.m4a" timescale="48000" startNumber="0">
        <SegmentTimeline>
          <S d="240000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="a0" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Should use video init and media templates
        assert!(result.init_segment_url.contains("video_init"));
        assert!(result.segment_urls[0].contains("video_0"));
        assert_eq!(result.segment_urls.len(), 2);
        // Duration should be from video timeline: 6000/1000 = 6.0s
        assert!((result.segment_durations[0] - 6.0).abs() < 0.001);
    }

    #[test]
    fn parse_audio_first_video_second_selects_video_template() {
        // Audio AdaptationSet appears BEFORE video — should still use video templates
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate initialization="audio_init.mp4" media="audio_$Number$.m4a" timescale="48000" startNumber="0">
        <SegmentTimeline>
          <S d="240000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="a0" bandwidth="128000"/>
    </AdaptationSet>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate initialization="video_init.mp4" media="video_$Number$.cmfv" timescale="1000" startNumber="0">
        <SegmentTimeline>
          <S d="6000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v0" bandwidth="2000000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Even though audio appears first, should use video init and media templates
        assert!(result.init_segment_url.contains("video_init"));
        assert!(result.segment_urls[0].contains("video_0"));
        assert_eq!(result.segment_urls.len(), 2);
        // Duration should be from video timeline: 6000/1000 = 6.0s (not 240000/48000 = 5.0s)
        assert!((result.segment_durations[0] - 6.0).abs() < 0.001);
    }

    #[test]
    fn parse_no_content_type_fallback_to_mime_type() {
        // AdaptationSet without contentType, only mimeType on Representation
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT18S">
  <Period>
    <AdaptationSet segmentAlignment="true">
      <Representation mimeType="audio/mp4" bandwidth="128000">
        <SegmentTemplate initialization="audio_init.mp4" media="audio_$Number$.m4a" timescale="48000" startNumber="0">
          <SegmentTimeline>
            <S d="240000" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
    <AdaptationSet segmentAlignment="true">
      <Representation mimeType="video/mp4" bandwidth="2000000">
        <SegmentTemplate initialization="video_init.mp4" media="video_$Number$.cmfv" timescale="1000" startNumber="0">
          <SegmentTimeline>
            <S d="6000" r="2"/>
          </SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Should prefer video by detecting mimeType="video/mp4" on Representation
        assert!(result.init_segment_url.contains("video_init"));
        assert!(result.segment_urls[0].contains("video_0"));
    }

    #[test]
    fn parse_audio_only_mpd_uses_audio_template() {
        // When there's only audio (no video), should use the audio template
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT10S">
  <Period>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate initialization="audio_init.mp4" media="audio_$Number$.m4a" timescale="48000" startNumber="0">
        <SegmentTimeline>
          <S d="240000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="a0" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Should use audio template since it's the only one available
        assert!(result.init_segment_url.contains("audio_init"));
        assert!(result.segment_urls[0].contains("audio_0"));
        assert_eq!(result.segment_urls.len(), 2);
    }

    #[test]
    fn parse_segment_base_prefers_video_over_audio() {
        // SegmentBase with audio first, then video — should prefer video
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT60S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <Representation id="a0" bandwidth="128000" codecs="mp4a.40.2">
        <BaseURL>audio.mp4</BaseURL>
        <SegmentBase indexRange="822-1981" timescale="48000">
          <Initialization range="0-821"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v0" bandwidth="2000000" codecs="avc1.64001f">
        <BaseURL>video.mp4</BaseURL>
        <SegmentBase indexRange="823-1982" timescale="15360">
          <Initialization range="0-822"/>
        </SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Should have SegmentBase data from the video Representation
        assert!(result.segment_base.is_some());
        let sb = result.segment_base.as_ref().unwrap();
        assert!(sb.file_url.contains("video.mp4"), "Expected video.mp4 but got {}", sb.file_url);
        assert_eq!(sb.timescale, 15360);
    }

    #[test]
    fn parse_three_adaptation_sets_video_audio_webm_selects_mp4_video() {
        // Multiple AdaptationSets: video/mp4, audio/mp4, video/webm
        // Should select the video/mp4 SegmentTemplate
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate initialization="mp4_video_init.mp4" media="mp4_video_$Number$.cmfv" timescale="1000" startNumber="0">
        <SegmentTimeline>
          <S d="6000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v0" bandwidth="2000000"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <SegmentTemplate initialization="mp4_audio_init.mp4" media="mp4_audio_$Number$.m4a" timescale="48000" startNumber="0">
        <SegmentTimeline>
          <S d="240000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="a0" bandwidth="128000"/>
    </AdaptationSet>
    <AdaptationSet contentType="video" mimeType="video/webm">
      <SegmentTemplate initialization="webm_video_init.webm" media="webm_video_$Number$.webm" timescale="1000" startNumber="0">
        <SegmentTimeline>
          <S d="6000" r="1"/>
        </SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v1" bandwidth="1500000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Should use the first video/mp4 template (not audio or webm)
        assert!(result.init_segment_url.contains("mp4_video_init"));
        assert!(result.segment_urls[0].contains("mp4_video_0"));
    }

    #[test]
    fn parse_uniform_duration_audio_then_video_selects_video() {
        // Uniform duration mode (no SegmentTimeline) with audio first
        let mpd = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT12S">
  <Period>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <Representation>
        <SegmentTemplate initialization="audio_init.mp4" media="audio_$Number$.m4a" duration="240000" timescale="48000" startNumber="0"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation>
        <SegmentTemplate initialization="video_init.mp4" media="video_$Number$.cmfv" duration="6000" timescale="1000" startNumber="0"/>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let result = parse_dash_manifest(mpd, BASE_URL).unwrap();
        // Should use video init and media templates
        assert!(result.init_segment_url.contains("video_init"));
        assert!(result.segment_urls[0].contains("video_0"));
        // Duration should be from video: 6000/1000 = 6.0s
        assert!((result.segment_durations[0] - 6.0).abs() < 0.001);
    }
}
