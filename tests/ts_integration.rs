//! Integration tests for MPEG-TS input (Phase 10).
//!
//! Tests cover:
//! - Synthetic TS packet building and demuxing
//! - TS-to-CMAF transmuxing (init synthesis, segment conversion)
//! - AES-128-CBC encrypt/decrypt roundtrip for TS segments
//! - HLS manifest parsing with TS source detection
//! - Cross-module workflows: demux → extract config → synthesize init → transmux
//!
//! All tests are feature-gated behind `#[cfg(feature = "ts")]`.

#![cfg(feature = "ts")]

mod common;

use edgepack::media::ts::{
    self, DemuxedSegment, PesPacket, TsCodec, TsDemuxer, TS_PACKET_SIZE, TS_SYNC_BYTE, PAT_PID,
};
use edgepack::media::transmux::{
    self, AudioConfig, VideoConfig,
};
use edgepack::manifest::hls_input;

// ─── Test Helpers ────────────────────────────────────────────────────

/// Build a minimal 188-byte TS packet with given PID, PUSI flag, CC, and payload.
fn build_ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
    let mut pkt = vec![0u8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = ((pid >> 8) as u8 & 0x1F) | if pusi { 0x40 } else { 0x00 };
    pkt[2] = (pid & 0xFF) as u8;
    // adaptation_field_control = 01 (payload only), continuity_counter
    pkt[3] = 0x10 | (cc & 0x0F);
    let copy_len = payload.len().min(TS_PACKET_SIZE - 4);
    pkt[4..4 + copy_len].copy_from_slice(&payload[..copy_len]);
    pkt
}

/// Build a PAT payload pointing to the given PMT PID.
fn build_pat_payload(pmt_pid: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(0x00); // pointer_field
    payload.push(0x00); // table_id = 0x00 (PAT)
    let section_length: u16 = 5 + 4 + 4;
    payload.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    payload.push((section_length & 0xFF) as u8);
    payload.extend_from_slice(&[0x00, 0x01]); // transport_stream_id
    payload.push(0xC1); // version=0, current=1
    payload.push(0x00); // section_number
    payload.push(0x00); // last_section_number
    payload.extend_from_slice(&[0x00, 0x01]); // program_number = 1
    payload.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
    payload.push((pmt_pid & 0xFF) as u8);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // CRC32 placeholder
    payload
}

/// Build a PMT payload with video (H.264) and audio (AAC) streams.
fn build_pmt_payload(video_pid: u16, audio_pid: u16) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(0x00); // pointer_field
    payload.push(0x02); // table_id = 0x02 (PMT)
    let section_length: u16 = 5 + 4 + 5 + 5 + 4;
    payload.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    payload.push((section_length & 0xFF) as u8);
    payload.extend_from_slice(&[0x00, 0x01]); // program_number
    payload.push(0xC1); // version=0, current=1
    payload.push(0x00); // section_number
    payload.push(0x00); // last_section_number
    payload.push(0xE0 | ((video_pid >> 8) as u8 & 0x1F));
    payload.push((video_pid & 0xFF) as u8);
    payload.extend_from_slice(&[0xF0, 0x00]); // program_info_length = 0
    // Video stream: H.264 (0x1B)
    payload.push(0x1B);
    payload.push(0xE0 | ((video_pid >> 8) as u8 & 0x1F));
    payload.push((video_pid & 0xFF) as u8);
    payload.extend_from_slice(&[0xF0, 0x00]);
    // Audio stream: AAC (0x0F)
    payload.push(0x0F);
    payload.push(0xE0 | ((audio_pid >> 8) as u8 & 0x1F));
    payload.push((audio_pid & 0xFF) as u8);
    payload.extend_from_slice(&[0xF0, 0x00]);
    payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // CRC32 placeholder
    payload
}

/// Build a PES packet with PTS for the given stream ID and ES data.
fn build_pes_packet(stream_id: u8, pts: u64, data: &[u8]) -> Vec<u8> {
    let mut pes = Vec::new();
    pes.extend_from_slice(&[0x00, 0x00, 0x01]); // start code
    pes.push(stream_id);
    let pes_data_len = 3 + 5 + data.len();
    if stream_id >= 0xC0 && stream_id <= 0xDF {
        pes.extend_from_slice(&(pes_data_len as u16).to_be_bytes());
    } else {
        pes.extend_from_slice(&[0x00, 0x00]); // unbounded
    }
    pes.push(0x80); // marker bits
    pes.push(0x80); // PTS only
    pes.push(0x05); // PES header data length = 5
    // Encode PTS
    pes.push(0x21 | (((pts >> 30) as u8 & 0x07) << 1));
    pes.push(((pts >> 22) & 0xFF) as u8);
    pes.push((((pts >> 15) & 0x7F) as u8) << 1 | 0x01);
    pes.push(((pts >> 7) & 0xFF) as u8);
    pes.push((((pts) & 0x7F) as u8) << 1 | 0x01);
    pes.extend_from_slice(data);
    pes
}

