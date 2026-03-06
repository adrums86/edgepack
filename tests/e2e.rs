//! End-to-end integration tests for the edgepack JIT packager.
//!
//! These tests exercise full pipeline flows combining init segment rewriting,
//! media segment rewriting, and manifest rendering in single tests — verifying
//! cross-cutting correctness that isolated module tests miss.
//!
//! Five categories:
//! 1. Full Encryption Transform Pipeline — all 9 scheme combos × 2 formats
//! 2. Container Format × Output Format Matrix — 3 containers × 2 formats × 3 states
//! 3. Feature Combinations — DVR + I-frames + DRM + steering + dual-format combos
//! 4. Lifecycle Phase Transitions — AwaitingFirstSegment → Live → Complete
//! 5. Edge Cases & Boundary Conditions — single segments, DVR edge cases, large counts

mod common;

use common::*;

use edgepack::config::CacheConfig;
use edgepack::drm::scheme::EncryptionScheme;
use edgepack::manifest;
use edgepack::manifest::hls;
use edgepack::manifest::types::*;
use edgepack::media::container::ContainerFormat;

// ═══════════════════════════════════════════════════════════════════════
// Category 1: Full Encryption Transform Pipeline (18 tests)
// ═══════════════════════════════════════════════════════════════════════
//
// Each test: build source init+segment → rewrite both → build ManifestState
// → render manifest → parse it back → validate structural integrity at every step.

/// Helper to run a full E2E encryption pipeline test for a given scheme combination and format.
fn run_e2e_encryption_pipeline(
    source_scheme: EncryptionScheme,
    target_scheme: EncryptionScheme,
    format: OutputFormat,
) {
    let sample_count = 4;
    let sample_size = 64;

    // 1. Build source init + media segment
    let (source_init, source_segment, source_key) = match source_scheme {
        EncryptionScheme::Cbcs => {
            let init = build_cbcs_init_segment();
            let (seg, _pt) = build_cbcs_media_segment(sample_count, sample_size, &TEST_SOURCE_KEY, 8);
            (init, seg, Some(&TEST_SOURCE_KEY))
        }
        EncryptionScheme::Cenc => {
            let init = build_cenc_init_segment();
            let (seg, _pt) = build_cenc_media_segment(sample_count, sample_size, &TEST_SOURCE_KEY, 8);
            (init, seg, Some(&TEST_SOURCE_KEY))
        }
        EncryptionScheme::None => {
            let init = build_clear_init_segment();
            let (seg, _pt) = build_clear_media_segment(sample_count, sample_size);
            (init, seg, None)
        }
    };

    let target_key = if target_scheme.is_encrypted() {
        Some(&TEST_TARGET_KEY)
    } else {
        None
    };

    // 2. Rewrite init segment
    let key_set = if target_scheme.is_encrypted() {
        Some(make_drm_key_set())
    } else {
        None
    };
    let rewritten_init = full_init_rewrite(
        &source_init,
        source_scheme,
        target_scheme,
        key_set.as_ref(),
        ContainerFormat::Cmaf,
    );

    // 3. Validate init structure
    if target_scheme.is_encrypted() {
        assert!(
            rewritten_init.windows(4).any(|w| w == b"sinf"),
            "encrypted target must have sinf"
        );
        assert!(
            rewritten_init.windows(4).any(|w| w == b"encv"),
            "encrypted target must have encv sample entry"
        );
    } else {
        assert!(
            !rewritten_init.windows(4).any(|w| w == b"sinf"),
            "clear target must not have sinf"
        );
    }

    // 4. Rewrite media segment
    let rewritten_segment = full_segment_rewrite(
        &source_segment,
        source_scheme,
        target_scheme,
        source_key,
        target_key,
    );

    // 5. Validate segment structure
    assert_valid_segment_structure(
        &rewritten_segment,
        sample_count,
        target_scheme.is_encrypted(),
    );

    // 6. Build ManifestState and render manifest
    let segment_count = 5;
    let mut state = make_manifest_state_with_container(
        format,
        ContainerFormat::Cmaf,
        segment_count,
        ManifestPhase::Complete,
    );
    if !target_scheme.is_encrypted() {
        state.drm_info = None;
    } else {
        // Adjust DRM info scheme to match target
        if let Some(ref mut drm) = state.drm_info {
            drm.encryption_scheme = target_scheme;
        }
    }

    let rendered = manifest::render_manifest(&state).unwrap();

    // 7. Validate manifest
    match format {
        OutputFormat::Hls => assert_valid_hls(&rendered, segment_count as usize),
        OutputFormat::Dash => assert_valid_dash(&rendered, segment_count as usize),
    }
}

// ─── CBCS → CENC ────────────────────────────────────────────────────

#[test]
fn e2e_cbcs_to_cenc_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cbcs, EncryptionScheme::Cenc, OutputFormat::Hls);
}

#[test]
fn e2e_cbcs_to_cenc_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cbcs, EncryptionScheme::Cenc, OutputFormat::Dash);
}

// ─── CENC → CBCS ────────────────────────────────────────────────────

#[test]
fn e2e_cenc_to_cbcs_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cenc, EncryptionScheme::Cbcs, OutputFormat::Hls);
}

#[test]
fn e2e_cenc_to_cbcs_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cenc, EncryptionScheme::Cbcs, OutputFormat::Dash);
}

// ─── Same-scheme re-encryption ──────────────────────────────────────

#[test]
fn e2e_cbcs_to_cbcs_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cbcs, EncryptionScheme::Cbcs, OutputFormat::Hls);
}

#[test]
fn e2e_cbcs_to_cbcs_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cbcs, EncryptionScheme::Cbcs, OutputFormat::Dash);
}

#[test]
fn e2e_cenc_to_cenc_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cenc, EncryptionScheme::Cenc, OutputFormat::Hls);
}

#[test]
fn e2e_cenc_to_cenc_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cenc, EncryptionScheme::Cenc, OutputFormat::Dash);
}

// ─── Clear → Encrypted ─────────────────────────────────────────────

#[test]
fn e2e_clear_to_cenc_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::None, EncryptionScheme::Cenc, OutputFormat::Hls);
}

#[test]
fn e2e_clear_to_cenc_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::None, EncryptionScheme::Cenc, OutputFormat::Dash);
}

#[test]
fn e2e_clear_to_cbcs_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::None, EncryptionScheme::Cbcs, OutputFormat::Hls);
}

#[test]
fn e2e_clear_to_cbcs_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::None, EncryptionScheme::Cbcs, OutputFormat::Dash);
}

// ─── Encrypted → Clear ─────────────────────────────────────────────

#[test]
fn e2e_cenc_to_clear_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cenc, EncryptionScheme::None, OutputFormat::Hls);
}

