//! Conformance test suite — validates structural correctness of repackaged output.
//!
//! Tests that init segments, media segments, and manifests produced by the
//! repackaging pipeline are well-formed and internally consistent.

mod common;

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::drm::ContentKey;
use edgepack::media::cmaf::{iterate_boxes, read_box_header};
use edgepack::media::codec::TrackKeyMapping;
use edgepack::media::container::ContainerFormat;
use edgepack::media::init;
use edgepack::media::segment::{self, SegmentRewriteParams};
use edgepack::media::{box_type, FourCC};

use common::*;

// ─── Helpers ─────────────────────────────────────────────────────────

fn make_source_content_key() -> ContentKey {
    ContentKey {
        kid: TEST_KID,
        key: TEST_SOURCE_KEY.to_vec(),
        iv: None,
    }
}

fn make_target_content_key() -> ContentKey {
    ContentKey {
        kid: TEST_KID,
        key: TEST_TARGET_KEY.to_vec(),
        iv: None,
    }
}

fn cbcs_to_cenc_params() -> SegmentRewriteParams {
    SegmentRewriteParams {
        source_key: Some(make_source_content_key()),
        target_key: Some(make_target_content_key()),
        source_scheme: EncryptionScheme::Cbcs,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 8,
        target_iv_size: 8,
        // Fixture uses full encryption (0:0) for simplicity
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    }
}

fn clear_to_cenc_params() -> SegmentRewriteParams {
    SegmentRewriteParams {
        source_key: None,
        target_key: Some(make_target_content_key()),
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 0,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    }
}

fn cbcs_to_clear_params() -> SegmentRewriteParams {
    SegmentRewriteParams {
        source_key: Some(make_source_content_key()),
        target_key: None,
        source_scheme: EncryptionScheme::Cbcs,
        target_scheme: EncryptionScheme::None,
        source_iv_size: 8,
        target_iv_size: 0,
        // Fixture uses full encryption (0:0) for simplicity
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    }
}

fn clear_to_clear_params() -> SegmentRewriteParams {
    SegmentRewriteParams {
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
    }
}

fn cenc_to_clear_params() -> SegmentRewriteParams {
    SegmentRewriteParams {
        source_key: Some(make_target_content_key()),
        target_key: None,
        source_scheme: EncryptionScheme::Cenc,
        target_scheme: EncryptionScheme::None,
        source_iv_size: 8,
        target_iv_size: 0,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    }
}

/// Recursively search for a box by type in binary data.
fn find_box_recursive(data: &[u8], target: &[u8; 4]) -> Option<Vec<u8>> {
    let target_fourcc: FourCC = *target;
    for header in iterate_boxes(data) {
        let h = match header {
            Ok(h) => h,
            Err(_) => break,
        };
        let box_start = h.offset as usize;
        let box_end = (h.offset + h.size) as usize;
        if box_end > data.len() {
            break;
        }
        if h.box_type == target_fourcc {
            return Some(data[box_start..box_end].to_vec());
        }
        let payload_start = box_start + h.header_size as usize;
        if payload_start < box_end {
            if let Some(found) = find_box_recursive(&data[payload_start..box_end], target) {
                return Some(found);
            }
        }
    }
    None
}

/// Count occurrences of a box type recursively.
fn count_boxes_recursive(data: &[u8], target: &[u8; 4]) -> usize {
    let target_fourcc: FourCC = *target;
    let mut count = 0;
    for header in iterate_boxes(data) {
        let h = match header {
            Ok(h) => h,
            Err(_) => break,
        };
        let box_start = h.offset as usize;
        let box_end = (h.offset + h.size) as usize;
        if box_end > data.len() {
            break;
        }
        if h.box_type == target_fourcc {
            count += 1;
        }
        let payload_start = box_start + h.header_size as usize;
        if payload_start < box_end {
            count += count_boxes_recursive(&data[payload_start..box_end], target);
        }
    }
    count
}

