//! MPEG-TS to CMAF transmuxer.
//!
//! Converts demuxed PES packets into CMAF-compatible fMP4 (ftyp+moov init + moof+mdat segments).
//! Feature-gated behind `#[cfg(feature = "ts")]`.

use crate::error::{EdgepackError, Result};
use crate::media::cmaf;
use crate::media::ts::{DemuxedSegment, PesPacket, TsCodec};

/// Extracted H.264 video configuration from SPS/PPS NAL units.
#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub codec: TsCodec,
    pub width: u16,
    pub height: u16,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
    pub profile_idc: u8,
    pub level_idc: u8,
    pub codec_string: String,
}

/// Extracted AAC audio configuration from ADTS headers.
#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub codec: TsCodec,
    pub sample_rate: u32,
    pub channel_count: u8,
    pub aac_profile: u8,
    pub codec_string: String,
}

/// Extract H.264 NAL units from Annex B byte stream.
///
/// Returns `Vec<(nal_type, nal_data)>` where `nal_data` is the raw NAL unit
/// bytes (without the start code prefix).
pub fn extract_h264_nal_units(data: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut nals = Vec::new();
    let mut i = 0;

    while i < data.len() {
        // Look for start code: 00 00 01 or 00 00 00 01
        let (found, start) = find_start_code(data, i);
        if !found {
            break;
        }

        // Find the end of this NAL (next start code or end of data)
        let (next_found, next_start) = find_next_start_code_or_end(data, start);
        let nal_end = if next_found { next_start } else { data.len() };

        if start < nal_end && start < data.len() {
            let nal_type = data[start] & 0x1F;
            let nal_data = data[start..nal_end].to_vec();
            nals.push((nal_type, nal_data));
        }

        i = nal_end;
    }

    nals
}

fn find_start_code(data: &[u8], start: usize) -> (bool, usize) {
    let mut i = start;
    while i + 2 < data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            if data[i + 2] == 0x01 {
                return (true, i + 3);
            }
            if i + 3 < data.len() && data[i + 2] == 0x00 && data[i + 3] == 0x01 {
                return (true, i + 4);
            }
        }
        i += 1;
    }
    (false, data.len())
}

fn find_next_start_code_or_end(data: &[u8], start: usize) -> (bool, usize) {
    let mut i = start;
    while i + 2 < data.len() {
        if data[i] == 0x00 && data[i + 1] == 0x00 {
            if data[i + 2] == 0x01 {
                return (true, i);
            }
            if i + 3 < data.len() && data[i + 2] == 0x00 && data[i + 3] == 0x01 {
                return (true, i);
            }
        }
        i += 1;
    }
    (false, data.len())
}

/// Parse SPS (Sequence Parameter Set) to extract video dimensions and profile info.
///
/// Returns `(width, height, profile_idc, level_idc)`.
pub fn parse_sps(sps_data: &[u8]) -> (u16, u16, u8, u8) {
    if sps_data.len() < 4 {
        return (0, 0, 0, 0);
    }

    let profile_idc = sps_data[1];
    let _constraint_set_flags = sps_data[2];
    let level_idc = sps_data[3];

    // Simplified SPS parsing: extract width/height from exp-golomb coded values.
    // Full SPS parsing requires a bitstream reader with exp-golomb decoding.
    // For a production parser, this would be more thorough. Here we use a
    // simplified approach that works for common H.264 profiles.
    let (width, height) = parse_sps_dimensions(sps_data);

    (width, height, profile_idc, level_idc)
}

/// Simplified SPS dimension parser using exp-golomb decoding.
fn parse_sps_dimensions(sps_data: &[u8]) -> (u16, u16) {
    if sps_data.len() < 5 {
        return (0, 0);
    }

    let mut reader = BitReader::new(sps_data);

    // Skip forbidden_zero_bit(1) + nal_ref_idc(2) + nal_unit_type(5)
    reader.skip(8);
    // profile_idc(8)
    let profile_idc = reader.read_bits(8) as u8;
    // constraint_set0..5_flag(6) + reserved_zero_2bits(2)
    reader.skip(8);
    // level_idc(8)
    reader.skip(8);
    // seq_parameter_set_id (ue)
    reader.read_exp_golomb();

    if profile_idc == 100 || profile_idc == 110 || profile_idc == 122
        || profile_idc == 244 || profile_idc == 44 || profile_idc == 83
        || profile_idc == 86 || profile_idc == 118 || profile_idc == 128
    {
        let chroma_format_idc = reader.read_exp_golomb();
        if chroma_format_idc == 3 {
            reader.skip(1); // separate_colour_plane_flag
        }
        reader.read_exp_golomb(); // bit_depth_luma_minus8
        reader.read_exp_golomb(); // bit_depth_chroma_minus8
        reader.skip(1); // qpprime_y_zero_transform_bypass_flag
        let seq_scaling_matrix_present = reader.read_bits(1);
        if seq_scaling_matrix_present == 1 {
            let count = if chroma_format_idc != 3 { 8 } else { 12 };
            for _ in 0..count {
                let present = reader.read_bits(1);
                if present == 1 {
                    skip_scaling_list(&mut reader, if count <= 6 { 16 } else { 64 });
                }
            }
        }
    }

    // log2_max_frame_num_minus4 (ue)
    reader.read_exp_golomb();
    // pic_order_cnt_type (ue)
    let poc_type = reader.read_exp_golomb();
    if poc_type == 0 {
        reader.read_exp_golomb(); // log2_max_pic_order_cnt_lsb_minus4
    } else if poc_type == 1 {
        reader.skip(1); // delta_pic_order_always_zero_flag
        reader.read_signed_exp_golomb(); // offset_for_non_ref_pic
        reader.read_signed_exp_golomb(); // offset_for_top_to_bottom_field
        let num_ref = reader.read_exp_golomb();
        for _ in 0..num_ref {
            reader.read_signed_exp_golomb();
        }
    }

    // max_num_ref_frames (ue)
    reader.read_exp_golomb();
    // gaps_in_frame_num_value_allowed_flag
    reader.skip(1);

    // pic_width_in_mbs_minus1 (ue)
    let pic_width_in_mbs_minus1 = reader.read_exp_golomb();
    // pic_height_in_map_units_minus1 (ue)
    let pic_height_in_map_units_minus1 = reader.read_exp_golomb();
    // frame_mbs_only_flag
    let frame_mbs_only_flag = reader.read_bits(1);

    let width = ((pic_width_in_mbs_minus1 + 1) * 16) as u16;
    let height = ((2 - frame_mbs_only_flag as u64) * (pic_height_in_map_units_minus1 + 1) * 16) as u16;

    (width, height)
}

fn skip_scaling_list(reader: &mut BitReader, size: usize) {
    let mut last_scale = 8i64;
    let mut next_scale = 8i64;
    for _ in 0..size {
        if next_scale != 0 {
            let delta = reader.read_signed_exp_golomb();
            next_scale = (last_scale + delta + 256) % 256;
        }
        last_scale = if next_scale == 0 {
            last_scale
        } else {
            next_scale
        };
    }
}

