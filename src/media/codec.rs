use crate::error::{EdgepackError, Result};
use crate::media::box_type;
use crate::media::cmaf::{
    box_payload, find_child_box, iterate_boxes, parse_tenc, read_box_header,
};
use crate::media::TrackType;

/// Metadata extracted from a single track in an init segment.
#[derive(Debug, Clone)]
pub struct TrackInfo {
    /// Track type (Video, Audio, Subtitle, Unknown).
    pub track_type: TrackType,
    /// Track ID from tkhd box.
    pub track_id: u32,
    /// RFC 6381 codec string (e.g., "avc1.64001f", "mp4a.40.2").
    pub codec_string: String,
    /// Media timescale from mdhd box.
    pub timescale: u32,
    /// Default KID from tenc box, if the track is encrypted.
    pub kid: Option<[u8; 16]>,
}

/// Maps track types to key IDs for per-track keying.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrackKeyMapping {
    entries: Vec<(TrackType, [u8; 16])>,
}

impl TrackKeyMapping {
    /// Create a mapping where all tracks use the same KID (backward compat).
    pub fn single(kid: [u8; 16]) -> Self {
        Self {
            entries: vec![
                (TrackType::Video, kid),
                (TrackType::Audio, kid),
                (TrackType::Subtitle, kid),
            ],
        }
    }

    /// Create a mapping from extracted track info.
    ///
    /// Uses each track's existing KID if present. When tracks share the same KID,
    /// only one entry per track type is stored.
    pub fn from_tracks(tracks: &[TrackInfo]) -> Self {
        let mut entries = Vec::new();
        let mut seen_types = Vec::new();

        for track in tracks {
            if let Some(kid) = track.kid {
                if !seen_types.contains(&track.track_type) {
                    entries.push((track.track_type, kid));
                    seen_types.push(track.track_type);
                }
            }
        }

        Self { entries }
    }

    /// Create a mapping with explicit per-type KIDs.
    pub fn per_type(video_kid: [u8; 16], audio_kid: [u8; 16]) -> Self {
        Self {
            entries: vec![
                (TrackType::Video, video_kid),
                (TrackType::Audio, audio_kid),
            ],
        }
    }

    /// Look up the KID for a given track type.
    ///
    /// Falls back to the first available KID if no specific mapping exists.
    pub fn kid_for_track(&self, track_type: TrackType) -> Option<&[u8; 16]> {
        self.entries
            .iter()
            .find(|(t, _)| *t == track_type)
            .map(|(_, kid)| kid)
            .or_else(|| self.entries.first().map(|(_, kid)| kid))
    }

    /// Return all unique KIDs in the mapping.
    pub fn all_kids(&self) -> Vec<[u8; 16]> {
        let mut kids = Vec::new();
        for (_, kid) in &self.entries {
            if !kids.contains(kid) {
                kids.push(*kid);
            }
        }
        kids
    }

    /// Return all entries in the mapping.
    pub fn entries(&self) -> &[(TrackType, [u8; 16])] {
        &self.entries
    }

    /// Whether this mapping has different KIDs for different track types.
    pub fn is_multi_key(&self) -> bool {
        let kids = self.all_kids();
        kids.len() > 1
    }
}

/// Extract track metadata from an init segment's moov box.
///
/// Parses moov → trak(s), extracting track ID, type, codec string,
/// timescale, and encryption key ID for each track.
pub fn extract_tracks(init_data: &[u8]) -> Result<Vec<TrackInfo>> {
    // Find moov box
    let moov_header = find_child_box(init_data, &box_type::MOOV)
        .ok_or_else(|| EdgepackError::MediaParse("no moov box found".into()))?;
    let moov_payload = box_payload(init_data, &moov_header);

    let mut tracks = Vec::new();

    // Iterate trak boxes inside moov
    for box_result in iterate_boxes(moov_payload) {
        let header = box_result?;
        if header.box_type != box_type::TRAK {
            continue;
        }

        let trak_payload = box_payload(moov_payload, &header);
        if let Some(info) = parse_trak(trak_payload)? {
            tracks.push(info);
        }
    }

    Ok(tracks)
}

/// Parse a single trak box and extract track metadata.
fn parse_trak(trak_payload: &[u8]) -> Result<Option<TrackInfo>> {
    // Extract track_id from tkhd
    let track_id = if let Some(tkhd) = find_child_box(trak_payload, &box_type::TKHD) {
        parse_tkhd_track_id(box_payload(trak_payload, &tkhd))
    } else {
        0
    };

    // Find mdia box
    let mdia_header = match find_child_box(trak_payload, &box_type::MDIA) {
        Some(h) => h,
        None => return Ok(None),
    };
    let mdia_payload = box_payload(trak_payload, &mdia_header);

    // Extract track type from hdlr
    let track_type = if let Some(hdlr) = find_child_box(mdia_payload, &box_type::HDLR) {
        parse_hdlr_track_type(box_payload(mdia_payload, &hdlr))
    } else {
        TrackType::Unknown
    };

    // Extract timescale from mdhd
    let timescale = if let Some(mdhd) = find_child_box(mdia_payload, &box_type::MDHD) {
        parse_mdhd_timescale(box_payload(mdia_payload, &mdhd))
    } else {
        0
    };

    // Navigate to stsd: mdia → minf → stbl → stsd
    let (codec_string, kid) = parse_codec_and_kid_from_mdia(mdia_payload)?;

    Ok(Some(TrackInfo {
        track_type,
        track_id,
        codec_string,
        timescale,
        kid,
    }))
}

/// Extract track ID from tkhd payload (after box header).
/// tkhd is a full box: version(1) + flags(3) + ...
fn parse_tkhd_track_id(payload: &[u8]) -> u32 {
    if payload.len() < 4 {
        return 0;
    }
    let version = payload[0];
    // Version 0: creation_time(4) + modification_time(4) + track_id(4) ...
    // Version 1: creation_time(8) + modification_time(8) + track_id(4) ...
    let track_id_offset = if version == 0 { 4 + 4 + 4 } else { 4 + 8 + 8 };
    if payload.len() < track_id_offset + 4 {
        return 0;
    }
    u32::from_be_bytes([
        payload[track_id_offset],
        payload[track_id_offset + 1],
        payload[track_id_offset + 2],
        payload[track_id_offset + 3],
    ])
}

