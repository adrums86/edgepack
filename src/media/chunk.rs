//! CMAF chunk (partial segment) boundary detection.
//!
//! Detects moof+mdat pairs within a CMAF segment, enabling LL-HLS part extraction.
//! A single CMAF segment may contain multiple moof+mdat pairs when produced by
//! a low-latency encoder (chunked CMAF).

use crate::media::{box_type, FourCC};

/// A detected chunk boundary within a CMAF segment.
#[derive(Debug, Clone)]
pub struct ChunkBoundary {
    /// Byte offset of the moof box within the segment.
    pub offset: usize,
    /// Total size of the moof+mdat pair in bytes.
    pub size: usize,
    /// Duration of this chunk in the segment's timescale (0 if not computed).
    pub duration_ticks: u64,
    /// Whether this chunk contains an independent (IDR/sync) frame.
    pub independent: bool,
}

/// Detect chunk boundaries (moof+mdat pairs) in a CMAF segment.
///
/// Walks through top-level boxes looking for consecutive moof+mdat pairs.
/// Each pair represents one CMAF chunk that can be served as an LL-HLS part.
pub fn detect_chunk_boundaries(data: &[u8]) -> Vec<ChunkBoundary> {
    let mut boundaries = Vec::new();
    let mut pos = 0;

    while pos + 8 <= data.len() {
        let box_size = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]])
            as usize;
        let bt: FourCC = [data[pos + 4], data[pos + 5], data[pos + 6], data[pos + 7]];

        if box_size < 8 || pos + box_size > data.len() {
            break;
        }

        if bt == box_type::MOOF {
            let moof_offset = pos;
            let moof_size = box_size;

            // Check for traf->trun to determine independence
            let independent = is_independent_chunk(&data[pos..pos + box_size]);

            // Look for immediately following mdat
            let mdat_pos = pos + box_size;
            if mdat_pos + 8 <= data.len() {
                let mdat_size = u32::from_be_bytes([
                    data[mdat_pos],
                    data[mdat_pos + 1],
                    data[mdat_pos + 2],
                    data[mdat_pos + 3],
                ]) as usize;
                let mdat_type: FourCC = [
                    data[mdat_pos + 4],
                    data[mdat_pos + 5],
                    data[mdat_pos + 6],
                    data[mdat_pos + 7],
                ];

                if mdat_type == box_type::MDAT
                    && mdat_size >= 8
                    && mdat_pos + mdat_size <= data.len()
                {
                    boundaries.push(ChunkBoundary {
                        offset: moof_offset,
                        size: moof_size + mdat_size,
                        duration_ticks: 0,
                        independent,
                    });
                    pos = mdat_pos + mdat_size;
                    continue;
                }
            }
        }

        pos += box_size;
    }

    boundaries
}

/// Extract chunk data (moof+mdat) from a segment at the given boundary.
///
/// Returns `None` if the boundary extends beyond the data length.
pub fn extract_chunk(data: &[u8], boundary: &ChunkBoundary) -> Option<Vec<u8>> {
    let end = boundary.offset + boundary.size;
    if end > data.len() {
        return None;
    }
    Some(data[boundary.offset..end].to_vec())
}

/// Check if a moof box contains a sync/IDR sample (independent chunk).
///
/// Looks at the trun box's first_sample_flags (if present) or tfhd default_sample_flags.
/// A sample is independent if the `sample_depends_on` field is 2 (depends on nothing).
fn is_independent_chunk(moof_data: &[u8]) -> bool {
    // Walk inside moof looking for traf
    let mut pos = 8; // skip moof header
    while pos + 8 <= moof_data.len() {
        let box_size = u32::from_be_bytes([
            moof_data[pos],
            moof_data[pos + 1],
            moof_data[pos + 2],
            moof_data[pos + 3],
        ]) as usize;
        let bt: FourCC = [
            moof_data[pos + 4],
            moof_data[pos + 5],
            moof_data[pos + 6],
            moof_data[pos + 7],
        ];

        if box_size < 8 || pos + box_size > moof_data.len() {
            break;
        }

        if bt == box_type::TRAF {
            // Walk inside traf
            return check_traf_independence(&moof_data[pos + 8..pos + box_size]);
        }

        pos += box_size;
    }
    false
}

