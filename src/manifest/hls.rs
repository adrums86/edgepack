use crate::error::Result;
use crate::manifest::types::{ManifestPhase, ManifestState};

/// Render an HLS M3U8 manifest from the current state.
///
/// - During `Live` phase: produces a live playlist (no `#EXT-X-ENDLIST`)
/// - During `Complete` phase: produces a VOD playlist (with `#EXT-X-ENDLIST`)
pub fn render(state: &ManifestState) -> Result<String> {
    let mut m3u8 = String::new();

    // Header
    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:7\n");

    // Target duration (rounded up to nearest integer)
    let target_dur = state.target_duration.ceil() as u64;
    m3u8.push_str(&format!("#EXT-X-TARGETDURATION:{target_dur}\n"));

    // Media sequence
    m3u8.push_str(&format!(
        "#EXT-X-MEDIA-SEQUENCE:{}\n",
        state.media_sequence
    ));

    // Playlist type
    match state.phase {
        ManifestPhase::Complete => {
            m3u8.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
        }
        ManifestPhase::Live => {
            m3u8.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        ManifestPhase::AwaitingFirstSegment => {
            // Shouldn't render in this state, but handle gracefully
            return Ok(m3u8);
        }
    }

    // Independent segments (CMAF guarantees this)
    m3u8.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    // DRM signaling — CENC (Widevine + PlayReady)
    if let Some(ref drm) = state.drm_info {
        // Widevine
        if let Some(ref pssh) = drm.widevine_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,\
                 URI=\"data:text/plain;base64,{pssh}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }

        // PlayReady
        if let Some(ref pssh) = drm.playready_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-KEY:METHOD=SAMPLE-AES-CTR,\
                 URI=\"data:text/plain;base64,{pssh}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }
    }

    // Init segment (EXT-X-MAP)
    if let Some(ref init) = state.init_segment {
        m3u8.push_str(&format!("#EXT-X-MAP:URI=\"{}\"\n", init.uri));
    }

    // Segments
    for segment in &state.segments {
        m3u8.push_str(&format!("#EXTINF:{:.6},\n", segment.duration));
        m3u8.push_str(&format!("{}\n", segment.uri));
    }

    // End list for completed manifests
    if state.phase == ManifestPhase::Complete {
        m3u8.push_str("#EXT-X-ENDLIST\n");
    }

    Ok(m3u8)
}

/// Render an HLS master playlist referencing variant streams.
pub fn render_master(state: &ManifestState, variant_playlist_uris: &[String]) -> Result<String> {
    let mut m3u8 = String::new();

    m3u8.push_str("#EXTM3U\n");
    m3u8.push_str("#EXT-X-VERSION:7\n");
    m3u8.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    // Content protection at master level
    if let Some(ref drm) = state.drm_info {
        if let Some(ref pssh) = drm.widevine_pssh {
            m3u8.push_str(&format!(
                "#EXT-X-SESSION-KEY:METHOD=SAMPLE-AES-CTR,\
                 URI=\"data:text/plain;base64,{pssh}\",\
                 KEYID=0x{},\
                 KEYFORMAT=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\",\
                 KEYFORMATVERSIONS=\"1\"\n",
                drm.default_kid
            ));
        }
    }

    for (i, variant) in state.variants.iter().enumerate() {
        let uri = variant_playlist_uris
            .get(i)
            .map(|s| s.as_str())
            .unwrap_or("variant.m3u8");

        match variant.track_type {
            crate::manifest::types::TrackMediaType::Video => {
                let mut attrs = format!("BANDWIDTH={}", variant.bandwidth);
                attrs.push_str(&format!(",CODECS=\"{}\"", variant.codecs));
                if let Some((w, h)) = variant.resolution {
                    attrs.push_str(&format!(",RESOLUTION={w}x{h}"));
                }
                if let Some(fps) = variant.frame_rate {
                    attrs.push_str(&format!(",FRAME-RATE={fps:.3}"));
                }
                m3u8.push_str(&format!("#EXT-X-STREAM-INF:{attrs}\n"));
                m3u8.push_str(&format!("{uri}\n"));
            }
            crate::manifest::types::TrackMediaType::Audio => {
                m3u8.push_str(&format!(
                    "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"{}\",\
                     DEFAULT=YES,AUTOSELECT=YES,URI=\"{uri}\"\n",
                    variant.id
                ));
            }
        }
    }

    Ok(m3u8)
}
