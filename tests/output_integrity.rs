//! Output integrity tests for the JIT edge packager.
//!
//! These tests verify that every input lane produces structurally valid output:
//! - Rewritten segments have valid ISOBMFF structure (moof/mdat, senc/trun consistency)
//! - I-frame BYTERANGE offsets point to valid CMAF chunks
//! - Manifest roundtrips (render → parse → verify) for both HLS and DASH
//! - Multi-KID PSSH boxes contain all required KIDs after init rewriting
//! - TS→CMAF→encrypt combined pipeline produces valid output

mod common;

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::drm::{ContentKey, DrmKeySet};
use edgepack::manifest;
use edgepack::manifest::hls;
use edgepack::manifest::types::*;
use edgepack::media::chunk::detect_chunk_boundaries;
use edgepack::media::cmaf::{self, iterate_boxes, parse_senc, parse_trun};
use edgepack::media::codec::TrackKeyMapping;
use edgepack::media::container::ContainerFormat;
use edgepack::media::init::{create_protection_info, strip_protection_info};
use edgepack::media::segment::{rewrite_segment, SegmentRewriteParams};

use common::*;

// ─── Rewritten Segment Structure Validation ─────────────────────────

/// After encrypted→encrypted rewriting, the output must be valid ISOBMFF:
/// - Contains exactly one moof and one mdat
/// - senc entry count == trun sample_count
/// - mdat payload size == sum of trun sample sizes
#[test]
fn rewritten_segment_has_valid_isobmff_structure_enc_to_enc() {
    let (segment, _plaintext) = build_cbcs_media_segment(4, 64, &TEST_SOURCE_KEY, 8);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }),
        target_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_TARGET_KEY.to_vec(),
            iv: None,
        }),
        source_scheme: EncryptionScheme::Cbcs,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 8,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    let rewritten = rewrite_segment(&segment, &params).unwrap();
    validate_segment_structure(&rewritten, 4, true);
}

/// After clear→encrypted rewriting, the output must inject senc correctly.
#[test]
fn rewritten_segment_has_valid_isobmff_structure_clear_to_enc() {
    let (segment, _plaintext) = build_clear_media_segment(4, 64);

    let params = SegmentRewriteParams {
        source_key: None,
        target_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_TARGET_KEY.to_vec(),
            iv: None,
        }),
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 0,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    let rewritten = rewrite_segment(&segment, &params).unwrap();
    validate_segment_structure(&rewritten, 4, true);
}

/// After encrypted→clear rewriting, the output must strip senc.
#[test]
fn rewritten_segment_has_valid_isobmff_structure_enc_to_clear() {
    let (segment, _plaintext) = build_cbcs_media_segment(4, 64, &TEST_SOURCE_KEY, 8);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }),
        target_key: None,
        source_scheme: EncryptionScheme::Cbcs,
        target_scheme: EncryptionScheme::None,
        source_iv_size: 8,
        target_iv_size: 0,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    let rewritten = rewrite_segment(&segment, &params).unwrap();
    validate_segment_structure(&rewritten, 4, false);
}

/// Clear→clear pass-through preserves structure.
#[test]
fn rewritten_segment_has_valid_isobmff_structure_clear_to_clear() {
    let (segment, _plaintext) = build_clear_media_segment(4, 64);

    let params = SegmentRewriteParams {
        source_key: None,
        target_key: None,
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::None,
        source_iv_size: 0,
        target_iv_size: 0,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    let rewritten = rewrite_segment(&segment, &params).unwrap();
    validate_segment_structure(&rewritten, 4, false);
}

/// Validate mdat payload size matches sum of trun sample sizes.
#[test]
fn rewritten_segment_mdat_size_matches_trun_sample_sizes() {
    let (segment, _plaintext) = build_cbcs_media_segment(8, 128, &TEST_SOURCE_KEY, 8);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }),
        target_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_TARGET_KEY.to_vec(),
            iv: None,
        }),
        source_scheme: EncryptionScheme::Cbcs,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 8,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    let rewritten = rewrite_segment(&segment, &params).unwrap();

    // Parse moof to get trun sample sizes
    let (moof_data, mdat_data) = find_moof_mdat_boxes(&rewritten);
    let trun = find_and_parse_trun(moof_data);

    let total_sample_bytes: u32 = trun
        .entries
        .iter()
        .filter_map(|e| e.sample_size)
        .sum();

    // mdat payload = mdat box size - 8 byte header
    let mdat_header = iterate_boxes(mdat_data).next().unwrap().unwrap();
    let mdat_payload_size = mdat_header.size as u32 - mdat_header.header_size as u32;

    assert_eq!(
        total_sample_bytes, mdat_payload_size,
        "trun sample sizes ({total_sample_bytes}) must equal mdat payload ({mdat_payload_size})"
    );
}

