//! CMAF-to-MPEG-TS muxer for producing TS segment output.
//!
//! Converts CMAF moof/mdat segments into MPEG-TS packets with embedded PAT/PMT.
//! Feature-gated behind `#[cfg(feature = "ts")]`.

use crate::error::{EdgepackError, Result};
use crate::media::cmaf::{self, find_child_box, iterate_boxes};
use crate::media::codec::extract_tracks;
use crate::media::ts::TsCodec;
use crate::media::{box_type, TrackType};

const TS_PACKET_SIZE: usize = 188;
const TS_SYNC_BYTE: u8 = 0x47;
#[cfg(test)]
const PAT_PID: u16 = 0x0000;
const PMT_PID: u16 = 0x1000;
const VIDEO_PID: u16 = 0x0100;
const AUDIO_PID: u16 = 0x0101;
const VIDEO_STREAM_ID: u8 = 0xE0;
const AUDIO_STREAM_ID: u8 = 0xC0;

/// Configuration extracted from a CMAF init segment for TS muxing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TsMuxConfig {
    pub video_codec: Option<TsCodec>,
    pub audio_codec: Option<TsCodec>,
    /// Raw SPS bytes (H.264, without start code).
    pub sps: Vec<u8>,
    /// Raw PPS bytes (H.264, without start code).
    pub pps: Vec<u8>,
    pub aac_profile: u8,
    pub aac_sample_rate_index: u8,
    pub aac_channel_count: u8,
    pub video_timescale: u32,
    pub audio_timescale: u32,
}

/// Extract TS mux configuration from a CMAF init segment.
///
/// Parses the moov box to find SPS/PPS from avcC and AAC config from esds.
pub fn extract_mux_config(init_data: &[u8]) -> Result<TsMuxConfig> {
    let tracks = extract_tracks(init_data)?;

    let mut config = TsMuxConfig {
        video_codec: None,
        audio_codec: None,
        sps: Vec::new(),
        pps: Vec::new(),
        aac_profile: 2, // AAC-LC default
        aac_sample_rate_index: 3, // 48000 default
        aac_channel_count: 2,
        video_timescale: 90000,
        audio_timescale: 48000,
    };

    for track in &tracks {
        match track.track_type {
            TrackType::Video => {
                if track.codec_string.starts_with("avc1") || track.codec_string.starts_with("avc3") {
                    config.video_codec = Some(TsCodec::H264);
                } else if track.codec_string.starts_with("hev1") || track.codec_string.starts_with("hvc1") {
                    config.video_codec = Some(TsCodec::H265);
                }
                config.video_timescale = track.timescale;
            }
            TrackType::Audio => {
                if track.codec_string.starts_with("mp4a") {
                    config.audio_codec = Some(TsCodec::Aac);
                }
                config.audio_timescale = track.timescale;
            }
            _ => {}
        }
    }

    // Parse avcC from init segment to extract SPS/PPS
    if config.video_codec == Some(TsCodec::H264) {
        if let Some((sps, pps)) = parse_avcc_from_init(init_data) {
            config.sps = sps;
            config.pps = pps;
        }
    }

    // Parse esds from init segment to extract AAC config
    if config.audio_codec == Some(TsCodec::Aac) {
        if let Some((profile, sr_index, channels)) = parse_esds_from_init(init_data) {
            config.aac_profile = profile;
            config.aac_sample_rate_index = sr_index;
            config.aac_channel_count = channels;
        }
    }

    Ok(config)
}

