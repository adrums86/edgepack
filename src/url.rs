//! Lightweight URL parser for edge-packager.
//!
//! Replaces the `url` crate to avoid pulling in ICU/IDNA Unicode
//! normalization tables (~200 KB of static data in the WASM binary).
//! Supports the subset of URL operations used by the crate:
//! parsing, component access, relative URL resolution, and serde.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A parsed URL with scheme, authority, path, and query components.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Url {
    /// Full original/normalized URL string.
    raw: String,
    /// Byte offset where the scheme ends (just before "://").
    scheme_end: usize,
    /// Byte offset where the authority starts (after "://").
    authority_start: usize,
    /// Byte offset where the authority ends / path starts.
    path_start: usize,
    /// Byte offset where the query starts (after '?'), or raw.len() if none.
    query_start: usize,
}

/// Error returned when URL parsing fails.
#[derive(Debug, Clone)]
pub struct ParseError(String);

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid URL: {}", self.0)
    }
}

impl std::error::Error for ParseError {}

impl Url {
    /// Parse an absolute URL string.
    ///
    /// Supports `http://` and `https://` schemes. Does not perform IDNA
    /// normalization — hostnames are used as-is (ASCII CDN hostnames only).
    pub fn parse(input: &str) -> Result<Self, ParseError> {
        let scheme_end = input.find("://").ok_or_else(|| {
            ParseError(format!("missing scheme in '{input}'"))
        })?;

        let scheme = &input[..scheme_end];
        if !scheme.chars().all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.') {
            return Err(ParseError(format!("invalid scheme: '{scheme}'")));
        }

        let authority_start = scheme_end + 3; // skip "://"
        if authority_start >= input.len() {
            return Err(ParseError("empty authority".into()));
        }

        // Find where authority ends and path begins.
        let rest = &input[authority_start..];
        let path_offset = rest.find('/').unwrap_or(rest.len());
        let path_start = authority_start + path_offset;

        // Find query string.
        let query_start = input[path_start..]
            .find('?')
            .map(|i| path_start + i + 1) // +1 to skip the '?'
            .unwrap_or(input.len());

        Ok(Url {
            raw: input.to_string(),
            scheme_end,
            authority_start,
            path_start,
            query_start,
        })
    }

    /// URL scheme (e.g. `"https"`).
    pub fn scheme(&self) -> &str {
        &self.raw[..self.scheme_end]
    }

    /// Authority / host string without port (e.g. `"cdn.example.com"`).
    pub fn host_str(&self) -> Option<&str> {
        let authority = self.authority();
        if authority.is_empty() {
            return None;
        }
        // Strip optional userinfo (user:pass@)
        let host_part = match authority.rfind('@') {
            Some(i) => &authority[i + 1..],
            None => authority,
        };
        // Strip port
        // Handle IPv6: [::1]:8080
        if host_part.starts_with('[') {
            // IPv6 — find closing bracket
            match host_part.find(']') {
                Some(bracket) => Some(&host_part[..bracket + 1]),
                None => Some(host_part),
            }
        } else {
            // Regular host — strip :port
            match host_part.rfind(':') {
                Some(i) => Some(&host_part[..i]),
                None => Some(host_part),
            }
        }
    }

    /// Port number if explicitly specified.
    pub fn port(&self) -> Option<u16> {
        let authority = self.authority();
        // Strip userinfo
        let host_part = match authority.rfind('@') {
            Some(i) => &authority[i + 1..],
            None => authority,
        };
        // Handle IPv6
        if host_part.starts_with('[') {
            // IPv6: [::1]:8080
            if let Some(bracket) = host_part.find(']') {
                let after = &host_part[bracket + 1..];
                if let Some(colon) = after.strip_prefix(':') {
                    return colon.parse().ok();
                }
            }
            None
        } else {
            // Regular host:port
            host_part.rfind(':').and_then(|i| host_part[i + 1..].parse().ok())
        }
    }

    /// Full authority string (host + optional port).
    fn authority(&self) -> &str {
        &self.raw[self.authority_start..self.path_start]
    }

    /// Path component (e.g. `"/content/master.m3u8"`). Always starts with `/`
    /// for URLs that have a path, or is `""` for authority-only URLs.
    pub fn path(&self) -> &str {
        let end = if self.query_start < self.raw.len() {
            self.query_start - 1 // -1 to exclude the '?'
        } else {
            self.raw.len()
        };
        &self.raw[self.path_start..end]
    }

    /// Query string without the leading `?`, or `None`.
    pub fn query(&self) -> Option<&str> {
        if self.query_start < self.raw.len() {
            Some(&self.raw[self.query_start..])
        } else {
            None
        }
    }

    /// Resolve a relative URL against this base URL.
    ///
    /// Handles:
    /// - Absolute URLs (returned as-is)
    /// - Protocol-relative (`//host/path`)
    /// - Root-relative (`/path`)
    /// - Path-relative (`segment.cmfv`, `../other`)
    pub fn join(&self, relative: &str) -> Result<Url, ParseError> {
        // Already absolute
        if relative.contains("://") {
            return Url::parse(relative);
        }

        // Protocol-relative
        if relative.starts_with("//") {
            let new_url = format!("{}:{relative}", self.scheme());
            return Url::parse(&new_url);
        }

        // Root-relative
        if relative.starts_with('/') {
            let new_url = format!(
                "{}://{}{}",
                self.scheme(),
                self.authority(),
                relative
            );
            return Url::parse(&new_url);
        }

        // Path-relative: resolve against the base path's directory
        let base_path = self.path();
        let dir = match base_path.rfind('/') {
            Some(i) => &base_path[..i + 1],
            None => "/",
        };

        // Combine and normalize ".." / "."
        let combined = format!("{dir}{relative}");
        let normalized = normalize_path(&combined);

        let new_url = format!("{}://{}{}", self.scheme(), self.authority(), normalized);
        Url::parse(&new_url)
    }

    /// Return the full URL as a string.
    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl fmt::Display for Url {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.raw)
    }
}

