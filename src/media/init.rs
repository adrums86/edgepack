use crate::drm::scheme::EncryptionScheme;
use crate::drm::DrmKeySet;
use crate::error::Result;
use crate::media::box_type;
use crate::media::codec::TrackKeyMapping;
use crate::media::container::ContainerFormat;
use crate::media::cmaf::{
    self, BoxHeader, ProtectionSchemeInfo, build_pssh_box, find_child_box, iterate_boxes,
    parse_tenc, read_box_header, PsshBox,
};
use crate::media::TrackType;

/// Rewrite an init segment to the target encryption scheme.
///
/// This modifies:
/// - `schm` box: set scheme type to target (cbcs/cenc)
/// - `tenc` box: update pattern encryption fields for target scheme
/// - `pssh` boxes: filter/add PSSH boxes appropriate for the target scheme
///
/// The `key_mapping` parameter controls which KID is used per track type.
/// For single-key content, use `TrackKeyMapping::single(kid)`.
///
/// Returns the rewritten init segment data.
pub fn rewrite_init_segment(
    init_data: &[u8],
    key_set: &DrmKeySet,
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
    container_format: ContainerFormat,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(init_data.len());

    for box_result in iterate_boxes(init_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &init_data[header.offset as usize..box_end.min(init_data.len())];

        match &header.box_type {
            t if t == &box_type::FTYP => {
                // Rewrite ftyp box with container-format-specific brands
                output.extend_from_slice(&container_format.build_ftyp());
            }
            t if t == &box_type::MOOV => {
                output.extend_from_slice(&rewrite_moov(box_data, &header, key_set, key_mapping, target_scheme, target_iv_size, target_pattern)?);
            }
            _ => {
                // Copy other boxes as-is
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
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
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
                children.extend_from_slice(&rewrite_trak(box_data, &header, key_mapping, target_scheme, target_iv_size, target_pattern)?);
            }
            t if t == &box_type::PSSH => {
                // Replace all PSSH boxes with our new ones
                if !wrote_new_pssh {
                    children.extend_from_slice(&build_pssh_boxes(key_set, key_mapping, target_scheme)?);
                    wrote_new_pssh = true;
                }
                // Skip original PSSH (including FairPlay for CENC output)
            }
            _ => {
                children.extend_from_slice(box_data);
            }
        }
    }

    // If there were no PSSH boxes in the original, add them
    if !wrote_new_pssh {
        children.extend_from_slice(&build_pssh_boxes(key_set, key_mapping, target_scheme)?);
    }

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::MOOV);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Rewrite a trak box, recursing into stbl/sinf for encryption info.
///
/// Detects track type from the hdlr box inside mdia and selects the
/// appropriate KID from the key mapping.
fn rewrite_trak(
    trak_data: &[u8],
    trak_header: &BoxHeader,
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
) -> Result<Vec<u8>> {
    let payload = &trak_data[trak_header.header_size as usize..];

    // Detect track type from hdlr box to select the correct KID
    let track_type = detect_track_type(payload);
    let kid = key_mapping
        .kid_for_track(track_type)
        .copied()
        .unwrap_or([0u8; 16]);

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
                    &kid,
                    target_scheme,
                    target_iv_size,
                    target_pattern,
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

/// Detect track type by searching for hdlr box in trak → mdia.
fn detect_track_type(trak_payload: &[u8]) -> TrackType {
    // Look for mdia box
    if let Some(mdia_header) = find_child_box(trak_payload, &box_type::MDIA) {
        let mdia_end = (mdia_header.offset + mdia_header.size) as usize;
        let mdia_data = &trak_payload[mdia_header.offset as usize..mdia_end.min(trak_payload.len())];
        let mdia_payload = &mdia_data[mdia_header.header_size as usize..];

        // Look for hdlr box inside mdia
        if let Some(hdlr_header) = find_child_box(mdia_payload, &box_type::HDLR) {
            let hdlr_end = (hdlr_header.offset + hdlr_header.size) as usize;
            let hdlr_data = &mdia_payload[hdlr_header.offset as usize..hdlr_end.min(mdia_payload.len())];
            let hdlr_payload = &hdlr_data[hdlr_header.header_size as usize..];
            // hdlr: version(1)+flags(3)+pre_defined(4)+handler_type(4)...
            if hdlr_payload.len() >= 12 {
                let handler: [u8; 4] = [
                    hdlr_payload[8],
                    hdlr_payload[9],
                    hdlr_payload[10],
                    hdlr_payload[11],
                ];
                return TrackType::from_handler(&handler);
            }
        }
    }
    TrackType::Unknown
}

/// Generic container box rewriting — recurses into children looking for sinf.
fn rewrite_container_box(
    box_data: &[u8],
    header: &BoxHeader,
    box_type_code: &[u8; 4],
    kid: &[u8; 16],
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
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
                    kid,
                    target_scheme,
                    target_iv_size,
                    target_pattern,
                )?);
            }
            t if t == &box_type::STSD => {
                children.extend_from_slice(&rewrite_stsd(child_data, &child, kid, target_scheme, target_iv_size, target_pattern)?);
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
    kid: &[u8; 16],
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
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
        entries_output.extend_from_slice(&rewrite_sample_entry(entry_data, &entry_header, kid, target_scheme, target_iv_size, target_pattern)?);
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
    kid: &[u8; 16],
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
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
                output_payload.extend_from_slice(&rewrite_sinf(sinf_data, &sinf_header, kid, target_scheme, target_iv_size, target_pattern)?);
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

/// Rewrite a sinf box to the target encryption scheme.
fn rewrite_sinf(
    sinf_data: &[u8],
    sinf_header: &BoxHeader,
    kid: &[u8; 16],
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
) -> Result<Vec<u8>> {
    let payload = &sinf_data[sinf_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::SCHM => {
                // Rewrite scheme type to target scheme
                children.extend_from_slice(&build_schm(target_scheme));
            }
            t if t == &box_type::SCHI => {
                // Rewrite schi container (contains tenc)
                children.extend_from_slice(&rewrite_schi(box_data, &header, kid, target_iv_size, target_pattern)?);
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
    kid: &[u8; 16],
    target_iv_size: u8,
    target_pattern: (u8, u8),
) -> Result<Vec<u8>> {
    let payload = &schi_data[schi_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::TENC => {
                // Rewrite tenc for target scheme with per-track KID
                children.extend_from_slice(&build_tenc(kid, target_iv_size, target_pattern));
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

/// Build a schm box with the given encryption scheme type.
fn build_schm(scheme: EncryptionScheme) -> Vec<u8> {
    // schm is a full box: version(1) + flags(3) + scheme_type(4) + scheme_version(4)
    let size: u32 = 8 + 4 + 4 + 4; // header + version/flags + scheme_type + version
    let mut output = Vec::with_capacity(size as usize);
    cmaf::write_box_header(&mut output, size, &box_type::SCHM);
    output.extend_from_slice(&[0u8; 4]); // version 0 + flags 0
    output.extend_from_slice(&scheme.scheme_type_bytes()); // scheme_type
    output.extend_from_slice(&0x00010000u32.to_be_bytes()); // scheme_version = 1.0
    output
}

/// Build a tenc box configured for the target encryption scheme.
///
/// * `kid` — 16-byte key ID
/// * `iv_size` — per-sample IV size (8 or 16)
/// * `pattern` — (crypt_byte_block, skip_byte_block); (0, 0) for CENC, (1, 9) for CBCS video
fn build_tenc(kid: &[u8; 16], iv_size: u8, pattern: (u8, u8)) -> Vec<u8> {
    // Full box: header(8) + version(1) + flags(3) + reserved/crypt_skip(1)
    //         + isProtected(1) + ivSize(1) + KID(16) = 31
    let total: u32 = 8 + 1 + 3 + 1 + 1 + 1 + 16;
    let mut output = Vec::with_capacity(total as usize);
    cmaf::write_box_header(&mut output, total, &box_type::TENC);
    output.push(0); // version
    output.extend_from_slice(&[0u8; 3]); // flags
    // reserved byte encodes crypt_byte_block (upper nibble) + skip_byte_block (lower nibble)
    let crypt_skip = (pattern.0 << 4) | (pattern.1 & 0x0F);
    output.push(crypt_skip);
    output.push(1); // default_isProtected = 1
    output.push(iv_size); // default_Per_Sample_IV_Size (8 or 16)
    output.extend_from_slice(kid);
    output
}

/// Build PSSH boxes for the target encryption scheme from the DRM key set.
///
/// Groups DRM system entries by system_id and builds one PSSH v1 per system
/// with all unique KIDs. For CENC output: skips FairPlay (CBCS only).
///
/// When `key_mapping` has multiple KIDs, the PSSH boxes will contain
/// all track KIDs for each DRM system.
fn build_pssh_boxes(
    key_set: &DrmKeySet,
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();

    // Group DRM system entries by system_id
    let mut systems: Vec<([u8; 16], Vec<[u8; 16]>, Vec<u8>)> = Vec::new();

    for drm_data in &key_set.drm_systems {
        // For CENC output, skip FairPlay (FairPlay only supports CBCS)
        if target_scheme == EncryptionScheme::Cenc
            && drm_data.system_id == crate::drm::system_ids::FAIRPLAY
        {
            continue;
        }

        // Find or create entry for this system_id
        if let Some(entry) = systems.iter_mut().find(|(sid, _, _)| *sid == drm_data.system_id) {
            // Add KID if not already present
            if !entry.1.contains(&drm_data.kid) {
                entry.1.push(drm_data.kid);
            }
        } else {
            systems.push((drm_data.system_id, vec![drm_data.kid], drm_data.pssh_data.clone()));
        }
    }

    // Build one PSSH v1 per DRM system with all KIDs
    for (system_id, kid_list, pssh_data) in &systems {
        // Use all KIDs from key_mapping if multi-key, otherwise use the system's KIDs
        let kids = if key_mapping.is_multi_key() {
            // Merge system-specific KIDs with key_mapping KIDs
            let mapping_kids = key_mapping.all_kids();
            let mut merged = kid_list.clone();
            for kid in &mapping_kids {
                if !merged.contains(kid) {
                    merged.push(*kid);
                }
            }
            merged
        } else {
            kid_list.clone()
        };

        let pssh = PsshBox {
            version: 1,
            system_id: *system_id,
            key_ids: kids,
            data: pssh_data.clone(),
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

/// Rewrite only the ftyp box for container format conversion (clear-to-clear path).
///
/// All other boxes are passed through unchanged. This is used when both source
/// and target are unencrypted and only the container format needs updating.
pub fn rewrite_ftyp_only(
    init_data: &[u8],
    container_format: ContainerFormat,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(init_data.len());

    for box_result in iterate_boxes(init_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &init_data[header.offset as usize..box_end.min(init_data.len())];

        match &header.box_type {
            t if t == &box_type::FTYP => {
                output.extend_from_slice(&container_format.build_ftyp());
            }
            _ => {
                output.extend_from_slice(box_data);
            }
        }
    }

    Ok(output)
}

/// Inject protection info into a clear init segment for clear-to-encrypted transform.
///
/// This transforms a clear init segment into an encrypted one by:
/// 1. Rewriting ftyp with container-format-specific brands
/// 2. Renaming sample entries (avc1→encv, mp4a→enca, etc.)
/// 3. Injecting sinf/frma/schm/schi/tenc into each sample entry
/// 4. Adding PSSH boxes to moov
///
/// The `key_mapping` parameter controls which KID is used per track type.
pub fn create_protection_info(
    init_data: &[u8],
    key_set: &DrmKeySet,
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
    container_format: ContainerFormat,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(init_data.len() + 256);

    for box_result in iterate_boxes(init_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &init_data[header.offset as usize..box_end.min(init_data.len())];

        match &header.box_type {
            t if t == &box_type::FTYP => {
                output.extend_from_slice(&container_format.build_ftyp());
            }
            t if t == &box_type::MOOV => {
                output.extend_from_slice(&inject_protection_moov(
                    box_data, &header, key_set, key_mapping, target_scheme, target_iv_size, target_pattern,
                )?);
            }
            _ => {
                output.extend_from_slice(box_data);
            }
        }
    }

    Ok(output)
}

/// Rewrite moov for clear-to-encrypted: recurse into trak, add PSSH boxes.
fn inject_protection_moov(
    moov_data: &[u8],
    moov_header: &BoxHeader,
    key_set: &DrmKeySet,
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
) -> Result<Vec<u8>> {
    let payload = &moov_data[moov_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::TRAK => {
                children.extend_from_slice(&inject_protection_trak(
                    box_data, &header, key_mapping, target_scheme, target_iv_size, target_pattern,
                )?);
            }
            _ => {
                children.extend_from_slice(box_data);
            }
        }
    }

    // Add PSSH boxes
    children.extend_from_slice(&build_pssh_boxes(key_set, key_mapping, target_scheme)?);

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::MOOV);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Rewrite a trak for clear-to-encrypted: detect track type, select KID, recurse.
fn inject_protection_trak(
    trak_data: &[u8],
    trak_header: &BoxHeader,
    key_mapping: &TrackKeyMapping,
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
) -> Result<Vec<u8>> {
    let payload = &trak_data[trak_header.header_size as usize..];

    // Detect track type from hdlr to select the correct KID
    let track_type = detect_track_type(payload);
    let kid = key_mapping
        .kid_for_track(track_type)
        .copied()
        .unwrap_or([0u8; 16]);

    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::MDIA || t == &box_type::MINF || t == &box_type::STBL => {
                children.extend_from_slice(&inject_protection_container(
                    box_data, &header, &header.box_type,
                    &kid, target_iv_size, target_pattern, target_scheme,
                )?);
            }
            t if t == &box_type::STSD => {
                children.extend_from_slice(&inject_protection_stsd(
                    box_data, &header, &kid, target_iv_size, target_pattern, target_scheme,
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

/// Generic container box rewriting for clear-to-encrypted — recurses into children.
fn inject_protection_container(
    box_data: &[u8],
    header: &BoxHeader,
    box_type_code: &[u8; 4],
    kid: &[u8; 16],
    target_iv_size: u8,
    target_pattern: (u8, u8),
    target_scheme: EncryptionScheme,
) -> Result<Vec<u8>> {
    let payload = &box_data[header.header_size as usize..];
    let mut children = Vec::new();

    for child_result in iterate_boxes(payload) {
        let child = child_result?;
        let child_end = (child.offset + child.size) as usize;
        let child_data = &payload[child.offset as usize..child_end.min(payload.len())];

        match &child.box_type {
            t if t == &box_type::MDIA || t == &box_type::MINF || t == &box_type::STBL => {
                children.extend_from_slice(&inject_protection_container(
                    child_data, &child, &child.box_type,
                    kid, target_iv_size, target_pattern, target_scheme,
                )?);
            }
            t if t == &box_type::STSD => {
                children.extend_from_slice(&inject_protection_stsd(
                    child_data, &child, kid, target_iv_size, target_pattern, target_scheme,
                )?);
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

/// Rewrite stsd for clear-to-encrypted: rename sample entries and inject sinf.
fn inject_protection_stsd(
    stsd_data: &[u8],
    stsd_header: &BoxHeader,
    kid: &[u8; 16],
    target_iv_size: u8,
    target_pattern: (u8, u8),
    target_scheme: EncryptionScheme,
) -> Result<Vec<u8>> {
    let payload = &stsd_data[stsd_header.header_size as usize..];
    if payload.len() < 8 {
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

        // Determine the encrypted sample entry type
        let original_format = entry_header.box_type;
        if let Some(encrypted_type) = encrypted_sample_entry_type(&original_format) {
            // Build sinf box with per-track KID
            let sinf = build_sinf(&original_format, target_scheme, kid, target_iv_size, target_pattern);

            // Rewrite: change box type to encrypted variant, append sinf
            let entry_payload = &entry_data[entry_header.header_size as usize..];
            let new_size = entry_header.header_size as u32 + entry_payload.len() as u32 + sinf.len() as u32;
            let mut new_entry = Vec::with_capacity(new_size as usize);
            cmaf::write_box_header(&mut new_entry, new_size, &encrypted_type);
            new_entry.extend_from_slice(entry_payload);
            new_entry.extend_from_slice(&sinf);
            entries_output.extend_from_slice(&new_entry);
        } else {
            // Unknown codec — pass through unchanged
            entries_output.extend_from_slice(entry_data);
        }

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

/// Build a complete sinf box for injecting into a clear sample entry.
///
/// Structure: sinf { frma(original_format), schm(target_scheme), schi { tenc(kid, iv_size, pattern) } }
fn build_sinf(
    original_format: &[u8; 4],
    target_scheme: EncryptionScheme,
    kid: &[u8; 16],
    iv_size: u8,
    pattern: (u8, u8),
) -> Vec<u8> {
    let mut children = Vec::new();

    // frma: original format
    let frma_size: u32 = 12;
    cmaf::write_box_header(&mut children, frma_size, &box_type::FRMA);
    children.extend_from_slice(original_format);

    // schm: target encryption scheme
    children.extend_from_slice(&build_schm(target_scheme));

    // schi { tenc }
    let tenc_data = build_tenc(kid, iv_size, pattern);
    let schi_size = 8 + tenc_data.len() as u32;
    cmaf::write_box_header(&mut children, schi_size, &box_type::SCHI);
    children.extend_from_slice(&tenc_data);

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::SINF);
    output.extend_from_slice(&children);
    output
}

/// Map a clear sample entry FourCC to its encrypted equivalent.
///
/// Returns None if the FourCC is already encrypted or unrecognized.
fn encrypted_sample_entry_type(fourcc: &[u8; 4]) -> Option<[u8; 4]> {
    match fourcc {
        b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"vp09" | b"av01" => Some(*b"encv"),
        b"mp4a" | b"ac-3" | b"ec-3" | b"Opus" | b"fLaC" => Some(*b"enca"),
        _ => None,
    }
}

/// Map an encrypted sample entry FourCC back to its clear equivalent using frma data.
///
/// Returns None if the FourCC is not an encrypted sample entry.
fn is_encrypted_sample_entry(fourcc: &[u8; 4]) -> bool {
    matches!(fourcc, b"encv" | b"enca" | b"enct" | b"encs")
}

/// Strip protection info from an encrypted init segment for encrypted-to-clear transform.
///
/// This transforms an encrypted init segment into a clear one by:
/// 1. Rewriting ftyp with container-format-specific brands
/// 2. Restoring sample entry names from sinf/frma (encv→avc1, enca→mp4a, etc.)
/// 3. Removing sinf boxes from sample entries
/// 4. Removing PSSH boxes from moov
pub fn strip_protection_info(
    init_data: &[u8],
    container_format: ContainerFormat,
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(init_data.len());

    for box_result in iterate_boxes(init_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &init_data[header.offset as usize..box_end.min(init_data.len())];

        match &header.box_type {
            t if t == &box_type::FTYP => {
                output.extend_from_slice(&container_format.build_ftyp());
            }
            t if t == &box_type::MOOV => {
                output.extend_from_slice(&strip_protection_moov(box_data, &header)?);
            }
            _ => {
                output.extend_from_slice(box_data);
            }
        }
    }

    Ok(output)
}

/// Rewrite moov for encrypted-to-clear: recurse into trak, remove PSSH boxes.
fn strip_protection_moov(
    moov_data: &[u8],
    moov_header: &BoxHeader,
) -> Result<Vec<u8>> {
    let payload = &moov_data[moov_header.header_size as usize..];
    let mut children = Vec::new();

    for box_result in iterate_boxes(payload) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &payload[header.offset as usize..box_end.min(payload.len())];

        match &header.box_type {
            t if t == &box_type::TRAK => {
                children.extend_from_slice(&strip_protection_container(
                    box_data, &header, &box_type::TRAK,
                )?);
            }
            t if t == &box_type::PSSH => {
                // Remove all PSSH boxes
            }
            _ => {
                children.extend_from_slice(box_data);
            }
        }
    }

    let total_size = 8 + children.len() as u32;
    let mut output = Vec::with_capacity(total_size as usize);
    cmaf::write_box_header(&mut output, total_size, &box_type::MOOV);
    output.extend_from_slice(&children);
    Ok(output)
}

/// Generic container box rewriting for encrypted-to-clear — recurses into children.
fn strip_protection_container(
    box_data: &[u8],
    header: &BoxHeader,
    box_type_code: &[u8; 4],
) -> Result<Vec<u8>> {
    let payload = &box_data[header.header_size as usize..];
    let mut children = Vec::new();

    for child_result in iterate_boxes(payload) {
        let child = child_result?;
        let child_end = (child.offset + child.size) as usize;
        let child_data = &payload[child.offset as usize..child_end.min(payload.len())];

        match &child.box_type {
            t if t == &box_type::MDIA || t == &box_type::MINF || t == &box_type::STBL => {
                children.extend_from_slice(&strip_protection_container(
                    child_data, &child, &child.box_type,
                )?);
            }
            t if t == &box_type::STSD => {
                children.extend_from_slice(&strip_protection_stsd(child_data, &child)?);
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

/// Rewrite stsd for encrypted-to-clear: restore sample entry names, remove sinf.
fn strip_protection_stsd(
    stsd_data: &[u8],
    stsd_header: &BoxHeader,
) -> Result<Vec<u8>> {
    let payload = &stsd_data[stsd_header.header_size as usize..];
    if payload.len() < 8 {
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

        if is_encrypted_sample_entry(&entry_header.box_type) {
            // Find sinf to get original_format, then strip sinf and rename
            let entry_payload = &entry_data[entry_header.header_size as usize..];
            let (original_format, payload_without_sinf) = extract_frma_and_strip_sinf(entry_payload);

            let restored_type = original_format.unwrap_or(entry_header.box_type);
            let new_size = entry_header.header_size as u32 + payload_without_sinf.len() as u32;
            let mut new_entry = Vec::with_capacity(new_size as usize);
            cmaf::write_box_header(&mut new_entry, new_size, &restored_type);
            new_entry.extend_from_slice(&payload_without_sinf);
            entries_output.extend_from_slice(&new_entry);
        } else {
            entries_output.extend_from_slice(entry_data);
        }

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

/// Extract the original format from sinf/frma and return payload with sinf removed.
fn extract_frma_and_strip_sinf(entry_payload: &[u8]) -> (Option<[u8; 4]>, Vec<u8>) {
    let mut original_format: Option<[u8; 4]> = None;
    let mut output = Vec::new();
    let mut pos = 0;

    while pos + 8 <= entry_payload.len() {
        if &entry_payload[pos + 4..pos + 8] == &box_type::SINF {
            let sinf_size = u32::from_be_bytes([
                entry_payload[pos], entry_payload[pos + 1],
                entry_payload[pos + 2], entry_payload[pos + 3],
            ]) as usize;

            if sinf_size >= 8 && pos + sinf_size <= entry_payload.len() {
                // Parse sinf to find frma
                let sinf_payload = &entry_payload[pos + 8..pos + sinf_size];
                if let Some(format) = find_frma_in_sinf(sinf_payload) {
                    original_format = Some(format);
                }
                // Skip the sinf box entirely
                pos += sinf_size;
                continue;
            }
        }

        // Not a sinf — copy byte by byte
        output.push(entry_payload[pos]);
        pos += 1;
    }

    // Copy remaining bytes
    if pos < entry_payload.len() {
        output.extend_from_slice(&entry_payload[pos..]);
    }

    (original_format, output)
}

/// Find frma box inside sinf payload and return the original format FourCC.
fn find_frma_in_sinf(sinf_payload: &[u8]) -> Option<[u8; 4]> {
    for box_result in iterate_boxes(sinf_payload) {
        if let Ok(header) = box_result {
            if header.box_type == box_type::FRMA {
                let box_end = (header.offset + header.size) as usize;
                let box_data = &sinf_payload[header.offset as usize..box_end.min(sinf_payload.len())];
                let payload = &box_data[header.header_size as usize..];
                if payload.len() >= 4 {
                    let mut format = [0u8; 4];
                    format.copy_from_slice(&payload[..4]);
                    return Some(format);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::scheme::EncryptionScheme;
    use crate::drm::{system_ids, ContentKey, DrmSystemData};
    use crate::media::cmaf;
    use crate::media::codec::TrackKeyMapping;
    use crate::media::container::ContainerFormat;

    fn make_key_set() -> DrmKeySet {
        DrmKeySet {
            keys: vec![ContentKey {
                kid: [0x01; 16],
                key: vec![0xAA; 16],
                iv: None,
            }],
            drm_systems: vec![
                DrmSystemData {
                    system_id: system_ids::WIDEVINE,
                    kid: [0x01; 16],
                    pssh_data: vec![0x10, 0x20],
                    content_protection_data: None,
                },
                DrmSystemData {
                    system_id: system_ids::PLAYREADY,
                    kid: [0x01; 16],
                    pssh_data: vec![0x30, 0x40],
                    content_protection_data: None,
                },
            ],
        }
    }

    fn make_key_mapping() -> TrackKeyMapping {
        TrackKeyMapping::single([0x01; 16])
    }

    #[test]
    fn build_schm_cenc_produces_valid_box() {
        let schm = build_schm(EncryptionScheme::Cenc);
        assert_eq!(schm.len(), 20);
        assert_eq!(&schm[4..8], b"schm");
        assert_eq!(&schm[12..16], b"cenc");
        assert_eq!(&schm[16..20], &0x00010000u32.to_be_bytes());
    }

    #[test]
    fn build_schm_cbcs_produces_valid_box() {
        let schm = build_schm(EncryptionScheme::Cbcs);
        assert_eq!(schm.len(), 20);
        assert_eq!(&schm[4..8], b"schm");
        assert_eq!(&schm[12..16], b"cbcs");
        assert_eq!(&schm[16..20], &0x00010000u32.to_be_bytes());
    }

    #[test]
    fn build_tenc_cenc_produces_valid_box() {
        let kid = [0x01; 16];
        let tenc = build_tenc(&kid, 8, (0, 0));
        assert_eq!(&tenc[4..8], b"tenc");
        let version_flags_reserved_len = 4 + 1; // version(1)+flags(3)+reserved(1)
        let is_protected_offset = 8 + version_flags_reserved_len;
        assert_eq!(tenc[is_protected_offset], 1);
        assert_eq!(tenc[is_protected_offset + 1], 8);
        assert_eq!(&tenc[is_protected_offset + 2..is_protected_offset + 18], &[0x01; 16]);
        // Check pattern byte is 0 for CENC
        assert_eq!(tenc[8 + 4], 0x00);
    }

    #[test]
    fn build_tenc_cbcs_produces_valid_box() {
        let kid = [0x03; 16];
        let tenc = build_tenc(&kid, 16, (1, 9));
        assert_eq!(&tenc[4..8], b"tenc");
        // Check pattern byte: crypt=1 (upper nibble), skip=9 (lower nibble) = 0x19
        assert_eq!(tenc[8 + 4], 0x19);
        let is_protected_offset = 8 + 5;
        assert_eq!(tenc[is_protected_offset], 1); // isProtected
        assert_eq!(tenc[is_protected_offset + 1], 16); // IV size
    }

    #[test]
    fn build_tenc_cenc_iv_size_16() {
        let kid = [0x02; 16];
        let tenc = build_tenc(&kid, 16, (0, 0));
        let is_protected_offset = 8 + 5;
        assert_eq!(tenc[is_protected_offset + 1], 16);
    }

    #[test]
    fn build_pssh_boxes_cenc_skips_fairplay() {
        let mut key_set = make_key_set();
        key_set.drm_systems.push(DrmSystemData {
            system_id: system_ids::FAIRPLAY,
            kid: [0x01; 16],
            pssh_data: vec![0xFF],
            content_protection_data: None,
        });
        let pssh_data = build_pssh_boxes(&key_set, &make_key_mapping(), EncryptionScheme::Cenc).unwrap();
        let mut pssh_count = 0;
        let mut pos = 0;
        while pos + 8 <= pssh_data.len() {
            if &pssh_data[pos + 4..pos + 8] == b"pssh" {
                pssh_count += 1;
                let size = u32::from_be_bytes([
                    pssh_data[pos], pssh_data[pos + 1], pssh_data[pos + 2], pssh_data[pos + 3],
                ]) as usize;
                pos += size;
            } else {
                pos += 1;
            }
        }
        assert_eq!(pssh_count, 2); // Widevine + PlayReady only
    }

    #[test]
    fn build_pssh_boxes_cbcs_includes_fairplay() {
        let mut key_set = make_key_set();
        key_set.drm_systems.push(DrmSystemData {
            system_id: system_ids::FAIRPLAY,
            kid: [0x01; 16],
            pssh_data: vec![0xFF],
            content_protection_data: None,
        });
        let pssh_data = build_pssh_boxes(&key_set, &make_key_mapping(), EncryptionScheme::Cbcs).unwrap();
        let mut pssh_count = 0;
        let mut pos = 0;
        while pos + 8 <= pssh_data.len() {
            if &pssh_data[pos + 4..pos + 8] == b"pssh" {
                pssh_count += 1;
                let size = u32::from_be_bytes([
                    pssh_data[pos], pssh_data[pos + 1], pssh_data[pos + 2], pssh_data[pos + 3],
                ]) as usize;
                pos += size;
            } else {
                pos += 1;
            }
        }
        assert_eq!(pssh_count, 3); // Widevine + PlayReady + FairPlay
    }

    #[test]
    fn build_pssh_boxes_includes_key_ids() {
        let key_set = make_key_set();
        let pssh_data = build_pssh_boxes(&key_set, &make_key_mapping(), EncryptionScheme::Cenc).unwrap();
        assert!(!pssh_data.is_empty());
        assert_eq!(&pssh_data[4..8], b"pssh");
    }

    #[test]
    fn parse_protection_info_no_sinf_returns_none() {
        // ftyp box only — no sinf
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, 16, b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x00\x00"); // brand + version
        let result = parse_protection_info(&data).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_protection_info_empty_data_returns_none() {
        let result = parse_protection_info(&[]).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn parse_protection_info_finds_sinf_with_tenc() {
        // Build a minimal init segment with a sinf box containing frma + schm + schi(tenc)
        let mut init_data = Vec::new();

        // ftyp
        cmaf::write_box_header(&mut init_data, 12, b"ftyp");
        init_data.extend_from_slice(b"isom");

        // Build sinf contents
        let mut sinf_children = Vec::new();

        // frma: original_format = "avc1"
        cmaf::write_box_header(&mut sinf_children, 12, b"frma");
        sinf_children.extend_from_slice(b"avc1");

        // schm: version(1)+flags(3)+scheme_type(4)+scheme_version(4) = full box
        let schm_size: u32 = 8 + 4 + 4 + 4;
        cmaf::write_box_header(&mut sinf_children, schm_size, b"schm");
        sinf_children.extend_from_slice(&[0; 4]); // version + flags
        sinf_children.extend_from_slice(b"cbcs");
        sinf_children.extend_from_slice(&0x00010000u32.to_be_bytes());

        // schi containing tenc
        // tenc: header(8) + version(1) + flags(3) + reserved/crypt_skip(1)
        //       + isProtected(1) + ivSize(1) + KID(16) = 31
        let tenc_size: u32 = 8 + 1 + 3 + 1 + 1 + 1 + 16;
        let mut tenc_data = Vec::new();
        cmaf::write_box_header(&mut tenc_data, tenc_size, b"tenc");
        tenc_data.push(0); // version
        tenc_data.extend_from_slice(&[0; 3]); // flags
        tenc_data.push(0x19); // crypt=1, skip=9
        tenc_data.push(1); // isProtected
        tenc_data.push(8); // ivSize
        tenc_data.extend_from_slice(&[0xAA; 16]); // KID

        let schi_size = 8 + tenc_data.len() as u32;
        cmaf::write_box_header(&mut sinf_children, schi_size, b"schi");
        sinf_children.extend_from_slice(&tenc_data);

        // Now wrap in sinf
        let sinf_size = 8 + sinf_children.len() as u32;
        cmaf::write_box_header(&mut init_data, sinf_size, b"sinf");
        init_data.extend_from_slice(&sinf_children);

        let result = parse_protection_info(&init_data).unwrap();
        assert!(result.is_some());
        let info = result.unwrap();
        assert_eq!(&info.original_format, b"avc1");
        assert_eq!(&info.scheme_type, b"cbcs");
    }

    #[test]
    fn rewrite_init_segment_empty_data() {
        let key_set = make_key_set();
        let result = rewrite_init_segment(&[], &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::default()).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn rewrite_init_segment_ftyp_only_passthrough() {
        let key_set = make_key_set();
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, 16, b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x02\x00");

        let result = rewrite_init_segment(&data, &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::Cmaf).unwrap();
        assert_eq!(&result[4..8], b"ftyp");
        // ftyp is rewritten with container-format-specific brands
        let ftyp_size = u32::from_be_bytes([result[0], result[1], result[2], result[3]]) as usize;
        assert!(ftyp_size >= 16); // At least header + major_brand + minor_version
    }

    #[test]
    fn rewrite_init_segment_ftyp_cmaf_has_cmfc_brand() {
        let key_set = make_key_set();
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, 16, b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x02\x00");

        let result = rewrite_init_segment(&data, &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::Cmaf).unwrap();
        // CMAF ftyp should contain cmfc compatible brand
        assert!(result.windows(4).any(|w| w == b"cmfc"), "CMAF ftyp should contain cmfc brand");
    }

    #[test]
    fn rewrite_init_segment_ftyp_fmp4_no_cmfc_brand() {
        let key_set = make_key_set();
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, 16, b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x02\x00");

        let result = rewrite_init_segment(&data, &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::Fmp4).unwrap();
        assert_eq!(&result[4..8], b"ftyp");
        // fMP4 ftyp should NOT contain cmfc brand
        assert!(!result.windows(4).any(|w| w == b"cmfc"), "fMP4 ftyp should not contain cmfc brand");
    }

    #[test]
    fn rewrite_init_segment_moov_adds_pssh() {
        let key_set = make_key_set();

        let mut mvhd_data = Vec::new();
        cmaf::write_box_header(&mut mvhd_data, 16, b"mvhd");
        mvhd_data.extend_from_slice(&[0u8; 8]);

        let moov_size = 8 + mvhd_data.len() as u32;
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, moov_size, b"moov");
        data.extend_from_slice(&mvhd_data);

        let result = rewrite_init_segment(&data, &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::default()).unwrap();
        assert_eq!(&result[4..8], b"moov");
        let has_pssh = result.windows(4).any(|w| w == b"pssh");
        assert!(has_pssh);
    }

    #[test]
    fn rewrite_init_segment_cbcs_target() {
        let key_set = make_key_set();

        let mut mvhd_data = Vec::new();
        cmaf::write_box_header(&mut mvhd_data, 16, b"mvhd");
        mvhd_data.extend_from_slice(&[0u8; 8]);

        let moov_size = 8 + mvhd_data.len() as u32;
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, moov_size, b"moov");
        data.extend_from_slice(&mvhd_data);

        let result = rewrite_init_segment(&data, &key_set, &make_key_mapping(), EncryptionScheme::Cbcs, 16, (1, 9), ContainerFormat::default()).unwrap();
        assert_eq!(&result[4..8], b"moov");
        let has_pssh = result.windows(4).any(|w| w == b"pssh");
        assert!(has_pssh);
    }

    // --- Helper: build a minimal clear init segment ---

    /// Build a minimal clear init segment: ftyp + moov { trak { mdia { minf { stbl { stsd { avc1 } } } } } }
    fn build_clear_init(codec: &[u8; 4]) -> Vec<u8> {
        let mut data = Vec::new();

        // ftyp
        cmaf::write_box_header(&mut data, 16, b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x02\x00");

        // Build sample entry: codec with 8 bytes of payload
        let entry_payload = [0u8; 8];
        let entry_size = 8 + entry_payload.len() as u32;
        let mut entry = Vec::new();
        cmaf::write_box_header(&mut entry, entry_size, codec);
        entry.extend_from_slice(&entry_payload);

        // stsd (full box): header(8) + version_flags(4) + entry_count(4) + entries
        let stsd_size = 8 + 4 + 4 + entry.len() as u32;
        let mut stsd = Vec::new();
        cmaf::write_box_header(&mut stsd, stsd_size, b"stsd");
        stsd.extend_from_slice(&[0u8; 4]); // version + flags
        stsd.extend_from_slice(&1u32.to_be_bytes()); // entry_count = 1
        stsd.extend_from_slice(&entry);

        // Nest: stbl { stsd }
        let stbl_size = 8 + stsd.len() as u32;
        let mut stbl = Vec::new();
        cmaf::write_box_header(&mut stbl, stbl_size, b"stbl");
        stbl.extend_from_slice(&stsd);

        // minf { stbl }
        let minf_size = 8 + stbl.len() as u32;
        let mut minf = Vec::new();
        cmaf::write_box_header(&mut minf, minf_size, b"minf");
        minf.extend_from_slice(&stbl);

        // mdia { minf }
        let mdia_size = 8 + minf.len() as u32;
        let mut mdia = Vec::new();
        cmaf::write_box_header(&mut mdia, mdia_size, b"mdia");
        mdia.extend_from_slice(&minf);

        // trak { mdia }
        let trak_size = 8 + mdia.len() as u32;
        let mut trak = Vec::new();
        cmaf::write_box_header(&mut trak, trak_size, b"trak");
        trak.extend_from_slice(&mdia);

        // moov { trak }
        let moov_size = 8 + trak.len() as u32;
        cmaf::write_box_header(&mut data, moov_size, b"moov");
        data.extend_from_slice(&trak);

        data
    }

    /// Build a minimal encrypted init segment: ftyp + moov { trak { mdia { minf { stbl { stsd { encv { sinf } } } } } }, pssh }
    fn build_encrypted_init(encrypted_type: &[u8; 4], original_format: &[u8; 4]) -> Vec<u8> {
        let mut data = Vec::new();

        // ftyp
        cmaf::write_box_header(&mut data, 16, b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x02\x00");

        // Build sinf: frma + schm + schi { tenc }
        let sinf = build_sinf(original_format, EncryptionScheme::Cenc, &[0x01; 16], 8, (0, 0));

        // Sample entry: encrypted_type with 8 bytes of codec payload + sinf
        let entry_payload_base = [0u8; 8];
        let entry_size = 8 + entry_payload_base.len() as u32 + sinf.len() as u32;
        let mut entry = Vec::new();
        cmaf::write_box_header(&mut entry, entry_size, encrypted_type);
        entry.extend_from_slice(&entry_payload_base);
        entry.extend_from_slice(&sinf);

        // stsd (full box)
        let stsd_size = 8 + 4 + 4 + entry.len() as u32;
        let mut stsd = Vec::new();
        cmaf::write_box_header(&mut stsd, stsd_size, b"stsd");
        stsd.extend_from_slice(&[0u8; 4]);
        stsd.extend_from_slice(&1u32.to_be_bytes());
        stsd.extend_from_slice(&entry);

        // Nest: stbl { stsd }
        let stbl_size = 8 + stsd.len() as u32;
        let mut stbl = Vec::new();
        cmaf::write_box_header(&mut stbl, stbl_size, b"stbl");
        stbl.extend_from_slice(&stsd);

        let minf_size = 8 + stbl.len() as u32;
        let mut minf = Vec::new();
        cmaf::write_box_header(&mut minf, minf_size, b"minf");
        minf.extend_from_slice(&stbl);

        let mdia_size = 8 + minf.len() as u32;
        let mut mdia = Vec::new();
        cmaf::write_box_header(&mut mdia, mdia_size, b"mdia");
        mdia.extend_from_slice(&minf);

        let trak_size = 8 + mdia.len() as u32;
        let mut trak = Vec::new();
        cmaf::write_box_header(&mut trak, trak_size, b"trak");
        trak.extend_from_slice(&mdia);

        // PSSH box (minimal)
        let mut pssh = Vec::new();
        cmaf::write_box_header(&mut pssh, 20, b"pssh");
        pssh.extend_from_slice(&[0u8; 12]); // version + flags + system_id partial

        // moov { trak, pssh }
        let moov_size = 8 + trak.len() as u32 + pssh.len() as u32;
        cmaf::write_box_header(&mut data, moov_size, b"moov");
        data.extend_from_slice(&trak);
        data.extend_from_slice(&pssh);

        data
    }

    // --- Tests for new Phase 3 functions ---

    #[test]
    fn encrypted_sample_entry_type_video_codecs() {
        assert_eq!(encrypted_sample_entry_type(b"avc1"), Some(*b"encv"));
        assert_eq!(encrypted_sample_entry_type(b"avc3"), Some(*b"encv"));
        assert_eq!(encrypted_sample_entry_type(b"hvc1"), Some(*b"encv"));
        assert_eq!(encrypted_sample_entry_type(b"hev1"), Some(*b"encv"));
        assert_eq!(encrypted_sample_entry_type(b"vp09"), Some(*b"encv"));
        assert_eq!(encrypted_sample_entry_type(b"av01"), Some(*b"encv"));
    }

    #[test]
    fn encrypted_sample_entry_type_audio_codecs() {
        assert_eq!(encrypted_sample_entry_type(b"mp4a"), Some(*b"enca"));
        assert_eq!(encrypted_sample_entry_type(b"ac-3"), Some(*b"enca"));
        assert_eq!(encrypted_sample_entry_type(b"ec-3"), Some(*b"enca"));
        assert_eq!(encrypted_sample_entry_type(b"Opus"), Some(*b"enca"));
        assert_eq!(encrypted_sample_entry_type(b"fLaC"), Some(*b"enca"));
    }

    #[test]
    fn encrypted_sample_entry_type_unknown_returns_none() {
        assert_eq!(encrypted_sample_entry_type(b"encv"), None);
        assert_eq!(encrypted_sample_entry_type(b"enca"), None);
        assert_eq!(encrypted_sample_entry_type(b"abcd"), None);
    }

    #[test]
    fn is_encrypted_sample_entry_recognizes_types() {
        assert!(is_encrypted_sample_entry(b"encv"));
        assert!(is_encrypted_sample_entry(b"enca"));
        assert!(is_encrypted_sample_entry(b"enct"));
        assert!(is_encrypted_sample_entry(b"encs"));
        assert!(!is_encrypted_sample_entry(b"avc1"));
        assert!(!is_encrypted_sample_entry(b"mp4a"));
    }

    #[test]
    fn build_sinf_produces_valid_box() {
        let sinf = build_sinf(b"avc1", EncryptionScheme::Cenc, &[0x01; 16], 8, (0, 0));
        assert_eq!(&sinf[4..8], b"sinf");
        // Should contain frma, schm, schi
        assert!(sinf.windows(4).any(|w| w == b"frma"));
        assert!(sinf.windows(4).any(|w| w == b"schm"));
        assert!(sinf.windows(4).any(|w| w == b"schi"));
        assert!(sinf.windows(4).any(|w| w == b"tenc"));
        // frma should contain original format "avc1"
        let frma_pos = sinf.windows(4).position(|w| w == b"frma").unwrap();
        assert_eq!(&sinf[frma_pos + 4..frma_pos + 8], b"avc1");
    }

    #[test]
    fn build_sinf_cbcs_has_pattern() {
        let sinf = build_sinf(b"mp4a", EncryptionScheme::Cbcs, &[0x02; 16], 16, (1, 9));
        assert!(sinf.windows(4).any(|w| w == b"cbcs"));
    }

    #[test]
    fn find_frma_in_sinf_extracts_format() {
        // Build a sinf payload with frma box
        let mut sinf_payload = Vec::new();
        cmaf::write_box_header(&mut sinf_payload, 12, b"frma");
        sinf_payload.extend_from_slice(b"avc1");

        let result = find_frma_in_sinf(&sinf_payload);
        assert_eq!(result, Some(*b"avc1"));
    }

    #[test]
    fn find_frma_in_sinf_returns_none_for_empty() {
        assert_eq!(find_frma_in_sinf(&[]), None);
    }

    #[test]
    fn rewrite_ftyp_only_preserves_moov() {
        let init = build_clear_init(b"avc1");
        let result = rewrite_ftyp_only(&init, ContainerFormat::Cmaf).unwrap();
        // Should have ftyp and moov
        assert!(result.windows(4).any(|w| w == b"ftyp"));
        assert!(result.windows(4).any(|w| w == b"moov"));
        // Moov content should be the same (stsd with avc1)
        assert!(result.windows(4).any(|w| w == b"avc1"));
        // Should NOT have sinf or pssh (clear content stays clear)
        assert!(!result.windows(4).any(|w| w == b"sinf"));
        assert!(!result.windows(4).any(|w| w == b"pssh"));
    }

    #[test]
    fn rewrite_ftyp_only_cmaf_has_cmfc() {
        let init = build_clear_init(b"avc1");
        let result = rewrite_ftyp_only(&init, ContainerFormat::Cmaf).unwrap();
        assert!(result.windows(4).any(|w| w == b"cmfc"));
    }

    #[test]
    fn create_protection_info_injects_sinf_and_pssh() {
        let init = build_clear_init(b"avc1");
        let key_set = make_key_set();
        let result = create_protection_info(
            &init, &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::Cmaf,
        ).unwrap();

        // Should have sinf injected
        assert!(result.windows(4).any(|w| w == b"sinf"));
        assert!(result.windows(4).any(|w| w == b"frma"));
        assert!(result.windows(4).any(|w| w == b"schm"));
        assert!(result.windows(4).any(|w| w == b"tenc"));
        // Should have PSSH boxes
        assert!(result.windows(4).any(|w| w == b"pssh"));
        // Sample entry should be renamed to encv
        assert!(result.windows(4).any(|w| w == b"encv"));
        // Original avc1 should only appear inside frma, not as a sample entry type
        // (frma contains it, but the entry type itself should be encv)
    }

    #[test]
    fn create_protection_info_audio_enca() {
        let init = build_clear_init(b"mp4a");
        let key_set = make_key_set();
        let result = create_protection_info(
            &init, &key_set, &make_key_mapping(), EncryptionScheme::Cbcs, 16, (0, 0), ContainerFormat::Cmaf,
        ).unwrap();

        assert!(result.windows(4).any(|w| w == b"enca"));
        assert!(result.windows(4).any(|w| w == b"sinf"));
        assert!(result.windows(4).any(|w| w == b"cbcs"));
    }

    #[test]
    fn create_protection_info_cenc_has_cenc_scheme() {
        let init = build_clear_init(b"avc1");
        let key_set = make_key_set();
        let result = create_protection_info(
            &init, &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::Cmaf,
        ).unwrap();

        // schm box should contain "cenc"
        assert!(result.windows(4).any(|w| w == b"cenc"));
    }

    #[test]
    fn strip_protection_info_removes_sinf_and_pssh() {
        let init = build_encrypted_init(b"encv", b"avc1");
        let result = strip_protection_info(&init, ContainerFormat::Cmaf).unwrap();

        // sinf should be removed
        assert!(!result.windows(4).any(|w| w == b"sinf"));
        // PSSH should be removed
        assert!(!result.windows(4).any(|w| w == b"pssh"));
        // Sample entry should be restored to avc1
        assert!(result.windows(4).any(|w| w == b"avc1"));
        // encv should no longer appear
        assert!(!result.windows(4).any(|w| w == b"encv"));
    }

    #[test]
    fn strip_protection_info_audio_enca() {
        let init = build_encrypted_init(b"enca", b"mp4a");
        let result = strip_protection_info(&init, ContainerFormat::Cmaf).unwrap();

        assert!(!result.windows(4).any(|w| w == b"sinf"));
        assert!(result.windows(4).any(|w| w == b"mp4a"));
        assert!(!result.windows(4).any(|w| w == b"enca"));
    }

    #[test]
    fn create_then_strip_roundtrip() {
        // Clear → Encrypted → Clear should restore original structure
        let init = build_clear_init(b"avc1");
        let key_set = make_key_set();

        let encrypted = create_protection_info(
            &init, &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::Cmaf,
        ).unwrap();

        // Verify encryption was applied
        assert!(encrypted.windows(4).any(|w| w == b"encv"));
        assert!(encrypted.windows(4).any(|w| w == b"sinf"));

        let clear = strip_protection_info(&encrypted, ContainerFormat::Cmaf).unwrap();

        // Verify decryption info was stripped
        assert!(clear.windows(4).any(|w| w == b"avc1"));
        assert!(!clear.windows(4).any(|w| w == b"encv"));
        assert!(!clear.windows(4).any(|w| w == b"sinf"));
        assert!(!clear.windows(4).any(|w| w == b"pssh"));
    }

    #[test]
    fn rewrite_ftyp_only_empty_data() {
        let result = rewrite_ftyp_only(&[], ContainerFormat::Cmaf).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn create_protection_info_empty_data() {
        let key_set = make_key_set();
        let result = create_protection_info(
            &[], &key_set, &make_key_mapping(), EncryptionScheme::Cenc, 8, (0, 0), ContainerFormat::Cmaf,
        ).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn strip_protection_info_empty_data() {
        let result = strip_protection_info(&[], ContainerFormat::Cmaf).unwrap();
        assert!(result.is_empty());
    }
}