/// Check traf children for sync sample flags.
fn check_traf_independence(traf_data: &[u8]) -> bool {
    let mut pos = 0;
    let mut default_flags: Option<u32> = None;

    while pos + 8 <= traf_data.len() {
        let box_size = u32::from_be_bytes([
            traf_data[pos],
            traf_data[pos + 1],
            traf_data[pos + 2],
            traf_data[pos + 3],
        ]) as usize;
        let bt: FourCC = [
            traf_data[pos + 4],
            traf_data[pos + 5],
            traf_data[pos + 6],
            traf_data[pos + 7],
        ];

        if box_size < 8 || pos + box_size > traf_data.len() {
            break;
        }

        if bt == box_type::TFHD {
            // tfhd: version(1) + flags(3) + track_id(4) + optional fields based on flags
            if pos + 12 + 4 <= traf_data.len() {
                let tfhd_flags = u32::from_be_bytes([
                    0,
                    traf_data[pos + 9],
                    traf_data[pos + 10],
                    traf_data[pos + 11],
                ]);
                // Check if default-sample-flags-present (0x000020)
                if tfhd_flags & 0x000020 != 0 {
                    // Count optional fields before default_sample_flags
                    let mut offset = pos + 12 + 4; // after version+flags+track_id
                    if tfhd_flags & 0x000001 != 0 {
                        offset += 8; // base-data-offset
                    }
                    if tfhd_flags & 0x000002 != 0 {
                        offset += 4; // sample-description-index
                    }
                    if tfhd_flags & 0x000008 != 0 {
                        offset += 4; // default-sample-duration
                    }
                    if tfhd_flags & 0x000010 != 0 {
                        offset += 4; // default-sample-size
                    }
                    if offset + 4 <= traf_data.len() {
                        default_flags = Some(u32::from_be_bytes([
                            traf_data[offset],
                            traf_data[offset + 1],
                            traf_data[offset + 2],
                            traf_data[offset + 3],
                        ]));
                    }
                }
            }
        }

        if bt == box_type::TRUN {
            // trun: version(1) + flags(3) + sample_count(4)
            if pos + 12 + 4 <= traf_data.len() {
                let trun_flags = u32::from_be_bytes([
                    0,
                    traf_data[pos + 9],
                    traf_data[pos + 10],
                    traf_data[pos + 11],
                ]);
                // first-sample-flags-present (0x000004)
                if trun_flags & 0x000004 != 0 {
                    // first_sample_flags is after sample_count, and optionally data_offset
                    let mut offset = pos + 12 + 4; // after version+flags+sample_count
                    if trun_flags & 0x000001 != 0 {
                        offset += 4; // data-offset
                    }
                    if offset + 4 <= traf_data.len() {
                        let first_flags = u32::from_be_bytes([
                            traf_data[offset],
                            traf_data[offset + 1],
                            traf_data[offset + 2],
                            traf_data[offset + 3],
                        ]);
                        return is_sync_sample(first_flags);
                    }
                }
                // No first-sample-flags, fall through to default flags
            }
        }

        pos += box_size;
    }

    // Fall back to default flags from tfhd
    if let Some(flags) = default_flags {
        return is_sync_sample(flags);
    }

    // If neither present, assume first chunk is independent
    true
}

