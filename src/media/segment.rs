use crate::drm::sample_cryptor::{create_decryptor, create_encryptor};
use crate::drm::scheme::EncryptionScheme;
use crate::drm::ContentKey;
use crate::error::{EdgepackError, Result};
use crate::media::box_type;
use crate::media::cmaf::{
    self, iterate_boxes, parse_senc, parse_trun, BoxHeader, SencEntry, TrackRunBox,
};

/// Parameters for repackaging a media segment between encryption schemes.
///
/// Supports any combination: encrypted↔encrypted, clear→encrypted,
/// encrypted→clear, and clear→clear (pass-through).
pub struct SegmentRewriteParams {
    /// Source content key for decryption. None when source is clear.
    pub source_key: Option<ContentKey>,
    /// Target content key for encryption. None when target is clear.
    pub target_key: Option<ContentKey>,
    /// Source encryption scheme.
    pub source_scheme: EncryptionScheme,
    /// Target encryption scheme.
    pub target_scheme: EncryptionScheme,
    /// Per-sample IV size from source tenc box.
    pub source_iv_size: u8,
    /// Per-sample IV size for output.
    pub target_iv_size: u8,
    /// Source encryption pattern: (crypt_byte_block, skip_byte_block).
    pub source_pattern: (u8, u8),
    /// Target encryption pattern: (crypt_byte_block, skip_byte_block).
    pub target_pattern: (u8, u8),
    /// Constant IV from tenc (for CBCS when per_sample_iv_size == 0).
    pub constant_iv: Option<Vec<u8>>,
    /// Segment sequence number (used for generating IVs).
    pub segment_number: u32,
}

/// Rewrite a media segment (moof + mdat) between encryption schemes.
///
/// Dispatches to the appropriate handler based on source/target encryption:
/// - encrypted→encrypted: decrypt + re-encrypt, rewrite senc
/// - clear→encrypted: encrypt, inject senc
/// - encrypted→clear: decrypt, strip senc
/// - clear→clear: pass-through
pub fn rewrite_segment(segment_data: &[u8], params: &SegmentRewriteParams) -> Result<Vec<u8>> {
    match (params.source_scheme.is_encrypted(), params.target_scheme.is_encrypted()) {
        (true, true) => rewrite_encrypted_to_encrypted(segment_data, params),
        (false, true) => rewrite_clear_to_encrypted(segment_data, params),
        (true, false) => rewrite_encrypted_to_clear(segment_data, params),
        (false, false) => Ok(segment_data.to_vec()), // pass-through
    }
}

