//! Shared test fixtures and helpers for integration tests.
//!
//! Provides builders for synthetic CMAF init segments, media segments,
//! DRM key sets, and other mock data structures used across tests.
#![allow(dead_code)]

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::drm::{system_ids, ContentKey, DrmKeySet, DrmSystemData};
use edgepack::manifest::types::{
    ContentSteeringConfig, IFrameSegmentInfo, InitSegmentInfo, ManifestDrmInfo, ManifestPhase,
    ManifestState, OutputFormat, SegmentInfo,
};
use edgepack::media::cmaf;
use edgepack::media::codec::TrackKeyMapping;
use edgepack::media::container::ContainerFormat;
use edgepack::media::init;
use edgepack::media::segment::{rewrite_segment, SegmentRewriteParams};

// ─── DRM Fixtures ───────────────────────────────────────────────────

/// Standard AES-128 key used in tests.
pub const TEST_SOURCE_KEY: [u8; 16] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x10,
];

/// Standard AES-128 target key for CENC re-encryption.
pub const TEST_TARGET_KEY: [u8; 16] = [
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
    0x20,
];

/// Standard 16-byte Key ID (KID) for tests.
pub const TEST_KID: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
    0xFF,
];

/// Standard 16-byte IV for CBCS tests.
pub const TEST_IV: [u8; 16] = [
    0xA0, 0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xAB, 0xAC, 0xAD, 0xAE,
    0xAF,
];

/// Build a DRM key set with Widevine and PlayReady DRM system data.
pub fn make_drm_key_set() -> DrmKeySet {
    DrmKeySet {
        keys: vec![ContentKey {
            kid: TEST_KID,
            key: TEST_SOURCE_KEY.to_vec(),
            iv: None,
        }],
        drm_systems: vec![
            DrmSystemData {
                system_id: system_ids::WIDEVINE,
                kid: TEST_KID,
                pssh_data: vec![0x08, 0x01, 0x12, 0x10], // Minimal Widevine init data
                content_protection_data: None,
            },
            DrmSystemData {
                system_id: system_ids::PLAYREADY,
                kid: TEST_KID,
                pssh_data: vec![0x48, 0x00, 0x65, 0x00], // Minimal PlayReady data
                content_protection_data: Some("<WRMHEADER/>".into()),
            },
        ],
    }
}

/// Build a DRM key set that also includes a FairPlay entry (for testing filtering).
pub fn make_drm_key_set_with_fairplay() -> DrmKeySet {
    let mut ks = make_drm_key_set();
    ks.drm_systems.push(DrmSystemData {
        system_id: system_ids::FAIRPLAY,
        kid: TEST_KID,
        pssh_data: vec![0x00, 0x00, 0x00, 0x01],
        content_protection_data: None,
    });
    ks
}

// ─── ISOBMFF / CMAF Fixture Builders ────────────────────────────────

/// Build a minimal CBCS init segment with sinf box (ftyp + moov containing stsd→sinf).
///
/// Structure:
/// ```text
/// ftyp (file type box)
/// moov (movie box)
///   ├── mvhd (movie header)
///   ├── trak (track box)
///   │   └── mdia
///   │       └── minf
///   │           └── stbl
///   │               └── stsd (sample description)
///   │                   └── encv (encrypted video sample entry)
///   │                       ├── ... (sample entry prefix bytes)
///   │                       └── sinf (protection scheme info)
///   │                           ├── frma (original format = "avc1")
///   │                           ├── schm (scheme = "cbcs")
///   │                           └── schi
///   │                               └── tenc (track encryption box)
///   └── pssh (Widevine PSSH — to be replaced)
/// ```
pub fn build_cbcs_init_segment() -> Vec<u8> {
    let mut data = Vec::new();

    // ftyp box
    let ftyp_payload = b"isom\x00\x00\x02\x00isomiso6cmfc";
    let ftyp_size = 8 + ftyp_payload.len() as u32;
    cmaf::write_box_header(&mut data, ftyp_size, b"ftyp");
    data.extend_from_slice(ftyp_payload);

    // Build moov children
    let mut moov_children = Vec::new();

    // mvhd (minimal)
    let mut mvhd = Vec::new();
    cmaf::write_full_box_header(&mut mvhd, 120, b"mvhd", 1, 0);
    mvhd.resize(120, 0); // Fill rest with zeros (timescale etc.)
    moov_children.extend_from_slice(&mvhd);

    // trak → mdia → minf → stbl → stsd → encv → sinf
    let sinf = build_cbcs_sinf();
    let encv = build_sample_entry(b"encv", &sinf);
    let stsd = build_stsd(&encv);
    let stbl = wrap_box(b"stbl", &stsd);
    let minf = wrap_box(b"minf", &stbl);
    let mdia = wrap_box(b"mdia", &minf);
    let trak = wrap_box(b"trak", &mdia);
    moov_children.extend_from_slice(&trak);

    // Add a PSSH box (Widevine) — will be replaced during rewriting
    let pssh = cmaf::build_pssh_box(&cmaf::PsshBox {
        version: 0,
        system_id: system_ids::WIDEVINE,
        key_ids: vec![],
        data: vec![0x08, 0x01],
    });
    moov_children.extend_from_slice(&pssh);

    // Wrap in moov
    let moov_size = 8 + moov_children.len() as u32;
    cmaf::write_box_header(&mut data, moov_size, b"moov");
    data.extend_from_slice(&moov_children);

    data
}

