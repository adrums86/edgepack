//! JIT packaging latency benchmarks.
//!
//! Measures the core operations that determine first-byte latency in JIT mode:
//! - Segment rewriting (the encryption transform hot path)
//! - Init segment rewriting (DRM scheme transform)
//! - Manifest rendering (HLS M3U8 and DASH MPD generation)
//!
//! Run with: cargo bench --target $(rustc -vV | grep host | awk '{print $2}')
//!
//! These benchmarks run on native targets (not WASM). WASM performance is
//! proportional but not identical — use binary size as the cold-start proxy.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::drm::{system_ids, ContentKey, DrmKeySet, DrmSystemData};
use edgepack::manifest;
use edgepack::manifest::hls;
use edgepack::manifest::types::*;
use edgepack::media::cmaf;
use edgepack::media::codec::TrackKeyMapping;
use edgepack::media::container::ContainerFormat;
use edgepack::media::init::{create_protection_info, rewrite_init_segment};
use edgepack::media::segment::{rewrite_segment, SegmentRewriteParams};

// ─── Test Data Constants ────────────────────────────────────────────

const SOURCE_KEY: [u8; 16] = [
    0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F,
    0x10,
];

const TARGET_KEY: [u8; 16] = [
    0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x1B, 0x1C, 0x1D, 0x1E, 0x1F,
    0x20,
];

const TEST_KID: [u8; 16] = [
    0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xAA, 0xBB, 0xCC, 0xDD, 0xEE,
    0xFF,
];

// ─── Fixture Builders ───────────────────────────────────────────────

fn build_cbcs_media_segment(sample_count: usize, sample_size: usize) -> Vec<u8> {
    use aes::Aes128;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    let iv_size: u8 = 8;
    let mut encrypted_mdat_payload = Vec::new();
    let mut senc_entries = Vec::with_capacity(sample_count);

    for i in 0..sample_count {
        let mut sample = vec![0u8; sample_size];
        for (j, byte) in sample.iter_mut().enumerate() {
            *byte = ((i * sample_size + j) & 0xFF) as u8;
        }

        let mut iv = vec![0u8; iv_size as usize];
        iv[iv_size as usize - 1] = i as u8;

        let mut iv_16 = [0u8; 16];
        let start = 16 - iv.len();
        iv_16[start..].copy_from_slice(&iv);

        let blocks = sample.len() / 16;
        if blocks > 0 {
            let encrypt_end = blocks * 16;
            let encryptor = Aes128CbcEnc::new((&SOURCE_KEY).into(), &iv_16.into());
            encryptor
                .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(
                    &mut sample[..encrypt_end],
                    encrypt_end,
                )
                .unwrap();
        }

        encrypted_mdat_payload.extend_from_slice(&sample);
        senc_entries.push(cmaf::SencEntry {
            iv: iv.clone(),
            subsamples: None,
        });
    }

    let sample_sizes: Vec<u32> = (0..sample_count).map(|_| sample_size as u32).collect();
    let trun = build_trun_box(&sample_sizes);
    let senc = cmaf::build_senc_box(&senc_entries, false);

    let mut mfhd = Vec::new();
    cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
    mfhd.extend_from_slice(&1u32.to_be_bytes());

    let mut tfhd = Vec::new();
    cmaf::write_full_box_header(&mut tfhd, 16, b"tfhd", 0, 0x020000);
    tfhd.extend_from_slice(&1u32.to_be_bytes());

    let mut traf_children = Vec::new();
    traf_children.extend_from_slice(&tfhd);
    traf_children.extend_from_slice(&trun);
    traf_children.extend_from_slice(&senc);
    let traf = wrap_box(b"traf", &traf_children);

    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof = wrap_box(b"moof", &moof_children);
    let mdat = wrap_box(b"mdat", &encrypted_mdat_payload);

    let mut segment = Vec::with_capacity(moof.len() + mdat.len());
    segment.extend_from_slice(&moof);
    segment.extend_from_slice(&mdat);
    segment
}