/// Build a minimal H.264 SPS NAL unit (Annex B format).
/// Profile: Baseline (66), Level: 3.0 (30).
fn build_h264_sps() -> Vec<u8> {
    let mut sps = Vec::new();
    // Start code
    sps.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    // NAL header: forbidden_zero_bit(0) + nal_ref_idc(3) + nal_unit_type(7=SPS)
    sps.push(0x67);
    // profile_idc = 66 (Baseline)
    sps.push(66);
    // constraint_set_flags + reserved_zero_2bits
    sps.push(0xC0);
    // level_idc = 30
    sps.push(30);
    // seq_parameter_set_id = 0 (exp-golomb: 1 bit = 0b1 = value 0, but need proper encoding)
    // We use a minimal SPS that the parse_sps function can handle:
    // After profile/constraint/level, we need exp-golomb coded fields.
    // For a 320x240 video:
    // seq_parameter_set_id=0, log2_max_frame_num=0, pic_order_cnt_type=0, log2_max_poc=0,
    // max_num_ref_frames=1, gaps_allowed=0, pic_width_in_mbs_minus1=19(320/16-1),
    // pic_height_in_map_units_minus1=14(240/16-1), frame_mbs_only_flag=1
    // Encoded as exp-golomb:
    // 0(1bit) 0(1bit) 0(1bit) 010(ue=1) 0(1bit) 00101 00(ue=19) 00100 10(ue=14) 1(1bit)
    // Let's just provide enough bytes for the parser to read dimensions.
    // Simpler: just provide raw bytes that parse_sps will interpret as valid enough.
    sps.extend_from_slice(&[
        0xE4, // seq_parameter_set_id=0(1) + log2_max_frame_num_minus4=0(1) + pic_order_cnt_type=0(1) + log2_max_pic_order=0(1) + ...
        0x40, 0x00, 0xDA, 0x10, // exp-golomb encoded values for dimensions + flags
    ]);
    sps
}

/// Build a minimal H.264 PPS NAL unit (Annex B format).
fn build_h264_pps() -> Vec<u8> {
    let mut pps = Vec::new();
    pps.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    // NAL header: forbidden_zero_bit(0) + nal_ref_idc(3) + nal_unit_type(8=PPS)
    pps.push(0x68);
    // Minimal PPS data: pic_parameter_set_id=0, seq_parameter_set_id=0, ...
    pps.extend_from_slice(&[0xCE, 0x38, 0x80]);
    pps
}

/// Build an H.264 IDR NAL unit (Annex B format).
fn build_h264_idr() -> Vec<u8> {
    let mut idr = Vec::new();
    idr.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
    idr.push(0x65); // NAL type 5 = IDR
    idr.extend_from_slice(&[0xB8, 0x00, 0x04, 0x00]); // Minimal slice data
    idr
}

/// Build a minimal ADTS frame header for AAC-LC at 44100 Hz stereo.
fn build_adts_frame(aac_data: &[u8]) -> Vec<u8> {
    let frame_len = 7 + aac_data.len();
    let mut adts = Vec::new();
    adts.push(0xFF); // syncword high
    adts.push(0xF1); // syncword low + ID=0(MPEG-4) + layer=00 + protection_absent=1
    // profile=1(AAC-LC) + sampling_freq_idx=4(44100) + private=0 + channel_config=2(stereo)
    // 01 0100 0 10 => 0x54, then 0x80 for the remaining bits
    adts.push(0x50); // profile(2bits) + sampling_freq_index(4bits, 0100=44100) high 2 bits
    adts.push(0x80 | ((frame_len >> 11) as u8 & 0x03)); // channel_config(3bits=010) + original/copy + home + copyright + frame_length high
    adts.push(((frame_len >> 3) & 0xFF) as u8);
    adts.push(((frame_len & 0x07) as u8) << 5 | 0x1F);
    adts.push(0xFC); // buffer fullness + number_of_raw_data_blocks
    adts.extend_from_slice(aac_data);
    adts
}

