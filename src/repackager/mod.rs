pub mod pipeline;
pub mod progressive;

use crate::drm::scheme::EncryptionScheme;
use crate::manifest::types::OutputFormat;
use crate::media::container::ContainerFormat;
use serde::{Deserialize, Serialize};

/// A raw content encryption key provided directly (bypasses SPEKE).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawKeyEntry {
    /// Key ID (KID) — 16 bytes.
    pub kid: [u8; 16],
    /// Content encryption key (CEK) — 16 bytes (AES-128).
    pub key: [u8; 16],
    /// Optional explicit IV — 16 bytes. If None, IVs come from senc boxes.
    #[serde(default)]
    pub iv: Option<[u8; 16]>,
}

/// Key rotation configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRotationConfig {
    /// Number of segments per key rotation period.
    /// 0 means rotation is disabled.
    pub period_segments: u32,
}

/// A request to repackage content between encryption schemes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepackageRequest {
    /// Unique content identifier.
    pub content_id: String,
    /// URL of the source manifest (HLS or DASH).
    pub source_url: String,
    /// Desired output format.
    pub output_format: OutputFormat,
    /// Target encryption schemes. Multiple schemes produce separate output per scheme.
    /// Default: `[Cenc]` for backward compatibility.
    #[serde(default = "default_target_schemes")]
    pub target_schemes: Vec<EncryptionScheme>,
    /// Target container format (default: CMAF for backward compatibility).
    #[serde(default)]
    pub container_format: ContainerFormat,
    /// Optional: specific key IDs to request. If empty, derived from source.
    pub key_ids: Vec<String>,
    /// Raw encryption keys (bypass SPEKE). If non-empty, SPEKE is skipped.
    #[serde(default)]
    pub raw_keys: Vec<RawKeyEntry>,
    /// Key rotation configuration. If None, no rotation.
    #[serde(default)]
    pub key_rotation: Option<KeyRotationConfig>,
    /// Number of initial segments to leave unencrypted (clear lead).
    /// If None or 0, all segments use the target encryption scheme.
    #[serde(default)]
    pub clear_lead_segments: Option<u32>,
    /// Explicit DRM systems to include in output (overrides auto-detection).
    /// Valid values: "widevine", "playready", "fairplay", "clearkey".
    /// If empty, uses default systems based on target scheme.
    #[serde(default)]
    pub drm_systems: Vec<String>,
    /// Whether to generate I-frame / trick play playlists.
    /// Default: false (opt-in to avoid overhead for clients that don't need trick play).
    #[serde(default)]
    pub enable_iframe_playlist: bool,
    /// DVR sliding window duration in seconds. When set, live manifests only render
    /// segments within this window from the live edge. None = all segments (EVENT playlist).
    #[serde(default)]
    pub dvr_window_duration: Option<f64>,
}

fn default_target_schemes() -> Vec<EncryptionScheme> {
    vec![EncryptionScheme::Cenc]
}

/// Per-content source configuration for JIT packaging.
///
/// Maps a `content_id` to its source parameters, enabling GET-triggered
/// on-demand repackaging without a prior webhook call.
#[cfg(feature = "jit")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    /// URL of the source manifest.
    pub source_url: String,
    /// Target encryption schemes for output.
    #[serde(default = "default_target_schemes")]
    pub target_schemes: Vec<EncryptionScheme>,
    /// Target container format.
    #[serde(default)]
    pub container_format: ContainerFormat,
}

