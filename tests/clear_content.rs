//! Integration tests for Phase 3: Unencrypted input support.
//!
//! Tests the four pipeline paths:
//! - clear → CENC (init + segment)
//! - clear → CBCS (init + segment)
//! - encrypted → clear (init + segment)
//! - clear → clear (init + segment pass-through)

mod common;

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::drm::ContentKey;
use edgepack::media::container::ContainerFormat;
use edgepack::media::init;
use edgepack::media::segment::{rewrite_segment, SegmentRewriteParams};

// ─── Init Segment Tests ─────────────────────────────────────────────

#[test]
fn clear_to_cenc_init_segment() {
    let init = common::build_clear_init_segment();
    let key_set = common::make_drm_key_set();

    let result = init::create_protection_info(
        &init,
        &key_set,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Verify sinf injected
    assert!(result.windows(4).any(|w| w == b"sinf"), "should contain sinf");
    assert!(result.windows(4).any(|w| w == b"frma"), "should contain frma");
    assert!(result.windows(4).any(|w| w == b"schm"), "should contain schm");
    assert!(result.windows(4).any(|w| w == b"tenc"), "should contain tenc");

    // Verify PSSH boxes added
    assert!(result.windows(4).any(|w| w == b"pssh"), "should contain pssh");

    // Verify sample entry renamed to encv
    assert!(result.windows(4).any(|w| w == b"encv"), "should have encv sample entry");

    // Verify scheme type is cenc inside schm
    assert!(result.windows(4).any(|w| w == b"cenc"), "should contain cenc scheme");
}

#[test]
fn clear_to_cbcs_init_segment() {
    let init = common::build_clear_init_segment();
    let key_set = common::make_drm_key_set_with_fairplay();

    let result = init::create_protection_info(
        &init,
        &key_set,
        EncryptionScheme::Cbcs,
        16,
        (1, 9),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Verify CBCS-specific features
    assert!(result.windows(4).any(|w| w == b"cbcs"), "should contain cbcs scheme");
    assert!(result.windows(4).any(|w| w == b"encv"), "should have encv sample entry");
    assert!(result.windows(4).any(|w| w == b"sinf"), "should contain sinf");
    assert!(result.windows(4).any(|w| w == b"pssh"), "should contain pssh");
}

#[test]
fn encrypted_to_clear_init_segment() {
    let init = common::build_cbcs_init_segment();

    // Verify source has encryption info
    let source_info = init::parse_protection_info(&init).unwrap();
    assert!(source_info.is_some(), "source should have protection info");

    let result = init::strip_protection_info(&init, ContainerFormat::Cmaf).unwrap();

    // Verify sinf stripped
    assert!(!result.windows(4).any(|w| w == b"sinf"), "should not contain sinf");

    // Verify PSSH removed
    assert!(!result.windows(4).any(|w| w == b"pssh"), "should not contain pssh");

    // Verify sample entry restored to avc1
    assert!(result.windows(4).any(|w| w == b"avc1"), "should have avc1 sample entry");
    assert!(!result.windows(4).any(|w| w == b"encv"), "should not have encv sample entry");

    // Verify result parses as clear
    let clear_info = init::parse_protection_info(&result).unwrap();
    assert!(clear_info.is_none(), "cleared init should have no protection info");
}

#[test]
fn clear_to_clear_init_segment() {
    let init = common::build_clear_init_segment();

    let result = init::rewrite_ftyp_only(&init, ContainerFormat::Cmaf).unwrap();

    // Should still be clear
    assert!(!result.windows(4).any(|w| w == b"sinf"), "should not inject sinf");
    assert!(!result.windows(4).any(|w| w == b"pssh"), "should not inject pssh");

    // Should preserve avc1
    assert!(result.windows(4).any(|w| w == b"avc1"), "should preserve avc1");

    // Should have moov
    assert!(result.windows(4).any(|w| w == b"moov"), "should have moov");

    // Should have ftyp with CMAF brands
    assert!(result.windows(4).any(|w| w == b"cmfc"), "should have cmfc brand");
}

#[test]
fn clear_to_encrypted_then_strip_roundtrip() {
    let init = common::build_clear_init_segment();
    let key_set = common::make_drm_key_set();

    // Clear → Encrypted
    let encrypted = init::create_protection_info(
        &init,
        &key_set,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    assert!(encrypted.windows(4).any(|w| w == b"sinf"));
    assert!(encrypted.windows(4).any(|w| w == b"encv"));

    // Encrypted → Clear
    let clear = init::strip_protection_info(&encrypted, ContainerFormat::Cmaf).unwrap();

    assert!(!clear.windows(4).any(|w| w == b"sinf"));
    assert!(!clear.windows(4).any(|w| w == b"pssh"));
    assert!(clear.windows(4).any(|w| w == b"avc1"));
}

// ─── Segment Tests ──────────────────────────────────────────────────

#[test]
fn clear_to_cenc_segment() {
    let (segment, _plaintext) = common::build_clear_media_segment(2, 64);

    let params = SegmentRewriteParams {
        source_key: None,
        target_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_SOURCE_KEY.to_vec(),
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

    let result = rewrite_segment(&segment, &params).unwrap();

    // Should have senc injected
    assert!(result.windows(4).any(|w| w == b"senc"), "should contain senc");

    // mdat should be different (encrypted)
    assert_ne!(result, segment, "encrypted segment should differ from clear");
}

#[test]
fn clear_to_cbcs_segment() {
    let (segment, _plaintext) = common::build_clear_media_segment(2, 48);

    let params = SegmentRewriteParams {
        source_key: None,
        target_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }),
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::Cbcs,
        source_iv_size: 0,
        target_iv_size: 16,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    let result = rewrite_segment(&segment, &params).unwrap();
    assert!(result.windows(4).any(|w| w == b"senc"), "should contain senc");
    assert_ne!(result, segment);
}

#[test]
fn encrypted_to_clear_segment() {
    // First create an encrypted segment by encrypting clear data
    let (clear_segment, _) = common::build_clear_media_segment(2, 64);
    let key = ContentKey {
        kid: common::TEST_KID,
        key: common::TEST_SOURCE_KEY.to_vec(),
        iv: None,
    };

    let encrypted = rewrite_segment(&clear_segment, &SegmentRewriteParams {
        source_key: None,
        target_key: Some(key.clone()),
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 0,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    })
    .unwrap();

    assert!(encrypted.windows(4).any(|w| w == b"senc"));

    // Now decrypt back to clear
    let decrypted = rewrite_segment(&encrypted, &SegmentRewriteParams {
        source_key: Some(key),
        target_key: None,
        source_scheme: EncryptionScheme::Cenc,
        target_scheme: EncryptionScheme::None,
        source_iv_size: 8,
        target_iv_size: 0,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    })
    .unwrap();

    // senc should be stripped
    assert!(!decrypted.windows(4).any(|w| w == b"senc"), "should not contain senc");
}

#[test]
fn clear_to_clear_segment_passthrough() {
    let (segment, _) = common::build_clear_media_segment(2, 64);

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

    let result = rewrite_segment(&segment, &params).unwrap();

    // Should be byte-for-byte identical (pass-through)
    assert_eq!(result, segment, "clear-to-clear should be identical");
}

#[test]
fn clear_to_cenc_to_clear_segment_roundtrip() {
    let (clear_segment, plaintext_samples) = common::build_clear_media_segment(3, 48);
    let key = ContentKey {
        kid: common::TEST_KID,
        key: common::TEST_SOURCE_KEY.to_vec(),
        iv: None,
    };

    // Clear → CENC
    let encrypted = rewrite_segment(&clear_segment, &SegmentRewriteParams {
        source_key: None,
        target_key: Some(key.clone()),
        source_scheme: EncryptionScheme::None,
        target_scheme: EncryptionScheme::Cenc,
        source_iv_size: 0,
        target_iv_size: 8,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    })
    .unwrap();

    // CENC → Clear
    let decrypted = rewrite_segment(&encrypted, &SegmentRewriteParams {
        source_key: Some(key),
        target_key: None,
        source_scheme: EncryptionScheme::Cenc,
        target_scheme: EncryptionScheme::None,
        source_iv_size: 8,
        target_iv_size: 0,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    })
    .unwrap();

    // Extract mdat payloads and compare
    let orig_mdat = extract_mdat_payload(&clear_segment);
    let dec_mdat = extract_mdat_payload(&decrypted);
    assert_eq!(orig_mdat, dec_mdat, "mdat payload should match after roundtrip");

    // Verify against known plaintext
    let mut expected_payload = Vec::new();
    for sample in &plaintext_samples {
        expected_payload.extend_from_slice(sample);
    }
    assert_eq!(dec_mdat, expected_payload);
}

/// Helper to extract mdat payload from a segment.
fn extract_mdat_payload(segment: &[u8]) -> Vec<u8> {
    let mdat_pos = segment
        .windows(4)
        .position(|w| w == b"mdat")
        .expect("no mdat found");
    let size_start = mdat_pos - 4;
    let mdat_size = u32::from_be_bytes([
        segment[size_start],
        segment[size_start + 1],
        segment[size_start + 2],
        segment[size_start + 3],
    ]) as usize;
    segment[size_start + 8..size_start + mdat_size].to_vec()
}