/// Build a complete synthetic TS segment with PAT, PMT, video PES, and audio PES.
fn build_synthetic_ts_segment() -> Vec<u8> {
    let video_pid: u16 = 0x101;
    let audio_pid: u16 = 0x102;
    let pmt_pid: u16 = 0x100;

    let mut ts_data = Vec::new();

    // PAT
    ts_data.extend_from_slice(&build_ts_packet(PAT_PID, true, 0, &build_pat_payload(pmt_pid)));

    // PMT
    ts_data.extend_from_slice(&build_ts_packet(pmt_pid, true, 0, &build_pmt_payload(video_pid, audio_pid)));

    // Video PES: SPS + PPS + IDR in Annex B format
    let mut video_es = Vec::new();
    video_es.extend_from_slice(&build_h264_sps());
    video_es.extend_from_slice(&build_h264_pps());
    video_es.extend_from_slice(&build_h264_idr());
    let video_pes = build_pes_packet(0xE0, 90000, &video_es);
    ts_data.extend_from_slice(&build_ts_packet(video_pid, true, 0, &video_pes));

    // Audio PES: ADTS frame
    let aac_data = vec![0xDE, 0xAD, 0xBE, 0xEF]; // Fake AAC payload
    let adts_frame = build_adts_frame(&aac_data);
    let audio_pes = build_pes_packet(0xC0, 90000, &adts_frame);
    ts_data.extend_from_slice(&build_ts_packet(audio_pid, true, 0, &audio_pes));

    ts_data
}

// ─── Demuxer Integration Tests ───────────────────────────────────────

#[test]
fn demux_synthetic_ts_segment_extracts_video_and_audio() {
    let ts_data = build_synthetic_ts_segment();
    let result = ts::demux_segment(&ts_data).unwrap();

    assert_eq!(result.video_codec, Some(TsCodec::H264));
    assert_eq!(result.audio_codec, Some(TsCodec::Aac));
    assert_eq!(result.video_packets.len(), 1);
    assert_eq!(result.audio_packets.len(), 1);
    assert!(result.pmt.is_some());
}

#[test]
fn demux_segment_preserves_pts() {
    let ts_data = build_synthetic_ts_segment();
    let result = ts::demux_segment(&ts_data).unwrap();

    assert_eq!(result.video_packets[0].pts, Some(90000));
    assert_eq!(result.audio_packets[0].pts, Some(90000));
}

#[test]
fn demux_segment_detects_correct_pmt_streams() {
    let ts_data = build_synthetic_ts_segment();
    let result = ts::demux_segment(&ts_data).unwrap();

    let pmt = result.pmt.unwrap();
    assert_eq!(pmt.streams.len(), 2);
    assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
    assert_eq!(pmt.streams[1].stream_type, 0x0F); // AAC
}

#[test]
fn demux_video_only_segment() {
    let video_pid: u16 = 0x101;
    let pmt_pid: u16 = 0x100;

    let mut ts_data = Vec::new();
    ts_data.extend_from_slice(&build_ts_packet(PAT_PID, true, 0, &build_pat_payload(pmt_pid)));

    // PMT with video only (build custom single-stream PMT)
    let mut pmt_payload = Vec::new();
    pmt_payload.push(0x00); // pointer_field
    pmt_payload.push(0x02); // table_id = 0x02
    let section_length: u16 = 5 + 4 + 5 + 4;
    pmt_payload.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
    pmt_payload.push((section_length & 0xFF) as u8);
    pmt_payload.extend_from_slice(&[0x00, 0x01]); // program_number
    pmt_payload.push(0xC1);
    pmt_payload.push(0x00);
    pmt_payload.push(0x00);
    pmt_payload.push(0xE0 | ((video_pid >> 8) as u8 & 0x1F));
    pmt_payload.push((video_pid & 0xFF) as u8);
    pmt_payload.extend_from_slice(&[0xF0, 0x00]);
    // Only video stream
    pmt_payload.push(0x1B);
    pmt_payload.push(0xE0 | ((video_pid >> 8) as u8 & 0x1F));
    pmt_payload.push((video_pid & 0xFF) as u8);
    pmt_payload.extend_from_slice(&[0xF0, 0x00]);
    pmt_payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // CRC

    ts_data.extend_from_slice(&build_ts_packet(pmt_pid, true, 0, &pmt_payload));

    let video_es = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA];
    let video_pes = build_pes_packet(0xE0, 90000, &video_es);
    ts_data.extend_from_slice(&build_ts_packet(video_pid, true, 0, &video_pes));

    let result = ts::demux_segment(&ts_data).unwrap();
    assert_eq!(result.video_packets.len(), 1);
    assert!(result.audio_packets.is_empty());
    assert_eq!(result.video_codec, Some(TsCodec::H264));
    assert!(result.audio_codec.is_none());
}