/// Mux a CMAF segment (moof+mdat) to MPEG-TS.
///
/// Parses the CMAF segment, extracts samples, converts video AVCC→Annex B
/// and audio raw AAC→ADTS, then packetizes into 188-byte TS packets.
pub fn mux_to_ts(
    cmaf_segment: &[u8],
    config: &TsMuxConfig,
    _segment_number: u32,
) -> Result<Vec<u8>> {
    // Find moof and mdat
    let mut moof_data: Option<(&[u8], usize)> = None;
    let mut mdat_payload_offset: Option<usize> = None;
    let mut mdat_payload_len: Option<usize> = None;

    for box_result in iterate_boxes(cmaf_segment) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        if header.box_type == box_type::MOOF {
            let payload_start = header.payload_offset() as usize;
            moof_data = Some((&cmaf_segment[payload_start..box_end.min(cmaf_segment.len())], header.offset as usize));
        }
        if header.box_type == box_type::MDAT {
            mdat_payload_offset = Some(header.payload_offset() as usize);
            mdat_payload_len = Some(header.size as usize - header.header_size as usize);
        }
    }

    let moof_payload = moof_data
        .ok_or_else(|| EdgepackError::MediaParse("no moof box found in CMAF segment".into()))?;
    let mdat_offset = mdat_payload_offset
        .ok_or_else(|| EdgepackError::MediaParse("no mdat box found in CMAF segment".into()))?;
    let mdat_len = mdat_payload_len.unwrap_or(0);

    let mdat_payload = &cmaf_segment[mdat_offset..mdat_offset + mdat_len.min(cmaf_segment.len() - mdat_offset)];

    // Find traf → trun inside moof
    let traf_header = find_child_box(moof_payload.0, &box_type::TRAF)
        .ok_or_else(|| EdgepackError::MediaParse("no traf box in moof".into()))?;
    let traf_payload = &moof_payload.0[traf_header.payload_offset() as usize
        ..(traf_header.offset + traf_header.size) as usize];

    let trun_header = find_child_box(traf_payload, &box_type::TRUN)
        .ok_or_else(|| EdgepackError::MediaParse("no trun box in traf".into()))?;
    let trun_payload = &traf_payload[trun_header.payload_offset() as usize
        ..(trun_header.offset + trun_header.size) as usize];

    let trun = cmaf::parse_trun(trun_payload)?;

    // Parse tfdt for base decode time
    let base_decode_time = if let Some(tfdt_header) = find_child_box(traf_payload, &box_type::TFDT) {
        let tfdt_payload = &traf_payload[tfdt_header.payload_offset() as usize
            ..(tfdt_header.offset + tfdt_header.size) as usize];
        parse_tfdt(tfdt_payload)
    } else {
        0u64
    };

    // Determine which track this is (video or audio) by checking hdlr in init
    // For now, use heuristic: if we have video config, try video first
    let is_video = config.video_codec.is_some();
    let timescale = if is_video {
        config.video_timescale
    } else {
        config.audio_timescale
    };

    // Extract raw samples from mdat using trun entries
    let mut samples = Vec::new();
    let mut mdat_pos = 0usize;
    let mut current_dts = base_decode_time;

    for entry in &trun.entries {
        let sample_size = entry.sample_size.unwrap_or(0) as usize;
        if mdat_pos + sample_size > mdat_payload.len() {
            break;
        }
        let sample_data = &mdat_payload[mdat_pos..mdat_pos + sample_size];
        let cto = entry.sample_composition_time_offset.unwrap_or(0);
        let pts = if cto >= 0 {
            current_dts + cto as u64
        } else {
            current_dts.saturating_sub((-cto) as u64)
        };

        let is_idr = is_sync_sample(entry, &trun);

        samples.push(SampleInfo {
            data: sample_data,
            dts: current_dts,
            pts,
            duration: entry.sample_duration.unwrap_or(0),
            is_idr,
        });

        current_dts += entry.sample_duration.unwrap_or(0) as u64;
        mdat_pos += sample_size;
    }

    // Build TS output
    let mut output = Vec::with_capacity(cmaf_segment.len() * 2);
    let mut video_cc: u8 = 0;
    let mut audio_cc: u8 = 0;
    let pat_cc: u8 = 0;
    let pmt_cc: u8 = 0;

    // Emit PAT + PMT at start of segment
    output.extend_from_slice(&build_pat_packet(pat_cc));
    output.extend_from_slice(&build_pmt_packet(
        config.video_codec.unwrap_or(TsCodec::H264),
        config.audio_codec.unwrap_or(TsCodec::Aac),
        pmt_cc,
    ));

    // Convert and packetize each sample
    for sample in &samples {
        if is_video {
            // Convert AVCC → Annex B
            let annexb = convert_avcc_to_annexb(sample.data, &config.sps, &config.pps, sample.is_idr);

            // Convert timescale to 90kHz for PTS/DTS
            let pts_90k = rescale_time(sample.pts, timescale, 90000);
            let dts_90k = rescale_time(sample.dts, timescale, 90000);

            let dts_opt = if pts_90k != dts_90k { Some(dts_90k) } else { None };

            let pes = build_pes_packet(VIDEO_STREAM_ID, pts_90k, dts_opt, &annexb);
            let packets = packetize_pes(VIDEO_PID, &pes, true, &mut video_cc, sample.is_idr);
            for pkt in &packets {
                output.extend_from_slice(pkt);
            }
        } else {
            // Add ADTS header to raw AAC frame
            let adts_header = build_adts_header(
                config.aac_profile,
                config.aac_sample_rate_index,
                config.aac_channel_count,
                sample.data.len(),
            );
            let mut adts_frame = Vec::with_capacity(7 + sample.data.len());
            adts_frame.extend_from_slice(&adts_header);
            adts_frame.extend_from_slice(sample.data);

            let pts_90k = rescale_time(sample.pts, timescale, 90000);

            let pes = build_pes_packet(AUDIO_STREAM_ID, pts_90k, None, &adts_frame);
            let packets = packetize_pes(AUDIO_PID, &pes, true, &mut audio_cc, false);
            for pkt in &packets {
                output.extend_from_slice(pkt);
            }
        }
    }

    Ok(output)
}

/// Convert AVCC (4-byte length-prefixed) video data to Annex B (start code-prefixed).
///
/// Reverses `convert_annexb_to_avcc()` in transmux.rs.
/// If `is_idr`, prepends SPS and PPS before the IDR NAL.
pub fn convert_avcc_to_annexb(avcc_data: &[u8], sps: &[u8], pps: &[u8], is_idr: bool) -> Vec<u8> {
    let mut output = Vec::with_capacity(avcc_data.len() + 128);
    let start_code: &[u8] = &[0x00, 0x00, 0x00, 0x01];

    // If IDR, prepend SPS and PPS
    if is_idr && !sps.is_empty() && !pps.is_empty() {
        output.extend_from_slice(start_code);
        output.extend_from_slice(sps);
        output.extend_from_slice(start_code);
        output.extend_from_slice(pps);
    }

    // Parse AVCC NAL units (4-byte length prefix)
    let mut pos = 0;
    while pos + 4 <= avcc_data.len() {
        let nal_len = u32::from_be_bytes([
            avcc_data[pos],
            avcc_data[pos + 1],
            avcc_data[pos + 2],
            avcc_data[pos + 3],
        ]) as usize;
        pos += 4;

        if pos + nal_len > avcc_data.len() {
            break;
        }

        output.extend_from_slice(start_code);
        output.extend_from_slice(&avcc_data[pos..pos + nal_len]);
        pos += nal_len;
    }

    output
}

/// Build a 7-byte ADTS header for an AAC frame.
///
/// Reverses `strip_adts_headers()` in transmux.rs.
pub fn build_adts_header(
    profile: u8,
    sample_rate_index: u8,
    channel_count: u8,
    aac_frame_len: usize,
) -> [u8; 7] {
    let frame_length = (aac_frame_len + 7) as u16; // Include header size
    let profile_minus1 = profile.saturating_sub(1); // ADTS uses profile - 1

    let mut header = [0u8; 7];
    // Syncword (12 bits) = 0xFFF
    header[0] = 0xFF;
    // Syncword (4 bits) + ID (1 bit, 0=MPEG-4) + Layer (2 bits, 0) + Protection absent (1 bit, 1)
    header[1] = 0xF1;
    // Profile (2 bits) + Sampling frequency index (4 bits) + Private (1 bit) + Channel config high (1 bit)
    header[2] = (profile_minus1 << 6) | (sample_rate_index << 2) | ((channel_count >> 2) & 0x01);
    // Channel config low (2 bits) + Original/copy (1 bit) + Home (1 bit) + Copyright ID (1 bit) +
    // Copyright start (1 bit) + Frame length high (2 bits)
    header[3] = ((channel_count & 0x03) << 6) | ((frame_length >> 11) as u8 & 0x03);
    // Frame length middle (8 bits)
    header[4] = ((frame_length >> 3) & 0xFF) as u8;
    // Frame length low (3 bits) + Buffer fullness high (5 bits)
    header[5] = ((frame_length & 0x07) as u8) << 5 | 0x1F; // buffer fullness = 0x7FF (variable bitrate)
    // Buffer fullness low (6 bits) + Number of AAC frames minus 1 (2 bits)
    header[6] = 0xFC; // buffer fullness low bits = 0x3F << 2, num_frames = 0

    header
}

