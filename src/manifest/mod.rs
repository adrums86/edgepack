pub mod dash;
pub mod dash_input;
pub mod hls;
pub mod hls_input;
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

/// Render an I-frame / trick play manifest.
///
/// - **HLS**: Returns the I-frame-only playlist (`#EXT-X-I-FRAMES-ONLY`).
/// - **DASH**: Returns `Ok(None)` — trick play is embedded in the regular MPD.
pub fn render_iframe_manifest(state: &ManifestState) -> Result<Option<String>> {
    match state.format {
        OutputFormat::Hls => hls::render_iframe_playlist(state),
        OutputFormat::Dash => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::container::ContainerFormat;
    use types::{ManifestPhase, SegmentInfo, InitSegmentInfo};

    fn make_live_state(format: OutputFormat) -> ManifestState {
        let mut s = ManifestState::new("test".into(), format, "/".into(), ContainerFormat::default());
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
            key_period: None,
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
        let state = ManifestState::new("test".into(), OutputFormat::Hls, "/".into(), ContainerFormat::default());
        let result = render_manifest(&state);
        assert!(result.is_ok());
    }

    #[test]
    fn render_manifest_awaiting_dash_returns_ok() {
        let state = ManifestState::new("test".into(), OutputFormat::Dash, "/".into(), ContainerFormat::default());
        let result = render_manifest(&state);
        assert!(result.is_ok());
    }
}