/// Minimal bitstream reader for exp-golomb parsing.
struct BitReader<'a> {
    data: &'a [u8],
    byte_offset: usize,
    bit_offset: u8,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_offset: 0,
            bit_offset: 0,
        }
    }

    fn read_bit(&mut self) -> u8 {
        if self.byte_offset >= self.data.len() {
            return 0;
        }
        let bit = (self.data[self.byte_offset] >> (7 - self.bit_offset)) & 1;
        self.bit_offset += 1;
        if self.bit_offset == 8 {
            self.bit_offset = 0;
            self.byte_offset += 1;
        }
        bit
    }

    fn read_bits(&mut self, n: u8) -> u64 {
        let mut value = 0u64;
        for _ in 0..n {
            value = (value << 1) | self.read_bit() as u64;
        }
        value
    }

    fn skip(&mut self, n: u32) {
        for _ in 0..n {
            self.read_bit();
        }
    }

    fn read_exp_golomb(&mut self) -> u64 {
        let mut leading_zeros = 0u32;
        while self.read_bit() == 0 {
            leading_zeros += 1;
            if leading_zeros > 31 {
                return 0; // Prevent infinite loop on malformed data
            }
        }
        if leading_zeros == 0 {
            return 0;
        }
        let value = self.read_bits(leading_zeros as u8);
        (1u64 << leading_zeros) - 1 + value
    }

    fn read_signed_exp_golomb(&mut self) -> i64 {
        let code = self.read_exp_golomb();
        if code == 0 {
            return 0;
        }
        let sign = if code % 2 == 1 { 1 } else { -1 };
        sign * ((code + 1) / 2) as i64
    }
}

/// Convert Annex B byte stream to AVCC format (4-byte length-prefixed NAL units).
pub fn convert_annexb_to_avcc(data: &[u8]) -> Vec<u8> {
    let nals = extract_h264_nal_units(data);
    let mut avcc = Vec::new();

    for (_, nal_data) in &nals {
        let nal_type = if !nal_data.is_empty() {
            nal_data[0] & 0x1F
        } else {
            continue;
        };
        // Skip SPS (7) and PPS (8) from AVCC data — they go in avcC box
        if nal_type == 7 || nal_type == 8 {
            continue;
        }
        let len = nal_data.len() as u32;
        avcc.extend_from_slice(&len.to_be_bytes());
        avcc.extend_from_slice(nal_data);
    }

    avcc
}

/// Build an avcC (AVC Decoder Configuration Record) box.
pub fn build_avcc_box(sps: &[u8], pps: &[u8], profile: u8, level: u8) -> Vec<u8> {
    let constraint_flags = if sps.len() >= 3 { sps[2] } else { 0 };

    let mut record = Vec::new();
    record.push(1); // configurationVersion
    record.push(profile); // AVCProfileIndication
    record.push(constraint_flags); // profile_compatibility
    record.push(level); // AVCLevelIndication
    record.push(0xFF); // lengthSizeMinusOne = 3 (4-byte lengths), + reserved 6 bits = 0b111111_11
    record.push(0xE1); // numOfSequenceParameterSets = 1, + reserved 3 bits = 0b111_00001
    record.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    record.extend_from_slice(sps);
    record.push(1); // numOfPictureParameterSets = 1
    record.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    record.extend_from_slice(pps);

    // Wrap in avcC box
    let total_size = 8 + record.len() as u32;
    let mut box_data = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut box_data, total_size, b"avcC");
    box_data.extend_from_slice(&record);
    box_data
}

/// Build an esds (Elementary Stream Descriptor) box for AAC.
pub fn build_esds_box(profile: u8, sample_rate: u32, channels: u8) -> Vec<u8> {
    let sample_rate_index = match sample_rate {
        96000 => 0u8,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        _ => 4, // Default to 44100
    };

    // AudioSpecificConfig: 5 bits profile + 4 bits freq_index + 4 bits channel_config
    let asc_byte1 = (profile << 3) | (sample_rate_index >> 1);
    let asc_byte2 = ((sample_rate_index & 1) << 7) | ((channels & 0x0F) << 3);
    let audio_specific_config = [asc_byte1, asc_byte2];

    // Build ES_Descriptor
    let mut es_descriptor = Vec::new();

    // ES_Descriptor tag = 0x03
    es_descriptor.push(0x03);
    // We'll fill in the length later
    let es_desc_start = es_descriptor.len();
    es_descriptor.push(0x00); // placeholder for length

    es_descriptor.extend_from_slice(&[0x00, 0x01]); // ES_ID = 1
    es_descriptor.push(0x00); // streamDependenceFlag=0, URL_Flag=0, OCRstreamFlag=0, streamPriority=0

    // DecoderConfigDescriptor tag = 0x04
    es_descriptor.push(0x04);
    let dec_config_start = es_descriptor.len();
    es_descriptor.push(0x00); // placeholder for length

    es_descriptor.push(0x40); // objectTypeIndication = 0x40 (Audio ISO/IEC 14496-3)
    es_descriptor.push(0x15); // streamType = 0x05 (AudioStream) << 2 | 1 (upstream=0)
    es_descriptor.extend_from_slice(&[0x00, 0x00, 0x00]); // bufferSizeDB = 0
    es_descriptor.extend_from_slice(&[0x00, 0x01, 0xF4, 0x00]); // maxBitrate = 128000
    es_descriptor.extend_from_slice(&[0x00, 0x01, 0xF4, 0x00]); // avgBitrate = 128000

    // DecoderSpecificInfo tag = 0x05
    es_descriptor.push(0x05);
    es_descriptor.push(audio_specific_config.len() as u8);
    es_descriptor.extend_from_slice(&audio_specific_config);

    // Fill in DecoderConfigDescriptor length
    let dec_config_len = es_descriptor.len() - dec_config_start - 1;
    es_descriptor[dec_config_start] = dec_config_len as u8;

    // SLConfigDescriptor tag = 0x06
    es_descriptor.push(0x06);
    es_descriptor.push(0x01);
    es_descriptor.push(0x02); // predefined = 2

    // Fill in ES_Descriptor length
    let es_desc_len = es_descriptor.len() - es_desc_start - 1;
    es_descriptor[es_desc_start] = es_desc_len as u8;

    // Wrap in esds full box (version=0, flags=0)
    let total_size = 12 + es_descriptor.len() as u32; // 8 header + 4 version/flags + data
    let mut box_data = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut box_data, total_size, b"esds", 0, 0);
    box_data.extend_from_slice(&es_descriptor);
    box_data
}