/// Build a PAT (Program Association Table) TS packet.
pub fn build_pat_packet(cc: u8) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40; // PUSI = 1, PID high = 0
    pkt[2] = 0x00; // PID low = 0 (PAT)
    pkt[3] = 0x10 | (cc & 0x0F); // payload only

    let mut pos = 4;
    pkt[pos] = 0x00; // pointer_field
    pos += 1;

    // PAT section
    pkt[pos] = 0x00; // table_id
    pos += 1;

    let section_length: u16 = 5 + 4 + 4; // header_after_length + 1 program + CRC
    pkt[pos] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
    pos += 1;
    pkt[pos] = (section_length & 0xFF) as u8;
    pos += 1;

    // transport_stream_id
    pkt[pos] = 0x00;
    pkt[pos + 1] = 0x01;
    pos += 2;

    pkt[pos] = 0xC1; // version=0, current=1
    pos += 1;
    pkt[pos] = 0x00; // section_number
    pos += 1;
    pkt[pos] = 0x00; // last_section_number
    pos += 1;

    // Program 1 → PMT_PID
    pkt[pos] = 0x00;
    pkt[pos + 1] = 0x01; // program_number = 1
    pos += 2;
    pkt[pos] = 0xE0 | ((PMT_PID >> 8) as u8 & 0x1F);
    pkt[pos + 1] = (PMT_PID & 0xFF) as u8;
    pos += 2;

    // CRC32 placeholder (not verified by our parser)
    pkt[pos] = 0x00;
    pkt[pos + 1] = 0x00;
    pkt[pos + 2] = 0x00;
    pkt[pos + 3] = 0x00;

    pkt
}

/// Build a PMT (Program Map Table) TS packet.
pub fn build_pmt_packet(video_codec: TsCodec, audio_codec: TsCodec, cc: u8) -> [u8; TS_PACKET_SIZE] {
    let mut pkt = [0xFFu8; TS_PACKET_SIZE];
    pkt[0] = TS_SYNC_BYTE;
    pkt[1] = 0x40 | ((PMT_PID >> 8) as u8 & 0x1F); // PUSI = 1
    pkt[2] = (PMT_PID & 0xFF) as u8;
    pkt[3] = 0x10 | (cc & 0x0F); // payload only

    let mut pos = 4;
    pkt[pos] = 0x00; // pointer_field
    pos += 1;

    pkt[pos] = 0x02; // table_id (PMT)
    pos += 1;

    // section_length = 5 (header) + 4 (PCR+prog_info) + 5 (video) + 5 (audio) + 4 (CRC)
    let section_length: u16 = 5 + 4 + 5 + 5 + 4;
    pkt[pos] = 0xB0 | ((section_length >> 8) as u8 & 0x0F);
    pos += 1;
    pkt[pos] = (section_length & 0xFF) as u8;
    pos += 1;

    // program_number
    pkt[pos] = 0x00;
    pkt[pos + 1] = 0x01;
    pos += 2;

    pkt[pos] = 0xC1; // version=0, current=1
    pos += 1;
    pkt[pos] = 0x00; // section_number
    pos += 1;
    pkt[pos] = 0x00; // last_section_number
    pos += 1;

    // PCR PID = VIDEO_PID
    pkt[pos] = 0xE0 | ((VIDEO_PID >> 8) as u8 & 0x1F);
    pkt[pos + 1] = (VIDEO_PID & 0xFF) as u8;
    pos += 2;

    // program_info_length = 0
    pkt[pos] = 0xF0;
    pkt[pos + 1] = 0x00;
    pos += 2;

    // Video stream
    let video_stream_type = match video_codec {
        TsCodec::H264 => 0x1B,
        TsCodec::H265 => 0x24,
        _ => 0x1B,
    };
    pkt[pos] = video_stream_type;
    pos += 1;
    pkt[pos] = 0xE0 | ((VIDEO_PID >> 8) as u8 & 0x1F);
    pkt[pos + 1] = (VIDEO_PID & 0xFF) as u8;
    pos += 2;
    pkt[pos] = 0xF0;
    pkt[pos + 1] = 0x00; // ES_info_length = 0
    pos += 2;

    // Audio stream
    let audio_stream_type = match audio_codec {
        TsCodec::Aac => 0x0F,
        TsCodec::Ac3 => 0x81,
        _ => 0x0F,
    };
    pkt[pos] = audio_stream_type;
    pos += 1;
    pkt[pos] = 0xE0 | ((AUDIO_PID >> 8) as u8 & 0x1F);
    pkt[pos + 1] = (AUDIO_PID & 0xFF) as u8;
    pos += 2;
    pkt[pos] = 0xF0;
    pkt[pos + 1] = 0x00; // ES_info_length = 0
    pos += 2;

    // CRC32 placeholder
    pkt[pos] = 0x00;
    pkt[pos + 1] = 0x00;
    pkt[pos + 2] = 0x00;
    pkt[pos + 3] = 0x00;

    pkt
}

