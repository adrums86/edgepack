//! Integration tests: ISOBMFF/CMAF box parsing, init segment rewriting,
//! and media segment rewriting with mock data.
//!
//! Tests the media processing pipeline:
//! - Build synthetic CBCS init segment → parse → verify structure
//! - Rewrite init segment from CBCS to CENC → verify output
//! - Build synthetic media segment → rewrite → verify output

mod common;

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::drm::{system_ids, ContentKey};
use edgepack::media::cmaf::{self, iterate_boxes, parse_pssh, parse_tenc};
use edgepack::media::container::ContainerFormat;
use edgepack::media::init::{parse_protection_info, rewrite_init_segment};
use edgepack::media::segment::{rewrite_segment, SegmentRewriteParams};

// ─── Init Segment Parsing ───────────────────────────────────────────

#[test]
fn parse_synthetic_cbcs_init_segment_finds_protection_info() {
    let init_data = common::build_cbcs_init_segment();

    let result = parse_protection_info(&init_data).expect("parse should succeed");
    let info = result.expect("should find protection info");

    assert_eq!(&info.original_format, b"avc1");
    assert_eq!(&info.scheme_type, b"cbcs");
    assert_eq!(info.tenc.is_protected, 1);
    assert_eq!(info.tenc.default_per_sample_iv_size, 8);
    assert_eq!(info.tenc.default_crypt_byte_block, 1);
    assert_eq!(info.tenc.default_skip_byte_block, 9);
    assert_eq!(info.tenc.default_kid, common::TEST_KID);
}

#[test]
fn synthetic_init_segment_has_correct_box_structure() {
    let init_data = common::build_cbcs_init_segment();

    // Verify top-level boxes
    let top_boxes: Vec<_> = iterate_boxes(&init_data)
        .collect::<Result<Vec<_>, _>>()
        .expect("should parse all boxes");

    assert!(top_boxes.len() >= 2, "should have ftyp + moov at minimum");
    assert_eq!(top_boxes[0].box_type, *b"ftyp");
    assert_eq!(top_boxes[1].box_type, *b"moov");
}

#[test]
fn synthetic_init_segment_contains_pssh() {
    let init_data = common::build_cbcs_init_segment();

    // Search for PSSH box
    let has_pssh = init_data
        .windows(4)
        .any(|w| w == b"pssh");

    assert!(has_pssh, "init segment should contain a PSSH box");
}

// ─── Init Segment Rewriting (CBCS → CENC) ───────────────────────────

#[test]
fn rewrite_init_segment_cbcs_to_cenc() {
    let init_data = common::build_cbcs_init_segment();
    let key_set = common::make_drm_key_set();

    let rewritten = rewrite_init_segment(&init_data, &key_set, EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::default())
        .expect("rewrite should succeed");

    // Verify the rewritten segment has correct structure
    let top_boxes: Vec<_> = iterate_boxes(&rewritten)
        .collect::<Result<Vec<_>, _>>()
        .expect("should parse rewritten segment");

    assert!(top_boxes.len() >= 2);
    assert_eq!(top_boxes[0].box_type, *b"ftyp");
    assert_eq!(top_boxes[1].box_type, *b"moov");
}

#[test]
fn rewritten_init_segment_has_cenc_pssh_boxes() {
    let init_data = common::build_cbcs_init_segment();
    let key_set = common::make_drm_key_set();

    let rewritten = rewrite_init_segment(&init_data, &key_set, EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::default())
        .expect("rewrite should succeed");

    // Count PSSH boxes and verify they are for CENC systems (Widevine + PlayReady)
    let mut pssh_system_ids = Vec::new();
    let mut pos = 0;
    while pos + 8 <= rewritten.len() {
        if &rewritten[pos + 4..pos + 8] == b"pssh" {
            let size = u32::from_be_bytes([
                rewritten[pos], rewritten[pos + 1], rewritten[pos + 2], rewritten[pos + 3],
            ]) as usize;

            if size >= 8 && pos + size <= rewritten.len() {
                // Parse the PSSH payload (skip box header)
                let pssh_payload = &rewritten[pos + 8..pos + size];
                if let Ok(pssh) = parse_pssh(pssh_payload) {
                    pssh_system_ids.push(pssh.system_id);
                }
            }
            pos += size;
        } else {
            pos += 1;
        }
    }

    assert!(
        pssh_system_ids.len() >= 2,
        "should have at least 2 PSSH boxes (Widevine + PlayReady), found {}",
        pssh_system_ids.len()
    );

    // Verify Widevine PSSH is present
    assert!(
        pssh_system_ids.contains(&system_ids::WIDEVINE),
        "should contain Widevine PSSH"
    );

    // Verify PlayReady PSSH is present
    assert!(
        pssh_system_ids.contains(&system_ids::PLAYREADY),
        "should contain PlayReady PSSH"
    );
}