/// Extract video configuration (SPS/PPS) from the first video PES packet.
pub fn extract_video_config(pes: &PesPacket) -> Result<VideoConfig> {
    let nals = extract_h264_nal_units(&pes.data);

    let mut sps: Option<Vec<u8>> = None;
    let mut pps: Option<Vec<u8>> = None;

    for (nal_type, nal_data) in &nals {
        match nal_type {
            7 => {
                if sps.is_none() {
                    sps = Some(nal_data.clone());
                }
            }
            8 => {
                if pps.is_none() {
                    pps = Some(nal_data.clone());
                }
            }
            _ => {}
        }
    }

    let sps_data = sps.ok_or_else(|| {
        EdgepackError::MediaParse("no SPS found in video PES data".to_string())
    })?;
    let pps_data = pps.ok_or_else(|| {
        EdgepackError::MediaParse("no PPS found in video PES data".to_string())
    })?;

    let (width, height, profile_idc, level_idc) = parse_sps(&sps_data);

    let codec_string = format!(
        "avc1.{:02x}{:02x}{:02x}",
        profile_idc,
        if sps_data.len() >= 3 { sps_data[2] } else { 0 },
        level_idc,
    );

    Ok(VideoConfig {
        codec: TsCodec::H264,
        width,
        height,
        sps: sps_data,
        pps: pps_data,
        profile_idc,
        level_idc,
        codec_string,
    })
}

/// Extract audio configuration from the first audio PES packet (ADTS header).
pub fn extract_audio_config(pes: &PesPacket) -> Result<AudioConfig> {
    if pes.data.len() < 7 {
        return Err(EdgepackError::MediaParse(
            "audio PES data too short for ADTS header".to_string(),
        ));
    }

    // ADTS header: syncword(12) + ID(1) + layer(2) + protection(1) + profile(2) +
    //              sampling_freq_index(4) + private(1) + channel_config(3) + ...
    if pes.data[0] != 0xFF || (pes.data[1] & 0xF0) != 0xF0 {
        return Err(EdgepackError::MediaParse(
            "invalid ADTS sync word".to_string(),
        ));
    }

    let profile = ((pes.data[2] >> 6) & 0x03) + 1; // ADTS profile is 0-based, AAC profile is 1-based
    let sampling_freq_index = (pes.data[2] >> 2) & 0x0F;
    let channel_config = ((pes.data[2] & 0x01) << 2) | ((pes.data[3] >> 6) & 0x03);

    let sample_rate = match sampling_freq_index {
        0 => 96000,
        1 => 88200,
        2 => 64000,
        3 => 48000,
        4 => 44100,
        5 => 32000,
        6 => 24000,
        7 => 22050,
        8 => 16000,
        9 => 12000,
        10 => 11025,
        11 => 8000,
        _ => 44100,
    };

    let codec_string = format!("mp4a.40.{}", profile);

    Ok(AudioConfig {
        codec: TsCodec::Aac,
        sample_rate,
        channel_count: channel_config,
        aac_profile: profile,
        codec_string,
    })
}

/// Synthesize a CMAF init segment (ftyp + moov) from extracted configs.
pub fn synthesize_init_segment(
    video: Option<&VideoConfig>,
    audio: Option<&AudioConfig>,
) -> Result<Vec<u8>> {
    if video.is_none() && audio.is_none() {
        return Err(EdgepackError::MediaParse(
            "cannot synthesize init segment without video or audio config".to_string(),
        ));
    }

    let mut output = Vec::new();

    // ftyp box: isom + brands [isom, iso5, dash, cmfc]
    let ftyp_payload = b"isom\x00\x00\x02\x00isomiso5dashcmfc";
    let ftyp_size = 8 + ftyp_payload.len() as u32;
    cmaf::write_box_header(&mut output, ftyp_size, b"ftyp");
    output.extend_from_slice(ftyp_payload);

    // moov box
    let mut moov_children = Vec::new();

    // mvhd (movie header) — version 0
    let mvhd = build_mvhd(90000); // 90kHz timescale (matches TS)
    moov_children.extend_from_slice(&mvhd);

    let mut track_id = 1u32;

    // Video track
    if let Some(video_config) = video {
        let trak = build_video_trak(track_id, video_config);
        moov_children.extend_from_slice(&trak);
        track_id += 1;
    }

    // Audio track
    if let Some(audio_config) = audio {
        let trak = build_audio_trak(track_id, audio_config);
        moov_children.extend_from_slice(&trak);
        track_id += 1;
    }

    // mvex (movie extends) with trex for each track
    let mut mvex_children = Vec::new();
    for tid in 1..track_id {
        let trex = build_trex(tid);
        mvex_children.extend_from_slice(&trex);
    }
    let mvex_size = 8 + mvex_children.len() as u32;
    let mut mvex = Vec::with_capacity(mvex_size as usize);
    cmaf::write_box_header(&mut mvex, mvex_size, b"mvex");
    mvex.extend_from_slice(&mvex_children);
    moov_children.extend_from_slice(&mvex);

    // Wrap in moov
    let moov_size = 8 + moov_children.len() as u32;
    cmaf::write_box_header(&mut output, moov_size, b"moov");
    output.extend_from_slice(&moov_children);

    Ok(output)
}

/// Transmux demuxed PES packets into a CMAF media segment (moof + mdat).
pub fn transmux_to_cmaf(
    segment: &DemuxedSegment,
    video_config: Option<&VideoConfig>,
    audio_config: Option<&AudioConfig>,
    sequence_number: u32,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();

    // Build video moof+mdat
    if video_config.is_some() && !segment.video_packets.is_empty() {
        let (moof, mdat_payload) =
            build_video_fragment(&segment.video_packets, sequence_number, 1)?;
        output.extend_from_slice(&moof);
        let mdat_size = 8 + mdat_payload.len() as u32;
        cmaf::write_box_header(&mut output, mdat_size, b"mdat");
        output.extend_from_slice(&mdat_payload);
    }

    // Build audio moof+mdat (separate fragment for audio track)
    if audio_config.is_some() && !segment.audio_packets.is_empty() {
        let track_id = if video_config.is_some() { 2 } else { 1 };
        let (moof, mdat_payload) =
            build_audio_fragment(&segment.audio_packets, sequence_number, track_id, audio_config.unwrap())?;
        output.extend_from_slice(&moof);
        let mdat_size = 8 + mdat_payload.len() as u32;
        cmaf::write_box_header(&mut output, mdat_size, b"mdat");
        output.extend_from_slice(&mdat_payload);
    }

    if output.is_empty() {
        return Err(EdgepackError::MediaParse(
            "no video or audio data to transmux".to_string(),
        ));
    }

    Ok(output)
}

// ─── Internal helpers ─────────────────────────────────────────────