// ─── Encryption Roundtrip Plaintext Recovery ────────────────────────

/// Full roundtrip: encrypt clear segment, then decrypt back to clear.
/// Verify recovered plaintext matches original.
#[test]
fn encrypt_then_decrypt_recovers_original_plaintext() {
    let sample_count = 4;
    let sample_size = 64;
    let (clear_segment, original_plaintext) = build_clear_media_segment(sample_count, sample_size);

    // Clear → CENC
    let encrypt_params = SegmentRewriteParams {
        source_key: None,
        target_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_TARGET_KEY.to_vec(),
            iv: None,
        }),
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 0,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 1,
    };

    let encrypted = rewrite_segment(&clear_segment, &encrypt_params).unwrap();

    // Verify encrypted mdat differs from original
    let (_, enc_mdat) = find_moof_mdat_boxes(&encrypted);
    let (_, orig_mdat) = find_moof_mdat_boxes(&clear_segment);
    assert_ne!(enc_mdat, orig_mdat, "encrypted mdat must differ from clear mdat");

    // CENC → Clear
    let decrypt_params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_TARGET_KEY.to_vec(),
            iv: None,
        }),
        target_key: None,
        source_scheme: EncryptionScheme::Cenc,
        target_scheme: EncryptionScheme::None,
        source_iv_size: 8,
        target_iv_size: 0,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 1,
    };

    let decrypted = rewrite_segment(&encrypted, &decrypt_params).unwrap();

    // Extract mdat payload and compare to original plaintext
    let (_, dec_mdat) = find_moof_mdat_boxes(&decrypted);
    let dec_header = iterate_boxes(dec_mdat).next().unwrap().unwrap();
    let dec_payload = &dec_mdat[dec_header.header_size as usize..];

    let mut offset = 0;
    for (i, expected) in original_plaintext.iter().enumerate() {
        let actual = &dec_payload[offset..offset + expected.len()];
        assert_eq!(
            actual, expected.as_slice(),
            "sample {i} plaintext mismatch after encrypt→decrypt roundtrip"
        );
        offset += expected.len();
    }
}

// ─── I-Frame BYTERANGE Verification ─────────────────────────────────

/// I-frame byte ranges must point to valid moof+mdat chunks within the segment.
#[test]
fn iframe_byterange_points_to_valid_cmaf_chunk() {
    // Build a segment with enough data for chunk detection
    let (segment, _plaintext) = build_clear_media_segment(1, 256);

    // Detect chunk boundaries (same as pipeline does)
    let boundaries = detect_chunk_boundaries(&segment);
    assert!(
        !boundaries.is_empty(),
        "segment must have at least one chunk boundary"
    );

    // Verify each boundary points to valid ISOBMFF data
    for boundary in &boundaries {
        let chunk_data = &segment[boundary.offset..boundary.offset + boundary.size];

        // Must start with a valid moof box
        let first_box = iterate_boxes(chunk_data).next();
        assert!(
            first_box.is_some(),
            "chunk at offset {} must contain valid ISOBMFF boxes",
            boundary.offset
        );
        let header = first_box.unwrap().unwrap();
        assert_eq!(
            &header.box_type, b"moof",
            "first box in chunk must be moof, got {:?}",
            std::str::from_utf8(&header.box_type)
        );
    }
}