impl Serialize for Url {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.raw)
    }
}

impl<'de> Deserialize<'de> for Url {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Url::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// Normalize a path by resolving `.` and `..` segments.
fn normalize_path(path: &str) -> String {
    let mut segments: Vec<&str> = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." | "" if !segments.is_empty() => {}
            ".." => {
                segments.pop();
            }
            _ => segments.push(segment),
        }
    }
    let result = segments.join("/");
    if result.starts_with('/') {
        result
    } else {
        format!("/{result}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_https_url() {
        let url = Url::parse("https://cdn.example.com/content/master.m3u8").unwrap();
        assert_eq!(url.scheme(), "https");
        assert_eq!(url.host_str(), Some("cdn.example.com"));
        assert_eq!(url.port(), None);
        assert_eq!(url.path(), "/content/master.m3u8");
        assert_eq!(url.query(), None);
    }

    #[test]
    fn parse_url_with_port() {
        let url = Url::parse("http://localhost:8080/api/data").unwrap();
        assert_eq!(url.scheme(), "http");
        assert_eq!(url.host_str(), Some("localhost"));
        assert_eq!(url.port(), Some(8080));
        assert_eq!(url.path(), "/api/data");
    }

    #[test]
    fn parse_url_with_query() {
        let url = Url::parse("https://example.com/api?foo=bar&baz=1").unwrap();
        assert_eq!(url.path(), "/api");
        assert_eq!(url.query(), Some("foo=bar&baz=1"));
    }

    #[test]
    fn parse_authority_only() {
        let url = Url::parse("https://example.com").unwrap();
        assert_eq!(url.host_str(), Some("example.com"));
        assert_eq!(url.path(), "");
        assert_eq!(url.query(), None);
    }

    #[test]
    fn parse_missing_scheme_fails() {
        assert!(Url::parse("example.com/path").is_err());
    }

    #[test]
    fn display_roundtrip() {
        let input = "https://cdn.example.com/content/master.m3u8?token=abc";
        let url = Url::parse(input).unwrap();
        assert_eq!(url.to_string(), input);
    }

    #[test]
    fn join_absolute_url() {
        let base = Url::parse("https://cdn.example.com/content/master.m3u8").unwrap();
        let joined = base.join("https://other.com/video.mp4").unwrap();
        assert_eq!(joined.to_string(), "https://other.com/video.mp4");
    }

    #[test]
    fn join_root_relative() {
        let base = Url::parse("https://cdn.example.com/content/master.m3u8").unwrap();
        let joined = base.join("/other/media.m3u8").unwrap();
        assert_eq!(joined.to_string(), "https://cdn.example.com/other/media.m3u8");
    }

    #[test]
    fn join_path_relative() {
        let base = Url::parse("https://cdn.example.com/content/master.m3u8").unwrap();
        let joined = base.join("segment_0.cmfv").unwrap();
        assert_eq!(joined.to_string(), "https://cdn.example.com/content/segment_0.cmfv");
    }

    #[test]
    fn join_parent_relative() {
        let base = Url::parse("https://cdn.example.com/a/b/master.m3u8").unwrap();
        let joined = base.join("../c/segment.ts").unwrap();
        assert_eq!(joined.to_string(), "https://cdn.example.com/a/c/segment.ts");
    }

    #[test]
    fn join_protocol_relative() {
        let base = Url::parse("https://cdn.example.com/content/master.m3u8").unwrap();
        let joined = base.join("//other.com/path").unwrap();
        assert_eq!(joined.to_string(), "https://other.com/path");
    }

    #[test]
    fn serde_roundtrip() {
        let url = Url::parse("https://drm.example.com/speke/v2").unwrap();
        let json = serde_json::to_string(&url).unwrap();
        assert_eq!(json, "\"https://drm.example.com/speke/v2\"");
        let parsed: Url = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, url);
    }

    #[test]
    fn path_and_query_combined() {
        let url = Url::parse("https://example.com/api/data?foo=bar").unwrap();
        let path_and_query = match url.query() {
            Some(q) => format!("{}?{}", url.path(), q),
            None => url.path().to_string(),
        };
        assert_eq!(path_and_query, "/api/data?foo=bar");
    }

    #[test]
    fn authority_with_port() {
        let url = Url::parse("https://cdn.example.com:8443/path").unwrap();
        let authority = match url.port() {
            Some(port) => format!("{}:{}", url.host_str().unwrap_or(""), port),
            None => url.host_str().unwrap_or("").to_string(),
        };
        assert_eq!(authority, "cdn.example.com:8443");
    }
}