/// Build a PES (Packetized Elementary Stream) packet.
pub fn build_pes_packet(stream_id: u8, pts: u64, dts: Option<u64>, data: &[u8]) -> Vec<u8> {
    let has_dts = dts.is_some();
    let pts_dts_len = if has_dts { 10 } else { 5 };
    let pes_header_data_length = pts_dts_len;

    let pes_data_len = 3 + pes_header_data_length + data.len();
    let pes_packet_length = if stream_id >= 0xE0 && stream_id <= 0xEF {
        0u16 // Video: unbounded
    } else {
        pes_data_len.min(0xFFFF) as u16
    };

    let mut pes = Vec::with_capacity(9 + pes_header_data_length + data.len());

    // PES start code
    pes.extend_from_slice(&[0x00, 0x00, 0x01]);
    pes.push(stream_id);
    pes.extend_from_slice(&pes_packet_length.to_be_bytes());

    // Optional PES header
    pes.push(0x80); // marker bits (10), no scrambling, no priority, no alignment, no copyright, no original
    let pts_dts_flags: u8 = if has_dts { 0xC0 } else { 0x80 }; // 11 = both, 10 = PTS only
    pes.push(pts_dts_flags);
    pes.push(pes_header_data_length as u8);

    // Encode PTS
    let pts_marker = if has_dts { 0x31 } else { 0x21 }; // '0011' or '0010' prefix
    pes.extend_from_slice(&encode_timestamp(pts, pts_marker));

    // Encode DTS if present
    if let Some(dts_val) = dts {
        pes.extend_from_slice(&encode_timestamp(dts_val, 0x11)); // '0001' prefix
    }

    // Elementary stream data
    pes.extend_from_slice(data);

    pes
}

/// Packetize PES data into 188-byte TS packets.
pub fn packetize_pes(
    pid: u16,
    pes_data: &[u8],
    _is_start: bool,
    cc: &mut u8,
    random_access: bool,
) -> Vec<[u8; TS_PACKET_SIZE]> {
    let mut packets = Vec::new();
    let mut pos = 0;
    let mut first = true;

    while pos < pes_data.len() {
        let mut pkt = [0xFFu8; TS_PACKET_SIZE];
        pkt[0] = TS_SYNC_BYTE;

        let pusi = first;
        pkt[1] = ((pid >> 8) as u8 & 0x1F) | if pusi { 0x40 } else { 0x00 };
        pkt[2] = (pid & 0xFF) as u8;

        let mut header_size = 4;

        // Add adaptation field for first packet if random access (IDR)
        if first && random_access {
            // adaptation_field_control = 11 (both AF and payload)
            pkt[3] = 0x30 | (*cc & 0x0F);

            // Adaptation field: length=1, random_access_indicator=1
            pkt[4] = 0x01; // AF length
            pkt[5] = 0x40; // RAI flag
            header_size = 6;
        } else {
            // adaptation_field_control = 01 (payload only)
            pkt[3] = 0x10 | (*cc & 0x0F);
        }

        let available = TS_PACKET_SIZE - header_size;
        let remaining = pes_data.len() - pos;
        let to_copy = remaining.min(available);

        if to_copy < available {
            // Need stuffing — use adaptation field padding
            let stuff_bytes = available - to_copy;
            if first && random_access {
                // Already have AF, extend it
                let new_af_len = 1 + stuff_bytes;
                pkt[3] = 0x30 | (*cc & 0x0F); // AF + payload
                pkt[4] = new_af_len as u8;
                pkt[5] = 0x40; // RAI flag
                // Fill stuffing bytes with 0xFF
                for i in 0..stuff_bytes {
                    pkt[6 + i] = 0xFF;
                }
                let payload_start = 4 + 1 + new_af_len;
                pkt[payload_start..payload_start + to_copy]
                    .copy_from_slice(&pes_data[pos..pos + to_copy]);
            } else if stuff_bytes == 1 {
                // Minimal AF with length=0
                pkt[3] = 0x30 | (*cc & 0x0F);
                pkt[4] = 0x00; // AF length = 0
                let payload_start = 5;
                pkt[payload_start..payload_start + to_copy]
                    .copy_from_slice(&pes_data[pos..pos + to_copy]);
            } else if stuff_bytes >= 2 {
                // AF with stuffing
                let af_len = stuff_bytes - 1; // -1 for the AF length byte itself
                pkt[3] = 0x30 | (*cc & 0x0F);
                pkt[4] = af_len as u8;
                if af_len > 0 {
                    pkt[5] = 0x00; // No flags
                    for i in 1..af_len {
                        pkt[5 + i] = 0xFF;
                    }
                }
                let payload_start = 4 + 1 + af_len;
                pkt[payload_start..payload_start + to_copy]
                    .copy_from_slice(&pes_data[pos..pos + to_copy]);
            } else {
                // No stuffing needed (stuff_bytes == 0, shouldn't happen due to min check)
                pkt[header_size..header_size + to_copy]
                    .copy_from_slice(&pes_data[pos..pos + to_copy]);
            }
        } else {
            // Full payload, no stuffing
            pkt[header_size..header_size + to_copy]
                .copy_from_slice(&pes_data[pos..pos + to_copy]);
        }

        pos += to_copy;
        *cc = (*cc + 1) & 0x0F;
        first = false;
        packets.push(pkt);
    }

    packets
}

/// Encrypt a TS segment using AES-128-CBC with PKCS7 padding.
///
/// Reverses `decrypt_ts_segment()` in ts.rs.
pub fn encrypt_ts_segment(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    use aes::Aes128;
    use cbc::cipher::{BlockEncryptMut, KeyIvInit};

    type Aes128CbcEnc = cbc::Encryptor<Aes128>;

    if data.is_empty() {
        return Ok(Vec::new());
    }

    // Add PKCS7 padding
    let pad_len = 16 - (data.len() % 16);
    let mut to_encrypt = Vec::with_capacity(data.len() + pad_len);
    to_encrypt.extend_from_slice(data);
    to_encrypt.extend(vec![pad_len as u8; pad_len]);

    let len = to_encrypt.len();
    let encryptor = Aes128CbcEnc::new(key.into(), iv.into());
    encryptor
        .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut to_encrypt, len)
        .map_err(|e| EdgepackError::Encryption(format!("AES-128-CBC encryption failed: {e}")))?;

    Ok(to_encrypt)
}

