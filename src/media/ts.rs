//! MPEG-TS demuxer for extracting PES packets from transport stream segments.
//!
//! Feature-gated behind `#[cfg(feature = "ts")]`.

use crate::error::{EdgepackError, Result};

/// TS packet size.
pub const TS_PACKET_SIZE: usize = 188;
/// TS sync byte.
pub const TS_SYNC_BYTE: u8 = 0x47;
/// PAT PID.
pub const PAT_PID: u16 = 0x0000;

/// A parsed MPEG-TS packet header.
#[derive(Debug, Clone)]
pub struct TsPacket {
    /// Packet ID (13 bits).
    pub pid: u16,
    /// Payload unit start indicator.
    pub pusi: bool,
    /// Continuity counter (4 bits).
    pub continuity_counter: u8,
    /// Adaptation field data (if present).
    pub adaptation_field: Option<AdaptationField>,
    /// Payload bytes (after header + adaptation field).
    pub payload: Vec<u8>,
}

/// Adaptation field data.
#[derive(Debug, Clone)]
pub struct AdaptationField {
    /// Length of the adaptation field.
    pub length: u8,
    /// Random access indicator (indicates IDR/sync point).
    pub random_access_indicator: bool,
    /// PCR value (if present), in 90kHz ticks.
    pub pcr: Option<u64>,
}

/// A reassembled PES (Packetized Elementary Stream) packet.
#[derive(Debug, Clone)]
pub struct PesPacket {
    /// Stream ID (e.g., 0xE0 for video, 0xC0 for audio).
    pub stream_id: u8,
    /// PTS (Presentation Time Stamp) in 90kHz ticks.
    pub pts: Option<u64>,
    /// DTS (Decode Time Stamp) in 90kHz ticks.
    pub dts: Option<u64>,
    /// Elementary stream data (H.264 NAL units for video, ADTS frames for audio).
    pub data: Vec<u8>,
}

/// Program Association Table entry.
#[derive(Debug, Clone)]
pub struct PatTable {
    /// Program number -> PMT PID mapping.
    pub programs: Vec<(u16, u16)>,
}

/// Program Map Table.
#[derive(Debug, Clone)]
pub struct PmtTable {
    /// PCR PID.
    pub pcr_pid: u16,
    /// Elementary streams in this program.
    pub streams: Vec<PmtStream>,
}

/// A stream entry in the PMT.
#[derive(Debug, Clone)]
pub struct PmtStream {
    /// Stream type (0x1B = H.264, 0x24 = H.265, 0x0F = AAC, 0x81 = AC-3).
    pub stream_type: u8,
    /// Elementary stream PID.
    pub pid: u16,
}

/// Detected codec from PMT stream type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TsCodec {
    H264,
    H265,
    Aac,
    Ac3,
    Unknown(u8),
}

impl TsCodec {
    pub fn from_stream_type(st: u8) -> Self {
        match st {
            0x1B => TsCodec::H264,
            0x24 => TsCodec::H265,
            0x0F => TsCodec::Aac,
            0x81 => TsCodec::Ac3,
            other => TsCodec::Unknown(other),
        }
    }

    pub fn is_video(&self) -> bool {
        matches!(self, TsCodec::H264 | TsCodec::H265)
    }

    pub fn is_audio(&self) -> bool {
        matches!(self, TsCodec::Aac | TsCodec::Ac3)
    }
}

/// Result of demuxing a TS segment.
#[derive(Debug, Clone)]
pub struct DemuxedSegment {
    /// Video PES packets in decode order.
    pub video_packets: Vec<PesPacket>,
    /// Audio PES packets in decode order.
    pub audio_packets: Vec<PesPacket>,
    /// Detected video codec.
    pub video_codec: Option<TsCodec>,
    /// Detected audio codec.
    pub audio_codec: Option<TsCodec>,
    /// PMT information.
    pub pmt: Option<PmtTable>,
}

/// Parse a single 188-byte TS packet.
pub fn parse_ts_packet(data: &[u8]) -> Result<TsPacket> {
    if data.len() < TS_PACKET_SIZE {
        return Err(EdgepackError::MediaParse(format!(
            "TS packet too short: {} bytes, expected {}",
            data.len(),
            TS_PACKET_SIZE
        )));
    }

    if data[0] != TS_SYNC_BYTE {
        return Err(EdgepackError::MediaParse(format!(
            "invalid TS sync byte: 0x{:02x}, expected 0x{:02x}",
            data[0], TS_SYNC_BYTE
        )));
    }

    let pid = ((data[1] as u16 & 0x1F) << 8) | data[2] as u16;
    let pusi = (data[1] & 0x40) != 0;
    let adaptation_field_control = (data[3] >> 4) & 0x03;
    let continuity_counter = data[3] & 0x0F;

    let has_adaptation_field = adaptation_field_control == 2 || adaptation_field_control == 3;
    let has_payload = adaptation_field_control == 1 || adaptation_field_control == 3;

    let mut offset = 4;
    let adaptation_field = if has_adaptation_field && offset < TS_PACKET_SIZE {
        let af = parse_adaptation_field(&data[offset..]);
        offset += 1 + af.length as usize;
        Some(af)
    } else {
        None
    };

    let payload = if has_payload && offset < TS_PACKET_SIZE {
        data[offset..TS_PACKET_SIZE].to_vec()
    } else {
        Vec::new()
    };

    Ok(TsPacket {
        pid,
        pusi,
        continuity_counter,
        adaptation_field,
        payload,
    })
}