fn build_mvhd(timescale: u32) -> Vec<u8> {
    // mvhd version 0: 108 bytes total (header 12 + body 96)
    let total_size: u32 = 108;
    let mut mvhd = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut mvhd, total_size, b"mvhd", 0, 0);
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    mvhd.extend_from_slice(&timescale.to_be_bytes()); // timescale
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    mvhd.extend_from_slice(&0x00010000u32.to_be_bytes()); // rate = 1.0
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume = 1.0
    mvhd.extend_from_slice(&[0u8; 10]); // reserved
    // Matrix (identity): 3x3 fixed-point
    let identity_matrix: [u32; 9] = [
        0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000,
    ];
    for &val in &identity_matrix {
        mvhd.extend_from_slice(&val.to_be_bytes());
    }
    mvhd.extend_from_slice(&[0u8; 24]); // pre_defined
    mvhd.extend_from_slice(&0xFFFFFFFFu32.to_be_bytes()); // next_track_ID (placeholder)
    mvhd
}

fn build_video_trak(track_id: u32, config: &VideoConfig) -> Vec<u8> {
    let mut trak_children = Vec::new();

    // tkhd
    let tkhd = build_tkhd(track_id, config.width as u32, config.height as u32, false);
    trak_children.extend_from_slice(&tkhd);

    // mdia
    let mut mdia_children = Vec::new();

    // mdhd
    let mdhd = build_mdhd(90000); // Video timescale = 90kHz (TS standard)
    mdia_children.extend_from_slice(&mdhd);

    // hdlr
    let hdlr = build_hdlr(b"vide", "VideoHandler");
    mdia_children.extend_from_slice(&hdlr);

    // minf
    let mut minf_children = Vec::new();

    // vmhd (video media header)
    let vmhd = build_vmhd();
    minf_children.extend_from_slice(&vmhd);

    // dinf + dref
    let dinf = build_dinf();
    minf_children.extend_from_slice(&dinf);

    // stbl
    let stbl = build_video_stbl(config);
    minf_children.extend_from_slice(&stbl);

    let minf_size = 8 + minf_children.len() as u32;
    let mut minf = Vec::with_capacity(minf_size as usize);
    cmaf::write_box_header(&mut minf, minf_size, b"minf");
    minf.extend_from_slice(&minf_children);
    mdia_children.extend_from_slice(&minf);

    let mdia_size = 8 + mdia_children.len() as u32;
    let mut mdia = Vec::with_capacity(mdia_size as usize);
    cmaf::write_box_header(&mut mdia, mdia_size, b"mdia");
    mdia.extend_from_slice(&mdia_children);
    trak_children.extend_from_slice(&mdia);

    let trak_size = 8 + trak_children.len() as u32;
    let mut trak = Vec::with_capacity(trak_size as usize);
    cmaf::write_box_header(&mut trak, trak_size, b"trak");
    trak.extend_from_slice(&trak_children);
    trak
}

fn build_audio_trak(track_id: u32, config: &AudioConfig) -> Vec<u8> {
    let mut trak_children = Vec::new();

    // tkhd (audio: width=0, height=0, is_audio=true)
    let tkhd = build_tkhd(track_id, 0, 0, true);
    trak_children.extend_from_slice(&tkhd);

    // mdia
    let mut mdia_children = Vec::new();

    let mdhd = build_mdhd(config.sample_rate);
    mdia_children.extend_from_slice(&mdhd);

    let hdlr = build_hdlr(b"soun", "SoundHandler");
    mdia_children.extend_from_slice(&hdlr);

    // minf
    let mut minf_children = Vec::new();

    // smhd (sound media header)
    let smhd = build_smhd();
    minf_children.extend_from_slice(&smhd);

    let dinf = build_dinf();
    minf_children.extend_from_slice(&dinf);

    let stbl = build_audio_stbl(config);
    minf_children.extend_from_slice(&stbl);

    let minf_size = 8 + minf_children.len() as u32;
    let mut minf = Vec::with_capacity(minf_size as usize);
    cmaf::write_box_header(&mut minf, minf_size, b"minf");
    minf.extend_from_slice(&minf_children);
    mdia_children.extend_from_slice(&minf);

    let mdia_size = 8 + mdia_children.len() as u32;
    let mut mdia = Vec::with_capacity(mdia_size as usize);
    cmaf::write_box_header(&mut mdia, mdia_size, b"mdia");
    mdia.extend_from_slice(&mdia_children);
    trak_children.extend_from_slice(&mdia);

    let trak_size = 8 + trak_children.len() as u32;
    let mut trak = Vec::with_capacity(trak_size as usize);
    cmaf::write_box_header(&mut trak, trak_size, b"trak");
    trak.extend_from_slice(&trak_children);
    trak
}

fn build_tkhd(track_id: u32, width: u32, height: u32, is_audio: bool) -> Vec<u8> {
    let total_size: u32 = 92; // 12 header + 80 body
    let flags: u32 = 0x000003; // track_enabled + track_in_movie
    let mut tkhd = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut tkhd, total_size, b"tkhd", 0, flags);
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    tkhd.extend_from_slice(&track_id.to_be_bytes());
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // reserved
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    tkhd.extend_from_slice(&[0u8; 8]); // reserved
    tkhd.extend_from_slice(&0i16.to_be_bytes()); // layer
    tkhd.extend_from_slice(&(if is_audio { 1i16 } else { 0i16 }).to_be_bytes()); // alternate_group
    tkhd.extend_from_slice(&(if is_audio { 0x0100u16 } else { 0u16 }).to_be_bytes()); // volume
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // reserved
    // Identity matrix
    let identity: [u32; 9] = [
        0x00010000, 0, 0, 0, 0x00010000, 0, 0, 0, 0x40000000,
    ];
    for &val in &identity {
        tkhd.extend_from_slice(&val.to_be_bytes());
    }
    // Width and height in 16.16 fixed-point
    tkhd.extend_from_slice(&((width << 16) as u32).to_be_bytes());
    tkhd.extend_from_slice(&((height << 16) as u32).to_be_bytes());
    tkhd
}

fn build_mdhd(timescale: u32) -> Vec<u8> {
    let total_size: u32 = 32; // 12 header + 20 body
    let mut mdhd = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut mdhd, total_size, b"mdhd", 0, 0);
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // creation_time
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // modification_time
    mdhd.extend_from_slice(&timescale.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    // language: "und" packed as 3x5 bits = (u-1)(n-1)(d-1) = (20)(13)(3) = 0x55C4
    mdhd.extend_from_slice(&0x55C4u16.to_be_bytes());
    mdhd.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    mdhd
}

fn build_hdlr(handler_type: &[u8; 4], name: &str) -> Vec<u8> {
    let name_bytes = name.as_bytes();
    let total_size = 33 + name_bytes.len() as u32; // 12 header + 4 pre_defined + 4 handler + 12 reserved + 1 null + name
    let mut hdlr = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut hdlr, total_size, b"hdlr", 0, 0);
    hdlr.extend_from_slice(&0u32.to_be_bytes()); // pre_defined
    hdlr.extend_from_slice(handler_type);
    hdlr.extend_from_slice(&[0u8; 12]); // reserved
    hdlr.extend_from_slice(name_bytes);
    hdlr.push(0x00); // null terminator
    hdlr
}