/// Status of a repackaging job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub content_id: String,
    pub format: OutputFormat,
    pub state: JobState,
    pub segments_completed: u32,
    pub segments_total: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobState {
    /// Job is queued but not started.
    Pending,
    /// Fetching DRM keys from license server.
    FetchingKeys,
    /// Processing segments.
    Processing,
    /// All segments processed, manifest finalized.
    Complete,
    /// Job failed.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::container::ContainerFormat;

    #[test]
    fn repackage_request_construction() {
        let req = RepackageRequest {
            content_id: "movie-123".to_string(),
            source_url: "https://cdn.example.com/manifest.m3u8".to_string(),
            output_format: OutputFormat::Hls,
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::default(),
            key_ids: vec!["aabbccdd".to_string()],
            raw_keys: vec![],
            key_rotation: None,
            clear_lead_segments: None,
            drm_systems: vec![],
            enable_iframe_playlist: false,
            dvr_window_duration: None,
        };
        assert_eq!(req.content_id, "movie-123");
        assert_eq!(req.output_format, OutputFormat::Hls);
        assert_eq!(req.target_schemes, vec![EncryptionScheme::Cenc]);
        assert_eq!(req.container_format, ContainerFormat::Cmaf);
        assert_eq!(req.key_ids.len(), 1);
        assert!(req.raw_keys.is_empty());
        assert!(req.key_rotation.is_none());
        assert!(req.clear_lead_segments.is_none());
        assert!(req.drm_systems.is_empty());
    }

    #[test]
    fn repackage_request_serde_roundtrip() {
        let req = RepackageRequest {
            content_id: "test".into(),
            source_url: "https://example.com/source.mpd".into(),
            output_format: OutputFormat::Dash,
            target_schemes: vec![EncryptionScheme::Cbcs],
            container_format: ContainerFormat::Fmp4,
            key_ids: vec![],
            raw_keys: vec![],
            key_rotation: None,
            clear_lead_segments: None,
            drm_systems: vec![],
            enable_iframe_playlist: false,
            dvr_window_duration: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "test");
        assert_eq!(parsed.output_format, OutputFormat::Dash);
        assert_eq!(parsed.target_schemes, vec![EncryptionScheme::Cbcs]);
        assert_eq!(parsed.container_format, ContainerFormat::Fmp4);
        assert!(parsed.key_ids.is_empty());
        assert!(parsed.raw_keys.is_empty());
        assert!(parsed.key_rotation.is_none());
    }

    #[test]
    fn repackage_request_default_target_schemes() {
        // When target_schemes is missing from JSON, it should default to [Cenc]
        let json = r#"{"content_id":"test","source_url":"https://example.com","output_format":"Hls","key_ids":[]}"#;
        let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.target_schemes, vec![EncryptionScheme::Cenc]);
        assert_eq!(parsed.container_format, ContainerFormat::Cmaf);
        assert!(parsed.raw_keys.is_empty());
        assert!(parsed.key_rotation.is_none());
        assert!(parsed.clear_lead_segments.is_none());
        assert!(parsed.drm_systems.is_empty());
    }

    #[test]
    fn repackage_request_multi_scheme() {
        let req = RepackageRequest {
            content_id: "dual".into(),
            source_url: "https://example.com/source.m3u8".into(),
            output_format: OutputFormat::Hls,
            target_schemes: vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
            container_format: ContainerFormat::default(),
            key_ids: vec![],
            raw_keys: vec![],
            key_rotation: None,
            clear_lead_segments: None,
            drm_systems: vec![],
            enable_iframe_playlist: false,
            dvr_window_duration: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.target_schemes.len(), 2);
        assert_eq!(parsed.target_schemes[0], EncryptionScheme::Cenc);
        assert_eq!(parsed.target_schemes[1], EncryptionScheme::Cbcs);
    }

    #[test]
    fn job_status_construction() {
        let status = JobStatus {
            content_id: "c1".into(),
            format: OutputFormat::Hls,
            state: JobState::Processing,
            segments_completed: 5,
            segments_total: Some(10),
        };
        assert_eq!(status.segments_completed, 5);
        assert_eq!(status.segments_total, Some(10));
    }

    #[test]
    fn job_status_serde_roundtrip() {
        let status = JobStatus {
            content_id: "c2".into(),
            format: OutputFormat::Dash,
            state: JobState::Complete,
            segments_completed: 10,
            segments_total: Some(10),
        };
        let json = serde_json::to_string(&status).unwrap();
        let parsed: JobStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.state, JobState::Complete);
        assert_eq!(parsed.segments_completed, 10);
    }

    #[test]
    fn job_state_values() {
        assert_eq!(JobState::Pending, JobState::Pending);
        assert_eq!(JobState::FetchingKeys, JobState::FetchingKeys);
        assert_eq!(JobState::Processing, JobState::Processing);
        assert_eq!(JobState::Complete, JobState::Complete);
        assert_eq!(JobState::Failed, JobState::Failed);
        assert_ne!(JobState::Pending, JobState::Complete);
    }

    #[test]
    fn job_state_serde_roundtrip() {
        for state in [
            JobState::Pending,
            JobState::FetchingKeys,
            JobState::Processing,
            JobState::Complete,
            JobState::Failed,
        ] {
            let json = serde_json::to_string(&state).unwrap();
            let parsed: JobState = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, state);
        }
    }

    #[test]
    fn repackage_request_empty_key_ids() {
        let req = RepackageRequest {
            content_id: "empty".into(),
            source_url: "https://example.com/source".into(),
            output_format: OutputFormat::Hls,
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::default(),
            key_ids: vec![],
            raw_keys: vec![],
            key_rotation: None,
            clear_lead_segments: None,
            drm_systems: vec![],
            enable_iframe_playlist: false,
            dvr_window_duration: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.key_ids.is_empty());
    }

    #[cfg(feature = "jit")]
    #[test]
    fn source_config_construction() {
        let cfg = SourceConfig {
            source_url: "https://origin.example.com/content/manifest.m3u8".into(),
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::Cmaf,
        };
        assert_eq!(cfg.source_url, "https://origin.example.com/content/manifest.m3u8");
        assert_eq!(cfg.target_schemes, vec![EncryptionScheme::Cenc]);
        assert_eq!(cfg.container_format, ContainerFormat::Cmaf);
    }

    #[cfg(feature = "jit")]
    #[test]
    fn source_config_serde_roundtrip() {
        let cfg = SourceConfig {
            source_url: "https://example.com/source.mpd".into(),
            target_schemes: vec![EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
            container_format: ContainerFormat::Fmp4,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: SourceConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.source_url, cfg.source_url);
        assert_eq!(parsed.target_schemes, cfg.target_schemes);
        assert_eq!(parsed.container_format, cfg.container_format);
    }

    #[cfg(feature = "jit")]
    #[test]
    fn source_config_defaults() {
        // When optional fields are missing, defaults should apply
        let json = r#"{"source_url":"https://example.com/source.m3u8"}"#;
        let parsed: SourceConfig = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.target_schemes, vec![EncryptionScheme::Cenc]);
        assert_eq!(parsed.container_format, ContainerFormat::Cmaf);
    }

    #[test]
    fn job_status_no_total() {
        let status = JobStatus {
            content_id: "c3".into(),
            format: OutputFormat::Hls,
            state: JobState::FetchingKeys,
            segments_completed: 0,
            segments_total: None,
        };
        assert!(status.segments_total.is_none());
    }

    #[test]
    fn raw_key_entry_construction() {
        let entry = RawKeyEntry {
            kid: [0xAA; 16],
            key: [0xBB; 16],
            iv: Some([0xCC; 16]),
        };
        assert_eq!(entry.kid, [0xAA; 16]);
        assert_eq!(entry.key, [0xBB; 16]);
        assert_eq!(entry.iv, Some([0xCC; 16]));
    }

    #[test]
    fn raw_key_entry_serde_roundtrip() {
        let entry = RawKeyEntry {
            kid: [0x01; 16],
            key: [0x02; 16],
            iv: Some([0x03; 16]),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: RawKeyEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.kid, entry.kid);
        assert_eq!(parsed.key, entry.key);
        assert_eq!(parsed.iv, entry.iv);
    }

    #[test]
    fn raw_key_entry_no_iv() {
        let entry = RawKeyEntry {
            kid: [0x01; 16],
            key: [0x02; 16],
            iv: None,
        };
        let json = serde_json::to_string(&entry).unwrap();
        let parsed: RawKeyEntry = serde_json::from_str(&json).unwrap();
        assert!(parsed.iv.is_none());
    }

    #[test]
    fn key_rotation_config_serde_roundtrip() {
        let cfg = KeyRotationConfig { period_segments: 10 };
        let json = serde_json::to_string(&cfg).unwrap();
        let parsed: KeyRotationConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.period_segments, 10);
    }

    #[test]
    fn key_rotation_config_zero_means_disabled() {
        let cfg = KeyRotationConfig { period_segments: 0 };
        assert_eq!(cfg.period_segments, 0);
    }

    #[test]
    fn repackage_request_with_raw_keys_serde() {
        let req = RepackageRequest {
            content_id: "raw-test".into(),
            source_url: "https://example.com/source.m3u8".into(),
            output_format: OutputFormat::Hls,
            target_schemes: vec![EncryptionScheme::Cenc],
            container_format: ContainerFormat::default(),
            key_ids: vec![],
            raw_keys: vec![RawKeyEntry {
                kid: [0xAA; 16],
                key: [0xBB; 16],
                iv: None,
            }],
            key_rotation: Some(KeyRotationConfig { period_segments: 5 }),
            clear_lead_segments: Some(2),
            drm_systems: vec!["widevine".into(), "clearkey".into()],
            enable_iframe_playlist: false,
            dvr_window_duration: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.raw_keys.len(), 1);
        assert_eq!(parsed.raw_keys[0].kid, [0xAA; 16]);
        assert_eq!(parsed.key_rotation.unwrap().period_segments, 5);
        assert_eq!(parsed.clear_lead_segments, Some(2));
        assert_eq!(parsed.drm_systems, vec!["widevine", "clearkey"]);
    }

    #[test]
    fn repackage_request_new_fields_default_from_json() {
        // Old JSON without new fields should still parse (backward compat)
        let json = r#"{"content_id":"test","source_url":"https://example.com","output_format":"Hls","key_ids":[]}"#;
        let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
        assert!(parsed.raw_keys.is_empty());
        assert!(parsed.key_rotation.is_none());
        assert!(parsed.clear_lead_segments.is_none());
        assert!(parsed.drm_systems.is_empty());
        assert!(!parsed.enable_iframe_playlist);
        assert!(parsed.dvr_window_duration.is_none());
    }
}