/// Check if sample_flags indicate a sync/IDR sample.
/// The `sample_depends_on` field occupies bits 25-24 of sample_flags.
/// Value 2 = "does not depend on others" = sync/IDR.
fn is_sync_sample(flags: u32) -> bool {
    let sample_depends_on = (flags >> 24) & 0x3;
    sample_depends_on == 2
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::cmaf;

    /// Helper: wrap children in a box with the given type.
    fn wrap_box(box_type: &[u8; 4], children: &[u8]) -> Vec<u8> {
        let total = 8 + children.len() as u32;
        let mut output = Vec::with_capacity(total as usize);
        cmaf::write_box_header(&mut output, total, box_type);
        output.extend_from_slice(children);
        output
    }

    /// Build a minimal mfhd box.
    fn build_mfhd(seq: u32) -> Vec<u8> {
        let mut mfhd = Vec::new();
        cmaf::write_full_box_header(&mut mfhd, 16, b"mfhd", 0, 0);
        mfhd.extend_from_slice(&seq.to_be_bytes());
        mfhd
    }

    /// Build a tfhd box with optional default sample flags.
    fn build_tfhd(track_id: u32, default_sample_flags: Option<u32>) -> Vec<u8> {
        let flags: u32 = 0x020000 | if default_sample_flags.is_some() { 0x000020 } else { 0 };
        let size = if default_sample_flags.is_some() { 20u32 } else { 16u32 };
        let mut tfhd = Vec::new();
        cmaf::write_full_box_header(&mut tfhd, size, b"tfhd", 0, flags);
        tfhd.extend_from_slice(&track_id.to_be_bytes());
        if let Some(df) = default_sample_flags {
            tfhd.extend_from_slice(&df.to_be_bytes());
        }
        tfhd
    }

    /// Build a trun box with first_sample_flags.
    fn build_trun_with_first_flags(sample_count: u32, first_flags: u32) -> Vec<u8> {
        // flags: 0x000004 (first_sample_flags_present), 0x000200 (sample_size_present)
        let trun_flags: u32 = 0x000004 | 0x000200;
        let size = 8 + 4 + 4 + 4 + (sample_count * 4); // header + ver_flags + count + first_flags + sizes
        let mut trun = Vec::new();
        cmaf::write_box_header(&mut trun, size, b"trun");
        trun.push(0); // version
        trun.extend_from_slice(&trun_flags.to_be_bytes()[1..4]);
        trun.extend_from_slice(&sample_count.to_be_bytes());
        trun.extend_from_slice(&first_flags.to_be_bytes());
        for _ in 0..sample_count {
            trun.extend_from_slice(&100u32.to_be_bytes()); // sample_size
        }
        trun
    }

    /// Build a simple trun without first_sample_flags.
    fn build_simple_trun(sample_count: u32) -> Vec<u8> {
        let trun_flags: u32 = 0x000200; // sample_size_present only
        let size = 8 + 4 + 4 + (sample_count * 4);
        let mut trun = Vec::new();
        cmaf::write_box_header(&mut trun, size, b"trun");
        trun.push(0);
        trun.extend_from_slice(&trun_flags.to_be_bytes()[1..4]);
        trun.extend_from_slice(&sample_count.to_be_bytes());
        for _ in 0..sample_count {
            trun.extend_from_slice(&100u32.to_be_bytes());
        }
        trun
    }

    /// Build a moof+mdat pair (one chunk).
    fn build_chunk(seq: u32, data_size: usize, first_flags: Option<u32>) -> Vec<u8> {
        let mfhd = build_mfhd(seq);
        let tfhd = build_tfhd(1, None);
        let trun = if let Some(ff) = first_flags {
            build_trun_with_first_flags(1, ff)
        } else {
            build_simple_trun(1)
        };
        let mut traf_children = Vec::new();
        traf_children.extend_from_slice(&tfhd);
        traf_children.extend_from_slice(&trun);
        let traf = wrap_box(b"traf", &traf_children);

        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&mfhd);
        moof_children.extend_from_slice(&traf);
        let moof = wrap_box(b"moof", &moof_children);

        let mdat_payload = vec![0xAA; data_size];
        let mdat = wrap_box(b"mdat", &mdat_payload);

        let mut chunk = Vec::new();
        chunk.extend_from_slice(&moof);
        chunk.extend_from_slice(&mdat);
        chunk
    }

    #[test]
    fn detect_single_chunk() {
        let segment = build_chunk(1, 100, Some(0x02000000));
        let boundaries = detect_chunk_boundaries(&segment);
        assert_eq!(boundaries.len(), 1);
        assert_eq!(boundaries[0].offset, 0);
        assert_eq!(boundaries[0].size, segment.len());
        assert!(boundaries[0].independent);
    }

    #[test]
    fn detect_multiple_chunks() {
        let mut segment = Vec::new();
        // IDR chunk
        let chunk1 = build_chunk(1, 200, Some(0x02000000));
        // Non-IDR chunk (sample_depends_on=1 => depends on others)
        let chunk2 = build_chunk(2, 150, Some(0x01000000));
        // Another non-IDR
        let chunk3 = build_chunk(3, 100, Some(0x01000000));

        segment.extend_from_slice(&chunk1);
        segment.extend_from_slice(&chunk2);
        segment.extend_from_slice(&chunk3);

        let boundaries = detect_chunk_boundaries(&segment);
        assert_eq!(boundaries.len(), 3);

        assert_eq!(boundaries[0].offset, 0);
        assert!(boundaries[0].independent);

        assert_eq!(boundaries[1].offset, chunk1.len());
        assert!(!boundaries[1].independent);

        assert_eq!(boundaries[2].offset, chunk1.len() + chunk2.len());
        assert!(!boundaries[2].independent);
    }

    #[test]
    fn detect_empty_data() {
        let boundaries = detect_chunk_boundaries(&[]);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn detect_too_short_data() {
        let boundaries = detect_chunk_boundaries(&[0x00, 0x00, 0x00]);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn detect_no_moof() {
        // Just an ftyp box, no moof+mdat
        let ftyp = wrap_box(b"ftyp", b"isom\x00\x00\x02\x00");
        let boundaries = detect_chunk_boundaries(&ftyp);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn detect_moof_without_mdat() {
        // moof without a following mdat should not be detected
        let mfhd = build_mfhd(1);
        let tfhd = build_tfhd(1, None);
        let trun = build_simple_trun(1);
        let mut traf_children = Vec::new();
        traf_children.extend_from_slice(&tfhd);
        traf_children.extend_from_slice(&trun);
        let traf = wrap_box(b"traf", &traf_children);
        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&mfhd);
        moof_children.extend_from_slice(&traf);
        let moof = wrap_box(b"moof", &moof_children);

        // Follow with a non-mdat box
        let free_box = wrap_box(b"free", &[0u8; 8]);
        let mut data = Vec::new();
        data.extend_from_slice(&moof);
        data.extend_from_slice(&free_box);

        let boundaries = detect_chunk_boundaries(&data);
        assert!(boundaries.is_empty());
    }

    #[test]
    fn extract_chunk_valid() {
        let segment = build_chunk(1, 50, None);
        let boundaries = detect_chunk_boundaries(&segment);
        assert_eq!(boundaries.len(), 1);
        let extracted = extract_chunk(&segment, &boundaries[0]);
        assert!(extracted.is_some());
        assert_eq!(extracted.unwrap().len(), segment.len());
    }

    #[test]
    fn extract_chunk_out_of_bounds() {
        let segment = build_chunk(1, 50, None);
        let oob = ChunkBoundary {
            offset: 0,
            size: segment.len() + 100, // larger than data
            duration_ticks: 0,
            independent: true,
        };
        assert!(extract_chunk(&segment, &oob).is_none());
    }

    #[test]
    fn extract_second_chunk_from_multi() {
        let chunk1 = build_chunk(1, 100, Some(0x02000000));
        let chunk2 = build_chunk(2, 80, Some(0x01000000));
        let mut segment = Vec::new();
        segment.extend_from_slice(&chunk1);
        segment.extend_from_slice(&chunk2);

        let boundaries = detect_chunk_boundaries(&segment);
        assert_eq!(boundaries.len(), 2);

        let extracted = extract_chunk(&segment, &boundaries[1]).unwrap();
        assert_eq!(extracted.len(), chunk2.len());
    }

    #[test]
    fn independence_from_tfhd_default_flags() {
        // No first_sample_flags in trun, but default_sample_flags in tfhd
        let mfhd = build_mfhd(1);
        // default_sample_flags with sample_depends_on=2 (independent)
        let tfhd = build_tfhd(1, Some(0x02000000));
        let trun = build_simple_trun(1);
        let mut traf_children = Vec::new();
        traf_children.extend_from_slice(&tfhd);
        traf_children.extend_from_slice(&trun);
        let traf = wrap_box(b"traf", &traf_children);
        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&mfhd);
        moof_children.extend_from_slice(&traf);
        let moof = wrap_box(b"moof", &moof_children);
        let mdat = wrap_box(b"mdat", &[0xBB; 50]);

        let mut segment = Vec::new();
        segment.extend_from_slice(&moof);
        segment.extend_from_slice(&mdat);

        let boundaries = detect_chunk_boundaries(&segment);
        assert_eq!(boundaries.len(), 1);
        assert!(boundaries[0].independent);
    }

    #[test]
    fn non_independence_from_tfhd_default_flags() {
        let mfhd = build_mfhd(1);
        // default_sample_flags with sample_depends_on=1 (depends on others)
        let tfhd = build_tfhd(1, Some(0x01000000));
        let trun = build_simple_trun(1);
        let mut traf_children = Vec::new();
        traf_children.extend_from_slice(&tfhd);
        traf_children.extend_from_slice(&trun);
        let traf = wrap_box(b"traf", &traf_children);
        let mut moof_children = Vec::new();
        moof_children.extend_from_slice(&mfhd);
        moof_children.extend_from_slice(&traf);
        let moof = wrap_box(b"moof", &moof_children);
        let mdat = wrap_box(b"mdat", &[0xCC; 50]);

        let mut segment = Vec::new();
        segment.extend_from_slice(&moof);
        segment.extend_from_slice(&mdat);

        let boundaries = detect_chunk_boundaries(&segment);
        assert_eq!(boundaries.len(), 1);
        assert!(!boundaries[0].independent);
    }

    #[test]
    fn is_sync_sample_independent() {
        // sample_depends_on=2 at bits 25-24
        assert!(is_sync_sample(0x02000000));
    }

    #[test]
    fn is_sync_sample_dependent() {
        // sample_depends_on=1 at bits 25-24
        assert!(!is_sync_sample(0x01000000));
    }

    #[test]
    fn is_sync_sample_unknown() {
        // sample_depends_on=0 (unknown)
        assert!(!is_sync_sample(0x00000000));
    }
}