/// Build a sinf box for CBCS scheme (frma + schm + schi/tenc).
fn build_cbcs_sinf() -> Vec<u8> {
    let mut sinf_children = Vec::new();

    // frma: original_format = "avc1"
    let frma_size: u32 = 12;
    cmaf::write_box_header(&mut sinf_children, frma_size, b"frma");
    sinf_children.extend_from_slice(b"avc1");

    // schm: version(1) + flags(3) + scheme_type("cbcs") + scheme_version(0x00010000)
    let schm_size: u32 = 8 + 4 + 4 + 4;
    cmaf::write_box_header(&mut sinf_children, schm_size, b"schm");
    sinf_children.extend_from_slice(&[0u8; 4]); // version + flags
    sinf_children.extend_from_slice(b"cbcs");
    sinf_children.extend_from_slice(&0x00010000u32.to_be_bytes());

    // schi containing tenc
    let tenc = build_cbcs_tenc();
    let schi_size = 8 + tenc.len() as u32;
    cmaf::write_box_header(&mut sinf_children, schi_size, b"schi");
    sinf_children.extend_from_slice(&tenc);

    let sinf_size = 8 + sinf_children.len() as u32;
    let mut sinf = Vec::with_capacity(sinf_size as usize);
    cmaf::write_box_header(&mut sinf, sinf_size, b"sinf");
    sinf.extend_from_slice(&sinf_children);
    sinf
}

/// Build a tenc box configured for CBCS (1:9 pattern, 8-byte IV).
fn build_cbcs_tenc() -> Vec<u8> {
    // header(8) + version(1) + flags(3) + crypt_skip(1) + isProtected(1) + ivSize(1) + KID(16) = 31
    let total: u32 = 31;
    let mut tenc = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut tenc, total, b"tenc");
    tenc.push(0); // version
    tenc.extend_from_slice(&[0u8; 3]); // flags
    tenc.push(0x19); // crypt_byte_block=1, skip_byte_block=9
    tenc.push(1); // isProtected = 1
    tenc.push(8); // defaultPerSampleIVSize = 8
    tenc.extend_from_slice(&TEST_KID);
    tenc
}

/// Build an stsd (sample description) full box containing one sample entry.
fn build_stsd(entry: &[u8]) -> Vec<u8> {
    // stsd: header(8) + version(1) + flags(3) + entry_count(4) + entries
    let inner_size = 4 + 4 + entry.len();
    let total_size = 8 + inner_size as u32;
    let mut stsd = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut stsd, total_size, b"stsd");
    stsd.extend_from_slice(&[0u8; 4]); // version + flags
    stsd.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
    stsd.extend_from_slice(entry);
    stsd
}

/// Build a minimal sample entry box (encv or enca) with a prefix and child box.
fn build_sample_entry(box_type: &[u8; 4], child: &[u8]) -> Vec<u8> {
    // Sample entry has: header(8) + reserved(6) + data_ref_index(2) + codec-specific prefix(16)
    // For simplicity, we use 24 bytes of prefix (covering reserved + data_ref + minimal fields)
    const PREFIX_SIZE: u32 = 24;
    let total = 8 + PREFIX_SIZE + child.len() as u32;
    let mut entry = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut entry, total, box_type);
    entry.extend_from_slice(&[0u8; PREFIX_SIZE as usize]); // prefix (reserved+data_ref+codec fields)
    entry.extend_from_slice(child);
    entry
}

/// Wrap child data in a container box.
pub fn wrap_box(box_type: &[u8; 4], children: &[u8]) -> Vec<u8> {
    let total = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, box_type);
    output.extend_from_slice(children);
    output
}

/// Build a synthetic CBCS-encrypted media segment (moof + mdat) with known data.
///
/// The segment contains `sample_count` samples, each `sample_size` bytes.
/// Each sample is encrypted with CBCS using the given key and per-sample IVs.
///
/// Returns `(segment_data, plaintext_samples)` where plaintext_samples are the
/// pre-encryption plaintext values for verification.
pub fn build_cbcs_media_segment(
    sample_count: usize,
    sample_size: usize,
    key: &[u8; 16],
    iv_size: u8,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    use aes::Aes128;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    // Generate plaintext samples (each sample has distinct bytes)
    let mut plaintext_samples = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let mut sample = vec![0u8; sample_size];
        for (j, byte) in sample.iter_mut().enumerate() {
            *byte = ((i * sample_size + j) & 0xFF) as u8;
        }
        plaintext_samples.push(sample);
    }

    // Generate per-sample IVs
    let sample_ivs: Vec<Vec<u8>> = (0..sample_count)
        .map(|i| {
            let mut iv = vec![0u8; iv_size as usize];
            iv[iv_size as usize - 1] = i as u8; // Simple incrementing IV
            iv
        })
        .collect();

    // Encrypt samples with CBCS (1:9 pattern — encrypt first block of each 10-block group)
    // For simplicity, use full encryption (0:0 pattern) if sample size is small
    let mut encrypted_mdat_payload = Vec::new();
    for (i, plaintext) in plaintext_samples.iter().enumerate() {
        let mut encrypted = plaintext.clone();
        let blocks = encrypted.len() / 16;

        // Pad IV to 16 bytes for CBC
        let mut iv_16 = [0u8; 16];
        let iv = &sample_ivs[i];
        let start = 16 - iv.len().min(16);
        iv_16[start..].copy_from_slice(&iv[..iv.len().min(16)]);

        if blocks > 0 {
            // Full encryption (pattern 0:0) — encrypt all complete blocks
            let encrypt_end = blocks * 16;
            let encryptor = Aes128CbcEnc::new(key.into(), &iv_16.into());
            encryptor
                .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(
                    &mut encrypted[..encrypt_end],
                    encrypt_end,
                )
                .unwrap();
        }

        encrypted_mdat_payload.extend_from_slice(&encrypted);
    }

    // Build senc entries
    let senc_entries: Vec<cmaf::SencEntry> = sample_ivs
        .iter()
        .map(|iv| cmaf::SencEntry {
            iv: iv.clone(),
            subsamples: None, // No subsamples for simplicity
        })
        .collect();

    // Build trun entries with sample sizes
    let trun = build_trun_box(
        &plaintext_samples
            .iter()
            .map(|s| s.len() as u32)
            .collect::<Vec<_>>(),
    );

    // Build senc box
    let senc = cmaf::build_senc_box(&senc_entries, false);

    // Build mfhd (movie fragment header)
    let mut mfhd = Vec::new();
    cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
    mfhd.extend_from_slice(&1u32.to_be_bytes()); // sequence_number

    // Build traf (track fragment)
    let mut traf_children = Vec::new();

    // tfhd (track fragment header) — minimal
    let mut tfhd = Vec::new();
    cmaf::write_full_box_header(&mut tfhd, 16, b"tfhd", 0, 0x020000); // default-base-is-moof
    tfhd.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    traf_children.extend_from_slice(&tfhd);

    traf_children.extend_from_slice(&trun);
    traf_children.extend_from_slice(&senc);

    let traf = wrap_box(b"traf", &traf_children);

    // Build moof
    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof = wrap_box(b"moof", &moof_children);

    // Build mdat
    let mdat = wrap_box(b"mdat", &encrypted_mdat_payload);

    // Combine moof + mdat
    let mut segment = Vec::with_capacity(moof.len() + mdat.len());
    segment.extend_from_slice(&moof);
    segment.extend_from_slice(&mdat);

    (segment, plaintext_samples)
}