#[test]
fn rewritten_init_segment_excludes_fairplay() {
    let init_data = common::build_cbcs_init_segment();
    let key_set = common::make_drm_key_set_with_fairplay();

    let rewritten = rewrite_init_segment(&init_data, &key_set, EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::default())
        .expect("rewrite should succeed");

    // Verify no FairPlay PSSH boxes
    let mut pos = 0;
    while pos + 8 <= rewritten.len() {
        if &rewritten[pos + 4..pos + 8] == b"pssh" {
            let size = u32::from_be_bytes([
                rewritten[pos], rewritten[pos + 1], rewritten[pos + 2], rewritten[pos + 3],
            ]) as usize;

            if size >= 8 && pos + size <= rewritten.len() {
                let pssh_payload = &rewritten[pos + 8..pos + size];
                if let Ok(pssh) = parse_pssh(pssh_payload) {
                    assert_ne!(
                        pssh.system_id,
                        system_ids::FAIRPLAY,
                        "FairPlay PSSH should NOT be in rewritten CENC init segment"
                    );
                }
            }
            pos += size;
        } else {
            pos += 1;
        }
    }
}

#[test]
fn rewritten_init_contains_cenc_scheme() {
    let init_data = common::build_cbcs_init_segment();
    let key_set = common::make_drm_key_set();

    let rewritten = rewrite_init_segment(&init_data, &key_set, EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::default())
        .expect("rewrite should succeed");

    // Verify 'cenc' scheme appears in the rewritten data
    let has_cenc_scheme = rewritten.windows(4).any(|w| w == b"cenc");
    assert!(has_cenc_scheme, "rewritten segment should contain 'cenc' scheme type");
}

#[test]
fn rewritten_init_preserves_ftyp() {
    let init_data = common::build_cbcs_init_segment();
    let key_set = common::make_drm_key_set();

    let rewritten = rewrite_init_segment(&init_data, &key_set, EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::default())
        .expect("rewrite should succeed");

    // ftyp box should be identical (copied as-is)
    let orig_ftyp_size = u32::from_be_bytes([
        init_data[0], init_data[1], init_data[2], init_data[3],
    ]) as usize;
    let rewritten_ftyp_size = u32::from_be_bytes([
        rewritten[0], rewritten[1], rewritten[2], rewritten[3],
    ]) as usize;

    assert_eq!(orig_ftyp_size, rewritten_ftyp_size, "ftyp size should be preserved");
    assert_eq!(
        &init_data[..orig_ftyp_size],
        &rewritten[..rewritten_ftyp_size],
        "ftyp box should be identical"
    );
}

#[test]
fn rewrite_with_iv_size_16() {
    let init_data = common::build_cbcs_init_segment();
    let key_set = common::make_drm_key_set();

    let rewritten = rewrite_init_segment(&init_data, &key_set, EncryptionScheme::Cenc, 16, (0, 0), ContainerFormat::default())
        .expect("rewrite with IV size 16 should succeed");

    // Verify the tenc box in the output uses IV size 16
    // Search for tenc box and parse it
    for pos in 0..rewritten.len().saturating_sub(7) {
        if &rewritten[pos + 4..pos + 8] == b"tenc" {
            let size = u32::from_be_bytes([
                rewritten[pos], rewritten[pos + 1], rewritten[pos + 2], rewritten[pos + 3],
            ]) as usize;
            if size >= 8 && pos + size <= rewritten.len() {
                let tenc_payload = &rewritten[pos + 8..pos + size];
                let tenc = parse_tenc(tenc_payload).expect("tenc should parse");
                assert_eq!(
                    tenc.default_per_sample_iv_size, 16,
                    "tenc should have IV size 16"
                );
                assert_eq!(
                    tenc.default_crypt_byte_block, 0,
                    "CENC tenc should have no pattern"
                );
                assert_eq!(
                    tenc.default_skip_byte_block, 0,
                    "CENC tenc should have no skip pattern"
                );
                return;
            }
        }
    }
    panic!("Could not find tenc box in rewritten init segment");
}

