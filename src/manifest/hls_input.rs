//! HLS M3U8 source manifest input parser.
//!
//! Parses an HLS media playlist to extract init segment URL, media segment URLs,
//! durations, and live/VOD status. This is the *input* side — the output renderers
//! are in `hls.rs`.

use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgepackError, Result};
use crate::manifest::types::{AdBreakInfo, HlsRenditionInfo, ServerControl, SourceManifest, SourcePartInfo, SourceVariantInfo};
use crate::url::Url;

/// Parse an HLS M3U8 media playlist into a `SourceManifest`.
///
/// The `manifest_url` is used as the base for resolving relative segment URIs.
///
/// # Errors
///
/// Returns an error if:
/// - The manifest URL is invalid
/// - The manifest is a master playlist (contains `#EXT-X-STREAM-INF`)
/// - No `#EXT-X-MAP` (init segment) is found
pub fn parse_hls_manifest(manifest_text: &str, manifest_url: &str) -> Result<SourceManifest> {
    let base_url = Url::parse(manifest_url).map_err(|e| {
        EdgepackError::Manifest(format!("invalid manifest URL: {e}"))
    })?;

    // Detect master playlist
    if manifest_text.contains("#EXT-X-STREAM-INF") {
        return Err(EdgepackError::Manifest(
            "received HLS master playlist — expected a media playlist URL. \
             Select a specific variant/rendition playlist URL instead."
                .into(),
        ));
    }

    let mut init_segment_url: Option<String> = None;
    let mut segment_urls = Vec::new();
    let mut segment_durations = Vec::new();
    let mut is_live = true; // live unless #EXT-X-ENDLIST is found
    let mut pending_duration: Option<f64> = None;
    let mut source_scheme: Option<EncryptionScheme> = None;
    let mut ad_breaks = Vec::new();
    let mut segment_number: u32 = 0;
    let mut elapsed_time: f64 = 0.0;
    let mut is_ts_source = false;
    let mut aes128_key_url: Option<String> = None;
    let mut aes128_iv: Option<[u8; 16]> = None;

    // LL-HLS state
    let mut parts: Vec<SourcePartInfo> = Vec::new();
    let mut part_target_duration: Option<f64> = None;
    let mut server_control: Option<ServerControl> = None;
    let mut part_index_in_segment: u32 = 0;

    for line in manifest_text.lines() {
        let line = line.trim();

        if line.starts_with("#EXT-X-MAP:") {
            if let Some(uri) = extract_attribute(line, "URI") {
                init_segment_url = Some(resolve_url(&base_url, &uri)?);
            }
        } else if line.starts_with("#EXT-X-KEY:") {
            // Parse DRM signaling to detect source encryption scheme
            if let Some(method) = extract_attribute_unquoted(line, "METHOD") {
                match method.as_str() {
                    "SAMPLE-AES-CTR" => source_scheme = Some(EncryptionScheme::Cenc),
                    "SAMPLE-AES" => source_scheme = Some(EncryptionScheme::Cbcs),
                    "AES-128" => {
                        // AES-128 segment-level encryption (used with TS segments)
                        if let Some(uri) = extract_attribute(line, "URI") {
                            aes128_key_url = Some(resolve_url(&base_url, &uri)?);
                        }
                        if let Some(iv_hex) = extract_attribute_unquoted(line, "IV") {
                            let iv_hex = iv_hex.strip_prefix("0x")
                                .or_else(|| iv_hex.strip_prefix("0X"))
                                .unwrap_or(&iv_hex);
                            if let Some(bytes) = hex_decode(iv_hex) {
                                if bytes.len() == 16 {
                                    let mut iv = [0u8; 16];
                                    iv.copy_from_slice(&bytes);
                                    aes128_iv = Some(iv);
                                }
                            }
                        }
                    }
                    _ => {} // NONE, etc.
                }
            }
        } else if line.starts_with("#EXT-X-DATERANGE:") {
            // Parse SCTE-35 ad marker signaling
            if let Some(ab) = parse_daterange_ad_break(line, segment_number, elapsed_time) {
                ad_breaks.push(ab);
            }
        } else if line.starts_with("#EXT-X-PART-INF:") {
            // LL-HLS: parse part target duration
            if let Some(val) = extract_attribute_unquoted(line, "PART-TARGET") {
                part_target_duration = val.parse::<f64>().ok();
            }
        } else if line.starts_with("#EXT-X-PART:") {
            // LL-HLS: parse partial segment
            if let Some(part) = parse_ext_x_part(line, &base_url, segment_number, part_index_in_segment) {
                parts.push(part);
                part_index_in_segment += 1;
            }
        } else if line.starts_with("#EXT-X-SERVER-CONTROL:") {
            // LL-HLS: parse server control parameters
            server_control = Some(parse_server_control(line));
        } else if line.starts_with("#EXT-X-PRELOAD-HINT:") {
            // LL-HLS: acknowledged but not stored (informational only)
        } else if line.starts_with("#EXTINF:") {
            let duration_str = line.strip_prefix("#EXTINF:").unwrap_or("");
            let duration_str = duration_str.split(',').next().unwrap_or("0");
            pending_duration = duration_str.parse::<f64>().ok();
        } else if line.starts_with("#EXT-X-ENDLIST") {
            is_live = false;
        } else if !line.starts_with('#') && !line.is_empty() {
            // URI line — associate with the pending EXTINF duration
            if pending_duration.is_some() {
                let duration = pending_duration.take().unwrap_or(6.0);
                let resolved = resolve_url(&base_url, line)?;
                // Detect TS segment extension
                if !is_ts_source && (line.ends_with(".ts") || line.contains(".ts?")) {
                    is_ts_source = true;
                }
                segment_urls.push(resolved);
                segment_durations.push(duration);
                elapsed_time += duration;
                segment_number += 1;
                // Reset part index for next segment
                part_index_in_segment = 0;
            }
        }
    }

    // For TS sources, init segment is not required (synthesized from first segment)
    let init_url = if is_ts_source {
        init_segment_url.unwrap_or_default()
    } else {
        init_segment_url.ok_or_else(|| {
            EdgepackError::Manifest(
                "HLS manifest missing #EXT-X-MAP (init segment)".into(),
            )
        })?
    };

    Ok(SourceManifest {
        init_segment_url: init_url,
        segment_urls,
        segment_durations,
        is_live,
        source_scheme,
        ad_breaks,
        parts,
        part_target_duration,
        server_control,
        ll_dash_info: None,
        is_ts_source,
        aes128_key_url,
        aes128_iv,
        content_steering: None,
        init_byte_range: None,
        segment_byte_ranges: Vec::new(),
        segment_base: None,
        source_variants: Vec::new(),
    })
}