fn build_clear_media_segment(sample_count: usize, sample_size: usize) -> Vec<u8> {
    let mut mdat_payload = Vec::new();
    let mut sample_sizes_vec = Vec::with_capacity(sample_count);

    for i in 0..sample_count {
        let mut sample = vec![0u8; sample_size];
        for (j, byte) in sample.iter_mut().enumerate() {
            *byte = ((i * sample_size + j) & 0xFF) as u8;
        }
        mdat_payload.extend_from_slice(&sample);
        sample_sizes_vec.push(sample_size as u32);
    }

    let trun = build_trun_box(&sample_sizes_vec);

    let mut mfhd = Vec::new();
    cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
    mfhd.extend_from_slice(&1u32.to_be_bytes());

    let mut tfhd = Vec::new();
    cmaf::write_full_box_header(&mut tfhd, 16, b"tfhd", 0, 0x020000);
    tfhd.extend_from_slice(&1u32.to_be_bytes());

    let mut traf_children = Vec::new();
    traf_children.extend_from_slice(&tfhd);
    traf_children.extend_from_slice(&trun);
    let traf = wrap_box(b"traf", &traf_children);

    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof = wrap_box(b"moof", &moof_children);
    let mdat = wrap_box(b"mdat", &mdat_payload);

    let mut segment = Vec::with_capacity(moof.len() + mdat.len());
    segment.extend_from_slice(&moof);
    segment.extend_from_slice(&mdat);
    segment
}

fn build_cbcs_init_segment() -> Vec<u8> {
    let mut data = Vec::new();
    let ftyp_payload = b"isom\x00\x00\x02\x00isomiso6cmfc";
    let ftyp_size = 8 + ftyp_payload.len() as u32;
    cmaf::write_box_header(&mut data, ftyp_size, b"ftyp");
    data.extend_from_slice(ftyp_payload);

    let mut moov_children = Vec::new();
    let mut mvhd = Vec::new();
    cmaf::write_full_box_header(&mut mvhd, 120, b"mvhd", 1, 0);
    mvhd.resize(120, 0);
    moov_children.extend_from_slice(&mvhd);

    let sinf = build_cbcs_sinf();
    let encv = build_sample_entry(b"encv", &sinf);
    let stsd = build_stsd(&encv);
    let stbl = wrap_box(b"stbl", &stsd);
    let minf = wrap_box(b"minf", &stbl);
    let mdia = wrap_box(b"mdia", &minf);
    let trak = wrap_box(b"trak", &mdia);
    moov_children.extend_from_slice(&trak);

    let pssh = cmaf::build_pssh_box(&cmaf::PsshBox {
        version: 0,
        system_id: system_ids::WIDEVINE,
        key_ids: vec![],
        data: vec![0x08, 0x01],
    });
    moov_children.extend_from_slice(&pssh);

    let moov_size = 8 + moov_children.len() as u32;
    cmaf::write_box_header(&mut data, moov_size, b"moov");
    data.extend_from_slice(&moov_children);
    data
}

fn build_clear_init_segment() -> Vec<u8> {
    let mut data = Vec::new();
    let ftyp_payload = b"isom\x00\x00\x02\x00isomiso6cmfc";
    let ftyp_size = 8 + ftyp_payload.len() as u32;
    cmaf::write_box_header(&mut data, ftyp_size, b"ftyp");
    data.extend_from_slice(ftyp_payload);

    let mut moov_children = Vec::new();
    let mut mvhd = Vec::new();
    cmaf::write_full_box_header(&mut mvhd, 120, b"mvhd", 1, 0);
    mvhd.resize(120, 0);
    moov_children.extend_from_slice(&mvhd);

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

    let moov_size = 8 + moov_children.len() as u32;
    cmaf::write_box_header(&mut data, moov_size, b"moov");
    data.extend_from_slice(&moov_children);
    data
}

fn build_cbcs_sinf() -> Vec<u8> {
    let mut sinf_children = Vec::new();

    let frma_size: u32 = 12;
    cmaf::write_box_header(&mut sinf_children, frma_size, b"frma");
    sinf_children.extend_from_slice(b"avc1");

    let schm_size: u32 = 20;
    cmaf::write_box_header(&mut sinf_children, schm_size, b"schm");
    sinf_children.extend_from_slice(&[0u8; 4]);
    sinf_children.extend_from_slice(b"cbcs");
    sinf_children.extend_from_slice(&0x00010000u32.to_be_bytes());

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

fn build_cbcs_tenc() -> Vec<u8> {
    let total: u32 = 31;
    let mut tenc = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut tenc, total, b"tenc");
    tenc.push(0);
    tenc.extend_from_slice(&[0u8; 3]);
    tenc.push(0x19);
    tenc.push(1);
    tenc.push(8);
    tenc.extend_from_slice(&TEST_KID);
    tenc
}

fn build_stsd(entry: &[u8]) -> Vec<u8> {
    let inner_size = 4 + 4 + entry.len();
    let total_size = 8 + inner_size as u32;
    let mut stsd = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut stsd, total_size, b"stsd");
    stsd.extend_from_slice(&[0u8; 4]);
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend_from_slice(entry);
    stsd
}

