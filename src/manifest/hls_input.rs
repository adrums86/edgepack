//! HLS M3U8 source manifest input parser.
//!
//! Parses an HLS media playlist to extract init segment URL, media segment URLs,
//! durations, and live/VOD status. This is the *input* side — the output renderers
//! are in `hls.rs`.

use crate::error::{EdgePackagerError, Result};
use crate::manifest::types::SourceManifest;
use url::Url;

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

    for line in manifest_text.lines() {
        let line = line.trim();

        if line.starts_with("#EXT-X-MAP:") {
            if let Some(uri) = extract_attribute(line, "URI") {
                init_segment_url = Some(resolve_url(&base_url, &uri)?);
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
