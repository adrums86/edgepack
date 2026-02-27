use crate::drm::DrmKeySet;
use crate::error::Result;
use crate::media::box_type;
use crate::media::cmaf::{
    self, BoxHeader, ProtectionSchemeInfo, build_pssh_box, find_child_box, iterate_boxes,
    parse_tenc, read_box_header, PsshBox,
};

/// Rewrite an init segment from CBCS to CENC encryption.
///
/// This modifies:
/// - `schm` box: cbcs → cenc
/// - `tenc` box: update pattern encryption fields for CENC (no pattern)
/// - `pssh` boxes: remove FairPlay, add/update Widevine + PlayReady PSSH data
///
/// Returns the rewritten init segment data.
pub fn rewrite_init_segment(
    init_data: &[u8],
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(init_data.len());

    for box_result in iterate_boxes(init_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &init_data[header.offset as usize..box_end.min(init_data.len())];

        match &header.box_type {
            t if t == &box_type::MOOV => {
                output.extend_from_slice(&rewrite_moov(box_data, &header, key_set, target_iv_size)?);
            }
            _ => {
                // Copy box as-is (ftyp, etc.)
                output.extend_from_slice(box_data);
            }
        }
    }

    Ok(output)
}

/// Rewrite the moov box, recursing into its children.
fn rewrite_moov(
    moov_data: &[u8],
    moov_header: &BoxHeader,
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    let payload = &moov_data[moov_header.header_size as usize..];
    let mut children = Vec::new();

    // Track which PSSH boxes we've already seen
    let mut wrote_new_pssh = false;

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::TRAK => {
                children.extend_from_slice(&rewrite_trak(box_data, &header, key_set, target_iv_size)?);
            }
            t if t == &box_type::PSSH => {
                // Replace all PSSH boxes with our new ones
                if !wrote_new_pssh {
                    children.extend_from_slice(&build_cenc_pssh_boxes(key_set)?);
                    wrote_new_pssh = true;
                }
                // Skip original PSSH (including FairPlay)
            }
            _ => {
                children.extend_from_slice(box_data);
            }
        }
    }

    // If there were no PSSH boxes in the original, add them
    if !wrote_new_pssh {
        children.extend_from_slice(&build_cenc_pssh_boxes(key_set)?);
    }

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::MOOV);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Rewrite a trak box, recursing into stbl/sinf for encryption info.
fn rewrite_trak(
    trak_data: &[u8],
    trak_header: &BoxHeader,
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    let payload = &trak_data[trak_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::MDIA => {
                children.extend_from_slice(&rewrite_container_box(
                    box_data,
                    &header,
                    &box_type::MDIA,
                    key_set,
                    target_iv_size,
                )?);
            }
            _ => {
                children.extend_from_slice(box_data);
            }
        }
    }

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::TRAK);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Generic container box rewriting — recurses into children looking for sinf.
fn rewrite_container_box(
    box_data: &[u8],
    header: &BoxHeader,
    box_type_code: &[u8; 4],
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    let payload = &box_data[header.header_size as usize..];
    let mut children = Vec::new();

    for child_result in iterate_boxes(payload) {
        let child = child_result?;
        let child_end = (child.offset + child.size) as usize;
        let child_data = &payload[child.offset as usize..child_end.min(payload.len())];

        match &child.box_type {
            t if t == &box_type::MINF || t == &box_type::STBL => {
                children.extend_from_slice(&rewrite_container_box(
                    child_data,
                    &child,
                    &child.box_type,
                    key_set,
                    target_iv_size,
                )?);
            }
            t if t == &box_type::STSD => {
                children.extend_from_slice(&rewrite_stsd(child_data, &child, key_set, target_iv_size)?);
            }
            _ => {
                children.extend_from_slice(child_data);
            }
        }
    }

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, box_type_code);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Rewrite the stsd box, looking for sinf children in sample entries.
fn rewrite_stsd(
    stsd_data: &[u8],
    stsd_header: &BoxHeader,
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    // stsd is a full box: version(1) + flags(3) + entry_count(4) + entries
    let payload = &stsd_data[stsd_header.header_size as usize..];
    if payload.len() < 8 {
        // Too small, pass through
        return Ok(stsd_data.to_vec());
    }

    let version_flags = &payload[..4];
    let entry_count = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);

    let mut entries_output = Vec::new();
    let mut offset = 8usize;

    for _ in 0..entry_count {
        if offset + 8 > payload.len() {
            break;
        }
        let entry_header = read_box_header(payload, offset as u64)?;
        let entry_end = (entry_header.offset + entry_header.size) as usize;
        let entry_data = &payload[offset..entry_end.min(payload.len())];

        // Look for sinf inside this sample entry and rewrite it
        entries_output.extend_from_slice(&rewrite_sample_entry(entry_data, &entry_header, key_set, target_iv_size)?);
        offset = entry_end;
    }

    let inner_size = 4 + 4 + entries_output.len();
    let total_size = 8 + inner_size as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::STSD);
    output.extend_from_slice(version_flags);
    output.extend_from_slice(&entry_count.to_be_bytes());
    output.extend_from_slice(&entries_output);
    Ok(output)
}