// --- Internal helpers ---

struct SampleInfo<'a> {
    data: &'a [u8],
    dts: u64,
    pts: u64,
    #[allow(dead_code)]
    duration: u32,
    is_idr: bool,
}

/// Encode a PES timestamp into 5 bytes.
fn encode_timestamp(ts: u64, marker_bits: u8) -> [u8; 5] {
    let mut bytes = [0u8; 5];
    bytes[0] = marker_bits | (((ts >> 30) as u8 & 0x07) << 1) | 0x01;
    bytes[1] = ((ts >> 22) & 0xFF) as u8;
    bytes[2] = (((ts >> 15) & 0x7F) as u8) << 1 | 0x01;
    bytes[3] = ((ts >> 7) & 0xFF) as u8;
    bytes[4] = ((ts & 0x7F) as u8) << 1 | 0x01;
    bytes
}

/// Rescale a timestamp from one timescale to another.
fn rescale_time(time: u64, from_timescale: u32, to_timescale: u32) -> u64 {
    if from_timescale == to_timescale || from_timescale == 0 {
        return time;
    }
    (time as u128 * to_timescale as u128 / from_timescale as u128) as u64
}

/// Check if a trun entry represents a sync/IDR sample.
fn is_sync_sample(entry: &cmaf::TrunEntry, trun: &cmaf::TrackRunBox) -> bool {
    // Check per-sample flags
    if let Some(flags) = entry.sample_flags {
        // sample_depends_on == 2 means "does not depend on others" (IDR)
        let depends_on = (flags >> 24) & 0x03;
        if depends_on == 2 {
            return true;
        }
        // Also check is_non_sync_sample flag (bit 16)
        let is_non_sync = (flags >> 16) & 0x01;
        return is_non_sync == 0;
    }

    // Check first_sample_flags for first entry
    if let Some(first_flags) = trun.first_sample_flags {
        if std::ptr::eq(entry, &trun.entries[0]) {
            let depends_on = (first_flags >> 24) & 0x03;
            if depends_on == 2 {
                return true;
            }
            let is_non_sync = (first_flags >> 16) & 0x01;
            return is_non_sync == 0;
        }
    }

    false
}

/// Parse avcC box from init segment to extract raw SPS and PPS.
fn parse_avcc_from_init(init_data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    // Navigate: moov → trak → mdia → minf → stbl → stsd → avc1/encv → avcC
    let moov = find_child_box(init_data, &box_type::MOOV)?;
    let moov_payload = &init_data[moov.payload_offset() as usize..(moov.offset + moov.size) as usize];

    for trak_result in iterate_boxes(moov_payload) {
        let trak = trak_result.ok()?;
        if trak.box_type != box_type::TRAK {
            continue;
        }
        let trak_payload = &moov_payload[trak.payload_offset() as usize..(trak.offset + trak.size) as usize];

        let mdia = find_child_box(trak_payload, &box_type::MDIA)?;
        let mdia_payload = &trak_payload[mdia.payload_offset() as usize..(mdia.offset + mdia.size) as usize];

        // Check if this is a video track via hdlr
        if let Some(hdlr) = find_child_box(mdia_payload, &box_type::HDLR) {
            let hdlr_payload = &mdia_payload[hdlr.payload_offset() as usize..(hdlr.offset + hdlr.size) as usize];
            if hdlr_payload.len() >= 12 {
                let handler = &hdlr_payload[8..12];
                if handler != b"vide" {
                    continue;
                }
            }
        }

        let minf = find_child_box(mdia_payload, &box_type::MINF)?;
        let minf_payload = &mdia_payload[minf.payload_offset() as usize..(minf.offset + minf.size) as usize];

        let stbl = find_child_box(minf_payload, &box_type::STBL)?;
        let stbl_payload = &minf_payload[stbl.payload_offset() as usize..(stbl.offset + stbl.size) as usize];

        let stsd = find_child_box(stbl_payload, &box_type::STSD)?;
        let stsd_payload = &stbl_payload[stsd.payload_offset() as usize..(stsd.offset + stsd.size) as usize];

        // stsd is a full box: version(1) + flags(3) + entry_count(4) = 8 bytes
        if stsd_payload.len() < 8 {
            continue;
        }
        let entry_data = &stsd_payload[8..];

        // Find avcC in the sample entry (which is nested inside avc1/encv)
        return find_avcc_in_entry(entry_data);
    }

    None
}

/// Find avcC box within a sample entry and parse SPS/PPS from it.
fn find_avcc_in_entry(entry_data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    // The sample entry has a header (8 bytes), then sample entry fields,
    // then child boxes. For video, the sample entry is at least 78 bytes
    // (8 header + 6 reserved + 2 data_ref_index + 62 visual fields).
    // We search for the avcC fourcc in the data.
    for box_result in iterate_boxes(entry_data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        if box_end > entry_data.len() {
            continue;
        }

        let box_payload_start = header.payload_offset() as usize;
        let box_payload = &entry_data[box_payload_start..box_end];

        // Check if this is avc1/encv — dive into it
        if header.box_type == *b"avc1" || header.box_type == *b"avc3"
            || header.box_type == *b"encv"
        {
            // Skip 78 bytes of visual sample entry, then iterate child boxes
            if box_payload.len() > 70 {
                let children = &box_payload[70..]; // After visual sample entry fields
                if let Some(result) = find_avcc_in_children(children) {
                    return Some(result);
                }
            }
        }

        // Direct avcC box
        if header.box_type == *b"avcC" {
            return parse_avcc_record(box_payload);
        }
    }

    None
}

fn find_avcc_in_children(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    for box_result in iterate_boxes(data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        if box_end > data.len() {
            continue;
        }
        let box_payload = &data[header.payload_offset() as usize..box_end];

        if header.box_type == *b"avcC" {
            return parse_avcc_record(box_payload);
        }

        // Check inside sinf for encrypted content
        if header.box_type == box_type::SINF {
            if let Some(result) = find_avcc_in_children(box_payload) {
                return Some(result);
            }
        }
    }
    None
}