fn build_vmhd() -> Vec<u8> {
    let total_size: u32 = 20; // 12 header + 8 body
    let mut vmhd = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut vmhd, total_size, b"vmhd", 0, 1);
    vmhd.extend_from_slice(&0u16.to_be_bytes()); // graphicsmode
    vmhd.extend_from_slice(&[0u8; 6]); // opcolor
    vmhd
}

fn build_smhd() -> Vec<u8> {
    let total_size: u32 = 16; // 12 header + 4 body
    let mut smhd = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut smhd, total_size, b"smhd", 0, 0);
    smhd.extend_from_slice(&0i16.to_be_bytes()); // balance
    smhd.extend_from_slice(&0u16.to_be_bytes()); // reserved
    smhd
}

fn build_dinf() -> Vec<u8> {
    // dinf { dref { url (self-contained) } }
    let mut url_box = Vec::new();
    cmaf::write_full_box_header(&mut url_box, 12, b"url ", 0, 1); // flag=1 = self-contained

    let dref_size = 12 + 4 + url_box.len() as u32; // full box header + entry_count + entries
    let mut dref = Vec::with_capacity(dref_size as usize);
    cmaf::write_full_box_header(&mut dref, dref_size, b"dref", 0, 0);
    dref.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    dref.extend_from_slice(&url_box);

    let dinf_size = 8 + dref.len() as u32;
    let mut dinf = Vec::with_capacity(dinf_size as usize);
    cmaf::write_box_header(&mut dinf, dinf_size, b"dinf");
    dinf.extend_from_slice(&dref);
    dinf
}

fn build_video_stbl(config: &VideoConfig) -> Vec<u8> {
    let mut stbl_children = Vec::new();

    // stsd with avc1 sample entry
    let avcc = build_avcc_box(&config.sps, &config.pps, config.profile_idc, config.level_idc);

    // avc1 sample entry: header(8) + reserved(6) + data_ref_index(2) +
    //                    pre_defined(2) + reserved(2) + pre_defined2(12) +
    //                    width(2) + height(2) + horizresolution(4) + vertresolution(4) +
    //                    reserved(4) + frame_count(2) + compressorname(32) +
    //                    depth(2) + pre_defined(2) = 78 bytes before child boxes
    let avc1_prefix_size = 78;
    let avc1_total = avc1_prefix_size + avcc.len() as u32;
    let mut avc1 = Vec::with_capacity(avc1_total as usize);
    cmaf::write_box_header(&mut avc1, avc1_total, b"avc1");
    avc1.extend_from_slice(&[0u8; 6]); // reserved
    avc1.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    avc1.extend_from_slice(&[0u8; 16]); // pre_defined + reserved + pre_defined2
    avc1.extend_from_slice(&config.width.to_be_bytes());
    avc1.extend_from_slice(&config.height.to_be_bytes());
    avc1.extend_from_slice(&0x00480000u32.to_be_bytes()); // horizresolution = 72 dpi
    avc1.extend_from_slice(&0x00480000u32.to_be_bytes()); // vertresolution = 72 dpi
    avc1.extend_from_slice(&0u32.to_be_bytes()); // reserved
    avc1.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    avc1.extend_from_slice(&[0u8; 32]); // compressorname
    avc1.extend_from_slice(&0x0018u16.to_be_bytes()); // depth = 24
    avc1.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined = -1
    avc1.extend_from_slice(&avcc);

    // stsd
    let stsd_size = 16 + avc1.len() as u32;
    let mut stsd = Vec::with_capacity(stsd_size as usize);
    cmaf::write_full_box_header(&mut stsd, stsd_size, b"stsd", 0, 0);
    stsd.extend_from_slice(&1u32.to_be_bytes()); // entry_count
    stsd.extend_from_slice(&avc1);
    stbl_children.extend_from_slice(&stsd);

    // Empty required boxes: stts, stsc, stsz, stco
    stbl_children.extend_from_slice(&build_empty_stts());
    stbl_children.extend_from_slice(&build_empty_stsc());
    stbl_children.extend_from_slice(&build_empty_stsz());
    stbl_children.extend_from_slice(&build_empty_stco());

    let stbl_size = 8 + stbl_children.len() as u32;
    let mut stbl = Vec::with_capacity(stbl_size as usize);
    cmaf::write_box_header(&mut stbl, stbl_size, b"stbl");
    stbl.extend_from_slice(&stbl_children);
    stbl
}

fn build_audio_stbl(config: &AudioConfig) -> Vec<u8> {
    let mut stbl_children = Vec::new();

    // Build esds box
    let esds = build_esds_box(config.aac_profile, config.sample_rate, config.channel_count);

    // mp4a sample entry: header(8) + reserved(6) + data_ref_index(2) +
    //                    reserved2(8) + channel_count(2) + sample_size(2) +
    //                    pre_defined(2) + reserved3(2) + sample_rate(4) = 28 before children
    let mp4a_prefix_size = 36;
    let mp4a_total = mp4a_prefix_size + esds.len() as u32;
    let mut mp4a = Vec::with_capacity(mp4a_total as usize);
    cmaf::write_box_header(&mut mp4a, mp4a_total, b"mp4a");
    mp4a.extend_from_slice(&[0u8; 6]); // reserved
    mp4a.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    mp4a.extend_from_slice(&[0u8; 8]); // reserved
    mp4a.extend_from_slice(&(config.channel_count as u16).to_be_bytes());
    mp4a.extend_from_slice(&16u16.to_be_bytes()); // sample_size = 16 bits
    mp4a.extend_from_slice(&0u16.to_be_bytes()); // pre_defined
    mp4a.extend_from_slice(&0u16.to_be_bytes()); // reserved
    // Sample rate as 16.16 fixed-point
    mp4a.extend_from_slice(&((config.sample_rate as u32) << 16).to_be_bytes());
    mp4a.extend_from_slice(&esds);

    let stsd_size = 16 + mp4a.len() as u32;
    let mut stsd = Vec::with_capacity(stsd_size as usize);
    cmaf::write_full_box_header(&mut stsd, stsd_size, b"stsd", 0, 0);
    stsd.extend_from_slice(&1u32.to_be_bytes());
    stsd.extend_from_slice(&mp4a);
    stbl_children.extend_from_slice(&stsd);

    stbl_children.extend_from_slice(&build_empty_stts());
    stbl_children.extend_from_slice(&build_empty_stsc());
    stbl_children.extend_from_slice(&build_empty_stsz());
    stbl_children.extend_from_slice(&build_empty_stco());

    let stbl_size = 8 + stbl_children.len() as u32;
    let mut stbl = Vec::with_capacity(stbl_size as usize);
    cmaf::write_box_header(&mut stbl, stbl_size, b"stbl");
    stbl.extend_from_slice(&stbl_children);
    stbl
}

fn build_empty_stts() -> Vec<u8> {
    let mut stts = Vec::new();
    cmaf::write_full_box_header(&mut stts, 16, b"stts", 0, 0);
    stts.extend_from_slice(&0u32.to_be_bytes()); // entry_count = 0
    stts
}