/// Extract track type from hdlr payload (after box header).
/// hdlr is a full box: version(1) + flags(3) + pre_defined(4) + handler_type(4) ...
fn parse_hdlr_track_type(payload: &[u8]) -> TrackType {
    // version(1) + flags(3) + pre_defined(4) + handler_type(4)
    if payload.len() < 12 {
        return TrackType::Unknown;
    }
    let handler: [u8; 4] = [payload[8], payload[9], payload[10], payload[11]];
    TrackType::from_handler(&handler)
}

/// Extract timescale from mdhd payload (after box header).
/// mdhd is a full box: version(1) + flags(3) + ...
fn parse_mdhd_timescale(payload: &[u8]) -> u32 {
    if payload.len() < 4 {
        return 0;
    }
    let version = payload[0];
    // Version 0: creation_time(4) + modification_time(4) + timescale(4) ...
    // Version 1: creation_time(8) + modification_time(8) + timescale(4) ...
    let timescale_offset = if version == 0 { 4 + 4 + 4 } else { 4 + 8 + 8 };
    if payload.len() < timescale_offset + 4 {
        return 0;
    }
    u32::from_be_bytes([
        payload[timescale_offset],
        payload[timescale_offset + 1],
        payload[timescale_offset + 2],
        payload[timescale_offset + 3],
    ])
}

/// Navigate mdia → minf → stbl → stsd to extract codec string and KID.
fn parse_codec_and_kid_from_mdia(mdia_payload: &[u8]) -> Result<(String, Option<[u8; 16]>)> {
    let minf_header = match find_child_box(mdia_payload, &box_type::MINF) {
        Some(h) => h,
        None => return Ok((String::new(), None)),
    };
    let minf_payload = box_payload(mdia_payload, &minf_header);

    let stbl_header = match find_child_box(minf_payload, &box_type::STBL) {
        Some(h) => h,
        None => return Ok((String::new(), None)),
    };
    let stbl_payload = box_payload(minf_payload, &stbl_header);

    let stsd_header = match find_child_box(stbl_payload, &box_type::STSD) {
        Some(h) => h,
        None => return Ok((String::new(), None)),
    };
    let stsd_payload = box_payload(stbl_payload, &stsd_header);

    // stsd is a full box: version(1) + flags(3) + entry_count(4) + entries
    if stsd_payload.len() < 8 {
        return Ok((String::new(), None));
    }
    let entry_count = u32::from_be_bytes([
        stsd_payload[4],
        stsd_payload[5],
        stsd_payload[6],
        stsd_payload[7],
    ]);

    if entry_count == 0 {
        return Ok((String::new(), None));
    }

    // Parse first sample entry
    let entries_data = &stsd_payload[8..];
    if entries_data.len() < 8 {
        return Ok((String::new(), None));
    }

    let entry_header = read_box_header(entries_data, 0)?;
    let entry_payload = box_payload(entries_data, &entry_header);

    // Determine if this is an encrypted entry (encv/enca) and find sinf
    let (fourcc, kid) = if is_encrypted_entry(&entry_header.box_type) {
        let kid = find_kid_in_entry(entry_payload);
        let original_format = find_original_format_in_entry(entry_payload);
        (original_format.unwrap_or(entry_header.box_type), kid)
    } else {
        (entry_header.box_type, None)
    };

    let codec_string = extract_codec_string(&fourcc, entry_payload);
    Ok((codec_string, kid))
}

/// Check if a sample entry FourCC is an encrypted type.
fn is_encrypted_entry(fourcc: &[u8; 4]) -> bool {
    matches!(fourcc, b"encv" | b"enca" | b"enct" | b"encs")
}

/// Find the KID from a sinf → schi → tenc box within a sample entry's payload.
fn find_kid_in_entry(entry_payload: &[u8]) -> Option<[u8; 16]> {
    // Scan for sinf box
    let mut pos = 0;
    while pos + 8 <= entry_payload.len() {
        if &entry_payload[pos + 4..pos + 8] == &box_type::SINF {
            let sinf_size = u32::from_be_bytes([
                entry_payload[pos],
                entry_payload[pos + 1],
                entry_payload[pos + 2],
                entry_payload[pos + 3],
            ]) as usize;

            if sinf_size >= 8 && pos + sinf_size <= entry_payload.len() {
                let sinf_inner = &entry_payload[pos + 8..pos + sinf_size];
                // Find schi inside sinf
                if let Some(schi) = find_child_box(sinf_inner, &box_type::SCHI) {
                    let schi_payload = box_payload(sinf_inner, &schi);
                    // Find tenc inside schi
                    if let Some(tenc_header) = find_child_box(schi_payload, &box_type::TENC) {
                        let tenc_payload = box_payload(schi_payload, &tenc_header);
                        if let Ok(tenc) = parse_tenc(tenc_payload) {
                            return Some(tenc.default_kid);
                        }
                    }
                }
            }
            break;
        }
        pos += 1;
    }
    None
}

/// Find the original format FourCC from sinf → frma inside a sample entry.
fn find_original_format_in_entry(entry_payload: &[u8]) -> Option<[u8; 4]> {
    let mut pos = 0;
    while pos + 8 <= entry_payload.len() {
        if &entry_payload[pos + 4..pos + 8] == &box_type::SINF {
            let sinf_size = u32::from_be_bytes([
                entry_payload[pos],
                entry_payload[pos + 1],
                entry_payload[pos + 2],
                entry_payload[pos + 3],
            ]) as usize;

            if sinf_size >= 8 && pos + sinf_size <= entry_payload.len() {
                let sinf_inner = &entry_payload[pos + 8..pos + sinf_size];
                if let Some(frma) = find_child_box(sinf_inner, &box_type::FRMA) {
                    let frma_payload = box_payload(sinf_inner, &frma);
                    if frma_payload.len() >= 4 {
                        return Some([
                            frma_payload[0],
                            frma_payload[1],
                            frma_payload[2],
                            frma_payload[3],
                        ]);
                    }
                }
            }
            break;
        }
        pos += 1;
    }
    None
}

