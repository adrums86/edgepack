pub mod pipeline;
pub mod progressive;

use crate::drm::scheme::EncryptionScheme;
use crate::manifest::types::OutputFormat;
use serde::{Deserialize, Serialize};

/// A request to repackage content between encryption schemes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepackageRequest {
    /// Unique content identifier.
    pub content_id: String,
    /// URL of the source manifest (HLS or DASH).
    pub source_url: String,
    /// Desired output format.
    pub output_format: OutputFormat,
    /// Target encryption scheme (default: CENC for backward compatibility).
    #[serde(default = "default_target_scheme")]
    pub target_scheme: EncryptionScheme,
    /// Optional: specific key IDs to request. If empty, derived from source.
    pub key_ids: Vec<String>,
}

fn default_target_scheme() -> EncryptionScheme {
    EncryptionScheme::Cenc
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

    #[test]
    fn repackage_request_construction() {
        let req = RepackageRequest {
            content_id: "movie-123".to_string(),
            source_url: "https://cdn.example.com/manifest.m3u8".to_string(),
            output_format: OutputFormat::Hls,
            target_scheme: EncryptionScheme::Cenc,
            key_ids: vec!["aabbccdd".to_string()],
        };
        assert_eq!(req.content_id, "movie-123");
        assert_eq!(req.output_format, OutputFormat::Hls);
        assert_eq!(req.target_scheme, EncryptionScheme::Cenc);
        assert_eq!(req.key_ids.len(), 1);
    }

    #[test]
    fn repackage_request_serde_roundtrip() {
        let req = RepackageRequest {
            content_id: "test".into(),
            source_url: "https://example.com/source.mpd".into(),
            output_format: OutputFormat::Dash,
            target_scheme: EncryptionScheme::Cbcs,
            key_ids: vec![],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.content_id, "test");
        assert_eq!(parsed.output_format, OutputFormat::Dash);
        assert_eq!(parsed.target_scheme, EncryptionScheme::Cbcs);
        assert!(parsed.key_ids.is_empty());
    }

    #[test]
    fn repackage_request_default_target_scheme() {
        // When target_scheme is missing from JSON, it should default to Cenc
        let json = r#"{"content_id":"test","source_url":"https://example.com","output_format":"Hls","key_ids":[]}"#;
        let parsed: RepackageRequest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.target_scheme, EncryptionScheme::Cenc);
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
            target_scheme: EncryptionScheme::Cenc,
            key_ids: vec![],
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: RepackageRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.key_ids.is_empty());
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
}
