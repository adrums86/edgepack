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
    /// Key rotation period index for this segment. None = no rotation.
    #[serde(default)]
    pub key_period: Option<u32>,
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
    /// ClearKey PSSH (base64-encoded full PSSH box).
    #[serde(default)]
    pub clearkey_pssh: Option<String>,
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

/// I-frame byte range within a regular media segment.
///
/// For each rewritten video segment, records the byte offset and size
/// of the first independent (IDR) moof+mdat chunk. Used to build
/// HLS `#EXT-X-I-FRAMES-ONLY` playlists with `#EXT-X-BYTERANGE`
/// and DASH trick play `<AdaptationSet>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IFrameSegmentInfo {
    /// Segment number (matches SegmentInfo.number).
    pub segment_number: u32,
    /// Byte offset of the IDR chunk within the segment.
    pub byte_offset: u64,
    /// Byte length of the IDR chunk.
    pub byte_length: u64,
    /// Duration of the parent segment in seconds.
    pub duration: f64,
    /// URI of the parent segment (same file, byte-ranged).
    pub segment_uri: String,
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

/// Information about a CMAF part (LL-HLS partial segment).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartInfo {
    /// Parent segment number.
    pub segment_number: u32,
    /// Part index within the segment (0-indexed).
    pub part_index: u32,
    /// Duration of this part in seconds.
    pub duration: f64,
    /// Whether this part starts with an independent frame (IDR/sync).
    pub independent: bool,
    /// URI path for this part.
    pub uri: String,
    /// Byte size of the part.
    pub byte_size: u64,
}

/// LL-HLS server control parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerControl {
    /// CAN-SKIP-UNTIL: duration in seconds of skippable content.
    #[serde(default)]
    pub can_skip_until: Option<f64>,
    /// HOLD-BACK: server-recommended live edge distance in seconds.
    #[serde(default)]
    pub hold_back: Option<f64>,
    /// PART-HOLD-BACK: live edge distance for parts in seconds.
    #[serde(default)]
    pub part_hold_back: Option<f64>,
    /// CAN-BLOCK-RELOAD: server supports blocking playlist reload.
    #[serde(default)]
    pub can_block_reload: bool,
}

/// LL-DASH low-latency parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LowLatencyDashInfo {
    /// availabilityTimeOffset on SegmentTemplate (seconds).
    pub availability_time_offset: f64,
    /// availabilityTimeComplete (false = chunks available before segment complete).
    pub availability_time_complete: bool,
}