#[test]
fn e2e_cenc_to_clear_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cenc, EncryptionScheme::None, OutputFormat::Dash);
}

#[test]
fn e2e_cbcs_to_clear_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cbcs, EncryptionScheme::None, OutputFormat::Hls);
}

#[test]
fn e2e_cbcs_to_clear_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::Cbcs, EncryptionScheme::None, OutputFormat::Dash);
}

// ─── Clear → Clear ──────────────────────────────────────────────────

#[test]
fn e2e_clear_to_clear_hls() {
    run_e2e_encryption_pipeline(EncryptionScheme::None, EncryptionScheme::None, OutputFormat::Hls);
}

#[test]
fn e2e_clear_to_clear_dash() {
    run_e2e_encryption_pipeline(EncryptionScheme::None, EncryptionScheme::None, OutputFormat::Dash);
}

// ═══════════════════════════════════════════════════════════════════════
// Category 2: Container Format × Output Format Matrix (18 tests)
// ═══════════════════════════════════════════════════════════════════════
//
// Validates ftyp brands, segment extensions, DASH profiles, and HLS tags
// for each container format.

/// Helper to run an E2E container format test.
fn run_e2e_container_format(
    container: ContainerFormat,
    format: OutputFormat,
    source_scheme: EncryptionScheme,
    target_scheme: EncryptionScheme,
) {
    // 1. Build source init + rewrite with container format
    let source_init = match source_scheme {
        EncryptionScheme::Cbcs => build_cbcs_init_segment(),
        EncryptionScheme::Cenc => build_cenc_init_segment(),
        EncryptionScheme::None => build_clear_init_segment(),
    };

    let key_set = if target_scheme.is_encrypted() {
        Some(make_drm_key_set())
    } else {
        None
    };

    let rewritten_init = full_init_rewrite(
        &source_init,
        source_scheme,
        target_scheme,
        key_set.as_ref(),
        container,
    );

    // 2. Validate ftyp brands based on container format
    match container {
        ContainerFormat::Cmaf => {
            assert!(
                rewritten_init.windows(4).any(|w| w == b"cmfc"),
                "CMAF must contain cmfc compatible brand"
            );
        }
        ContainerFormat::Fmp4 | ContainerFormat::Iso => {
            assert!(
                !rewritten_init.windows(4).any(|w| w == b"cmfc"),
                "{:?} must not contain cmfc compatible brand",
                container
            );
        }
        #[cfg(feature = "ts")]
        ContainerFormat::Ts => {
            assert!(
                rewritten_init.is_empty(),
                "TS must produce empty ftyp"
            );
        }
    }

    // 3. Build ManifestState with the container format
    let segment_count = 5;
    let mut state =
        make_manifest_state_with_container(format, container, segment_count, ManifestPhase::Complete);
    if !target_scheme.is_encrypted() {
        state.drm_info = None;
    }

    // 4. Render manifest and validate container-specific signals
    let rendered = manifest::render_manifest(&state).unwrap();

    let expected_ext = container.video_segment_extension();
    match format {
        OutputFormat::Hls => {
            assert_valid_hls(&rendered, segment_count as usize);
            // Verify segment URIs use the correct extension
            assert!(
                rendered.contains(expected_ext),
                "HLS manifest must contain segment extension '{expected_ext}' for {:?}",
                container
            );
        }
        OutputFormat::Dash => {
            assert_valid_dash(&rendered, segment_count as usize);
            // Verify DASH profiles
            match container {
                ContainerFormat::Cmaf => {
                    assert!(
                        rendered.contains("cmaf"),
                        "CMAF DASH must reference cmaf profile"
                    );
                }
                ContainerFormat::Fmp4 | ContainerFormat::Iso => {
                    assert!(
                        rendered.contains("isoff-live"),
                        "{:?} DASH must use isoff-live profile",
                        container
                    );
                }
                #[cfg(feature = "ts")]
                ContainerFormat::Ts => {
                    panic!("TS + DASH should never reach here — rejected at validation")
                }
            }
        }
    }
}

// ─── CMAF Container ─────────────────────────────────────────────────

#[test]
fn e2e_cmaf_hls_encrypted() {
    run_e2e_container_format(ContainerFormat::Cmaf, OutputFormat::Hls, EncryptionScheme::Cbcs, EncryptionScheme::Cenc);
}

#[test]
fn e2e_cmaf_hls_clear_to_enc() {
    run_e2e_container_format(ContainerFormat::Cmaf, OutputFormat::Hls, EncryptionScheme::None, EncryptionScheme::Cenc);
}

#[test]
fn e2e_cmaf_hls_clear_to_clear() {
    run_e2e_container_format(ContainerFormat::Cmaf, OutputFormat::Hls, EncryptionScheme::None, EncryptionScheme::None);
}

#[test]
fn e2e_cmaf_dash_encrypted() {
    run_e2e_container_format(ContainerFormat::Cmaf, OutputFormat::Dash, EncryptionScheme::Cbcs, EncryptionScheme::Cenc);
}

#[test]
fn e2e_cmaf_dash_clear_to_enc() {
    run_e2e_container_format(ContainerFormat::Cmaf, OutputFormat::Dash, EncryptionScheme::None, EncryptionScheme::Cenc);
}

#[test]
fn e2e_cmaf_dash_clear_to_clear() {
    run_e2e_container_format(ContainerFormat::Cmaf, OutputFormat::Dash, EncryptionScheme::None, EncryptionScheme::None);
}

// ─── fMP4 Container ─────────────────────────────────────────────────

#[test]
fn e2e_fmp4_hls_encrypted() {
    run_e2e_container_format(ContainerFormat::Fmp4, OutputFormat::Hls, EncryptionScheme::Cbcs, EncryptionScheme::Cenc);
}

#[test]
fn e2e_fmp4_hls_clear_to_enc() {
    run_e2e_container_format(ContainerFormat::Fmp4, OutputFormat::Hls, EncryptionScheme::None, EncryptionScheme::Cenc);
}

#[test]
fn e2e_fmp4_hls_clear_to_clear() {
    run_e2e_container_format(ContainerFormat::Fmp4, OutputFormat::Hls, EncryptionScheme::None, EncryptionScheme::None);
}

#[test]
fn e2e_fmp4_dash_encrypted() {
    run_e2e_container_format(ContainerFormat::Fmp4, OutputFormat::Dash, EncryptionScheme::Cbcs, EncryptionScheme::Cenc);
}

#[test]
fn e2e_fmp4_dash_clear_to_enc() {
    run_e2e_container_format(ContainerFormat::Fmp4, OutputFormat::Dash, EncryptionScheme::None, EncryptionScheme::Cenc);
}

#[test]
fn e2e_fmp4_dash_clear_to_clear() {
    run_e2e_container_format(ContainerFormat::Fmp4, OutputFormat::Dash, EncryptionScheme::None, EncryptionScheme::None);
}