#[test]
fn demux_multiple_video_pes_packets() {
    let video_pid: u16 = 0x101;
    let audio_pid: u16 = 0x102;
    let pmt_pid: u16 = 0x100;

    let mut ts_data = Vec::new();
    ts_data.extend_from_slice(&build_ts_packet(PAT_PID, true, 0, &build_pat_payload(pmt_pid)));
    ts_data.extend_from_slice(&build_ts_packet(pmt_pid, true, 0, &build_pmt_payload(video_pid, audio_pid)));

    // First video PES (IDR)
    let es1 = [0x00, 0x00, 0x00, 0x01, 0x65, 0x11, 0x22];
    let pes1 = build_pes_packet(0xE0, 90000, &es1);
    ts_data.extend_from_slice(&build_ts_packet(video_pid, true, 0, &pes1));

    // Second video PES (non-IDR)
    let es2 = [0x00, 0x00, 0x00, 0x01, 0x41, 0x33, 0x44];
    let pes2 = build_pes_packet(0xE0, 93003, &es2);
    ts_data.extend_from_slice(&build_ts_packet(video_pid, true, 1, &pes2));

    let result = ts::demux_segment(&ts_data).unwrap();
    assert_eq!(result.video_packets.len(), 2);
    assert_eq!(result.video_packets[0].pts, Some(90000));
    assert_eq!(result.video_packets[1].pts, Some(93003));
    // TS packets are fixed 188 bytes, so PES data includes trailing zero padding.
    // Verify the data starts with the expected ES bytes.
    assert!(result.video_packets[0].data.starts_with(&es1),
        "first PES data should start with expected ES bytes");
    assert!(result.video_packets[1].data.starts_with(&es2),
        "second PES data should start with expected ES bytes");
}

#[test]
fn demux_empty_ts_segment() {
    let result = ts::demux_segment(&[]).unwrap();
    assert!(result.video_packets.is_empty());
    assert!(result.audio_packets.is_empty());
    assert!(result.video_codec.is_none());
    assert!(result.audio_codec.is_none());
    assert!(result.pmt.is_none());
}

// ─── AES-128 Encryption Roundtrip Tests ──────────────────────────────

#[test]
fn aes128_cbc_ts_decrypt_roundtrip() {
    use aes::Aes128;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    let key: [u8; 16] = [0x01; 16];
    let iv: [u8; 16] = [0x02; 16];
    let plaintext = vec![0xAA; 64]; // 4 AES blocks

    // Encrypt with PKCS7 padding
    let mut to_encrypt = plaintext.clone();
    let pad_len = 16 - (to_encrypt.len() % 16);
    to_encrypt.extend(vec![pad_len as u8; pad_len]);

    let len = to_encrypt.len();
    let encryptor = Aes128CbcEnc::new(&key.into(), &iv.into());
    let encrypted = encryptor
        .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(
            &mut to_encrypt,
            len,
        )
        .unwrap()
        .to_vec();

    let decrypted = ts::decrypt_ts_segment(&encrypted, &key, &iv).unwrap();
    assert_eq!(decrypted, plaintext, "AES-128-CBC roundtrip should preserve plaintext");
}

#[test]
fn aes128_cbc_ts_decrypt_different_keys_fail() {
    use aes::Aes128;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};
    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    let key: [u8; 16] = [0x01; 16];
    let wrong_key: [u8; 16] = [0x02; 16];
    let iv: [u8; 16] = [0x03; 16];
    let plaintext = vec![0xBB; 32];

    let mut to_encrypt = plaintext.clone();
    let pad_len = 16 - (to_encrypt.len() % 16);
    to_encrypt.extend(vec![pad_len as u8; pad_len]);

    let len = to_encrypt.len();
    let encryptor = Aes128CbcEnc::new(&key.into(), &iv.into());
    let encrypted = encryptor
        .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(
            &mut to_encrypt,
            len,
        )
        .unwrap()
        .to_vec();

    let decrypted = ts::decrypt_ts_segment(&encrypted, &wrong_key, &iv).unwrap();
    // Decryption with wrong key should produce different data
    assert_ne!(decrypted, plaintext);
}