// ─── Media Segment Rewriting ────────────────────────────────────────

#[test]
fn rewrite_media_segment_basic_roundtrip() {
    // Build a synthetic CBCS-encrypted segment
    let (segment_data, original_plaintext) =
        common::build_cbcs_media_segment(2, 64, &common::TEST_SOURCE_KEY, 8);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }),
        target_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_TARGET_KEY.to_vec(),
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

    let rewritten = rewrite_segment(&segment_data, &params)
        .expect("segment rewrite should succeed");

    // Verify the output contains moof + mdat
    let boxes: Vec<_> = iterate_boxes(&rewritten)
        .collect::<Result<Vec<_>, _>>()
        .expect("should parse rewritten segment");

    let moof_boxes: Vec<_> = boxes.iter().filter(|b| b.box_type == *b"moof").collect();
    let mdat_boxes: Vec<_> = boxes.iter().filter(|b| b.box_type == *b"mdat").collect();

    assert_eq!(moof_boxes.len(), 1, "should have exactly 1 moof box");
    assert_eq!(mdat_boxes.len(), 1, "should have exactly 1 mdat box");

    // The mdat payload should have the same total size as the original samples
    let mdat_header = &mdat_boxes[0];
    let mdat_payload_size = mdat_header.payload_size() as usize;
    let original_total: usize = original_plaintext.iter().map(|s| s.len()).sum();
    assert_eq!(
        mdat_payload_size, original_total,
        "mdat payload size should match original sample sizes"
    );
}

#[test]
fn rewrite_media_segment_mdat_is_encrypted() {
    let (segment_data, original_plaintext) =
        common::build_cbcs_media_segment(1, 64, &common::TEST_SOURCE_KEY, 8);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }),
        target_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_TARGET_KEY.to_vec(),
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

    let rewritten = rewrite_segment(&segment_data, &params).unwrap();

    // Find the mdat payload in the rewritten output
    for box_result in iterate_boxes(&rewritten) {
        let header: cmaf::BoxHeader = box_result.unwrap();
        if header.box_type == *b"mdat" {
            let mdat_start = header.payload_offset() as usize;
            let mdat_end = (header.offset + header.size) as usize;
            let mdat_payload = &rewritten[mdat_start..mdat_end];

            // The mdat should NOT contain the original plaintext (it's now CENC-encrypted)
            assert_ne!(
                mdat_payload,
                original_plaintext[0].as_slice(),
                "mdat should be CENC-encrypted, not plaintext"
            );
            return;
        }
    }
    panic!("mdat box not found in rewritten output");
}