/// Rewrite a sample entry (e.g., encv, enca), modifying the sinf child.
fn rewrite_sample_entry(
    entry_data: &[u8],
    entry_header: &BoxHeader,
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    // Sample entry format varies (video vs audio), but sinf is always a child box.
    // We scan for sinf and rewrite it; everything else is copied as-is.
    //
    // For video: 8(header) + 8(reserved+data_ref) + 16(video fields) + 4(compressor) + ...
    // For audio: 8(header) + 8(reserved+data_ref) + 20(audio fields) + ...
    //
    // We need to be careful: the sample entry has a fixed prefix before child boxes begin.
    // Rather than trying to parse the exact format, we'll scan for known box types.

    let payload = &entry_data[entry_header.header_size as usize..];

    // The sample entry has a fixed-size prefix (depends on codec), followed by child boxes.
    // We'll do a simple scan: find the sinf box in the payload and split around it.
    let mut output_payload = Vec::new();
    let mut _found_sinf = false;

    // Simple strategy: copy everything, but when we find a 'sinf' box pattern, replace it.
    let mut pos = 0;
    while pos + 8 <= payload.len() {
        // Check if the next 4 bytes at pos+4 are 'sinf'
        if pos + 8 <= payload.len() && &payload[pos + 4..pos + 8] == &box_type::SINF {
            let sinf_size = u32::from_be_bytes([
                payload[pos], payload[pos + 1], payload[pos + 2], payload[pos + 3],
            ]) as usize;

            if sinf_size > 0 && pos + sinf_size <= payload.len() {
                let sinf_data = &payload[pos..pos + sinf_size];
                let sinf_header = read_box_header(sinf_data, 0)?;
                output_payload.extend_from_slice(&rewrite_sinf(sinf_data, &sinf_header, key_set, target_iv_size)?);
                _found_sinf = true;
                pos += sinf_size;
                continue;
            }
        }

        // Not a sinf — copy byte by byte until we find one.
        // (In practice, we'd want a smarter parser, but this works for CMAF.)
        output_payload.push(payload[pos]);
        pos += 1;
    }

    // Copy any remaining bytes
    if pos < payload.len() {
        output_payload.extend_from_slice(&payload[pos..]);
    }

    let total_size = entry_header.header_size as u32 + output_payload.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    // Preserve the original box type (encv, enca, etc.)
    cmaf::write_box_header(&mut output, total_size, &entry_header.box_type);
    output.extend_from_slice(&output_payload);
    Ok(output)
}

/// Rewrite a sinf box from CBCS to CENC.
fn rewrite_sinf(
    sinf_data: &[u8],
    sinf_header: &BoxHeader,
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    let payload = &sinf_data[sinf_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::SCHM => {
                // Rewrite scheme type from cbcs to cenc
                children.extend_from_slice(&build_schm_cenc());
            }
            t if t == &box_type::SCHI => {
                // Rewrite schi container (contains tenc)
                children.extend_from_slice(&rewrite_schi(box_data, &header, key_set, target_iv_size)?);
            }
            _ => {
                // Copy frma and other boxes as-is
                children.extend_from_slice(box_data);
            }
        }
    }

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::SINF);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Rewrite the schi box (contains tenc).
fn rewrite_schi(
    schi_data: &[u8],
    schi_header: &BoxHeader,
    key_set: &DrmKeySet,
    target_iv_size: u8,
) -> Result<Vec<u8>> {
    let payload = &schi_data[schi_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::TENC => {
                // Rewrite tenc for CENC
                let kid = if let Some(key) = key_set.keys.first() {
                    key.kid
                } else {
                    [0u8; 16]
                };
                children.extend_from_slice(&build_tenc_cenc(&kid, target_iv_size));
            }
            _ => {
                children.extend_from_slice(box_data);
            }
        }
    }

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::SCHI);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Build a schm box with scheme_type = "cenc".
fn build_schm_cenc() -> Vec<u8> {
    // schm is a full box: version(1) + flags(3) + scheme_type(4) + scheme_version(4)
    let size: u32 = 8 + 4 + 4 + 4; // header + version/flags + scheme_type + version
    let mut output = Vec::with_capacity(size as usize);
    cmaf::write_box_header(&mut output, size, &box_type::SCHM);
    output.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    output.extend_from_slice(b"cenc");    // scheme_type
    output.extend_from_slice(&0x00010000u32.to_be_bytes()); // scheme_version = 1.0
    output
}

