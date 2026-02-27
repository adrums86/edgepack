use thiserror::Error;

#[derive(Error, Debug)]
pub enum EdgePackagerError {
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

pub type Result<T> = std::result::Result<T, EdgePackagerError>;
