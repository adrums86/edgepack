use crate::error::{EdgepackError, Result};
use crate::media::cmaf::EmsgBox;
use serde::{Deserialize, Serialize};

/// SCTE-35 scheme ID URI for binary splice info in emsg boxes.
pub const SCTE35_SCHEME_URI: &str = "urn:scte:scte35:2013:bin";

/// Alternative SCTE-35 scheme URIs that should also be recognized.
pub const SCTE35_XML_SCHEME_URI: &str = "urn:scte:scte35:2014:xml+bin";

/// Splice command type: splice_insert (ad boundary).
pub const SPLICE_INSERT: u8 = 0x05;

/// Splice command type: time_signal (cue point).
pub const TIME_SIGNAL: u8 = 0x06;

/// Parsed SCTE-35 splice information from an emsg box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scte35SpliceInfo {
    /// Splice command type (0x05 = splice_insert, 0x06 = time_signal).
    pub splice_command_type: u8,
    /// PTS time from splice_time() (33-bit PTS value, in 90kHz ticks).
    pub pts_time: Option<u64>,
    /// Break duration in seconds (from break_duration()).
    pub break_duration: Option<f64>,
    /// Splice event ID (from splice_insert).
    pub splice_event_id: u32,
    /// True = start of ad (out of network), false = return to content.
    pub out_of_network: bool,
    /// Unique program ID.
    pub unique_program_id: u16,
}

/// Check if an emsg box carries SCTE-35 splice information.
pub fn is_scte35_emsg(emsg: &EmsgBox) -> bool {
    emsg.scheme_id_uri == SCTE35_SCHEME_URI || emsg.scheme_id_uri == SCTE35_XML_SCHEME_URI
}

/// Parse a SCTE-35 splice_info_section from binary data.
///
/// Handles splice_insert (0x05) and time_signal (0x06) commands.
/// Other command types are parsed minimally (command type + event_id=0).
pub fn parse_splice_info(data: &[u8]) -> Result<Scte35SpliceInfo> {
    if data.len() < 3 {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 splice_info_section too small".into(),
        ));
    }

    // table_id (8 bits) — should be 0xFC for splice_info_section
    let table_id = data[0];
    if table_id != 0xFC {
        return Err(EdgepackError::MediaParse(format!(
            "SCTE-35 invalid table_id: 0x{table_id:02X} (expected 0xFC)"
        )));
    }

    // section_syntax_indicator (1) + private_indicator (1) + sap_type (2) + section_length (12)
    // = 2 bytes
    let section_length = (((data[1] & 0x0F) as u16) << 8) | data[2] as u16;
    let section_end = 3 + section_length as usize;
    if section_end > data.len() {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 section_length exceeds data".into(),
        ));
    }

    // protocol_version (8) + encrypted_packet (1) + encryption_algorithm (6) +
    // pts_adjustment (33) + cw_index (8) + tier (12) + splice_command_length (12) +
    // splice_command_type (8)
    // = 11 bytes minimum after section header (bytes 3..14)
    if data.len() < 14 {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 too small for header fields".into(),
        ));
    }

    // Skip to splice_command_length and splice_command_type
    // Byte offsets from start:
    // 3: protocol_version (1)
    // 4-7: encrypted(1) + encryption_algo(6) + pts_adjustment(33) = 40 bits = 5 bytes
    // 8: cw_index (1 byte)
    // 9-10: tier(12) + splice_command_length(12) = 24 bits = 3 bytes
    // But the bits cross byte boundaries, so let's parse carefully.
    let _protocol_version = data[3];
    let encrypted_packet = (data[4] >> 7) & 0x01;

    if encrypted_packet != 0 {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 encrypted packets not supported".into(),
        ));
    }

    // pts_adjustment: bits 6..38 of byte 4 (33 bits total)
    // Skip it for now — we use splice_time directly.

    // Bytes 10-12: tier (12 bits) + splice_command_length (12 bits) = 24 bits = 3 bytes
    // Byte 13: splice_command_type
    // Byte 14+: splice command data
    if data.len() < 14 {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 too small for command header".into(),
        ));
    }
    let splice_command_length =
        (((data[11] & 0x0F) as u16) << 8) | data[12] as u16;
    let splice_command_type = data[13];

    let cmd_start = 14usize;
    let cmd_end = if splice_command_length == 0xFFF {
        // 0xFFF means the length is unknown — read until end of section
        section_end.saturating_sub(4) // subtract CRC32
    } else {
        cmd_start + splice_command_length as usize
    };

    if cmd_end > data.len() {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 splice command extends beyond data".into(),
        ));
    }

    let cmd_data = &data[cmd_start..cmd_end];

    match splice_command_type {
        SPLICE_INSERT => parse_splice_insert(cmd_data),
        TIME_SIGNAL => parse_time_signal(cmd_data),
        _ => {
            // Unknown command type — return minimal info
            Ok(Scte35SpliceInfo {
                splice_command_type,
                pts_time: None,
                break_duration: None,
                splice_event_id: 0,
                out_of_network: false,
                unique_program_id: 0,
            })
        }
    }
}

