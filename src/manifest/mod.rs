pub mod dash;
pub mod hls;
pub mod types;

use crate::error::Result;
use types::{ManifestState, OutputFormat};

/// Generate a manifest string from the current state.
pub fn render_manifest(state: &ManifestState) -> Result<String> {
    match state.format {
        OutputFormat::Hls => hls::render(state),
        OutputFormat::Dash => dash::render(state),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{ManifestPhase, SegmentInfo, InitSegmentInfo};

    fn make_live_state(format: OutputFormat) -> ManifestState {
        let mut s = ManifestState::new("test".into(), format, "/".into());
        s.phase = ManifestPhase::Live;
        s.init_segment = Some(InitSegmentInfo {
            uri: "/init.mp4".into(),
            byte_size: 256,
        });
        s.segments.push(SegmentInfo {
            number: 0,
            duration: 6.0,
            uri: "/segment_0.cmfv".into(),
            byte_size: 1024,
        });
        s
    }

    #[test]
    fn render_manifest_dispatches_to_hls() {
        let state = make_live_state(OutputFormat::Hls);
        let result = render_manifest(&state).unwrap();
        assert!(result.contains("#EXTM3U"));
        assert!(!result.contains("<MPD"));
    }

    #[test]
    fn render_manifest_dispatches_to_dash() {
        let state = make_live_state(OutputFormat::Dash);
        let result = render_manifest(&state).unwrap();
        assert!(result.contains("<MPD"));
        assert!(!result.contains("#EXTM3U"));
    }

    #[test]
    fn render_manifest_awaiting_hls_returns_ok() {
        let state = ManifestState::new("test".into(), OutputFormat::Hls, "/".into());
        let result = render_manifest(&state);
        assert!(result.is_ok());
    }

    #[test]
    fn render_manifest_awaiting_dash_returns_ok() {
        let state = ManifestState::new("test".into(), OutputFormat::Dash, "/".into());
        let result = render_manifest(&state);
        assert!(result.is_ok());
    }
}
