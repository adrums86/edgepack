use crate::error::{EdgePackagerError, Result};
use crate::media::FourCC;

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
        return Err(EdgePackagerError::MediaParse(
            "not enough data for box header".into(),
        ));
    }

    let size32 = u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]);
    let box_type: FourCC = [data[off + 4], data[off + 5], data[off + 6], data[off + 7]];

    if size32 == 1 {
        // Extended size
        if off + 16 > data.len() {
            return Err(EdgePackagerError::MediaParse(
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
        return Err(EdgePackagerError::MediaParse(
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
        return Err(EdgePackagerError::MediaParse("senc box too small".into()));
    }

    let flags = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    let sample_count = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);
    let has_subsamples = flags & 0x02 != 0;

    let mut entries = Vec::with_capacity(sample_count as usize);
    let mut offset = 8usize;

    for _ in 0..sample_count {
        let iv_size = per_sample_iv_size as usize;
        if offset + iv_size > data.len() {
            return Err(EdgePackagerError::MediaParse(
                "senc: not enough data for IV".into(),
            ));
        }
        let iv = data[offset..offset + iv_size].to_vec();
        offset += iv_size;

        let subsamples = if has_subsamples {
            if offset + 2 > data.len() {
                return Err(EdgePackagerError::MediaParse(
                    "senc: not enough data for subsample count".into(),
                ));
            }
            let sub_count = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;

            let mut subs = Vec::with_capacity(sub_count);
            for _ in 0..sub_count {
                if offset + 6 > data.len() {
                    return Err(EdgePackagerError::MediaParse(
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
        return Err(EdgePackagerError::MediaParse("PSSH box too small".into()));
    }

    let version = data[0];
    let mut system_id = [0u8; 16];
    system_id.copy_from_slice(&data[4..20]);

    let mut offset = 20;
    let mut key_ids = Vec::new();

    if version >= 1 {
        if offset + 4 > data.len() {
            return Err(EdgePackagerError::MediaParse(
                "PSSH v1: not enough data for KID count".into(),
            ));
        }
        let kid_count =
            u32::from_be_bytes([data[offset], data[offset + 1], data[offset + 2], data[offset + 3]])
                as usize;
        offset += 4;

        for _ in 0..kid_count {
            if offset + 16 > data.len() {
                return Err(EdgePackagerError::MediaParse(
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
        return Err(EdgePackagerError::MediaParse(
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
        return Err(EdgePackagerError::MediaParse("trun box too small".into()));
    }

    let flags = u32::from_be_bytes([0, data[1], data[2], data[3]]);
    let sample_count = u32::from_be_bytes([data[4], data[5], data[6], data[7]]);

    let mut offset = 8usize;

    let data_offset = if flags & 0x0001 != 0 {
        if offset + 4 > data.len() {
            return Err(EdgePackagerError::MediaParse(
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
            return Err(EdgePackagerError::MediaParse(
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
                return Err(EdgePackagerError::MediaParse(
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
                return Err(EdgePackagerError::MediaParse(
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
                return Err(EdgePackagerError::MediaParse(
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
                return Err(EdgePackagerError::MediaParse(
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