/// Rewrite an encrypted segment to a different encryption scheme.
fn rewrite_encrypted_to_encrypted(segment_data: &[u8], params: &SegmentRewriteParams) -> Result<Vec<u8>> {
    let (moof_header, moof_bytes, mdat_header, mdat_bytes) = find_moof_mdat(segment_data)?;

    // Parse moof to extract senc and trun
    let (senc_box, trun_box) = parse_moof_encryption_info(moof_bytes, &moof_header, params.source_iv_size)?;
    let sample_sizes = extract_sample_sizes(&trun_box)?;

    // Extract mdat payload
    let mdat_payload = &mdat_bytes[mdat_header.header_size as usize..];
    let mut data = mdat_payload.to_vec();

    // Decrypt each sample
    let source_key = params.source_key.as_ref().ok_or_else(|| {
        EdgepackError::SegmentRewrite("source_key required for encrypted source".into())
    })?;
    let source_key_bytes: [u8; 16] = source_key.key.clone().try_into().map_err(|_| {
        EdgepackError::Encryption("source key must be 16 bytes".into())
    })?;
    let decryptor = create_decryptor(params.source_scheme, source_key_bytes, params.source_pattern);

    let mut sample_offset = 0usize;
    for (i, entry) in senc_box.entries.iter().enumerate() {
        let sample_size = sample_sizes.get(i).copied().unwrap_or(0) as usize;
        if sample_offset + sample_size > data.len() {
            return Err(EdgepackError::SegmentRewrite(format!(
                "sample {i} extends beyond mdat (offset={sample_offset}, size={sample_size}, mdat={})",
                data.len()
            )));
        }

        let sample_data = &mut data[sample_offset..sample_offset + sample_size];
        let iv = resolve_iv(i, entry, &params.constant_iv)?;
        let decryption_iv = if params.source_scheme == EncryptionScheme::Cbcs {
            pad_iv_to_16(&iv)
        } else {
            iv
        };

        let subsamples = subsample_pairs(entry);
        decryptor.decrypt_sample(sample_data, &decryption_iv, subsamples.as_deref())?;
        sample_offset += sample_size;
    }

    // Re-encrypt each sample
    let target_key = params.target_key.as_ref().ok_or_else(|| {
        EdgepackError::SegmentRewrite("target_key required for encrypted target".into())
    })?;
    let target_key_bytes: [u8; 16] = target_key.key.clone().try_into().map_err(|_| {
        EdgepackError::Encryption("target key must be 16 bytes".into())
    })?;
    let encryptor = create_encryptor(params.target_scheme, target_key_bytes, params.target_pattern);

    let mut new_senc_entries = Vec::with_capacity(senc_box.entries.len());
    sample_offset = 0;

    for (i, entry) in senc_box.entries.iter().enumerate() {
        let sample_size = sample_sizes.get(i).copied().unwrap_or(0) as usize;
        let sample_data = &mut data[sample_offset..sample_offset + sample_size];

        let new_iv = encryptor.generate_iv(params.segment_number, i as u32);
        let subsamples = subsample_pairs(entry);
        encryptor.encrypt_sample(sample_data, &new_iv, subsamples.as_deref())?;

        new_senc_entries.push(SencEntry {
            iv: new_iv,
            subsamples: entry.subsamples.clone(),
        });
        sample_offset += sample_size;
    }

    // Rebuild moof with new senc box
    let has_subsamples = senc_box.flags & 0x02 != 0;
    let new_moof = rebuild_moof(moof_bytes, &moof_header, &new_senc_entries, has_subsamples, params.source_iv_size)?;
    let new_mdat = rebuild_mdat(&data);

    let mut output = Vec::with_capacity(new_moof.len() + new_mdat.len());
    output.extend_from_slice(&new_moof);
    output.extend_from_slice(&new_mdat);
    Ok(output)
}

/// Encrypt a clear segment (no senc expected) to produce an encrypted segment.
fn rewrite_clear_to_encrypted(segment_data: &[u8], params: &SegmentRewriteParams) -> Result<Vec<u8>> {
    let (moof_header, moof_bytes, mdat_header, mdat_bytes) = find_moof_mdat(segment_data)?;

    // Parse trun only — clear segments have no senc
    let trun_box = parse_moof_trun_only(moof_bytes, &moof_header)?;
    let sample_sizes = extract_sample_sizes(&trun_box)?;

    // Extract mdat payload
    let mdat_payload = &mdat_bytes[mdat_header.header_size as usize..];
    let mut data = mdat_payload.to_vec();

    // Encrypt each sample
    let target_key = params.target_key.as_ref().ok_or_else(|| {
        EdgepackError::SegmentRewrite("target_key required for encrypted target".into())
    })?;
    let target_key_bytes: [u8; 16] = target_key.key.clone().try_into().map_err(|_| {
        EdgepackError::Encryption("target key must be 16 bytes".into())
    })?;
    let encryptor = create_encryptor(params.target_scheme, target_key_bytes, params.target_pattern);

    let mut new_senc_entries = Vec::with_capacity(sample_sizes.len());
    let mut sample_offset = 0usize;

    for (i, &size) in sample_sizes.iter().enumerate() {
        let sample_size = size as usize;
        if sample_offset + sample_size > data.len() {
            return Err(EdgepackError::SegmentRewrite(format!(
                "sample {i} extends beyond mdat (offset={sample_offset}, size={sample_size}, mdat={})",
                data.len()
            )));
        }

        let sample_data = &mut data[sample_offset..sample_offset + sample_size];
        let new_iv = encryptor.generate_iv(params.segment_number, i as u32);

        // No subsamples for clear-to-encrypted — encrypt entire sample
        encryptor.encrypt_sample(sample_data, &new_iv, None)?;

        new_senc_entries.push(SencEntry {
            iv: new_iv,
            subsamples: None,
        });
        sample_offset += sample_size;
    }

    // Rebuild moof with injected senc
    let new_moof = rebuild_moof_inject_senc(moof_bytes, &moof_header, &new_senc_entries)?;
    let new_mdat = rebuild_mdat(&data);

    let mut output = Vec::with_capacity(new_moof.len() + new_mdat.len());
    output.extend_from_slice(&new_moof);
    output.extend_from_slice(&new_mdat);
    Ok(output)
}