/// Part info from source manifest (input side).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourcePartInfo {
    /// Parent segment number (index into segment_urls).
    pub segment_number: u32,
    /// Part index within the segment.
    pub part_index: u32,
    /// Part duration in seconds.
    pub duration: f64,
    /// Whether this part is independent (starts with IDR frame).
    pub independent: bool,
    /// URL to fetch this part.
    pub uri: String,
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
    /// Key rotation: per-period DRM info (indexed by key_period % len).
    #[serde(default)]
    pub rotation_drm_info: Vec<ManifestDrmInfo>,
    /// Clear lead: segment number boundary where encryption starts.
    #[serde(default)]
    pub clear_lead_boundary: Option<u32>,
    /// LL-HLS parts (partial segments).
    #[serde(default)]
    pub parts: Vec<PartInfo>,
    /// LL-HLS part target duration (EXT-X-PART-INF:PART-TARGET).
    #[serde(default)]
    pub part_target_duration: Option<f64>,
    /// LL-HLS server control parameters.
    #[serde(default)]
    pub server_control: Option<ServerControl>,
    /// LL-DASH low-latency parameters.
    #[serde(default)]
    pub ll_dash_info: Option<LowLatencyDashInfo>,
    /// I-frame byte ranges for each video segment (for HLS I-Frame playlists, DASH trick play).
    #[serde(default)]
    pub iframe_segments: Vec<IFrameSegmentInfo>,
    /// Whether trick play / I-frame playlist generation is enabled for this content.
    #[serde(default)]
    pub enable_iframe_playlist: bool,
    /// DVR sliding window duration in seconds. When set and phase is Live,
    /// manifests only render segments within this window from the live edge.
    /// When None, all segments are rendered (EVENT playlist for HLS).
    #[serde(default)]
    pub dvr_window_duration: Option<f64>,
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
            rotation_drm_info: Vec::new(),
            clear_lead_boundary: None,
            parts: Vec::new(),
            part_target_duration: None,
            server_control: None,
            ll_dash_info: None,
            iframe_segments: Vec::new(),
            enable_iframe_playlist: false,
            dvr_window_duration: None,
        }
    }

    pub fn is_complete(&self) -> bool {
        self.phase == ManifestPhase::Complete
    }

    /// Return the slice of segments visible within the DVR window.
    ///
    /// If `dvr_window_duration` is `None` or phase is `Complete`, returns all segments.
    /// Otherwise, returns segments from the live edge backward within the window duration.
    pub fn windowed_segments(&self) -> &[SegmentInfo] {
        match (self.dvr_window_duration, self.phase) {
            (Some(window), ManifestPhase::Live) if window > 0.0 => {
                let mut acc = 0.0;
                let mut start_idx = self.segments.len();
                for (i, seg) in self.segments.iter().enumerate().rev() {
                    acc += seg.duration;
                    if acc > window {
                        start_idx = i + 1;
                        break;
                    }
                    start_idx = i;
                }
                &self.segments[start_idx..]
            }
            _ => &self.segments,
        }
    }

    /// The media_sequence value for the first segment in the DVR window.
    pub fn windowed_media_sequence(&self) -> u32 {
        match (self.dvr_window_duration, self.phase) {
            (Some(window), ManifestPhase::Live) if window > 0.0 => {
                self.windowed_segments()
                    .first()
                    .map(|s| s.number)
                    .unwrap_or(self.media_sequence)
            }
            _ => self.media_sequence,
        }
    }

    /// Return I-frame segment infos filtered to the DVR window.
    pub fn windowed_iframe_segments(&self) -> Vec<&IFrameSegmentInfo> {
        let windowed = self.windowed_segments();
        if windowed.len() == self.segments.len() {
            return self.iframe_segments.iter().collect();
        }
        let first_num = windowed.first().map(|s| s.number).unwrap_or(0);
        let last_num = windowed.last().map(|s| s.number).unwrap_or(0);
        self.iframe_segments
            .iter()
            .filter(|f| f.segment_number >= first_num && f.segment_number <= last_num)
            .collect()
    }

    /// Return parts filtered to the DVR window.
    pub fn windowed_parts(&self) -> Vec<&PartInfo> {
        let windowed = self.windowed_segments();
        if windowed.len() == self.segments.len() {
            return self.parts.iter().collect();
        }
        let first_num = windowed.first().map(|s| s.number).unwrap_or(0);
        let last_num = windowed.last().map(|s| s.number).unwrap_or(0);
        self.parts
            .iter()
            .filter(|p| p.segment_number >= first_num && p.segment_number <= last_num)
            .collect()
    }

    /// Return ad breaks filtered to the DVR window.
    pub fn windowed_ad_breaks(&self) -> Vec<&AdBreakInfo> {
        let windowed = self.windowed_segments();
        if windowed.len() == self.segments.len() {
            return self.ad_breaks.iter().collect();
        }
        let first_num = windowed.first().map(|s| s.number).unwrap_or(0);
        let last_num = windowed.last().map(|s| s.number).unwrap_or(0);
        self.ad_breaks
            .iter()
            .filter(|ab| ab.segment_number >= first_num && ab.segment_number <= last_num)
            .collect()
    }

    /// Whether the DVR window is active (set and phase is Live).
    pub fn is_dvr_active(&self) -> bool {
        matches!(
            (self.dvr_window_duration, self.phase),
            (Some(w), ManifestPhase::Live) if w > 0.0
        )
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
    /// LL-HLS parts parsed from source manifest.
    #[serde(default)]
    pub parts: Vec<SourcePartInfo>,
    /// LL-HLS part target duration.
    #[serde(default)]
    pub part_target_duration: Option<f64>,
    /// LL-HLS server control parameters.
    #[serde(default)]
    pub server_control: Option<ServerControl>,
    /// LL-DASH low-latency parameters.
    #[serde(default)]
    pub ll_dash_info: Option<LowLatencyDashInfo>,
    /// Whether the source uses MPEG-TS segments (detected from .ts extension).
    #[serde(default)]
    pub is_ts_source: bool,
    /// AES-128 key URL for TS segment decryption.
    #[serde(default)]
    pub aes128_key_url: Option<String>,
    /// AES-128 IV for TS segment decryption (hex-decoded from manifest).
    #[serde(default)]
    pub aes128_iv: Option<[u8; 16]>,
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
            key_period: None,
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
            key_period: None,
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
            clearkey_pssh: None,
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
            key_period: None,
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

    #[test]
    fn segment_info_key_period_serde() {
        let seg = SegmentInfo {
            number: 0,
            duration: 6.0,
            uri: "seg.cmfv".into(),
            byte_size: 1024,
            key_period: Some(2),
        };
        let json = serde_json::to_string(&seg).unwrap();
        let parsed: SegmentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.key_period, Some(2));
    }

    #[test]
    fn segment_info_key_period_backward_compat() {
        let json = r#"{"number":0,"duration":6.0,"uri":"seg.cmfv","byte_size":1024}"#;
        let parsed: SegmentInfo = serde_json::from_str(json).unwrap();
        assert!(parsed.key_period.is_none());
    }

    #[test]
    fn manifest_state_rotation_drm_info() {
        let state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        assert!(state.rotation_drm_info.is_empty());
        assert!(state.clear_lead_boundary.is_none());
    }

    #[test]
    fn manifest_drm_info_clearkey_pssh() {
        let drm = ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: None,
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "0123456789abcdef0123456789abcdef".into(),
            clearkey_pssh: Some("CLEARKEY_DATA".into()),
        };
        assert!(drm.clearkey_pssh.is_some());
    }

    #[test]
    fn manifest_state_serde_backward_compat_no_rotation() {
        let json = r#"{"content_id":"c","format":"Hls","phase":"Live","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
        let parsed: ManifestState = serde_json::from_str(json).unwrap();
        assert!(parsed.rotation_drm_info.is_empty());
        assert!(parsed.clear_lead_boundary.is_none());
    }

    // --- LL-HLS / LL-DASH type tests ---

    #[test]
    fn part_info_construction() {
        let part = PartInfo {
            segment_number: 2,
            part_index: 1,
            duration: 0.33334,
            independent: true,
            uri: "/base/part_2.1.cmfv".to_string(),
            byte_size: 5000,
        };
        assert_eq!(part.segment_number, 2);
        assert_eq!(part.part_index, 1);
        assert!((part.duration - 0.33334).abs() < f64::EPSILON);
        assert!(part.independent);
        assert_eq!(part.byte_size, 5000);
    }

    #[test]
    fn part_info_serde_roundtrip() {
        let part = PartInfo {
            segment_number: 0,
            part_index: 3,
            duration: 0.5,
            independent: false,
            uri: "part_0.3.cmfv".to_string(),
            byte_size: 1234,
        };
        let json = serde_json::to_string(&part).unwrap();
        let parsed: PartInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.segment_number, 0);
        assert_eq!(parsed.part_index, 3);
        assert!(!parsed.independent);
    }

    #[test]
    fn server_control_construction_and_serde() {
        let sc = ServerControl {
            can_skip_until: Some(12.0),
            hold_back: Some(9.0),
            part_hold_back: Some(1.0),
            can_block_reload: true,
        };
        let json = serde_json::to_string(&sc).unwrap();
        let parsed: ServerControl = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.can_skip_until, Some(12.0));
        assert_eq!(parsed.hold_back, Some(9.0));
        assert_eq!(parsed.part_hold_back, Some(1.0));
        assert!(parsed.can_block_reload);
    }

    #[test]
    fn server_control_defaults() {
        let json = r#"{}"#;
        let parsed: ServerControl = serde_json::from_str(json).unwrap();
        assert!(parsed.can_skip_until.is_none());
        assert!(parsed.hold_back.is_none());
        assert!(parsed.part_hold_back.is_none());
        assert!(!parsed.can_block_reload);
    }

    #[test]
    fn low_latency_dash_info_serde_roundtrip() {
        let info = LowLatencyDashInfo {
            availability_time_offset: 5.0,
            availability_time_complete: false,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: LowLatencyDashInfo = serde_json::from_str(&json).unwrap();
        assert!((parsed.availability_time_offset - 5.0).abs() < f64::EPSILON);
        assert!(!parsed.availability_time_complete);
    }

    #[test]
    fn source_part_info_serde_roundtrip() {
        let sp = SourcePartInfo {
            segment_number: 1,
            part_index: 0,
            duration: 0.33334,
            independent: true,
            uri: "https://cdn.example.com/part1.0.cmfv".to_string(),
        };
        let json = serde_json::to_string(&sp).unwrap();
        let parsed: SourcePartInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.segment_number, 1);
        assert_eq!(parsed.part_index, 0);
        assert!(parsed.independent);
    }

    #[test]
    fn manifest_state_new_ll_defaults() {
        let state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        assert!(state.parts.is_empty());
        assert!(state.part_target_duration.is_none());
        assert!(state.server_control.is_none());
        assert!(state.ll_dash_info.is_none());
    }

    #[test]
    fn manifest_state_serde_backward_compat_no_ll_fields() {
        let json = r#"{"content_id":"c","format":"Hls","phase":"Live","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
        let parsed: ManifestState = serde_json::from_str(json).unwrap();
        assert!(parsed.parts.is_empty());
        assert!(parsed.part_target_duration.is_none());
        assert!(parsed.server_control.is_none());
        assert!(parsed.ll_dash_info.is_none());
    }

    #[test]
    fn source_manifest_serde_backward_compat_no_ll_fields() {
        let json = r#"{"init_segment_url":"https://cdn.example.com/init.mp4","segment_urls":[],"segment_durations":[],"is_live":false,"source_scheme":null,"ad_breaks":[]}"#;
        let parsed: SourceManifest = serde_json::from_str(json).unwrap();
        assert!(parsed.parts.is_empty());
        assert!(parsed.part_target_duration.is_none());
        assert!(parsed.server_control.is_none());
        assert!(parsed.ll_dash_info.is_none());
    }

    #[test]
    fn source_manifest_serde_backward_compat_no_ts_fields() {
        let json = r#"{"init_segment_url":"https://cdn.example.com/init.mp4","segment_urls":[],"segment_durations":[],"is_live":false,"source_scheme":null,"ad_breaks":[]}"#;
        let parsed: SourceManifest = serde_json::from_str(json).unwrap();
        assert!(!parsed.is_ts_source);
        assert!(parsed.aes128_key_url.is_none());
        assert!(parsed.aes128_iv.is_none());
    }

    #[test]
    fn source_manifest_ts_fields_serde_roundtrip() {
        let manifest = SourceManifest {
            init_segment_url: "".to_string(),
            segment_urls: vec!["https://cdn.example.com/seg0.ts".to_string()],
            segment_durations: vec![10.0],
            is_live: false,
            source_scheme: None,
            ad_breaks: Vec::new(),
            parts: Vec::new(),
            part_target_duration: None,
            server_control: None,
            ll_dash_info: None,
            is_ts_source: true,
            aes128_key_url: Some("https://keys.example.com/key".to_string()),
            aes128_iv: Some([0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                             0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01]),
        };
        let json = serde_json::to_string(&manifest).unwrap();
        let parsed: SourceManifest = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_ts_source);
        assert_eq!(parsed.aes128_key_url.as_deref(), Some("https://keys.example.com/key"));
        assert_eq!(parsed.aes128_iv.unwrap()[15], 0x01);
    }

    // --- I-Frame / Trick Play type tests ---

    #[test]
    fn iframe_segment_info_construction() {
        let info = IFrameSegmentInfo {
            segment_number: 3,
            byte_offset: 0,
            byte_length: 32456,
            duration: 6.006,
            segment_uri: "segment_3.cmfv".to_string(),
        };
        assert_eq!(info.segment_number, 3);
        assert_eq!(info.byte_offset, 0);
        assert_eq!(info.byte_length, 32456);
        assert!((info.duration - 6.006).abs() < f64::EPSILON);
        assert_eq!(info.segment_uri, "segment_3.cmfv");
    }

    #[test]
    fn iframe_segment_info_serde_roundtrip() {
        let info = IFrameSegmentInfo {
            segment_number: 5,
            byte_offset: 128,
            byte_length: 8192,
            duration: 4.0,
            segment_uri: "segment_5.m4s".to_string(),
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: IFrameSegmentInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.segment_number, 5);
        assert_eq!(parsed.byte_offset, 128);
        assert_eq!(parsed.byte_length, 8192);
        assert!((parsed.duration - 4.0).abs() < f64::EPSILON);
        assert_eq!(parsed.segment_uri, "segment_5.m4s");
    }

    #[test]
    fn manifest_state_new_iframe_defaults() {
        let state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        assert!(state.iframe_segments.is_empty());
        assert!(!state.enable_iframe_playlist);
    }

    #[test]
    fn manifest_state_with_iframe_segments() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.enable_iframe_playlist = true;
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: 0,
            byte_offset: 0,
            byte_length: 5000,
            duration: 6.0,
            segment_uri: "segment_0.cmfv".to_string(),
        });
        assert_eq!(state.iframe_segments.len(), 1);
        assert!(state.enable_iframe_playlist);

        // Verify serde roundtrip with iframe fields
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ManifestState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.iframe_segments.len(), 1);
        assert_eq!(parsed.iframe_segments[0].byte_length, 5000);
        assert!(parsed.enable_iframe_playlist);
    }

    #[test]
    fn manifest_state_serde_backward_compat_no_iframe_fields() {
        let json = r#"{"content_id":"c","format":"Hls","phase":"Live","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
        let parsed: ManifestState = serde_json::from_str(json).unwrap();
        assert!(parsed.iframe_segments.is_empty());
        assert!(!parsed.enable_iframe_playlist);
    }

    // --- DVR Window type tests ---

    #[test]
    fn manifest_state_new_dvr_defaults() {
        let state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        assert!(state.dvr_window_duration.is_none());
        assert!(!state.is_dvr_active());
    }

    #[test]
    fn manifest_state_serde_backward_compat_no_dvr_fields() {
        let json = r#"{"content_id":"c","format":"Hls","phase":"Live","init_segment":null,"segments":[],"target_duration":6.0,"variants":[],"drm_info":null,"media_sequence":0,"base_url":"/"}"#;
        let parsed: ManifestState = serde_json::from_str(json).unwrap();
        assert!(parsed.dvr_window_duration.is_none());
    }

    #[test]
    fn dvr_window_duration_serde_roundtrip() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.dvr_window_duration = Some(3600.0);
        let json = serde_json::to_string(&state).unwrap();
        let parsed: ManifestState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.dvr_window_duration, Some(3600.0));
    }

    #[test]
    fn windowed_segments_no_window_returns_all() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        for i in 0..10 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        assert_eq!(state.windowed_segments().len(), 10);
    }

    #[test]
    fn windowed_segments_window_smaller_than_total() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        state.dvr_window_duration = Some(30.0);
        for i in 0..10 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        // 10 segments * 6s = 60s total, window = 30s → last 5 segments
        let windowed = state.windowed_segments();
        assert_eq!(windowed.len(), 5);
        assert_eq!(windowed[0].number, 5);
        assert_eq!(windowed[4].number, 9);
    }

    #[test]
    fn windowed_segments_window_larger_than_total() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        state.dvr_window_duration = Some(3600.0);
        for i in 0..3 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        // 3 segments * 6s = 18s, window = 3600s → all segments
        assert_eq!(state.windowed_segments().len(), 3);
    }

    #[test]
    fn windowed_segments_complete_phase_ignores_window() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Complete;
        state.dvr_window_duration = Some(30.0);
        for i in 0..10 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        // Complete phase ignores window — returns all segments
        assert_eq!(state.windowed_segments().len(), 10);
    }

    #[test]
    fn windowed_media_sequence() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        state.dvr_window_duration = Some(30.0);
        for i in 0..10 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        assert_eq!(state.windowed_media_sequence(), 5);
    }

    #[test]
    fn windowed_media_sequence_no_window() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        for i in 0..10 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        assert_eq!(state.windowed_media_sequence(), 0);
    }

    #[test]
    fn windowed_iframe_segments_filters() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        state.dvr_window_duration = Some(18.0);
        for i in 0..5 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
            state.iframe_segments.push(IFrameSegmentInfo {
                segment_number: i,
                byte_offset: 0,
                byte_length: 500,
                duration: 6.0,
                segment_uri: format!("segment_{i}.cmfv"),
            });
        }
        // Window = 18s → last 3 segments (2,3,4)
        let windowed = state.windowed_iframe_segments();
        assert_eq!(windowed.len(), 3);
        assert_eq!(windowed[0].segment_number, 2);
        assert_eq!(windowed[2].segment_number, 4);
    }

    #[test]
    fn windowed_parts_filters() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        state.dvr_window_duration = Some(12.0);
        for i in 0..4 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
            state.parts.push(PartInfo {
                segment_number: i,
                part_index: 0,
                duration: 0.33,
                independent: true,
                uri: format!("part_{i}.0.cmfv"),
                byte_size: 100,
            });
        }
        // Window = 12s → last 2 segments (2,3)
        let windowed = state.windowed_parts();
        assert_eq!(windowed.len(), 2);
        assert_eq!(windowed[0].segment_number, 2);
    }

    #[test]
    fn windowed_ad_breaks_filters() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        state.phase = ManifestPhase::Live;
        state.dvr_window_duration = Some(12.0);
        for i in 0..4 {
            state.segments.push(SegmentInfo {
                number: i,
                duration: 6.0,
                uri: format!("segment_{i}.cmfv"),
                byte_size: 1024,
                key_period: None,
            });
        }
        state.ad_breaks.push(AdBreakInfo {
            id: 1, presentation_time: 6.0, duration: Some(15.0),
            scte35_cmd: None, segment_number: 1,
        });
        state.ad_breaks.push(AdBreakInfo {
            id: 2, presentation_time: 18.0, duration: None,
            scte35_cmd: None, segment_number: 3,
        });
        // Window = 12s → segments 2,3. Ad break at seg 1 excluded, seg 3 included.
        let windowed = state.windowed_ad_breaks();
        assert_eq!(windowed.len(), 1);
        assert_eq!(windowed[0].id, 2);
    }

    #[test]
    fn is_dvr_active_cases() {
        let mut state = ManifestState::new("c".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        assert!(!state.is_dvr_active()); // None, AwaitingFirstSegment

        state.phase = ManifestPhase::Live;
        assert!(!state.is_dvr_active()); // None, Live

        state.dvr_window_duration = Some(30.0);
        assert!(state.is_dvr_active()); // Some(30), Live

        state.phase = ManifestPhase::Complete;
        assert!(!state.is_dvr_active()); // Some(30), Complete
    }
}