/// Build a trun (track run) box with sample sizes.
fn build_trun_box(sample_sizes: &[u32]) -> Vec<u8> {
    // flags = 0x0200 (sample_size_present)
    let flags = 0x000200u32;
    // header(8) + version_flags(4) + sample_count(4) + entries(4 * count)
    let total = 8 + 4 + 4 + (sample_sizes.len() * 4) as u32;
    let mut trun = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut trun, total, b"trun");
    trun.push(0); // version
    trun.extend_from_slice(&flags.to_be_bytes()[1..4]);
    trun.extend_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
    for &size in sample_sizes {
        trun.extend_from_slice(&size.to_be_bytes());
    }
    trun
}

// ─── Clear Content Fixture Builders ─────────────────────────────────

/// Build a minimal clear init segment: ftyp + moov { trak { mdia { minf { stbl { stsd { avc1 } } } } } }
///
/// This is unencrypted — no sinf, no PSSH boxes.
pub fn build_clear_init_segment() -> Vec<u8> {
    let mut data = Vec::new();

    // ftyp box
    let ftyp_payload = b"isom\x00\x00\x02\x00isomiso6cmfc";
    let ftyp_size = 8 + ftyp_payload.len() as u32;
    cmaf::write_box_header(&mut data, ftyp_size, b"ftyp");
    data.extend_from_slice(ftyp_payload);

    // Build moov children
    let mut moov_children = Vec::new();

    // mvhd (minimal)
    let mut mvhd = Vec::new();
    cmaf::write_full_box_header(&mut mvhd, 120, b"mvhd", 1, 0);
    mvhd.resize(120, 0);
    moov_children.extend_from_slice(&mvhd);

    // Clear sample entry: avc1 with 24-byte prefix (no sinf)
    let entry_prefix = [0u8; 24];
    let entry_size = 8 + entry_prefix.len() as u32;
    let mut entry = Vec::new();
    cmaf::write_box_header(&mut entry, entry_size, b"avc1");
    entry.extend_from_slice(&entry_prefix);

    let stsd = build_stsd(&entry);
    let stbl = wrap_box(b"stbl", &stsd);
    let minf = wrap_box(b"minf", &stbl);
    let mdia = wrap_box(b"mdia", &minf);
    let trak = wrap_box(b"trak", &mdia);
    moov_children.extend_from_slice(&trak);

    // No PSSH boxes for clear content

    let moov_size = 8 + moov_children.len() as u32;
    cmaf::write_box_header(&mut data, moov_size, b"moov");
    data.extend_from_slice(&moov_children);

    data
}

/// Build a minimal clear media segment (moof + mdat) with unencrypted data.
///
/// The moof contains trun but no senc (since content is clear).
pub fn build_clear_media_segment(
    sample_count: usize,
    sample_size: usize,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    // Generate plaintext samples
    let mut plaintext_samples = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let mut sample = vec![0u8; sample_size];
        for (j, byte) in sample.iter_mut().enumerate() {
            *byte = ((i * sample_size + j) & 0xFF) as u8;
        }
        plaintext_samples.push(sample);
    }

    // Build trun entries with sample sizes
    let trun = build_trun_box(
        &plaintext_samples
            .iter()
            .map(|s| s.len() as u32)
            .collect::<Vec<_>>(),
    );

    // Build mfhd
    let mut mfhd = Vec::new();
    cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
    mfhd.extend_from_slice(&1u32.to_be_bytes());

    // Build traf (no senc for clear content)
    let mut traf_children = Vec::new();
    let mut tfhd = Vec::new();
    cmaf::write_full_box_header(&mut tfhd, 16, b"tfhd", 0, 0x020000);
    tfhd.extend_from_slice(&1u32.to_be_bytes());
    traf_children.extend_from_slice(&tfhd);
    traf_children.extend_from_slice(&trun);
    let traf = wrap_box(b"traf", &traf_children);

    // Build moof
    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof = wrap_box(b"moof", &moof_children);

    // Build mdat (plaintext)
    let mut mdat_payload = Vec::new();
    for sample in &plaintext_samples {
        mdat_payload.extend_from_slice(sample);
    }
    let mdat = wrap_box(b"mdat", &mdat_payload);

    let mut segment = Vec::with_capacity(moof.len() + mdat.len());
    segment.extend_from_slice(&moof);
    segment.extend_from_slice(&mdat);

    (segment, plaintext_samples)
}