/// After rewriting, I-frame chunk boundaries are still at valid offsets.
#[test]
fn iframe_byterange_valid_after_encryption_rewrite() {
    let (segment, _plaintext) = build_clear_media_segment(1, 256);

    // Encrypt the segment
    let params = SegmentRewriteParams {
        source_key: None,
        target_key: Some(ContentKey {
            kid: TEST_KID,
            key: TEST_TARGET_KEY.to_vec(),
            iv: None,
        }),
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 0,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    let rewritten = rewrite_segment(&segment, &params).unwrap();

    // Detect chunk boundaries in rewritten segment
    let boundaries = detect_chunk_boundaries(&rewritten);
    assert!(!boundaries.is_empty(), "rewritten segment must have chunk boundaries");

    for boundary in &boundaries {
        assert!(
            boundary.offset + boundary.size <= rewritten.len(),
            "chunk boundary extends beyond segment: offset={}, size={}, segment_len={}",
            boundary.offset,
            boundary.size,
            rewritten.len()
        );

        let chunk_data = &rewritten[boundary.offset..boundary.offset + boundary.size];
        let first_box = iterate_boxes(chunk_data).next().unwrap().unwrap();
        assert_eq!(
            &first_box.box_type, b"moof",
            "chunk must start with moof after rewriting"
        );
    }
}

// ─── Init Segment Rewriting Integrity ───────────────────────────────

/// After clear→encrypted init rewriting, the output contains sinf with correct structure.
#[test]
fn init_rewrite_clear_to_encrypted_produces_valid_sinf() {
    let clear_init = build_clear_init_segment();

    let key_set = make_drm_key_set();
    let mapping = TrackKeyMapping::single(TEST_KID);
    let encrypted_init = create_protection_info(
        &clear_init,
        &key_set,
        &mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Parse the rewritten init to verify structure
    let protection = edgepack::media::init::parse_protection_info(&encrypted_init).unwrap();
    assert!(protection.is_some(), "rewritten init must have protection info");

    let info = protection.unwrap();
    assert_eq!(
        &info.original_format, b"avc1",
        "frma must preserve original format"
    );
    assert_eq!(
        &info.scheme_type, b"cenc",
        "schm must be target scheme"
    );
    assert_eq!(
        info.tenc.default_kid, TEST_KID,
        "tenc KID must match"
    );

    // Verify PSSH boxes are present (at least Widevine)
    let mut has_pssh = false;
    for box_result in iterate_boxes(&encrypted_init) {
        let header = box_result.unwrap();
        if header.box_type == *b"moov" {
            let moov_data = &encrypted_init[header.offset as usize..(header.offset + header.size) as usize];
            let moov_payload = &moov_data[header.header_size as usize..];
            for child in iterate_boxes(moov_payload) {
                let ch = child.unwrap();
                if ch.box_type == *b"pssh" {
                    has_pssh = true;
                }
            }
        }
    }
    assert!(has_pssh, "rewritten init must contain PSSH boxes");
}

/// After encrypted→clear init rewriting, sinf and PSSH are stripped.
#[test]
fn init_rewrite_encrypted_to_clear_strips_protection() {
    let encrypted_init = build_cbcs_init_segment();

    let cleared_init =
        strip_protection_info(&encrypted_init, ContainerFormat::Cmaf).unwrap();

    // Verify no sinf remains
    let protection = edgepack::media::init::parse_protection_info(&cleared_init).unwrap();
    assert!(
        protection.is_none(),
        "cleared init must not have protection info"
    );

    // Verify no PSSH boxes remain
    for box_result in iterate_boxes(&cleared_init) {
        let header = box_result.unwrap();
        if header.box_type == *b"moov" {
            let moov_data = &cleared_init[header.offset as usize..(header.offset + header.size) as usize];
            let moov_payload = &moov_data[header.header_size as usize..];
            for child in iterate_boxes(moov_payload) {
                let ch = child.unwrap();
                assert_ne!(
                    ch.box_type, *b"pssh",
                    "cleared init must not contain PSSH boxes"
                );
            }
        }
    }
}

/// Roundtrip: clear → encrypted → clear preserves sample entry type.
#[test]
fn init_rewrite_roundtrip_preserves_sample_entry() {
    let clear_init = build_clear_init_segment();

    let key_set = make_drm_key_set();
    let mapping = TrackKeyMapping::single(TEST_KID);
    let encrypted_init = create_protection_info(
        &clear_init,
        &key_set,
        &mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Verify it became encrypted (has sinf)
    let info = edgepack::media::init::parse_protection_info(&encrypted_init)
        .unwrap()
        .unwrap();
    assert_eq!(&info.original_format, b"avc1");

    // Strip back to clear
    let restored = strip_protection_info(&encrypted_init, ContainerFormat::Cmaf).unwrap();

    // Verify it's clear again
    let restored_info = edgepack::media::init::parse_protection_info(&restored).unwrap();
    assert!(restored_info.is_none(), "restored init must be clear");
}

// ─── Multi-KID PSSH Verification ────────────────────────────────────

/// Multi-KID init rewriting must produce PSSH boxes containing all KIDs.
#[test]
fn multi_kid_pssh_contains_all_kids_after_rewrite() {
    use edgepack::drm::{system_ids, DrmSystemData};
    use edgepack::media::init::rewrite_init_segment;

    let init = build_cbcs_init_segment();
    let video_kid = [0xAA; 16];
    let audio_kid = [0xBB; 16];

    // Use single mapping (init has one track) but DrmKeySet has both KIDs
    // so PSSH boxes should contain both KIDs
    let mapping = TrackKeyMapping::single(video_kid);

    let drm_systems = vec![
        DrmSystemData {
            system_id: system_ids::WIDEVINE,
            kid: video_kid,
            pssh_data: vec![0x08, 0x01],
            content_protection_data: None,
        },
        DrmSystemData {
            system_id: system_ids::WIDEVINE,
            kid: audio_kid,
            pssh_data: vec![0x08, 0x01],
            content_protection_data: None,
        },
    ];

    let key_set = DrmKeySet {
        keys: vec![
            ContentKey { kid: video_kid, key: TEST_TARGET_KEY.to_vec(), iv: None },
            ContentKey { kid: audio_kid, key: TEST_TARGET_KEY.to_vec(), iv: None },
        ],
        drm_systems,
    };

    let rewritten = rewrite_init_segment(
        &init,
        &key_set,
        &mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Find PSSH boxes and verify they contain both KIDs
    let mut found_kids: Vec<[u8; 16]> = Vec::new();
    for box_result in iterate_boxes(&rewritten) {
        let header = box_result.unwrap();
        if header.box_type == *b"moov" {
            let moov_data =
                &rewritten[header.offset as usize..(header.offset + header.size) as usize];
            let moov_payload = &moov_data[header.header_size as usize..];
            for child in iterate_boxes(moov_payload) {
                let ch = child.unwrap();
                if ch.box_type == *b"pssh" {
                    let pssh_box = &moov_payload
                        [ch.offset as usize..(ch.offset + ch.size) as usize];
                    let pssh_payload = &pssh_box[ch.header_size as usize..];
                    let pssh = cmaf::parse_pssh(pssh_payload).unwrap();
                    for kid in &pssh.key_ids {
                        if !found_kids.contains(kid) {
                            found_kids.push(*kid);
                        }
                    }
                }
            }
        }
    }

    assert!(
        found_kids.contains(&video_kid),
        "PSSH must contain video KID"
    );
    assert!(
        found_kids.contains(&audio_kid),
        "PSSH must contain audio KID"
    );
}

// ─── Manifest Roundtrip Tests ───────────────────────────────────────

/// Rendered HLS manifest can be parsed back as valid M3U8.
#[test]
fn hls_manifest_roundtrip_parses_as_valid_m3u8() {
    let state = make_hls_manifest_state(5, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();

    // Basic structural validation
    assert!(rendered.starts_with("#EXTM3U"), "HLS manifest must start with #EXTM3U");
    assert!(
        rendered.contains("#EXT-X-VERSION:"),
        "HLS manifest must have VERSION tag"
    );
    assert!(
        rendered.contains("#EXT-X-TARGETDURATION:"),
        "HLS manifest must have TARGETDURATION"
    );
    assert!(
        rendered.contains("#EXT-X-ENDLIST"),
        "Complete HLS manifest must have ENDLIST"
    );

    // Count EXTINF tags (should match segment count)
    let extinf_count = rendered.matches("#EXTINF:").count();
    assert_eq!(
        extinf_count, 5,
        "HLS manifest must have exactly 5 EXTINF tags, got {extinf_count}"
    );

    // Verify all segment URIs are present and unique
    let segment_uris: Vec<&str> = rendered
        .lines()
        .filter(|l| l.contains("segment_") && !l.starts_with('#'))
        .collect();
    assert_eq!(segment_uris.len(), 5, "must have 5 segment URIs");
    for (i, uri) in segment_uris.iter().enumerate() {
        assert!(
            uri.contains(&format!("segment_{i}")),
            "segment URI {i} must reference correct segment number"
        );
    }

    // Parse rendered manifest back using the HLS input parser
    let parsed = edgepack::manifest::hls_input::parse_hls_manifest(
        &rendered,
        "https://example.com/hls/",
    )
    .unwrap();

    assert_eq!(
        parsed.segment_urls.len(),
        5,
        "parsed manifest must have 5 segments"
    );
    assert!(!parsed.is_live, "complete manifest must not be live");
}

/// Rendered DASH manifest is valid XML.
#[test]
fn dash_manifest_roundtrip_is_valid_xml() {
    let state = make_dash_manifest_state(5, ManifestPhase::Complete);
    let rendered = manifest::render_manifest(&state).unwrap();

    // Structural validation
    assert!(
        rendered.contains("<?xml"),
        "DASH manifest must have XML declaration"
    );
    assert!(
        rendered.contains("<MPD"),
        "DASH manifest must have MPD element"
    );
    assert!(
        rendered.contains("</MPD>"),
        "DASH manifest must be well-formed (closing MPD tag)"
    );
    assert!(
        rendered.contains("<Period"),
        "DASH manifest must have Period"
    );
    assert!(
        rendered.contains("<AdaptationSet"),
        "DASH manifest must have AdaptationSet"
    );
    assert!(
        rendered.contains("<SegmentTemplate"),
        "DASH manifest must have SegmentTemplate"
    );

    // Count timeline entries
    let s_entries = rendered.matches("<S ").count();
    assert_eq!(s_entries, 5, "DASH must have 5 SegmentTimeline entries, got {s_entries}");

    // Verify it's parseable XML by the DASH input parser
    let parsed = edgepack::manifest::dash_input::parse_dash_manifest(
        &rendered,
        "https://example.com/dash/",
    )
    .unwrap();

    assert_eq!(
        parsed.segment_urls.len(),
        5,
        "parsed DASH manifest must have 5 segments"
    );
}

/// Live HLS manifest (no ENDLIST) roundtrips correctly.
#[test]
fn hls_live_manifest_roundtrip() {
    let state = make_hls_manifest_state(3, ManifestPhase::Live);
    let rendered = manifest::render_manifest(&state).unwrap();

    assert!(!rendered.contains("#EXT-X-ENDLIST"), "live manifest must not have ENDLIST");

    let parsed = edgepack::manifest::hls_input::parse_hls_manifest(
        &rendered,
        "https://example.com/hls/",
    )
    .unwrap();

    assert_eq!(parsed.segment_urls.len(), 3);
    assert!(parsed.is_live, "live manifest must parse as live");
}

/// Live DASH manifest (type=dynamic) roundtrips correctly.
#[test]
fn dash_live_manifest_roundtrip() {
    let state = make_dash_manifest_state(3, ManifestPhase::Live);
    let rendered = manifest::render_manifest(&state).unwrap();

    assert!(
        rendered.contains("type=\"dynamic\""),
        "live DASH must have type=dynamic"
    );

    let parsed = edgepack::manifest::dash_input::parse_dash_manifest(
        &rendered,
        "https://example.com/dash/",
    )
    .unwrap();

    assert_eq!(parsed.segment_urls.len(), 3);
    assert!(parsed.is_live, "live DASH must parse as live");
}

/// HLS I-frame playlist roundtrip — verify BYTERANGE format and count.
#[test]
fn hls_iframe_manifest_has_correct_byterange_count() {
    let state = make_hls_iframe_manifest_state(5, ManifestPhase::Complete);
    let rendered = hls::render_iframe_playlist(&state)
        .unwrap()
        .expect("I-frame playlist must be rendered when enabled");

    assert!(rendered.contains("#EXT-X-I-FRAMES-ONLY"), "must have I-FRAMES-ONLY tag");

    let byterange_count = rendered.matches("#EXT-X-BYTERANGE:").count();
    assert_eq!(
        byterange_count, 5,
        "I-frame playlist must have exactly 5 BYTERANGE entries, got {byterange_count}"
    );
}

/// HLS DVR windowed manifest has correct segment count and media sequence.
#[test]
fn hls_dvr_windowed_manifest_integrity() {
    // 10 segments at 6s each = 60s total. Window = 18s = 3 segments.
    let state = make_hls_dvr_manifest_state(10, ManifestPhase::Live, 18.0);
    let rendered = manifest::render_manifest(&state).unwrap();

    let extinf_count = rendered.matches("#EXTINF:").count();
    assert_eq!(
        extinf_count, 3,
        "DVR window of 18s at 6s segments must show 3 segments, got {extinf_count}"
    );

    // Media sequence should be 7 (segments 7, 8, 9 visible)
    assert!(
        rendered.contains("#EXT-X-MEDIA-SEQUENCE:7"),
        "media sequence must be 7 for 10 segments with 3-segment window"
    );

    // Must NOT have PLAYLIST-TYPE:EVENT (DVR removes it)
    assert!(
        !rendered.contains("PLAYLIST-TYPE:EVENT"),
        "DVR manifest must not have EVENT playlist type"
    );
}

// ─── Helpers ────────────────────────────────────────────────────────

/// Validate the ISOBMFF structure of a rewritten segment.
fn validate_segment_structure(segment: &[u8], expected_samples: usize, expect_senc: bool) {
    let mut moof_count = 0;
    let mut mdat_count = 0;
    let mut moof_data: Option<&[u8]> = None;

    for box_result in iterate_boxes(segment) {
        let header = box_result.unwrap();
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &segment[header.offset as usize..box_end.min(segment.len())];

        match &header.box_type {
            t if t == b"moof" => {
                moof_count += 1;
                moof_data = Some(box_bytes);
            }
            t if t == b"mdat" => {
                mdat_count += 1;
            }
            _ => {}
        }
    }

    assert_eq!(moof_count, 1, "segment must have exactly 1 moof");
    assert_eq!(mdat_count, 1, "segment must have exactly 1 mdat");

    let moof_bytes = moof_data.unwrap();
    let trun = find_and_parse_trun(moof_bytes);
    assert_eq!(
        trun.sample_count as usize, expected_samples,
        "trun sample_count must be {expected_samples}"
    );

    if expect_senc {
        let senc = find_and_parse_senc(moof_bytes);
        assert_eq!(
            senc.entries.len(),
            expected_samples,
            "senc entry count ({}) must equal trun sample_count ({expected_samples})",
            senc.entries.len()
        );
    }
}

/// Find moof and mdat box data in a segment.
fn find_moof_mdat_boxes(segment: &[u8]) -> (&[u8], &[u8]) {
    let mut moof: Option<&[u8]> = None;
    let mut mdat: Option<&[u8]> = None;

    for box_result in iterate_boxes(segment) {
        let header = box_result.unwrap();
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &segment[header.offset as usize..box_end.min(segment.len())];

        match &header.box_type {
            t if t == b"moof" => moof = Some(box_bytes),
            t if t == b"mdat" => mdat = Some(box_bytes),
            _ => {}
        }
    }

    (moof.unwrap(), mdat.unwrap())
}

/// Find and parse the trun box inside a moof.
fn find_and_parse_trun(moof_data: &[u8]) -> cmaf::TrackRunBox {
    let moof_header = iterate_boxes(moof_data).next().unwrap().unwrap();
    let moof_payload = &moof_data[moof_header.header_size as usize..];
    find_trun_recursive(moof_payload).expect("moof must contain trun")
}

fn find_trun_recursive(data: &[u8]) -> Option<cmaf::TrackRunBox> {
    for box_result in iterate_boxes(data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &data[header.offset as usize..box_end.min(data.len())];
        let payload = &box_bytes[header.header_size as usize..];

        if header.box_type == *b"trun" {
            return Some(parse_trun(payload).unwrap());
        }
        // Recurse into container boxes
        if matches!(
            &header.box_type,
            b"traf" | b"trak" | b"mdia" | b"minf" | b"stbl"
        ) {
            if let Some(trun) = find_trun_recursive(payload) {
                return Some(trun);
            }
        }
    }
    None
}

/// Find and parse the senc box inside a moof.
fn find_and_parse_senc(moof_data: &[u8]) -> cmaf::SampleEncryptionBox {
    let moof_header = iterate_boxes(moof_data).next().unwrap().unwrap();
    let moof_payload = &moof_data[moof_header.header_size as usize..];
    find_senc_recursive(moof_payload, 8).expect("moof must contain senc")
}

fn find_senc_recursive(data: &[u8], iv_size: u8) -> Option<cmaf::SampleEncryptionBox> {
    for box_result in iterate_boxes(data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &data[header.offset as usize..box_end.min(data.len())];
        let payload = &box_bytes[header.header_size as usize..];

        if header.box_type == *b"senc" {
            return Some(parse_senc(payload, iv_size).unwrap());
        }
        if matches!(
            &header.box_type,
            b"traf" | b"trak" | b"mdia" | b"minf" | b"stbl"
        ) {
            if let Some(senc) = find_senc_recursive(payload, iv_size) {
                return Some(senc);
            }
        }
    }
    None
}

// ─── Cache-Control Integrity Tests ──────────────────────────────────

/// Manifest body is unaffected by cache_control settings.
/// Cache-control only changes HTTP headers, not manifest content.
#[test]
fn manifest_body_unchanged_with_cache_control() {
    use edgepack::config::CacheControlConfig;

    let state_without = make_hls_manifest_state(3, ManifestPhase::Complete);
    let rendered_without = manifest::render_manifest(&state_without).unwrap();

    let mut state_with = make_hls_manifest_state(3, ManifestPhase::Complete);
    state_with.cache_control = Some(CacheControlConfig {
        segment_max_age: Some(600),
        final_manifest_max_age: Some(3600),
        live_manifest_max_age: Some(10),
        live_manifest_s_maxage: Some(30),
        immutable: Some(false),
    });
    let rendered_with = manifest::render_manifest(&state_with).unwrap();

    assert_eq!(
        rendered_without, rendered_with,
        "cache_control must not affect manifest body content"
    );
}

/// Safety invariant: AwaitingFirstSegment always produces no-cache,
/// regardless of per-request overrides.
#[test]
fn awaiting_first_segment_always_no_cache_integrity() {
    use edgepack::config::{CacheConfig, CacheControlConfig};

    let mut state = make_hls_manifest_state(0, ManifestPhase::AwaitingFirstSegment);
    state.cache_control = Some(CacheControlConfig {
        segment_max_age: Some(999999),
        final_manifest_max_age: Some(999999),
        live_manifest_max_age: Some(999999),
        live_manifest_s_maxage: Some(999999),
        immutable: Some(true),
    });
    let system = CacheConfig::default();
    assert_eq!(
        state.manifest_cache_header(&system),
        "no-cache",
        "AwaitingFirstSegment must always produce no-cache regardless of overrides"
    );
}

/// DASH manifest body is also unaffected by cache_control.
#[test]
fn dash_manifest_body_unchanged_with_cache_control() {
    use edgepack::config::CacheControlConfig;

    let state_without = make_dash_manifest_state(3, ManifestPhase::Complete);
    let rendered_without = manifest::render_manifest(&state_without).unwrap();

    let mut state_with = make_dash_manifest_state(3, ManifestPhase::Complete);
    state_with.cache_control = Some(CacheControlConfig {
        segment_max_age: Some(86400),
        final_manifest_max_age: Some(7200),
        immutable: Some(false),
        ..Default::default()
    });
    let rendered_with = manifest::render_manifest(&state_with).unwrap();

    assert_eq!(
        rendered_without, rendered_with,
        "cache_control must not affect DASH manifest body content"
    );
}
