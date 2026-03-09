use crate::error::{EdgepackError, Result};
use crate::media::FourCC;
use serde::{Deserialize, Serialize};

/// An ISOBMFF box header.
#[derive(Debug, Clone)]
pub struct BoxHeader {
    /// Total size of the box including the header.
    pub size: u64,
    /// Four-character box type code.
    pub box_type: FourCC,
    /// Offset of this box within the input data.
    pub offset: u64,
    /// Size of the header itself (8 for normal, 16 for extended size).
    pub header_size: u8,
}

impl BoxHeader {
    /// Offset where the box payload (content) begins.
    pub fn payload_offset(&self) -> u64 {
        self.offset + self.header_size as u64
    }

    /// Size of the payload (content) in bytes.
    pub fn payload_size(&self) -> u64 {
        self.size - self.header_size as u64
    }
}

/// A parsed ISOBMFF box with its raw payload data.
#[derive(Debug, Clone)]
pub struct Mp4Box {
    pub header: BoxHeader,
    /// The raw payload bytes (excluding the header).
    pub payload: Vec<u8>,
}

/// Parsed protection scheme info from a sinf box.
#[derive(Debug, Clone)]
pub struct ProtectionSchemeInfo {
    /// Original codec FourCC from frma box (e.g., "avc1", "mp4a").
    pub original_format: FourCC,
    /// Scheme type from schm box (e.g., "cbcs", "cenc").
    pub scheme_type: FourCC,
    /// Scheme version from schm box.
    pub scheme_version: u32,
    /// Track encryption info from tenc box.
    pub tenc: TrackEncryptionBox,
}

/// Parsed tenc (track encryption) box contents.
#[derive(Debug, Clone)]
pub struct TrackEncryptionBox {
    pub is_protected: u8,
    pub default_per_sample_iv_size: u8,
    pub default_kid: [u8; 16],
    /// CBCS-specific: number of 16-byte blocks to encrypt per pattern.
    pub default_crypt_byte_block: u8,
    /// CBCS-specific: number of 16-byte blocks to skip per pattern.
    pub default_skip_byte_block: u8,
    /// CBCS-specific: constant IV (if per_sample_iv_size == 0).
    pub default_constant_iv: Option<Vec<u8>>,
}

/// Parsed senc (sample encryption) box for a fragment.
#[derive(Debug, Clone)]
pub struct SampleEncryptionBox {
    pub flags: u32,
    pub sample_count: u32,
    pub entries: Vec<SencEntry>,
}

/// A single sample's encryption info from the senc box.
#[derive(Debug, Clone)]
pub struct SencEntry {
    /// Per-sample IV. Length depends on defaultPerSampleIVSize from tenc.
    pub iv: Vec<u8>,
    /// Subsample encryption ranges, if present (flag 0x02).
    pub subsamples: Option<Vec<SubsampleEntry>>,
}

/// Clear/encrypted byte ranges within a sample.
#[derive(Debug, Clone, Copy)]
pub struct SubsampleEntry {
    pub clear_bytes: u16,
    pub encrypted_bytes: u32,
}

/// Parsed PSSH (Protection System Specific Header) box.
#[derive(Debug, Clone)]
pub struct PsshBox {
    pub version: u8,
    pub system_id: [u8; 16],
    /// Key IDs (only present in version 1).
    pub key_ids: Vec<[u8; 16]>,
    /// DRM system-specific data.
    pub data: Vec<u8>,
}

/// Parsed trun (track fragment run) box.
#[derive(Debug, Clone)]
pub struct TrackRunBox {
    pub flags: u32,
    pub sample_count: u32,
    pub data_offset: Option<i32>,
    pub first_sample_flags: Option<u32>,
    pub entries: Vec<TrunEntry>,
}

#[derive(Debug, Clone, Default)]
pub struct TrunEntry {
    pub sample_duration: Option<u32>,
    pub sample_size: Option<u32>,
    pub sample_flags: Option<u32>,
    pub sample_composition_time_offset: Option<i32>,
}

/// Read a box header at the given offset.
pub fn read_box_header(data: &[u8], offset: u64) -> Result<BoxHeader> {
    let off = offset as usize;
    if off + 8 > data.len() {
        return Err(EdgepackError::MediaParse(
            "not enough data for box header".into(),
        ));
    }

    let size32 = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
    let box_type: FourCC = [data[off + 4], data[off + 5], data[off + 6], data[off + 7]];

    if size32 == 1 {
        // Extended size
        if off + 16 > data.len() {
            return Err(EdgepackError::MediaParse(
                "not enough data for extended box header".into(),
            ));
        }
        let size64 = u64::from_be_bytes([
            data[off + 8],
            data[off + 9],
            data[off + 10],
            data[off + 11],
            data[off + 12],
            data[off + 13],
            data[off + 14],
            data[off + 15],
        ]);
        Ok(BoxHeader {
            size: size64,
            box_type,
            offset,
            header_size: 16,
        })
    } else if size32 == 0 {
        // Box extends to end of file
        Ok(BoxHeader {
            size: (data.len() - off) as u64,
            box_type,
            offset,
            header_size: 8,
        })
    } else {
        Ok(BoxHeader {
            size: size32 as u64,
            box_type,
            offset,
            header_size: 8,
        })
    }
}

/// Iterate over top-level boxes in a data buffer.
pub fn iterate_boxes(data: &[u8]) -> BoxIterator<'_> {
    BoxIterator { data, offset: 0 }
}

pub struct BoxIterator<'a> {
    data: &'a [u8],
    offset: u64,
}

impl<'a> Iterator for BoxIterator<'a> {
    type Item = Result<BoxHeader>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.offset as usize >= self.data.len() {
            return None;
        }
        match read_box_header(self.data, self.offset) {
            Ok(header) => {
                if header.size == 0 {
                    self.offset = self.data.len() as u64;
                } else {
                    self.offset += header.size;
                }
                Some(Ok(header))
            }
            Err(e) => {
                self.offset = self.data.len() as u64;
                Some(Err(e))
            }
        }
    }
}

/// Find a child box of the given type within a container box's payload.
pub fn find_child_box<'a>(data: &'a [u8], box_type: &FourCC) -> Option<BoxHeader> {
    let mut iter = iterate_boxes(data);
    iter.find_map(|result| {
        result.ok().filter(|header| &header.box_type == box_type)
    })
}

/// Extract the payload bytes of a box.
pub fn box_payload<'a>(data: &'a [u8], header: &BoxHeader) -> &'a [u8] {
    let start = header.payload_offset() as usize;
    let end = (header.offset + header.size) as usize;
    &data[start..end.min(data.len())]
}