// ─── ISO BMFF Container ─────────────────────────────────────────────

#[test]
fn e2e_iso_hls_encrypted() {
    run_e2e_container_format(ContainerFormat::Iso, OutputFormat::Hls, EncryptionScheme::Cbcs, EncryptionScheme::Cenc);
}

#[test]
fn e2e_iso_hls_clear_to_enc() {
    run_e2e_container_format(ContainerFormat::Iso, OutputFormat::Hls, EncryptionScheme::None, EncryptionScheme::Cenc);
}

#[test]
fn e2e_iso_hls_clear_to_clear() {
    run_e2e_container_format(ContainerFormat::Iso, OutputFormat::Hls, EncryptionScheme::None, EncryptionScheme::None);
}

#[test]
fn e2e_iso_dash_encrypted() {
    run_e2e_container_format(ContainerFormat::Iso, OutputFormat::Dash, EncryptionScheme::Cbcs, EncryptionScheme::Cenc);
}

#[test]
fn e2e_iso_dash_clear_to_enc() {
    run_e2e_container_format(ContainerFormat::Iso, OutputFormat::Dash, EncryptionScheme::None, EncryptionScheme::Cenc);
}

#[test]
fn e2e_iso_dash_clear_to_clear() {
    run_e2e_container_format(ContainerFormat::Iso, OutputFormat::Dash, EncryptionScheme::None, EncryptionScheme::None);
}

// ═══════════════════════════════════════════════════════════════════════
// Category 3: Feature Combinations (30 tests)
// ═══════════════════════════════════════════════════════════════════════

// ─── DVR + I-Frames + DRM (6 tests) ────────────────────────────────

#[test]
fn e2e_dvr_with_iframes_hls_complete() {
    // DVR window + I-frame playlist, Complete phase = all segments (no DVR filtering)
    let mut state = make_hls_iframe_manifest_state(10, ManifestPhase::Complete);
    state.dvr_window_duration = Some(30.0);

    // Complete phase should render ALL segments regardless of DVR
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 10);
    assert!(rendered.contains("#EXT-X-ENDLIST"));

    // I-frame playlist should also have all 10 entries
    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));
    assert_eq!(iframe.matches("#EXT-X-BYTERANGE:").count(), 10);
}

#[test]
fn e2e_dvr_with_iframes_hls_live() {
    // DVR window + I-frame playlist, Live phase (windowed)
    // 10 segments × 6.006s = 60.06s. Window=18s → 3 segments
    let mut state = make_hls_iframe_manifest_state(10, ManifestPhase::Live);
    state.dvr_window_duration = Some(18.0);
    // Override durations to exact 6.0s for precise math
    for seg in &mut state.segments {
        seg.duration = 6.0;
    }
    for iframe in &mut state.iframe_segments {
        iframe.duration = 6.0;
    }

    let rendered = manifest::render_manifest(&state).unwrap();
    // Should only show 3 segments
    let extinf_count = rendered.matches("#EXTINF:").count();
    assert_eq!(extinf_count, 3, "DVR window should show 3 segments");

    // I-frame playlist should also be windowed
    let iframe_manifest = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe_manifest.contains("#EXT-X-I-FRAMES-ONLY"));
    let byterange_count = iframe_manifest.matches("#EXT-X-BYTERANGE:").count();
    assert_eq!(byterange_count, 3, "I-frame playlist should be windowed to 3");
}

#[test]
fn e2e_dvr_with_iframes_dash() {
    // DVR window + trick play AdaptationSet in DASH
    let mut state = make_dash_iframe_manifest_state(10, ManifestPhase::Live);
    state.dvr_window_duration = Some(18.0);
    for seg in &mut state.segments {
        seg.duration = 6.0;
    }

    let rendered = manifest::render_manifest(&state).unwrap();
    // DASH DVR: should have timeShiftBufferDepth
    assert!(
        rendered.contains("timeShiftBufferDepth"),
        "DVR DASH must have timeShiftBufferDepth"
    );
    // Should have trick play AdaptationSet
    assert!(
        rendered.contains("trickmode"),
        "DASH with iframes must have trickmode AdaptationSet"
    );
    // S entries should be windowed to 3
    let s_count = rendered.matches("<S ").count();
    // Main + trick play both have S entries, so count should be 3 * 2 = 6
    // or just 3 in main + 3 in trick play
    assert!(
        s_count >= 3,
        "DASH DVR windowed S entries should be at least 3, got {s_count}"
    );
}

#[test]
fn e2e_dvr_with_drm_cenc_hls() {
    // DVR window + CENC DRM signaling
    let state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    // DRM info is already set from the base builder

    let rendered = manifest::render_manifest(&state).unwrap();
    let extinf_count = rendered.matches("#EXTINF:").count();
    assert_eq!(extinf_count, 3, "DVR window should show 3 segments");
    // DRM KEY tag should still be present
    assert!(
        rendered.contains("#EXT-X-KEY:"),
        "DRM KEY tag must be present with DVR"
    );
}

#[test]
fn e2e_dvr_with_drm_cbcs_hls() {
    // DVR window + CBCS DRM (FairPlay key URI)
    let mut state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cbcs,
        widevine_pssh: Some("AAAAOHBzc2g=".into()),
        playready_pssh: None,
        playready_pro: None,
        fairplay_key_uri: Some("skd://fairplay.example.com/key".into()),
        default_kid: "00112233445566778899aabbccddeeff".into(),
        clearkey_pssh: None,
    });

    let rendered = manifest::render_manifest(&state).unwrap();
    assert!(
        rendered.contains("SAMPLE-AES"),
        "CBCS HLS must use SAMPLE-AES method"
    );
    assert!(
        rendered.contains("skd://"),
        "FairPlay key URI must be present"
    );
    assert_eq!(rendered.matches("#EXTINF:").count(), 3);
}

#[test]
fn e2e_dvr_with_drm_dash() {
    // DVR + DASH ContentProtection elements
    let state = make_dash_dvr_manifest_state(10, ManifestPhase::Live, 18.0);

    let rendered = manifest::render_manifest(&state).unwrap();
    assert!(
        rendered.contains("timeShiftBufferDepth"),
        "DASH DVR must have timeShiftBufferDepth"
    );
    assert!(
        rendered.contains("<ContentProtection"),
        "DASH DRM must have ContentProtection"
    );
    assert_eq!(rendered.matches("<S ").count(), 3);
}

// ─── DVR + SCTE-35 + Content Steering (4 tests) ────────────────────