/// Parse an avcC (AVC Decoder Configuration Record) to extract SPS and PPS.
fn parse_avcc_record(data: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    // avcC record layout:
    // configurationVersion(1) + AVCProfileIndication(1) + profile_compatibility(1) +
    // AVCLevelIndication(1) + lengthSizeMinusOne(1) + numSPS(1) + spsLength(2) + sps...
    // + numPPS(1) + ppsLength(2) + pps...
    if data.len() < 8 {
        return None;
    }

    let num_sps = data[5] & 0x1F;
    if num_sps == 0 {
        return None;
    }

    let mut pos = 6;
    // Read first SPS
    if pos + 2 > data.len() {
        return None;
    }
    let sps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;
    if pos + sps_len > data.len() {
        return None;
    }
    let sps = data[pos..pos + sps_len].to_vec();
    pos += sps_len;

    // Skip remaining SPS (if any)
    for _ in 1..num_sps {
        if pos + 2 > data.len() {
            return Some((sps.clone(), Vec::new()));
        }
        let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
        pos += 2 + len;
    }

    // Read first PPS
    if pos >= data.len() {
        return Some((sps, Vec::new()));
    }
    let num_pps = data[pos];
    pos += 1;
    if num_pps == 0 || pos + 2 > data.len() {
        return Some((sps, Vec::new()));
    }
    let pps_len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
    pos += 2;
    if pos + pps_len > data.len() {
        return Some((sps, Vec::new()));
    }
    let pps = data[pos..pos + pps_len].to_vec();

    Some((sps, pps))
}

/// Parse esds box from init segment to extract AAC profile, sample rate index, and channels.
fn parse_esds_from_init(init_data: &[u8]) -> Option<(u8, u8, u8)> {
    // Navigate: moov → trak → mdia → minf → stbl → stsd → mp4a/enca → esds
    let moov = find_child_box(init_data, &box_type::MOOV)?;
    let moov_payload = &init_data[moov.payload_offset() as usize..(moov.offset + moov.size) as usize];

    for trak_result in iterate_boxes(moov_payload) {
        let trak = trak_result.ok()?;
        if trak.box_type != box_type::TRAK {
            continue;
        }
        let trak_payload = &moov_payload[trak.payload_offset() as usize..(trak.offset + trak.size) as usize];

        let mdia = find_child_box(trak_payload, &box_type::MDIA)?;
        let mdia_payload = &trak_payload[mdia.payload_offset() as usize..(mdia.offset + mdia.size) as usize];

        // Check if this is an audio track via hdlr
        if let Some(hdlr) = find_child_box(mdia_payload, &box_type::HDLR) {
            let hdlr_payload = &mdia_payload[hdlr.payload_offset() as usize..(hdlr.offset + hdlr.size) as usize];
            if hdlr_payload.len() >= 12 {
                let handler = &hdlr_payload[8..12];
                if handler != b"soun" {
                    continue;
                }
            }
        }

        let minf = find_child_box(mdia_payload, &box_type::MINF)?;
        let minf_payload = &mdia_payload[minf.payload_offset() as usize..(minf.offset + minf.size) as usize];

        let stbl = find_child_box(minf_payload, &box_type::STBL)?;
        let stbl_payload = &minf_payload[stbl.payload_offset() as usize..(stbl.offset + stbl.size) as usize];

        let stsd = find_child_box(stbl_payload, &box_type::STSD)?;
        let stsd_payload = &stbl_payload[stsd.payload_offset() as usize..(stsd.offset + stsd.size) as usize];

        if stsd_payload.len() < 8 {
            continue;
        }
        let entry_data = &stsd_payload[8..];

        if let Some(config) = find_esds_in_entry(entry_data) {
            return Some(config);
        }
    }

    None
}

fn find_esds_in_entry(entry_data: &[u8]) -> Option<(u8, u8, u8)> {
    for box_result in iterate_boxes(entry_data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        if box_end > entry_data.len() {
            continue;
        }
        let box_payload = &entry_data[header.payload_offset() as usize..box_end];

        if header.box_type == *b"mp4a" || header.box_type == *b"enca" {
            // Audio sample entry: skip 28 bytes (8 header + 6 reserved + 2 data_ref + 8 reserved + 2 channels + 2 sample_size + 2 compression + 2 packet_size + 4 sample_rate)
            if box_payload.len() > 20 {
                let children = &box_payload[20..];
                if let Some(config) = find_esds_in_children(children) {
                    return Some(config);
                }
            }
        }

        if header.box_type == *b"esds" {
            return parse_audio_specific_config(box_payload);
        }
    }
    None
}

fn find_esds_in_children(data: &[u8]) -> Option<(u8, u8, u8)> {
    for box_result in iterate_boxes(data) {
        let header = box_result.ok()?;
        let box_end = (header.offset + header.size) as usize;
        if box_end > data.len() {
            continue;
        }
        let box_payload = &data[header.payload_offset() as usize..box_end];

        if header.box_type == *b"esds" {
            return parse_audio_specific_config(box_payload);
        }
    }
    None
}

/// Parse AudioSpecificConfig from esds payload to extract profile, sample_rate_index, channels.
fn parse_audio_specific_config(esds_payload: &[u8]) -> Option<(u8, u8, u8)> {
    // esds is a full box: version(1) + flags(3), then ES_Descriptor tag structure
    // We need to find the AudioSpecificConfig (tag 0x05) which contains:
    // audioObjectType(5 bits) + samplingFrequencyIndex(4 bits) + channelConfiguration(4 bits)
    if esds_payload.len() < 4 {
        return None;
    }

    // Search for DecoderSpecificInfo tag (0x05)
    let data = &esds_payload[4..]; // Skip version+flags
    for i in 0..data.len().saturating_sub(3) {
        if data[i] == 0x05 {
            // Found tag 0x05, next byte(s) are length
            let (len, hdr_size) = parse_esds_tag_length(&data[i + 1..]);
            let config_start = i + 1 + hdr_size;
            if config_start + 2 <= data.len() && len >= 2 {
                let b0 = data[config_start];
                let b1 = data[config_start + 1];
                let audio_object_type = (b0 >> 3) & 0x1F;
                let sample_rate_index = ((b0 & 0x07) << 1) | ((b1 >> 7) & 0x01);
                let channel_config = (b1 >> 3) & 0x0F;
                return Some((audio_object_type, sample_rate_index, channel_config));
            }
        }
    }

    None
}