/// Build a tenc box configured for CENC (CTR mode, no pattern).
fn build_tenc_cenc(kid: &[u8; 16], iv_size: u8) -> Vec<u8> {
    // tenc: version(1) + flags(3) + reserved(1) + default_isProtected(1)
    //       + default_Per_Sample_IV_Size(1) + default_KID(16)
    let _size: u32 = 8 + 4 + 2 + 1 + 16;
    // Full box: header(8) + version(1) + flags(3) + reserved/crypt_skip(1)
    //         + isProtected(1) + ivSize(1) + KID(16) = 31
    let total: u32 = 8 + 1 + 3 + 1 + 1 + 1 + 16;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, &box_type::TENC);
    output.push(0); // version
    output.extend_from_slice(&[0u8; 3]); // flags
    output.push(0); // reserved (crypt_byte_block=0, skip_byte_block=0 — no pattern for CENC)
    output.push(1); // default_isProtected = 1
    output.push(iv_size); // default_Per_Sample_IV_Size (8 or 16)
    output.extend_from_slice(kid);
    output
}

/// Build PSSH boxes for CENC (Widevine + PlayReady) from the DRM key set.
fn build_cenc_pssh_boxes(key_set: &DrmKeySet) -> Result<Vec<u8>> {
    let mut output = Vec::new();

    for drm_data in &key_set.drm_systems {
        // Skip FairPlay — not used for CENC output
        if drm_data.system_id == crate::drm::system_ids::FAIRPLAY {
            continue;
        }

        let pssh = PsshBox {
            version: 1,
            system_id: drm_data.system_id,
            key_ids: vec![drm_data.kid],
            data: drm_data.pssh_data.clone(),
        };
        output.extend_from_slice(&build_pssh_box(&pssh));
    }

    Ok(output)
}

/// Parse the protection info from an init segment's sinf box.
/// Returns None if no sinf/encryption is found.
pub fn parse_protection_info(init_data: &[u8]) -> Result<Option<ProtectionSchemeInfo>> {
    // Navigate: moov → trak → mdia → minf → stbl → stsd → sample_entry → sinf
    // This is a simplified search that finds the first sinf box anywhere in the moov.

    fn search_for_sinf(data: &[u8]) -> Option<(usize, usize)> {
        // Scan for 'sinf' box pattern
        for i in 0..data.len().saturating_sub(7) {
            if &data[i + 4..i + 8] == &box_type::SINF {
                let size = u32::from_be_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]) as usize;
                if size >= 8 && i + size <= data.len() {
                    return Some((i, size));
                }
            }
        }
        None
    }

    if let Some((sinf_offset, sinf_size)) = search_for_sinf(init_data) {
        let sinf_data = &init_data[sinf_offset..sinf_offset + sinf_size];
        let sinf_payload = &sinf_data[8..]; // Skip sinf box header

        let mut original_format = [0u8; 4];
        let mut scheme_type = [0u8; 4];
        let mut scheme_version = 0u32;
        let mut tenc = None;

        for box_result in iterate_boxes(sinf_payload) {
            let header = match box_result {
                Ok(h) => h,
                Err(_) => continue,
            };
            let box_end = (header.offset + header.size) as usize;
            let box_data = &sinf_payload[header.offset as usize..box_end.min(sinf_payload.len())];
            let box_payload = &box_data[header.header_size as usize..];

            match &header.box_type {
                t if t == &box_type::FRMA => {
                    if box_payload.len() >= 4 {
                        original_format.copy_from_slice(&box_payload[..4]);
                    }
                }
                t if t == &box_type::SCHM => {
                    if box_payload.len() >= 12 {
                        // version(1) + flags(3) + scheme_type(4) + scheme_version(4)
                        scheme_type.copy_from_slice(&box_payload[4..8]);
                        scheme_version = u32::from_be_bytes([
                            box_payload[8], box_payload[9], box_payload[10], box_payload[11],
                        ]);
                    }
                }
                t if t == &box_type::SCHI => {
                    // Look for tenc inside schi
                    if let Some(tenc_header) = find_child_box(
                        &box_payload[..],
                        &box_type::TENC,
                    ) {
                        // Parse the tenc within schi. Need to account for schi payload offset.
                        let tenc_payload_start = tenc_header.payload_offset() as usize;
                        let tenc_payload_end = (tenc_header.offset + tenc_header.size) as usize;
                        if tenc_payload_end <= box_payload.len() {
                            let tenc_payload = &box_payload[tenc_payload_start..tenc_payload_end];
                            tenc = Some(parse_tenc(tenc_payload)?);
                        }
                    }
                }
                _ => {}
            }
        }

        if let Some(tenc_box) = tenc {
            return Ok(Some(ProtectionSchemeInfo {
                original_format,
                scheme_type,
                scheme_version,
                tenc: tenc_box,
            }));
        }
    }

    Ok(None)
}
