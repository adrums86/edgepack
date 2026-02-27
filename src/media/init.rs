use crate::drm::scheme::EncryptionScheme;
use crate::drm::DrmKeySet;
use crate::error::Result;
use crate::media::box_type;
use crate::media::cmaf::{
    self, BoxHeader, ProtectionSchemeInfo, build_pssh_box, find_child_box, iterate_boxes,
    parse_tenc, read_box_header, PsshBox,
};

/// Rewrite an init segment to the target encryption scheme.
///
/// This modifies:
/// - `schm` box: set scheme type to target (cbcs/cenc)
/// - `tenc` box: update pattern encryption fields for target scheme
/// - `pssh` boxes: filter/add PSSH boxes appropriate for the target scheme
///
/// Returns the rewritten init segment data.
pub fn rewrite_init_segment(
    init_data: &[u8],
    key_set: &DrmKeySet,
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(init_data.len());

    for box_result in iterate_boxes(init_data) {
        let header = box_result?;
        let box_end = (header.offset + header.size) as usize;
        let box_data = &init_data[header.offset as usize..box_end.min(init_data.len())];

        match &header.box_type {
            t if t == &box_type::MOOV => {
                output.extend_from_slice(&rewrite_moov(box_data, &header, key_set, target_scheme, target_iv_size, target_pattern)?);
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
                children.extend_from_slice(&rewrite_trak(box_data, &header, key_set, target_scheme, target_iv_size, target_pattern)?);
            }
            t if t == &box_type::PSSH => {
                // Replace all PSSH boxes with our new ones
                if !wrote_new_pssh {
                    children.extend_from_slice(&build_pssh_boxes(key_set, target_scheme)?);
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
        children.extend_from_slice(&build_pssh_boxes(key_set, target_scheme)?);
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
    target_scheme: EncryptionScheme,
    target_iv_size: u8,
    target_pattern: (u8, u8),
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

/// Generic container box rewriting — recurses into children looking for sinf.
fn rewrite_container_box(
    box_data: &[u8],
    header: &BoxHeader,
    box_type_code: &[u8; 4],
    key_set: &DrmKeySet,
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
                    key_set,
                    target_scheme,
                    target_iv_size,
                    target_pattern,
                )?);
            }
            t if t == &box_type::STSD => {
                children.extend_from_slice(&rewrite_stsd(child_data, &child, key_set, target_scheme, target_iv_size, target_pattern)?);
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
        entries_output.extend_from_slice(&rewrite_sample_entry(entry_data, &entry_header, key_set, target_scheme, target_iv_size, target_pattern)?);
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
                output_payload.extend_from_slice(&rewrite_sinf(sinf_data, &sinf_header, key_set, target_scheme, target_iv_size, target_pattern)?);
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
    key_set: &DrmKeySet,
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
                children.extend_from_slice(&rewrite_schi(box_data, &header, key_set, target_iv_size, target_pattern)?);
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
                // Rewrite tenc for target scheme
                let kid = if let Some(key) = key_set.keys.first() {
                    key.kid
                } else {
                    [0u8; 16]
                };
                children.extend_from_slice(&build_tenc(&kid, target_iv_size, target_pattern));
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
/// For CENC output: includes Widevine + PlayReady, skips FairPlay.
/// For CBCS output: includes all DRM systems (FairPlay, Widevine, PlayReady).
fn build_pssh_boxes(key_set: &DrmKeySet, target_scheme: EncryptionScheme) -> Result<Vec<u8>> {
    let mut output = Vec::new();

    for drm_data in &key_set.drm_systems {
        // For CENC output, skip FairPlay (FairPlay only supports CBCS)
        if target_scheme == EncryptionScheme::Cenc
            && drm_data.system_id == crate::drm::system_ids::FAIRPLAY
        {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::scheme::EncryptionScheme;
    use crate::drm::{system_ids, ContentKey, DrmSystemData};
    use crate::media::cmaf;

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
        let pssh_data = build_pssh_boxes(&key_set, EncryptionScheme::Cenc).unwrap();
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
        let pssh_data = build_pssh_boxes(&key_set, EncryptionScheme::Cbcs).unwrap();
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
        let pssh_data = build_pssh_boxes(&key_set, EncryptionScheme::Cenc).unwrap();
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
        let result = rewrite_init_segment(&[], &key_set, EncryptionScheme::Cenc, 8, (0, 0)).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn rewrite_init_segment_ftyp_only_passthrough() {
        let key_set = make_key_set();
        let mut data = Vec::new();
        cmaf::write_box_header(&mut data, 16, b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x02\x00");

        let result = rewrite_init_segment(&data, &key_set, EncryptionScheme::Cenc, 8, (0, 0)).unwrap();
        assert_eq!(&result[4..8], b"ftyp");
        assert_eq!(result.len(), data.len());
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

        let result = rewrite_init_segment(&data, &key_set, EncryptionScheme::Cenc, 8, (0, 0)).unwrap();
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

        let result = rewrite_init_segment(&data, &key_set, EncryptionScheme::Cbcs, 16, (1, 9)).unwrap();
        assert_eq!(&result[4..8], b"moov");
        let has_pssh = result.windows(4).any(|w| w == b"pssh");
        assert!(has_pssh);
    }
}