fn build_sample_entry(box_type: &[u8; 4], child: &[u8]) -> Vec<u8> {
    const PREFIX_SIZE: u32 = 24;
    let total = 8 + PREFIX_SIZE + child.len() as u32;
    let mut entry = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut entry, total, box_type);
    entry.extend_from_slice(&[0u8; PREFIX_SIZE as usize]);
    entry.extend_from_slice(child);
    entry
}

fn wrap_box(box_type: &[u8; 4], children: &[u8]) -> Vec<u8> {
    let total = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, box_type);
    output.extend_from_slice(children);
    output
}

fn build_trun_box(sample_sizes: &[u32]) -> Vec<u8> {
    let flags = 0x000200u32;
    let total = 8 + 4 + 4 + (sample_sizes.len() * 4) as u32;
    let mut trun = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut trun, total, b"trun");
    trun.push(0);
    trun.extend_from_slice(&flags.to_be_bytes()[1..4]);
    trun.extend_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
    for &size in sample_sizes {
        trun.extend_from_slice(&size.to_be_bytes());
    }
    trun
}

fn make_drm_key_set() -> DrmKeySet {
    DrmKeySet {
        keys: vec![ContentKey {
            kid: TEST_KID,
            key: SOURCE_KEY.to_vec(),
            iv: None,
        }],
        drm_systems: vec![
            DrmSystemData {
                system_id: system_ids::WIDEVINE,
                kid: TEST_KID,
                pssh_data: vec![0x08, 0x01, 0x12, 0x10],
                content_protection_data: None,
            },
            DrmSystemData {
                system_id: system_ids::PLAYREADY,
                kid: TEST_KID,
                pssh_data: vec![0x48, 0x00, 0x65, 0x00],
                content_protection_data: Some("<WRMHEADER/>".into()),
            },
        ],
    }
}

fn make_manifest_state(format: OutputFormat, segment_count: u32, phase: ManifestPhase) -> ManifestState {
    let fmt_str = match format {
        OutputFormat::Hls => "hls",
        OutputFormat::Dash => "dash",
    };
    let mut state = ManifestState::new(
        "bench-test".into(),
        format,
        format!("/repackage/bench-test/{fmt_str}/"),
        ContainerFormat::default(),
    );
    state.phase = phase;
    state.init_segment = Some(InitSegmentInfo {
        uri: format!("/repackage/bench-test/{fmt_str}/init.mp4"),
        byte_size: 1024,
    });
    state.drm_info = Some(ManifestDrmInfo {
        encryption_scheme: EncryptionScheme::Cenc,
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
            uri: format!("/repackage/bench-test/{fmt_str}/segment_{i}.cmfv"),
            byte_size: 50_000 + (i as u64 * 1000),
            key_period: None,
        });
    }
    state.target_duration = 6.006;
    state
}

// ─── Benchmarks ─────────────────────────────────────────────────────

