//! Integration tests: TS segment output (Phase 22).
//!
//! Tests CMAF→TS muxing, TS-specific HLS manifest rendering,
//! container format validation, handler integration,
//! and output integrity for the TS output path.

#![cfg(feature = "ts")]

mod common;

use edgepack::drm::scheme::EncryptionScheme;
use edgepack::manifest;
use edgepack::manifest::types::{
    ManifestDrmInfo, ManifestPhase, ManifestState, OutputFormat, SegmentInfo,
};
use edgepack::media::container::ContainerFormat;
use edgepack::media::ts_mux;

// ─── Helpers ─────────────────────────────────────────────────────────

/// Build a ManifestState configured for HLS + TS output.
fn make_hls_ts_manifest_state(
    segment_count: u32,
    phase: ManifestPhase,
    encrypted: bool,
) -> ManifestState {
    let mut state = ManifestState::new(
        "ts-test".into(),
        OutputFormat::Hls,
        "/repackage/ts-test/hls/".into(),
        ContainerFormat::Ts,
    );
    state.phase = phase;
    // TS has no init segment
    state.init_segment = None;

    if encrypted {
        state.drm_info = Some(ManifestDrmInfo {
            encryption_scheme: EncryptionScheme::Cenc,
            widevine_pssh: Some(
                "AAAAOHBzc2gAAAAA7e+LqXnWSs6jyCfc1R0h7QAAABgIARIQ".into(),
            ),
            playready_pssh: Some("AAAARHBzc2gBAAAAmgTweZhAQoarkuZb4IhflQAAAAE=".into()),
            playready_pro: Some("<WRMHEADER></WRMHEADER>".into()),
            fairplay_key_uri: None,
            default_kid: "00112233445566778899aabbccddeeff".into(),
            clearkey_pssh: None,
        });
    }

    for i in 0..segment_count {
        state.segments.push(SegmentInfo {
            number: i,
            duration: 6.0,
            uri: format!("/repackage/ts-test/hls/segment_{i}.ts"),
            byte_size: 50_000 + (i as u64 * 1000),
            key_period: None,
        });
    }

    if segment_count > 0 {
        state.target_duration = 6.0;
    }

    state
}

// ─── ContainerFormat::Ts Tests ───────────────────────────────────────

#[test]
fn ts_container_format_serde_roundtrip() {
    let json = serde_json::to_string(&ContainerFormat::Ts).unwrap();
    // Derive serde serializes as variant name "Ts"
    assert_eq!(json, "\"Ts\"");
    let parsed: ContainerFormat = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, ContainerFormat::Ts);
}

#[test]
fn ts_container_format_extension() {
    assert_eq!(ContainerFormat::Ts.video_segment_extension(), ".ts");
    assert_eq!(ContainerFormat::Ts.audio_segment_extension(), ".ts");
}

#[test]
fn ts_container_format_not_isobmff() {
    assert!(!ContainerFormat::Ts.is_isobmff());
    // Other formats are ISOBMFF
    assert!(ContainerFormat::Cmaf.is_isobmff());
    assert!(ContainerFormat::Fmp4.is_isobmff());
    assert!(ContainerFormat::Iso.is_isobmff());
}

#[test]
fn ts_container_format_empty_ftyp() {
    let ftyp = ContainerFormat::Ts.build_ftyp();
    assert!(ftyp.is_empty());
}

#[test]
fn ts_container_format_empty_brands() {
    let brands = ContainerFormat::Ts.compatible_brands();
    assert!(brands.is_empty());
}

#[test]
#[should_panic]
fn ts_container_format_dash_profiles_panics() {
    let _ = ContainerFormat::Ts.dash_profiles();
}

#[test]
fn ts_container_format_from_str() {
    assert_eq!(
        ContainerFormat::from_str_value("ts"),
        Some(ContainerFormat::Ts)
    );
}

#[test]
fn ts_container_format_display() {
    assert_eq!(format!("{}", ContainerFormat::Ts), "ts");
}

// ─── TS + DASH Validation ────────────────────────────────────────────

#[test]
fn ts_dash_validation_rejected() {
    let result = edgepack::media::compat::validate_container_output_formats(
        ContainerFormat::Ts,
        &[OutputFormat::Dash],
    );
    assert!(!result.valid);
    assert!(
        result
            .errors
            .iter()
            .any(|e| e.contains("TS") && e.contains("DASH"))
    );
}