fn build_empty_stsc() -> Vec<u8> {
    let mut stsc = Vec::new();
    cmaf::write_full_box_header(&mut stsc, 16, b"stsc", 0, 0);
    stsc.extend_from_slice(&0u32.to_be_bytes());
    stsc
}

fn build_empty_stsz() -> Vec<u8> {
    let mut stsz = Vec::new();
    cmaf::write_full_box_header(&mut stsz, 20, b"stsz", 0, 0);
    stsz.extend_from_slice(&0u32.to_be_bytes()); // sample_size
    stsz.extend_from_slice(&0u32.to_be_bytes()); // sample_count
    stsz
}

fn build_empty_stco() -> Vec<u8> {
    let mut stco = Vec::new();
    cmaf::write_full_box_header(&mut stco, 16, b"stco", 0, 0);
    stco.extend_from_slice(&0u32.to_be_bytes()); // entry_count
    stco
}

fn build_trex(track_id: u32) -> Vec<u8> {
    let total_size: u32 = 32; // 12 header + 20 body
    let mut trex = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut trex, total_size, b"trex", 0, 0);
    trex.extend_from_slice(&track_id.to_be_bytes());
    trex.extend_from_slice(&1u32.to_be_bytes()); // default_sample_description_index
    trex.extend_from_slice(&0u32.to_be_bytes()); // default_sample_duration
    trex.extend_from_slice(&0u32.to_be_bytes()); // default_sample_size
    trex.extend_from_slice(&0u32.to_be_bytes()); // default_sample_flags
    trex
}

fn build_video_fragment(
    packets: &[PesPacket],
    sequence_number: u32,
    track_id: u32,
) -> Result<(Vec<u8>, Vec<u8>)> {
    // Convert each PES packet to AVCC format and build trun entries
    let mut mdat_payload = Vec::new();
    let mut sample_sizes = Vec::new();
    let mut sample_durations = Vec::new();
    let mut sample_flags = Vec::new();

    let base_dts = packets
        .first()
        .and_then(|p| p.dts.or(p.pts))
        .unwrap_or(0);

    for (i, pes) in packets.iter().enumerate() {
        let avcc_data = convert_annexb_to_avcc(&pes.data);
        let size = avcc_data.len() as u32;
        mdat_payload.extend_from_slice(&avcc_data);
        sample_sizes.push(size);

        // Calculate duration from DTS/PTS difference to next sample
        let this_dts = pes.dts.or(pes.pts).unwrap_or(base_dts);
        let next_dts = if i + 1 < packets.len() {
            packets[i + 1].dts.or(packets[i + 1].pts).unwrap_or(this_dts + 3003)
        } else {
            this_dts + 3003 // Default: ~30fps at 90kHz
        };
        sample_durations.push((next_dts.saturating_sub(this_dts)) as u32);

        // Check if this is a key frame (IDR NAL type 5)
        let is_key = has_idr_nal(&pes.data);
        let flags = if is_key {
            0x02000000 // sample_depends_on = 2 (does not depend on others)
        } else {
            0x00010000 // sample_is_non_sync_sample
        };
        sample_flags.push(flags);
    }

    // Build trun with durations, sizes, and flags
    let trun = build_video_trun(&sample_durations, &sample_sizes, &sample_flags);

    // Build tfdt (track fragment decode time)
    let tfdt = build_tfdt(base_dts);

    // Build tfhd
    let tfhd = build_tfhd(track_id);

    // Build traf
    let mut traf_children = Vec::new();
    traf_children.extend_from_slice(&tfhd);
    traf_children.extend_from_slice(&tfdt);
    traf_children.extend_from_slice(&trun);
    let traf_size = 8 + traf_children.len() as u32;
    let mut traf = Vec::with_capacity(traf_size as usize);
    cmaf::write_box_header(&mut traf, traf_size, b"traf");
    traf.extend_from_slice(&traf_children);

    // Build mfhd
    let mfhd = build_mfhd(sequence_number);

    // Build moof
    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof_size = 8 + moof_children.len() as u32;
    let mut moof = Vec::with_capacity(moof_size as usize);
    cmaf::write_box_header(&mut moof, moof_size, b"moof");
    moof.extend_from_slice(&moof_children);

    Ok((moof, mdat_payload))
}

fn build_audio_fragment(
    packets: &[PesPacket],
    sequence_number: u32,
    track_id: u32,
    config: &AudioConfig,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let mut mdat_payload = Vec::new();
    let mut sample_sizes = Vec::new();
    let mut sample_durations = Vec::new();

    // Default sample duration: 1024 samples at the audio sample rate
    let default_duration = 1024u32;

    for pes in packets {
        // Strip ADTS headers from AAC frames
        let frame_data = strip_adts_headers(&pes.data);
        let size = frame_data.len() as u32;
        mdat_payload.extend_from_slice(&frame_data);
        sample_sizes.push(size);
        sample_durations.push(default_duration);
    }

    let trun = build_audio_trun(&sample_durations, &sample_sizes);

    let base_pts = packets.first().and_then(|p| p.pts).unwrap_or(0);
    // Convert from 90kHz to audio timescale
    let base_time = if config.sample_rate > 0 {
        base_pts * config.sample_rate as u64 / 90000
    } else {
        base_pts
    };
    let tfdt = build_tfdt(base_time);
    let tfhd = build_tfhd(track_id);

    let mut traf_children = Vec::new();
    traf_children.extend_from_slice(&tfhd);
    traf_children.extend_from_slice(&tfdt);
    traf_children.extend_from_slice(&trun);
    let traf_size = 8 + traf_children.len() as u32;
    let mut traf = Vec::with_capacity(traf_size as usize);
    cmaf::write_box_header(&mut traf, traf_size, b"traf");
    traf.extend_from_slice(&traf_children);

    let mfhd = build_mfhd(sequence_number);

    let mut moof_children = Vec::new();
    moof_children.extend_from_slice(&mfhd);
    moof_children.extend_from_slice(&traf);
    let moof_size = 8 + moof_children.len() as u32;
    let mut moof = Vec::with_capacity(moof_size as usize);
    cmaf::write_box_header(&mut moof, moof_size, b"moof");
    moof.extend_from_slice(&moof_children);

    Ok((moof, mdat_payload))
}

/// Strip ADTS headers from AAC data, returning raw AAC frames.
fn strip_adts_headers(data: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    let mut pos = 0;

    while pos + 7 <= data.len() {
        // Check for ADTS syncword
        if data[pos] == 0xFF && (data[pos + 1] & 0xF0) == 0xF0 {
            let header_size = if (data[pos + 1] & 0x01) == 0 { 9 } else { 7 };
            let frame_length =
                ((data[pos + 3] as usize & 0x03) << 11)
                    | ((data[pos + 4] as usize) << 3)
                    | ((data[pos + 5] as usize) >> 5);

            if frame_length > header_size && pos + frame_length <= data.len() {
                output.extend_from_slice(&data[pos + header_size..pos + frame_length]);
                pos += frame_length;
                continue;
            }
        }
        // Not an ADTS frame — copy raw
        output.extend_from_slice(&data[pos..]);
        break;
    }

    output
}

