//! Integration tests for Phase 5: Multi-Key DRM & Codec Awareness.
//!
//! Tests per-track keying, multi-KID PSSH generation, codec string extraction,
//! and backward compatibility with single-key content.

mod common;

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::drm::{system_ids, ContentKey, DrmKeySet, DrmSystemData};
use edgepack::media::codec::{extract_tracks, TrackKeyMapping};
use edgepack::media::container::ContainerFormat;
use edgepack::media::init;
use edgepack::media::TrackType;

// ─── Constants ──────────────────────────────────────────────────────

const VIDEO_KID: [u8; 16] = [
    0xAA, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
    0xFF,
];
const AUDIO_KID: [u8; 16] = [
    0xBB, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
    0xFF,
];
const VIDEO_KEY: [u8; 16] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x10,
];
const AUDIO_KEY: [u8; 16] = [
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
    0x20,
];

// ─── Helpers ────────────────────────────────────────────────────────

/// Build a multi-key DrmKeySet with separate video and audio keys.
fn make_multi_key_set() -> DrmKeySet {
    DrmKeySet {
        keys: vec![
            ContentKey {
                kid: VIDEO_KID,
                key: VIDEO_KEY.to_vec(),
                iv: None,
            },
            ContentKey {
                kid: AUDIO_KID,
                key: AUDIO_KEY.to_vec(),
                iv: None,
            },
        ],
        drm_systems: vec![
            DrmSystemData {
                system_id: system_ids::WIDEVINE,
                kid: VIDEO_KID,
                pssh_data: vec![0x08, 0x01, 0x12, 0x10],
                content_protection_data: None,
            },
            DrmSystemData {
                system_id: system_ids::WIDEVINE,
                kid: AUDIO_KID,
                pssh_data: vec![0x08, 0x01, 0x12, 0x10],
                content_protection_data: None,
            },
            DrmSystemData {
                system_id: system_ids::PLAYREADY,
                kid: VIDEO_KID,
                pssh_data: vec![0x48, 0x00, 0x65, 0x00],
                content_protection_data: Some("<WRMHEADER/>".into()),
            },
            DrmSystemData {
                system_id: system_ids::PLAYREADY,
                kid: AUDIO_KID,
                pssh_data: vec![0x48, 0x00, 0x65, 0x00],
                content_protection_data: None,
            },
        ],
    }
}

// ─── Multi-Key Init Rewriting ───────────────────────────────────────