/// Parse an adaptation field from the data starting at the adaptation field length byte.
pub fn parse_adaptation_field(data: &[u8]) -> AdaptationField {
    if data.is_empty() {
        return AdaptationField {
            length: 0,
            random_access_indicator: false,
            pcr: None,
        };
    }

    let length = data[0];
    if length == 0 || data.len() < 2 {
        return AdaptationField {
            length,
            random_access_indicator: false,
            pcr: None,
        };
    }

    let flags = data[1];
    let random_access_indicator = (flags & 0x40) != 0;
    let pcr_flag = (flags & 0x10) != 0;

    let pcr = if pcr_flag && data.len() >= 8 {
        // PCR is 33 bits base + 6 reserved + 9 bits extension
        let base = ((data[2] as u64) << 25)
            | ((data[3] as u64) << 17)
            | ((data[4] as u64) << 9)
            | ((data[5] as u64) << 1)
            | ((data[6] as u64) >> 7);
        let ext = (((data[6] as u64) & 0x01) << 8) | data[7] as u64;
        Some(base * 300 + ext)
    } else {
        None
    };

    AdaptationField {
        length,
        random_access_indicator,
        pcr,
    }
}

/// Parse a PES packet header.
///
/// Returns `(stream_id, pts, dts, header_length)` where `header_length` is the total
/// number of bytes consumed by the PES header (everything before the ES data).
pub fn parse_pes_header(data: &[u8]) -> Result<(u8, Option<u64>, Option<u64>, usize)> {
    if data.len() < 9 {
        return Err(EdgepackError::MediaParse(
            "PES header too short".to_string(),
        ));
    }

    // PES start code: 00 00 01
    if data[0] != 0x00 || data[1] != 0x00 || data[2] != 0x01 {
        return Err(EdgepackError::MediaParse(
            "invalid PES start code".to_string(),
        ));
    }

    let stream_id = data[3];
    // data[4..6] = PES packet length (0 for unbounded video)

    // Streams 0xBC, 0xBE, 0xBF, 0xF0-0xF2, 0xFF, 0xF8 have no optional header
    if stream_id == 0xBC
        || stream_id == 0xBE
        || stream_id == 0xBF
        || (0xF0..=0xF2).contains(&stream_id)
        || stream_id == 0xF8
        || stream_id == 0xFF
    {
        return Ok((stream_id, None, None, 6));
    }

    if data.len() < 9 {
        return Ok((stream_id, None, None, 6));
    }

    let pts_dts_flags = (data[7] >> 6) & 0x03;
    let pes_header_data_length = data[8] as usize;
    let header_length = 9 + pes_header_data_length;

    if data.len() < header_length {
        return Err(EdgepackError::MediaParse(format!(
            "PES header truncated: need {} bytes, have {}",
            header_length,
            data.len()
        )));
    }

    let pts = if pts_dts_flags >= 2 && data.len() >= 14 {
        Some(parse_timestamp(&data[9..14]))
    } else {
        None
    };

    let dts = if pts_dts_flags == 3 && data.len() >= 19 {
        Some(parse_timestamp(&data[14..19]))
    } else {
        None
    };

    Ok((stream_id, pts, dts, header_length))
}

/// Parse a 5-byte PES timestamp (PTS or DTS).
fn parse_timestamp(data: &[u8]) -> u64 {
    let b0 = data[0] as u64;
    let b1 = data[1] as u64;
    let b2 = data[2] as u64;
    let b3 = data[3] as u64;
    let b4 = data[4] as u64;

    ((b0 >> 1) & 0x07) << 30
        | (b1 << 22)
        | ((b2 >> 1) << 15)
        | (b3 << 7)
        | (b4 >> 1)
}