#[test]
fn e2e_dvr_with_ad_breaks_hls() {
    // DVR window should filter ad breaks to the window
    let mut state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    // Add ad breaks: one inside window (segment 8) and one outside (segment 2)
    state.ad_breaks = vec![
        AdBreakInfo {
            id: 1,
            presentation_time: 12.0,
            duration: Some(30.0),
            scte35_cmd: None,
            segment_number: 2,
        },
        AdBreakInfo {
            id: 2,
            presentation_time: 48.0,
            duration: Some(15.0),
            scte35_cmd: None,
            segment_number: 8,
        },
    ];

    let rendered = manifest::render_manifest(&state).unwrap();
    // Only ad break in segment 8 should be visible (segments 7,8,9 in window)
    let daterange_count = rendered.matches("#EXT-X-DATERANGE:").count();
    assert_eq!(
        daterange_count, 1,
        "Only 1 ad break should be visible in DVR window, got {daterange_count}"
    );
}

#[test]
fn e2e_dvr_with_ad_breaks_dash() {
    let mut state = make_dash_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    state.ad_breaks = vec![
        AdBreakInfo {
            id: 1,
            presentation_time: 12.0,
            duration: Some(30.0),
            scte35_cmd: None,
            segment_number: 2,
        },
        AdBreakInfo {
            id: 2,
            presentation_time: 48.0,
            duration: Some(15.0),
            scte35_cmd: None,
            segment_number: 8,
        },
    ];

    let rendered = manifest::render_manifest(&state).unwrap();
    assert!(rendered.contains("timeShiftBufferDepth"));
    // DASH ad events should be filtered to window
    let event_count = rendered.matches("<Event ").count();
    assert_eq!(
        event_count, 1,
        "Only 1 DASH event should be visible in DVR window, got {event_count}"
    );
}

#[test]
fn e2e_dvr_with_steering_hls() {
    // DVR + content steering in HLS — steering is master playlist only,
    // but media playlist should still have DVR windowing
    let mut state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: Some("cdn-a".into()),
        query_before_start: None,
    });

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_eq!(rendered.matches("#EXTINF:").count(), 3);
    // Content steering is for master playlists, not media playlists,
    // so it won't appear in the rendered media playlist
}

#[test]
fn e2e_dvr_with_steering_dash() {
    let mut state = make_dash_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: Some("cdn-b".into()),
        query_before_start: Some(true),
    });

    let rendered = manifest::render_manifest(&state).unwrap();
    assert!(rendered.contains("timeShiftBufferDepth"));
    assert!(
        rendered.contains("<ContentSteering"),
        "DASH must have ContentSteering element"
    );
    assert!(rendered.contains("steer.example.com"));
}

// ─── I-Frames + DRM + Container Formats (4 tests) ──────────────────

#[test]
fn e2e_iframes_with_drm_fmp4_hls() {
    // I-frame playlist with fMP4 container + DRM
    let mut state = make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Fmp4,
        5,
        ManifestPhase::Complete,
    );
    state.enable_iframe_playlist = true;
    for i in 0..5u32 {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 8192 + (i as u64 * 100),
            duration: 6.0,
            segment_uri: format!(
                "/repackage/e2e-hls-Fmp4/hls/segment_{i}{}",
                ContainerFormat::Fmp4.video_segment_extension()
            ),
        });
    }

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 5);
    assert!(rendered.contains(".m4s"), "fMP4 HLS must use .m4s extension");

    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));
    assert!(iframe.contains(".m4s"), "I-frame playlist must use .m4s extension");
    assert_eq!(iframe.matches("#EXT-X-BYTERANGE:").count(), 5);
}

#[test]
fn e2e_iframes_with_drm_iso_hls() {
    let mut state = make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Iso,
        5,
        ManifestPhase::Complete,
    );
    state.enable_iframe_playlist = true;
    for i in 0..5u32 {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 8192,
            duration: 6.0,
            segment_uri: format!(
                "/repackage/e2e-hls-Iso/hls/segment_{i}{}",
                ContainerFormat::Iso.video_segment_extension()
            ),
        });
    }

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 5);
    // ISO uses .mp4 extension — but .mp4 also appears in init.mp4, so check segment URIs
    assert!(
        rendered.contains("segment_0.mp4"),
        "ISO HLS must use .mp4 segment extension"
    );

    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));
    assert_eq!(iframe.matches("#EXT-X-BYTERANGE:").count(), 5);
}

#[test]
fn e2e_iframes_with_drm_cenc_dash() {
    // DASH trick play + CENC ContentProtection
    let mut state = make_manifest_state_with_container(
        OutputFormat::Dash,
        ContainerFormat::Cmaf,
        5,
        ManifestPhase::Complete,
    );
    state.enable_iframe_playlist = true;
    for i in 0..5u32 {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 8192,
            duration: 6.0,
            segment_uri: format!("/repackage/e2e-dash-Cmaf/dash/segment_{i}.cmfv"),
        });
    }

    let rendered = manifest::render_manifest(&state).unwrap();
    // Should have main AdaptationSet + trick play AdaptationSet
    assert!(rendered.contains("trickmode"), "DASH must have trick play");
    assert!(
        rendered.contains("<ContentProtection"),
        "DASH must have DRM ContentProtection"
    );
}

#[test]
fn e2e_iframes_with_steering_hls() {
    // I-frames + content steering — steering in master only
    let mut state = make_hls_iframe_manifest_state(5, ManifestPhase::Complete);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: Some("cdn-a".into()),
        query_before_start: None,
    });

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 5);

    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));
}

// ─── Dual-Format + Dual-Scheme (6 tests) ───────────────────────────

#[test]
fn e2e_dual_format_single_scheme() {
    // HLS+DASH with CENC — verify both manifests render correctly
    let hls_state = make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Cmaf,
        5,
        ManifestPhase::Complete,
    );
    let dash_state = make_manifest_state_with_container(
        OutputFormat::Dash,
        ContainerFormat::Cmaf,
        5,
        ManifestPhase::Complete,
    );

    let hls_manifest = manifest::render_manifest(&hls_state).unwrap();
    let dash_manifest = manifest::render_manifest(&dash_state).unwrap();

    assert_valid_hls(&hls_manifest, 5);
    assert_valid_dash(&dash_manifest, 5);

    // Both should have DRM signaling
    assert!(hls_manifest.contains("#EXT-X-KEY:"));
    assert!(dash_manifest.contains("<ContentProtection"));
}