fn parse_esds_tag_length(data: &[u8]) -> (usize, usize) {
    // ESDS uses expandable length encoding (1-4 bytes, high bit = continuation)
    let mut len = 0usize;
    let mut bytes_read = 0;
    for &b in data.iter().take(4) {
        len = (len << 7) | (b & 0x7F) as usize;
        bytes_read += 1;
        if b & 0x80 == 0 {
            break;
        }
    }
    (len, bytes_read)
}

/// Parse tfdt (Track Fragment Decode Time) box payload.
fn parse_tfdt(payload: &[u8]) -> u64 {
    if payload.len() < 4 {
        return 0;
    }
    let version = payload[0];
    if version == 1 && payload.len() >= 12 {
        // version 1: 64-bit base_media_decode_time
        u64::from_be_bytes([
            payload[4], payload[5], payload[6], payload[7],
            payload[8], payload[9], payload[10], payload[11],
        ])
    } else if payload.len() >= 8 {
        // version 0: 32-bit base_media_decode_time
        u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]) as u64
    } else {
        0
    }
}

/// Map a sample rate to ADTS sample_rate_index.
pub fn sample_rate_to_index(sample_rate: u32) -> u8 {
    match sample_rate {
        96000 => 0,
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
        7350 => 12,
        _ => 3, // Default to 48000
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ts;

    #[test]
    fn encode_timestamp_roundtrip() {
        let pts = 90000u64;
        let encoded = encode_timestamp(pts, 0x21);
        // Decode using the same logic as parse_timestamp in ts.rs
        let decoded = ((encoded[0] as u64 >> 1) & 0x07) << 30
            | (encoded[1] as u64) << 22
            | ((encoded[2] as u64 >> 1) & 0x7F) << 15
            | (encoded[3] as u64) << 7
            | (encoded[4] as u64 >> 1);
        assert_eq!(decoded, pts);
    }

    #[test]
    fn encode_timestamp_zero() {
        let encoded = encode_timestamp(0, 0x21);
        let decoded = ((encoded[0] as u64 >> 1) & 0x07) << 30
            | (encoded[1] as u64) << 22
            | ((encoded[2] as u64 >> 1) & 0x7F) << 15
            | (encoded[3] as u64) << 7
            | (encoded[4] as u64 >> 1);
        assert_eq!(decoded, 0);
    }

    #[test]
    fn encode_timestamp_large() {
        let pts = 8_589_934_591u64; // Max 33-bit value
        let encoded = encode_timestamp(pts, 0x21);
        let decoded = ((encoded[0] as u64 >> 1) & 0x07) << 30
            | (encoded[1] as u64) << 22
            | ((encoded[2] as u64 >> 1) & 0x7F) << 15
            | (encoded[3] as u64) << 7
            | (encoded[4] as u64 >> 1);
        assert_eq!(decoded, pts);
    }

    #[test]
    fn avcc_to_annexb_basic() {
        // Single NAL unit in AVCC format
        let nal_data = vec![0x65, 0xAA, 0xBB]; // IDR NAL
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&(nal_data.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal_data);

        let sps = vec![0x67, 0x42, 0x00, 0x1E]; // Fake SPS
        let pps = vec![0x68, 0xCE, 0x38, 0x80]; // Fake PPS

        let annexb = convert_avcc_to_annexb(&avcc, &sps, &pps, true);

        // Should have: start_code + SPS + start_code + PPS + start_code + NAL
        assert!(annexb.starts_with(&[0x00, 0x00, 0x00, 0x01]));
        // Verify SPS is present
        assert!(annexb.windows(4).any(|w| w == &sps[..]));
        // Verify PPS is present
        assert!(annexb.windows(4).any(|w| w == &pps[..]));
        // Verify NAL data is present
        assert!(annexb.windows(3).any(|w| w == &nal_data[..]));
    }

    #[test]
    fn avcc_to_annexb_non_idr() {
        let nal_data = vec![0x41, 0xAA]; // Non-IDR NAL
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&(nal_data.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal_data);

        let sps = vec![0x67, 0x42];
        let pps = vec![0x68, 0xCE];

        let annexb = convert_avcc_to_annexb(&avcc, &sps, &pps, false);

        // Non-IDR should NOT have SPS/PPS
        assert_eq!(&annexb[..4], &[0x00, 0x00, 0x00, 0x01]);
        assert_eq!(&annexb[4..], &nal_data[..]);
    }

    #[test]
    fn avcc_to_annexb_multiple_nals() {
        let nal1 = vec![0x65, 0xAA];
        let nal2 = vec![0x06, 0xBB, 0xCC]; // SEI
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&(nal1.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal1);
        avcc.extend_from_slice(&(nal2.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal2);

        let annexb = convert_avcc_to_annexb(&avcc, &[], &[], false);

        // Two NALs with start codes
        let start_code = [0x00, 0x00, 0x00, 0x01];
        let count = annexb.windows(4).filter(|w| *w == start_code).count();
        assert_eq!(count, 2);
    }

    #[test]
    fn build_adts_header_basic() {
        let header = build_adts_header(2, 3, 2, 128); // AAC-LC, 48kHz, stereo, 128 bytes
        // Syncword check
        assert_eq!(header[0], 0xFF);
        assert_eq!(header[1] & 0xF0, 0xF0);
        // Protection absent
        assert_eq!(header[1] & 0x01, 1);
        // Frame length = 128 + 7 = 135
        let frame_len = ((header[3] as u16 & 0x03) << 11)
            | ((header[4] as u16) << 3)
            | ((header[5] as u16) >> 5);
        assert_eq!(frame_len, 135);
    }

    #[test]
    fn build_adts_header_roundtrip_with_strip() {
        let aac_data = vec![0xAA; 64];
        let header = build_adts_header(2, 4, 2, aac_data.len()); // AAC-LC, 44100, stereo

        let mut adts_frame = Vec::new();
        adts_frame.extend_from_slice(&header);
        adts_frame.extend_from_slice(&aac_data);

        // Verify ADTS syncword
        assert_eq!(adts_frame[0], 0xFF);
        assert_eq!(adts_frame[1] & 0xF0, 0xF0);

        // Parse frame length from ADTS header
        let frame_len = ((adts_frame[3] as usize & 0x03) << 11)
            | ((adts_frame[4] as usize) << 3)
            | ((adts_frame[5] as usize) >> 5);
        assert_eq!(frame_len, aac_data.len() + 7);
    }

    #[test]
    fn pat_packet_roundtrip() {
        let pat_pkt = build_pat_packet(0);

        // Parse it as a TS packet
        let parsed = ts::parse_ts_packet(&pat_pkt).unwrap();
        assert_eq!(parsed.pid, PAT_PID);
        assert!(parsed.pusi);

        // Parse PAT from payload
        let pat = ts::parse_pat(&parsed.payload).unwrap();
        assert_eq!(pat.programs.len(), 1);
        assert_eq!(pat.programs[0], (1, PMT_PID));
    }

    #[test]
    fn pmt_packet_roundtrip() {
        let pmt_pkt = build_pmt_packet(TsCodec::H264, TsCodec::Aac, 0);

        let parsed = ts::parse_ts_packet(&pmt_pkt).unwrap();
        assert_eq!(parsed.pid, PMT_PID);
        assert!(parsed.pusi);

        let pmt = ts::parse_pmt(&parsed.payload).unwrap();
        assert_eq!(pmt.streams.len(), 2);
        assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
        assert_eq!(pmt.streams[0].pid, VIDEO_PID);
        assert_eq!(pmt.streams[1].stream_type, 0x0F); // AAC
        assert_eq!(pmt.streams[1].pid, AUDIO_PID);
    }

    #[test]
    fn pes_packet_pts_roundtrip() {
        let data = vec![0xAA; 32];
        let pes = build_pes_packet(VIDEO_STREAM_ID, 90000, None, &data);

        let (stream_id, pts, dts, header_len) = ts::parse_pes_header(&pes).unwrap();
        assert_eq!(stream_id, VIDEO_STREAM_ID);
        assert_eq!(pts, Some(90000));
        assert!(dts.is_none());
        assert_eq!(&pes[header_len..], &data[..]);
    }

    #[test]
    fn pes_packet_pts_dts_roundtrip() {
        let data = vec![0xBB; 16];
        let pes = build_pes_packet(VIDEO_STREAM_ID, 93000, Some(90000), &data);

        let (stream_id, pts, dts, header_len) = ts::parse_pes_header(&pes).unwrap();
        assert_eq!(stream_id, VIDEO_STREAM_ID);
        assert_eq!(pts, Some(93000));
        assert_eq!(dts, Some(90000));
        assert_eq!(&pes[header_len..], &data[..]);
    }

    #[test]
    fn packetize_pes_produces_188_byte_packets() {
        let pes = build_pes_packet(VIDEO_STREAM_ID, 90000, None, &vec![0xCC; 512]);
        let mut cc = 0u8;
        let packets = packetize_pes(VIDEO_PID, &pes, true, &mut cc, true);

        for pkt in &packets {
            assert_eq!(pkt.len(), TS_PACKET_SIZE);
            assert_eq!(pkt[0], TS_SYNC_BYTE);
            let pid = ((pkt[1] as u16 & 0x1F) << 8) | pkt[2] as u16;
            assert_eq!(pid, VIDEO_PID);
        }

        // First packet should have PUSI
        assert!(packets[0][1] & 0x40 != 0);
        // Subsequent should not
        if packets.len() > 1 {
            assert!(packets[1][1] & 0x40 == 0);
        }
    }

    #[test]
    fn packetize_pes_continuity_counter() {
        let pes = build_pes_packet(AUDIO_STREAM_ID, 90000, None, &vec![0xDD; 512]);
        let mut cc = 0u8;
        let packets = packetize_pes(AUDIO_PID, &pes, true, &mut cc, false);

        // CC should increment for each packet
        assert!(packets.len() > 1);
        assert_eq!(cc as usize, packets.len() % 16);
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let key = [0x01u8; 16];
        let iv = [0x02u8; 16];
        let plaintext = vec![0xAA; 100]; // Not aligned to 16 bytes

        let encrypted = encrypt_ts_segment(&plaintext, &key, &iv).unwrap();
        assert_ne!(encrypted, plaintext);
        assert_eq!(encrypted.len() % 16, 0); // Padded

        let decrypted = ts::decrypt_ts_segment(&encrypted, &key, &iv).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn encrypt_empty() {
        let key = [0u8; 16];
        let iv = [0u8; 16];
        let result = encrypt_ts_segment(&[], &key, &iv).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn rescale_time_same_timescale() {
        assert_eq!(rescale_time(90000, 90000, 90000), 90000);
    }

    #[test]
    fn rescale_time_48k_to_90k() {
        // 48000 ticks at 48kHz = 1 second = 90000 ticks at 90kHz
        assert_eq!(rescale_time(48000, 48000, 90000), 90000);
    }

    #[test]
    fn rescale_time_zero() {
        assert_eq!(rescale_time(0, 48000, 90000), 0);
    }

    #[test]
    fn sample_rate_to_index_known() {
        assert_eq!(sample_rate_to_index(48000), 3);
        assert_eq!(sample_rate_to_index(44100), 4);
        assert_eq!(sample_rate_to_index(96000), 0);
    }

    #[test]
    fn sample_rate_to_index_unknown() {
        assert_eq!(sample_rate_to_index(12345), 3); // Default
    }
}
