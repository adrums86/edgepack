use crate::manifest;
use crate::manifest::types::{
    InitSegmentInfo, ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat, SegmentInfo,
};

/// Progressive output manager.
///
/// Implements the progressive manifest/segment output state machine:
///
/// 1. `AwaitingFirstSegment` — no output yet
/// 2. First segment completes → write init + segment + live manifest → `Live`
/// 3. Subsequent segments → update manifest with new segment → stay `Live`
/// 4. Finalize → write final manifest (HLS ENDLIST / DASH static) → `Complete`
///
/// The caller is responsible for:
/// - Serving the manifest and segments via HTTP with appropriate cache headers
/// - Persisting the state to Redis for cross-request coordination
pub struct ProgressiveOutput {
    state: ManifestState,
    /// Rewritten init segment data (stored until first output).
    init_segment_data: Option<Vec<u8>>,
    /// Rewritten media segment data, indexed by segment number.
    segment_data: Vec<(u32, Vec<u8>)>,
}

impl ProgressiveOutput {
    pub fn new(
        content_id: String,
        format: OutputFormat,
        base_url: String,
        drm_info: ManifestDrmInfo,
    ) -> Self {
        let mut state = ManifestState::new(content_id, format, base_url);
        state.drm_info = Some(drm_info);

        Self {
            state,
            init_segment_data: None,
            segment_data: Vec::new(),
        }
    }

    /// Set the rewritten init segment data.
    pub fn set_init_segment(&mut self, data: Vec<u8>) {
        let uri = format!("{}init.mp4", self.state.base_url);
        self.state.init_segment = Some(InitSegmentInfo {
            uri,
            byte_size: data.len() as u64,
        });
        self.init_segment_data = Some(data);
    }

    /// Add a completed segment, updating the manifest accordingly.
    ///
    /// Returns the current manifest string after the update.
    pub fn add_segment(
        &mut self,
        number: u32,
        data: Vec<u8>,
        duration: f64,
    ) -> Option<String> {
        let uri = format!("{}segment_{number}.cmfv", self.state.base_url);
        let byte_size = data.len() as u64;

        self.state.segments.push(SegmentInfo {
            number,
            duration,
            uri,
            byte_size,
        });

        // Update target duration if this segment is longer
        if duration > self.state.target_duration {
            self.state.target_duration = duration;
        }

        self.segment_data.push((number, data));

        // Transition state
        match self.state.phase {
            ManifestPhase::AwaitingFirstSegment => {
                self.state.phase = ManifestPhase::Live;
            }
            ManifestPhase::Live => {
                // Stay in Live
            }
            ManifestPhase::Complete => {
                // Already complete — shouldn't normally happen
            }
        }

        // Render current manifest
        manifest::render_manifest(&self.state).ok()
    }

    /// Mark the manifest as complete (VOD).
    ///
    /// For HLS: adds `#EXT-X-ENDLIST`
    /// For DASH: changes `type` from `dynamic` to `static`
    pub fn finalize(&mut self) -> Option<String> {
        self.state.phase = ManifestPhase::Complete;
        manifest::render_manifest(&self.state).ok()
    }

    /// Get the current manifest state (for Redis persistence).
    pub fn manifest_state(&self) -> &ManifestState {
        &self.state
    }

    /// Get the init segment data.
    pub fn init_segment_data(&self) -> Option<&[u8]> {
        self.init_segment_data.as_deref()
    }

    /// Get a media segment's data by number.
    pub fn segment_data(&self, number: u32) -> Option<&[u8]> {
        self.segment_data
            .iter()
            .find(|(n, _)| *n == number)
            .map(|(_, data)| data.as_slice())
    }

    /// Get the current manifest as a rendered string.
    pub fn current_manifest(&self) -> Option<String> {
        if self.state.phase == ManifestPhase::AwaitingFirstSegment {
            return None;
        }
        manifest::render_manifest(&self.state).ok()
    }

    /// Determine the appropriate Cache-Control header value for a manifest response.
    pub fn manifest_cache_control(&self, vod_max_age: u64, live_max_age: u64) -> String {
        match self.state.phase {
            ManifestPhase::Complete => {
                format!("public, max-age={vod_max_age}, immutable")
            }
            ManifestPhase::Live => {
                format!("public, max-age={live_max_age}, s-maxage={live_max_age}")
            }
            ManifestPhase::AwaitingFirstSegment => {
                "no-cache".to_string()
            }
        }
    }

    /// Cache-Control for segment responses (always immutable — segments don't change).
    pub fn segment_cache_control(vod_max_age: u64) -> String {
        format!("public, max-age={vod_max_age}, immutable")
    }
}
