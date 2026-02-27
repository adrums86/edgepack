pub mod pipeline;
pub mod progressive;

use crate::manifest::types::OutputFormat;
use serde::{Deserialize, Serialize};

/// A request to repackage content from CBCS to CENC.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepackageRequest {
    /// Unique content identifier.
    pub content_id: String,
    /// URL of the source manifest (HLS or DASH).
    pub source_url: String,
    /// Desired output format.
    pub output_format: OutputFormat,
    /// Optional: specific key IDs to request. If empty, derived from source.
    pub key_ids: Vec<String>,
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
