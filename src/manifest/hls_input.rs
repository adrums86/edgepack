//! HLS M3U8 source manifest input parser.
//!
//! Parses an HLS media playlist to extract init segment URL, media segment URLs,
//! durations, and live/VOD status. This is the *input* side — the output renderers
//! are in `hls.rs`.

use crate::drm::scheme::EncryptionScheme;
use crate::error::{EdgePackagerError, Result};
use crate::manifest::types::SourceManifest;
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
        EdgePackagerError::Manifest(format!("invalid manifest URL: {e}"))
    })?;

    // Detect master playlist
    if manifest_text.contains("#EXT-X-STREAM-INF") {
        return Err(EdgePackagerError::Manifest(
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
                    _ => {} // AES-128, NONE, etc. — not CENC/CBCS
                }
            }
        } else if line.starts_with("#EXTINF:") {
            let duration_str = line.strip_prefix("#EXTINF:").unwrap_or("");
            let duration_str = duration_str.split(',').next().unwrap_or("0");
            pending_duration = duration_str.parse::<f64>().ok();
        } else if line.starts_with("#EXT-X-ENDLIST") {
            is_live = false;
        } else if !line.starts_with('#') && !line.is_empty() {
            // URI line — associate with the pending EXTINF duration
            if pending_duration.is_some() {
                segment_urls.push(resolve_url(&base_url, line)?);
                segment_durations.push(pending_duration.take().unwrap_or(6.0));
            }
        }
    }

    let init_url = init_segment_url.ok_or_else(|| {
        EdgePackagerError::Manifest(
            "HLS manifest missing #EXT-X-MAP (init segment)".into(),
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

/// Extract a quoted attribute value from an HLS tag line.
/// e.g., extract_attribute(`#EXT-X-MAP:URI="init.mp4"`, "URI") returns Some("init.mp4")
fn extract_attribute(line: &str, attr: &str) -> Option<String> {
    let search = format!("{attr}=\"");
    let start = line.find(&search)? + search.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
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
}