/// Decrypt an encrypted segment to produce a clear segment (strip senc).
fn rewrite_encrypted_to_clear(segment_data: &[u8], params: &SegmentRewriteParams) -> Result<Vec<u8>> {
    let (moof_header, moof_bytes, mdat_header, mdat_bytes) = find_moof_mdat(segment_data)?;

    // Parse senc + trun
    let (senc_box, trun_box) = parse_moof_encryption_info(moof_bytes, &moof_header, params.source_iv_size)?;
    let sample_sizes = extract_sample_sizes(&trun_box)?;

    // Extract mdat payload
    let mdat_payload = &mdat_bytes[mdat_header.header_size as usize..];
    let mut data = mdat_payload.to_vec();

    // Decrypt each sample
    let source_key = params.source_key.as_ref().ok_or_else(|| {
        EdgepackError::SegmentRewrite("source_key required for encrypted source".into())
    })?;
    let source_key_bytes: [u8; 16] = source_key.key.clone().try_into().map_err(|_| {
        EdgepackError::Encryption("source key must be 16 bytes".into())
    })?;
    let decryptor = create_decryptor(params.source_scheme, source_key_bytes, params.source_pattern);

    let mut sample_offset = 0usize;
    for (i, entry) in senc_box.entries.iter().enumerate() {
        let sample_size = sample_sizes.get(i).copied().unwrap_or(0) as usize;
        if sample_offset + sample_size > data.len() {
            return Err(EdgepackError::SegmentRewrite(format!(
                "sample {i} extends beyond mdat (offset={sample_offset}, size={sample_size}, mdat={})",
                data.len()
            )));
        }

        let sample_data = &mut data[sample_offset..sample_offset + sample_size];
        let iv = resolve_iv(i, entry, &params.constant_iv)?;
        let decryption_iv = if params.source_scheme == EncryptionScheme::Cbcs {
            pad_iv_to_16(&iv)
        } else {
            iv
        };

        let subsamples = subsample_pairs(entry);
        decryptor.decrypt_sample(sample_data, &decryption_iv, subsamples.as_deref())?;
        sample_offset += sample_size;
    }

    // Rebuild moof without senc
    let new_moof = rebuild_moof_strip_senc(moof_bytes, &moof_header)?;
    let new_mdat = rebuild_mdat(&data);

    let mut output = Vec::with_capacity(new_moof.len() + new_mdat.len());
    output.extend_from_slice(&new_moof);
    output.extend_from_slice(&new_mdat);
    Ok(output)
}

/// Find moof and mdat boxes in segment data.
fn find_moof_mdat<'a>(segment_data: &'a [u8]) -> Result<(BoxHeader, &'a [u8], BoxHeader, &'a [u8])> {
    let mut moof_data: Option<(BoxHeader, &[u8])> = None;
    let mut mdat_data: Option<(BoxHeader, &[u8])> = None;

    for box_result in iterate_boxes(segment_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &segment_data[header.offset as usize..box_end.min(segment_data.len())];

        match &header.box_type {
            t if t == &box_type::MOOF => moof_data = Some((header, box_bytes)),
            t if t == &box_type::MDAT => mdat_data = Some((header, box_bytes)),
            _ => {}
        }
    }

    let (moof_header, moof_bytes) =
        moof_data.ok_or_else(|| EdgepackError::SegmentRewrite("no moof box found".into()))?;
    let (mdat_header, mdat_bytes) =
        mdat_data.ok_or_else(|| EdgepackError::SegmentRewrite("no mdat box found".into()))?;

    Ok((moof_header, moof_bytes, mdat_header, mdat_bytes))
}

