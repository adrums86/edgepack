use crate::drm::scheme::EncryptionScheme;
use crate::media::container::ContainerFormat;
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
    /// ISO 639-2/T language code (e.g., "eng", "und"). Used for audio/subtitle renditions.
    #[serde(default)]
    pub language: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackMediaType {
    Video,
    Audio,
    Subtitle,
}

/// DRM signaling information for manifests.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestDrmInfo {
    /// Encryption scheme used for this output.
    pub encryption_scheme: EncryptionScheme,
    /// Widevine PSSH box (base64-encoded full box).
    pub widevine_pssh: Option<String>,
    /// PlayReady PSSH / PRO (base64-encoded).
    pub playready_pssh: Option<String>,
    /// PlayReady content protection data (XML, for DASH).
    pub playready_pro: Option<String>,
    /// FairPlay key URI (for CBCS output with FairPlay).
    pub fairplay_key_uri: Option<String>,
    /// Default Key ID (hex string, no hyphens).
    pub default_kid: String,
}

/// SCTE-35 ad break information extracted from emsg boxes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdBreakInfo {
    /// Splice event ID (from splice_insert).
    pub id: u32,
    /// Presentation time in seconds from stream start.
    pub presentation_time: f64,
    /// Break duration in seconds (None = unknown/unbounded).
    pub duration: Option<f64>,
    /// Base64-encoded splice_info_section (for HLS SCTE35-CMD).
    pub scte35_cmd: Option<String>,
    /// Segment number containing this splice point.
    pub segment_number: u32,
}

/// CEA-608/708 closed caption channel info for manifest signaling.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CeaCaptionInfo {
    /// CEA service name (e.g., "CC1", "SERVICE1").
    pub service_name: String,
    /// Language code (e.g., "eng", "spa").
    pub language: String,
    /// Whether this is CEA-608 (true) or CEA-708 (false).
    pub is_608: bool,
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
    /// Container format (CMAF or fMP4). Defaults to CMAF for backward compatibility.
    #[serde(default)]
    pub container_format: ContainerFormat,
    /// CEA-608/708 closed caption channels (embedded in video, signaled in manifest).
    #[serde(default)]
    pub cea_captions: Vec<CeaCaptionInfo>,
    /// SCTE-35 ad break markers extracted from emsg boxes.
    #[serde(default)]
    pub ad_breaks: Vec<AdBreakInfo>,
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
    pub fn new(
        content_id: String,
        format: OutputFormat,
        base_url: String,
        container_format: ContainerFormat,
    ) -> Self {
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
            container_format,
            cea_captions: Vec::new(),
            ad_breaks: Vec::new(),
        }
    }

    pub fn is_complete(&self) -> bool {
        self.phase == ManifestPhase::Complete
    }
}