// ─── Manifest Fixtures ──────────────────────────────────────────────

/// Build a ManifestState configured for HLS with segments and DRM info.
pub fn make_hls_manifest_state(segment_count: u32, phase: ManifestPhase) -> ManifestState {
    let mut state = ManifestState::new(
        "integration-test".into(),
        OutputFormat::Hls,
        "/repackage/integration-test/hls/".into(),
        ContainerFormat::default(),
    );
    state.phase = phase;
    state.init_segment = Some(InitSegmentInfo {
        uri: "/repackage/integration-test/hls/init.mp4".into(),
        byte_size: 1024,
    });
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: edgepack::drm::scheme::EncryptionScheme::Cenc,
        widevine_pssh: Some("AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgIARIQ".into()),
        playready_pssh: Some("AAAARHBzc2gBAAAAmgTweZhAQoarkuZb4IhflQAAAAE=".into()),
        playready_pro: Some("<WRMHEADER></WRMHEADER>".into()),
        fairplay_key_uri: None,
        default_kid: "00112233445566778899aabbccddeeff".into(),
        clearkey_pssh: None,
    });

    for i in 0..segment_count {
        state.segments.push(SegmentInfo {
            number: i,
            duration: 6.006,
            uri: format!("/repackage/integration-test/hls/segment_{i}.cmfv"),
            byte_size: 50_000 + (i as u64 * 1000),
            key_period: None,
        });
    }

    if segment_count > 0 {
        state.target_duration = 6.006;
    }

    state
}

/// Build a ManifestState configured for DASH with segments and DRM info.
pub fn make_dash_manifest_state(segment_count: u32, phase: ManifestPhase) -> ManifestState {
    let mut state = ManifestState::new(
        "integration-test".into(),
        OutputFormat::Dash,
        "/repackage/integration-test/dash/".into(),
        ContainerFormat::default(),
    );
    state.phase = phase;
    state.init_segment = Some(InitSegmentInfo {
        uri: "/repackage/integration-test/dash/init.mp4".into(),
        byte_size: 1024,
    });
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: edgepack::drm::scheme::EncryptionScheme::Cenc,
        widevine_pssh: Some("AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgIARIQ".into()),
        playready_pssh: Some("AAAARHBzc2gBAAAAmgTweZhAQoarkuZb4IhflQAAAAE=".into()),
        playready_pro: Some("<WRMHEADER></WRMHEADER>".into()),
        fairplay_key_uri: None,
        default_kid: "00112233445566778899aabbccddeeff".into(),
        clearkey_pssh: None,
    });

    for i in 0..segment_count {
        state.segments.push(SegmentInfo {
            number: i,
            duration: 6.0,
            uri: format!("/repackage/integration-test/dash/segment_{i}.cmfv"),
            byte_size: 50_000 + (i as u64 * 1000),
            key_period: None,
        });
    }

    if segment_count > 0 {
        state.target_duration = 6.0;
    }

    state
}

/// Build an HLS ManifestState with I-frame segment data for trick play testing.
pub fn make_hls_iframe_manifest_state(segment_count: u32, phase: ManifestPhase) -> ManifestState {
    let mut state = make_hls_manifest_state(segment_count, phase);
    state.enable_iframe_playlist = true;
    for i in 0..segment_count {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 8192 + (i as u64 * 100),
            duration: 6.006,
            segment_uri: format!("/repackage/integration-test/hls/segment_{i}.cmfv"),
        });
    }
    state
}

/// Build a DASH ManifestState with I-frame / trick play data.
pub fn make_dash_iframe_manifest_state(segment_count: u32, phase: ManifestPhase) -> ManifestState {
    let mut state = make_dash_manifest_state(segment_count, phase);
    state.enable_iframe_playlist = true;
    for i in 0..segment_count {
        state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 8192 + (i as u64 * 100),
            duration: 6.0,
            segment_uri: format!("/repackage/integration-test/dash/segment_{i}.cmfv"),
        });
    }
    state
}

/// Build an HLS ManifestState with DVR window for testing.
pub fn make_hls_dvr_manifest_state(
    segment_count: u32,
    phase: ManifestPhase,
    dvr_window_duration: f64,
) -> ManifestState {
    let mut state = make_hls_manifest_state(segment_count, phase);
    state.dvr_window_duration = Some(dvr_window_duration);
    // Override segment durations to exact 6.0s for precise windowing math in tests
    for seg in &mut state.segments {
        seg.duration = 6.0;
    }
    state
}

/// Build a DASH ManifestState with DVR window for testing.
pub fn make_dash_dvr_manifest_state(
    segment_count: u32,
    phase: ManifestPhase,
    dvr_window_duration: f64,
) -> ManifestState {
    let mut state = make_dash_manifest_state(segment_count, phase);
    state.dvr_window_duration = Some(dvr_window_duration);
    // Override segment durations to exact 6.0s for precise windowing math in tests
    for seg in &mut state.segments {
        seg.duration = 6.0;
    }
    state
}