#[test]
fn rewrite_segment_multiple_samples() {
    let sample_count = 5;
    let sample_size = 48; // 3 blocks each
    let (segment_data, _) =
        common::build_cbcs_media_segment(sample_count, sample_size, &common::TEST_SOURCE_KEY, 8);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }),
        target_key: Some(ContentKey {
            kid: common::TEST_KID,
            key: common::TEST_TARGET_KEY.to_vec(),
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

    let rewritten = rewrite_segment(&segment_data, &params)
        .expect("multi-sample rewrite should succeed");

    // Verify rewritten output has correct structure
    let boxes: Vec<_> = iterate_boxes(&rewritten)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(boxes.len(), 2, "should have moof + mdat");
    assert_eq!(boxes[0].box_type, *b"moof");
    assert_eq!(boxes[1].box_type, *b"mdat");

    // Verify mdat payload size matches expected
    let expected_mdat_size = sample_count * sample_size;
    let actual_mdat_size = boxes[1].payload_size() as usize;
    assert_eq!(actual_mdat_size, expected_mdat_size);
}

#[test]
fn rewrite_segment_error_on_missing_moof() {
    // Create a segment with only mdat (no moof)
    let mdat = common::wrap_box(b"mdat", &[0u8; 64]);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: [0; 16],
            key: vec![0; 16],
            iv: None,
        }),
        target_key: Some(ContentKey {
            kid: [0; 16],
            key: vec![0; 16],
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

    let result = rewrite_segment(&mdat, &params);
    assert!(result.is_err(), "should fail without moof box");
    assert!(
        result.unwrap_err().to_string().contains("no moof"),
        "error should mention missing moof"
    );
}

#[test]
fn rewrite_segment_error_on_missing_mdat() {
    // Create a segment with only moof (no mdat)
    let moof = common::wrap_box(b"moof", &[0u8; 64]);

    let params = SegmentRewriteParams {
        source_key: Some(ContentKey {
            kid: [0; 16],
            key: vec![0; 16],
            iv: None,
        }),
        target_key: Some(ContentKey {
            kid: [0; 16],
            key: vec![0; 16],
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

    let result = rewrite_segment(&moof, &params);
    assert!(result.is_err(), "should fail without mdat box");
    assert!(
        result.unwrap_err().to_string().contains("no mdat"),
        "error should mention missing mdat"
    );
}

// ─── PSSH Box Roundtrip ─────────────────────────────────────────────

#[test]
fn pssh_box_build_parse_roundtrip_v0() {
    let pssh = cmaf::PsshBox {
        version: 0,
        system_id: system_ids::WIDEVINE,
        key_ids: vec![],
        data: vec![0x08, 0x01, 0x12, 0x10, 0xDE, 0xAD, 0xBE, 0xEF],
    };

    let built = cmaf::build_pssh_box(&pssh);
    assert_eq!(&built[4..8], b"pssh");

    let header = cmaf::read_box_header(&built, 0).unwrap();
    let payload = &built[header.header_size as usize..];
    let parsed = parse_pssh(payload).unwrap();

    assert_eq!(parsed.version, 0);
    assert_eq!(parsed.system_id, system_ids::WIDEVINE);
    assert!(parsed.key_ids.is_empty());
    assert_eq!(parsed.data, pssh.data);
}

#[test]
fn pssh_box_build_parse_roundtrip_v1_with_kids() {
    let kid = common::TEST_KID;
    let pssh = cmaf::PsshBox {
        version: 1,
        system_id: system_ids::PLAYREADY,
        key_ids: vec![kid],
        data: vec![0x48, 0x00, 0x65, 0x00, 0x6C, 0x00],
    };

    let built = cmaf::build_pssh_box(&pssh);
    let header = cmaf::read_box_header(&built, 0).unwrap();
    let payload = &built[header.header_size as usize..];
    let parsed = parse_pssh(payload).unwrap();

    assert_eq!(parsed.version, 1);
    assert_eq!(parsed.system_id, system_ids::PLAYREADY);
    assert_eq!(parsed.key_ids.len(), 1);
    assert_eq!(parsed.key_ids[0], kid);
    assert_eq!(parsed.data, pssh.data);
}

// ─── senc Box Roundtrip ─────────────────────────────────────────────

#[test]
fn senc_box_build_parse_roundtrip_no_subsamples() {
    let entries = vec![
        cmaf::SencEntry {
            iv: vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
            subsamples: None,
        },
        cmaf::SencEntry {
            iv: vec![0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18],
            subsamples: None,
        },
    ];

    let built = cmaf::build_senc_box(&entries, false);
    let header = cmaf::read_box_header(&built, 0).unwrap();
    assert_eq!(header.box_type, *b"senc");

    let payload = &built[header.header_size as usize..];
    let parsed = cmaf::parse_senc(payload, 8).unwrap();

    assert_eq!(parsed.sample_count, 2);
    assert_eq!(parsed.entries[0].iv, entries[0].iv);
    assert_eq!(parsed.entries[1].iv, entries[1].iv);
    assert!(parsed.entries[0].subsamples.is_none());
}

#[test]
fn senc_box_build_parse_roundtrip_with_subsamples() {
    let entries = vec![cmaf::SencEntry {
        iv: vec![0xAA; 8],
        subsamples: Some(vec![
            cmaf::SubsampleEntry {
                clear_bytes: 5,
                encrypted_bytes: 48,
            },
            cmaf::SubsampleEntry {
                clear_bytes: 3,
                encrypted_bytes: 32,
            },
        ]),
    }];

    let built = cmaf::build_senc_box(&entries, true);
    let header = cmaf::read_box_header(&built, 0).unwrap();
    let payload = &built[header.header_size as usize..];
    let parsed = cmaf::parse_senc(payload, 8).unwrap();

    assert_eq!(parsed.sample_count, 1);
    let subs = parsed.entries[0].subsamples.as_ref().unwrap();
    assert_eq!(subs.len(), 2);
    assert_eq!(subs[0].clear_bytes, 5);
    assert_eq!(subs[0].encrypted_bytes, 48);
    assert_eq!(subs[1].clear_bytes, 3);
    assert_eq!(subs[1].encrypted_bytes, 32);
}