/// Parse a tenc box from its payload bytes (after version/flags).
pub fn parse_tenc(payload: &[u8]) -> Result<TrackEncryptionBox> {
    if payload.len() < 6 + 16 {
        return Err(EdgepackError::MediaParse(
            "tenc box too small".into(),
        ));
    }

    // Bytes 0-3: version (1 byte) + flags (3 bytes)
    let _version = payload[0];
    // Byte 4: reserved (or default_crypt/skip for version >= 1)
    // Byte 5: default_isProtected | default_Per_Sample_IV_Size
    let crypt_byte_block = (payload[4] >> 4) & 0x0F;
    let skip_byte_block = payload[4] & 0x0F;
    let is_protected = payload[5];
    let default_per_sample_iv_size = payload[6];

    let mut default_kid = [0u8; 16];
    default_kid.copy_from_slice(&payload[7..23]);

    let default_constant_iv = if default_per_sample_iv_size == 0 && payload.len() > 23 {
        let iv_size = payload[23] as usize;
        if payload.len() >= 24 + iv_size {
            Some(payload[24..24 + iv_size].to_vec())
        } else {
            None
        }
    } else {
        None
    };

    Ok(TrackEncryptionBox {
        is_protected,
        default_per_sample_iv_size,
        default_kid,
        default_crypt_byte_block: crypt_byte_block,
        default_skip_byte_block: skip_byte_block,
        default_constant_iv,
    })
}

/// Parse a senc box from its full box data (including version/flags).
pub fn parse_senc(data: &[u8], per_sample_iv_size: u8) -> Result<SampleEncryptionBox> {
    if data.len() < 8 {
        return Err(EdgepackError::MediaParse("senc box too small".into()));
    }

    let flags = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    let sample_count = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let has_subsamples = flags & 0x02 != 0;

    let mut entries = Vec::with_capacity(sample_count as usize);
    let mut offset = 8usize;

    for _ in 0..sample_count {
        let iv_size = per_sample_iv_size as usize;
        if offset + iv_size > data.len() {
            return Err(EdgepackError::MediaParse(
                "senc: not enough data for IV".into(),
            ));
        }
        let iv = data[offset..offset + iv_size].to_vec();
        offset += iv_size;

        let subsamples = if has_subsamples {
            if offset + 2 > data.len() {
                return Err(EdgepackError::MediaParse(
                    "senc: not enough data for subsample count".into(),
                ));
            }
            let sub_count = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;

            let mut subs = Vec::with_capacity(sub_count);
            for _ in 0..sub_count {
                if offset + 6 > data.len() {
                    return Err(EdgepackError::MediaParse(
                        "senc: not enough data for subsample entry".into(),
                    ));
                }
                let clear = u16::from_be_bytes([data[offset], data[offset + 1]]);
                let encrypted = u32::from_be_bytes([
                    data[offset + 2],
                    data[offset + 3],
                    data[offset + 4],
                    data[offset + 5],
                ]);
                subs.push(SubsampleEntry {
                    clear_bytes: clear,
                    encrypted_bytes: encrypted,
                });
                offset += 6;
            }
            Some(subs)
        } else {
            None
        };

        entries.push(SencEntry { iv, subsamples });
    }

    Ok(SampleEncryptionBox {
        flags,
        sample_count,
        entries,
    })
}

/// Parse a PSSH box from its payload (after box header).
pub fn parse_pssh(data: &[u8]) -> Result<PsshBox> {
    if data.len() < 4 + 16 + 4 {
        return Err(EdgepackError::MediaParse("PSSH box too small".into()));
    }

    let version = data[0];
    let mut system_id = [0u8; 16];
    system_id.copy_from_slice(&data[4..20]);

    let mut offset = 20;
    let mut key_ids = Vec::new();

    if version >= 1 {
        if offset + 4 > data.len() {
            return Err(EdgepackError::MediaParse(
                "PSSH v1: not enough data for KID count".into(),
            ));
        }
        let kid_count =
            u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
                as usize;
        offset += 4;

        for _ in 0..kid_count {
            if offset + 16 > data.len() {
                return Err(EdgepackError::MediaParse(
                    "PSSH v1: not enough data for KID".into(),
                ));
            }
            let mut kid = [0u8; 16];
            kid.copy_from_slice(&data[offset..offset + 16]);
            key_ids.push(kid);
            offset += 16;
        }
    }

    if offset + 4 > data.len() {
        return Err(EdgepackError::MediaParse(
            "PSSH: not enough data for data size".into(),
        ));
    }
    let data_size =
        u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
            as usize;
    offset += 4;

    let pssh_data = if offset + data_size <= data.len() {
        data[offset..offset + data_size].to_vec()
    } else {
        data[offset..].to_vec()
    };

    Ok(PsshBox {
        version,
        system_id,
        key_ids,
        data: pssh_data,
    })
}

/// Parse a trun box from its payload (after box header).
pub fn parse_trun(data: &[u8]) -> Result<TrackRunBox> {
    if data.len() < 8 {
        return Err(EdgepackError::MediaParse("trun box too small".into()));
    }

    let flags = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    let sample_count = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

    let mut offset = 8usize;

    let data_offset = if flags & 0x0001 != 0 {
        if offset + 4 > data.len() {
            return Err(EdgepackError::MediaParse(
                "trun: not enough data for data_offset".into(),
            ));
        }
        let v = i32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]);
        offset += 4;
        Some(v)
    } else {
        None
    };

    let first_sample_flags = if flags & 0x0004 != 0 {
        if offset + 4 > data.len() {
            return Err(EdgepackError::MediaParse(
                "trun: not enough data for first_sample_flags".into(),
            ));
        }
        let v = u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]]);
        offset += 4;
        Some(v)
    } else {
        None
    };

    let has_duration = flags & 0x0100 != 0;
    let has_size = flags & 0x0200 != 0;
    let has_flags = flags & 0x0400 != 0;
    let has_cto = flags & 0x0800 != 0;

    let mut entries = Vec::with_capacity(sample_count as usize);
    for _ in 0..sample_count {
        let mut entry = TrunEntry::default();

        if has_duration {
            if offset + 4 > data.len() {
                return Err(EdgepackError::MediaParse(
                    "trun: not enough data for sample_duration".into(),
                ));
            }
            entry.sample_duration = Some(u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]));
            offset += 4;
        }
        if has_size {
            if offset + 4 > data.len() {
                return Err(EdgepackError::MediaParse(
                    "trun: not enough data for sample_size".into(),
                ));
            }
            entry.sample_size = Some(u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]));
            offset += 4;
        }
        if has_flags {
            if offset + 4 > data.len() {
                return Err(EdgepackError::MediaParse(
                    "trun: not enough data for sample_flags".into(),
                ));
            }
            entry.sample_flags = Some(u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]));
            offset += 4;
        }
        if has_cto {
            if offset + 4 > data.len() {
                return Err(EdgepackError::MediaParse(
                    "trun: not enough data for composition_time_offset".into(),
                ));
            }
            entry.sample_composition_time_offset = Some(i32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]));
            offset += 4;
        }

        entries.push(entry);
    }

    Ok(TrackRunBox {
        flags,
        sample_count,
        data_offset,
        first_sample_flags,
        entries,
    })
}