fn has_idr_nal(annexb_data: &[u8]) -> bool {
    let nals = extract_h264_nal_units(annexb_data);
    nals.iter().any(|(nal_type, _)| *nal_type == 5)
}

fn build_mfhd(sequence_number: u32) -> Vec<u8> {
    let mut mfhd = Vec::new();
    cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
    mfhd.extend_from_slice(&sequence_number.to_be_bytes());
    mfhd
}

fn build_tfhd(track_id: u32) -> Vec<u8> {
    let mut tfhd = Vec::new();
    cmaf::write_full_box_header(&mut tfhd, 16, b"tfhd", 0, 0x020000); // default-base-is-moof
    tfhd.extend_from_slice(&track_id.to_be_bytes());
    tfhd
}

fn build_tfdt(base_decode_time: u64) -> Vec<u8> {
    let mut tfdt = Vec::new();
    cmaf::write_full_box_header(&mut tfdt, 20, b"tfdt", 1, 0); // version 1 for 64-bit time
    tfdt.extend_from_slice(&base_decode_time.to_be_bytes());
    tfdt
}

fn build_video_trun(durations: &[u32], sizes: &[u32], flags: &[u32]) -> Vec<u8> {
    // flags: 0x000001 (data_offset) + 0x000100 (duration) + 0x000200 (size) + 0x000400 (flags)
    let trun_flags: u32 = 0x000701;
    let total_size = 12 + 4 + 4 + (durations.len() as u32 * 12); // header + sample_count + data_offset + entries
    let mut trun = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut trun, total_size, b"trun", 0, trun_flags);
    trun.extend_from_slice(&(durations.len() as u32).to_be_bytes());
    trun.extend_from_slice(&0u32.to_be_bytes()); // data_offset (placeholder — set by muxer)

    for i in 0..durations.len() {
        trun.extend_from_slice(&durations[i].to_be_bytes());
        trun.extend_from_slice(&sizes[i].to_be_bytes());
        trun.extend_from_slice(&flags[i].to_be_bytes());
    }

    trun
}