/// Extract an RFC 6381 codec string from a sample entry.
///
/// Supported codecs:
/// - H.264: `avc1.{profile}{constraint}{level}` from avcC
/// - HEVC: `hev1.{profile}.{tier}{level}.{constraint}` from hvcC
/// - AAC: `mp4a.40.{audioObjectType}` from esds
/// - VP9: `vp09.{profile}.{level}.{bitDepth}` from vpcC
/// - AV1: `av01.{profile}.{level}{tier}.{bitDepth}` from av1C
/// - AC-3, E-AC-3, Opus, FLAC: simple string
fn extract_codec_string(fourcc: &[u8; 4], entry_payload: &[u8]) -> String {
    match fourcc {
        b"avc1" | b"avc3" => extract_avc_codec(fourcc, entry_payload),
        b"hev1" | b"hvc1" => extract_hevc_codec(fourcc, entry_payload),
        b"mp4a" => extract_aac_codec(entry_payload),
        b"vp09" => extract_vp9_codec(entry_payload),
        b"av01" => extract_av1_codec(entry_payload),
        b"ac-3" => "ac-3".to_string(),
        b"ec-3" => "ec-3".to_string(),
        b"Opus" => "opus".to_string(),
        b"fLaC" => "flac".to_string(),
        _ => String::from_utf8_lossy(fourcc).to_string(),
    }
}

/// Extract H.264 codec string from avcC box.
/// Format: `avc1.{profile_idc:02x}{constraint_set_flags:02x}{level_idc:02x}`
fn extract_avc_codec(fourcc: &[u8; 4], entry_payload: &[u8]) -> String {
    let prefix = std::str::from_utf8(fourcc).unwrap_or("avc1");
    if let Some(avcc) = find_config_box(entry_payload, b"avcC") {
        // avcC: configurationVersion(1) + AVCProfileIndication(1)
        //     + profile_compatibility(1) + AVCLevelIndication(1) ...
        if avcc.len() >= 4 {
            return format!(
                "{}.{:02x}{:02x}{:02x}",
                prefix, avcc[1], avcc[2], avcc[3]
            );
        }
    }
    prefix.to_string()
}

/// Extract HEVC codec string from hvcC box.
/// Format: `hev1.{general_profile_space?}{general_profile_idc}.{general_profile_compatibility_flags:X}.{general_tier_flag}{general_level_idc}.{constraint_indicator}`
fn extract_hevc_codec(fourcc: &[u8; 4], entry_payload: &[u8]) -> String {
    let prefix = std::str::from_utf8(fourcc).unwrap_or("hev1");
    if let Some(hvcc) = find_config_box(entry_payload, b"hvcC") {
        // HEVCDecoderConfigurationRecord:
        // byte 0: configurationVersion
        // byte 1: general_profile_space(2) | general_tier_flag(1) | general_profile_idc(5)
        // bytes 2-5: general_profile_compatibility_flags (32 bits)
        // bytes 6-11: general_constraint_indicator_flags (48 bits)
        // byte 12: general_level_idc
        if hvcc.len() >= 13 {
            let profile_space_tier = hvcc[1];
            let profile_space = (profile_space_tier >> 6) & 0x03;
            let tier_flag = (profile_space_tier >> 5) & 0x01;
            let profile_idc = profile_space_tier & 0x1F;

            let compat_flags = u32::from_be_bytes([hvcc[2], hvcc[3], hvcc[4], hvcc[5]]);
            let level_idc = hvcc[12];

            let tier_char = if tier_flag == 1 { 'H' } else { 'L' };
            let space_prefix = match profile_space {
                1 => "A",
                2 => "B",
                3 => "C",
                _ => "",
            };

            // Build constraint indicator string (trailing zero bytes omitted)
            let constraint_bytes = &hvcc[6..12];
            let constraint_str = build_hevc_constraint_string(constraint_bytes);

            return format!(
                "{}.{}{}.{:X}.{}{}.{}",
                prefix,
                space_prefix,
                profile_idc,
                compat_flags,
                tier_char,
                level_idc,
                constraint_str,
            );
        }
    }
    prefix.to_string()
}

/// Build HEVC constraint indicator string from 6 bytes.
/// Each byte is hex-encoded, trailing zero bytes are omitted, separated by dots.
fn build_hevc_constraint_string(bytes: &[u8]) -> String {
    // Find last non-zero byte
    let last_nonzero = bytes.iter().rposition(|&b| b != 0).map(|i| i + 1).unwrap_or(0);
    if last_nonzero == 0 {
        return String::new();
    }
    bytes[..last_nonzero]
        .iter()
        .map(|b| format!("{:X}", b))
        .collect::<Vec<_>>()
        .join(".")
}

/// Extract AAC codec string from esds box.
/// Format: `mp4a.40.{audioObjectType}`
fn extract_aac_codec(entry_payload: &[u8]) -> String {
    if let Some(esds) = find_config_box(entry_payload, b"esds") {
        if let Some(aot) = parse_esds_audio_object_type(esds) {
            return format!("mp4a.40.{}", aot);
        }
    }
    "mp4a.40.2".to_string() // Default to AAC-LC
}