/// Write a box header (size + type) to the output buffer.
pub fn write_box_header(output: &mut Vec<u8>, size: u32, box_type: &FourCC) {
    output.extend_from_slice(&size.to_be_bytes());
    output.extend_from_slice(box_type);
}

/// Write a full box header (size + type + version + flags) to the output buffer.
pub fn write_full_box_header(
    output: &mut Vec<u8>,
    size: u32,
    box_type: &FourCC,
    version: u8,
    flags: u32,
) {
    write_box_header(output, size, box_type);
    output.push(version);
    output.extend_from_slice(&flags.to_be_bytes()[1..4]);
}

/// Build a PSSH box from its components.
pub fn build_pssh_box(pssh: &PsshBox) -> Vec<u8> {
    let mut inner = Vec::new();

    // version + flags
    inner.push(pssh.version);
    inner.extend_from_slice(&[0u8; 3]); // flags

    // system_id
    inner.extend_from_slice(&pssh.system_id);

    // key_ids (version 1 only)
    if pssh.version >= 1 {
        inner.extend_from_slice(&(pssh.key_ids.len() as u32).to_be_bytes());
        for kid in &pssh.key_ids {
            inner.extend_from_slice(kid);
        }
    }

    // data
    inner.extend_from_slice(&(pssh.data.len() as u32).to_be_bytes());
    inner.extend_from_slice(&pssh.data);

    let total_size = 8 + inner.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    write_box_header(&mut output, total_size, &crate::media::box_type::PSSH);
    output.extend_from_slice(&inner);
    output
}

/// Build a senc box from sample encryption entries.
pub fn build_senc_box(entries: &[SencEntry], use_subsamples: bool) -> Vec<u8> {
    let mut inner = Vec::new();
    let flags: u32 = if use_subsamples { 0x02 } else { 0x00 };

    // version + flags
    inner.push(0u8); // version
    inner.extend_from_slice(&flags.to_be_bytes()[1..4]);

    // sample count
    inner.extend_from_slice(&(entries.len() as u32).to_be_bytes());

    for entry in entries {
        inner.extend_from_slice(&entry.iv);

        if use_subsamples {
            if let Some(ref subs) = entry.subsamples {
                inner.extend_from_slice(&(subs.len() as u16).to_be_bytes());
                for sub in subs {
                    inner.extend_from_slice(&sub.clear_bytes.to_be_bytes());
                    inner.extend_from_slice(&sub.encrypted_bytes.to_be_bytes());
                }
            } else {
                inner.extend_from_slice(&0u16.to_be_bytes());
            }
        }
    }

    let total_size = 8 + inner.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    write_box_header(&mut output, total_size, &crate::media::box_type::SENC);
    output.extend_from_slice(&inner);
    output
}

/// Parsed emsg (event message) box.
///
/// Carries in-band event messages in media segments. Used for SCTE-35 ad markers,
/// ID3 metadata, and other timed events.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmsgBox {
    /// Box version (0 or 1).
    pub version: u8,
    /// Scheme ID URI identifying the event type.
    pub scheme_id_uri: String,
    /// Value string (scheme-specific).
    pub value: String,
    /// Timescale for presentation_time and event_duration.
    pub timescale: u32,
    /// Presentation time (v0: delta from segment decode time; v1: absolute).
    pub presentation_time: u64,
    /// Event duration in timescale units (0xFFFFFFFF = unknown).
    pub event_duration: u32,
    /// Event identifier.
    pub id: u32,
    /// Event-specific data (e.g., SCTE-35 splice_info_section binary).
    pub message_data: Vec<u8>,
}

/// Read a NUL-terminated string from data starting at offset.
/// Returns (string, bytes_consumed_including_NUL).
fn read_nul_string(data: &[u8], offset: usize) -> Result<(String, usize)> {
    let start = offset;
    let mut end = offset;
    while end < data.len() && data[end] != 0 {
        end += 1;
    }
    if end >= data.len() {
        return Err(EdgepackError::MediaParse(
            "emsg: NUL-terminated string extends beyond data".into(),
        ));
    }
    let s = String::from_utf8_lossy(&data[start..end]).into_owned();
    Ok((s, end - start + 1)) // +1 for the NUL byte
}

/// Parse an emsg box from its full box payload (including version/flags).
///
/// Version 0 layout: version(1) + flags(3) + scheme_id_uri(NUL) + value(NUL) +
///   timescale(4) + presentation_time_delta(4) + event_duration(4) + id(4) + message_data
///
/// Version 1 layout: version(1) + flags(3) + timescale(4) + presentation_time(8) +
///   event_duration(4) + id(4) + scheme_id_uri(NUL) + value(NUL) + message_data
pub fn parse_emsg(data: &[u8]) -> Result<EmsgBox> {
    if data.len() < 4 {
        return Err(EdgepackError::MediaParse("emsg box too small".into()));
    }

    let version = data[0];
    // flags at data[1..4] — ignored

    match version {
        0 => {
            let mut offset = 4;
            let (scheme_id_uri, consumed) = read_nul_string(data, offset)?;
            offset += consumed;
            let (value, consumed) = read_nul_string(data, offset)?;
            offset += consumed;

            if offset + 16 > data.len() {
                return Err(EdgepackError::MediaParse(
                    "emsg v0: not enough data for fixed fields".into(),
                ));
            }

            let timescale = u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]);
            offset += 4;
            let presentation_time_delta = u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]);
            offset += 4;
            let event_duration = u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]);
            offset += 4;
            let id = u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]);
            offset += 4;

            let message_data = data[offset..].to_vec();

            Ok(EmsgBox {
                version: 0,
                scheme_id_uri,
                value,
                timescale,
                presentation_time: presentation_time_delta as u64,
                event_duration,
                id,
                message_data,
            })
        }
        1 => {
            if data.len() < 24 {
                return Err(EdgepackError::MediaParse(
                    "emsg v1: not enough data for fixed fields".into(),
                ));
            }

            let mut offset = 4;
            let timescale = u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]);
            offset += 4;
            let presentation_time = u64::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
                data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7],
            ]);
            offset += 8;
            let event_duration = u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]);
            offset += 4;
            let id = u32::from_be_bytes([
                data[offset], data[offset + 1], data[offset + 2], data[offset + 3],
            ]);
            offset += 4;

            let (scheme_id_uri, consumed) = read_nul_string(data, offset)?;
            offset += consumed;
            let (value, consumed) = read_nul_string(data, offset)?;
            offset += consumed;

            let message_data = data[offset..].to_vec();

            Ok(EmsgBox {
                version: 1,
                scheme_id_uri,
                value,
                timescale,
                presentation_time,
                event_duration,
                id,
                message_data,
            })
        }
        _ => Err(EdgepackError::MediaParse(format!(
            "emsg: unsupported version {version}"
        ))),
    }
}