/// Resolve the IV for a sample from the senc entry or constant IV.
fn resolve_iv(sample_index: usize, entry: &SencEntry, constant_iv: &Option<Vec<u8>>) -> Result<Vec<u8>> {
    if !entry.iv.is_empty() {
        Ok(entry.iv.clone())
    } else if let Some(ref constant) = constant_iv {
        Ok(constant.clone())
    } else {
        Err(EdgepackError::SegmentRewrite(
            format!("no IV for sample {sample_index}: senc entry has no IV and no constant IV configured")
        ))
    }
}

/// Extract subsample pairs from a senc entry.
fn subsample_pairs(entry: &SencEntry) -> Option<Vec<(u32, u32)>> {
    entry.subsamples.as_ref().map(|subs| {
        subs.iter()
            .map(|s| (s.clear_bytes as u32, s.encrypted_bytes))
            .collect()
    })
}

/// Parse the moof box to extract senc and trun information.
fn parse_moof_encryption_info(
    moof_data: &[u8],
    moof_header: &BoxHeader,
    iv_size: u8,
) -> Result<(cmaf::SampleEncryptionBox, TrackRunBox)> {
    let payload = &moof_data[moof_header.header_size as usize..];

    let mut senc: Option<cmaf::SampleEncryptionBox> = None;
    let mut trun: Option<TrackRunBox> = None;

    // Recurse into traf to find senc and trun
    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        if header.box_type == box_type::TRAF {
            let traf_payload = &box_data[header.header_size as usize..];
            for child_result in iterate_boxes(traf_payload) {
                let child = child_result?;
                let child_end = (child.offset + child.size) as usize;
                let child_data =
                    &traf_payload[child.offset as usize..child_end.min(traf_payload.len())];
                let child_payload = &child_data[child.header_size as usize..];

                match &child.box_type {
                    t if t == &box_type::SENC => {
                        senc = Some(parse_senc(child_payload, iv_size)?);
                    }
                    t if t == &box_type::TRUN => {
                        trun = Some(parse_trun(child_payload)?);
                    }
                    _ => {}
                }
            }
        }
    }

    let senc = senc.ok_or_else(|| {
        EdgepackError::SegmentRewrite("no senc box found in moof/traf".into())
    })?;
    let trun = trun.ok_or_else(|| {
        EdgepackError::SegmentRewrite("no trun box found in moof/traf".into())
    })?;

    Ok((senc, trun))
}

/// Extract sample sizes from a trun box.
fn extract_sample_sizes(trun: &TrackRunBox) -> Result<Vec<u32>> {
    trun.entries
        .iter()
        .map(|e| {
            e.sample_size.ok_or_else(|| {
                EdgepackError::SegmentRewrite(
                    "trun missing sample_size (flag 0x0200 not set)".into(),
                )
            })
        })
        .collect()
}

/// Rebuild the moof box with a new senc box.
fn rebuild_moof(
    original_moof: &[u8],
    moof_header: &BoxHeader,
    new_senc_entries: &[SencEntry],
    has_subsamples: bool,
    _original_iv_size: u8,
) -> Result<Vec<u8>> {
    let payload = &original_moof[moof_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        if header.box_type == box_type::TRAF {
            // Rebuild traf with new senc
            children.extend_from_slice(&rebuild_traf(
                box_data,
                &header,
                new_senc_entries,
                has_subsamples,
            )?);
        } else {
            children.extend_from_slice(box_data);
        }
    }

    let total = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, &box_type::MOOF);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Parse moof to extract trun only (no senc expected — for clear content).