/// Extract VP9 codec string from vpcC box.
/// Format: `vp09.{profile:02}.{level:02}.{bitDepth:02}`
fn extract_vp9_codec(entry_payload: &[u8]) -> String {
    if let Some(vpcc) = find_config_box(entry_payload, b"vpcC") {
        // VPCodecConfigurationBox:
        // byte 0: version (should be 1)
        // (version 1): byte 4: profile, byte 5: level, byte 6: bitDepth(4)|chromaSubsampling(3)|videoFullRangeFlag(1)
        if vpcc.len() >= 8 {
            let profile = vpcc[4];
            let level = vpcc[5];
            let bit_depth = (vpcc[6] >> 4) & 0x0F;
            return format!("vp09.{:02}.{:02}.{:02}", profile, level, bit_depth);
        }
    }
    "vp09".to_string()
}

/// Extract AV1 codec string from av1C box.
/// Format: `av01.{profile}.{level:02}{tier}.{bitDepth:02}`
fn extract_av1_codec(entry_payload: &[u8]) -> String {
    if let Some(av1c) = find_config_box(entry_payload, b"av1C") {
        // AV1CodecConfigurationRecord:
        // byte 0: marker(1) | version(7) — should be (1, 1)
        // byte 1: seq_profile(3) | seq_level_idx_0(5)
        // byte 2: seq_tier_0(1) | high_bitdepth(1) | twelve_bit(1) | ...
        if av1c.len() >= 4 {
            let profile = (av1c[1] >> 5) & 0x07;
            let level = av1c[1] & 0x1F;
            let tier = (av1c[2] >> 7) & 0x01;
            let high_bitdepth = (av1c[2] >> 6) & 0x01;
            let twelve_bit = (av1c[2] >> 5) & 0x01;
            let bit_depth = if high_bitdepth == 1 {
                if twelve_bit == 1 { 12 } else { 10 }
            } else {
                8
            };
            let tier_char = if tier == 1 { 'H' } else { 'M' };
            return format!(
                "av01.{}.{:02}{}.{:02}",
                profile, level, tier_char, bit_depth
            );
        }
    }
    "av01".to_string()
}

/// Parse audio object type from an esds box payload.
///
/// The esds box contains nested descriptors. We look for the DecoderConfigDescriptor
/// and extract the audio object type from the AudioSpecificConfig.
fn parse_esds_audio_object_type(esds: &[u8]) -> Option<u8> {
    // esds payload: version(4) + ES_Descriptor
    // ES_Descriptor tag = 0x03, then size, then ESID(2) + streamPriority(1)
    //   + DecoderConfigDescriptor (tag = 0x04, then size, then objectTypeIndication(1) + ...)
    //     + DecoderSpecificInfo (tag = 0x05, then size, then AudioSpecificConfig)
    //
    // AudioSpecificConfig: first 5 bits = audioObjectType
    if esds.len() < 5 {
        return None;
    }

    // Skip version/flags (4 bytes)
    let data = &esds[4..];

    // Find tag 0x05 (DecoderSpecificInfo) by scanning
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x05 {
            // Parse size (variable-length)
            let (size, size_bytes) = parse_descriptor_size(&data[i + 1..]);
            let config_start = i + 1 + size_bytes;
            if config_start < data.len() && size > 0 {
                // First 5 bits of AudioSpecificConfig = audioObjectType
                let aot = (data[config_start] >> 3) & 0x1F;
                if aot == 31 {
                    // Extended audio object type
                    if config_start + 1 < data.len() {
                        let ext = ((data[config_start] & 0x07) << 3) | (data[config_start + 1] >> 5);
                        return Some(32 + ext);
                    }
                }
                return Some(aot);
            }
        }
        i += 1;
    }

    None
}

/// Parse variable-length descriptor size (up to 4 bytes, each with continuation bit).
fn parse_descriptor_size(data: &[u8]) -> (usize, usize) {
    let mut size = 0usize;
    let mut bytes_read = 0;
    for &byte in data.iter().take(4) {
        bytes_read += 1;
        size = (size << 7) | (byte & 0x7F) as usize;
        if byte & 0x80 == 0 {
            break;
        }
    }
    (size, bytes_read)
}