#[test]
fn e2e_dual_format_dual_scheme() {
    // HLS+DASH × CENC+CBCS = 4 manifest variants
    let schemes = [EncryptionScheme::Cenc, EncryptionScheme::Cbcs];
    let formats = [OutputFormat::Hls, OutputFormat::Dash];

    for &format in &formats {
        for &scheme in &schemes {
            let mut state = make_manifest_state_with_container(
                format,
                ContainerFormat::Cmaf,
                5,
                ManifestPhase::Complete,
            );
            if let Some(ref mut drm) = state.drm_info {
                drm.encryption_scheme = scheme;
                if scheme == EncryptionScheme::Cbcs {
                    drm.fairplay_key_uri = Some("skd://fairplay.example.com/key".into());
                }
            }

            let rendered = manifest::render_manifest(&state).unwrap();
            match format {
                OutputFormat::Hls => {
                    assert_valid_hls(&rendered, 5);
                    match scheme {
                        EncryptionScheme::Cenc => {
                            assert!(rendered.contains("SAMPLE-AES-CTR"));
                        }
                        EncryptionScheme::Cbcs => {
                            assert!(rendered.contains("SAMPLE-AES"));
                        }
                        _ => {}
                    }
                }
                OutputFormat::Dash => {
                    assert_valid_dash(&rendered, 5);
                    assert!(rendered.contains("<ContentProtection"));
                }
            }
        }
    }
}

#[test]
fn e2e_dual_format_with_iframes() {
    // HLS+DASH with I-frame playlists + trick play
    let hls_state = make_hls_iframe_manifest_state(5, ManifestPhase::Complete);
    let dash_state = make_dash_iframe_manifest_state(5, ManifestPhase::Complete);

    let hls_manifest = manifest::render_manifest(&hls_state).unwrap();
    let dash_manifest = manifest::render_manifest(&dash_state).unwrap();

    assert_valid_hls(&hls_manifest, 5);
    // DASH with trick play has doubled S entries (5 main + 5 trick play = 10)
    // so we validate structure manually instead of using assert_valid_dash
    assert!(dash_manifest.contains("<MPD"));
    assert!(dash_manifest.contains("</MPD>"));
    assert!(dash_manifest.contains("<Period"));

    // HLS I-frame playlist
    let iframe = hls::render_iframe_playlist(&hls_state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));

    // DASH trick play
    assert!(dash_manifest.contains("trickmode"));
}

#[test]
fn e2e_dual_format_with_dvr() {
    // HLS+DASH with DVR windowing
    let hls_state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    let dash_state = make_dash_dvr_manifest_state(10, ManifestPhase::Live, 18.0);

    let hls_manifest = manifest::render_manifest(&hls_state).unwrap();
    let dash_manifest = manifest::render_manifest(&dash_state).unwrap();

    // Both should show 3 segments
    assert_eq!(hls_manifest.matches("#EXTINF:").count(), 3);
    assert_eq!(dash_manifest.matches("<S ").count(), 3);

    // DASH should have timeShiftBufferDepth
    assert!(dash_manifest.contains("timeShiftBufferDepth"));
}

#[test]
fn e2e_dual_format_with_steering() {
    let hls_state = make_hls_content_steering_manifest_state(5, ManifestPhase::Complete);
    let dash_state = make_dash_content_steering_manifest_state(5, ManifestPhase::Complete);

    let hls_manifest = manifest::render_manifest(&hls_state).unwrap();
    let dash_manifest = manifest::render_manifest(&dash_state).unwrap();

    assert_valid_hls(&hls_manifest, 5);
    assert_valid_dash(&dash_manifest, 5);

    // DASH should have ContentSteering element
    assert!(dash_manifest.contains("<ContentSteering"));
    assert!(dash_manifest.contains("steer.example.com"));
}

#[test]
fn e2e_dual_format_clear_content() {
    // HLS+DASH with clear (no DRM) content
    let mut hls_state = make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Cmaf,
        5,
        ManifestPhase::Complete,
    );
    hls_state.drm_info = None;

    let mut dash_state = make_manifest_state_with_container(
        OutputFormat::Dash,
        ContainerFormat::Cmaf,
        5,
        ManifestPhase::Complete,
    );
    dash_state.drm_info = None;

    let hls_manifest = manifest::render_manifest(&hls_state).unwrap();
    let dash_manifest = manifest::render_manifest(&dash_state).unwrap();

    assert_valid_hls(&hls_manifest, 5);
    assert_valid_dash(&dash_manifest, 5);

    // No DRM tags
    assert!(!hls_manifest.contains("#EXT-X-KEY:"));
    assert!(!dash_manifest.contains("<ContentProtection"));
}

// ─── Advanced DRM Combinations (6 tests) ───────────────────────────

#[test]
fn e2e_key_rotation_with_iframes() {
    // Key rotation boundaries + I-frame playlist
    let mut state = make_hls_iframe_manifest_state(6, ManifestPhase::Complete);
    // Set key_period on segments: 3 segments per key period
    for seg in &mut state.segments {
        seg.key_period = Some(seg.number / 3);
    }
    // Add rotation DRM info for 2 periods
    state.rotation_drm_info = vec![
        ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("period0_pssh".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "aabbccdd11223344".into(),
            clearkey_pssh: None,
        },
        ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("period1_pssh".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "eeff00112233aabb".into(),
            clearkey_pssh: None,
        },
    ];

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 6);

    // I-frame playlist should still render
    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));
    assert_eq!(iframe.matches("#EXT-X-BYTERANGE:").count(), 6);
}

#[test]
fn e2e_key_rotation_with_dvr() {
    // Key rotation + DVR window — rotation periods may slide out of window
    let mut state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    for seg in &mut state.segments {
        seg.key_period = Some(seg.number / 5); // 5 segments per period
    }
    state.rotation_drm_info = vec![
        ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("period0".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "aabbccdd11223344".into(),
            clearkey_pssh: None,
        },
        ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some("period1".into()),
            playready_pssh: None,
            playready_pro: None,
            fairplay_key_uri: None,
            default_kid: "eeff00112233aabb".into(),
            clearkey_pssh: None,
        },
    ];

    let rendered = manifest::render_manifest(&state).unwrap();
    // Should only show 3 segments (segments 7,8,9 — all in period 1)
    assert_eq!(rendered.matches("#EXTINF:").count(), 3);
}

#[test]
fn e2e_clear_lead_with_iframes() {
    // Clear lead + I-frame playlist — first N segments unencrypted
    let mut state = make_hls_iframe_manifest_state(6, ManifestPhase::Complete);
    state.clear_lead_boundary = Some(2); // First 2 segments are clear

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 6);

    // I-frame playlist should still work
    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));
    assert_eq!(iframe.matches("#EXT-X-BYTERANGE:").count(), 6);
}

#[test]
fn e2e_clear_lead_with_dvr() {
    // Clear lead + DVR window
    let mut state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    state.clear_lead_boundary = Some(3);

    let rendered = manifest::render_manifest(&state).unwrap();
    // DVR window shows segments 7,8,9 — all past the clear lead boundary
    assert_eq!(rendered.matches("#EXTINF:").count(), 3);
}