fn bench_segment_rewrite(c: &mut Criterion) {
    let mut group = c.benchmark_group("segment_rewrite");

    // Benchmark different sample counts to show scaling
    for &sample_count in &[4, 32, 128] {
        let sample_size = 1024; // 1KB per sample
        let segment = build_cbcs_media_segment(sample_count, sample_size);
        let total_kb = (sample_count * sample_size) / 1024;

        // CBCS → CENC (encrypted → encrypted)
        let params = SegmentRewriteParams {
            source_key: Some(ContentKey { kid: TEST_KID, key: SOURCE_KEY.to_vec(), iv: None }),
            target_key: Some(ContentKey { kid: TEST_KID, key: TARGET_KEY.to_vec(), iv: None }),
            source_scheme: EncryptionScheme::Cbcs,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 8,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        };

        group.bench_with_input(
            BenchmarkId::new("cbcs_to_cenc", format!("{sample_count}s_{total_kb}KB")),
            &(&segment, &params),
            |b, (seg, params)| {
                b.iter(|| rewrite_segment(black_box(seg), black_box(params)).unwrap());
            },
        );

        // Clear → CENC
        let clear_segment = build_clear_media_segment(sample_count, sample_size);
        let clear_params = SegmentRewriteParams {
            source_key: None,
            target_key: Some(ContentKey { kid: TEST_KID, key: TARGET_KEY.to_vec(), iv: None }),
            source_scheme: EncryptionScheme::None,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 0,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        };

        group.bench_with_input(
            BenchmarkId::new("clear_to_cenc", format!("{sample_count}s_{total_kb}KB")),
            &(&clear_segment, &clear_params),
            |b, (seg, params)| {
                b.iter(|| rewrite_segment(black_box(seg), black_box(params)).unwrap());
            },
        );

        // Clear → Clear (pass-through baseline)
        let passthrough_params = SegmentRewriteParams {
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

        group.bench_with_input(
            BenchmarkId::new("passthrough", format!("{sample_count}s_{total_kb}KB")),
            &(&clear_segment, &passthrough_params),
            |b, (seg, params)| {
                b.iter(|| rewrite_segment(black_box(seg), black_box(params)).unwrap());
            },
        );
    }

    group.finish();
}

fn bench_init_rewrite(c: &mut Criterion) {
    let mut group = c.benchmark_group("init_rewrite");

    let encrypted_init = build_cbcs_init_segment();
    let clear_init = build_clear_init_segment();
    let key_set = make_drm_key_set();
    let mapping = TrackKeyMapping::single(TEST_KID);

    // Encrypted → Encrypted (CBCS → CENC)
    group.bench_function("cbcs_to_cenc", |b| {
        b.iter(|| {
            rewrite_init_segment(
                black_box(&encrypted_init),
                black_box(&key_set),
                black_box(&mapping),
                black_box(EncryptionScheme::Cenc),
                black_box(8),
                black_box((0, 0)),
                black_box(ContainerFormat::Cmaf),
            )
            .unwrap()
        });
    });

    // Clear → Encrypted
    group.bench_function("clear_to_cenc", |b| {
        b.iter(|| {
            create_protection_info(
                black_box(&clear_init),
                black_box(&key_set),
                black_box(&mapping),
                black_box(EncryptionScheme::Cenc),
                black_box(8),
                black_box((0, 0)),
                black_box(ContainerFormat::Cmaf),
            )
            .unwrap()
        });
    });

    group.finish();
}

fn bench_manifest_render(c: &mut Criterion) {
    let mut group = c.benchmark_group("manifest_render");

    // HLS with varying segment counts
    for &count in &[10, 50, 200] {
        let state = make_manifest_state(OutputFormat::Hls, count, ManifestPhase::Complete);
        group.bench_with_input(
            BenchmarkId::new("hls_vod", count),
            &state,
            |b, state| {
                b.iter(|| manifest::render_manifest(black_box(state)).unwrap());
            },
        );
    }

    // DASH with varying segment counts
    for &count in &[10, 50, 200] {
        let state = make_manifest_state(OutputFormat::Dash, count, ManifestPhase::Complete);
        group.bench_with_input(
            BenchmarkId::new("dash_vod", count),
            &state,
            |b, state| {
                b.iter(|| manifest::render_manifest(black_box(state)).unwrap());
            },
        );
    }

    // HLS I-frame playlist
    let mut iframe_state = make_manifest_state(OutputFormat::Hls, 50, ManifestPhase::Complete);
    iframe_state.enable_iframe_playlist = true;
    for i in 0..50 {
        iframe_state.iframe_segments.push(IFrameSegmentInfo {
            segment_number: i,
            byte_offset: 0,
            byte_length: 8192,
            duration: 6.006,
            segment_uri: format!("/repackage/bench-test/hls/segment_{i}.cmfv"),
        });
    }
    group.bench_function("hls_iframe_50seg", |b| {
        b.iter(|| hls::render_iframe_playlist(black_box(&iframe_state)).unwrap());
    });

    // Live manifest (smaller, more frequent)
    let live_state = make_manifest_state(OutputFormat::Hls, 6, ManifestPhase::Live);
    group.bench_function("hls_live_6seg", |b| {
        b.iter(|| manifest::render_manifest(black_box(&live_state)).unwrap());
    });

    group.finish();
}

fn bench_manifest_parse(c: &mut Criterion) {
    let mut group = c.benchmark_group("manifest_parse");

    // Render manifests first, then benchmark parsing
    let hls_state = make_manifest_state(OutputFormat::Hls, 50, ManifestPhase::Complete);
    let hls_text = manifest::render_manifest(&hls_state).unwrap();

    group.bench_function("hls_parse_50seg", |b| {
        b.iter(|| {
            edgepack::manifest::hls_input::parse_hls_manifest(
                black_box(&hls_text),
                black_box("https://example.com/hls/"),
            )
            .unwrap()
        });
    });

    let dash_state = make_manifest_state(OutputFormat::Dash, 50, ManifestPhase::Complete);
    let dash_text = manifest::render_manifest(&dash_state).unwrap();

    group.bench_function("dash_parse_50seg", |b| {
        b.iter(|| {
            edgepack::manifest::dash_input::parse_dash_manifest(
                black_box(&dash_text),
                black_box("https://example.com/dash/"),
            )
            .unwrap()
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_segment_rewrite,
    bench_init_rewrite,
    bench_manifest_render,
    bench_manifest_parse,
);
criterion_main!(benches);