/// Build an HLS ManifestState with content steering for testing.
pub fn make_hls_content_steering_manifest_state(
    segment_count: u32,
    phase: ManifestPhase,
) -> ManifestState {
    let mut state = make_hls_manifest_state(segment_count, phase);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: Some("cdn-a".into()),
        query_before_start: None,
    });
    state
}

/// Build a DASH ManifestState with content steering for testing.
pub fn make_dash_content_steering_manifest_state(
    segment_count: u32,
    phase: ManifestPhase,
) -> ManifestState {
    let mut state = make_dash_manifest_state(segment_count, phase);
    state.content_steering = Some(ContentSteeringConfig {
        server_uri: "https://steer.example.com/v1".into(),
        default_pathway_id: Some("cdn-a".into()),
        query_before_start: Some(true),
    });
    state
}

// ─── CENC Fixture Builders ──────────────────────────────────────────

/// Build a minimal CENC init segment with sinf box (ftyp + moov containing stsd→sinf).
///
/// Mirrors `build_cbcs_init_segment()` but with CENC scheme:
/// - schm box uses `b"cenc"` instead of `b"cbcs"`
/// - tenc box: crypt_skip byte is `0x00` (no pattern), ivSize is `8`, isProtected=1
///
/// Structure matches the CBCS version with encryption scheme differences.
pub fn build_cenc_init_segment() -> Vec<u8> {
    let mut data = Vec::new();

    // ftyp box
    let ftyp_payload = b"isom\x00\x00\x02\x00isomiso6cmfc";
    let ftyp_size = 8 + ftyp_payload.len() as u32;
    cmaf::write_box_header(&mut data, ftyp_size, b"ftyp");
    data.extend_from_slice(ftyp_payload);

    // Build moov children
    let mut moov_children = Vec::new();

    // mvhd (minimal)
    let mut mvhd = Vec::new();
    cmaf::write_full_box_header(&mut mvhd, 120, b"mvhd", 1, 0);
    mvhd.resize(120, 0);
    moov_children.extend_from_slice(&mvhd);

    // trak → mdia → minf → stbl → stsd → encv → sinf
    let sinf = build_cenc_sinf();
    let encv = build_sample_entry(b"encv", &sinf);
    let stsd = build_stsd(&encv);
    let stbl = wrap_box(b"stbl", &stsd);
    let minf = wrap_box(b"minf", &stbl);
    let mdia = wrap_box(b"mdia", &minf);
    let trak = wrap_box(b"trak", &mdia);
    moov_children.extend_from_slice(&trak);

    // Add a PSSH box (Widevine) — will be replaced during rewriting
    let pssh = cmaf::build_pssh_box(&cmaf::PsshBox {
        version: 0,
        system_id: system_ids::WIDEVINE,
        key_ids: vec![],
        data: vec![0x08, 0x01],
    });
    moov_children.extend_from_slice(&pssh);

    // Wrap in moov
    let moov_size = 8 + moov_children.len() as u32;
    cmaf::write_box_header(&mut data, moov_size, b"moov");
    data.extend_from_slice(&moov_children);

    data
}

/// Build a sinf box for CENC scheme (frma + schm + schi/tenc).
fn build_cenc_sinf() -> Vec<u8> {
    let mut sinf_children = Vec::new();

    // frma: original_format = "avc1"
    let frma_size: u32 = 12;
    cmaf::write_box_header(&mut sinf_children, frma_size, b"frma");
    sinf_children.extend_from_slice(b"avc1");

    // schm: version(1) + flags(3) + scheme_type("cenc") + scheme_version(0x00010000)
    let schm_size: u32 = 8 + 4 + 4 + 4;
    cmaf::write_box_header(&mut sinf_children, schm_size, b"schm");
    sinf_children.extend_from_slice(&[0u8; 4]); // version + flags
    sinf_children.extend_from_slice(b"cenc");
    sinf_children.extend_from_slice(&0x00010000u32.to_be_bytes());

    // schi containing tenc
    let tenc = build_cenc_tenc();
    let schi_size = 8 + tenc.len() as u32;
    cmaf::write_box_header(&mut sinf_children, schi_size, b"schi");
    sinf_children.extend_from_slice(&tenc);

    let sinf_size = 8 + sinf_children.len() as u32;
    let mut sinf = Vec::with_capacity(sinf_size as usize);
    cmaf::write_box_header(&mut sinf, sinf_size, b"sinf");
    sinf.extend_from_slice(&sinf_children);
    sinf
}

/// Build a tenc box configured for CENC (no pattern, 8-byte IV).
fn build_cenc_tenc() -> Vec<u8> {
    // header(8) + version(1) + flags(3) + crypt_skip(1) + isProtected(1) + ivSize(1) + KID(16) = 31
    let total: u32 = 31;
    let mut tenc = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut tenc, total, b"tenc");
    tenc.push(0); // version
    tenc.extend_from_slice(&[0u8; 3]); // flags
    tenc.push(0x00); // crypt_byte_block=0, skip_byte_block=0 (no pattern for CENC)
    tenc.push(1); // isProtected = 1
    tenc.push(8); // defaultPerSampleIVSize = 8
    tenc.extend_from_slice(&TEST_KID);
    tenc
}