/// Build an emsg box from its components.
pub fn build_emsg_box(emsg: &EmsgBox) -> Vec<u8> {
    let mut inner = Vec::new();

    match emsg.version {
        0 => {
            // version + flags
            inner.push(0);
            inner.extend_from_slice(&[0u8; 3]);
            // scheme_id_uri + NUL
            inner.extend_from_slice(emsg.scheme_id_uri.as_bytes());
            inner.push(0);
            // value + NUL
            inner.extend_from_slice(emsg.value.as_bytes());
            inner.push(0);
            // timescale
            inner.extend_from_slice(&emsg.timescale.to_be_bytes());
            // presentation_time_delta (v0 is 32-bit)
            inner.extend_from_slice(&(emsg.presentation_time as u32).to_be_bytes());
            // event_duration
            inner.extend_from_slice(&emsg.event_duration.to_be_bytes());
            // id
            inner.extend_from_slice(&emsg.id.to_be_bytes());
            // message_data
            inner.extend_from_slice(&emsg.message_data);
        }
        _ => {
            // version 1 (or treat any non-0 as v1)
            inner.push(1);
            inner.extend_from_slice(&[0u8; 3]);
            // timescale
            inner.extend_from_slice(&emsg.timescale.to_be_bytes());
            // presentation_time (64-bit)
            inner.extend_from_slice(&emsg.presentation_time.to_be_bytes());
            // event_duration
            inner.extend_from_slice(&emsg.event_duration.to_be_bytes());
            // id
            inner.extend_from_slice(&emsg.id.to_be_bytes());
            // scheme_id_uri + NUL
            inner.extend_from_slice(emsg.scheme_id_uri.as_bytes());
            inner.push(0);
            // value + NUL
            inner.extend_from_slice(emsg.value.as_bytes());
            inner.push(0);
            // message_data
            inner.extend_from_slice(&emsg.message_data);
        }
    }

    let total_size = 8 + inner.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    write_box_header(&mut output, total_size, &crate::media::box_type::EMSG);
    output.extend_from_slice(&inner);
    output
}

/// A single reference entry from a parsed sidx (Segment Index) box.
#[derive(Debug, Clone)]
pub struct SidxReference {
    /// Whether this references another sidx (true) or a media subsegment (false).
    pub reference_type: bool,
    /// Size of the referenced data in bytes.
    pub referenced_size: u32,
    /// Duration of the subsegment in timescale units.
    pub subsegment_duration: u32,
    /// Whether this subsegment starts with a Stream Access Point (SAP).
    pub starts_with_sap: bool,
}

/// Parsed sidx (Segment Index) box result.
#[derive(Debug, Clone)]
pub struct SidxBox {
    /// Timescale for duration conversion.
    pub timescale: u32,
    /// Offset of the first referenced data relative to the end of this sidx box.
    pub first_offset: u64,
    /// Individual segment references.
    pub references: Vec<SidxReference>,
}

/// Parse a sidx (Segment Index) box payload.
///
/// The `data` should be the raw bytes of the sidx box (including the box header).
/// Returns the parsed sidx with subsegment references that can be used to
/// calculate byte ranges for individual segments.
pub fn parse_sidx(data: &[u8]) -> Result<SidxBox> {
    // Find the sidx box in the data (it might be preceded by other boxes)
    for result in iterate_boxes(data) {
        let header = result?;
        if header.box_type == crate::media::box_type::SIDX {
            let payload_start = header.payload_offset() as usize;
            let payload_end = (header.offset + header.size) as usize;
            if payload_end > data.len() {
                return Err(EdgepackError::MediaParse("sidx box extends beyond data".into()));
            }
            let payload = &data[payload_start..payload_end];
            return parse_sidx_payload(payload);
        }
    }
    Err(EdgepackError::MediaParse("no sidx box found in data".into()))
}