#[test]
fn aes128_cbc_ts_decrypt_invalid_length() {
    let key: [u8; 16] = [0x01; 16];
    let iv: [u8; 16] = [0x02; 16];
    // 17 bytes is not a multiple of 16
    let result = ts::decrypt_ts_segment(&[0x00; 17], &key, &iv);
    assert!(result.is_err());
    assert!(result.unwrap_err().to_string().contains("multiple of 16"));
}

#[test]
fn aes128_cbc_ts_decrypt_empty() {
    let key: [u8; 16] = [0x01; 16];
    let iv: [u8; 16] = [0x02; 16];
    let result = ts::decrypt_ts_segment(&[], &key, &iv).unwrap();
    assert!(result.is_empty());
}

// ─── NAL Unit Extraction Tests ───────────────────────────────────────

#[test]
fn extract_nal_units_from_annex_b() {
    let mut data = Vec::new();
    // 4-byte start code + SPS
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0xAA, 0xBB]);
    // 4-byte start code + PPS
    data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xCC]);
    // 3-byte start code + IDR
    data.extend_from_slice(&[0x00, 0x00, 0x01, 0x65, 0xDD, 0xEE]);

    let nals = transmux::extract_h264_nal_units(&data);
    assert_eq!(nals.len(), 3);
    assert_eq!(nals[0].0, 7); // SPS
    assert_eq!(nals[1].0, 8); // PPS
    assert_eq!(nals[2].0, 5); // IDR
}

#[test]
fn extract_nal_units_empty_data() {
    let nals = transmux::extract_h264_nal_units(&[]);
    assert!(nals.is_empty());
}

#[test]
fn extract_nal_units_no_start_code() {
    let data = [0x01, 0x02, 0x03, 0x04, 0x05];
    let nals = transmux::extract_h264_nal_units(&data);
    assert!(nals.is_empty());
}

// ─── Transmuxer Integration Tests ────────────────────────────────────

#[test]
fn synthesize_init_segment_video_only() {
    // Build a video config manually
    let video_config = VideoConfig {
        codec: TsCodec::H264,
        width: 320,
        height: 240,
        sps: vec![0x67, 0x42, 0xC0, 0x1E, 0xE4, 0x40, 0x00, 0xDA, 0x10],
        pps: vec![0x68, 0xCE, 0x38, 0x80],
        profile_idc: 66,
        level_idc: 30,
        codec_string: "avc1.42c01e".to_string(),
    };

    let init = transmux::synthesize_init_segment(Some(&video_config), None).unwrap();
    assert!(!init.is_empty(), "init segment should not be empty");

    // Verify it starts with ftyp box
    assert_eq!(&init[4..8], b"ftyp", "init segment should start with ftyp box");

    // Verify moov box is present
    let has_moov = init.windows(4).any(|w| w == b"moov");
    assert!(has_moov, "init segment should contain moov box");
}

#[test]
fn synthesize_init_segment_audio_only() {
    let audio_config = AudioConfig {
        codec: TsCodec::Aac,
        sample_rate: 44100,
        channel_count: 2,
        aac_profile: 2, // AAC-LC
        codec_string: "mp4a.40.2".to_string(),
    };

    let init = transmux::synthesize_init_segment(None, Some(&audio_config)).unwrap();
    assert!(!init.is_empty());
    assert_eq!(&init[4..8], b"ftyp");
}

#[test]
fn synthesize_init_segment_video_and_audio() {
    let video_config = VideoConfig {
        codec: TsCodec::H264,
        width: 1920,
        height: 1080,
        sps: vec![0x67, 0x64, 0x00, 0x28],
        pps: vec![0x68, 0xEE, 0x3C, 0x80],
        profile_idc: 100,
        level_idc: 40,
        codec_string: "avc1.640028".to_string(),
    };

    let audio_config = AudioConfig {
        codec: TsCodec::Aac,
        sample_rate: 48000,
        channel_count: 2,
        aac_profile: 2,
        codec_string: "mp4a.40.2".to_string(),
    };

    let init = transmux::synthesize_init_segment(Some(&video_config), Some(&audio_config)).unwrap();
    assert!(!init.is_empty());
    assert_eq!(&init[4..8], b"ftyp");

    // Should have two trak boxes (video + audio)
    let trak_count = init.windows(4).filter(|w| *w == b"trak").count();
    assert_eq!(trak_count, 2, "init segment should have 2 trak boxes (video + audio)");
}

