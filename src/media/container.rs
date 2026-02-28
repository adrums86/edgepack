use serde::{Deserialize, Serialize};

use crate::media::box_type;
use crate::media::cmaf;

/// Container format for media output.
///
/// Determines segment file extensions, ftyp box brands, and DASH profile signaling.
/// Both formats use ISOBMFF (ISO 14496-12) and produce identical moof/mdat structures;
/// the difference is in metadata signaling and naming conventions.
///
/// CMAF is the default (backward compatible with existing behavior).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ContainerFormat {
    /// CMAF: Common Media Application Format (ISO/IEC 23000-19).
    /// Uses `cmfc` compatible brand, `.cmfv`/`.cmfa` segment extensions.
    Cmaf,
    /// fMP4: Fragmented MP4 (ISO 14496-12).
    /// Uses `iso6` compatible brands only, `.m4s` segment extensions.
    Fmp4,
    /// ISO BMFF: ISO Base Media File Format (ISO 14496-12).
    /// Uses `iso6` compatible brands only, `.mp4` segment extensions.
    Iso,
}

impl ContainerFormat {
    /// Video segment file extension.
    ///
    /// Returns `".cmfv"` for CMAF, `".m4s"` for fMP4, `".mp4"` for ISO BMFF.
    pub fn video_segment_extension(&self) -> &'static str {
        match self {
            ContainerFormat::Cmaf => ".cmfv",
            ContainerFormat::Fmp4 => ".m4s",
            ContainerFormat::Iso => ".mp4",
        }
    }

    /// Audio segment file extension.
    pub fn audio_segment_extension(&self) -> &'static str {
        match self {
            ContainerFormat::Cmaf => ".cmfa",
            ContainerFormat::Fmp4 => ".m4s",
            ContainerFormat::Iso => ".mp4",
        }
    }

    /// Init segment file extension.
    ///
    /// Returns `".mp4"` for both formats.
    pub fn init_extension(&self) -> &'static str {
        ".mp4"
    }

    /// Major brand for the ftyp box.
    ///
    /// Returns `b"isom"` for both formats.
    pub fn major_brand(&self) -> &'static [u8; 4] {
        b"isom"
    }

    /// Compatible brands for the ftyp box.
    ///
    /// CMAF includes `cmfc` brand; fMP4 and ISO BMFF do not.
    pub fn compatible_brands(&self) -> &'static [&'static [u8; 4]] {
        match self {
            ContainerFormat::Cmaf => &[b"isom", b"iso6", b"cmfc"],
            ContainerFormat::Fmp4 | ContainerFormat::Iso => &[b"isom", b"iso6"],
        }
    }

    /// Build a complete ftyp box for this container format.
    ///
    /// Structure: box_header(8) + major_brand(4) + minor_version(4) + compatible_brands(4*n)
    pub fn build_ftyp(&self) -> Vec<u8> {
        let brands = self.compatible_brands();
        let total_size = (8 + 4 + 4 + brands.len() * 4) as u32;
        let mut output = Vec::with_capacity(total_size as usize);
        cmaf::write_box_header(&mut output, total_size, &box_type::FTYP);
        output.extend_from_slice(self.major_brand());
        output.extend_from_slice(&0x00000200u32.to_be_bytes()); // minor version
        for brand in brands {
            output.extend_from_slice(*brand);
        }
        output
    }

    /// DASH MPD profiles attribute value.
    ///
    /// CMAF includes the CMAF profile; fMP4 and ISO BMFF use only the DASH live profile.
    pub fn dash_profiles(&self) -> &'static str {
        match self {
            ContainerFormat::Cmaf => {
                "urn:mpeg:dash:profile:isoff-live:2011,urn:mpeg:dash:profile:cmaf:2019"
            }
            ContainerFormat::Fmp4 | ContainerFormat::Iso => {
                "urn:mpeg:dash:profile:isoff-live:2011"
            }
        }
    }

    /// Parse a string value to a ContainerFormat.
    ///
    /// Accepts `"cmaf"`, `"fmp4"`, or `"iso"`. Returns `None` for unrecognized values.
    pub fn from_str_value(s: &str) -> Option<Self> {
        match s {
            "cmaf" => Some(ContainerFormat::Cmaf),
            "fmp4" => Some(ContainerFormat::Fmp4),
            "iso" => Some(ContainerFormat::Iso),
            _ => None,
        }
    }

    /// String representation for serialization/display.
    pub fn as_str(&self) -> &'static str {
        match self {
            ContainerFormat::Cmaf => "cmaf",
            ContainerFormat::Fmp4 => "fmp4",
            ContainerFormat::Iso => "iso",
        }
    }
}