/// Build a synthetic CENC-encrypted media segment (moof + mdat) with known data.
///
/// The segment contains `sample_count` samples, each `sample_size` bytes.
/// Each sample is encrypted with CENC (AES-128-CTR) using the given key and per-sample IVs.
///
/// Returns `(segment_data, plaintext_samples)` where plaintext_samples are the
/// pre-encryption plaintext values for verification.
pub fn build_cenc_media_segment(
    sample_count: usize,
    sample_size: usize,
    key: &[u8; 16],
    iv_size: u8,
) -> (Vec<u8>, Vec<Vec<u8>>) {
    use aes::Aes128;
    use cipher::{KeyIvInit, StreamCipher};

    type Aes128Ctr = ctr::Ctr128BE<Aes128>;

    // Generate plaintext samples (each sample has distinct bytes)
    let mut plaintext_samples = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let mut sample = vec![0u8; sample_size];
        for (j, byte) in sample.iter_mut().enumerate() {
            *byte = ((i * sample_size + j) & 0xFF) as u8;
        }
        plaintext_samples.push(sample);
    }

    // Generate per-sample IVs
    let sample_ivs: Vec<Vec<u8>> = (0..sample_count)
        .map(|i| {
            let mut iv = vec![0u8; iv_size as usize];
            if iv_size > 0 {
                iv[iv_size as usize - 1] = i as u8; // Simple incrementing IV
            }
            iv
        })
        .collect();

    // Encrypt samples with CENC (AES-128-CTR, full encryption)
    let mut encrypted_mdat_payload = Vec::new();
    for (i, plaintext) in plaintext_samples.iter().enumerate() {
        let mut encrypted = plaintext.clone();

        // Pad IV to 16 bytes for CTR counter block
        let mut counter = [0u8; 16];
        let iv = &sample_ivs[i];
        let start = 16 - iv.len().min(16);
        counter[start..start + iv.len().min(16)].copy_from_slice(&iv[..iv.len().min(16)]);

        // Apply CTR keystream
        let mut cipher = Aes128Ctr::new(key.into(), &counter.into());
        cipher.apply_keystream(&mut encrypted);

        encrypted_mdat_payload.extend_from_slice(&encrypted);
    }

    // Build senc entries
    let senc_entries: Vec<cmaf::SencEntry> = sample_ivs
        .iter()
        .map(|iv| cmaf::SencEntry {
            iv: iv.clone(),
            subsamples: None,
        })
        .collect();

    // Build trun entries with sample sizes
    let trun = build_trun_box(
        &plaintext_samples
            .iter()
            .map(|s| s.len() as u32)
            .collect::<Vec<_>>(),
    );

    // Build senc box
    let senc = cmaf::build_senc_box(&senc_entries, false);

    // Build mfhd (movie fragment header)
    let mut mfhd = Vec::new();
    cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
    mfhd.extend_from_slice(&1u32.to_be_bytes()); // sequence_number

    // Build traf (track fragment)
    let mut traf_children = Vec::new();

    // tfhd (track fragment header) — minimal
    let mut tfhd = Vec::new();
    cmaf::write_full_box_header(&mut tfhd, 16, b"tfhd", 0, 0x020000); // default-base-is-moof
    tfhd.extend_from_slice(&1u32.to_be_bytes()); // track_ID
    traf_children.extend_from_slice(&tfhd);

    traf_children.extend_from_slice(&trun);
    traf_children.extend_from_slice(&senc);

    let traf = wrap_box(b"traf", &traf_children);

    // Build moof
    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof = wrap_box(b"moof", &moof_children);

    // Build mdat
    let mdat = wrap_box(b"mdat", &encrypted_mdat_payload);

    // Combine moof + mdat
    let mut segment = Vec::with_capacity(moof.len() + mdat.len());
    segment.extend_from_slice(&moof);
    segment.extend_from_slice(&mdat);

    (segment, plaintext_samples)
}

// ─── Generic Manifest Builder ───────────────────────────────────────

/// Build a ManifestState with configurable format, container, segment count, and phase.
///
/// Creates DRM info for CENC scheme (matching `make_hls_manifest_state`).
/// Segment URIs use the container format's video segment extension.
pub fn make_manifest_state_with_container(
    format: OutputFormat,
    container: ContainerFormat,
    segment_count: u32,
    phase: ManifestPhase,
) -> ManifestState {
    let format_str = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };
    let content_id = format!("e2e-{}-{:?}", format_str, container);
    let base_url = format!("/repackage/{}/{}/", content_id, format_str);

    let mut state = ManifestState::new(
        content_id.clone(),
        format,
        base_url.clone(),
        container,
    );
    state.phase = phase;
    // TS has no init segment (PAT/PMT embedded in each segment)
    #[cfg(feature = "ts")]
    let has_init = container.is_isobmff();
    #[cfg(not(feature = "ts"))]
    let has_init = true;
    if has_init {
        state.init_segment = Some(InitSegmentInfo {
            uri: format!("{}init.mp4", base_url),
            byte_size: 1024,
        });
    }
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
        widevine_pssh: Some("AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgIARIQ".into()),
        playready_pssh: Some("AAAARHBzc2gBAAAAmgTweZhAQoarkuZb4IhflQAAAAE=".into()),
        playready_pro: Some("<WRMHEADER></WRMHEADER>".into()),
        fairplay_key_uri: None,
        default_kid: "00112233445566778899aabbccddeeff".into(),
        clearkey_pssh: None,
    });

    let ext = container.video_segment_extension();
    for i in 0..segment_count {
        state.segments.push(SegmentInfo {
            number: i,
            duration: 6.0,
            uri: format!("{}segment_{i}{}", base_url, ext),
            byte_size: 50_000 + (i as u64 * 1000),
            key_period: None,
        });
    }

    if segment_count > 0 {
        state.target_duration = 6.0;
    }

    state
}