#[test]
fn synthesize_init_segment_no_config_errors() {
    let result = transmux::synthesize_init_segment(None, None);
    assert!(result.is_err(), "should error with no video or audio config");
}

#[test]
fn transmux_demuxed_segment_to_cmaf() {
    // Build a synthetic demuxed segment
    let demuxed = DemuxedSegment {
        video_packets: vec![PesPacket {
            stream_id: 0xE0,
            pts: Some(90000),
            dts: None,
            // Annex B format: start code + IDR NAL
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB, 0xCC],
        }],
        audio_packets: vec![PesPacket {
            stream_id: 0xC0,
            pts: Some(90000),
            dts: None,
            // ADTS header (7 bytes) + AAC data
            data: build_adts_frame(&[0xDE, 0xAD]),
        }],
        video_codec: Some(TsCodec::H264),
        audio_codec: Some(TsCodec::Aac),
        pmt: None,
    };

    let video_config = VideoConfig {
        codec: TsCodec::H264,
        width: 320,
        height: 240,
        sps: vec![0x67, 0x42, 0xC0, 0x1E],
        pps: vec![0x68, 0xCE, 0x38, 0x80],
        profile_idc: 66,
        level_idc: 30,
        codec_string: "avc1.42c01e".to_string(),
    };

    let audio_config = AudioConfig {
        codec: TsCodec::Aac,
        sample_rate: 44100,
        channel_count: 2,
        aac_profile: 2,
        codec_string: "mp4a.40.2".to_string(),
    };

    let cmaf = transmux::transmux_to_cmaf(&demuxed, Some(&video_config), Some(&audio_config), 0).unwrap();
    assert!(!cmaf.is_empty(), "transmuxed CMAF segment should not be empty");

    // Should contain moof and mdat boxes
    let has_moof = cmaf.windows(4).any(|w| w == b"moof");
    let has_mdat = cmaf.windows(4).any(|w| w == b"mdat");
    assert!(has_moof, "CMAF segment should contain moof box");
    assert!(has_mdat, "CMAF segment should contain mdat box");
}

#[test]
fn transmux_video_only_segment() {
    let demuxed = DemuxedSegment {
        video_packets: vec![PesPacket {
            stream_id: 0xE0,
            pts: Some(90000),
            dts: None,
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA],
        }],
        audio_packets: vec![],
        video_codec: Some(TsCodec::H264),
        audio_codec: None,
        pmt: None,
    };

    let video_config = VideoConfig {
        codec: TsCodec::H264,
        width: 640,
        height: 480,
        sps: vec![0x67, 0x42, 0xC0, 0x1E],
        pps: vec![0x68, 0xCE, 0x38, 0x80],
        profile_idc: 66,
        level_idc: 30,
        codec_string: "avc1.42c01e".to_string(),
    };

    let cmaf = transmux::transmux_to_cmaf(&demuxed, Some(&video_config), None, 1).unwrap();
    assert!(!cmaf.is_empty());
    let has_moof = cmaf.windows(4).any(|w| w == b"moof");
    assert!(has_moof);
}

// ─── Full Pipeline Tests: demux → config → init → transmux ──────────

#[test]
fn full_ts_to_cmaf_pipeline_video_config_extraction() {
    // Build ES data with SPS + PPS + IDR
    let mut video_es = Vec::new();
    video_es.extend_from_slice(&build_h264_sps());
    video_es.extend_from_slice(&build_h264_pps());
    video_es.extend_from_slice(&build_h264_idr());

    let video_pes = PesPacket {
        stream_id: 0xE0,
        pts: Some(90000),
        dts: None,
        data: video_es,
    };

    // Extract video config from PES
    let video_config = transmux::extract_video_config(&video_pes).unwrap();
    assert_eq!(video_config.codec, TsCodec::H264);
    assert!(!video_config.sps.is_empty(), "SPS should be extracted");
    assert!(!video_config.pps.is_empty(), "PPS should be extracted");
    assert!(!video_config.codec_string.is_empty(), "codec string should be generated");
}