impl std::fmt::Display for ContainerFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Default for ContainerFormat {
    fn default() -> Self {
        ContainerFormat::Cmaf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_segment_extension_cmaf() {
        assert_eq!(ContainerFormat::Cmaf.video_segment_extension(), ".cmfv");
    }

    #[test]
    fn audio_segment_extension_cmaf() {
        assert_eq!(ContainerFormat::Cmaf.audio_segment_extension(), ".cmfa");
    }

    #[test]
    fn audio_segment_extension_fmp4() {
        assert_eq!(ContainerFormat::Fmp4.audio_segment_extension(), ".m4s");
    }

    #[test]
    fn init_extension_both_formats() {
        assert_eq!(ContainerFormat::Cmaf.init_extension(), ".mp4");
        assert_eq!(ContainerFormat::Fmp4.init_extension(), ".mp4");
    }

    #[test]
    fn major_brand_both_isom() {
        assert_eq!(ContainerFormat::Cmaf.major_brand(), b"isom");
        assert_eq!(ContainerFormat::Fmp4.major_brand(), b"isom");
    }

    #[test]
    fn compatible_brands_cmaf_has_cmfc() {
        let brands = ContainerFormat::Cmaf.compatible_brands();
        assert_eq!(brands.len(), 3);
        assert!(brands.contains(&&b"cmfc"));
        assert!(brands.contains(&&b"isom"));
        assert!(brands.contains(&&b"iso6"));
    }

    #[test]
    fn compatible_brands_fmp4_no_cmfc() {
        let brands = ContainerFormat::Fmp4.compatible_brands();
        assert_eq!(brands.len(), 2);
        assert!(!brands.contains(&&b"cmfc"));
        assert!(brands.contains(&&b"isom"));
        assert!(brands.contains(&&b"iso6"));
    }

    #[test]
    fn build_ftyp_cmaf() {
        let ftyp = ContainerFormat::Cmaf.build_ftyp();
        // header(8) + major(4) + minor(4) + 3 brands(12) = 28
        assert_eq!(ftyp.len(), 28);
        // Verify box type
        assert_eq!(&ftyp[4..8], b"ftyp");
        // Verify major brand
        assert_eq!(&ftyp[8..12], b"isom");
        // Verify cmfc is present in compatible brands
        let brands_data = &ftyp[16..];
        assert!(brands_data.chunks(4).any(|b| b == b"cmfc"));
    }

    #[test]
    fn build_ftyp_fmp4() {
        let ftyp = ContainerFormat::Fmp4.build_ftyp();
        // header(8) + major(4) + minor(4) + 2 brands(8) = 24
        assert_eq!(ftyp.len(), 24);
        // Verify box type
        assert_eq!(&ftyp[4..8], b"ftyp");
        // Verify major brand
        assert_eq!(&ftyp[8..12], b"isom");
        // Verify cmfc is NOT present
        let brands_data = &ftyp[16..];
        assert!(!brands_data.chunks(4).any(|b| b == b"cmfc"));
    }

    #[test]
    fn build_ftyp_size_differs() {
        let cmaf = ContainerFormat::Cmaf.build_ftyp();
        let fmp4 = ContainerFormat::Fmp4.build_ftyp();
        // CMAF has one extra brand (cmfc), so 4 bytes larger
        assert_eq!(cmaf.len() - fmp4.len(), 4);
    }

    #[test]
    fn dash_profiles_cmaf_includes_cmaf() {
        let profiles = ContainerFormat::Cmaf.dash_profiles();
        assert!(profiles.contains("cmaf:2019"));
        assert!(profiles.contains("isoff-live:2011"));
    }

    #[test]
    fn dash_profiles_fmp4_no_cmaf() {
        let profiles = ContainerFormat::Fmp4.dash_profiles();
        assert!(!profiles.contains("cmaf"));
        assert!(profiles.contains("isoff-live:2011"));
    }

    #[test]
    fn video_segment_extension_fmp4() {
        assert_eq!(ContainerFormat::Fmp4.video_segment_extension(), ".m4s");
    }

    #[test]
    fn video_segment_extension_iso() {
        assert_eq!(ContainerFormat::Iso.video_segment_extension(), ".mp4");
    }

    #[test]
    fn audio_segment_extension_iso() {
        assert_eq!(ContainerFormat::Iso.audio_segment_extension(), ".mp4");
    }

    #[test]
    fn init_extension_iso() {
        assert_eq!(ContainerFormat::Iso.init_extension(), ".mp4");
    }

    #[test]
    fn compatible_brands_iso_no_cmfc() {
        // Iso has the same brands as Fmp4 by design — they differ only in segment extension.
        let brands = ContainerFormat::Iso.compatible_brands();
        assert_eq!(brands.len(), 2);
        assert!(!brands.contains(&&b"cmfc"));
        assert!(brands.contains(&&b"isom"));
        assert!(brands.contains(&&b"iso6"));
    }

    #[test]
    fn build_ftyp_iso() {
        // Iso ftyp is identical to Fmp4 by design — same brands, same structure.
        let ftyp = ContainerFormat::Iso.build_ftyp();
        // header(8) + major(4) + minor(4) + 2 brands(8) = 24 (same as fmp4)
        assert_eq!(ftyp.len(), 24);
        assert_eq!(&ftyp[4..8], b"ftyp");
        assert_eq!(&ftyp[8..12], b"isom");
        let brands_data = &ftyp[16..];
        assert!(!brands_data.chunks(4).any(|b| b == b"cmfc"));
    }

    #[test]
    fn dash_profiles_iso_no_cmaf() {
        // Iso uses the same DASH profile as Fmp4 by design — they differ only in segment extension.
        let profiles = ContainerFormat::Iso.dash_profiles();
        assert!(!profiles.contains("cmaf"));
        assert!(profiles.contains("isoff-live:2011"));
    }

    #[test]
    fn from_str_value_valid() {
        assert_eq!(ContainerFormat::from_str_value("cmaf"), Some(ContainerFormat::Cmaf));
        assert_eq!(ContainerFormat::from_str_value("fmp4"), Some(ContainerFormat::Fmp4));
        assert_eq!(ContainerFormat::from_str_value("iso"), Some(ContainerFormat::Iso));
    }

    #[test]
    fn from_str_value_invalid() {
        assert_eq!(ContainerFormat::from_str_value("mp4"), None);
        assert_eq!(ContainerFormat::from_str_value(""), None);
        assert_eq!(ContainerFormat::from_str_value("CMAF"), None);
    }

    #[test]
    fn as_str_values() {
        assert_eq!(ContainerFormat::Cmaf.as_str(), "cmaf");
        assert_eq!(ContainerFormat::Fmp4.as_str(), "fmp4");
        assert_eq!(ContainerFormat::Iso.as_str(), "iso");
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", ContainerFormat::Cmaf), "cmaf");
        assert_eq!(format!("{}", ContainerFormat::Fmp4), "fmp4");
        assert_eq!(format!("{}", ContainerFormat::Iso), "iso");
    }

    #[test]
    fn default_is_cmaf() {
        assert_eq!(ContainerFormat::default(), ContainerFormat::Cmaf);
    }

    #[test]
    fn serde_roundtrip_cmaf() {
        let fmt = ContainerFormat::Cmaf;
        let json = serde_json::to_string(&fmt).unwrap();
        let parsed: ContainerFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, fmt);
    }