/// Parse a Program Association Table (PAT).
pub fn parse_pat(payload: &[u8]) -> Result<PatTable> {
    // Skip pointer_field if present (first byte)
    let offset = if !payload.is_empty() {
        1 + payload[0] as usize
    } else {
        return Err(EdgepackError::MediaParse("PAT payload empty".to_string()));
    };

    if payload.len() < offset + 8 {
        return Err(EdgepackError::MediaParse(
            "PAT payload too short".to_string(),
        ));
    }

    let data = &payload[offset..];
    // data[0] = table_id (should be 0x00)
    if data[0] != 0x00 {
        return Err(EdgepackError::MediaParse(format!(
            "invalid PAT table_id: 0x{:02x}",
            data[0]
        )));
    }

    let section_length = ((data[1] as usize & 0x0F) << 8) | data[2] as usize;
    // Skip table_id(1) + section_syntax_indicator/section_length(2) +
    // transport_stream_id(2) + version/current(1) + section_number(1) + last_section(1) = 8 bytes
    let header_size = 8;
    let crc_size = 4;

    if section_length < 5 + crc_size {
        return Ok(PatTable {
            programs: Vec::new(),
        });
    }

    let program_data_len = section_length - 5 - crc_size;
    let program_start = header_size;
    let program_end = program_start + program_data_len;

    if data.len() < program_end {
        return Err(EdgepackError::MediaParse(
            "PAT section truncated".to_string(),
        ));
    }

    let mut programs = Vec::new();
    let mut i = program_start;
    while i + 4 <= program_end {
        let program_number = ((data[i] as u16) << 8) | data[i + 1] as u16;
        let pid = ((data[i + 2] as u16 & 0x1F) << 8) | data[i + 3] as u16;
        if program_number != 0 {
            // Skip NIT entry (program_number 0)
            programs.push((program_number, pid));
        }
        i += 4;
    }

    Ok(PatTable { programs })
}

/// Parse a Program Map Table (PMT).
pub fn parse_pmt(payload: &[u8]) -> Result<PmtTable> {
    // Skip pointer_field if present
    let offset = if !payload.is_empty() {
        1 + payload[0] as usize
    } else {
        return Err(EdgepackError::MediaParse("PMT payload empty".to_string()));
    };

    if payload.len() < offset + 12 {
        return Err(EdgepackError::MediaParse(
            "PMT payload too short".to_string(),
        ));
    }

    let data = &payload[offset..];
    // data[0] = table_id (should be 0x02)
    if data[0] != 0x02 {
        return Err(EdgepackError::MediaParse(format!(
            "invalid PMT table_id: 0x{:02x}",
            data[0]
        )));
    }

    let section_length = ((data[1] as usize & 0x0F) << 8) | data[2] as usize;
    // Header: table_id(1) + section_length_bytes(2) + program_number(2) + version_etc(1) +
    //         section_number(1) + last_section(1) = 8
    // Then: PCR_PID(2) + program_info_length(2) = 4 more
    let pcr_pid = ((data[8] as u16 & 0x1F) << 8) | data[9] as u16;
    let program_info_length = ((data[10] as usize & 0x0F) << 8) | data[11] as usize;

    // ES descriptors start after program info
    let es_start = 12 + program_info_length;
    let section_end = 3 + section_length; // table_id(1) + section_length_field(2) + section_length
    let crc_size = 4;

    if section_end < crc_size {
        return Ok(PmtTable {
            pcr_pid,
            streams: Vec::new(),
        });
    }

    let es_end = if section_end > crc_size {
        section_end - crc_size
    } else {
        es_start
    };

    let mut streams = Vec::new();
    let mut i = es_start;
    while i + 5 <= es_end && i + 5 <= data.len() {
        let stream_type = data[i];
        let elementary_pid = ((data[i + 1] as u16 & 0x1F) << 8) | data[i + 2] as u16;
        let es_info_length = ((data[i + 3] as usize & 0x0F) << 8) | data[i + 4] as usize;

        streams.push(PmtStream {
            stream_type,
            pid: elementary_pid,
        });

        i += 5 + es_info_length;
    }

    Ok(PmtTable { pcr_pid, streams })
}

/// Stateful TS packet accumulator that reassembles PES packets.
pub struct TsDemuxer {
    pat: Option<PatTable>,
    pmt: Option<PmtTable>,
    pmt_pid: Option<u16>,
    video_pid: Option<u16>,
    audio_pid: Option<u16>,
    video_codec: Option<TsCodec>,
    audio_codec: Option<TsCodec>,
    // PES packet assembly buffers
    video_buffer: Vec<u8>,
    audio_buffer: Vec<u8>,
    video_pusi_seen: bool,
    audio_pusi_seen: bool,
    // Collected PES packets
    video_packets: Vec<PesPacket>,
    audio_packets: Vec<PesPacket>,
    // Current PES header info
    video_pes_header: Option<(u8, Option<u64>, Option<u64>)>,
    audio_pes_header: Option<(u8, Option<u64>, Option<u64>)>,
}