fn parse_moof_trun_only(
    moof_data: &[u8],
    moof_header: &BoxHeader,
) -> Result<TrackRunBox> {
    let payload = &moof_data[moof_header.header_size as usize..];
    let mut trun: Option<TrackRunBox> = None;

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        if header.box_type == box_type::TRAF {
            let traf_payload = &box_data[header.header_size as usize..];
            for child_result in iterate_boxes(traf_payload) {
                let child = child_result?;
                let child_end = (child.offset + child.size) as usize;
                let child_data =
                    &traf_payload[child.offset as usize..child_end.min(traf_payload.len())];
                let child_payload = &child_data[child.header_size as usize..];

                if child.box_type == box_type::TRUN {
                    trun = Some(parse_trun(child_payload)?);
                }
            }
        }
    }

    trun.ok_or_else(|| {
        EdgepackError::SegmentRewrite("no trun box found in moof/traf".into())
    })
}

/// Rebuild moof with a new senc injected into traf (for clear-to-encrypted).
fn rebuild_moof_inject_senc(
    original_moof: &[u8],
    moof_header: &BoxHeader,
    new_senc_entries: &[SencEntry],
) -> Result<Vec<u8>> {
    let payload = &original_moof[moof_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        if header.box_type == box_type::TRAF {
            // Rebuild traf with senc appended
            let traf_payload = &box_data[header.header_size as usize..];
            let mut traf_children = Vec::new();

            for child_result in iterate_boxes(traf_payload) {
                let child = child_result?;
                let child_end = (child.offset + child.size) as usize;
                let child_data =
                    &traf_payload[child.offset as usize..child_end.min(traf_payload.len())];
                traf_children.extend_from_slice(child_data);
            }

            // Append new senc box
            traf_children.extend_from_slice(&cmaf::build_senc_box(new_senc_entries, false));

            let traf_total = 8 + traf_children.len() as u32;
            cmaf::write_box_header(&mut children, traf_total, &box_type::TRAF);
            children.extend_from_slice(&traf_children);
        } else {
            children.extend_from_slice(box_data);
        }
    }

    let total = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, &box_type::MOOF);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Rebuild moof with senc/saiz/saio removed from traf (for encrypted-to-clear).
fn rebuild_moof_strip_senc(
    original_moof: &[u8],
    moof_header: &BoxHeader,
) -> Result<Vec<u8>> {
    let payload = &original_moof[moof_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        if header.box_type == box_type::TRAF {
            // Rebuild traf without senc/saiz/saio
            let traf_payload = &box_data[header.header_size as usize..];
            let mut traf_children = Vec::new();

            for child_result in iterate_boxes(traf_payload) {
                let child = child_result?;
                let child_end = (child.offset + child.size) as usize;
                let child_data =
                    &traf_payload[child.offset as usize..child_end.min(traf_payload.len())];

                match &child.box_type {
                    t if t == &box_type::SENC || t == &box_type::SAIZ || t == &box_type::SAIO => {
                        // Strip encryption-related boxes
                    }
                    _ => {
                        traf_children.extend_from_slice(child_data);
                    }
                }
            }

            let traf_total = 8 + traf_children.len() as u32;
            cmaf::write_box_header(&mut children, traf_total, &box_type::TRAF);
            children.extend_from_slice(&traf_children);
        } else {
            children.extend_from_slice(box_data);
        }
    }

    let total = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, &box_type::MOOF);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Rebuild a traf box, replacing the senc with a new one.
fn rebuild_traf(
    traf_data: &[u8],
    traf_header: &BoxHeader,
    new_senc_entries: &[SencEntry],
    has_subsamples: bool,
) -> Result<Vec<u8>> {
    let payload = &traf_data[traf_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::SENC => {
                // Replace with new CENC senc
                children.extend_from_slice(&cmaf::build_senc_box(new_senc_entries, has_subsamples));
            }
            t if t == &box_type::SAIZ || t == &box_type::SAIO => {
                // Drop saiz/saio — they reference the old senc layout
                // They'll be recalculated if needed by the player
            }
            _ => {
                children.extend_from_slice(box_data);
            }
        }
    }

    let total = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, &box_type::TRAF);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Build a new mdat box with the given payload.
