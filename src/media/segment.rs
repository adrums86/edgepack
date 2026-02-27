use crate::drm::cbcs::CbcsDecryptor;
use crate::drm::cenc::{self, CencEncryptor};
use crate::drm::ContentKey;
use crate::error::{EdgePackagerError, Result};
use crate::media::box_type;
use crate::media::cmaf::{
    self, iterate_boxes, parse_senc, parse_trun, BoxHeader, SencEntry, TrackRunBox,
};

/// Parameters for repackaging a media segment from CBCS to CENC.
pub struct SegmentRewriteParams {
    /// Source content key (CBCS) for decryption.
    pub source_key: ContentKey,
    /// Target content key (CENC) for encryption.
    pub target_key: ContentKey,
    /// Per-sample IV size from source tenc box.
    pub source_iv_size: u8,
    /// Per-sample IV size for CENC output (8 or 16).
    pub target_iv_size: u8,
    /// CBCS pattern: crypt byte blocks.
    pub crypt_byte_block: u8,
    /// CBCS pattern: skip byte blocks.
    pub skip_byte_block: u8,
    /// Constant IV from tenc (for CBCS when per_sample_iv_size == 0).
    pub constant_iv: Option<Vec<u8>>,
    /// Segment sequence number (used for generating CENC IVs).
    pub segment_number: u32,
}

/// Rewrite a media segment (moof + mdat) from CBCS to CENC.
///
/// 1. Parse moof to find senc (sample encryption) and trun (sample sizes)
/// 2. Decrypt mdat samples using CBCS
/// 3. Re-encrypt using CENC
/// 4. Rewrite moof with new senc box (CENC IVs)
/// 5. Return rewritten moof + mdat
pub fn rewrite_segment(segment_data: &[u8], params: &SegmentRewriteParams) -> Result<Vec<u8>> {
    // Find moof and mdat boxes
    let mut moof_data: Option<(BoxHeader, &[u8])> = None;
    let mut mdat_data: Option<(BoxHeader, &[u8])> = None;

    for box_result in iterate_boxes(segment_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_bytes = &segment_data[header.offset as usize..box_end.min(segment_data.len())];

        match &header.box_type {
            t if t == &box_type::MOOF => {
                moof_data = Some((header, box_bytes));
            }
            t if t == &box_type::MDAT => {
                mdat_data = Some((header, box_bytes));
            }
            _ => {}
        }
    }

    let (moof_header, moof_bytes) =
        moof_data.ok_or_else(|| EdgePackagerError::SegmentRewrite("no moof box found".into()))?;
    let (mdat_header, mdat_bytes) =
        mdat_data.ok_or_else(|| EdgePackagerError::SegmentRewrite("no mdat box found".into()))?;

    // Parse moof to extract senc and trun
    let (senc_box, trun_box) = parse_moof_encryption_info(moof_bytes, &moof_header, params.source_iv_size)?;

    // Get sample sizes from trun
    let sample_sizes = extract_sample_sizes(&trun_box)?;

    // Extract mdat payload (the actual encrypted media data)
    let mdat_payload = &mdat_bytes[mdat_header.header_size as usize..];
    let mut decrypted_data = mdat_payload.to_vec();

    // Decrypt each sample using CBCS
    let cbcs = CbcsDecryptor::new(
        params.source_key.key.clone().try_into().map_err(|_| {
            EdgePackagerError::Encryption("source key must be 16 bytes".into())
        })?,
        params.crypt_byte_block,
        params.skip_byte_block,
    );

    let mut sample_offset = 0usize;
    for (i, entry) in senc_box.entries.iter().enumerate() {
        let sample_size = sample_sizes.get(i).copied().unwrap_or(0) as usize;
        if sample_offset + sample_size > decrypted_data.len() {
            return Err(EdgePackagerError::SegmentRewrite(format!(
                "sample {i} extends beyond mdat (offset={sample_offset}, size={sample_size}, mdat={})",
                decrypted_data.len()
            )));
        }

        let sample_data = &mut decrypted_data[sample_offset..sample_offset + sample_size];

        // Determine IV for this sample
        let iv = if !entry.iv.is_empty() {
            entry.iv.clone()
        } else if let Some(ref constant) = params.constant_iv {
            constant.clone()
        } else {
            return Err(EdgePackagerError::SegmentRewrite(
                format!("no IV for sample {i}: senc entry has no IV and no constant IV configured")
            ));
        };

        // Pad IV to 16 bytes for CBCS (CBC requires 16-byte IV)
        let iv_16 = pad_iv_to_16(&iv);

        let subsamples: Option<Vec<(u32, u32)>> = entry.subsamples.as_ref().map(|subs| {
            subs.iter()
                .map(|s| (s.clear_bytes as u32, s.encrypted_bytes))
                .collect()
        });

        cbcs.decrypt_sample(sample_data, &iv_16, subsamples.as_deref())?;
        sample_offset += sample_size;
    }

    // Re-encrypt each sample using CENC
    let target_key: [u8; 16] = params.target_key.key.clone().try_into().map_err(|_| {
        EdgePackagerError::Encryption("target key must be 16 bytes".into())
    })?;
    let cenc_enc = CencEncryptor::new(target_key);

    let mut new_senc_entries = Vec::with_capacity(senc_box.entries.len());
    sample_offset = 0;

    for (i, entry) in senc_box.entries.iter().enumerate() {
        let sample_size = sample_sizes.get(i).copied().unwrap_or(0) as usize;
        let sample_data = &mut decrypted_data[sample_offset..sample_offset + sample_size];

        // Generate CENC IV for this sample
        let new_iv = cenc::generate_sample_iv(params.segment_number, i as u32);

        // For CENC, we may preserve subsample structure (required for video NALUs)
        // but use CTR mode encryption instead of CBC pattern
        let subsamples: Option<Vec<(u32, u32)>> = entry.subsamples.as_ref().map(|subs| {
            subs.iter()
                .map(|s| (s.clear_bytes as u32, s.encrypted_bytes))
                .collect()
        });

        cenc_enc.encrypt_sample(sample_data, &new_iv, subsamples.as_deref())?;

        // Build new senc entry with CENC IV
        let new_entry = SencEntry {
            iv: new_iv.to_vec(),
            subsamples: entry.subsamples.clone(),
        };
        new_senc_entries.push(new_entry);

        sample_offset += sample_size;
    }

    // Rebuild moof with new senc box
    let has_subsamples = senc_box.flags & 0x02 != 0;
    let new_moof = rebuild_moof(moof_bytes, &moof_header, &new_senc_entries, has_subsamples, params.source_iv_size)?;

    // Rebuild mdat with re-encrypted data
    let new_mdat = rebuild_mdat(&decrypted_data);

    let mut output = Vec::with_capacity(new_moof.len() + new_mdat.len());
    output.extend_from_slice(&new_moof);
    output.extend_from_slice(&new_mdat);
    Ok(output)
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
        EdgePackagerError::SegmentRewrite("no senc box found in moof/traf".into())
    })?;
    let trun = trun.ok_or_else(|| {
        EdgePackagerError::SegmentRewrite("no trun box found in moof/traf".into())
    })?;

    Ok((senc, trun))
}

/// Extract sample sizes from a trun box.
fn extract_sample_sizes(trun: &TrackRunBox) -> Result<Vec<u32>> {
    trun.entries
        .iter()
        .map(|e| {
            e.sample_size.ok_or_else(|| {
                EdgePackagerError::SegmentRewrite(
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