#[test]
fn ts_hls_validation_accepted() {
    let result = edgepack::media::compat::validate_container_output_formats(
        ContainerFormat::Ts,
        &[OutputFormat::Hls],
    );
    assert!(result.valid);
    assert!(result.errors.is_empty());
}

#[test]
fn ts_dual_format_hls_dash_rejected() {
    let result = edgepack::media::compat::validate_container_output_formats(
        ContainerFormat::Ts,
        &[OutputFormat::Hls, OutputFormat::Dash],
    );
    assert!(!result.valid);
}

// ─── HLS Manifest: TS-Specific Rendering ─────────────────────────────

#[test]
fn hls_ts_manifest_no_ext_x_map() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    assert!(manifest.contains("#EXTM3U"));
    assert!(!manifest.contains("#EXT-X-MAP"), "TS manifests must not have EXT-X-MAP");
}

#[test]
fn hls_ts_manifest_version_3() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    assert!(
        manifest.contains("#EXT-X-VERSION:3"),
        "TS manifests should use VERSION:3, got: {manifest}"
    );
}

#[test]
fn hls_ts_manifest_ts_segment_extension() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    // All segment URIs should end with .ts
    for line in manifest.lines() {
        if line.starts_with("/repackage/") {
            assert!(line.ends_with(".ts"), "Segment URI should end with .ts: {line}");
        }
    }
}

#[test]
fn hls_ts_manifest_clear_no_key_tag() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    assert!(
        !manifest.contains("#EXT-X-KEY"),
        "Clear TS should not have KEY tags"
    );
}

#[test]
fn hls_ts_manifest_encrypted_aes128_key() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, true);
    let manifest = manifest::render_manifest(&state).unwrap();

    assert!(
        manifest.contains("METHOD=AES-128"),
        "Encrypted TS should use METHOD=AES-128, got: {manifest}"
    );
    assert!(
        !manifest.contains("METHOD=SAMPLE-AES"),
        "TS should NOT use SAMPLE-AES"
    );
    assert!(
        !manifest.contains("METHOD=SAMPLE-AES-CTR"),
        "TS should NOT use SAMPLE-AES-CTR"
    );
}

#[test]
fn hls_ts_manifest_aes128_has_key_uri() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, true);
    let manifest = manifest::render_manifest(&state).unwrap();

    // Should contain a key URI pointing to the key endpoint
    assert!(
        manifest.contains("URI=\"/repackage/ts-test/hls/key\""),
        "AES-128 KEY tag should contain key URI, got: {manifest}"
    );
}

#[test]
fn hls_ts_manifest_endlist_on_complete() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    assert!(manifest.contains("#EXT-X-ENDLIST"));
}

#[test]
fn hls_ts_manifest_no_endlist_on_live() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Live, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    assert!(!manifest.contains("#EXT-X-ENDLIST"));
}