/// Find a codec configuration box (avcC, hvcC, vpcC, av1C, esds) within a sample entry.
///
/// The sample entry has a format-dependent fixed prefix before child boxes.
/// We scan for the target FourCC pattern.
fn find_config_box<'a>(entry_payload: &'a [u8], config_type: &[u8; 4]) -> Option<&'a [u8]> {
    let mut pos = 0;
    while pos + 8 <= entry_payload.len() {
        if &entry_payload[pos + 4..pos + 8] == config_type {
            let box_size = u32::from_be_bytes([
                entry_payload[pos],
                entry_payload[pos + 1],
                entry_payload[pos + 2],
                entry_payload[pos + 3],
            ]) as usize;

            if box_size >= 8 && pos + box_size <= entry_payload.len() {
                return Some(&entry_payload[pos + 8..pos + box_size]);
            }
        }
        pos += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    // ---- Helpers for building synthetic MP4 structures ----

    /// Build a minimal box with header + payload.
    fn make_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = 8 + payload.len() as u32;
        let mut data = Vec::new();
        data.extend_from_slice(&size.to_be_bytes());
        data.extend_from_slice(box_type);
        data.extend_from_slice(payload);
        data
    }

    /// Build a tkhd box with a given track ID (version 0).
    fn make_tkhd(track_id: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        // version(1) + flags(3)
        payload.extend_from_slice(&[0u8; 4]);
        // creation_time(4) + modification_time(4)
        payload.extend_from_slice(&[0u8; 8]);
        // track_id(4)
        payload.extend_from_slice(&track_id.to_be_bytes());
        // remaining tkhd fields (reserved, duration, etc.)
        payload.extend_from_slice(&[0u8; 60]);
        make_box(b"tkhd", &payload)
    }

    /// Build an hdlr box with a given handler type.
    fn make_hdlr(handler: &[u8; 4]) -> Vec<u8> {
        let mut payload = Vec::new();
        // version(1) + flags(3)
        payload.extend_from_slice(&[0u8; 4]);
        // pre_defined(4)
        payload.extend_from_slice(&[0u8; 4]);
        // handler_type(4)
        payload.extend_from_slice(handler);
        // reserved(12) + name(variable)
        payload.extend_from_slice(&[0u8; 12]);
        payload.push(0); // null terminator for name
        make_box(b"hdlr", &payload)
    }

    /// Build an mdhd box with a given timescale (version 0).
    fn make_mdhd(timescale: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        // version(1) + flags(3)
        payload.extend_from_slice(&[0u8; 4]);
        // creation_time(4) + modification_time(4)
        payload.extend_from_slice(&[0u8; 8]);
        // timescale(4)
        payload.extend_from_slice(&timescale.to_be_bytes());
        // duration(4) + language(2) + pre_defined(2)
        payload.extend_from_slice(&[0u8; 8]);
        make_box(b"mdhd", &payload)
    }

    /// Build a video sample entry with an avcC config box.
    fn make_avc1_entry(profile: u8, constraint: u8, level: u8) -> Vec<u8> {
        let mut payload = Vec::new();
        // Video sample entry fixed fields:
        // reserved(6) + data_ref_index(2) + pre_defined(2) + reserved(2)
        // + pre_defined(12) + width(2) + height(2) + horiz_res(4) + vert_res(4)
        // + reserved(4) + frame_count(2) + compressor_name(32) + depth(2) + pre_defined(2)
        // Total: 70 bytes
        payload.extend_from_slice(&[0u8; 70]);

        // avcC box: configurationVersion(1) + profileIdc(1) + constraintFlags(1) + levelIdc(1) + rest
        let mut avcc_payload = vec![1u8]; // configurationVersion
        avcc_payload.push(profile);
        avcc_payload.push(constraint);
        avcc_payload.push(level);
        avcc_payload.extend_from_slice(&[0u8; 4]); // remaining avcC fields

        let avcc = make_box(b"avcC", &avcc_payload);
        payload.extend_from_slice(&avcc);

        make_box(b"avc1", &payload)
    }

    /// Build an audio sample entry with an esds box.
    fn make_mp4a_entry(audio_object_type: u8) -> Vec<u8> {
        let mut payload = Vec::new();
        // Audio sample entry fixed fields:
        // reserved(6) + data_ref_index(2) + reserved(8) + channel_count(2)
        // + sample_size(2) + pre_defined(2) + reserved(2) + sample_rate(4)
        // Total: 28 bytes
        payload.extend_from_slice(&[0u8; 28]);

        // esds box with embedded AudioSpecificConfig
        let mut esds_payload = Vec::new();
        // version(4)
        esds_payload.extend_from_slice(&[0u8; 4]);
        // ES_Descriptor: tag(1) + size(1) + ES_ID(2) + flags(1)
        esds_payload.push(0x03); // ES_Descriptor tag
        esds_payload.push(23); // size
        esds_payload.extend_from_slice(&[0, 1]); // ES_ID
        esds_payload.push(0); // streamDependenceFlag, etc.
        // DecoderConfigDescriptor: tag(1) + size(1) + objectTypeIndication(1) + streamType(1)
        //   + bufferSizeDB(3) + maxBitrate(4) + avgBitrate(4)
        esds_payload.push(0x04); // DecoderConfigDescriptor tag
        esds_payload.push(15); // size
        esds_payload.push(0x40); // objectTypeIndication = Audio ISO/IEC 14496-3
        esds_payload.push(0x15); // streamType = audio
        esds_payload.extend_from_slice(&[0u8; 3]); // bufferSizeDB
        esds_payload.extend_from_slice(&[0u8; 4]); // maxBitrate
        esds_payload.extend_from_slice(&[0u8; 4]); // avgBitrate
        // DecoderSpecificInfo: tag(1) + size(1) + AudioSpecificConfig
        esds_payload.push(0x05); // DecoderSpecificInfo tag
        esds_payload.push(2); // size
        // AudioSpecificConfig: first 5 bits = audioObjectType, next 4 = frequency index
        // AOT in bits: aot << 3 | freq_index >> 1
        let asc_byte1 = (audio_object_type << 3) | 0x03; // freq_index = 4 (44100) -> 0b0100 >> 1 = 0b010 -> 3? Let's just put 3
        let asc_byte2 = 0x90; // freq_index low bit(1) + channel_config(4) + padding
        esds_payload.push(asc_byte1);
        esds_payload.push(asc_byte2);

        let esds = make_box(b"esds", &esds_payload);
        payload.extend_from_slice(&esds);

        make_box(b"mp4a", &payload)
    }

    /// Build a stsd box wrapping a sample entry.
    fn make_stsd(entry: &[u8]) -> Vec<u8> {
        let mut payload = Vec::new();
        // version(1) + flags(3)
        payload.extend_from_slice(&[0u8; 4]);
        // entry_count(4)
        payload.extend_from_slice(&1u32.to_be_bytes());
        // entry
        payload.extend_from_slice(entry);
        make_box(b"stsd", &payload)
    }

    /// Build a full moov structure with one track: trak { tkhd, mdia { mdhd, hdlr, minf { stbl { stsd } } } }
    fn make_moov_single_track(
        track_id: u32,
        handler: &[u8; 4],
        timescale: u32,
        entry: &[u8],
    ) -> Vec<u8> {
        let stsd = make_stsd(entry);
        let stbl = make_box(b"stbl", &stsd);
        let minf = make_box(b"minf", &stbl);
        let mdhd = make_mdhd(timescale);
        let hdlr = make_hdlr(handler);
        let mut mdia_children = Vec::new();
        mdia_children.extend_from_slice(&mdhd);
        mdia_children.extend_from_slice(&hdlr);
        mdia_children.extend_from_slice(&minf);
        let mdia = make_box(b"mdia", &mdia_children);
        let tkhd = make_tkhd(track_id);
        let mut trak_children = Vec::new();
        trak_children.extend_from_slice(&tkhd);
        trak_children.extend_from_slice(&mdia);
        let trak = make_box(b"trak", &trak_children);
        make_box(b"moov", &trak)
    }

    /// Build an init segment: ftyp + moov
    fn make_init(moov: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        let ftyp = make_box(b"ftyp", b"isom\x00\x00\x02\x00");
        data.extend_from_slice(&ftyp);
        data.extend_from_slice(moov);
        data
    }

    // ---- TrackKeyMapping tests ----

    #[test]
    fn track_key_mapping_single_returns_same_kid_for_all() {
        let kid = [0x01; 16];
        let mapping = TrackKeyMapping::single(kid);
        assert_eq!(mapping.kid_for_track(TrackType::Video), Some(&kid));
        assert_eq!(mapping.kid_for_track(TrackType::Audio), Some(&kid));
        assert_eq!(mapping.kid_for_track(TrackType::Subtitle), Some(&kid));
        assert!(!mapping.is_multi_key());
        assert_eq!(mapping.all_kids(), vec![kid]);
    }

    #[test]
    fn track_key_mapping_per_type_returns_correct_kids() {
        let video_kid = [0xAA; 16];
        let audio_kid = [0xBB; 16];
        let mapping = TrackKeyMapping::per_type(video_kid, audio_kid);
        assert_eq!(mapping.kid_for_track(TrackType::Video), Some(&video_kid));
        assert_eq!(mapping.kid_for_track(TrackType::Audio), Some(&audio_kid));
        assert!(mapping.is_multi_key());
        let kids = mapping.all_kids();
        assert_eq!(kids.len(), 2);
        assert!(kids.contains(&video_kid));
        assert!(kids.contains(&audio_kid));
    }

    #[test]
    fn track_key_mapping_fallback_to_first() {
        let video_kid = [0xAA; 16];
        let audio_kid = [0xBB; 16];
        let mapping = TrackKeyMapping::per_type(video_kid, audio_kid);
        // Subtitle isn't mapped — should fall back to first entry (Video)
        assert_eq!(mapping.kid_for_track(TrackType::Subtitle), Some(&video_kid));
    }

    #[test]
    fn track_key_mapping_from_tracks_empty() {
        let mapping = TrackKeyMapping::from_tracks(&[]);
        assert!(mapping.entries.is_empty());
        assert!(mapping.all_kids().is_empty());
    }

    #[test]
    fn track_key_mapping_from_tracks_with_kids() {
        let tracks = vec![
            TrackInfo {
                track_type: TrackType::Video,
                track_id: 1,
                codec_string: "avc1.64001f".to_string(),
                timescale: 90000,
                kid: Some([0xAA; 16]),
            },
            TrackInfo {
                track_type: TrackType::Audio,
                track_id: 2,
                codec_string: "mp4a.40.2".to_string(),
                timescale: 44100,
                kid: Some([0xBB; 16]),
            },
        ];
        let mapping = TrackKeyMapping::from_tracks(&tracks);
        assert_eq!(mapping.kid_for_track(TrackType::Video), Some(&[0xAA; 16]));
        assert_eq!(mapping.kid_for_track(TrackType::Audio), Some(&[0xBB; 16]));
        assert!(mapping.is_multi_key());
    }

    #[test]
    fn track_key_mapping_from_tracks_no_kids() {
        let tracks = vec![TrackInfo {
            track_type: TrackType::Video,
            track_id: 1,
            codec_string: "avc1.64001f".to_string(),
            timescale: 90000,
            kid: None,
        }];
        let mapping = TrackKeyMapping::from_tracks(&tracks);
        assert!(mapping.entries.is_empty());
    }

    // ---- tkhd parsing ----

    #[test]
    fn parse_tkhd_track_id_v0() {
        let tkhd = make_tkhd(42);
        // tkhd box: header(8) + payload
        let payload = &tkhd[8..];
        assert_eq!(parse_tkhd_track_id(payload), 42);
    }

    #[test]
    fn parse_tkhd_track_id_v1() {
        let mut payload = Vec::new();
        // version 1 + flags(3)
        payload.push(1);
        payload.extend_from_slice(&[0u8; 3]);
        // creation_time(8) + modification_time(8)
        payload.extend_from_slice(&[0u8; 16]);
        // track_id(4)
        payload.extend_from_slice(&99u32.to_be_bytes());
        payload.extend_from_slice(&[0u8; 60]);
        assert_eq!(parse_tkhd_track_id(&payload), 99);
    }

    // ---- hdlr parsing ----

    #[test]
    fn parse_hdlr_video() {
        let hdlr = make_hdlr(b"vide");
        let payload = &hdlr[8..];
        assert_eq!(parse_hdlr_track_type(payload), TrackType::Video);
    }

    #[test]
    fn parse_hdlr_audio() {
        let hdlr = make_hdlr(b"soun");
        let payload = &hdlr[8..];
        assert_eq!(parse_hdlr_track_type(payload), TrackType::Audio);
    }

    #[test]
    fn parse_hdlr_too_short() {
        assert_eq!(parse_hdlr_track_type(&[0u8; 4]), TrackType::Unknown);
    }

    // ---- mdhd parsing ----

    #[test]
    fn parse_mdhd_timescale_v0() {
        let mdhd = make_mdhd(90000);
        let payload = &mdhd[8..];
        assert_eq!(parse_mdhd_timescale(payload), 90000);
    }

    #[test]
    fn parse_mdhd_timescale_v1() {
        let mut payload = Vec::new();
        payload.push(1); // version 1
        payload.extend_from_slice(&[0u8; 3]); // flags
        payload.extend_from_slice(&[0u8; 16]); // creation_time(8) + modification_time(8)
        payload.extend_from_slice(&44100u32.to_be_bytes()); // timescale
        payload.extend_from_slice(&[0u8; 12]); // remaining
        assert_eq!(parse_mdhd_timescale(&payload), 44100);
    }

    #[test]
    fn parse_mdhd_too_short() {
        assert_eq!(parse_mdhd_timescale(&[0u8; 4]), 0);
    }

    // ---- H.264 codec string ----

    #[test]
    fn extract_avc_codec_string() {
        let entry = make_avc1_entry(0x64, 0x00, 0x1f);
        let entry_payload = &entry[8..]; // skip box header
        let codec = extract_codec_string(b"avc1", entry_payload);
        assert_eq!(codec, "avc1.64001f");
    }

    #[test]
    fn extract_avc_codec_baseline() {
        let entry = make_avc1_entry(0x42, 0xC0, 0x1E);
        let entry_payload = &entry[8..];
        let codec = extract_codec_string(b"avc1", entry_payload);
        assert_eq!(codec, "avc1.42c01e");
    }

    #[test]
    fn extract_avc_codec_no_avcc_fallback() {
        let payload = vec![0u8; 70]; // video entry prefix, no avcC
        let codec = extract_codec_string(b"avc1", &payload);
        assert_eq!(codec, "avc1");
    }

    // ---- AAC codec string ----

    #[test]
    fn extract_aac_codec_string_lc() {
        let entry = make_mp4a_entry(2); // AAC-LC = AOT 2
        let entry_payload = &entry[8..];
        let codec = extract_codec_string(b"mp4a", entry_payload);
        assert_eq!(codec, "mp4a.40.2");
    }

    #[test]
    fn extract_aac_codec_string_he() {
        let entry = make_mp4a_entry(5); // HE-AAC = AOT 5
        let entry_payload = &entry[8..];
        let codec = extract_codec_string(b"mp4a", entry_payload);
        assert_eq!(codec, "mp4a.40.5");
    }

    #[test]
    fn extract_aac_codec_no_esds_default() {
        let payload = vec![0u8; 28]; // audio entry prefix, no esds
        let codec = extract_codec_string(b"mp4a", &payload);
        assert_eq!(codec, "mp4a.40.2"); // defaults to AAC-LC
    }

    // ---- Simple codec strings ----

    #[test]
    fn extract_simple_codec_strings() {
        assert_eq!(extract_codec_string(b"ac-3", &[]), "ac-3");
        assert_eq!(extract_codec_string(b"ec-3", &[]), "ec-3");
        assert_eq!(extract_codec_string(b"Opus", &[]), "opus");
        assert_eq!(extract_codec_string(b"fLaC", &[]), "flac");
    }

    // ---- VP9 codec string ----

    #[test]
    fn extract_vp9_codec_string() {
        // Build a vpcC box inside a dummy entry payload
        let mut vpcc_payload = Vec::new();
        vpcc_payload.push(1); // version
        vpcc_payload.extend_from_slice(&[0u8; 3]); // flags
        vpcc_payload.push(0); // profile = 0
        vpcc_payload.push(31); // level = 3.1
        vpcc_payload.push(0x80); // bitDepth = 8, chromaSubsampling = 0, fullRange = 0
        vpcc_payload.push(0); // padding
        let vpcc = make_box(b"vpcC", &vpcc_payload);

        let mut entry_payload = vec![0u8; 70]; // video entry prefix
        entry_payload.extend_from_slice(&vpcc);

        let codec = extract_codec_string(b"vp09", &entry_payload);
        assert_eq!(codec, "vp09.00.31.08");
    }

    // ---- AV1 codec string ----

    #[test]
    fn extract_av1_codec_string() {
        // Build an av1C box
        let mut av1c_payload = Vec::new();
        av1c_payload.push(0x81); // marker(1) | version(7) = (1, 1)
        av1c_payload.push(0x04); // seq_profile(3)=0 | seq_level_idx_0(5)=4
        av1c_payload.push(0x00); // tier=0 | high_bitdepth=0 | twelve_bit=0 ...
        av1c_payload.push(0x00);
        let av1c = make_box(b"av1C", &av1c_payload);

        let mut entry_payload = vec![0u8; 70];
        entry_payload.extend_from_slice(&av1c);

        let codec = extract_codec_string(b"av01", &entry_payload);
        assert_eq!(codec, "av01.0.04M.08");
    }

    #[test]
    fn extract_av1_codec_string_10bit() {
        let mut av1c_payload = Vec::new();
        av1c_payload.push(0x81);
        av1c_payload.push(0x0D); // profile=0 | level=13
        av1c_payload.push(0x40); // tier=0 | high_bitdepth=1 | twelve_bit=0
        av1c_payload.push(0x00);
        let av1c = make_box(b"av1C", &av1c_payload);

        let mut entry_payload = vec![0u8; 70];
        entry_payload.extend_from_slice(&av1c);

        let codec = extract_codec_string(b"av01", &entry_payload);
        assert_eq!(codec, "av01.0.13M.10");
    }

    // ---- HEVC constraint string ----

    #[test]
    fn hevc_constraint_string_trailing_zeros_omitted() {
        assert_eq!(build_hevc_constraint_string(&[0xB0, 0, 0, 0, 0, 0]), "B0");
        assert_eq!(build_hevc_constraint_string(&[0x90, 0, 0, 0, 0, 0]), "90");
    }

    #[test]
    fn hevc_constraint_string_multiple_bytes() {
        assert_eq!(build_hevc_constraint_string(&[0xB0, 0x01, 0, 0, 0, 0]), "B0.1");
    }

    #[test]
    fn hevc_constraint_string_all_zero() {
        assert_eq!(build_hevc_constraint_string(&[0, 0, 0, 0, 0, 0]), "");
    }

    // ---- extract_tracks integration ----

    #[test]
    fn extract_tracks_single_video() {
        let entry = make_avc1_entry(0x64, 0x00, 0x1f);
        let moov = make_moov_single_track(1, b"vide", 90000, &entry);
        let init = make_init(&moov);

        let tracks = extract_tracks(&init).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_type, TrackType::Video);
        assert_eq!(tracks[0].track_id, 1);
        assert_eq!(tracks[0].codec_string, "avc1.64001f");
        assert_eq!(tracks[0].timescale, 90000);
        assert!(tracks[0].kid.is_none());
    }

    #[test]
    fn extract_tracks_single_audio() {
        let entry = make_mp4a_entry(2);
        let moov = make_moov_single_track(2, b"soun", 44100, &entry);
        let init = make_init(&moov);

        let tracks = extract_tracks(&init).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_type, TrackType::Audio);
        assert_eq!(tracks[0].track_id, 2);
        assert_eq!(tracks[0].codec_string, "mp4a.40.2");
        assert_eq!(tracks[0].timescale, 44100);
    }

    #[test]
    fn extract_tracks_multi_track() {
        let video_entry = make_avc1_entry(0x64, 0x00, 0x1f);
        let audio_entry = make_mp4a_entry(2);

        // Build two trak boxes
        let stsd_v = make_stsd(&video_entry);
        let stbl_v = make_box(b"stbl", &stsd_v);
        let minf_v = make_box(b"minf", &stbl_v);
        let mdhd_v = make_mdhd(90000);
        let hdlr_v = make_hdlr(b"vide");
        let mut mdia_v_children = Vec::new();
        mdia_v_children.extend_from_slice(&mdhd_v);
        mdia_v_children.extend_from_slice(&hdlr_v);
        mdia_v_children.extend_from_slice(&minf_v);
        let mdia_v = make_box(b"mdia", &mdia_v_children);
        let tkhd_v = make_tkhd(1);
        let mut trak_v_children = Vec::new();
        trak_v_children.extend_from_slice(&tkhd_v);
        trak_v_children.extend_from_slice(&mdia_v);
        let trak_v = make_box(b"trak", &trak_v_children);

        let stsd_a = make_stsd(&audio_entry);
        let stbl_a = make_box(b"stbl", &stsd_a);
        let minf_a = make_box(b"minf", &stbl_a);
        let mdhd_a = make_mdhd(44100);
        let hdlr_a = make_hdlr(b"soun");
        let mut mdia_a_children = Vec::new();
        mdia_a_children.extend_from_slice(&mdhd_a);
        mdia_a_children.extend_from_slice(&hdlr_a);
        mdia_a_children.extend_from_slice(&minf_a);
        let mdia_a = make_box(b"mdia", &mdia_a_children);
        let tkhd_a = make_tkhd(2);
        let mut trak_a_children = Vec::new();
        trak_a_children.extend_from_slice(&tkhd_a);
        trak_a_children.extend_from_slice(&mdia_a);
        let trak_a = make_box(b"trak", &trak_a_children);

        let mut moov_children = Vec::new();
        moov_children.extend_from_slice(&trak_v);
        moov_children.extend_from_slice(&trak_a);
        let moov = make_box(b"moov", &moov_children);
        let init = make_init(&moov);

        let tracks = extract_tracks(&init).unwrap();
        assert_eq!(tracks.len(), 2);

        let video = tracks.iter().find(|t| t.track_type == TrackType::Video).unwrap();
        assert_eq!(video.track_id, 1);
        assert_eq!(video.codec_string, "avc1.64001f");
        assert_eq!(video.timescale, 90000);

        let audio = tracks.iter().find(|t| t.track_type == TrackType::Audio).unwrap();
        assert_eq!(audio.track_id, 2);
        assert_eq!(audio.codec_string, "mp4a.40.2");
        assert_eq!(audio.timescale, 44100);
    }

    #[test]
    fn extract_tracks_encrypted_entry_with_kid() {
        // Build an encrypted video entry: encv { video_data, sinf { frma, schm, schi { tenc } } }
        let mut entry_payload = vec![0u8; 70]; // video sample entry prefix

        // avcC config
        let avcc_payload = vec![1u8, 0x64, 0x00, 0x1f, 0, 0, 0, 0];
        let avcc = make_box(b"avcC", &avcc_payload);
        entry_payload.extend_from_slice(&avcc);

        // sinf box
        let kid = [0xCC; 16];
        let mut sinf_children = Vec::new();
        // frma
        sinf_children.extend_from_slice(&make_box(b"frma", b"avc1"));
        // schm
        let mut schm_payload = vec![0u8; 4]; // version + flags
        schm_payload.extend_from_slice(b"cenc");
        schm_payload.extend_from_slice(&0x00010000u32.to_be_bytes());
        sinf_children.extend_from_slice(&make_box(b"schm", &schm_payload));
        // schi { tenc }
        let mut tenc_payload = Vec::new();
        tenc_payload.push(0); // version
        tenc_payload.extend_from_slice(&[0u8; 3]); // flags
        tenc_payload.push(0); // crypt_skip
        tenc_payload.push(1); // isProtected
        tenc_payload.push(8); // ivSize
        tenc_payload.extend_from_slice(&kid);
        let tenc = make_box(b"tenc", &tenc_payload);
        let schi = make_box(b"schi", &tenc);
        sinf_children.extend_from_slice(&schi);

        let sinf = make_box(b"sinf", &sinf_children);
        entry_payload.extend_from_slice(&sinf);

        let entry = make_box(b"encv", &entry_payload);
        let moov = make_moov_single_track(1, b"vide", 90000, &entry);
        let init = make_init(&moov);

        let tracks = extract_tracks(&init).unwrap();
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].track_type, TrackType::Video);
        assert_eq!(tracks[0].codec_string, "avc1.64001f");
        assert_eq!(tracks[0].kid, Some(kid));
    }

    #[test]
    fn extract_tracks_no_moov_errors() {
        let data = make_box(b"ftyp", b"isom\x00\x00\x02\x00");
        let result = extract_tracks(&data);
        assert!(result.is_err());
    }

    // ---- descriptor size parsing ----

    #[test]
    fn parse_descriptor_size_single_byte() {
        let (size, bytes) = parse_descriptor_size(&[0x15]);
        assert_eq!(size, 21);
        assert_eq!(bytes, 1);
    }

    #[test]
    fn parse_descriptor_size_multi_byte() {
        // 0x80 | 0x01 = continuation, then 0x00 = 0x80 = 128
        let (size, bytes) = parse_descriptor_size(&[0x81, 0x00]);
        assert_eq!(size, 128);
        assert_eq!(bytes, 2);
    }
}