/// Parsed source manifest information (input side).
///
/// Extracted from an HLS M3U8 or DASH MPD source manifest. Contains
/// the URLs needed to fetch init and media segments from the origin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceManifest {
    /// URL of the init segment.
    pub init_segment_url: String,
    /// URLs of all media segments in order.
    pub segment_urls: Vec<String>,
    /// Duration of each media segment in seconds.
    pub segment_durations: Vec<f64>,
    /// Whether the source is a live/dynamic stream.
    pub is_live: bool,
    /// Encryption scheme detected from manifest DRM signaling.
    /// `None` if not signaled in the manifest (will be detected from init segment instead).
    pub source_scheme: Option<EncryptionScheme>,
    /// Ad break markers parsed from source manifest (HLS EXT-X-DATERANGE, DASH EventStream).
    #[serde(default)]
    pub ad_breaks: Vec<AdBreakInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::container::ContainerFormat;

    #[test]
    fn output_format_content_type_hls() {
        assert_eq!(OutputFormat::Hls.content_type(), "application/vnd.apple.mpegurl");
    }

    #[test]
    fn output_format_content_type_dash() {
        assert_eq!(OutputFormat::Dash.content_type(), "application/dash+xml");
    }

    #[test]
    fn output_format_manifest_extension_hls() {
        assert_eq!(OutputFormat::Hls.manifest_extension(), "m3u8");
    }

    #[test]
    fn output_format_manifest_extension_dash() {
        assert_eq!(OutputFormat::Dash.manifest_extension(), "mpd");
    }

    #[test]
    fn output_format_serde_roundtrip() {
        let hls = OutputFormat::Hls;
        let json = serde_json::to_string(&hls).unwrap();
        let parsed: OutputFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, hls);

        let dash = OutputFormat::Dash;
        let json = serde_json::to_string(&dash).unwrap();
        let parsed: OutputFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, dash);
    }

    #[test]
    fn output_format_equality() {
        assert_eq!(OutputFormat::Hls, OutputFormat::Hls);
        assert_eq!(OutputFormat::Dash, OutputFormat::Dash);
        assert_ne!(OutputFormat::Hls, OutputFormat::Dash);
    }

    #[test]
    fn segment_info_construction() {
        let seg = SegmentInfo {
            number: 5,
            duration: 6.006,
            uri: "segment_5.cmfv".to_string(),
            byte_size: 1024,
        };
        assert_eq!(seg.number, 5);
        assert!((seg.duration - 6.006).abs() < f64::EPSILON);
        assert_eq!(seg.uri, "segment_5.cmfv");
        assert_eq!(seg.byte_size, 1024);
    }

    #[test]
    fn segment_info_serde_roundtrip() {
        let seg = SegmentInfo {
            number: 3,
            duration: 4.004,
            uri: "segment_3.cmfv".to_string(),
            byte_size: 2048,
        };
        let json = serde_json::to_string(&seg).unwrap();
        let parsed: SegmentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.number, 3);
        assert!((parsed.duration - 4.004).abs() < f64::EPSILON);
    }

    #[test]
    fn init_segment_info_construction() {
        let init = InitSegmentInfo {
            uri: "init.mp4".to_string(),
            byte_size: 512,
        };
        assert_eq!(init.uri, "init.mp4");
        assert_eq!(init.byte_size, 512);
    }

    #[test]
    fn variant_info_video() {
        let v = VariantInfo {
            id: "v1".to_string(),
            bandwidth: 2_000_000,
            codecs: "avc1.64001f".to_string(),
            resolution: Some((1920, 1080)),
            frame_rate: Some(30.0),
            track_type: TrackMediaType::Video,
            language: None,
        };
        assert_eq!(v.track_type, TrackMediaType::Video);
        assert_eq!(v.resolution, Some((1920, 1080)));
    }

    #[test]
    fn variant_info_audio() {
        let v = VariantInfo {
            id: "a1".to_string(),
            bandwidth: 128_000,
            codecs: "mp4a.40.2".to_string(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Audio,
            language: Some("eng".to_string()),
        };
        assert_eq!(v.track_type, TrackMediaType::Audio);
        assert!(v.resolution.is_none());
        assert_eq!(v.language.as_deref(), Some("eng"));
    }

    #[test]
    fn variant_info_subtitle() {
        let v = VariantInfo {
            id: "sub_eng".to_string(),
            bandwidth: 0,
            codecs: "wvtt".to_string(),
            resolution: None,
            frame_rate: None,
            track_type: TrackMediaType::Subtitle,
            language: Some("eng".to_string()),
        };
        assert_eq!(v.track_type, TrackMediaType::Subtitle);
        assert_eq!(v.codecs, "wvtt");
        assert_eq!(v.language.as_deref(), Some("eng"));
    }

    #[test]
    fn track_media_type_equality() {
        assert_eq!(TrackMediaType::Video, TrackMediaType::Video);
        assert_eq!(TrackMediaType::Audio, TrackMediaType::Audio);
        assert_eq!(TrackMediaType::Subtitle, TrackMediaType::Subtitle);
        assert_ne!(TrackMediaType::Video, TrackMediaType::Audio);
        assert_ne!(TrackMediaType::Video, TrackMediaType::Subtitle);
        assert_ne!(TrackMediaType::Audio, TrackMediaType::Subtitle);
    }

    #[test]
    fn cea_caption_info_construction() {
        let caption = CeaCaptionInfo {
            service_name: "CC1".to_string(),
            language: "eng".to_string(),
            is_608: true,
        };
        assert_eq!(caption.service_name, "CC1");
        assert_eq!(caption.language, "eng");
        assert!(caption.is_608);
    }

    #[test]
    fn manifest_drm_info_construction() {
        let drm = ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("AAAA".to_string()),
            playready_pssh: Some("BBBB".to_string()),
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".to_string(),
        };
        assert_eq!(drm.encryption_scheme, EncryptionScheme::Cenc);
        assert!(drm.widevine_pssh.is_some());
        assert!(drm.playready_pssh.is_some());
        assert!(drm.playready_pro.is_none());
        assert!(drm.fairplay_key_uri.is_none());
        assert_eq!(drm.default_kid.len(), 32);
    }

    #[test]
    fn manifest_phase_values() {
        assert_eq!(ManifestPhase::AwaitingFirstSegment, ManifestPhase::AwaitingFirstSegment);
        assert_eq!(ManifestPhase::Live, ManifestPhase::Live);
        assert_eq!(ManifestPhase::Complete, ManifestPhase::Complete);
        assert_ne!(ManifestPhase::AwaitingFirstSegment, ManifestPhase::Live);
        assert_ne!(ManifestPhase::Live, ManifestPhase::Complete);
    }

    #[test]
    fn manifest_state_new_defaults() {
        let state = ManifestState::new("test-content".into(), OutputFormat::Hls, "/base/".into(), ContainerFormat::default());
        assert_eq!(state.content_id, "test-content");
        assert_eq!(state.format, OutputFormat::Hls);
        assert_eq!(state.phase, ManifestPhase::AwaitingFirstSegment);
        assert!(state.init_segment.is_none());
        assert!(state.segments.is_empty());
        assert!((state.target_duration - 6.0).abs() < f64::EPSILON);
        assert!(state.variants.is_empty());
        assert!(state.drm_info.is_none());
        assert_eq!(state.media_sequence, 0);
        assert_eq!(state.base_url, "/base/");
    }

    #[test]
    fn manifest_state_is_complete() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Dash, "/".into(), ContainerFormat::default());
        assert!(!state.is_complete());

        state.phase = ManifestPhase::Live;
        assert!(!state.is_complete());

        state.phase = ManifestPhase::Complete;
        assert!(state.is_complete());
    }

    #[test]
    fn ad_break_info_construction() {
        let ab = AdBreakInfo {
            id: 42,
            presentation_time: 30.0,
            duration: Some(15.0),
            scte35_cmd: Some("base64data".to_string()),
            segment_number: 5,
        };
        assert_eq!(ab.id, 42);
        assert!((ab.presentation_time - 30.0).abs() < f64::EPSILON);
        assert_eq!(ab.duration, Some(15.0));
        assert_eq!(ab.segment_number, 5);
    }

    #[test]
    fn ad_break_info_serde_roundtrip() {
        let ab = AdBreakInfo {
            id: 1,
            presentation_time: 60.0,
            duration: None,
            scte35_cmd: None,
            segment_number: 10,
        };
        let json = serde_json::to_string(&ab).unwrap();
        let parsed: AdBreakInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, 1);
        assert!((parsed.presentation_time - 60.0).abs() < f64::EPSILON);
        assert!(parsed.duration.is_none());
        assert!(parsed.scte35_cmd.is_none());
    }

    #[test]
    fn manifest_state_with_ad_breaks() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        assert!(state.ad_breaks.is_empty());
        state.ad_breaks.push(AdBreakInfo {
            id: 1,
            presentation_time: 30.0,
            duration: Some(15.0),
            scte35_cmd: None,
            segment_number: 5,
        });
        assert_eq!(state.ad_breaks.len(), 1);

        // Verify serde roundtrip with ad_breaks
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ManifestState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.ad_breaks.len(), 1);
        assert_eq!(parsed.ad_breaks[0].id, 1);
    }

    #[test]
    fn manifest_state_serde_backward_compat_no_ad_breaks() {
        // Verify ManifestState from JSON without ad_breaks field deserializes correctly
        let json = r#"{"content_id":"c","format":"Hls","phase":"Live","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
        let parsed: ManifestState = serde_json::from_str(json).unwrap();
        assert!(parsed.ad_breaks.is_empty());
    }

    #[test]
    fn manifest_state_serde_roundtrip() {
        let mut state = ManifestState::new("content-1".into(), OutputFormat::Hls, "/base/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        state.segments.push(SegmentInfo {
            number: 0,
            duration: 6.0,
            uri: "segment_0.cmfv".to_string(),
            byte_size: 1024,
        });
        state.init_segment = Some(InitSegmentInfo {
            uri: "init.mp4".into(),
            byte_size: 256,
        });

        let json = serde_json::to_string(&state).unwrap();
        let parsed: ManifestState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "content-1");
        assert_eq!(parsed.format, OutputFormat::Hls);
        assert_eq!(parsed.phase, ManifestPhase::Live);
        assert_eq!(parsed.segments.len(), 1);
        assert!(parsed.init_segment.is_some());
    }
}
