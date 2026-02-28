use thiserror::Error;

#[derive(Error, Debug)]
pub enum EdgepackError {
    #[error("cache error: {0}")]
    Cache(String),

    #[error("DRM error: {0}")]
    Drm(String),

    #[error("SPEKE request failed: {0}")]
    Speke(String),

    #[error("CPIX parse error: {0}")]
    Cpix(String),

    #[error("encryption error: {0}")]
    Encryption(String),

    #[error("ISOBMFF parse error: {0}")]
    MediaParse(String),

    #[error("segment rewrite error: {0}")]
    SegmentRewrite(String),

    #[error("manifest error: {0}")]
    Manifest(String),

    #[error("HTTP error: {status} {message}")]
    Http { status: u16, message: String },

    #[error("configuration error: {0}")]
    Config(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, EdgepackError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_cache() {
        let e = EdgepackError::Cache("connection refused".into());
        assert_eq!(e.to_string(), "cache error: connection refused");
    }

    #[test]
    fn error_display_drm() {
        let e = EdgepackError::Drm("key not found".into());
        assert_eq!(e.to_string(), "DRM error: key not found");
    }

    #[test]
    fn error_display_speke() {
        let e = EdgepackError::Speke("timeout".into());
        assert_eq!(e.to_string(), "SPEKE request failed: timeout");
    }

    #[test]
    fn error_display_cpix() {
        let e = EdgepackError::Cpix("malformed XML".into());
        assert_eq!(e.to_string(), "CPIX parse error: malformed XML");
    }

    #[test]
    fn error_display_encryption() {
        let e = EdgepackError::Encryption("invalid key length".into());
        assert_eq!(e.to_string(), "encryption error: invalid key length");
    }

    #[test]
    fn error_display_media_parse() {
        let e = EdgepackError::MediaParse("truncated box".into());
        assert_eq!(e.to_string(), "ISOBMFF parse error: truncated box");
    }

    #[test]
    fn error_display_segment_rewrite() {
        let e = EdgepackError::SegmentRewrite("mdat too short".into());
        assert_eq!(e.to_string(), "segment rewrite error: mdat too short");
    }

    #[test]
    fn error_display_manifest() {
        let e = EdgepackError::Manifest("missing init segment".into());
        assert_eq!(e.to_string(), "manifest error: missing init segment");
    }

    #[test]
    fn error_display_http() {
        let e = EdgepackError::Http {
            status: 404,
            message: "not found".into(),
        };
        assert_eq!(e.to_string(), "HTTP error: 404 not found");
    }

    #[test]
    fn error_display_config() {
        let e = EdgepackError::Config("missing env var".into());
        assert_eq!(e.to_string(), "configuration error: missing env var");
    }

    #[test]
    fn error_display_invalid_input() {
        let e = EdgepackError::InvalidInput("bad format".into());
        assert_eq!(e.to_string(), "invalid input: bad format");
    }

    #[test]
    fn error_display_not_found() {
        let e = EdgepackError::NotFound("segment 5".into());
        assert_eq!(e.to_string(), "not found: segment 5");
    }

    #[test]
    fn error_display_io() {
        let e = EdgepackError::Io("read failed".into());
        assert_eq!(e.to_string(), "IO error: read failed");
    }

    #[test]
    fn error_is_debug() {
        let e = EdgepackError::Cache("test".into());
        let debug = format!("{:?}", e);
        assert!(debug.contains("Cache"));
    }

    #[test]
    fn result_type_alias_ok() {
        let ok: Result<i32> = Ok(42);
        assert_eq!(ok.unwrap(), 42);
    }

    #[test]
    fn result_type_alias_err() {
        let err: Result<i32> = Err(EdgepackError::Config("test".into()));
        assert!(err.is_err());
    }
}
