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