/// Parse a splice_insert command.
fn parse_splice_insert(data: &[u8]) -> Result<Scte35SpliceInfo> {
    if data.len() < 5 {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 splice_insert too small".into(),
        ));
    }

    let splice_event_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    let splice_event_cancel = (data[4] >> 7) & 0x01;

    if splice_event_cancel != 0 {
        return Ok(Scte35SpliceInfo {
            splice_command_type: SPLICE_INSERT,
            pts_time: None,
            break_duration: None,
            splice_event_id,
            out_of_network: false,
            unique_program_id: 0,
        });
    }

    if data.len() < 10 {
        return Err(EdgepackError::MediaParse(
            "SCTE-35 splice_insert too small for event data".into(),
        ));
    }

    let out_of_network = (data[5] >> 7) & 0x01 != 0;
    let program_splice_flag = (data[5] >> 6) & 0x01 != 0;
    let duration_flag = (data[5] >> 5) & 0x01 != 0;
    let splice_immediate_flag = (data[5] >> 4) & 0x01 != 0;

    let mut offset = 6usize;
    let mut pts_time = None;

    if program_splice_flag && !splice_immediate_flag {
        // splice_time()
        if offset < data.len() {
            let time_specified = (data[offset] >> 7) & 0x01 != 0;
            if time_specified && offset + 5 <= data.len() {
                let pts = ((data[offset] as u64 & 0x01) << 32)
                    | (data[offset + 1] as u64) << 24
                    | (data[offset + 2] as u64) << 16
                    | (data[offset + 3] as u64) << 8
                    | data[offset + 4] as u64;
                pts_time = Some(pts);
                offset += 5;
            } else {
                offset += 1;
            }
        }
    }

    let mut break_duration = None;

    if duration_flag && offset + 5 <= data.len() {
        // break_duration()
        let _auto_return = (data[offset] >> 7) & 0x01;
        let duration_ticks = ((data[offset] as u64 & 0x01) << 32)
            | (data[offset + 1] as u64) << 24
            | (data[offset + 2] as u64) << 16
            | (data[offset + 3] as u64) << 8
            | data[offset + 4] as u64;
        break_duration = Some(duration_ticks as f64 / 90000.0);
        offset += 5;
    }

    let unique_program_id = if offset + 2 <= data.len() {
        u16::from_be_bytes([data[offset], data[offset + 1]])
    } else {
        0
    };

    Ok(Scte35SpliceInfo {
        splice_command_type: SPLICE_INSERT,
        pts_time,
        break_duration,
        splice_event_id,
        out_of_network,
        unique_program_id,
    })
}