#[test]
fn multi_key_clear_to_cenc_per_track_tenc() {
    let init = common::build_clear_init_segment();
    let key_set = make_multi_key_set();

    // Per-track key mapping: video gets VIDEO_KID, audio gets AUDIO_KID
    let key_mapping = TrackKeyMapping::per_type(VIDEO_KID, AUDIO_KID);

    let result = init::create_protection_info(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Should have sinf, encv, cenc scheme, and pssh
    assert!(result.windows(4).any(|w| w == b"sinf"), "should contain sinf");
    assert!(result.windows(4).any(|w| w == b"encv"), "should have encv");
    assert!(result.windows(4).any(|w| w == b"cenc"), "should have cenc scheme");
    assert!(result.windows(4).any(|w| w == b"pssh"), "should contain pssh");

    // The tenc in the result should have the VIDEO_KID (since the init has a video track)
    assert!(
        result.windows(16).any(|w| w == VIDEO_KID),
        "should contain video KID in tenc"
    );
}

#[test]
fn multi_key_clear_to_cbcs_per_track_tenc() {
    let init = common::build_clear_init_segment();
    let key_set = make_multi_key_set();

    let key_mapping = TrackKeyMapping::per_type(VIDEO_KID, AUDIO_KID);

    let result = init::create_protection_info(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cbcs,
        16,
        (1, 9),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    assert!(result.windows(4).any(|w| w == b"cbcs"), "should have cbcs scheme");
    assert!(result.windows(4).any(|w| w == b"sinf"), "should contain sinf");
    assert!(result.windows(4).any(|w| w == b"pssh"), "should contain pssh");
}

// ─── Multi-KID PSSH ────────────────────────────────────────────────

#[test]
fn multi_key_pssh_contains_both_kids() {
    let init = common::build_clear_init_segment();
    let key_set = make_multi_key_set();

    let key_mapping = TrackKeyMapping::per_type(VIDEO_KID, AUDIO_KID);

    let result = init::create_protection_info(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Find PSSH boxes and verify they contain both KIDs
    // Since key_mapping.is_multi_key(), build_pssh_boxes should merge KIDs
    assert!(
        result.windows(16).any(|w| w == VIDEO_KID),
        "PSSH should contain video KID"
    );
    assert!(
        result.windows(16).any(|w| w == AUDIO_KID),
        "PSSH should contain audio KID"
    );
}

// ─── Single-Key Backward Compatibility ──────────────────────────────

#[test]
fn single_key_init_rewrite_backward_compat() {
    let init = common::build_clear_init_segment();
    let key_set = common::make_drm_key_set();

    // Use TrackKeyMapping::single — same behavior as before multi-key support
    let key_mapping = TrackKeyMapping::single(key_set.keys[0].kid);

    let result = init::create_protection_info(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    assert!(result.windows(4).any(|w| w == b"sinf"));
    assert!(result.windows(4).any(|w| w == b"encv"));
    assert!(result.windows(4).any(|w| w == b"pssh"));

    // Verify the single KID is present
    assert!(result.windows(16).any(|w| w == common::TEST_KID));
}

#[test]
fn single_key_encrypted_to_encrypted_rewrite_backward_compat() {
    let init = common::build_cbcs_init_segment();
    let key_set = common::make_drm_key_set();

    let key_mapping = TrackKeyMapping::single(key_set.keys[0].kid);

    let result = init::rewrite_init_segment(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    // Should have CENC scheme instead of CBCS
    assert!(result.windows(4).any(|w| w == b"cenc"), "should have cenc scheme");
    assert!(!result.windows(4).any(|w| w == b"cbcs"), "should not have cbcs scheme");
    assert!(result.windows(4).any(|w| w == b"sinf"));
    assert!(result.windows(4).any(|w| w == b"pssh"));
}

// ─── Codec String Extraction ────────────────────────────────────────

#[test]
fn extract_tracks_from_clear_init() {
    let init = common::build_clear_init_segment();

    let tracks = extract_tracks(&init).unwrap();
    // The clear init has one track (avc1) but no hdlr box in the minimal fixture,
    // so track_type may be Unknown. The important thing is extraction doesn't fail.
    assert!(!tracks.is_empty(), "should extract at least one track");
}

#[test]
fn extract_tracks_from_encrypted_init() {
    let init = common::build_cbcs_init_segment();

    let tracks = extract_tracks(&init).unwrap();
    // The encrypted init has one track (encv). Should extract the KID.
    assert!(!tracks.is_empty(), "should extract at least one track");
}

// ─── TrackKeyMapping Serialization ──────────────────────────────────

#[test]
fn track_key_mapping_serde_roundtrip_single() {
    let mapping = TrackKeyMapping::single([0x42; 16]);

    let json = serde_json::to_string(&mapping).unwrap();
    let parsed: TrackKeyMapping = serde_json::from_str(&json).unwrap();

    assert!(!parsed.is_multi_key());
    assert_eq!(parsed.all_kids(), vec![[0x42; 16]]);
    assert_eq!(
        parsed.kid_for_track(TrackType::Video),
        Some(&[0x42; 16])
    );
    assert_eq!(
        parsed.kid_for_track(TrackType::Audio),
        Some(&[0x42; 16])
    );
}

#[test]
fn track_key_mapping_serde_roundtrip_multi() {
    let mapping = TrackKeyMapping::per_type(VIDEO_KID, AUDIO_KID);

    let json = serde_json::to_string(&mapping).unwrap();
    let parsed: TrackKeyMapping = serde_json::from_str(&json).unwrap();

    assert!(parsed.is_multi_key());
    let kids = parsed.all_kids();
    assert_eq!(kids.len(), 2);
    assert!(kids.contains(&VIDEO_KID));
    assert!(kids.contains(&AUDIO_KID));
    assert_eq!(parsed.kid_for_track(TrackType::Video), Some(&VIDEO_KID));
    assert_eq!(parsed.kid_for_track(TrackType::Audio), Some(&AUDIO_KID));
}

// ─── Multi-Key Strip and Roundtrip ──────────────────────────────────

#[test]
fn multi_key_create_then_strip_roundtrip() {
    let init = common::build_clear_init_segment();
    let key_set = make_multi_key_set();

    let key_mapping = TrackKeyMapping::per_type(VIDEO_KID, AUDIO_KID);

    // Clear → Encrypted
    let encrypted = init::create_protection_info(
        &init,
        &key_set,
        &key_mapping,
        EncryptionScheme::Cenc,
        8,
        (0, 0),
        ContainerFormat::Cmaf,
    )
    .unwrap();

    assert!(encrypted.windows(4).any(|w| w == b"sinf"));
    assert!(encrypted.windows(4).any(|w| w == b"encv"));
    assert!(encrypted.windows(4).any(|w| w == b"pssh"));

    // Encrypted → Clear
    let clear = init::strip_protection_info(&encrypted, ContainerFormat::Cmaf).unwrap();

    assert!(!clear.windows(4).any(|w| w == b"sinf"), "should not contain sinf");
    assert!(!clear.windows(4).any(|w| w == b"pssh"), "should not contain pssh");
    assert!(clear.windows(4).any(|w| w == b"avc1"), "should restore avc1");
    assert!(!clear.windows(4).any(|w| w == b"encv"), "should not have encv");

    // Verify result parses as clear
    let clear_info = init::parse_protection_info(&clear).unwrap();
    assert!(clear_info.is_none(), "cleared init should have no protection info");
}

// ─── Key Mapping from Tracks ────────────────────────────────────────

#[test]
fn track_key_mapping_from_tracks_preserves_per_track_kids() {
    use edgepack::media::codec::TrackInfo;

    let tracks = vec![
        TrackInfo {
            track_type: TrackType::Video,
            track_id: 1,
            codec_string: "avc1.64001f".to_string(),
            timescale: 90000,
            kid: Some(VIDEO_KID),
            language: None,
            width: Some(1920),
            height: Some(1080),
        },
        TrackInfo {
            track_type: TrackType::Audio,
            track_id: 2,
            codec_string: "mp4a.40.2".to_string(),
            timescale: 44100,
            kid: Some(AUDIO_KID),
            language: None,
            width: None,
            height: None,
        },
    ];

    let mapping = TrackKeyMapping::from_tracks(&tracks);
    assert!(mapping.is_multi_key());
    assert_eq!(mapping.kid_for_track(TrackType::Video), Some(&VIDEO_KID));
    assert_eq!(mapping.kid_for_track(TrackType::Audio), Some(&AUDIO_KID));

    // All unique KIDs
    let kids = mapping.all_kids();
    assert_eq!(kids.len(), 2);
}

#[test]
fn track_key_mapping_from_tracks_same_kid_is_single() {
    use edgepack::media::codec::TrackInfo;

    let shared_kid = [0xCC; 16];
    let tracks = vec![
        TrackInfo {
            track_type: TrackType::Video,
            track_id: 1,
            codec_string: "avc1.64001f".to_string(),
            timescale: 90000,
            kid: Some(shared_kid),
            language: None,
            width: Some(1920),
            height: Some(1080),
        },
        TrackInfo {
            track_type: TrackType::Audio,
            track_id: 2,
            codec_string: "mp4a.40.2".to_string(),
            timescale: 44100,
            kid: Some(shared_kid),
            language: None,
            width: None,
            height: None,
        },
    ];

    let mapping = TrackKeyMapping::from_tracks(&tracks);
    assert!(!mapping.is_multi_key(), "same KID for both tracks should be single-key");
    assert_eq!(mapping.all_kids(), vec![shared_kid]);
}