// ─── Manifest Validation Helpers ────────────────────────────────────

/// Assert that an HLS manifest is structurally valid.
///
/// Checks:
/// - Starts with `#EXTM3U`
/// - Contains `#EXT-X-VERSION:`
/// - Contains `#EXT-X-TARGETDURATION:`
/// - `#EXTINF` count equals `expected_segments`
/// - Parseable via `parse_hls_manifest()`
pub fn assert_valid_hls(manifest: &str, expected_segments: usize) {
    assert!(
        manifest.starts_with("#EXTM3U"),
        "HLS manifest must start with #EXTM3U"
    );
    assert!(
        manifest.contains("#EXT-X-VERSION:"),
        "HLS manifest must contain #EXT-X-VERSION"
    );
    assert!(
        manifest.contains("#EXT-X-TARGETDURATION:"),
        "HLS manifest must contain #EXT-X-TARGETDURATION"
    );

    let extinf_count = manifest.lines().filter(|l| l.starts_with("#EXTINF:")).count();
    assert_eq!(
        extinf_count, expected_segments,
        "HLS EXTINF count ({extinf_count}) must equal expected segments ({expected_segments})"
    );

    // Parse with the HLS input parser to ensure it's structurally valid
    let result = edgepack::manifest::hls_input::parse_hls_manifest(
        manifest,
        "https://example.com/test.m3u8",
    );
    assert!(
        result.is_ok(),
        "HLS manifest must be parseable: {:?}",
        result.err()
    );
}

/// Assert that a DASH manifest is structurally valid.
///
/// Checks:
/// - Contains `<?xml`
/// - Contains `<MPD`
/// - Contains `</MPD>`
/// - Contains `<Period`
/// - `<S ` entry count equals `expected_segments`
/// - Parseable via `parse_dash_manifest()`
pub fn assert_valid_dash(manifest: &str, expected_segments: usize) {
    assert!(
        manifest.contains("<?xml"),
        "DASH manifest must contain <?xml declaration"
    );
    assert!(
        manifest.contains("<MPD"),
        "DASH manifest must contain <MPD element"
    );
    assert!(
        manifest.contains("</MPD>"),
        "DASH manifest must contain </MPD> closing tag"
    );
    assert!(
        manifest.contains("<Period"),
        "DASH manifest must contain <Period element"
    );

    // Count <S entries in SegmentTimeline
    let s_count = manifest.matches("<S ").count();
    assert_eq!(
        s_count, expected_segments,
        "DASH <S> entry count ({s_count}) must equal expected segments ({expected_segments})"
    );

    // Parse with the DASH input parser to ensure it's structurally valid
    let result = edgepack::manifest::dash_input::parse_dash_manifest(
        manifest,
        "https://example.com/test.mpd",
    );
    assert!(
        result.is_ok(),
        "DASH manifest must be parseable: {:?}",
        result.err()
    );
}

// ─── Segment Rewrite Helpers ────────────────────────────────────────

/// Convenience wrapper around `rewrite_segment()` for common test scenarios.
///
/// Automatically sets iv_size, pattern, and segment_number based on source/target schemes.
/// Pattern is (0,0) for all schemes (full encryption, matching test fixtures).
pub fn full_segment_rewrite(
    source_segment: &[u8],
    source_scheme: EncryptionScheme,
    target_scheme: EncryptionScheme,
    source_key: Option<&[u8; 16]>,
    target_key: Option<&[u8; 16]>,
) -> Vec<u8> {
    // IV sizes match the test fixture builders:
    // - CBCS fixtures use 8-byte IVs (build_cbcs_media_segment with iv_size=8)
    // - CENC fixtures use 8-byte IVs (build_cenc_media_segment with iv_size=8)
    let source_iv_size = match source_scheme {
        EncryptionScheme::Cbcs => 8,
        EncryptionScheme::Cenc => 8,
        EncryptionScheme::None => 0,
    };
    let target_iv_size = match target_scheme {
        EncryptionScheme::Cbcs => 8,
        EncryptionScheme::Cenc => 8,
        EncryptionScheme::None => 0,
    };

    let params = SegmentRewriteParams {
        source_key: source_key.map(|k| ContentKey {
            kid: TEST_KID,
            key: k.to_vec(),
            iv: None,
        }),
        target_key: target_key.map(|k| ContentKey {
            kid: TEST_KID,
            key: k.to_vec(),
            iv: None,
        }),
        source_scheme,
        target_scheme,
        source_iv_size,
        target_iv_size,
        source_pattern: (0, 0),
        target_pattern: (0, 0),
        constant_iv: None,
        segment_number: 0,
    };

    rewrite_segment(source_segment, &params).expect("segment rewrite failed")
}