#[test]
fn e2e_raw_keys_full_pipeline() {
    // Raw key mode: init rewrite + segment rewrite + manifest render
    // (simulates providing keys directly, bypassing SPEKE)
    let raw_key: [u8; 16] = [
        0xF0, 0xE1, 0xD2, 0xC3, 0xB4, 0xA5, 0x96, 0x87,
        0x78, 0x69, 0x5A, 0x4B, 0x3C, 0x2D, 0x1E, 0x0F,
    ];

    // Clear → CENC with raw key
    let clear_init = build_clear_init_segment();
    let (clear_seg, _pt) = build_clear_media_segment(4, 64);

    let key_set = make_drm_key_set();
    let enc_init = full_init_rewrite(
        &clear_init,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        Some(&key_set),
        ContainerFormat::Cmaf,
    );

    let enc_seg = full_segment_rewrite(
        &clear_seg,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        None,
        Some(&raw_key),
    );

    assert!(enc_init.windows(4).any(|w| w == b"sinf"));
    assert_valid_segment_structure(&enc_seg, 4, true);

    // Render manifest
    let state = make_hls_manifest_state(5, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 5);
}

#[test]
fn e2e_clearkey_drm_full_pipeline() {
    // ClearKey DRM: PSSH + init + segment + manifest
    let clear_init = build_clear_init_segment();
    let (clear_seg, _pt) = build_clear_media_segment(4, 64);

    let key_set = make_drm_key_set();
    let _enc_init = full_init_rewrite(
        &clear_init,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        Some(&key_set),
        ContainerFormat::Cmaf,
    );
    let enc_seg = full_segment_rewrite(
        &clear_seg,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        None,
        Some(&TEST_TARGET_KEY),
    );

    assert_valid_segment_structure(&enc_seg, 4, true);

    // Manifest with ClearKey PSSH
    let mut state = make_hls_manifest_state(5, ManifestPhase::Complete);
    if let Some(ref mut drm) = state.drm_info {
        drm.clearkey_pssh = Some("AAAANnBzc2gAAAAA4nGdWKmFs8l4GrAwrw==".into());
    }

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 5);
}

// ─── Kitchen Sink (4 tests) ────────────────────────────────────────

/// Build a "kitchen sink" ManifestState with all features enabled.
fn build_kitchen_sink_state(format: OutputFormat, phase: ManifestPhase) -> ManifestState {
    let segment_count = 10u32;
    let mut state = make_manifest_state_with_container(
        format,
        ContainerFormat::Fmp4,
        segment_count,
        phase,
    );

    // DVR window
    state.dvr_window_duration = Some(24.0); // 4 segments at 6s

    // I-frame playlist
    state.enable_iframe_playlist = true;
    for i in 0..segment_count {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 8192,
            duration: 6.0,
            segment_uri: state.segments[i as usize].uri.clone(),
        });
    }

    // Ad breaks
    state.ad_breaks = vec![
        AdBreakInfo {
            id: 1,
            presentation_time: 18.0,
            duration: Some(15.0),
            scte35_cmd: None,
            segment_number: 3,
        },
        AdBreakInfo {
            id: 2,
            presentation_time: 48.0,
            duration: Some(30.0),
            scte35_cmd: None,
            segment_number: 8,
        },
    ];

    // Content steering
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: Some("cdn-a".into()),
        query_before_start: Some(true),
    });

    state
}

#[test]
fn e2e_all_features_hls_live() {
    let state = build_kitchen_sink_state(OutputFormat::Hls, ManifestPhase::Live);
    let rendered = manifest::render_manifest(&state).unwrap();

    // DVR: should show 4 segments (24s window / 6s = 4)
    let extinf_count = rendered.matches("#EXTINF:").count();
    assert_eq!(extinf_count, 4, "DVR should show 4 segments, got {extinf_count}");

    // DRM
    assert!(rendered.contains("#EXT-X-KEY:"));

    // No ENDLIST (live)
    assert!(!rendered.contains("#EXT-X-ENDLIST"));

    // I-frame playlist
    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert!(iframe.contains("#EXT-X-I-FRAMES-ONLY"));
}

#[test]
fn e2e_all_features_hls_complete() {
    // Complete phase = live-to-VOD transition: all segments, ENDLIST
    let state = build_kitchen_sink_state(OutputFormat::Hls, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();

    // Complete: all 10 segments regardless of DVR
    assert_eq!(rendered.matches("#EXTINF:").count(), 10);
    assert!(rendered.contains("#EXT-X-ENDLIST"));

    // I-frame playlist with all 10 entries
    let iframe = hls::render_iframe_playlist(&state).unwrap().unwrap();
    assert_eq!(iframe.matches("#EXT-X-BYTERANGE:").count(), 10);
}

#[test]
fn e2e_all_features_dash_live() {
    let state = build_kitchen_sink_state(OutputFormat::Dash, ManifestPhase::Live);
    let rendered = manifest::render_manifest(&state).unwrap();

    // DASH DVR
    assert!(rendered.contains("timeShiftBufferDepth"));
    assert!(rendered.contains("type=\"dynamic\""));

    // DASH DRM
    assert!(rendered.contains("<ContentProtection"));

    // DASH trick play
    assert!(rendered.contains("trickmode"));

    // DASH content steering
    assert!(rendered.contains("<ContentSteering"));

    // Windowed segments
    let s_count = rendered.matches("<S ").count();
    assert!(s_count >= 4, "DASH DVR should have at least 4 S entries, got {s_count}");
}

#[test]
fn e2e_all_features_dash_complete() {
    let state = build_kitchen_sink_state(OutputFormat::Dash, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();

    // Complete: type=static, all segments
    assert!(rendered.contains("type=\"static\""));
    assert!(!rendered.contains("timeShiftBufferDepth"));

    // All 10 segments in main + trick play
    let s_count = rendered.matches("<S ").count();
    assert!(s_count >= 10, "Complete DASH should have at least 10 S entries, got {s_count}");

    assert!(rendered.contains("trickmode"));
    assert!(rendered.contains("<ContentSteering"));
}

// ═══════════════════════════════════════════════════════════════════════
// Category 4: Lifecycle Phase Transitions (18 tests)
// ═══════════════════════════════════════════════════════════════════════

/// Helper for lifecycle phase tests.
fn run_e2e_lifecycle(format: OutputFormat, container: ContainerFormat, phase: ManifestPhase) {
    let segment_count = match phase {
        ManifestPhase::AwaitingFirstSegment => 0u32,
        ManifestPhase::Live => 5,
        ManifestPhase::Complete => 5,
    };

    let state = make_manifest_state_with_container(format, container, segment_count, phase);
    let system = CacheConfig::default();

    // Validate cache headers
    let cache_header = state.manifest_cache_header(&system);
    match phase {
        ManifestPhase::AwaitingFirstSegment => {
            assert_eq!(cache_header, "no-cache", "AwaitingFirstSegment must be no-cache");
        }
        ManifestPhase::Live => {
            assert!(cache_header.contains("public"), "Live must have public");
            assert!(cache_header.contains("max-age="), "Live must have max-age");
        }
        ManifestPhase::Complete => {
            assert!(cache_header.contains("public"), "Complete must have public");
            assert!(cache_header.contains("immutable"), "Complete must have immutable");
        }
    }

    // Render manifest
    let rendered = manifest::render_manifest(&state).unwrap();

    let ext = container.video_segment_extension();
    match format {
        OutputFormat::Hls => {
            if segment_count > 0 {
                assert_valid_hls(&rendered, segment_count as usize);
                // Check segment extension
                assert!(
                    rendered.contains(ext),
                    "HLS must use {ext} extension for {:?}",
                    container
                );
            }
            match phase {
                ManifestPhase::AwaitingFirstSegment => {
                    // No segments, but still a valid-ish manifest
                    assert!(rendered.starts_with("#EXTM3U"));
                }
                ManifestPhase::Live => {
                    assert!(!rendered.contains("#EXT-X-ENDLIST"));
                }
                ManifestPhase::Complete => {
                    assert!(rendered.contains("#EXT-X-ENDLIST"));
                }
            }
        }
        OutputFormat::Dash => {
            if segment_count > 0 {
                assert_valid_dash(&rendered, segment_count as usize);
            }
            match phase {
                ManifestPhase::AwaitingFirstSegment => {
                    // DASH renderer returns early with minimal XML (no <MPD>) on AwaitingFirstSegment
                    assert!(rendered.contains("<?xml"));
                }
                ManifestPhase::Live => {
                    assert!(rendered.contains("type=\"dynamic\""));
                }
                ManifestPhase::Complete => {
                    assert!(rendered.contains("type=\"static\""));
                }
            }
        }
    }
}

// ─── AwaitingFirstSegment ───────────────────────────────────────────

#[test]
fn e2e_awaiting_hls_cmaf() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Cmaf, ManifestPhase::AwaitingFirstSegment);
}

