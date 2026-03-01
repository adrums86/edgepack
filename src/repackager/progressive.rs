use crate::manifest;
use crate::manifest::types::{
    InitSegmentInfo, ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat, SegmentInfo,
    VariantInfo,
};
use crate::media::container::ContainerFormat;

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
        drm_info: Option<ManifestDrmInfo>,
        container_format: ContainerFormat,
    ) -> Self {
        let mut state = ManifestState::new(content_id, format, base_url, container_format);
        state.drm_info = drm_info;

        Self {
            state,
            init_segment_data: None,
            segment_data: Vec::new(),
        }
    }

    /// Set variant/representation info (codec strings, bandwidth, etc.).
    pub fn set_variants(&mut self, variants: Vec<VariantInfo>) {
        self.state.variants = variants;
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
        let ext = self.state.container_format.video_segment_extension();
        let uri = format!("{}segment_{number}{ext}", self.state.base_url);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_drm_info() -> ManifestDrmInfo {
        ManifestDrmInfo {
            encryption_scheme: crate::drm::scheme::EncryptionScheme::Cenc,
            widevine_pssh: Some("WV_PSSH".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "00112233445566778899aabbccddeeff".into(),
        }
    }

    #[test]
    fn new_starts_in_awaiting_state() {
        let po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/base/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        assert_eq!(po.manifest_state().phase, ManifestPhase::AwaitingFirstSegment);
        assert_eq!(po.manifest_state().content_id, "c1");
        assert_eq!(po.manifest_state().format, OutputFormat::Hls);
        assert!(po.manifest_state().drm_info.is_some());
    }

    #[test]
    fn set_init_segment_stores_data() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/base/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00, 0x01, 0x02]);
        assert!(po.init_segment_data().is_some());
        assert_eq!(po.init_segment_data().unwrap(), &[0x00, 0x01, 0x02]);
        assert!(po.manifest_state().init_segment.is_some());
        let init = po.manifest_state().init_segment.as_ref().unwrap();
        assert_eq!(init.uri, "/base/init.mp4");
        assert_eq!(init.byte_size, 3);
    }

    #[test]
    fn add_first_segment_transitions_to_live() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/base/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);

        let manifest = po.add_segment(0, vec![0xAA; 100], 6.006);
        assert!(manifest.is_some());
        assert_eq!(po.manifest_state().phase, ManifestPhase::Live);
        assert_eq!(po.manifest_state().segments.len(), 1);
        assert_eq!(po.manifest_state().segments[0].number, 0);
    }

    #[test]
    fn add_subsequent_segment_stays_live() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/base/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 100], 6.0);
        po.add_segment(1, vec![0xBB; 100], 6.0);

        assert_eq!(po.manifest_state().phase, ManifestPhase::Live);
        assert_eq!(po.manifest_state().segments.len(), 2);
    }

    #[test]
    fn finalize_transitions_to_complete() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/base/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 100], 6.0);

        let manifest = po.finalize();
        assert!(manifest.is_some());
        assert_eq!(po.manifest_state().phase, ManifestPhase::Complete);
        assert!(po.manifest_state().is_complete());
    }

    #[test]
    fn finalize_hls_manifest_contains_endlist() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/base/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 100], 6.0);

        let manifest = po.finalize().unwrap();
        assert!(manifest.contains("#EXT-X-ENDLIST"));
    }

    #[test]
    fn finalize_dash_manifest_static() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Dash,
            "/base/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 100], 6.0);

        let manifest = po.finalize().unwrap();
        assert!(manifest.contains("type=\"static\""));
    }

    #[test]
    fn current_manifest_none_when_awaiting() {
        let po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        assert!(po.current_manifest().is_none());
    }

    #[test]
    fn current_manifest_some_when_live() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 50], 6.0);
        assert!(po.current_manifest().is_some());
    }

    #[test]
    fn segment_data_lookup() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 50], 6.0);
        po.add_segment(1, vec![0xBB; 60], 6.0);

        assert!(po.segment_data(0).is_some());
        assert_eq!(po.segment_data(0).unwrap().len(), 50);
        assert!(po.segment_data(1).is_some());
        assert_eq!(po.segment_data(1).unwrap().len(), 60);
        assert!(po.segment_data(2).is_none());
    }

    #[test]
    fn target_duration_updates_when_longer_segment() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        assert!((po.manifest_state().target_duration - 6.0).abs() < f64::EPSILON);

        po.add_segment(0, vec![0xAA; 50], 8.0);
        assert!((po.manifest_state().target_duration - 8.0).abs() < f64::EPSILON);

        // Shorter segment shouldn't lower target duration
        po.add_segment(1, vec![0xBB; 50], 4.0);
        assert!((po.manifest_state().target_duration - 8.0).abs() < f64::EPSILON);
    }

    #[test]
    fn segment_uri_format() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/repackage/c1/hls/".into(),
            Some(make_drm_info()),
            ContainerFormat::default(),
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(5, vec![0xAA; 50], 6.0);
        assert_eq!(
            po.manifest_state().segments[0].uri,
            "/repackage/c1/hls/segment_5.cmfv"
        );
    }

    #[test]
    fn segment_uri_format_fmp4() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/repackage/c1/hls/".into(),
            Some(make_drm_info()),
            ContainerFormat::Fmp4,
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 50], 6.0);
        assert_eq!(
            po.manifest_state().segments[0].uri,
            "/repackage/c1/hls/segment_0.m4s"
        );
    }

    #[test]
    fn segment_uri_format_iso() {
        let mut po = ProgressiveOutput::new(
            "c1".into(),
            OutputFormat::Hls,
            "/repackage/c1/hls/".into(),
            Some(make_drm_info()),
            ContainerFormat::Iso,
        );
        po.set_init_segment(vec![0x00]);
        po.add_segment(3, vec![0xAA; 50], 6.0);
        assert_eq!(
            po.manifest_state().segments[0].uri,
            "/repackage/c1/hls/segment_3.mp4"
        );
    }

    #[test]
    fn manifest_cache_control_awaiting() {
        let po = ProgressiveOutput::new("c".into(), OutputFormat::Hls, "/".into(), Some(make_drm_info()), ContainerFormat::default());
        assert_eq!(po.manifest_cache_control(31536000, 1), "no-cache");
    }

    #[test]
    fn manifest_cache_control_live() {
        let mut po = ProgressiveOutput::new("c".into(), OutputFormat::Hls, "/".into(), Some(make_drm_info()), ContainerFormat::default());
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 50], 6.0);
        assert_eq!(
            po.manifest_cache_control(31536000, 1),
            "public, max-age=1, s-maxage=1"
        );
    }

    #[test]
    fn manifest_cache_control_complete() {
        let mut po = ProgressiveOutput::new("c".into(), OutputFormat::Hls, "/".into(), Some(make_drm_info()), ContainerFormat::default());
        po.set_init_segment(vec![0x00]);
        po.add_segment(0, vec![0xAA; 50], 6.0);
        po.finalize();
        assert_eq!(
            po.manifest_cache_control(31536000, 1),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn segment_cache_control_always_immutable() {
        assert_eq!(
            ProgressiveOutput::segment_cache_control(31536000),
            "public, max-age=31536000, immutable"
        );
    }

    #[test]
    fn segment_cache_control_custom_max_age() {
        assert_eq!(
            ProgressiveOutput::segment_cache_control(86400),
            "public, max-age=86400, immutable"
        );
    }
}