/// Check if a box type exists as a direct child within box data.
fn find_child_in_data(box_data: &[u8], target: &[u8; 4]) -> bool {
    let target_fourcc: FourCC = *target;
    let payload = if box_data.len() > 8 { &box_data[8..] } else { return false; };
    for header in iterate_boxes(payload) {
        if let Ok(h) = header {
            if h.box_type == target_fourcc {
                return true;
            }
        }
    }
    false
}

/// Extract mdat payload from a segment.
fn extract_mdat_payload(data: &[u8]) -> Vec<u8> {
    for header in iterate_boxes(data) {
        let h = match header {
            Ok(h) => h,
            Err(_) => break,
        };
        if h.box_type == box_type::MDAT {
            let start = h.offset as usize + h.header_size as usize;
            let end = (h.offset + h.size) as usize;
            if end <= data.len() {
                return data[start..end].to_vec();
            }
        }
    }
    Vec::new()
}

// ─── Init Segment Conformance ────────────────────────────────────────

#[test]
fn ftyp_is_first_box_in_init() {
    let init = build_cbcs_init_segment();
    let header = read_box_header(&init, 0).unwrap();
    assert_eq!(header.box_type, *b"ftyp");
}

#[test]
fn moov_box_exists_in_init() {
    let init = build_cbcs_init_segment();
    let mut found_moov = false;
    for header in iterate_boxes(&init) {
        let h = header.unwrap();
        if h.box_type == *b"moov" {
            found_moov = true;
        }
    }
    assert!(found_moov, "init segment must contain a moov box");
}

#[test]
fn encrypted_init_has_sinf_schm_tenc() {
    let init = build_cbcs_init_segment();
    let key_set = make_drm_key_set();
    let key_mapping = TrackKeyMapping::single(TEST_KID);
    let rewritten = init::rewrite_init_segment(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::default(),
    )
    .unwrap();

    let sinf_data = find_box_recursive(&rewritten, b"sinf");
    assert!(sinf_data.is_some(), "encrypted init must have sinf box");

    let sinf = sinf_data.unwrap();
    assert!(find_child_in_data(&sinf, b"frma"), "sinf must contain frma box");
    assert!(find_child_in_data(&sinf, b"schm"), "sinf must contain schm box");
}

#[test]
fn clear_init_has_no_sinf() {
    let init = build_cbcs_init_segment();
    let rewritten = init::strip_protection_info(&init, ContainerFormat::default()).unwrap();
    let sinf_data = find_box_recursive(&rewritten, b"sinf");
    assert!(sinf_data.is_none(), "clear init must not have sinf box");
}

#[test]
fn clear_init_has_no_pssh() {
    let init = build_cbcs_init_segment();
    let rewritten = init::strip_protection_info(&init, ContainerFormat::default()).unwrap();
    let pssh_data = find_box_recursive(&rewritten, b"pssh");
    assert!(pssh_data.is_none(), "clear init must not have pssh box");
}

#[test]
fn encrypted_init_has_pssh_boxes() {
    let init = build_cbcs_init_segment();
    let key_set = make_drm_key_set();
    let key_mapping = TrackKeyMapping::single(TEST_KID);
    let rewritten = init::rewrite_init_segment(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::default(),
    )
    .unwrap();

    let pssh_count = count_boxes_recursive(&rewritten, b"pssh");
    assert!(
        pssh_count >= 1,
        "encrypted init must have at least one pssh box, found {pssh_count}"
    );
}