impl TsDemuxer {
    pub fn new() -> Self {
        Self {
            pat: None,
            pmt: None,
            pmt_pid: None,
            video_pid: None,
            audio_pid: None,
            video_codec: None,
            audio_codec: None,
            video_buffer: Vec::new(),
            audio_buffer: Vec::new(),
            video_pusi_seen: false,
            audio_pusi_seen: false,
            video_packets: Vec::new(),
            audio_packets: Vec::new(),
            video_pes_header: None,
            audio_pes_header: None,
        }
    }

    /// Process a single TS packet, accumulating PES data.
    pub fn push_packet(&mut self, packet: &TsPacket) -> Result<()> {
        if packet.pid == PAT_PID {
            let pat = parse_pat(&packet.payload)?;
            if let Some(&(_, pid)) = pat.programs.first() {
                self.pmt_pid = Some(pid);
            }
            self.pat = Some(pat);
            return Ok(());
        }

        if Some(packet.pid) == self.pmt_pid {
            let pmt = parse_pmt(&packet.payload)?;
            // Identify video and audio PIDs from PMT streams
            for stream in &pmt.streams {
                let codec = TsCodec::from_stream_type(stream.stream_type);
                if codec.is_video() && self.video_pid.is_none() {
                    self.video_pid = Some(stream.pid);
                    self.video_codec = Some(codec);
                } else if codec.is_audio() && self.audio_pid.is_none() {
                    self.audio_pid = Some(stream.pid);
                    self.audio_codec = Some(codec);
                }
            }
            self.pmt = Some(pmt);
            return Ok(());
        }

        // Process video PES
        if Some(packet.pid) == self.video_pid {
            self.process_pes_packet(packet, true)?;
        }

        // Process audio PES
        if Some(packet.pid) == self.audio_pid {
            self.process_pes_packet(packet, false)?;
        }

        Ok(())
    }

    fn process_pes_packet(&mut self, packet: &TsPacket, is_video: bool) -> Result<()> {
        let buffer = if is_video {
            &mut self.video_buffer
        } else {
            &mut self.audio_buffer
        };
        let pusi_seen = if is_video {
            &mut self.video_pusi_seen
        } else {
            &mut self.audio_pusi_seen
        };
        let packets = if is_video {
            &mut self.video_packets
        } else {
            &mut self.audio_packets
        };
        let pes_header = if is_video {
            &mut self.video_pes_header
        } else {
            &mut self.audio_pes_header
        };

        if packet.pusi {
            // Start of a new PES packet — flush previous one
            if *pusi_seen && !buffer.is_empty() {
                if let Some((stream_id, pts, dts)) = pes_header.take() {
                    packets.push(PesPacket {
                        stream_id,
                        pts,
                        dts,
                        data: std::mem::take(buffer),
                    });
                }
            }

            *pusi_seen = true;

            // Parse PES header from payload
            if packet.payload.len() >= 9
                && packet.payload[0] == 0x00
                && packet.payload[1] == 0x00
                && packet.payload[2] == 0x01
            {
                let (stream_id, pts, dts, header_len) = parse_pes_header(&packet.payload)?;
                *pes_header = Some((stream_id, pts, dts));
                if header_len < packet.payload.len() {
                    buffer.extend_from_slice(&packet.payload[header_len..]);
                }
            } else {
                buffer.extend_from_slice(&packet.payload);
            }
        } else if *pusi_seen {
            // Continuation of current PES packet
            buffer.extend_from_slice(&packet.payload);
        }

        Ok(())
    }

    /// Flush remaining buffered data and return the demuxed segment.
    pub fn flush(mut self) -> DemuxedSegment {
        // Flush remaining video PES
        if self.video_pusi_seen && !self.video_buffer.is_empty() {
            if let Some((stream_id, pts, dts)) = self.video_pes_header.take() {
                self.video_packets.push(PesPacket {
                    stream_id,
                    pts,
                    dts,
                    data: self.video_buffer,
                });
            }
        }

        // Flush remaining audio PES
        if self.audio_pusi_seen && !self.audio_buffer.is_empty() {
            if let Some((stream_id, pts, dts)) = self.audio_pes_header.take() {
                self.audio_packets.push(PesPacket {
                    stream_id,
                    pts,
                    dts,
                    data: self.audio_buffer,
                });
            }
        }

        DemuxedSegment {
            video_packets: self.video_packets,
            audio_packets: self.audio_packets,
            video_codec: self.video_codec,
            audio_codec: self.audio_codec,
            pmt: self.pmt,
        }
    }
}