/// Convenience wrapper around init segment rewriting that dispatches to the
/// correct function based on source/target encryption state.
///
/// - encrypted→encrypted: `rewrite_init_segment()`
/// - clear→encrypted: `create_protection_info()`
/// - encrypted→clear: `strip_protection_info()`
/// - clear→clear: `rewrite_ftyp_only()`
///
/// Uses `TrackKeyMapping::single(TEST_KID)` for key mapping.
/// Target pattern: CBCS=(1,9), CENC=(0,0), None=(0,0).
pub fn full_init_rewrite(
    source_init: &[u8],
    source_scheme: EncryptionScheme,
    target_scheme: EncryptionScheme,
    key_set: Option<&DrmKeySet>,
    container_format: ContainerFormat,
) -> Vec<u8> {
    let target_iv_size = match target_scheme {
        EncryptionScheme::Cbcs => 16,
        EncryptionScheme::Cenc => 8,
        EncryptionScheme::None => 0,
    };
    let target_pattern = match target_scheme {
        EncryptionScheme::Cbcs => (1, 9),
        EncryptionScheme::Cenc => (0, 0),
        EncryptionScheme::None => (0, 0),
    };

    let key_mapping = TrackKeyMapping::single(TEST_KID);

    match (source_scheme.is_encrypted(), target_scheme.is_encrypted()) {
        (true, true) => {
            let ks = key_set.expect("key_set required for encrypted→encrypted init rewrite");
            init::rewrite_init_segment(
                source_init,
                ks,
                &key_mapping,
                target_scheme,
                target_iv_size,
                target_pattern,
                container_format,
            )
            .expect("init rewrite (encrypted→encrypted) failed")
        }
        (false, true) => {
            let ks = key_set.expect("key_set required for clear→encrypted init rewrite");
            init::create_protection_info(
                source_init,
                ks,
                &key_mapping,
                target_scheme,
                target_iv_size,
                target_pattern,
                container_format,
            )
            .expect("init rewrite (clear→encrypted) failed")
        }
        (true, false) => {
            init::strip_protection_info(source_init, container_format)
                .expect("init rewrite (encrypted→clear) failed")
        }
        (false, false) => {
            init::rewrite_ftyp_only(source_init, container_format)
                .expect("init rewrite (clear→clear) failed")
        }
    }
}

// ─── Segment Structure Validation ───────────────────────────────────

/// Assert that a media segment has valid moof/mdat structure.
///
/// Validates:
/// - Exactly one moof and one mdat box
/// - trun sample_count equals `expected_samples`
/// - If `expect_senc`, senc entry count equals `expected_samples`
///
/// Uses recursive box search to find trun and senc inside moof→traf.
pub fn assert_valid_segment_structure(
    segment: &[u8],
    expected_samples: usize,
    expect_senc: bool,
) {
    let mut moof_count = 0;
    let mut mdat_count = 0;
    let mut moof_data: Option<&[u8]> = None;

    for box_result in cmaf::iterate_boxes(segment) {
        let header = box_result.expect("failed to parse top-level box");
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &segment[header.offset as usize..box_end.min(segment.len())];

        if header.box_type == *b"moof" {
            moof_count += 1;
            moof_data = Some(box_bytes);
        } else if header.box_type == *b"mdat" {
            mdat_count += 1;
        }
    }

    assert_eq!(moof_count, 1, "segment must have exactly 1 moof");
    assert_eq!(mdat_count, 1, "segment must have exactly 1 mdat");

    let moof_bytes = moof_data.expect("moof not found");

    // Find and parse trun recursively
    let trun = find_trun_in_moof(moof_bytes)
        .expect("moof must contain trun");
    assert_eq!(
        trun.sample_count as usize, expected_samples,
        "trun sample_count ({}) must equal expected samples ({expected_samples})",
        trun.sample_count,
    );

    if expect_senc {
        let senc = find_senc_in_moof(moof_bytes)
            .expect("moof must contain senc when expect_senc is true");
        assert_eq!(
            senc.entries.len(),
            expected_samples,
            "senc entry count ({}) must equal expected samples ({expected_samples})",
            senc.entries.len(),
        );
    }
}

/// Recursively find and parse a trun box inside moof data.
fn find_trun_in_moof(moof_data: &[u8]) -> Option<cmaf::TrackRunBox> {
    let moof_header = cmaf::iterate_boxes(moof_data).next()?.ok()?;
    let moof_payload = &moof_data[moof_header.header_size as usize..];
    find_trun_recursive(moof_payload)
}

fn find_trun_recursive(data: &[u8]) -> Option<cmaf::TrackRunBox> {
    for box_result in cmaf::iterate_boxes(data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &data[header.offset as usize..box_end.min(data.len())];
        let payload = &box_bytes[header.header_size as usize..];

        if header.box_type == *b"trun" {
            return Some(cmaf::parse_trun(payload).ok()?);
        }
        // Recurse into container boxes
        if matches!(&header.box_type, b"traf" | b"trak" | b"mdia" | b"minf" | b"stbl") {
            if let Some(trun) = find_trun_recursive(payload) {
                return Some(trun);
            }
        }
    }
    None
}

/// Recursively find and parse a senc box inside moof data.
fn find_senc_in_moof(moof_data: &[u8]) -> Option<cmaf::SampleEncryptionBox> {
    let moof_header = cmaf::iterate_boxes(moof_data).next()?.ok()?;
    let moof_payload = &moof_data[moof_header.header_size as usize..];
    find_senc_recursive(moof_payload, 8)
}

fn find_senc_recursive(data: &[u8], iv_size: u8) -> Option<cmaf::SampleEncryptionBox> {
    for box_result in cmaf::iterate_boxes(data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &data[header.offset as usize..box_end.min(data.len())];
        let payload = &box_bytes[header.header_size as usize..];

        if header.box_type == *b"senc" {
            return Some(cmaf::parse_senc(payload, iv_size).ok()?);
        }
        if matches!(&header.box_type, b"traf" | b"trak" | b"mdia" | b"minf" | b"stbl") {
            if let Some(senc) = find_senc_recursive(payload, iv_size) {
                return Some(senc);
            }
        }
    }
    None
}