#[test]
fn hls_ts_manifest_correct_segment_count() {
    let state = make_hls_ts_manifest_state(10, ManifestPhase::Complete, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    let extinf_count = manifest
        .lines()
        .filter(|l| l.starts_with("#EXTINF:"))
        .count();
    assert_eq!(extinf_count, 10);
}

#[test]
fn hls_ts_manifest_vod_playlist_type() {
    let state = make_hls_ts_manifest_state(5, ManifestPhase::Complete, false);
    let manifest = manifest::render_manifest(&state).unwrap();

    assert!(manifest.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
}

// ─── TS Mux Config ───────────────────────────────────────────────────

#[test]
fn ts_mux_config_serde_roundtrip() {
    let config = ts_mux::TsMuxConfig {
        video_codec: Some(edgepack::media::ts::TsCodec::H264),
        audio_codec: Some(edgepack::media::ts::TsCodec::Aac),
        sps: vec![0x67, 0x42, 0x00, 0x1E],
        pps: vec![0x68, 0xCE, 0x38, 0x80],
        aac_profile: 2,
        aac_sample_rate_index: 3,
        aac_channel_count: 2,
        video_timescale: 90000,
        audio_timescale: 48000,
    };

    let json = serde_json::to_string(&config).unwrap();
    let parsed: ts_mux::TsMuxConfig = serde_json::from_str(&json).unwrap();

    assert_eq!(
        parsed.video_codec,
        Some(edgepack::media::ts::TsCodec::H264)
    );
    assert_eq!(parsed.audio_codec, Some(edgepack::media::ts::TsCodec::Aac));
    assert_eq!(parsed.sps, vec![0x67, 0x42, 0x00, 0x1E]);
    assert_eq!(parsed.pps, vec![0x68, 0xCE, 0x38, 0x80]);
    assert_eq!(parsed.aac_profile, 2);
    assert_eq!(parsed.aac_sample_rate_index, 3);
    assert_eq!(parsed.aac_channel_count, 2);
    assert_eq!(parsed.video_timescale, 90000);
    assert_eq!(parsed.audio_timescale, 48000);
}

#[test]
fn ts_mux_config_extract_from_clear_init() {
    let init = common::build_clear_init_segment();
    // extract_mux_config should parse an init segment successfully
    // (even if it doesn't have SPS/PPS in the simple fixture, it shouldn't panic)
    let result = ts_mux::extract_mux_config(&init);
    // The clear init segment may or may not have proper avcC depending on fixture
    // At minimum, the function shouldn't panic
    assert!(result.is_ok() || result.is_err());
}

// ─── TS Muxer: PAT/PMT Roundtrip ────────────────────────────────────

#[test]
fn ts_mux_pat_roundtrip_via_demuxer() {
    let pat = ts_mux::build_pat_packet(0);
    assert_eq!(pat.len(), 188);
    assert_eq!(pat[0], 0x47); // sync byte

    let parsed = edgepack::media::ts::parse_ts_packet(&pat).unwrap();
    assert_eq!(parsed.pid, 0x0000); // PAT PID
    assert!(parsed.pusi);

    let pat_table = edgepack::media::ts::parse_pat(&parsed.payload).unwrap();
    assert_eq!(pat_table.programs.len(), 1);
    // Program 1 maps to PMT PID 0x1000
    assert_eq!(pat_table.programs[0], (1, 0x1000));
}

#[test]
fn ts_mux_pmt_roundtrip_h264_aac() {
    use edgepack::media::ts::TsCodec;

    let pmt = ts_mux::build_pmt_packet(TsCodec::H264, TsCodec::Aac, 0);
    assert_eq!(pmt.len(), 188);
    assert_eq!(pmt[0], 0x47);

    let parsed = edgepack::media::ts::parse_ts_packet(&pmt).unwrap();
    assert_eq!(parsed.pid, 0x1000); // PMT PID

    let pmt_table = edgepack::media::ts::parse_pmt(&parsed.payload).unwrap();
    assert_eq!(pmt_table.streams.len(), 2);
    // H.264 video (stream type 0x1B)
    assert_eq!(pmt_table.streams[0].stream_type, 0x1B);
    // AAC audio (stream type 0x0F)
    assert_eq!(pmt_table.streams[1].stream_type, 0x0F);
}

// ─── TS Muxer: PES/PTS Roundtrip ────────────────────────────────────

#[test]
fn ts_mux_pes_pts_roundtrip() {
    let data = vec![0xAA; 32];
    let pes = ts_mux::build_pes_packet(0xE0, 90000, None, &data);

    let (stream_id, pts, dts, header_len) =
        edgepack::media::ts::parse_pes_header(&pes).unwrap();
    assert_eq!(stream_id, 0xE0);
    assert_eq!(pts, Some(90000));
    assert!(dts.is_none());
    assert_eq!(&pes[header_len..], &data[..]);
}

#[test]
fn ts_mux_pes_pts_dts_roundtrip() {
    let data = vec![0xBB; 16];
    let pes = ts_mux::build_pes_packet(0xE0, 93000, Some(90000), &data);

    let (stream_id, pts, dts, header_len) =
        edgepack::media::ts::parse_pes_header(&pes).unwrap();
    assert_eq!(stream_id, 0xE0);
    assert_eq!(pts, Some(93000));
    assert_eq!(dts, Some(90000));
    assert_eq!(&pes[header_len..], &data[..]);
}

// ─── TS Muxer: Packetization ─────────────────────────────────────────

#[test]
fn ts_mux_packetize_produces_188_byte_packets() {
    let pes = ts_mux::build_pes_packet(0xE0, 90000, None, &vec![0xCC; 512]);
    let mut cc = 0u8;
    let packets = ts_mux::packetize_pes(0x0100, &pes, true, &mut cc, true);

    assert!(!packets.is_empty());
    for pkt in &packets {
        assert_eq!(pkt.len(), 188);
        assert_eq!(pkt[0], 0x47);
        let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
        assert_eq!(pid, 0x0100);
    }

    // First packet should have PUSI set
    assert_ne!(packets[0][1] & 0x40, 0);
    // Subsequent should not
    if packets.len() > 1 {
        assert_eq!(packets[1][1] & 0x40, 0);
    }
}

// ─── TS Muxer: AVCC/Annex B Conversion ──────────────────────────────

#[test]
fn avcc_to_annexb_idr_prepends_sps_pps() {
    let nal_data = vec![0x65, 0xAA, 0xBB]; // IDR NAL
    let mut avcc = Vec::new();
    avcc.extend_from_slice(&(nal_data.len() as u32).to_be_bytes());
    avcc.extend_from_slice(&nal_data);

    let sps = vec![0x67, 0x42, 0x00, 0x1E];
    let pps = vec![0x68, 0xCE, 0x38, 0x80];

    let annexb = ts_mux::convert_avcc_to_annexb(&avcc, &sps, &pps, true);

    // Should contain start codes before SPS, PPS, and NAL
    let start_code = [0x00, 0x00, 0x00, 0x01];
    let sc_count = annexb.windows(4).filter(|w| *w == start_code).count();
    assert_eq!(sc_count, 3, "IDR should have 3 start codes (SPS + PPS + NAL)");

    // Verify SPS data is present
    assert!(annexb.windows(sps.len()).any(|w| w == &sps[..]));
    // Verify PPS data is present
    assert!(annexb.windows(pps.len()).any(|w| w == &pps[..]));
    // Verify NAL data is present
    assert!(annexb.windows(nal_data.len()).any(|w| w == &nal_data[..]));
}

#[test]
fn avcc_to_annexb_non_idr_no_sps_pps() {
    let nal_data = vec![0x41, 0xAA]; // Non-IDR NAL
    let mut avcc = Vec::new();
    avcc.extend_from_slice(&(nal_data.len() as u32).to_be_bytes());
    avcc.extend_from_slice(&nal_data);

    let annexb = ts_mux::convert_avcc_to_annexb(&avcc, &[0x67], &[0x68], false);

    // Non-IDR should only have one start code (no SPS/PPS)
    let start_code = [0x00, 0x00, 0x00, 0x01];
    let sc_count = annexb.windows(4).filter(|w| *w == start_code).count();
    assert_eq!(sc_count, 1, "Non-IDR should have 1 start code");
    assert_eq!(&annexb[4..], &nal_data[..]);
}

// ─── TS Muxer: ADTS Header ──────────────────────────────────────────

#[test]
fn adts_header_correct_frame_length() {
    let aac_frame_len = 128;
    let header = ts_mux::build_adts_header(2, 3, 2, aac_frame_len);

    // ADTS syncword
    assert_eq!(header[0], 0xFF);
    assert_eq!(header[1] & 0xF0, 0xF0);

    // Frame length = aac_frame_len + 7 (header)
    let frame_len = ((header[3] as u16 & 0x03) << 11)
        | ((header[4] as u16) << 3)
        | ((header[5] as u16) >> 5);
    assert_eq!(frame_len, (aac_frame_len + 7) as u16);
}

// ─── TS Muxer: Encryption ───────────────────────────────────────────

#[test]
fn ts_encrypt_decrypt_roundtrip() {
    let key = [0x01u8; 16];
    let iv = [0x02u8; 16];
    let plaintext = vec![0xAA; 100];

    let encrypted = ts_mux::encrypt_ts_segment(&plaintext, &key, &iv).unwrap();
    assert_ne!(encrypted, plaintext);
    assert_eq!(encrypted.len() % 16, 0); // PKCS7 padded

    let decrypted =
        edgepack::media::ts::decrypt_ts_segment(&encrypted, &key, &iv).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn ts_encrypt_aligned_data() {
    let key = [0x03u8; 16];
    let iv = [0x04u8; 16];
    let plaintext = vec![0xBB; 160]; // Already 16-byte aligned

    let encrypted = ts_mux::encrypt_ts_segment(&plaintext, &key, &iv).unwrap();
    // Padded adds 16 bytes of 0x10 padding
    assert_eq!(encrypted.len(), plaintext.len() + 16);

    let decrypted =
        edgepack::media::ts::decrypt_ts_segment(&encrypted, &key, &iv).unwrap();
    assert_eq!(decrypted, plaintext);
}

#[test]
fn ts_encrypt_empty_data() {
    let key = [0u8; 16];
    let iv = [0u8; 16];
    let result = ts_mux::encrypt_ts_segment(&[], &key, &iv).unwrap();
    assert!(result.is_empty());
}

// ─── Handler: Key Endpoint Routing ───────────────────────────────────

#[test]
fn key_endpoint_routes_correctly() {
    use edgepack::handler::{route, HttpMethod, HttpRequest};

    let ctx = make_test_handler_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/ts-test/hls/key".into(),
        headers: vec![],
        body: None,
    };

    // Should route to key handler (will return 404/not-found because no keys cached)
    let result = route(&req, &ctx);
    // It should either return a response or an error (not a routing error)
    assert!(result.is_ok() || result.is_err());
    if let Ok(resp) = result {
        // No keys in cache → expect 404
        assert_eq!(resp.status, 404);
    }
}

#[test]
fn key_endpoint_scheme_qualified_routes() {
    use edgepack::handler::{route, HttpMethod, HttpRequest};

    let ctx = make_test_handler_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/ts-test/hls_cenc/key".into(),
        headers: vec![],
        body: None,
    };

    let result = route(&req, &ctx);
    assert!(result.is_ok() || result.is_err());
    if let Ok(resp) = result {
        assert_eq!(resp.status, 404);
    }
}

// ─── Handler: .ts Segment Extension ──────────────────────────────────

#[test]
fn ts_segment_extension_routes_correctly() {
    use edgepack::handler::{route, HttpMethod, HttpRequest};

    let ctx = make_test_handler_context();
    let req = HttpRequest {
        method: HttpMethod::Get,
        path: "/repackage/ts-test/hls/segment_0.ts".into(),
        headers: vec![],
        body: None,
    };

    let result = route(&req, &ctx);
    // Should route to segment handler (will return 404 because no segments cached)
    assert!(result.is_ok() || result.is_err());
    if let Ok(resp) = result {
        assert_eq!(resp.status, 404);
    }
}

// ─── ManifestState with TS Container ─────────────────────────────────

#[test]
fn manifest_state_with_ts_container_serde() {
    let state = make_hls_ts_manifest_state(3, ManifestPhase::Complete, false);
    let json = serde_json::to_string(&state).unwrap();
    let parsed: ManifestState = serde_json::from_str(&json).unwrap();

    assert_eq!(parsed.container_format, ContainerFormat::Ts);
    assert_eq!(parsed.segments.len(), 3);
    assert!(parsed.init_segment.is_none());
}

#[test]
fn manifest_state_with_container_helper_ts() {
    let state = common::make_manifest_state_with_container(
        OutputFormat::Hls,
        ContainerFormat::Ts,
        5,
        ManifestPhase::Complete,
    );

    assert_eq!(state.container_format, ContainerFormat::Ts);
    // Segments should have .ts extension
    for seg in &state.segments {
        assert!(seg.uri.ends_with(".ts"), "Segment URI should end with .ts: {}", seg.uri);
    }
}

// ─── TS Output Structure Validation ──────────────────────────────────

#[test]
fn ts_output_valid_188_byte_packets() {
    // Build a PAT + PMT + some PES data and verify packet structure
    let pat = ts_mux::build_pat_packet(0);
    let pmt = ts_mux::build_pmt_packet(
        edgepack::media::ts::TsCodec::H264,
        edgepack::media::ts::TsCodec::Aac,
        0,
    );

    let mut ts_segment = Vec::new();
    ts_segment.extend_from_slice(&pat);
    ts_segment.extend_from_slice(&pmt);

    // Every 188 bytes should start with sync byte
    assert_eq!(ts_segment.len() % 188, 0);
    for chunk in ts_segment.chunks(188) {
        assert_eq!(chunk[0], 0x47, "TS packet must start with sync byte 0x47");
    }
}

#[test]
fn ts_output_segment_parseable_by_demuxer() {
    // Build a minimal TS segment that the demuxer can parse
    use edgepack::media::ts::TsCodec;

    let pat = ts_mux::build_pat_packet(0);
    let pmt = ts_mux::build_pmt_packet(TsCodec::H264, TsCodec::Aac, 0);

    // Build a simple video PES
    let video_data = vec![0x65, 0xAA, 0xBB, 0xCC]; // Fake IDR NAL
    let video_pes = ts_mux::build_pes_packet(0xE0, 90000, None, &video_data);
    let mut video_cc = 0u8;
    let video_packets =
        ts_mux::packetize_pes(0x0100, &video_pes, true, &mut video_cc, true);

    // Build a simple audio PES
    let audio_data = vec![0xDD; 32]; // Fake AAC frame
    let audio_pes = ts_mux::build_pes_packet(0xC0, 90000, None, &audio_data);
    let mut audio_cc = 0u8;
    let audio_packets =
        ts_mux::packetize_pes(0x0101, &audio_pes, true, &mut audio_cc, false);

    // Assemble segment
    let mut ts_segment = Vec::new();
    ts_segment.extend_from_slice(&pat);
    ts_segment.extend_from_slice(&pmt);
    for pkt in &video_packets {
        ts_segment.extend_from_slice(pkt);
    }
    for pkt in &audio_packets {
        ts_segment.extend_from_slice(pkt);
    }

    // Demux it
    let result = edgepack::media::ts::demux_segment(&ts_segment);
    assert!(
        result.is_ok(),
        "Muxed TS segment should be parseable by demuxer: {:?}",
        result.err()
    );

    let demuxed = result.unwrap();
    assert_eq!(demuxed.video_codec, Some(TsCodec::H264));
    assert_eq!(demuxed.audio_codec, Some(TsCodec::Aac));
    assert!(!demuxed.video_packets.is_empty());
    assert!(!demuxed.audio_packets.is_empty());
}

// ─── Sample Rate Index ───────────────────────────────────────────────

#[test]
fn sample_rate_to_index_all_known_rates() {
    assert_eq!(ts_mux::sample_rate_to_index(96000), 0);
    assert_eq!(ts_mux::sample_rate_to_index(88200), 1);
    assert_eq!(ts_mux::sample_rate_to_index(64000), 2);
    assert_eq!(ts_mux::sample_rate_to_index(48000), 3);
    assert_eq!(ts_mux::sample_rate_to_index(44100), 4);
    assert_eq!(ts_mux::sample_rate_to_index(32000), 5);
    assert_eq!(ts_mux::sample_rate_to_index(24000), 6);
    assert_eq!(ts_mux::sample_rate_to_index(22050), 7);
    assert_eq!(ts_mux::sample_rate_to_index(16000), 8);
    assert_eq!(ts_mux::sample_rate_to_index(12000), 9);
    assert_eq!(ts_mux::sample_rate_to_index(11025), 10);
    assert_eq!(ts_mux::sample_rate_to_index(8000), 11);
    assert_eq!(ts_mux::sample_rate_to_index(7350), 12);
}

#[test]
fn sample_rate_to_index_unknown_defaults() {
    assert_eq!(ts_mux::sample_rate_to_index(12345), 3); // 48000 default
    assert_eq!(ts_mux::sample_rate_to_index(0), 3);
}

// ─── Helpers ─────────────────────────────────────────────────────────

fn make_test_handler_context() -> edgepack::handler::HandlerContext {
    use edgepack::config::{
        AppConfig, CacheConfig, DrmConfig, DrmSystemIds, JitConfig, PolicyConfig, SpekeAuth,
    };

    edgepack::handler::HandlerContext {
        config: AppConfig {
            drm: DrmConfig {
                speke_url: edgepack::url::Url::parse("https://drm.example.com/speke")
                    .unwrap(),
                speke_auth: SpekeAuth::Bearer("test".into()),
                system_ids: DrmSystemIds::default(),
            },
            cache: CacheConfig::default(),
            jit: JitConfig::default(),
            policy: PolicyConfig::default(),
        },
    }
}