#[test]
fn full_ts_to_cmaf_pipeline_audio_config_extraction() {
    let aac_data = vec![0xDE, 0xAD, 0xBE, 0xEF];
    let adts_frame = build_adts_frame(&aac_data);

    let audio_pes = PesPacket {
        stream_id: 0xC0,
        pts: Some(90000),
        dts: None,
        data: adts_frame,
    };

    let audio_config = transmux::extract_audio_config(&audio_pes).unwrap();
    assert_eq!(audio_config.codec, TsCodec::Aac);
    assert!(audio_config.sample_rate > 0, "sample rate should be extracted");
    assert!(audio_config.channel_count > 0, "channel count should be extracted");
    assert!(!audio_config.codec_string.is_empty(), "codec string should be generated");
}

// ─── HLS Manifest TS Detection Tests ─────────────────────────────────

#[test]
fn hls_manifest_detects_ts_source() {
    let manifest = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXTINF:6.006,
https://cdn.example.com/segment0.ts
#EXTINF:6.006,
https://cdn.example.com/segment1.ts
#EXTINF:6.006,
https://cdn.example.com/segment2.ts
#EXT-X-ENDLIST
"#;

    let source = hls_input::parse_hls_manifest(manifest, "https://cdn.example.com/master.m3u8").unwrap();
    assert!(source.is_ts_source, "should detect .ts extension as TS source");
    assert_eq!(source.segment_urls.len(), 3);
    assert!(source.aes128_key_url.is_none());
    assert!(source.aes128_iv.is_none());
}

#[test]
fn hls_manifest_detects_ts_with_aes128() {
    let manifest = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXT-X-KEY:METHOD=AES-128,URI="https://keys.example.com/key.bin",IV=0x00000000000000000000000000000001
#EXTINF:6.006,
https://cdn.example.com/segment0.ts
#EXTINF:6.006,
https://cdn.example.com/segment1.ts
#EXT-X-ENDLIST
"#;

    let source = hls_input::parse_hls_manifest(manifest, "https://cdn.example.com/master.m3u8").unwrap();
    assert!(source.is_ts_source);
    assert_eq!(source.aes128_key_url, Some("https://keys.example.com/key.bin".to_string()));
    assert!(source.aes128_iv.is_some());
    let iv = source.aes128_iv.unwrap();
    assert_eq!(iv[15], 0x01);
}

#[test]
fn hls_manifest_cmaf_not_detected_as_ts() {
    let manifest = r#"#EXTM3U
#EXT-X-VERSION:7
#EXT-X-TARGETDURATION:6
#EXT-X-MAP:URI="init.mp4"
#EXTINF:6.006,
segment0.cmfv
#EXTINF:6.006,
segment1.cmfv
#EXT-X-ENDLIST
"#;

    let source = hls_input::parse_hls_manifest(manifest, "https://cdn.example.com/master.m3u8").unwrap();
    assert!(!source.is_ts_source, "CMAF segments should not be detected as TS");
}

#[test]
fn hls_manifest_ts_with_query_string() {
    let manifest = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXTINF:6.006,
https://cdn.example.com/segment0.ts?token=abc123
#EXT-X-ENDLIST
"#;

    let source = hls_input::parse_hls_manifest(manifest, "https://cdn.example.com/master.m3u8").unwrap();
    assert!(source.is_ts_source, "should detect .ts with query string as TS source");
}

#[test]
fn hls_manifest_ts_no_map_tag_accepted() {
    // TS manifests typically don't have EXT-X-MAP
    let manifest = r#"#EXTM3U
#EXT-X-VERSION:3
#EXT-X-TARGETDURATION:6
#EXTINF:6.006,
segment0.ts
#EXTINF:6.006,
segment1.ts
#EXT-X-ENDLIST
"#;

    let source = hls_input::parse_hls_manifest(manifest, "https://cdn.example.com/master.m3u8").unwrap();
    assert!(source.is_ts_source);
    // init_segment_url should be empty for TS sources without EXT-X-MAP
    assert!(source.init_segment_url.is_empty(), "TS source without EXT-X-MAP should have empty init URL");
}

// ─── SourceManifest Serde Tests ──────────────────────────────────────