fn build_audio_trun(durations: &[u32], sizes: &[u32]) -> Vec<u8> {
    // flags: 0x000001 (data_offset) + 0x000100 (duration) + 0x000200 (size)
    let trun_flags: u32 = 0x000301;
    let total_size = 12 + 4 + 4 + (durations.len() as u32 * 8);
    let mut trun = Vec::with_capacity(total_size as usize);
    cmaf::write_full_box_header(&mut trun, total_size, b"trun", 0, trun_flags);
    trun.extend_from_slice(&(durations.len() as u32).to_be_bytes());
    trun.extend_from_slice(&0u32.to_be_bytes()); // data_offset

    for i in 0..durations.len() {
        trun.extend_from_slice(&durations[i].to_be_bytes());
        trun.extend_from_slice(&sizes[i].to_be_bytes());
    }

    trun
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ts::{DemuxedSegment, PesPacket, TsCodec};

    /// Build a minimal H.264 Annex B stream with SPS + PPS + IDR.
    fn build_test_annexb_stream() -> Vec<u8> {
        let mut data = Vec::new();

        // SPS (NAL type 7): Baseline profile, level 3.1, 1920x1080
        // This is a simplified SPS — just enough for the parser
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]); // Start code
        data.extend_from_slice(&[
            0x67, // NAL type 7 (SPS) + nal_ref_idc
            0x42, // profile_idc = 66 (Baseline)
            0xC0, // constraint_set0_flag=1, others=0
            0x1F, // level_idc = 31
            0xE9, // seq_parameter_set_id=0, log2_max_frame_num=4 (encoded)
            0x40, 0x10, 0x18, 0x60, // pic_order_cnt etc
        ]);

        // PPS (NAL type 8)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x68, 0xCE, 0x38, 0x80]);

        // IDR slice (NAL type 5)
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(&[0x65, 0x88, 0x84, 0x00, 0x33]);

        data
    }

    /// Build a minimal ADTS AAC frame.
    fn build_test_adts_frame() -> Vec<u8> {
        // ADTS header (7 bytes): syncword=FFF, ID=0, layer=00, protection=1, profile=LC(1),
        // freq_index=4(44100), private=0, channel=2, frame_length=10
        let frame_len = 10u16; // 7 header + 3 data bytes
        vec![
            0xFF,
            0xF1, // syncword + ID=0 + layer=00 + protection=1
            0x50, // profile=LC(01), freq_index=0100=44100
            0x80 | ((frame_len >> 11) as u8 & 0x03), // channel_config upper + frame_length upper
            ((frame_len >> 3) & 0xFF) as u8,
            ((frame_len & 0x07) << 5) as u8 | 0x1F, // frame_length lower + buffer fullness
            0xFC, // buffer fullness + number_of_raw_data_blocks=0
            0xDE, 0xAD, 0xBE, // raw AAC data
        ]
    }

    #[test]
    fn extract_h264_nals_basic() {
        let data = build_test_annexb_stream();
        let nals = extract_h264_nal_units(&data);
        assert!(nals.len() >= 3);
        // SPS
        assert_eq!(nals[0].0, 7);
        // PPS
        assert_eq!(nals[1].0, 8);
        // IDR
        assert_eq!(nals[2].0, 5);
    }

    #[test]
    fn extract_h264_nals_empty() {
        let nals = extract_h264_nal_units(&[]);
        assert!(nals.is_empty());
    }

    #[test]
    fn convert_annexb_to_avcc_strips_sps_pps() {
        let data = build_test_annexb_stream();
        let avcc = convert_annexb_to_avcc(&data);
        // AVCC should contain only the IDR slice (length-prefixed)
        assert!(avcc.len() >= 4);
        let nal_len = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]) as usize;
        assert_eq!(nal_len + 4, avcc.len());
        // First NAL byte should be IDR type
        assert_eq!(avcc[4] & 0x1F, 5);
    }

    #[test]
    fn build_avcc_box_structure() {
        let sps = vec![0x67, 0x42, 0xC0, 0x1F, 0xE9];
        let pps = vec![0x68, 0xCE, 0x38, 0x80];
        let avcc = build_avcc_box(&sps, &pps, 0x42, 0x1F);

        // Should be a valid box: size(4) + "avcC"(4) + payload
        assert!(avcc.len() > 8);
        let box_size = u32::from_be_bytes([avcc[0], avcc[1], avcc[2], avcc[3]]) as usize;
        assert_eq!(box_size, avcc.len());
        assert_eq!(&avcc[4..8], b"avcC");
        // Check configurationVersion
        assert_eq!(avcc[8], 1);
        // Check profile
        assert_eq!(avcc[9], 0x42);
        // Check level
        assert_eq!(avcc[11], 0x1F);
    }

    #[test]
    fn build_esds_box_structure() {
        let esds = build_esds_box(2, 44100, 2);
        assert!(esds.len() > 12);
        let box_size = u32::from_be_bytes([esds[0], esds[1], esds[2], esds[3]]) as usize;
        assert_eq!(box_size, esds.len());
        assert_eq!(&esds[4..8], b"esds");
    }

    #[test]
    fn extract_video_config_from_pes() {
        let pes = PesPacket {
            stream_id: 0xE0,
            pts: Some(90000),
            dts: None,
            data: build_test_annexb_stream(),
        };
        let config = extract_video_config(&pes).unwrap();
        assert_eq!(config.codec, TsCodec::H264);
        assert!(!config.sps.is_empty());
        assert!(!config.pps.is_empty());
        assert_eq!(config.profile_idc, 0x42);
        assert_eq!(config.level_idc, 0x1F);
        assert!(config.codec_string.starts_with("avc1."));
    }

    #[test]
    fn extract_video_config_no_sps() {
        let pes = PesPacket {
            stream_id: 0xE0,
            pts: Some(90000),
            dts: None,
            data: vec![0x00, 0x00, 0x00, 0x01, 0x65, 0xAA], // Only IDR, no SPS
        };
        let result = extract_video_config(&pes);
        assert!(result.is_err());
    }

    #[test]
    fn extract_audio_config_from_adts() {
        let pes = PesPacket {
            stream_id: 0xC0,
            pts: Some(90000),
            dts: None,
            data: build_test_adts_frame(),
        };
        let config = extract_audio_config(&pes).unwrap();
        assert_eq!(config.codec, TsCodec::Aac);
        assert_eq!(config.sample_rate, 44100);
        assert_eq!(config.channel_count, 2);
        assert_eq!(config.aac_profile, 2); // AAC-LC
        assert_eq!(config.codec_string, "mp4a.40.2");
    }

    #[test]
    fn extract_audio_config_too_short() {
        let pes = PesPacket {
            stream_id: 0xC0,
            pts: None,
            dts: None,
            data: vec![0xFF, 0xF1, 0x50], // Only 3 bytes, need 7
        };
        let result = extract_audio_config(&pes);
        assert!(result.is_err());
    }

    #[test]
    fn synthesize_init_segment_video_only() {
        let video_config = VideoConfig {
            codec: TsCodec::H264,
            width: 1920,
            height: 1080,
            sps: vec![0x67, 0x42, 0xC0, 0x1F, 0xE9],
            pps: vec![0x68, 0xCE, 0x38, 0x80],
            profile_idc: 0x42,
            level_idc: 0x1F,
            codec_string: "avc1.42c01f".to_string(),
        };

        let init = synthesize_init_segment(Some(&video_config), None).unwrap();
        assert!(!init.is_empty());

        // Verify ftyp box
        let ftyp_size = u32::from_be_bytes([init[0], init[1], init[2], init[3]]) as usize;
        assert_eq!(&init[4..8], b"ftyp");

        // Verify moov box follows
        let moov_offset = ftyp_size;
        assert_eq!(&init[moov_offset + 4..moov_offset + 8], b"moov");
    }

    #[test]
    fn synthesize_init_segment_audio_only() {
        let audio_config = AudioConfig {
            codec: TsCodec::Aac,
            sample_rate: 44100,
            channel_count: 2,
            aac_profile: 2,
            codec_string: "mp4a.40.2".to_string(),
        };

        let init = synthesize_init_segment(None, Some(&audio_config)).unwrap();
        assert!(!init.is_empty());
    }

    #[test]
    fn synthesize_init_segment_no_config_errors() {
        let result = synthesize_init_segment(None, None);
        assert!(result.is_err());
    }

    #[test]
    fn synthesize_init_segment_video_and_audio() {
        let video_config = VideoConfig {
            codec: TsCodec::H264,
            width: 1280,
            height: 720,
            sps: vec![0x67, 0x42, 0xC0, 0x1E],
            pps: vec![0x68, 0xCE, 0x38, 0x80],
            profile_idc: 0x42,
            level_idc: 0x1E,
            codec_string: "avc1.42c01e".to_string(),
        };
        let audio_config = AudioConfig {
            codec: TsCodec::Aac,
            sample_rate: 48000,
            channel_count: 2,
            aac_profile: 2,
            codec_string: "mp4a.40.2".to_string(),
        };

        let init = synthesize_init_segment(Some(&video_config), Some(&audio_config)).unwrap();
        assert!(!init.is_empty());
    }

    #[test]
    fn transmux_video_to_cmaf() {
        let video_config = VideoConfig {
            codec: TsCodec::H264,
            width: 1920,
            height: 1080,
            sps: vec![0x67, 0x42, 0xC0, 0x1F, 0xE9],
            pps: vec![0x68, 0xCE, 0x38, 0x80],
            profile_idc: 0x42,
            level_idc: 0x1F,
            codec_string: "avc1.42c01f".to_string(),
        };

        let segment = DemuxedSegment {
            video_packets: vec![PesPacket {
                stream_id: 0xE0,
                pts: Some(90000),
                dts: Some(90000),
                data: build_test_annexb_stream(),
            }],
            audio_packets: vec![],
            video_codec: Some(TsCodec::H264),
            audio_codec: None,
            pmt: None,
        };

        let result = transmux_to_cmaf(&segment, Some(&video_config), None, 1).unwrap();
        assert!(!result.is_empty());

        // Verify moof box present
        let moof_size = u32::from_be_bytes([result[0], result[1], result[2], result[3]]) as usize;
        assert_eq!(&result[4..8], b"moof");

        // Verify mdat box follows
        assert_eq!(&result[moof_size + 4..moof_size + 8], b"mdat");
    }

    #[test]
    fn transmux_empty_segment_errors() {
        let segment = DemuxedSegment {
            video_packets: vec![],
            audio_packets: vec![],
            video_codec: None,
            audio_codec: None,
            pmt: None,
        };
        let result = transmux_to_cmaf(&segment, None, None, 1);
        assert!(result.is_err());
    }

    #[test]
    fn strip_adts_headers_basic() {
        let frame = build_test_adts_frame();
        let raw = strip_adts_headers(&frame);
        // Should strip 7-byte header, keeping 3 bytes of data
        assert_eq!(raw, vec![0xDE, 0xAD, 0xBE]);
    }

    #[test]
    fn strip_adts_headers_empty() {
        let raw = strip_adts_headers(&[]);
        assert!(raw.is_empty());
    }

    #[test]
    fn has_idr_nal_true() {
        let data = build_test_annexb_stream();
        assert!(has_idr_nal(&data));
    }

    #[test]
    fn has_idr_nal_false() {
        // Only SPS + PPS, no IDR
        let mut data = Vec::new();
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x67, 0x42]); // SPS
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x68, 0xCE]); // PPS
        assert!(!has_idr_nal(&data));
    }
}