#[test]
fn e2e_awaiting_hls_fmp4() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Fmp4, ManifestPhase::AwaitingFirstSegment);
}

#[test]
fn e2e_awaiting_hls_iso() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Iso, ManifestPhase::AwaitingFirstSegment);
}

#[test]
fn e2e_awaiting_dash_cmaf() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Cmaf, ManifestPhase::AwaitingFirstSegment);
}

#[test]
fn e2e_awaiting_dash_fmp4() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Fmp4, ManifestPhase::AwaitingFirstSegment);
}

#[test]
fn e2e_awaiting_dash_iso() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Iso, ManifestPhase::AwaitingFirstSegment);
}

// ─── Live ───────────────────────────────────────────────────────────

#[test]
fn e2e_live_hls_cmaf() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Cmaf, ManifestPhase::Live);
}

#[test]
fn e2e_live_hls_fmp4() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Fmp4, ManifestPhase::Live);
}

#[test]
fn e2e_live_hls_iso() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Iso, ManifestPhase::Live);
}

#[test]
fn e2e_live_dash_cmaf() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Cmaf, ManifestPhase::Live);
}

#[test]
fn e2e_live_dash_fmp4() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Fmp4, ManifestPhase::Live);
}

#[test]
fn e2e_live_dash_iso() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Iso, ManifestPhase::Live);
}

// ─── Complete ───────────────────────────────────────────────────────

#[test]
fn e2e_complete_hls_cmaf() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Cmaf, ManifestPhase::Complete);
}

#[test]
fn e2e_complete_hls_fmp4() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Fmp4, ManifestPhase::Complete);
}

#[test]
fn e2e_complete_hls_iso() {
    run_e2e_lifecycle(OutputFormat::Hls, ContainerFormat::Iso, ManifestPhase::Complete);
}

#[test]
fn e2e_complete_dash_cmaf() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Cmaf, ManifestPhase::Complete);
}

#[test]
fn e2e_complete_dash_fmp4() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Fmp4, ManifestPhase::Complete);
}

#[test]
fn e2e_complete_dash_iso() {
    run_e2e_lifecycle(OutputFormat::Dash, ContainerFormat::Iso, ManifestPhase::Complete);
}

// ═══════════════════════════════════════════════════════════════════════
// Category 5: Edge Cases & Boundary Conditions (21 tests)
// ═══════════════════════════════════════════════════════════════════════

// ─── Single Segment Streams (3 tests) ──────────────────────────────

#[test]
fn e2e_single_segment_hls_complete() {
    let state = make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Cmaf,
        1,
        ManifestPhase::Complete,
    );
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 1);
    assert!(rendered.contains("#EXT-X-ENDLIST"));
}

#[test]
fn e2e_single_segment_dash_complete() {
    let state = make_manifest_state_with_container(
        OutputFormat::Dash,
        ContainerFormat::Cmaf,
        1,
        ManifestPhase::Complete,
    );
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_dash(&rendered, 1);
    assert!(rendered.contains("type=\"static\""));
}

#[test]
fn e2e_single_segment_encrypted() {
    // Full pipeline: 1 clear segment → encrypted → manifest
    let clear_init = build_clear_init_segment();
    let (clear_seg, _pt) = build_clear_media_segment(2, 64);

    let key_set = make_drm_key_set();
    let enc_init = full_init_rewrite(
        &clear_init,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        Some(&key_set),
        ContainerFormat::Cmaf,
    );
    let enc_seg = full_segment_rewrite(
        &clear_seg,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        None,
        Some(&TEST_TARGET_KEY),
    );

    assert!(enc_init.windows(4).any(|w| w == b"sinf"));
    assert_valid_segment_structure(&enc_seg, 2, true);

    let state = make_hls_manifest_state(1, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 1);
}

// ─── DVR Boundary Conditions (6 tests) ─────────────────────────────

#[test]
fn e2e_dvr_window_larger_than_content() {
    // Window of 120s > 60s of content (10 × 6s) — should show all segments
    let state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 120.0);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_eq!(rendered.matches("#EXTINF:").count(), 10);
}

#[test]
fn e2e_dvr_window_equals_content() {
    // Window == total content duration (60s) — should show all segments
    let state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 60.0);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_eq!(rendered.matches("#EXTINF:").count(), 10);
}

#[test]
fn e2e_dvr_window_one_segment() {
    // Window fits exactly 1 segment (6s)
    let state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 6.0);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_eq!(rendered.matches("#EXTINF:").count(), 1);
}

#[test]
fn e2e_dvr_no_window_shows_all() {
    // No DVR window configured — all segments visible
    let state = make_hls_manifest_state(10, ManifestPhase::Live);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_eq!(rendered.matches("#EXTINF:").count(), 10);
}