/// Parsed HLS master playlist variant and rendition metadata.
///
/// Contains all variant streams and rendition groups extracted from an HLS master
/// playlist. Used by the sandbox and CDN handler to discover available quality
/// levels, audio tracks, and subtitle tracks for multi-variant processing.
#[derive(Debug, Clone)]
pub struct HlsMasterPlaylistInfo {
    /// Video variant metadata (bandwidth, resolution, codecs, frame rate).
    pub variants: Vec<SourceVariantInfo>,
    /// URIs of variant media playlists (parallel to `variants`).
    pub variant_uris: Vec<String>,
    /// Audio rendition group members.
    pub audio_renditions: Vec<HlsRenditionInfo>,
    /// Subtitle rendition group members.
    pub subtitle_renditions: Vec<HlsRenditionInfo>,
}

/// Parse an HLS master playlist to extract variant and rendition metadata.
///
/// This is separate from `parse_hls_manifest()` which handles media playlists.
/// The master parser extracts metadata only — it does not parse segment data.
///
/// # Errors
///
/// Returns an error if:
/// - The manifest URL is invalid
/// - The manifest is not a master playlist (no `#EXT-X-STREAM-INF`)
pub fn parse_hls_master_playlist(manifest_text: &str, manifest_url: &str) -> Result<HlsMasterPlaylistInfo> {
    let base_url = Url::parse(manifest_url).map_err(|e| {
        EdgepackError::Manifest(format!("invalid manifest URL: {e}"))
    })?;

    if !manifest_text.contains("#EXT-X-STREAM-INF") {
        return Err(EdgepackError::Manifest(
            "not an HLS master playlist — no #EXT-X-STREAM-INF found".into(),
        ));
    }

    let mut variants = Vec::new();
    let mut variant_uris = Vec::new();
    let mut audio_renditions = Vec::new();
    let mut subtitle_renditions = Vec::new();
    let mut pending_stream_inf: Option<SourceVariantInfo> = None;

    for line in manifest_text.lines() {
        let line = line.trim();

        if line.starts_with("#EXT-X-STREAM-INF:") {
            let attrs = &line["#EXT-X-STREAM-INF:".len()..];

            let bandwidth = extract_attribute_unquoted_from(attrs, "BANDWIDTH")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(0);

            let (width, height) = extract_attribute_unquoted_from(attrs, "RESOLUTION")
                .and_then(|v| {
                    let parts: Vec<&str> = v.split('x').collect();
                    if parts.len() == 2 {
                        Some((parts[0].parse::<u32>().ok()?, parts[1].parse::<u32>().ok()?))
                    } else {
                        None
                    }
                })
                .map(|(w, h)| (Some(w), Some(h)))
                .unwrap_or((None, None));

            let codecs = extract_attribute(line, "CODECS");

            let frame_rate = extract_attribute_unquoted_from(attrs, "FRAME-RATE")
                .map(|v| v.to_string());

            pending_stream_inf = Some(SourceVariantInfo {
                bandwidth,
                width,
                height,
                codecs,
                frame_rate,
            });
        } else if line.starts_with("#EXT-X-MEDIA:") {
            // Parse rendition groups
            let media_type = extract_attribute_unquoted(line, "TYPE");
            let uri = extract_attribute(line, "URI");
            let name = extract_attribute(line, "NAME").unwrap_or_default();
            let language = extract_attribute(line, "LANGUAGE");
            let group_id = extract_attribute(line, "GROUP-ID").unwrap_or_default();
            let is_default = extract_attribute_unquoted(line, "DEFAULT")
                .map(|v| v == "YES")
                .unwrap_or(false);

            // Resolve URI if present
            let resolved_uri = if let Some(ref u) = uri {
                Some(resolve_url(&base_url, u)?)
            } else {
                None
            };

            let rendition = HlsRenditionInfo {
                uri: resolved_uri,
                name,
                language,
                group_id,
                is_default,
            };

            match media_type.as_deref() {
                Some("AUDIO") => audio_renditions.push(rendition),
                Some("SUBTITLES") => subtitle_renditions.push(rendition),
                // CLOSED-CAPTIONS have no URI — still captured for manifest rendering
                Some("CLOSED-CAPTIONS") => subtitle_renditions.push(rendition),
                _ => {}
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            // URI line following #EXT-X-STREAM-INF
            if let Some(variant_info) = pending_stream_inf.take() {
                let resolved = resolve_url(&base_url, line)?;
                variants.push(variant_info);
                variant_uris.push(resolved);
            }
        }
    }

    Ok(HlsMasterPlaylistInfo {
        variants,
        variant_uris,
        audio_renditions,
        subtitle_renditions,
    })
}

/// Extract an unquoted attribute from a substring (without the tag prefix).
fn extract_attribute_unquoted_from(attrs: &str, attr: &str) -> Option<String> {
    let search = format!("{attr}=");
    let start = attrs.find(&search)? + search.len();
    let rest = &attrs[start..];
    if rest.starts_with('"') {
        let end = rest[1..].find('"')?;
        return Some(rest[1..1 + end].to_string());
    }
    let end = rest.find(',').unwrap_or(rest.len());
    Some(rest[..end].to_string())
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

/// Extract a quoted attribute value from an HLS tag line.
/// e.g., extract_attribute(`#EXT-X-MAP:URI="init.mp4"`, "URI") returns Some("init.mp4")
fn extract_attribute(line: &str, attr: &str) -> Option<String> {
    let search = format!("{attr}=\"");
    let start = line.find(&search)? + search.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Parse an `#EXT-X-DATERANGE` tag into an `AdBreakInfo` if it contains SCTE-35 signaling.
///
/// Looks for `SCTE35-CMD` (hex-encoded) or `SCTE35-OUT=YES` attributes.
/// Uses the `ID` attribute (expected format `splice-{id}`) to extract the splice event ID.
fn parse_daterange_ad_break(
    line: &str,
    segment_number: u32,
    elapsed_time: f64,
) -> Option<AdBreakInfo> {
    // Require SCTE35-CMD or SCTE35-OUT to identify this as an ad break
    let scte35_cmd = extract_attribute_unquoted(line, "SCTE35-CMD");
    let scte35_out = extract_attribute_unquoted(line, "SCTE35-OUT");

    if scte35_cmd.is_none() && scte35_out.is_none() {
        return None;
    }

    // Extract ID — expected format "splice-{id}" or any string
    let id = extract_attribute(line, "ID").unwrap_or_default();
    let splice_id: u32 = id
        .strip_prefix("splice-")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    // Extract duration from PLANNED-DURATION
    let duration = extract_attribute_unquoted(line, "PLANNED-DURATION")
        .and_then(|s| s.parse::<f64>().ok());

    // Convert SCTE35-CMD from hex (0x...) to base64
    let scte35_base64 = scte35_cmd.and_then(|hex| {
        let hex = hex.strip_prefix("0x").or_else(|| hex.strip_prefix("0X")).unwrap_or(&hex);
        hex_decode(hex).map(|bytes| {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&bytes)
        })
    });

    Some(AdBreakInfo {
        id: splice_id,
        presentation_time: elapsed_time,
        duration,
        scte35_cmd: scte35_base64,
        segment_number,
    })
}

/// Decode a hex string into bytes. Returns None if the string has odd length or invalid chars.
fn hex_decode(hex: &str) -> Option<Vec<u8>> {
    if hex.len() % 2 != 0 {
        return None;
    }
    let mut bytes = Vec::with_capacity(hex.len() / 2);
    for i in (0..hex.len()).step_by(2) {
        let byte = u8::from_str_radix(&hex[i..i + 2], 16).ok()?;
        bytes.push(byte);
    }
    Some(bytes)
}

/// Parse an `#EXT-X-PART` tag into a `SourcePartInfo`.
fn parse_ext_x_part(
    line: &str,
    base_url: &Url,
    segment_number: u32,
    part_index: u32,
) -> Option<SourcePartInfo> {
    let duration = extract_attribute_unquoted(line, "DURATION")?
        .parse::<f64>()
        .ok()?;
    let uri_raw = extract_attribute(line, "URI")?;
    let uri = resolve_url(base_url, &uri_raw).ok()?;
    let independent = extract_attribute_unquoted(line, "INDEPENDENT")
        .map(|v| v == "YES")
        .unwrap_or(false);

    Some(SourcePartInfo {
        segment_number,
        part_index,
        duration,
        independent,
        uri,
    })
}

/// Parse an `#EXT-X-SERVER-CONTROL` tag into a `ServerControl`.
fn parse_server_control(line: &str) -> ServerControl {
    let can_skip_until = extract_attribute_unquoted(line, "CAN-SKIP-UNTIL")
        .and_then(|v| v.parse::<f64>().ok());
    // HOLD-BACK must not match PART-HOLD-BACK.
    // Use extract_exact_attribute which checks for a proper boundary.
    let hold_back = extract_exact_attribute(line, "HOLD-BACK")
        .and_then(|v| v.parse::<f64>().ok());
    let part_hold_back = extract_attribute_unquoted(line, "PART-HOLD-BACK")
        .and_then(|v| v.parse::<f64>().ok());
    let can_block_reload = extract_attribute_unquoted(line, "CAN-BLOCK-RELOAD")
        .map(|v| v == "YES")
        .unwrap_or(false);

    ServerControl {
        can_skip_until,
        hold_back,
        part_hold_back,
        can_block_reload,
    }
}

/// Extract an attribute value ensuring the match is at a proper boundary
/// (preceded by comma, colon, or start of string, not by another letter/hyphen).
/// This avoids "HOLD-BACK" matching inside "PART-HOLD-BACK".
fn extract_exact_attribute(line: &str, attr: &str) -> Option<String> {
    let search = format!("{attr}=");
    let mut search_start = 0;
    while let Some(pos) = line[search_start..].find(&search) {
        let abs_pos = search_start + pos;
        // Check that the character before is a valid boundary (comma, colon, or start)
        if abs_pos == 0 || matches!(line.as_bytes()[abs_pos - 1], b',' | b':' | b' ') {
            let value_start = abs_pos + search.len();
            let rest = &line[value_start..];
            if rest.starts_with('"') {
                let end = rest[1..].find('"')?;
                return Some(rest[1..1 + end].to_string());
            }
            let end = rest.find(',').unwrap_or(rest.len());
            return Some(rest[..end].to_string());
        }
        search_start = abs_pos + 1;
    }
    None
}

/// Extract an unquoted attribute value from an HLS tag line.
/// e.g., extract_attribute_unquoted(`#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI="..."`, "METHOD")
/// returns Some("SAMPLE-AES-CTR")
fn extract_attribute_unquoted(line: &str, attr: &str) -> Option<String> {
    let search = format!("{attr}=");
    let start = line.find(&search)? + search.len();
    let rest = &line[start..];
    // If value starts with a quote, delegate to quoted extraction
    if rest.starts_with('"') {
        let end = rest[1..].find('"')?;
        return Some(rest[1..1 + end].to_string());
    }
    // Unquoted value ends at comma or end of line
    let end = rest.find(',').unwrap_or(rest.len());
    Some(rest[..end].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE_URL: &str = "https://cdn.example.com/content/master.m3u8";

    fn minimal_vod_manifest() -> &'static str {
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:7\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-MAP:URI=\"init.mp4\"\n\
         #EXTINF:6.006000,\n\
         segment_0.cmfv\n\
         #EXTINF:6.006000,\n\
         segment_1.cmfv\n\
         #EXTINF:4.004000,\n\
         segment_2.cmfv\n\
         #EXT-X-ENDLIST\n"
    }

    #[test]
    fn parse_minimal_vod_manifest() {
        let result = parse_hls_manifest(minimal_vod_manifest(), BASE_URL).unwrap();
        assert_eq!(
            result.init_segment_url,
            "https://cdn.example.com/content/init.mp4"
        );
        assert_eq!(result.segment_urls.len(), 3);
        assert_eq!(result.segment_durations.len(), 3);
        assert!(!result.is_live);
    }

    #[test]
    fn parse_resolves_relative_urls() {
        let result = parse_hls_manifest(minimal_vod_manifest(), BASE_URL).unwrap();
        assert_eq!(
            result.segment_urls[0],
            "https://cdn.example.com/content/segment_0.cmfv"
        );
        assert_eq!(
            result.segment_urls[2],
            "https://cdn.example.com/content/segment_2.cmfv"
        );
    }

    #[test]
    fn parse_extracts_durations() {
        let result = parse_hls_manifest(minimal_vod_manifest(), BASE_URL).unwrap();
        assert!((result.segment_durations[0] - 6.006).abs() < 0.001);
        assert!((result.segment_durations[1] - 6.006).abs() < 0.001);
        assert!((result.segment_durations[2] - 4.004).abs() < 0.001);
    }

    #[test]
    fn parse_live_manifest_no_endlist() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-TARGETDURATION:7\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.006,\n\
             segment_0.cmfv\n\
             #EXTINF:6.006,\n\
             segment_1.cmfv\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert!(result.is_live);
        assert_eq!(result.segment_urls.len(), 2);
    }

    #[test]
    fn parse_absolute_segment_urls() {
        let manifest = "#EXTM3U\n\
             #EXT-X-MAP:URI=\"https://other.cdn.com/init.mp4\"\n\
             #EXTINF:6.0,\n\
             https://other.cdn.com/seg_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.init_segment_url, "https://other.cdn.com/init.mp4");
        assert_eq!(result.segment_urls[0], "https://other.cdn.com/seg_0.cmfv");
    }

    #[test]
    fn parse_master_playlist_returns_error() {
        let master = "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.64001f\"\n\
             variant_high.m3u8\n";
        let result = parse_hls_manifest(master, BASE_URL);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("master playlist"));
    }

    #[test]
    fn parse_missing_init_segment_returns_error() {
        let manifest = "#EXTM3U\n\
             #EXTINF:6.0,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("EXT-X-MAP"));
    }

    #[test]
    fn parse_invalid_manifest_url() {
        let result = parse_hls_manifest(minimal_vod_manifest(), "not-a-url");
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("invalid manifest URL"));
    }

    #[test]
    fn parse_empty_manifest_no_segments() {
        let manifest = "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.init_segment_url, "https://cdn.example.com/content/init.mp4");
        assert!(result.segment_urls.is_empty());
        assert!(result.segment_durations.is_empty());
        assert!(!result.is_live);
    }

    #[test]
    fn parse_ignores_comment_lines() {
        let manifest = "#EXTM3U\n\
             # This is a comment\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.0,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.segment_urls.len(), 1);
    }

    #[test]
    fn parse_subdirectory_relative_paths() {
        let manifest = "#EXTM3U\n\
             #EXT-X-MAP:URI=\"video/init.mp4\"\n\
             #EXTINF:6.0,\n\
             video/segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(
            result.init_segment_url,
            "https://cdn.example.com/content/video/init.mp4"
        );
        assert_eq!(
            result.segment_urls[0],
            "https://cdn.example.com/content/video/segment_0.cmfv"
        );
    }

    #[test]
    fn parse_detects_cenc_from_ext_x_key() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-TARGETDURATION:7\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI=\"skd://key-id\",KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",KEYFORMATVERSIONS=\"1\"\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.006,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.source_scheme, Some(EncryptionScheme::Cenc));
    }

    #[test]
    fn parse_detects_cbcs_from_ext_x_key() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-TARGETDURATION:7\n\
             #EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://key-id\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.006,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.source_scheme, Some(EncryptionScheme::Cbcs));
    }

    #[test]
    fn parse_no_ext_x_key_source_scheme_is_none() {
        let result = parse_hls_manifest(minimal_vod_manifest(), BASE_URL).unwrap();
        assert_eq!(result.source_scheme, None);
    }

    #[test]
    fn parse_ignores_aes128_method() {
        let manifest = "#EXTM3U\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"https://example.com/key\"\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.006,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.source_scheme, None);
    }

    #[test]
    fn parse_daterange_with_scte35_cmd() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-TARGETDURATION:7\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.006,\n\
             segment_0.cmfv\n\
             #EXT-X-DATERANGE:ID=\"splice-42\",START-DATE=\"2024-01-01T00:00:06.006Z\",PLANNED-DURATION=30.0,SCTE35-CMD=0xFC301100000000000000FF\n\
             #EXTINF:6.006,\n\
             segment_1.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.ad_breaks.len(), 1);
        let ab = &result.ad_breaks[0];
        assert_eq!(ab.id, 42);
        assert!((ab.duration.unwrap() - 30.0).abs() < 0.001);
        assert!(ab.scte35_cmd.is_some());
        // Segment number should be 1 (DATERANGE appears after segment_0's URI)
        assert_eq!(ab.segment_number, 1);
    }

    #[test]
    fn parse_daterange_scte35_out() {
        let manifest = "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:6.0,\n\
             segment_0.cmfv\n\
             #EXT-X-DATERANGE:ID=\"splice-100\",START-DATE=\"2024-01-01T00:00:00Z\",SCTE35-OUT=YES,PLANNED-DURATION=15.5\n\
             #EXTINF:6.0,\n\
             segment_1.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.ad_breaks.len(), 1);
        let ab = &result.ad_breaks[0];
        assert_eq!(ab.id, 100);
        assert!((ab.duration.unwrap() - 15.5).abs() < 0.001);
        assert!(ab.scte35_cmd.is_none()); // SCTE35-OUT, not SCTE35-CMD
    }

    #[test]
    fn parse_daterange_no_scte35_ignored() {
        // A DATERANGE without SCTE35-CMD or SCTE35-OUT should be ignored
        let manifest = "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXT-X-DATERANGE:ID=\"program-1\",START-DATE=\"2024-01-01T00:00:00Z\",PLANNED-DURATION=60.0\n\
             #EXTINF:6.0,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert!(result.ad_breaks.is_empty());
    }

    #[test]
    fn parse_no_daterange_empty_ad_breaks() {
        let result = parse_hls_manifest(minimal_vod_manifest(), BASE_URL).unwrap();
        assert!(result.ad_breaks.is_empty());
    }

    #[test]
    fn extract_attribute_unquoted_basic() {
        let line = r#"#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,URI="skd://key""#;
        assert_eq!(
            extract_attribute_unquoted(line, "METHOD"),
            Some("SAMPLE-AES-CTR".to_string())
        );
    }

    #[test]
    fn extract_attribute_unquoted_at_end() {
        let line = r#"#EXT-X-KEY:URI="skd://key",METHOD=SAMPLE-AES"#;
        assert_eq!(
            extract_attribute_unquoted(line, "METHOD"),
            Some("SAMPLE-AES".to_string())
        );
    }

    #[test]
    fn extract_attribute_unquoted_missing() {
        let line = r#"#EXT-X-KEY:METHOD=SAMPLE-AES"#;
        assert_eq!(extract_attribute_unquoted(line, "URI"), None);
    }

    #[test]
    fn extract_attribute_basic() {
        let line = r#"#EXT-X-MAP:URI="init.mp4""#;
        assert_eq!(extract_attribute(line, "URI"), Some("init.mp4".to_string()));
    }

    #[test]
    fn extract_attribute_missing() {
        let line = r#"#EXT-X-MAP:URI="init.mp4""#;
        assert_eq!(extract_attribute(line, "BYTERANGE"), None);
    }

    #[test]
    fn extract_attribute_multiple() {
        let line = r#"#EXT-X-MAP:URI="init.mp4",BYTERANGE="500@0""#;
        assert_eq!(extract_attribute(line, "URI"), Some("init.mp4".to_string()));
        assert_eq!(
            extract_attribute(line, "BYTERANGE"),
            Some("500@0".to_string())
        );
    }

    // --- LL-HLS parsing tests ---

    #[test]
    fn parse_part_inf() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:9\n\
             #EXT-X-TARGETDURATION:4\n\
             #EXT-X-PART-INF:PART-TARGET=0.33334\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4.0,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.part_target_duration, Some(0.33334));
    }

    #[test]
    fn parse_ext_x_parts() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:9\n\
             #EXT-X-TARGETDURATION:4\n\
             #EXT-X-PART-INF:PART-TARGET=0.33334\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXT-X-PART:DURATION=0.33334,URI=\"part0.0.cmfv\",INDEPENDENT=YES\n\
             #EXT-X-PART:DURATION=0.33334,URI=\"part0.1.cmfv\"\n\
             #EXT-X-PART:DURATION=0.33334,URI=\"part0.2.cmfv\"\n\
             #EXTINF:1.0,\n\
             segment_0.cmfv\n\
             #EXT-X-PART:DURATION=0.33334,URI=\"part1.0.cmfv\",INDEPENDENT=YES\n\
             #EXTINF:0.33334,\n\
             segment_1.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.parts.len(), 4);
        // First 3 parts belong to segment 0
        assert_eq!(result.parts[0].segment_number, 0);
        assert_eq!(result.parts[0].part_index, 0);
        assert!(result.parts[0].independent);
        assert_eq!(result.parts[1].segment_number, 0);
        assert_eq!(result.parts[1].part_index, 1);
        assert!(!result.parts[1].independent);
        assert_eq!(result.parts[2].segment_number, 0);
        assert_eq!(result.parts[2].part_index, 2);
        // Fourth part belongs to segment 1 (part index resets)
        assert_eq!(result.parts[3].segment_number, 1);
        assert_eq!(result.parts[3].part_index, 0);
        assert!(result.parts[3].independent);
    }

    #[test]
    fn parse_server_control_tag() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:9\n\
             #EXT-X-TARGETDURATION:4\n\
             #EXT-X-SERVER-CONTROL:CAN-BLOCK-RELOAD=YES,PART-HOLD-BACK=1.0,CAN-SKIP-UNTIL=12.0\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4.0,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        let sc = result.server_control.unwrap();
        assert!(sc.can_block_reload);
        assert_eq!(sc.part_hold_back, Some(1.0));
        assert_eq!(sc.can_skip_until, Some(12.0));
        assert!(sc.hold_back.is_none());
    }

    #[test]
    fn parse_preload_hint_skipped() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:9\n\
             #EXT-X-TARGETDURATION:4\n\
             #EXT-X-PRELOAD-HINT:TYPE=PART,URI=\"next.cmfv\"\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXTINF:4.0,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        // Preload hint is acknowledged but not stored
        assert_eq!(result.segment_urls.len(), 1);
    }

    #[test]
    fn parse_backward_compat_no_ll_tags() {
        let result = parse_hls_manifest(minimal_vod_manifest(), BASE_URL).unwrap();
        assert!(result.parts.is_empty());
        assert!(result.part_target_duration.is_none());
        assert!(result.server_control.is_none());
    }

    #[test]
    fn parse_part_independent_flag() {
        let manifest = "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4\"\n\
             #EXT-X-PART:DURATION=0.5,URI=\"p.cmfv\",INDEPENDENT=YES\n\
             #EXTINF:0.5,\n\
             segment_0.cmfv\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert_eq!(result.parts.len(), 1);
        assert!(result.parts[0].independent);
    }

    // --- TS source detection tests ---

    #[test]
    fn parse_ts_source_detected_from_extension() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:10\n\
             #EXTINF:10.0,\n\
             segment_0.ts\n\
             #EXTINF:10.0,\n\
             segment_1.ts\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert!(result.is_ts_source);
        assert_eq!(result.segment_urls.len(), 2);
        // init_segment_url should be empty for TS (no #EXT-X-MAP)
        assert_eq!(result.init_segment_url, "");
    }

    #[test]
    fn parse_ts_source_with_aes128_key() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:10\n\
             #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example.com/key.bin\",IV=0x00000000000000000000000000000001\n\
             #EXTINF:10.0,\n\
             segment_0.ts\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert!(result.is_ts_source);
        assert_eq!(
            result.aes128_key_url.as_deref(),
            Some("https://keys.example.com/key.bin")
        );
        assert!(result.aes128_iv.is_some());
        let iv = result.aes128_iv.unwrap();
        assert_eq!(iv[15], 0x01);
        assert_eq!(iv[0], 0x00);
    }

    #[test]
    fn parse_cmaf_source_not_ts() {
        let result = parse_hls_manifest(minimal_vod_manifest(), BASE_URL).unwrap();
        assert!(!result.is_ts_source);
        assert!(result.aes128_key_url.is_none());
        assert!(result.aes128_iv.is_none());
    }

    #[test]
    fn parse_ts_source_with_query_string() {
        let manifest = "#EXTM3U\n\
             #EXT-X-VERSION:3\n\
             #EXT-X-TARGETDURATION:10\n\
             #EXTINF:10.0,\n\
             segment_0.ts?token=abc\n\
             #EXT-X-ENDLIST\n";
        let result = parse_hls_manifest(manifest, BASE_URL).unwrap();
        assert!(result.is_ts_source);
    }
}