/// Parse a time_signal command.
fn parse_time_signal(data: &[u8]) -> Result<Scte35SpliceInfo> {
    let mut pts_time = None;

    if !data.is_empty() {
        let time_specified = (data[0] >> 7) & 0x01 != 0;
        if time_specified && data.len() >= 5 {
            let pts = ((data[0] as u64 & 0x01) << 32)
                | (data[1] as u64) << 24
                | (data[2] as u64) << 16
                | (data[3] as u64) << 8
                | data[4] as u64;
            pts_time = Some(pts);
        }
    }

    Ok(Scte35SpliceInfo {
        splice_command_type: TIME_SIGNAL,
        pts_time,
        break_duration: None,
        splice_event_id: 0,
        out_of_network: false,
        unique_program_id: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal splice_info_section with a splice_insert command.
    fn build_splice_insert(
        event_id: u32,
        out_of_network: bool,
        pts: Option<u64>,
        break_dur_secs: Option<f64>,
    ) -> Vec<u8> {
        let mut cmd = Vec::new();
        cmd.extend_from_slice(&event_id.to_be_bytes()); // splice_event_id
        cmd.push(0x00); // splice_event_cancel_indicator = 0

        let mut flags: u8 = 0;
        if out_of_network {
            flags |= 0x80;
        }
        flags |= 0x40; // program_splice_flag = 1
        if break_dur_secs.is_some() {
            flags |= 0x20; // duration_flag = 1
        }
        if pts.is_none() {
            flags |= 0x10; // splice_immediate_flag = 1
        }
        cmd.push(flags);

        if let Some(pts_val) = pts {
            // splice_time with time_specified = 1
            cmd.push(0xFE | ((pts_val >> 32) as u8 & 0x01)); // time_specified(1) + reserved(6) + pts_bit32
            cmd.push((pts_val >> 24) as u8);
            cmd.push((pts_val >> 16) as u8);
            cmd.push((pts_val >> 8) as u8);
            cmd.push(pts_val as u8);
        }

        if let Some(dur) = break_dur_secs {
            let ticks = (dur * 90000.0) as u64;
            cmd.push(0xFE | ((ticks >> 32) as u8 & 0x01)); // auto_return(1) + reserved(6) + duration_bit32
            cmd.push((ticks >> 24) as u8);
            cmd.push((ticks >> 16) as u8);
            cmd.push((ticks >> 8) as u8);
            cmd.push(ticks as u8);
        }

        // unique_program_id + avail_num + avails_expected
        cmd.extend_from_slice(&[0x00, 0x01, 0x00, 0x01]);

        build_splice_info_section(SPLICE_INSERT, &cmd)
    }

    /// Build a minimal splice_info_section with a time_signal command.
    fn build_time_signal(pts: Option<u64>) -> Vec<u8> {
        let mut cmd = Vec::new();
        if let Some(pts_val) = pts {
            cmd.push(0xFE | ((pts_val >> 32) as u8 & 0x01));
            cmd.push((pts_val >> 24) as u8);
            cmd.push((pts_val >> 16) as u8);
            cmd.push((pts_val >> 8) as u8);
            cmd.push(pts_val as u8);
        } else {
            cmd.push(0x00); // time_specified = 0
        }

        build_splice_info_section(TIME_SIGNAL, &cmd)
    }

    /// Build a splice_info_section wrapper around a command.
    fn build_splice_info_section(command_type: u8, command_data: &[u8]) -> Vec<u8> {
        let mut section = Vec::new();
        section.push(0xFC); // table_id

        // We need to compute section_length (everything after these 3 bytes, excluding CRC)
        // header_after_length = protocol_version(1) + encrypted/pts_adj(5) + cw_index(1) +
        //                       tier_cmd_length(3) + command_type(1) = 11
        // + command_data.len() + descriptor_loop_length(2) + CRC(4)
        let section_body_len = 11 + command_data.len() + 2 + 4;
        let section_length = section_body_len as u16;
        section.push(0x30 | ((section_length >> 8) as u8 & 0x0F)); // section_syntax_indicator=0, private=0, sap=3
        section.push(section_length as u8);

        // protocol_version
        section.push(0x00);
        // encrypted_packet(1)=0 + encryption_algo(6)=0 + pts_adjustment(33)=0
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00]);
        // cw_index
        section.push(0x00);
        // tier(12)=0xFFF + splice_command_length(12)
        let cmd_len = command_data.len() as u16;
        section.push(0xFF); // tier upper 8 bits
        section.push(0xF0 | ((cmd_len >> 8) as u8 & 0x0F)); // tier lower 4 + cmd_length upper 4
        section.push(cmd_len as u8); // cmd_length lower 8
        // splice_command_type
        section.push(command_type);
        // command data
        section.extend_from_slice(command_data);
        // descriptor_loop_length = 0
        section.extend_from_slice(&[0x00, 0x00]);
        // CRC32 placeholder (not validated)
        section.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);

        section
    }

    #[test]
    fn is_scte35_emsg_true() {
        let emsg = EmsgBox {
            version: 1,
            scheme_id_uri: SCTE35_SCHEME_URI.to_string(),
            value: String::new(),
            timescale: 90000,
            presentation_time: 0,
            event_duration: 0,
            id: 0,
            message_data: Vec::new(),
        };
        assert!(is_scte35_emsg(&emsg));
    }

    #[test]
    fn is_scte35_emsg_xml_variant() {
        let emsg = EmsgBox {
            version: 1,
            scheme_id_uri: SCTE35_XML_SCHEME_URI.to_string(),
            value: String::new(),
            timescale: 90000,
            presentation_time: 0,
            event_duration: 0,
            id: 0,
            message_data: Vec::new(),
        };
        assert!(is_scte35_emsg(&emsg));
    }

    #[test]
    fn is_scte35_emsg_false() {
        let emsg = EmsgBox {
            version: 1,
            scheme_id_uri: "urn:other:scheme".to_string(),
            value: String::new(),
            timescale: 90000,
            presentation_time: 0,
            event_duration: 0,
            id: 0,
            message_data: Vec::new(),
        };
        assert!(!is_scte35_emsg(&emsg));
    }

    #[test]
    fn parse_splice_insert_basic() {
        let data = build_splice_insert(1234, true, None, None);
        let info = parse_splice_info(&data).unwrap();
        assert_eq!(info.splice_command_type, SPLICE_INSERT);
        assert_eq!(info.splice_event_id, 1234);
        assert!(info.out_of_network);
        assert!(info.pts_time.is_none());
        assert!(info.break_duration.is_none());
    }

    #[test]
    fn parse_splice_insert_with_pts() {
        let pts = 900000; // 10 seconds at 90kHz
        let data = build_splice_insert(42, true, Some(pts), None);
        let info = parse_splice_info(&data).unwrap();
        assert_eq!(info.splice_event_id, 42);
        assert_eq!(info.pts_time, Some(pts));
    }

    #[test]
    fn parse_splice_insert_with_duration() {
        let data = build_splice_insert(1, true, None, Some(30.0));
        let info = parse_splice_info(&data).unwrap();
        assert!(info.break_duration.is_some());
        let dur = info.break_duration.unwrap();
        assert!((dur - 30.0).abs() < 0.001);
    }

    #[test]
    fn parse_splice_insert_return_to_content() {
        let data = build_splice_insert(1, false, None, None);
        let info = parse_splice_info(&data).unwrap();
        assert!(!info.out_of_network);
    }

    #[test]
    fn parse_time_signal_with_pts() {
        let pts = 8100000; // 90 seconds at 90kHz
        let data = build_time_signal(Some(pts));
        let info = parse_splice_info(&data).unwrap();
        assert_eq!(info.splice_command_type, TIME_SIGNAL);
        assert_eq!(info.pts_time, Some(pts));
        assert_eq!(info.splice_event_id, 0);
    }

    #[test]
    fn parse_time_signal_no_pts() {
        let data = build_time_signal(None);
        let info = parse_splice_info(&data).unwrap();
        assert_eq!(info.splice_command_type, TIME_SIGNAL);
        assert!(info.pts_time.is_none());
    }

    #[test]
    fn parse_splice_info_invalid_table_id() {
        let data = vec![0x00; 20]; // table_id != 0xFC
        assert!(parse_splice_info(&data).is_err());
    }

    #[test]
    fn parse_splice_info_too_small() {
        let data = vec![0xFC, 0x00];
        assert!(parse_splice_info(&data).is_err());
    }

    #[test]
    fn parse_splice_info_serde_roundtrip() {
        let info = Scte35SpliceInfo {
            splice_command_type: SPLICE_INSERT,
            pts_time: Some(900000),
            break_duration: Some(30.0),
            splice_event_id: 42,
            out_of_network: true,
            unique_program_id: 1,
        };
        let json = serde_json::to_string(&info).unwrap();
        let parsed: Scte35SpliceInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.splice_command_type, SPLICE_INSERT);
        assert_eq!(parsed.splice_event_id, 42);
        assert_eq!(parsed.pts_time, Some(900000));
    }
}