fn rebuild_mdat(payload: &[u8]) -> Vec<u8> {
    let total = 8 + payload.len() as u32;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, &box_type::MDAT);
    output.extend_from_slice(payload);
    output
}

/// Pad an IV to 16 bytes (required for CBC mode).
fn pad_iv_to_16(iv: &[u8]) -> Vec<u8> {
    let mut padded = vec![0u8; 16];
    let start = 16 - iv.len().min(16);
    padded[start..].copy_from_slice(&iv[..iv.len().min(16)]);
    padded
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::cmaf;

    #[test]
    fn pad_iv_to_16_from_8_bytes() {
        let iv = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        let padded = pad_iv_to_16(&iv);
        assert_eq!(padded.len(), 16);
        assert_eq!(&padded[..8], &[0u8; 8]);
        assert_eq!(&padded[8..], &iv);
    }

    #[test]
    fn pad_iv_to_16_from_16_bytes() {
        let iv = [0xAA; 16];
        let padded = pad_iv_to_16(&iv);
        assert_eq!(padded, iv.to_vec());
    }

    #[test]
    fn pad_iv_to_16_empty() {
        let padded = pad_iv_to_16(&[]);
        assert_eq!(padded, vec![0u8; 16]);
    }

    #[test]
    fn pad_iv_to_16_single_byte() {
        let padded = pad_iv_to_16(&[0xFF]);
        assert_eq!(padded[15], 0xFF);
        assert_eq!(&padded[..15], &[0u8; 15]);
    }

    #[test]
    fn extract_sample_sizes_all_present() {
        let trun = TrackRunBox {
            flags: 0x0200,
            sample_count: 2,
            data_offset: None,
            first_sample_flags: None,
            entries: vec![
                crate::media::cmaf::TrunEntry {
                    sample_duration: None,
                    sample_size: Some(100),
                    sample_flags: None,
                    sample_composition_time_offset: None,
                },
                crate::media::cmaf::TrunEntry {
                    sample_duration: None,
                    sample_size: Some(200),
                    sample_flags: None,
                    sample_composition_time_offset: None,
                },
            ],
        };
        let sizes = extract_sample_sizes(&trun).unwrap();
        assert_eq!(sizes, vec![100, 200]);
    }

    #[test]
    fn extract_sample_sizes_missing_returns_error() {
        let trun = TrackRunBox {
            flags: 0,
            sample_count: 1,
            data_offset: None,
            first_sample_flags: None,
            entries: vec![crate::media::cmaf::TrunEntry {
                sample_duration: None,
                sample_size: None,
                sample_flags: None,
                sample_composition_time_offset: None,
            }],
        };
        let result = extract_sample_sizes(&trun);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("sample_size"));
    }

    #[test]
    fn rebuild_mdat_produces_valid_box() {
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let mdat = rebuild_mdat(&payload);
        // Header: 4 bytes size + 4 bytes type + payload
        assert_eq!(mdat.len(), 8 + 4);
        assert_eq!(&mdat[4..8], b"mdat");
        assert_eq!(&mdat[8..], &payload);
        let size = u32::from_be_bytes([mdat[0], mdat[1], mdat[2], mdat[3]]);
        assert_eq!(size, 12);
    }

    #[test]
    fn rebuild_mdat_empty_payload() {
        let mdat = rebuild_mdat(&[]);
        assert_eq!(mdat.len(), 8);
        assert_eq!(&mdat[4..8], b"mdat");
    }

    #[test]
    fn segment_rewrite_params_construction() {
        let params = SegmentRewriteParams {
            source_key: Some(ContentKey {
                kid: [0x01; 16],
                key: vec![0xAA; 16],
                iv: None,
            }),
            target_key: Some(ContentKey {
                kid: [0x02; 16],
                key: vec![0xBB; 16],
                iv: None,
            }),
            source_scheme: EncryptionScheme::Cbcs,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 8,
            target_iv_size: 8,
            source_pattern: (1, 9),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 42,
        };
        assert_eq!(params.source_scheme, EncryptionScheme::Cbcs);
        assert_eq!(params.target_scheme, EncryptionScheme::Cenc);
        assert_eq!(params.source_iv_size, 8);
        assert_eq!(params.target_iv_size, 8);
        assert_eq!(params.source_pattern, (1, 9));
        assert_eq!(params.target_pattern, (0, 0));
        assert_eq!(params.segment_number, 42);
    }

    #[test]
    fn rewrite_segment_missing_moof_returns_error() {
        // Only mdat, no moof
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, 12, b"mdat");
        data.extend_from_slice(&[0u8; 4]);

        let params = SegmentRewriteParams {
            source_key: Some(ContentKey { kid: [0; 16], key: vec![0; 16], iv: None }),
            target_key: Some(ContentKey { kid: [0; 16], key: vec![0; 16], iv: None }),
            source_scheme: EncryptionScheme::Cbcs,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 8,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        };
        let result = rewrite_segment(&data, &params);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no moof"));
    }

    #[test]
    fn rewrite_segment_missing_mdat_returns_error() {
        // Only moof (with no senc/trun inside, but the mdat check comes first)
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, 12, b"moof");
        data.extend_from_slice(&[0u8; 4]);

        let params = SegmentRewriteParams {
            source_key: Some(ContentKey { kid: [0; 16], key: vec![0; 16], iv: None }),
            target_key: Some(ContentKey { kid: [0; 16], key: vec![0; 16], iv: None }),
            source_scheme: EncryptionScheme::Cbcs,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 8,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        };
        let result = rewrite_segment(&data, &params);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no mdat"));
    }

    /// Build a minimal clear segment: moof { traf { tfhd, trun } } + mdat
    fn build_clear_segment(sample_sizes: &[u32]) -> Vec<u8> {
        let mut data = Vec::new();

        // Build trun
        let trun_flags: u32 = 0x000201; // data_offset_present | sample_size_present
        let mut trun_payload = Vec::new();
        trun_payload.push(0); // version
        trun_payload.extend_from_slice(&trun_flags.to_be_bytes()[1..4]);
        trun_payload.extend_from_slice(&(sample_sizes.len() as u32).to_be_bytes());
        trun_payload.extend_from_slice(&0i32.to_be_bytes()); // data_offset (placeholder)
        for &size in sample_sizes {
            trun_payload.extend_from_slice(&size.to_be_bytes());
        }
        let trun_size = 8 + trun_payload.len() as u32;
        let mut trun = Vec::new();
        cmaf::write_box_header(&mut trun, trun_size, b"trun");
        trun.extend_from_slice(&trun_payload);

        // Build tfhd (minimal)
        let mut tfhd = Vec::new();
        cmaf::write_box_header(&mut tfhd, 16, b"tfhd");
        tfhd.extend_from_slice(&[0u8; 4]); // version + flags
        tfhd.extend_from_slice(&1u32.to_be_bytes()); // track_id

        // traf { tfhd, trun }
        let traf_size = 8 + tfhd.len() as u32 + trun.len() as u32;
        let mut traf = Vec::new();
        cmaf::write_box_header(&mut traf, traf_size, b"traf");
        traf.extend_from_slice(&tfhd);
        traf.extend_from_slice(&trun);

        // moof { traf }
        let moof_size = 8 + traf.len() as u32;
        cmaf::write_box_header(&mut data, moof_size, b"moof");
        data.extend_from_slice(&traf);

        // mdat with sample data
        let total_mdat_payload: usize = sample_sizes.iter().map(|&s| s as usize).sum();
        let mdat_payload: Vec<u8> = (0..total_mdat_payload).map(|i| (i % 256) as u8).collect();
        let mdat_size = 8 + mdat_payload.len() as u32;
        cmaf::write_box_header(&mut data, mdat_size, b"mdat");
        data.extend_from_slice(&mdat_payload);

        data
    }

    #[test]
    fn clear_to_clear_passthrough() {
        let segment = build_clear_segment(&[64, 32]);
        let params = SegmentRewriteParams {
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
        let result = rewrite_segment(&segment, &params).unwrap();
        assert_eq!(result, segment); // byte-for-byte identical
    }

    #[test]
    fn clear_to_encrypted_injects_senc() {
        let segment = build_clear_segment(&[48, 48]);
        let params = SegmentRewriteParams {
            source_key: None,
            target_key: Some(ContentKey { kid: [0x01; 16], key: vec![0xAA; 16], iv: None }),
            source_scheme: EncryptionScheme::None,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 0,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        };
        let result = rewrite_segment(&segment, &params).unwrap();
        // Result should contain senc box
        assert!(result.windows(4).any(|w| w == b"senc"));
        // mdat should be different (encrypted)
        assert_ne!(result, segment);
    }

    #[test]
    fn clear_to_encrypted_then_decrypt_roundtrip() {
        let segment = build_clear_segment(&[48]);
        let key = ContentKey { kid: [0x01; 16], key: vec![0xAA; 16], iv: None };

        // Clear → Encrypted
        let encrypted = rewrite_segment(&segment, &SegmentRewriteParams {
            source_key: None,
            target_key: Some(key.clone()),
            source_scheme: EncryptionScheme::None,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 0,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        }).unwrap();

        // Encrypted → Clear
        let decrypted = rewrite_segment(&encrypted, &SegmentRewriteParams {
            source_key: Some(key),
            target_key: None,
            source_scheme: EncryptionScheme::Cenc,
            target_scheme: EncryptionScheme::None,
            source_iv_size: 8,
            target_iv_size: 0,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        }).unwrap();

        // Extract mdat payloads from original and decrypted
        let orig_mdat_pos = segment.windows(4).position(|w| w == b"mdat").unwrap() - 4;
        let orig_mdat_size = u32::from_be_bytes([
            segment[orig_mdat_pos], segment[orig_mdat_pos + 1],
            segment[orig_mdat_pos + 2], segment[orig_mdat_pos + 3],
        ]) as usize;
        let orig_payload = &segment[orig_mdat_pos + 8..orig_mdat_pos + orig_mdat_size];

        let dec_mdat_pos = decrypted.windows(4).position(|w| w == b"mdat").unwrap() - 4;
        let dec_mdat_size = u32::from_be_bytes([
            decrypted[dec_mdat_pos], decrypted[dec_mdat_pos + 1],
            decrypted[dec_mdat_pos + 2], decrypted[dec_mdat_pos + 3],
        ]) as usize;
        let dec_payload = &decrypted[dec_mdat_pos + 8..dec_mdat_pos + dec_mdat_size];

        assert_eq!(orig_payload, dec_payload, "mdat payload should match after roundtrip");
    }

    #[test]
    fn encrypted_to_clear_strips_senc() {
        let segment = build_clear_segment(&[48]);
        let key = ContentKey { kid: [0x01; 16], key: vec![0xAA; 16], iv: None };

        // First encrypt
        let encrypted = rewrite_segment(&segment, &SegmentRewriteParams {
            source_key: None,
            target_key: Some(key.clone()),
            source_scheme: EncryptionScheme::None,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 0,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        }).unwrap();
        assert!(encrypted.windows(4).any(|w| w == b"senc"));

        // Then decrypt to clear
        let clear = rewrite_segment(&encrypted, &SegmentRewriteParams {
            source_key: Some(key),
            target_key: None,
            source_scheme: EncryptionScheme::Cenc,
            target_scheme: EncryptionScheme::None,
            source_iv_size: 8,
            target_iv_size: 0,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        }).unwrap();

        // senc should be removed
        assert!(!clear.windows(4).any(|w| w == b"senc"));
    }

    #[test]
    fn clear_to_encrypted_missing_target_key_errors() {
        let segment = build_clear_segment(&[48]);
        let result = rewrite_segment(&segment, &SegmentRewriteParams {
            source_key: None,
            target_key: None, // Missing!
            source_scheme: EncryptionScheme::None,
            target_scheme: EncryptionScheme::Cenc,
            source_iv_size: 0,
            target_iv_size: 8,
            source_pattern: (0, 0),
            target_pattern: (0, 0),
            constant_iv: None,
            segment_number: 0,
        });
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("target_key"));
    }
}