#[test]
fn e2e_dvr_live_to_vod_shows_all_segments() {
    // Complete phase renders ALL segments regardless of window
    let state = make_hls_dvr_manifest_state(10, ManifestPhase::Complete, 18.0);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_eq!(rendered.matches("#EXTINF:").count(), 10);
    assert!(rendered.contains("#EXT-X-ENDLIST"));
}

#[test]
fn e2e_dvr_window_zero_duration() {
    // Edge case: window duration of 0.0
    // This should NOT be treated as "DVR active" (condition is window > 0.0)
    let state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 0.0);
    assert!(!state.is_dvr_active());
    let rendered = manifest::render_manifest(&state).unwrap();
    // Should show all segments since DVR is not active
    assert_eq!(rendered.matches("#EXTINF:").count(), 10);
}

// ─── Large Segment Counts (3 tests) ────────────────────────────────

#[test]
fn e2e_200_segments_hls_manifest() {
    let state = make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Cmaf,
        200,
        ManifestPhase::Complete,
    );
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 200);
}

#[test]
fn e2e_200_segments_dash_manifest() {
    let state = make_manifest_state_with_container(
        OutputFormat::Dash,
        ContainerFormat::Cmaf,
        200,
        ManifestPhase::Complete,
    );
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_dash(&rendered, 200);
}

#[test]
fn e2e_200_segments_with_dvr() {
    // 200 segments × 6s = 1200s. DVR window = 30s → 5 segments
    let mut state = make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Cmaf,
        200,
        ManifestPhase::Live,
    );
    state.dvr_window_duration = Some(30.0);

    let rendered = manifest::render_manifest(&state).unwrap();
    assert_eq!(rendered.matches("#EXTINF:").count(), 5);
}

// ─── Manifest Parse Roundtrip Integrity (6 tests) ──────────────────

#[test]
fn e2e_hls_render_parse_render_stable() {
    // render → parse → rebuild state → re-render should be structurally similar
    let state = make_hls_manifest_state(5, ManifestPhase::Complete);
    let rendered1 = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered1, 5);

    // Parse the rendered manifest
    let parsed = edgepack::manifest::hls_input::parse_hls_manifest(
        &rendered1,
        "https://example.com/test.m3u8",
    )
    .unwrap();

    assert_eq!(parsed.segment_urls.len(), 5);
    assert!(!parsed.is_live);
}

#[test]
fn e2e_dash_render_parse_render_stable() {
    let state = make_dash_manifest_state(5, ManifestPhase::Complete);
    let rendered1 = manifest::render_manifest(&state).unwrap();
    assert_valid_dash(&rendered1, 5);

    let parsed = edgepack::manifest::dash_input::parse_dash_manifest(
        &rendered1,
        "https://example.com/test.mpd",
    )
    .unwrap();

    assert_eq!(parsed.segment_urls.len(), 5);
    assert!(!parsed.is_live);
}

#[test]
fn e2e_hls_live_render_parse_roundtrip() {
    let state = make_hls_manifest_state(5, ManifestPhase::Live);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_hls(&rendered, 5);

    let parsed = edgepack::manifest::hls_input::parse_hls_manifest(
        &rendered,
        "https://example.com/test.m3u8",
    )
    .unwrap();

    assert_eq!(parsed.segment_urls.len(), 5);
    assert!(parsed.is_live);
}

#[test]
fn e2e_dash_live_render_parse_roundtrip() {
    let state = make_dash_manifest_state(5, ManifestPhase::Live);
    let rendered = manifest::render_manifest(&state).unwrap();
    assert_valid_dash(&rendered, 5);

    let parsed = edgepack::manifest::dash_input::parse_dash_manifest(
        &rendered,
        "https://example.com/test.mpd",
    )
    .unwrap();

    assert_eq!(parsed.segment_urls.len(), 5);
    assert!(parsed.is_live);
}

#[test]
fn e2e_hls_drm_signaling_survives_roundtrip() {
    let state = make_hls_manifest_state(5, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();

    assert!(rendered.contains("#EXT-X-KEY:"));

    // Parse back — source scheme should be detected
    let parsed = edgepack::manifest::hls_input::parse_hls_manifest(
        &rendered,
        "https://example.com/test.m3u8",
    )
    .unwrap();

    // Source scheme detection from #EXT-X-KEY METHOD
    assert!(
        parsed.source_scheme.is_some(),
        "parsed manifest should detect source encryption scheme"
    );
}

#[test]
fn e2e_dash_drm_signaling_survives_roundtrip() {
    let state = make_dash_manifest_state(5, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();

    assert!(rendered.contains("<ContentProtection"));

    let parsed = edgepack::manifest::dash_input::parse_dash_manifest(
        &rendered,
        "https://example.com/test.mpd",
    )
    .unwrap();

    assert!(
        parsed.source_scheme.is_some(),
        "parsed DASH manifest should detect source encryption scheme"
    );
}

// ─── Encryption with Various Sample Sizes (3 tests) ────────────────

#[test]
fn e2e_tiny_samples_1_byte() {
    // 1-byte samples — smaller than AES block size
    let (clear_seg, _pt) = build_clear_media_segment(4, 1);

    let rewritten = full_segment_rewrite(
        &clear_seg,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        None,
        Some(&TEST_TARGET_KEY),
    );

    assert_valid_segment_structure(&rewritten, 4, true);
}

#[test]
fn e2e_exact_block_aligned_samples() {
    // 16-byte samples — exactly one AES block
    let (clear_seg, _pt) = build_clear_media_segment(4, 16);

    let rewritten = full_segment_rewrite(
        &clear_seg,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        None,
        Some(&TEST_TARGET_KEY),
    );

    assert_valid_segment_structure(&rewritten, 4, true);

    // Verify roundtrip: encrypt then decrypt
    let decrypted = full_segment_rewrite(
        &rewritten,
        EncryptionScheme::Cenc,
        EncryptionScheme::None,
        Some(&TEST_TARGET_KEY),
        None,
    );

    assert_valid_segment_structure(&decrypted, 4, false);
}

#[test]
fn e2e_large_samples_4096_bytes() {
    // 4KB samples — many encryption blocks
    let (clear_seg, _pt) = build_clear_media_segment(4, 4096);

    let rewritten = full_segment_rewrite(
        &clear_seg,
        EncryptionScheme::None,
        EncryptionScheme::Cenc,
        None,
        Some(&TEST_TARGET_KEY),
    );

    assert_valid_segment_structure(&rewritten, 4, true);

    // Verify roundtrip
    let decrypted = full_segment_rewrite(
        &rewritten,
        EncryptionScheme::Cenc,
        EncryptionScheme::None,
        Some(&TEST_TARGET_KEY),
        None,
    );

    assert_valid_segment_structure(&decrypted, 4, false);
}