#[test]
fn source_manifest_ts_fields_serde_roundtrip() {
    use edgepack::manifest::types::SourceManifest;

    let source = SourceManifest {
        init_segment_url: String::new(),
        segment_urls: vec!["https://cdn.example.com/seg0.ts".to_string()],
        segment_durations: vec![6.006],
        is_live: false,
        source_scheme: None,
        ad_breaks: vec![],
        parts: vec![],
        part_target_duration: None,
        server_control: None,
        ll_dash_info: None,
        is_ts_source: true,
        aes128_key_url: Some("https://keys.example.com/key.bin".to_string()),
        aes128_iv: Some([0u8; 16]),
        content_steering: None,
        init_byte_range: None,
        segment_byte_ranges: Vec::new(),
        segment_base: None,
        source_variants: Vec::new(),
    };

    let json = serde_json::to_string(&source).unwrap();
    let deserialized: SourceManifest = serde_json::from_str(&json).unwrap();
    assert!(deserialized.is_ts_source);
    assert_eq!(deserialized.aes128_key_url, Some("https://keys.example.com/key.bin".to_string()));
    assert_eq!(deserialized.aes128_iv, Some([0u8; 16]));
}

#[test]
fn source_manifest_backward_compat_without_ts_fields() {
    use edgepack::manifest::types::SourceManifest;

    // JSON without TS fields should deserialize with defaults
    let json = r#"{
        "init_segment_url": "https://cdn.example.com/init.mp4",
        "segment_urls": ["https://cdn.example.com/seg0.cmfv"],
        "segment_durations": [6.006],
        "is_live": false,
        "source_scheme": null,
        "ad_breaks": [],
        "parts": [],
        "part_target_duration": null,
        "server_control": null,
        "ll_dash_info": null
    }"#;

    let deserialized: SourceManifest = serde_json::from_str(json).unwrap();
    assert!(!deserialized.is_ts_source);
    assert!(deserialized.aes128_key_url.is_none());
    assert!(deserialized.aes128_iv.is_none());
}

// ─── TsDemuxer Stateful Tests ────────────────────────────────────────

#[test]
fn ts_demuxer_stateful_push_and_flush() {
    let video_pid: u16 = 0x101;
    let audio_pid: u16 = 0x102;
    let pmt_pid: u16 = 0x100;

    let mut demuxer = TsDemuxer::new();

    // Push PAT
    let pat_payload = build_pat_payload(pmt_pid);
    let pat_pkt = ts::parse_ts_packet(&build_ts_packet(PAT_PID, true, 0, &pat_payload)).unwrap();
    demuxer.push_packet(&pat_pkt).unwrap();

    // Push PMT
    let pmt_payload = build_pmt_payload(video_pid, audio_pid);
    let pmt_pkt = ts::parse_ts_packet(&build_ts_packet(pmt_pid, true, 0, &pmt_payload)).unwrap();
    demuxer.push_packet(&pmt_pkt).unwrap();

    // Push video PES
    let video_es = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA];
    let video_pes = build_pes_packet(0xE0, 90000, &video_es);
    let video_pkt = ts::parse_ts_packet(&build_ts_packet(video_pid, true, 0, &video_pes)).unwrap();
    demuxer.push_packet(&video_pkt).unwrap();

    // Push audio PES
    let audio_es = build_adts_frame(&[0xBB]);
    let audio_pes = build_pes_packet(0xC0, 90000, &audio_es);
    let audio_pkt = ts::parse_ts_packet(&build_ts_packet(audio_pid, true, 0, &audio_pes)).unwrap();
    demuxer.push_packet(&audio_pkt).unwrap();

    let result = demuxer.flush();
    assert_eq!(result.video_packets.len(), 1);
    assert_eq!(result.audio_packets.len(), 1);
    assert_eq!(result.video_codec, Some(TsCodec::H264));
    assert_eq!(result.audio_codec, Some(TsCodec::Aac));
}

#[test]
fn avcc_conversion_produces_valid_output() {
    // Annex B data with SPS + PPS + IDR
    let mut annexb_data = Vec::new();
    annexb_data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // start code
    annexb_data.push(0x65); // IDR NAL
    annexb_data.extend_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD]);

    let avcc = transmux::convert_annexb_to_avcc(&annexb_data);
    assert!(!avcc.is_empty(), "AVCC output should not be empty");

    // AVCC format: 4-byte length prefix + NAL data
    let nal_len = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]) as usize;
    assert_eq!(nal_len, 5, "NAL length should be 5 (1 header + 4 data bytes)");
    assert_eq!(avcc[4], 0x65, "first NAL should be IDR");
}