#[test]
fn ftyp_brands_match_cmaf_format() {
    let init = build_cbcs_init_segment();
    let key_set = make_drm_key_set();
    let key_mapping = TrackKeyMapping::single(TEST_KID);
    let rewritten = init::rewrite_init_segment(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    let header = read_box_header(&rewritten, 0).unwrap();
    assert_eq!(header.box_type, *b"ftyp");
    let payload = &rewritten[header.header_size as usize..header.size as usize];
    let compat_brands_data = &payload[8..];
    let mut brands: Vec<[u8; 4]> = Vec::new();
    for chunk in compat_brands_data.chunks_exact(4) {
        let mut brand = [0u8; 4];
        brand.copy_from_slice(chunk);
        brands.push(brand);
    }
    assert!(
        brands.contains(b"cmfc"),
        "CMAF format ftyp must include 'cmfc' compatible brand"
    );
}

#[test]
fn ftyp_brands_fmp4_no_cmfc() {
    let init = build_cbcs_init_segment();
    let key_set = make_drm_key_set();
    let key_mapping = TrackKeyMapping::single(TEST_KID);
    let rewritten = init::rewrite_init_segment(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Fmp4,
    )
    .unwrap();

    let header = read_box_header(&rewritten, 0).unwrap();
    let payload = &rewritten[header.header_size as usize..header.size as usize];
    let compat_brands_data = &payload[8..];
    let mut brands: Vec<[u8; 4]> = Vec::new();
    for chunk in compat_brands_data.chunks_exact(4) {
        let mut brand = [0u8; 4];
        brand.copy_from_slice(chunk);
        brands.push(brand);
    }
    assert!(
        !brands.contains(b"cmfc"),
        "fMP4 format ftyp must NOT include 'cmfc' compatible brand"
    );
}

// ─── Media Segment Conformance ───────────────────────────────────────

#[test]
fn media_segment_has_moof_and_mdat() {
    let (seg, _) = build_cbcs_media_segment(3, 160, &TEST_SOURCE_KEY, 8);
    let mut found_moof = false;
    let mut found_mdat = false;
    for header in iterate_boxes(&seg) {
        let h = header.unwrap();
        if h.box_type == box_type::MOOF {
            found_moof = true;
        }
        if h.box_type == box_type::MDAT {
            found_mdat = true;
        }
    }
    assert!(found_moof, "media segment must contain moof");
    assert!(found_mdat, "media segment must contain mdat");
}

#[test]
fn encrypted_segment_has_senc() {
    let (seg, _) = build_cbcs_media_segment(3, 160, &TEST_SOURCE_KEY, 8);
    assert!(find_box_recursive(&seg, b"senc").is_some(), "encrypted segment must contain senc");
}

#[test]
fn clear_segment_has_no_senc() {
    let (seg, _) = build_clear_media_segment(3, 160);
    assert!(find_box_recursive(&seg, b"senc").is_none(), "clear segment must not contain senc");
}

#[test]
fn rewritten_encrypted_segment_has_senc() {
    let (seg, _) = build_cbcs_media_segment(3, 160, &TEST_SOURCE_KEY, 8);
    let rewritten = segment::rewrite_segment(&seg, &cbcs_to_cenc_params()).unwrap();
    assert!(
        find_box_recursive(&rewritten, b"senc").is_some(),
        "rewritten encrypted segment must have senc"
    );
}

#[test]
fn rewritten_clear_to_encrypted_has_senc() {
    let (seg, _) = build_clear_media_segment(3, 160);
    let rewritten = segment::rewrite_segment(&seg, &clear_to_cenc_params()).unwrap();
    assert!(
        find_box_recursive(&rewritten, b"senc").is_some(),
        "clear→encrypted rewritten segment must have senc"
    );
}

#[test]
fn rewritten_encrypted_to_clear_no_senc() {
    let (seg, _) = build_cbcs_media_segment(3, 160, &TEST_SOURCE_KEY, 8);
    let rewritten = segment::rewrite_segment(&seg, &cbcs_to_clear_params()).unwrap();
    assert!(
        find_box_recursive(&rewritten, b"senc").is_none(),
        "encrypted→clear rewritten segment must not have senc"
    );
}

#[test]
fn clear_to_clear_passthrough() {
    let (seg, _) = build_clear_media_segment(3, 160);
    let rewritten = segment::rewrite_segment(&seg, &clear_to_clear_params()).unwrap();
    assert_eq!(seg, rewritten, "clear→clear should be byte-for-byte passthrough");
}

// ─── Roundtrip Conformance ───────────────────────────────────────────

#[test]
fn cbcs_to_cenc_to_clear_roundtrip() {
    let (original_seg, plaintext_samples) =
        build_cbcs_media_segment(3, 160, &TEST_SOURCE_KEY, 8);

    // CBCS → CENC
    let cenc_seg = segment::rewrite_segment(&original_seg, &cbcs_to_cenc_params()).unwrap();

    // CENC → Clear
    let clear_seg = segment::rewrite_segment(&cenc_seg, &cenc_to_clear_params()).unwrap();

    let mdat_payload = extract_mdat_payload(&clear_seg);
    let mut expected_payload = Vec::new();
    for sample in &plaintext_samples {
        expected_payload.extend_from_slice(sample);
    }
    assert_eq!(
        mdat_payload, expected_payload,
        "CBCS→CENC→clear roundtrip must recover original plaintext"
    );
}

#[test]
fn clear_to_encrypted_to_clear_roundtrip() {
    let (original_seg, plaintext_samples) = build_clear_media_segment(3, 160);

    // Clear → CENC
    let encrypted_seg =
        segment::rewrite_segment(&original_seg, &clear_to_cenc_params()).unwrap();

    // CENC → Clear
    let clear_seg = segment::rewrite_segment(&encrypted_seg, &cenc_to_clear_params()).unwrap();

    let mdat_payload = extract_mdat_payload(&clear_seg);
    let mut expected_payload = Vec::new();
    for sample in &plaintext_samples {
        expected_payload.extend_from_slice(sample);
    }
    assert_eq!(
        mdat_payload, expected_payload,
        "clear→encrypted→clear roundtrip must recover original plaintext"
    );
}

// ─── Manifest Conformance ────────────────────────────────────────────

#[test]
fn hls_manifest_segment_count_matches() {
    use edgepack::manifest;
    use edgepack::manifest::types::ManifestPhase;
    let state = make_hls_manifest_state(5, ManifestPhase::Complete);
    let text = manifest::render_manifest(&state).unwrap();
    assert_eq!(text.matches("#EXTINF:").count(), 5);
}

#[test]
fn dash_manifest_segment_count_matches() {
    use edgepack::manifest;
    use edgepack::manifest::types::ManifestPhase;
    let state = make_dash_manifest_state(5, ManifestPhase::Complete);
    let text = manifest::render_manifest(&state).unwrap();
    assert_eq!(text.matches("<S ").count(), 5);
}

#[test]
fn hls_complete_manifest_has_endlist() {
    use edgepack::manifest;
    use edgepack::manifest::types::ManifestPhase;
    let state = make_hls_manifest_state(3, ManifestPhase::Complete);
    let text = manifest::render_manifest(&state).unwrap();
    assert!(text.contains("#EXT-X-ENDLIST"));
}

#[test]
fn hls_live_manifest_no_endlist() {
    use edgepack::manifest;
    use edgepack::manifest::types::ManifestPhase;
    let state = make_hls_manifest_state(3, ManifestPhase::Live);
    let text = manifest::render_manifest(&state).unwrap();
    assert!(!text.contains("#EXT-X-ENDLIST"));
}

#[test]
fn dash_complete_manifest_is_static() {
    use edgepack::manifest;
    use edgepack::manifest::types::ManifestPhase;
    let state = make_dash_manifest_state(3, ManifestPhase::Complete);
    let text = manifest::render_manifest(&state).unwrap();
    assert!(text.contains("type=\"static\""));
}

#[test]
fn dash_live_manifest_is_dynamic() {
    use edgepack::manifest;
    use edgepack::manifest::types::ManifestPhase;
    let state = make_dash_manifest_state(3, ManifestPhase::Live);
    let text = manifest::render_manifest(&state).unwrap();
    assert!(text.contains("type=\"dynamic\""));
}
