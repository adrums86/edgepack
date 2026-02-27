use serde::{Deserialize, Serialize};

/// Output format for the repackaged content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutputFormat {
    Hls,
    Dash,
}

impl OutputFormat {
    pub fn content_type(&self) -> &'static str {
        match self {
            OutputFormat::Hls => "application/vnd.apple.mpegurl",
            OutputFormat::Dash => "application/dash+xml",
        }
    }

    pub fn manifest_extension(&self) -> &'static str {
        match self {
            OutputFormat::Hls => "m3u8",
            OutputFormat::Dash => "mpd",
        }
    }
}

/// Information about a single media segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentInfo {
    /// Segment sequence number (0-indexed).
    pub number: u32,
    /// Duration of the segment in seconds.
    pub duration: f64,
    /// URI path for this segment (relative to manifest).
    pub uri: String,
    /// Byte size of the segment.
    pub byte_size: u64,
}

/// Information about the init segment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitSegmentInfo {
    /// URI path for the init segment.
    pub uri: String,
    /// Byte size of the init segment.
    pub byte_size: u64,
}

/// Representation/variant stream info for multi-bitrate content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VariantInfo {
    /// Track/representation ID.
    pub id: String,
    /// Bandwidth in bits per second.
    pub bandwidth: u64,
    /// Codec string (e.g., "avc1.64001f", "mp4a.40.2").
    pub codecs: String,
    /// Resolution (width x height) for video tracks.
    pub resolution: Option<(u32, u32)>,
    /// Frame rate for video tracks.
    pub frame_rate: Option<f64>,
    /// Track type.
    pub track_type: TrackMediaType,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackMediaType {
    Video,
    Audio,
}

/// DRM signaling information for manifests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestDrmInfo {
    /// Widevine PSSH box (base64-encoded full box).
    pub widevine_pssh: Option<String>,
    /// PlayReady PSSH / PRO (base64-encoded).
    pub playready_pssh: Option<String>,
    /// PlayReady content protection data (XML, for DASH).
    pub playready_pro: Option<String>,
    /// Default Key ID (hex string, no hyphens).
    pub default_kid: String,
}

/// The persistent state of a manifest being progressively built.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestState {
    /// Content identifier.
    pub content_id: String,
    /// Output format.
    pub format: OutputFormat,
    /// Current lifecycle phase.
    pub phase: ManifestPhase,
    /// Init segment info (set once after first segment).
    pub init_segment: Option<InitSegmentInfo>,
    /// Completed segments in order.
    pub segments: Vec<SegmentInfo>,
    /// Target segment duration (for HLS EXT-X-TARGETDURATION).
    pub target_duration: f64,
    /// Variant/representation info.
    pub variants: Vec<VariantInfo>,
    /// DRM signaling data for manifest.
    pub drm_info: Option<ManifestDrmInfo>,
    /// Media sequence number (HLS).
    pub media_sequence: u32,
    /// Base URL path for segments.
    pub base_url: String,
}

/// Lifecycle phase of the manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManifestPhase {
    /// No segments processed yet.
    AwaitingFirstSegment,
    /// Manifest is live/dynamic — segments are being added.
    Live,
    /// All segments processed — manifest is finalized.
    Complete,
}

impl ManifestState {
    pub fn new(content_id: String, format: OutputFormat, base_url: String) -> Self {
        Self {
            content_id,
            format,
            phase: ManifestPhase::AwaitingFirstSegment,
            init_segment: None,
            segments: Vec::new(),
            target_duration: 6.0,
            variants: Vec::new(),
            drm_info: None,
            media_sequence: 0,
            base_url,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.phase == ManifestPhase::Complete
    }
}