/// Parse the payload of a sidx box (after box header).
fn parse_sidx_payload(payload: &[u8]) -> Result<SidxBox> {
    if payload.len() < 4 {
        return Err(EdgepackError::MediaParse("sidx payload too short".into()));
    }

    let version = payload[0];
    // bytes 1-3 are flags

    let mut offset = 4;

    // reference_ID (4 bytes)
    if offset + 4 > payload.len() {
        return Err(EdgepackError::MediaParse("sidx: truncated reference_ID".into()));
    }
    offset += 4; // skip reference_ID

    // timescale (4 bytes)
    if offset + 4 > payload.len() {
        return Err(EdgepackError::MediaParse("sidx: truncated timescale".into()));
    }
    let timescale = u32::from_be_bytes([
        payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
    ]);
    offset += 4;

    // earliest_presentation_time and first_offset
    let first_offset = if version == 0 {
        if offset + 8 > payload.len() {
            return Err(EdgepackError::MediaParse("sidx: truncated v0 times".into()));
        }
        let _ept = u32::from_be_bytes([
            payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
        ]);
        offset += 4;
        let fo = u32::from_be_bytes([
            payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
        ]);
        offset += 4;
        fo as u64
    } else {
        if offset + 16 > payload.len() {
            return Err(EdgepackError::MediaParse("sidx: truncated v1 times".into()));
        }
        let _ept = u64::from_be_bytes([
            payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
            payload[offset + 4], payload[offset + 5], payload[offset + 6], payload[offset + 7],
        ]);
        offset += 8;
        let fo = u64::from_be_bytes([
            payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
            payload[offset + 4], payload[offset + 5], payload[offset + 6], payload[offset + 7],
        ]);
        offset += 8;
        fo
    };

    // reserved (2 bytes) + reference_count (2 bytes)
    if offset + 4 > payload.len() {
        return Err(EdgepackError::MediaParse("sidx: truncated reference count".into()));
    }
    offset += 2; // skip reserved
    let reference_count = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
    offset += 2;

    // Each reference is 12 bytes
    let needed = reference_count * 12;
    if offset + needed > payload.len() {
        return Err(EdgepackError::MediaParse(format!(
            "sidx: {} references need {} bytes but only {} remain",
            reference_count, needed, payload.len() - offset
        )));
    }

    let mut references = Vec::with_capacity(reference_count);
    for _ in 0..reference_count {
        let word1 = u32::from_be_bytes([
            payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
        ]);
        let reference_type = (word1 >> 31) != 0;
        let referenced_size = word1 & 0x7FFF_FFFF;
        offset += 4;

        let subsegment_duration = u32::from_be_bytes([
            payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
        ]);
        offset += 4;

        let word3 = u32::from_be_bytes([
            payload[offset], payload[offset + 1], payload[offset + 2], payload[offset + 3],
        ]);
        let starts_with_sap = (word3 >> 31) != 0;
        offset += 4;

        references.push(SidxReference {
            reference_type,
            referenced_size,
            subsegment_duration,
            starts_with_sap,
        });
    }

    Ok(SidxBox {
        timescale,
        first_offset,
        references,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::box_type;

    /// Helper: build a minimal box (header + payload).
    fn make_box(box_type: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = 8 + payload.len() as u32;
        let mut data = Vec::new();
        data.extend_from_slice(&size.to_be_bytes());
        data.extend_from_slice(box_type);
        data.extend_from_slice(payload);
        data
    }

    // --- BoxHeader ---

    #[test]
    fn box_header_payload_offset() {
        let h = BoxHeader {
            size: 100,
            box_type: *b"test",
            offset: 0,
            header_size: 8,
        };
        assert_eq!(h.payload_offset(), 8);
    }

    #[test]
    fn box_header_payload_size() {
        let h = BoxHeader {
            size: 100,
            box_type: *b"test",
            offset: 0,
            header_size: 8,
        };
        assert_eq!(h.payload_size(), 92);
    }

    #[test]
    fn box_header_extended_size_payload() {
        let h = BoxHeader {
            size: 200,
            box_type: *b"test",
            offset: 10,
            header_size: 16,
        };
        assert_eq!(h.payload_offset(), 26);
        assert_eq!(h.payload_size(), 184);
    }

    // --- read_box_header ---

    #[test]
    fn read_box_header_normal() {
        let data = make_box(b"ftyp", &[0u8; 4]);
        let header = read_box_header(&data, 0).unwrap();
        assert_eq!(header.size, 12);
        assert_eq!(header.box_type, *b"ftyp");
        assert_eq!(header.offset, 0);
        assert_eq!(header.header_size, 8);
    }

    #[test]
    fn read_box_header_at_offset() {
        let mut data = vec![0u8; 10]; // padding
        data.extend_from_slice(&make_box(b"moov", &[0u8; 20]));
        let header = read_box_header(&data, 10).unwrap();
        assert_eq!(header.box_type, *b"moov");
        assert_eq!(header.offset, 10);
    }

    #[test]
    fn read_box_header_too_short() {
        let data = [0u8; 4]; // only 4 bytes, need 8
        let result = read_box_header(&data, 0);
        assert!(result.is_err());
    }

    #[test]
    fn read_box_header_size_zero_extends_to_end() {
        let mut data = vec![0u8; 8];
        data[4..8].copy_from_slice(b"test");
        // size = 0 means extends to end of data
        let header = read_box_header(&data, 0).unwrap();
        assert_eq!(header.size, 8); // extends to end
    }

    #[test]
    fn read_box_header_extended_size() {
        let mut data = vec![0u8; 24];
        data[0..4].copy_from_slice(&1u32.to_be_bytes()); // size=1 triggers extended
        data[4..8].copy_from_slice(b"test");
        data[8..16].copy_from_slice(&100u64.to_be_bytes()); // extended size
        let header = read_box_header(&data, 0).unwrap();
        assert_eq!(header.size, 100);
        assert_eq!(header.header_size, 16);
    }

    // --- iterate_boxes ---

    #[test]
    fn iterate_boxes_single() {
        let data = make_box(b"ftyp", &[0u8; 8]);
        let boxes: Vec<_> = iterate_boxes(&data).collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].box_type, *b"ftyp");
    }

    #[test]
    fn iterate_boxes_multiple() {
        let mut data = make_box(b"ftyp", &[0u8; 4]);
        data.extend_from_slice(&make_box(b"moov", &[0u8; 20]));
        data.extend_from_slice(&make_box(b"mdat", &[0u8; 100]));

        let boxes: Vec<_> = iterate_boxes(&data).collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert_eq!(boxes.len(), 3);
        assert_eq!(boxes[0].box_type, *b"ftyp");
        assert_eq!(boxes[1].box_type, *b"moov");
        assert_eq!(boxes[2].box_type, *b"mdat");
    }

    #[test]
    fn iterate_boxes_empty() {
        let data: &[u8] = &[];
        let boxes: Vec<_> = iterate_boxes(data).collect::<std::result::Result<Vec<_>, _>>().unwrap();
        assert!(boxes.is_empty());
    }

    // --- find_child_box ---

    #[test]
    fn find_child_box_found() {
        let mut children = make_box(b"tkhd", &[0u8; 10]);
        children.extend_from_slice(&make_box(b"mdia", &[0u8; 20]));

        let result = find_child_box(&children, &box_type::MDIA);
        assert!(result.is_some());
        assert_eq!(result.unwrap().box_type, *b"mdia");
    }

    #[test]
    fn find_child_box_not_found() {
        let children = make_box(b"tkhd", &[0u8; 10]);
        let result = find_child_box(&children, &box_type::SINF);
        assert!(result.is_none());
    }

    // --- write_box_header ---

    #[test]
    fn write_box_header_produces_valid_data() {
        let mut output = Vec::new();
        write_box_header(&mut output, 42, b"test");
        assert_eq!(output.len(), 8);
        assert_eq!(&output[0..4], &42u32.to_be_bytes());
        assert_eq!(&output[4..8], b"test");
    }

    #[test]
    fn write_box_header_roundtrip() {
        let mut output = Vec::new();
        write_box_header(&mut output, 100, b"moov");
        let header = read_box_header(&output, 0).unwrap();
        assert_eq!(header.size, 100);
        assert_eq!(header.box_type, *b"moov");
    }

    // --- write_full_box_header ---

    #[test]
    fn write_full_box_header_correct() {
        let mut output = Vec::new();
        write_full_box_header(&mut output, 20, b"tenc", 1, 0x000002);
        assert_eq!(output.len(), 12);
        assert_eq!(&output[4..8], b"tenc");
        assert_eq!(output[8], 1); // version
        assert_eq!(&output[9..12], &[0, 0, 2]); // flags
    }

    // --- parse_tenc ---

    #[test]
    fn parse_tenc_basic() {
        // version(1) + flags(3) + reserved/crypt_skip(1) + isProtected(1)
        // + ivSize(1) + KID(16) = 23 bytes minimum
        let mut payload = vec![0u8; 23];
        payload[0] = 0; // version
        payload[4] = 0x19; // crypt=1, skip=9
        payload[5] = 1; // isProtected
        payload[6] = 8; // ivSize
        payload[7..23].copy_from_slice(&[0xAA; 16]); // KID

        let tenc = parse_tenc(&payload).unwrap();
        assert_eq!(tenc.is_protected, 1);
        assert_eq!(tenc.default_per_sample_iv_size, 8);
        assert_eq!(tenc.default_kid, [0xAA; 16]);
        assert_eq!(tenc.default_crypt_byte_block, 1);
        assert_eq!(tenc.default_skip_byte_block, 9);
        assert!(tenc.default_constant_iv.is_none());
    }

    #[test]
    fn parse_tenc_with_constant_iv() {
        let mut payload = vec![0u8; 40];
        payload[5] = 1; // isProtected
        payload[6] = 0; // ivSize = 0 → use constant IV
        payload[7..23].copy_from_slice(&[0xBB; 16]); // KID
        payload[23] = 16; // constant IV size
        payload[24..40].copy_from_slice(&[0xCC; 16]); // constant IV

        let tenc = parse_tenc(&payload).unwrap();
        assert_eq!(tenc.default_per_sample_iv_size, 0);
        let civ = tenc.default_constant_iv.unwrap();
        assert_eq!(civ, vec![0xCC; 16]);
    }

    #[test]
    fn parse_tenc_too_small_errors() {
        let payload = vec![0u8; 10]; // too small
        assert!(parse_tenc(&payload).is_err());
    }

    // --- parse_senc ---

    #[test]
    fn parse_senc_no_subsamples() {
        // version(1) + flags(3) + sample_count(4) + entries
        let mut data = vec![0u8; 8];
        data[3] = 0; // flags = 0 (no subsamples)
        data[4..8].copy_from_slice(&2u32.to_be_bytes()); // 2 samples

        // 2 entries, each with 8-byte IV
        data.extend_from_slice(&[0x01; 8]); // sample 0 IV
        data.extend_from_slice(&[0x02; 8]); // sample 1 IV

        let senc = parse_senc(&data, 8).unwrap();
        assert_eq!(senc.sample_count, 2);
        assert_eq!(senc.entries.len(), 2);
        assert_eq!(senc.entries[0].iv, vec![0x01; 8]);
        assert_eq!(senc.entries[1].iv, vec![0x02; 8]);
        assert!(senc.entries[0].subsamples.is_none());
    }

    #[test]
    fn parse_senc_with_subsamples() {
        let mut data = vec![0u8; 8];
        data[3] = 0x02; // flags = 0x02 (has subsamples)
        data[4..8].copy_from_slice(&1u32.to_be_bytes()); // 1 sample

        // Sample entry: 8-byte IV + subsample count(2) + subsample data
        data.extend_from_slice(&[0xAA; 8]); // IV
        data.extend_from_slice(&1u16.to_be_bytes()); // 1 subsample
        data.extend_from_slice(&100u16.to_be_bytes()); // clear_bytes
        data.extend_from_slice(&200u32.to_be_bytes()); // encrypted_bytes

        let senc = parse_senc(&data, 8).unwrap();
        assert_eq!(senc.entries.len(), 1);
        let subs = senc.entries[0].subsamples.as_ref().unwrap();
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].clear_bytes, 100);
        assert_eq!(subs[0].encrypted_bytes, 200);
    }

    // --- parse_pssh ---

    #[test]
    fn parse_pssh_v0() {
        let mut data = vec![0u8; 24];
        data[0] = 0; // version 0
        data[4..20].copy_from_slice(&[0xAA; 16]); // system_id
        data[20..24].copy_from_slice(&0u32.to_be_bytes()); // data_size = 0

        let pssh = parse_pssh(&data).unwrap();
        assert_eq!(pssh.version, 0);
        assert_eq!(pssh.system_id, [0xAA; 16]);
        assert!(pssh.key_ids.is_empty());
        assert!(pssh.data.is_empty());
    }

    #[test]
    fn parse_pssh_v1_with_kids() {
        let mut data = vec![0u8; 44];
        data[0] = 1; // version 1
        data[4..20].copy_from_slice(&[0xBB; 16]); // system_id
        data[20..24].copy_from_slice(&1u32.to_be_bytes()); // 1 KID
        data[24..40].copy_from_slice(&[0xCC; 16]); // KID
        data[40..44].copy_from_slice(&0u32.to_be_bytes()); // data_size = 0

        let pssh = parse_pssh(&data).unwrap();
        assert_eq!(pssh.version, 1);
        assert_eq!(pssh.key_ids.len(), 1);
        assert_eq!(pssh.key_ids[0], [0xCC; 16]);
    }

    // --- parse_trun ---

    #[test]
    fn parse_trun_with_sizes() {
        // flags = 0x0200 (has sample_size)
        let mut data = vec![0u8; 8];
        data[1..4].copy_from_slice(&0x000200u32.to_be_bytes()[1..4]);
        data[4..8].copy_from_slice(&2u32.to_be_bytes()); // 2 samples

        // 2 sample sizes
        data.extend_from_slice(&1024u32.to_be_bytes());
        data.extend_from_slice(&2048u32.to_be_bytes());

        let trun = parse_trun(&data).unwrap();
        assert_eq!(trun.sample_count, 2);
        assert_eq!(trun.entries[0].sample_size, Some(1024));
        assert_eq!(trun.entries[1].sample_size, Some(2048));
    }

    #[test]
    fn parse_trun_with_data_offset() {
        let mut data = vec![0u8; 8];
        data[1..4].copy_from_slice(&0x000001u32.to_be_bytes()[1..4]); // has data_offset
        data[4..8].copy_from_slice(&0u32.to_be_bytes()); // 0 samples
        data.extend_from_slice(&42i32.to_be_bytes());

        let trun = parse_trun(&data).unwrap();
        assert_eq!(trun.data_offset, Some(42));
    }

    // --- build_pssh_box ---

    #[test]
    fn build_pssh_box_v1() {
        let pssh = PsshBox {
            version: 1,
            system_id: [0xAA; 16],
            key_ids: vec![[0xBB; 16]],
            data: vec![0xCC; 8],
        };
        let built = build_pssh_box(&pssh);

        // Parse it back
        let header = read_box_header(&built, 0).unwrap();
        assert_eq!(header.box_type, box_type::PSSH);
        let payload = &built[header.header_size as usize..];
        let parsed = parse_pssh(payload).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.system_id, [0xAA; 16]);
        assert_eq!(parsed.key_ids, vec![[0xBB; 16]]);
        assert_eq!(parsed.data, vec![0xCC; 8]);
    }

    // --- build_senc_box ---

    #[test]
    fn build_senc_box_no_subsamples() {
        let entries = vec![
            SencEntry { iv: vec![0x01; 8], subsamples: None },
            SencEntry { iv: vec![0x02; 8], subsamples: None },
        ];
        let built = build_senc_box(&entries, false);

        let header = read_box_header(&built, 0).unwrap();
        assert_eq!(header.box_type, box_type::SENC);

        let payload = &built[header.header_size as usize..];
        let parsed = parse_senc(payload, 8).unwrap();
        assert_eq!(parsed.sample_count, 2);
        assert_eq!(parsed.entries[0].iv, vec![0x01; 8]);
        assert_eq!(parsed.entries[1].iv, vec![0x02; 8]);
    }

    #[test]
    fn build_senc_box_with_subsamples() {
        let entries = vec![SencEntry {
            iv: vec![0xAA; 8],
            subsamples: Some(vec![SubsampleEntry {
                clear_bytes: 10,
                encrypted_bytes: 200,
            }]),
        }];
        let built = build_senc_box(&entries, true);

        let header = read_box_header(&built, 0).unwrap();
        let payload = &built[header.header_size as usize..];
        let parsed = parse_senc(payload, 8).unwrap();
        assert_eq!(parsed.entries.len(), 1);
        let subs = parsed.entries[0].subsamples.as_ref().unwrap();
        assert_eq!(subs[0].clear_bytes, 10);
        assert_eq!(subs[0].encrypted_bytes, 200);
    }

    // --- box_payload ---

    #[test]
    fn box_payload_extracts_correctly() {
        let data = make_box(b"test", &[0xAA, 0xBB, 0xCC]);
        let header = read_box_header(&data, 0).unwrap();
        let payload = box_payload(&data, &header);
        assert_eq!(payload, &[0xAA, 0xBB, 0xCC]);
    }

    // --- parse_emsg ---

    #[test]
    fn parse_emsg_v0() {
        let mut payload = Vec::new();
        payload.push(0); // version
        payload.extend_from_slice(&[0u8; 3]); // flags
        payload.extend_from_slice(b"urn:test:scheme\0"); // scheme_id_uri + NUL
        payload.extend_from_slice(b"value1\0"); // value + NUL
        payload.extend_from_slice(&90000u32.to_be_bytes()); // timescale
        payload.extend_from_slice(&1000u32.to_be_bytes()); // presentation_time_delta
        payload.extend_from_slice(&0xFFFFFFFFu32.to_be_bytes()); // event_duration
        payload.extend_from_slice(&42u32.to_be_bytes()); // id
        payload.extend_from_slice(&[0xDE, 0xAD]); // message_data

        let emsg = parse_emsg(&payload).unwrap();
        assert_eq!(emsg.version, 0);
        assert_eq!(emsg.scheme_id_uri, "urn:test:scheme");
        assert_eq!(emsg.value, "value1");
        assert_eq!(emsg.timescale, 90000);
        assert_eq!(emsg.presentation_time, 1000);
        assert_eq!(emsg.event_duration, 0xFFFFFFFF);
        assert_eq!(emsg.id, 42);
        assert_eq!(emsg.message_data, vec![0xDE, 0xAD]);
    }

    #[test]
    fn parse_emsg_v1() {
        let mut payload = Vec::new();
        payload.push(1); // version
        payload.extend_from_slice(&[0u8; 3]); // flags
        payload.extend_from_slice(&90000u32.to_be_bytes()); // timescale
        payload.extend_from_slice(&8100000u64.to_be_bytes()); // presentation_time (90s)
        payload.extend_from_slice(&2700000u32.to_be_bytes()); // event_duration (30s)
        payload.extend_from_slice(&99u32.to_be_bytes()); // id
        payload.extend_from_slice(b"urn:scte:scte35:2013:bin\0"); // scheme_id_uri + NUL
        payload.extend_from_slice(b"\0"); // value (empty + NUL)
        payload.extend_from_slice(&[0xFC, 0x30]); // message_data

        let emsg = parse_emsg(&payload).unwrap();
        assert_eq!(emsg.version, 1);
        assert_eq!(emsg.scheme_id_uri, "urn:scte:scte35:2013:bin");
        assert_eq!(emsg.value, "");
        assert_eq!(emsg.timescale, 90000);
        assert_eq!(emsg.presentation_time, 8100000);
        assert_eq!(emsg.event_duration, 2700000);
        assert_eq!(emsg.id, 99);
        assert_eq!(emsg.message_data, vec![0xFC, 0x30]);
    }

    #[test]
    fn parse_emsg_v0_empty_message() {
        let mut payload = Vec::new();
        payload.push(0);
        payload.extend_from_slice(&[0u8; 3]);
        payload.extend_from_slice(b"urn:test\0");
        payload.extend_from_slice(b"\0"); // empty value
        payload.extend_from_slice(&1000u32.to_be_bytes());
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&0u32.to_be_bytes());
        payload.extend_from_slice(&0u32.to_be_bytes());
        // no message_data

        let emsg = parse_emsg(&payload).unwrap();
        assert!(emsg.message_data.is_empty());
    }

    #[test]
    fn parse_emsg_too_small() {
        assert!(parse_emsg(&[0, 0, 0]).is_err());
    }

    #[test]
    fn parse_emsg_unsupported_version() {
        let mut payload = vec![2u8]; // version 2
        payload.extend_from_slice(&[0u8; 3]);
        payload.extend_from_slice(&[0u8; 20]); // padding
        assert!(parse_emsg(&payload).is_err());
    }

    // --- build_emsg_box ---

    #[test]
    fn build_emsg_box_v0_roundtrip() {
        let emsg = EmsgBox {
            version: 0,
            scheme_id_uri: "urn:test:scheme".to_string(),
            value: "val".to_string(),
            timescale: 90000,
            presentation_time: 500,
            event_duration: 1000,
            id: 7,
            message_data: vec![0xAA, 0xBB, 0xCC],
        };
        let built = build_emsg_box(&emsg);
        let header = read_box_header(&built, 0).unwrap();
        assert_eq!(header.box_type, box_type::EMSG);
        let payload = &built[header.header_size as usize..];
        let parsed = parse_emsg(payload).unwrap();
        assert_eq!(parsed.version, 0);
        assert_eq!(parsed.scheme_id_uri, "urn:test:scheme");
        assert_eq!(parsed.value, "val");
        assert_eq!(parsed.timescale, 90000);
        assert_eq!(parsed.presentation_time, 500);
        assert_eq!(parsed.event_duration, 1000);
        assert_eq!(parsed.id, 7);
        assert_eq!(parsed.message_data, vec![0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn build_emsg_box_v1_roundtrip() {
        let emsg = EmsgBox {
            version: 1,
            scheme_id_uri: "urn:scte:scte35:2013:bin".to_string(),
            value: String::new(),
            timescale: 90000,
            presentation_time: 8100000,
            event_duration: 2700000,
            id: 42,
            message_data: vec![0xFC, 0x30, 0x00],
        };
        let built = build_emsg_box(&emsg);
        let header = read_box_header(&built, 0).unwrap();
        let payload = &built[header.header_size as usize..];
        let parsed = parse_emsg(payload).unwrap();
        assert_eq!(parsed.version, 1);
        assert_eq!(parsed.scheme_id_uri, "urn:scte:scte35:2013:bin");
        assert_eq!(parsed.timescale, 90000);
        assert_eq!(parsed.presentation_time, 8100000);
        assert_eq!(parsed.event_duration, 2700000);
        assert_eq!(parsed.id, 42);
        assert_eq!(parsed.message_data, vec![0xFC, 0x30, 0x00]);
    }

    #[test]
    fn build_emsg_box_empty_message() {
        let emsg = EmsgBox {
            version: 0,
            scheme_id_uri: "urn:x".to_string(),
            value: String::new(),
            timescale: 1,
            presentation_time: 0,
            event_duration: 0,
            id: 0,
            message_data: Vec::new(),
        };
        let built = build_emsg_box(&emsg);
        let header = read_box_header(&built, 0).unwrap();
        let payload = &built[header.header_size as usize..];
        let parsed = parse_emsg(payload).unwrap();
        assert!(parsed.message_data.is_empty());
    }

    // --- sidx parsing tests ---

    /// Build a synthetic sidx box for testing.
    fn build_test_sidx(version: u8, timescale: u32, first_offset: u64, refs: &[(u32, u32)]) -> Vec<u8> {
        let mut inner = Vec::new();
        // Version + flags
        inner.push(version);
        inner.extend_from_slice(&[0, 0, 0]); // flags
        // reference_ID
        inner.extend_from_slice(&1u32.to_be_bytes());
        // timescale
        inner.extend_from_slice(&timescale.to_be_bytes());
        // earliest_presentation_time + first_offset
        if version == 0 {
            inner.extend_from_slice(&0u32.to_be_bytes()); // ept
            inner.extend_from_slice(&(first_offset as u32).to_be_bytes());
        } else {
            inner.extend_from_slice(&0u64.to_be_bytes()); // ept
            inner.extend_from_slice(&first_offset.to_be_bytes());
        }
        // reserved (2 bytes) + reference_count (2 bytes)
        inner.extend_from_slice(&[0, 0]);
        inner.extend_from_slice(&(refs.len() as u16).to_be_bytes());
        // references
        for (size, duration) in refs {
            // reference_type=0 (media) | referenced_size
            inner.extend_from_slice(&size.to_be_bytes());
            // subsegment_duration
            inner.extend_from_slice(&duration.to_be_bytes());
            // starts_with_SAP=1, SAP_type=1, SAP_delta_time=0
            inner.extend_from_slice(&0x9000_0000u32.to_be_bytes());
        }

        let total_size = 8 + inner.len() as u32;
        let mut output = Vec::with_capacity(total_size as usize);
        write_box_header(&mut output, total_size, &crate::media::box_type::SIDX);
        output.extend_from_slice(&inner);
        output
    }

    #[test]
    fn parse_sidx_v0_basic() {
        let data = build_test_sidx(0, 1000, 0, &[
            (50000, 2000),
            (60000, 2000),
            (40000, 1000),
        ]);
        let result = parse_sidx(&data).unwrap();
        assert_eq!(result.timescale, 1000);
        assert_eq!(result.first_offset, 0);
        assert_eq!(result.references.len(), 3);
        assert_eq!(result.references[0].referenced_size, 50000);
        assert_eq!(result.references[0].subsegment_duration, 2000);
        assert!(!result.references[0].reference_type);
        assert_eq!(result.references[1].referenced_size, 60000);
        assert_eq!(result.references[2].referenced_size, 40000);
        assert_eq!(result.references[2].subsegment_duration, 1000);
    }

    #[test]
    fn parse_sidx_v1_with_first_offset() {
        let data = build_test_sidx(1, 90000, 512, &[
            (100000, 360000),
            (80000, 360000),
        ]);
        let result = parse_sidx(&data).unwrap();
        assert_eq!(result.timescale, 90000);
        assert_eq!(result.first_offset, 512);
        assert_eq!(result.references.len(), 2);
        assert_eq!(result.references[0].referenced_size, 100000);
        assert_eq!(result.references[0].subsegment_duration, 360000);
    }

    #[test]
    fn parse_sidx_empty_references() {
        let data = build_test_sidx(0, 1000, 0, &[]);
        let result = parse_sidx(&data).unwrap();
        assert_eq!(result.references.len(), 0);
    }

    #[test]
    fn parse_sidx_no_sidx_box_returns_error() {
        // Just an ftyp box, no sidx
        let mut data = Vec::new();
        write_box_header(&mut data, 12, &crate::media::box_type::FTYP);
        data.extend_from_slice(&[0, 0, 0, 0]);
        let result = parse_sidx(&data);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no sidx"));
    }

    #[test]
    fn parse_sidx_duration_to_seconds() {
        let data = build_test_sidx(0, 15360, 0, &[
            (50000, 61440), // 61440 / 15360 = 4.0 seconds
        ]);
        let result = parse_sidx(&data).unwrap();
        let duration_secs = result.references[0].subsegment_duration as f64 / result.timescale as f64;
        assert!((duration_secs - 4.0).abs() < 0.001);
    }
}