/// Demux a complete TS segment into video and audio PES packets.
pub fn demux_segment(data: &[u8]) -> Result<DemuxedSegment> {
    let mut demuxer = TsDemuxer::new();
    let packet_count = data.len() / TS_PACKET_SIZE;
    for i in 0..packet_count {
        let start = i * TS_PACKET_SIZE;
        let packet_data = &data[start..start + TS_PACKET_SIZE];
        if packet_data[0] != TS_SYNC_BYTE {
            continue; // Skip malformed packets
        }
        let packet = parse_ts_packet(packet_data)?;
        demuxer.push_packet(&packet)?;
    }
    Ok(demuxer.flush())
}

/// Decrypt an AES-128-CBC encrypted TS segment.
///
/// HLS uses AES-128-CBC encryption on the entire TS segment (not per-sample).
/// The IV is typically the segment sequence number or explicitly signaled.
pub fn decrypt_ts_segment(data: &[u8], key: &[u8; 16], iv: &[u8; 16]) -> Result<Vec<u8>> {
    use aes::Aes128;
    use cbc::cipher::{BlockDecryptMut, KeyIvInit};

    type Aes128CbcDec = cbc::Decryptor<Aes128>;

    if data.is_empty() {
        return Ok(Vec::new());
    }

    if data.len() % 16 != 0 {
        return Err(EdgepackError::Encryption(format!(
            "AES-128-CBC data must be a multiple of 16 bytes, got {}",
            data.len()
        )));
    }

    let mut decrypted = data.to_vec();
    let decryptor = Aes128CbcDec::new(key.into(), iv.into());
    decryptor
        .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut decrypted)
        .map_err(|e| EdgepackError::Encryption(format!("AES-128-CBC decryption failed: {e}")))?;

    // Remove PKCS7 padding
    if let Some(&last_byte) = decrypted.last() {
        let pad_len = last_byte as usize;
        if pad_len > 0 && pad_len <= 16 && pad_len <= decrypted.len() {
            // Verify all padding bytes are correct
            let valid_padding = decrypted[decrypted.len() - pad_len..]
                .iter()
                .all(|&b| b == last_byte);
            if valid_padding {
                decrypted.truncate(decrypted.len() - pad_len);
            }
        }
    }

    Ok(decrypted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ts_codec_from_stream_type_h264() {
        assert_eq!(TsCodec::from_stream_type(0x1B), TsCodec::H264);
    }

    #[test]
    fn ts_codec_from_stream_type_h265() {
        assert_eq!(TsCodec::from_stream_type(0x24), TsCodec::H265);
    }

    #[test]
    fn ts_codec_from_stream_type_aac() {
        assert_eq!(TsCodec::from_stream_type(0x0F), TsCodec::Aac);
    }

    #[test]
    fn ts_codec_from_stream_type_ac3() {
        assert_eq!(TsCodec::from_stream_type(0x81), TsCodec::Ac3);
    }

    #[test]
    fn ts_codec_from_stream_type_unknown() {
        assert_eq!(TsCodec::from_stream_type(0x42), TsCodec::Unknown(0x42));
    }

    #[test]
    fn ts_codec_is_video() {
        assert!(TsCodec::H264.is_video());
        assert!(TsCodec::H265.is_video());
        assert!(!TsCodec::Aac.is_video());
        assert!(!TsCodec::Ac3.is_video());
        assert!(!TsCodec::Unknown(0x42).is_video());
    }

    #[test]
    fn ts_codec_is_audio() {
        assert!(TsCodec::Aac.is_audio());
        assert!(TsCodec::Ac3.is_audio());
        assert!(!TsCodec::H264.is_audio());
        assert!(!TsCodec::H265.is_audio());
        assert!(!TsCodec::Unknown(0x42).is_audio());
    }

    /// Build a minimal 188-byte TS packet for testing.
    fn build_test_ts_packet(pid: u16, pusi: bool, cc: u8, payload: &[u8]) -> Vec<u8> {
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

    /// Build a PAT payload (with pointer_field = 0).
    fn build_pat_payload(pmt_pid: u16) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(0x00); // pointer_field
        payload.push(0x00); // table_id = 0x00 (PAT)
        // section_syntax_indicator(1) + '0'(1) + reserved(2) + section_length(12)
        // section_length = 5 (header after length) + 4 (one program entry) + 4 (CRC)
        let section_length: u16 = 5 + 4 + 4;
        payload.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        payload.push((section_length & 0xFF) as u8);
        payload.extend_from_slice(&[0x00, 0x01]); // transport_stream_id
        payload.push(0xC1); // version=0, current=1
        payload.push(0x00); // section_number
        payload.push(0x00); // last_section_number
        // Program entry: program_number=1, PMT PID
        payload.extend_from_slice(&[0x00, 0x01]); // program_number = 1
        payload.push(0xE0 | ((pmt_pid >> 8) as u8 & 0x1F));
        payload.push((pmt_pid & 0xFF) as u8);
        // CRC32 (placeholder — not verified in our parser)
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        payload
    }

    /// Build a PMT payload (with pointer_field = 0).
    fn build_pmt_payload(video_pid: u16, audio_pid: u16) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(0x00); // pointer_field
        payload.push(0x02); // table_id = 0x02 (PMT)
        // section_length = 5 (header after length) + 4 (PCR + prog_info_len) +
        //                  5 (video stream) + 5 (audio stream) + 4 (CRC)
        let section_length: u16 = 5 + 4 + 5 + 5 + 4;
        payload.push(0xB0 | ((section_length >> 8) as u8 & 0x0F));
        payload.push((section_length & 0xFF) as u8);
        payload.extend_from_slice(&[0x00, 0x01]); // program_number
        payload.push(0xC1); // version=0, current=1
        payload.push(0x00); // section_number
        payload.push(0x00); // last_section_number
        // PCR PID = video_pid
        payload.push(0xE0 | ((video_pid >> 8) as u8 & 0x1F));
        payload.push((video_pid & 0xFF) as u8);
        // program_info_length = 0
        payload.extend_from_slice(&[0xF0, 0x00]);
        // Video stream: H.264 (0x1B)
        payload.push(0x1B);
        payload.push(0xE0 | ((video_pid >> 8) as u8 & 0x1F));
        payload.push((video_pid & 0xFF) as u8);
        payload.extend_from_slice(&[0xF0, 0x00]); // ES_info_length = 0
        // Audio stream: AAC (0x0F)
        payload.push(0x0F);
        payload.push(0xE0 | ((audio_pid >> 8) as u8 & 0x1F));
        payload.push((audio_pid & 0xFF) as u8);
        payload.extend_from_slice(&[0xF0, 0x00]); // ES_info_length = 0
        // CRC32 (placeholder)
        payload.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
        payload
    }

    /// Build a minimal PES packet with PTS.
    fn build_pes_packet(stream_id: u8, pts: u64, data: &[u8]) -> Vec<u8> {
        let mut pes = Vec::new();
        // Start code: 00 00 01
        pes.extend_from_slice(&[0x00, 0x00, 0x01]);
        pes.push(stream_id);
        // PES packet length (0 for video = unbounded)
        let pes_data_len = 3 + 5 + data.len(); // header fields + PTS + data
        if stream_id >= 0xC0 && stream_id <= 0xDF {
            // Audio: set length
            pes.extend_from_slice(&(pes_data_len as u16).to_be_bytes());
        } else {
            // Video: 0 = unbounded
            pes.extend_from_slice(&[0x00, 0x00]);
        }
        // Optional PES header: marker(2) + flags
        pes.push(0x80); // marker bits
        pes.push(0x80); // PTS only (pts_dts_flags = 10)
        pes.push(0x05); // PES header data length = 5 (PTS only)
        // Encode PTS (5 bytes)
        pes.push(0x21 | (((pts >> 30) as u8 & 0x07) << 1)); // '0010' + PTS[32..30] + '1'
        pes.push(((pts >> 22) & 0xFF) as u8);
        pes.push((((pts >> 15) & 0x7F) as u8) << 1 | 0x01);
        pes.push(((pts >> 7) & 0xFF) as u8);
        pes.push((((pts) & 0x7F) as u8) << 1 | 0x01);
        // ES data
        pes.extend_from_slice(data);
        pes
    }

    #[test]
    fn parse_ts_packet_basic() {
        let payload = [0xAA; 184];
        let pkt = build_test_ts_packet(0x100, false, 3, &payload);
        let parsed = parse_ts_packet(&pkt).unwrap();
        assert_eq!(parsed.pid, 0x100);
        assert!(!parsed.pusi);
        assert_eq!(parsed.continuity_counter, 3);
        assert_eq!(parsed.payload.len(), 184);
        assert!(parsed.adaptation_field.is_none());
    }

    #[test]
    fn parse_ts_packet_with_pusi() {
        let pkt = build_test_ts_packet(0x200, true, 0, &[0xBB; 184]);
        let parsed = parse_ts_packet(&pkt).unwrap();
        assert_eq!(parsed.pid, 0x200);
        assert!(parsed.pusi);
    }

    #[test]
    fn parse_ts_packet_invalid_sync_byte() {
        let mut pkt = build_test_ts_packet(0x100, false, 0, &[]);
        pkt[0] = 0x00; // Invalid sync byte
        let result = parse_ts_packet(&pkt);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("sync byte"));
    }

    #[test]
    fn parse_ts_packet_too_short() {
        let result = parse_ts_packet(&[0x47, 0x00]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_pat_basic() {
        let payload = build_pat_payload(0x100);
        let pat = parse_pat(&payload).unwrap();
        assert_eq!(pat.programs.len(), 1);
        assert_eq!(pat.programs[0], (1, 0x100));
    }

    #[test]
    fn parse_pmt_basic() {
        let payload = build_pmt_payload(0x101, 0x102);
        let pmt = parse_pmt(&payload).unwrap();
        assert_eq!(pmt.pcr_pid, 0x101);
        assert_eq!(pmt.streams.len(), 2);
        assert_eq!(pmt.streams[0].stream_type, 0x1B); // H.264
        assert_eq!(pmt.streams[0].pid, 0x101);
        assert_eq!(pmt.streams[1].stream_type, 0x0F); // AAC
        assert_eq!(pmt.streams[1].pid, 0x102);
    }

    #[test]
    fn parse_pes_header_with_pts() {
        let es_data = [0x00, 0x00, 0x00, 0x01, 0x65]; // H.264 IDR NAL
        let pes = build_pes_packet(0xE0, 90000, &es_data);
        let (stream_id, pts, dts, header_len) = parse_pes_header(&pes).unwrap();
        assert_eq!(stream_id, 0xE0);
        assert!(pts.is_some());
        assert_eq!(pts.unwrap(), 90000);
        assert!(dts.is_none());
        assert_eq!(header_len, 14); // 9 base + 5 PTS bytes
    }

    #[test]
    fn parse_pes_header_invalid_start_code() {
        let data = [0x01, 0x02, 0x03, 0xE0, 0x00, 0x00, 0x00, 0x00, 0x00];
        let result = parse_pes_header(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("start code"));
    }

    #[test]
    fn demux_segment_basic() {
        let video_pid: u16 = 0x101;
        let audio_pid: u16 = 0x102;
        let pmt_pid: u16 = 0x100;

        let mut ts_data = Vec::new();

        // PAT packet
        let pat_payload = build_pat_payload(pmt_pid);
        ts_data.extend_from_slice(&build_test_ts_packet(PAT_PID, true, 0, &pat_payload));

        // PMT packet
        let pmt_payload = build_pmt_payload(video_pid, audio_pid);
        ts_data.extend_from_slice(&build_test_ts_packet(pmt_pid, true, 0, &pmt_payload));

        // Video PES packet
        let video_es = [0x00, 0x00, 0x00, 0x01, 0x65, 0xAA, 0xBB]; // H.264 IDR
        let video_pes = build_pes_packet(0xE0, 90000, &video_es);
        ts_data.extend_from_slice(&build_test_ts_packet(video_pid, true, 0, &video_pes));

        // Audio PES packet
        let audio_es = [0xFF, 0xF1, 0x50, 0x80, 0x02, 0x00, 0xFC]; // ADTS header
        let audio_pes = build_pes_packet(0xC0, 90000, &audio_es);
        ts_data.extend_from_slice(&build_test_ts_packet(audio_pid, true, 0, &audio_pes));

        let result = demux_segment(&ts_data).unwrap();
        assert_eq!(result.video_codec, Some(TsCodec::H264));
        assert_eq!(result.audio_codec, Some(TsCodec::Aac));
        assert_eq!(result.video_packets.len(), 1);
        assert_eq!(result.audio_packets.len(), 1);
        assert_eq!(result.video_packets[0].pts, Some(90000));
        assert_eq!(result.audio_packets[0].pts, Some(90000));
        // TS packets are 188 bytes; PES data includes trailing zero padding from the packet.
        assert!(result.video_packets[0].data.starts_with(&video_es));
        assert!(result.audio_packets[0].data.starts_with(&audio_es));
    }

    #[test]
    fn demux_segment_multi_pes() {
        let video_pid: u16 = 0x101;
        let audio_pid: u16 = 0x102;
        let pmt_pid: u16 = 0x100;

        let mut ts_data = Vec::new();

        // PAT
        ts_data.extend_from_slice(&build_test_ts_packet(
            PAT_PID,
            true,
            0,
            &build_pat_payload(pmt_pid),
        ));

        // PMT
        ts_data.extend_from_slice(&build_test_ts_packet(
            pmt_pid,
            true,
            0,
            &build_pmt_payload(video_pid, audio_pid),
        ));

        // First video PES
        let video_es1 = [0x00, 0x00, 0x00, 0x01, 0x65, 0x11];
        let video_pes1 = build_pes_packet(0xE0, 90000, &video_es1);
        ts_data.extend_from_slice(&build_test_ts_packet(video_pid, true, 0, &video_pes1));

        // Second video PES (new PUSI)
        let video_es2 = [0x00, 0x00, 0x00, 0x01, 0x41, 0x22];
        let video_pes2 = build_pes_packet(0xE0, 93003, &video_es2);
        ts_data.extend_from_slice(&build_test_ts_packet(video_pid, true, 1, &video_pes2));

        let result = demux_segment(&ts_data).unwrap();
        assert_eq!(result.video_packets.len(), 2);
        assert_eq!(result.video_packets[0].pts, Some(90000));
        assert_eq!(result.video_packets[1].pts, Some(93003));
        // TS packets are 188 bytes; PES data includes trailing zero padding.
        assert!(result.video_packets[0].data.starts_with(&video_es1));
        assert!(result.video_packets[1].data.starts_with(&video_es2));
    }

    #[test]
    fn demux_empty_segment() {
        let result = demux_segment(&[]).unwrap();
        assert!(result.video_packets.is_empty());
        assert!(result.audio_packets.is_empty());
        assert!(result.video_codec.is_none());
        assert!(result.audio_codec.is_none());
    }

    #[test]
    fn demux_skips_non_sync_packets() {
        // Data that doesn't start with 0x47 should be skipped
        let data = vec![0x00; TS_PACKET_SIZE * 2];
        let result = demux_segment(&data).unwrap();
        assert!(result.video_packets.is_empty());
    }

    #[test]
    fn decrypt_ts_segment_roundtrip() {
        use aes::Aes128;
        use cbc::cipher::{BlockEncryptMut, KeyIvInit};
        type Aes128CbcEnc = cbc::Encryptor<Aes128>;

        let key: [u8; 16] = [0x01; 16];
        let iv: [u8; 16] = [0x02; 16];
        let plaintext = vec![0xAA; 48]; // 3 AES blocks

        // Encrypt with PKCS7 padding
        let mut to_encrypt = plaintext.clone();
        // Add PKCS7 padding manually
        let pad_len = 16 - (to_encrypt.len() % 16);
        to_encrypt.extend(vec![pad_len as u8; pad_len]);

        let len = to_encrypt.len();
        let encryptor = Aes128CbcEnc::new(&key.into(), &iv.into());
        let encrypted = encryptor
            .encrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut to_encrypt, len)
            .unwrap()
            .to_vec();

        let decrypted = decrypt_ts_segment(&encrypted, &key, &iv).unwrap();
        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_ts_segment_empty() {
        let key = [0u8; 16];
        let iv = [0u8; 16];
        let result = decrypt_ts_segment(&[], &key, &iv).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn decrypt_ts_segment_invalid_length() {
        let key = [0u8; 16];
        let iv = [0u8; 16];
        let result = decrypt_ts_segment(&[0x00; 17], &key, &iv);
        assert!(result.is_err());
    }

    #[test]
    fn parse_adaptation_field_empty() {
        let af = parse_adaptation_field(&[]);
        assert_eq!(af.length, 0);
        assert!(!af.random_access_indicator);
        assert!(af.pcr.is_none());
    }

    #[test]
    fn parse_adaptation_field_zero_length() {
        let af = parse_adaptation_field(&[0x00]);
        assert_eq!(af.length, 0);
        assert!(!af.random_access_indicator);
    }

    #[test]
    fn parse_adaptation_field_with_random_access() {
        let data = [0x01, 0x40]; // length=1, random_access_indicator=1
        let af = parse_adaptation_field(&data);
        assert_eq!(af.length, 1);
        assert!(af.random_access_indicator);
        assert!(af.pcr.is_none());
    }

    #[test]
    fn demuxed_segment_has_pmt() {
        let video_pid: u16 = 0x101;
        let audio_pid: u16 = 0x102;
        let pmt_pid: u16 = 0x100;

        let mut ts_data = Vec::new();
        ts_data.extend_from_slice(&build_test_ts_packet(
            PAT_PID,
            true,
            0,
            &build_pat_payload(pmt_pid),
        ));
        ts_data.extend_from_slice(&build_test_ts_packet(
            pmt_pid,
            true,
            0,
            &build_pmt_payload(video_pid, audio_pid),
        ));

        let result = demux_segment(&ts_data).unwrap();
        assert!(result.pmt.is_some());
        let pmt = result.pmt.unwrap();
        assert_eq!(pmt.streams.len(), 2);
    }

    #[test]
    fn pat_payload_invalid_table_id() {
        let mut payload = build_pat_payload(0x100);
        payload[1] = 0xFF; // Invalid table_id
        let result = parse_pat(&payload);
        assert!(result.is_err());
    }

    #[test]
    fn pmt_payload_invalid_table_id() {
        let mut payload = build_pmt_payload(0x101, 0x102);
        payload[1] = 0xFF; // Invalid table_id
        let result = parse_pmt(&payload);
        assert!(result.is_err());
    }
}