    #[test]
    fn serde_roundtrip_fmp4() {
        let fmt = ContainerFormat::Fmp4;
        let json = serde_json::to_string(&fmt).unwrap();
        let parsed: ContainerFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, fmt);
    }

    #[test]
    fn serde_roundtrip_iso() {
        let fmt = ContainerFormat::Iso;
        let json = serde_json::to_string(&fmt).unwrap();
        let parsed: ContainerFormat = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, fmt);
    }

    #[test]
    fn equality_and_hash() {
        use std::collections::HashSet;
        assert_eq!(ContainerFormat::Cmaf, ContainerFormat::Cmaf);
        assert_eq!(ContainerFormat::Fmp4, ContainerFormat::Fmp4);
        assert_eq!(ContainerFormat::Iso, ContainerFormat::Iso);
        assert_ne!(ContainerFormat::Cmaf, ContainerFormat::Fmp4);
        assert_ne!(ContainerFormat::Cmaf, ContainerFormat::Iso);
        assert_ne!(ContainerFormat::Fmp4, ContainerFormat::Iso);

        let mut set = HashSet::new();
        set.insert(ContainerFormat::Cmaf);
        set.insert(ContainerFormat::Fmp4);
        set.insert(ContainerFormat::Iso);
        assert_eq!(set.len(), 3);
    }
}
