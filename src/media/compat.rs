use crate::drm::scheme::EncryptionScheme;
use crate::media::box_type;
use crate::media::cmaf::{find_child_box, iterate_boxes};
use crate::media::codec::TrackInfo;
use crate::media::container::ContainerFormat;
use serde::{Deserialize, Serialize};

/// Result of a validation check.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationResult {
    pub valid: bool,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl ValidationResult {
    pub fn ok() -> Self {
        Self {
            valid: true,
            warnings: Vec::new(),
            errors: Vec::new(),
        }
    }

    pub fn with_warning(mut self, msg: impl Into<String>) -> Self {
        self.warnings.push(msg.into());
        self
    }

    pub fn with_error(mut self, msg: impl Into<String>) -> Self {
        self.errors.push(msg.into());
        self.valid = false;
        self
    }

    pub fn merge(&mut self, other: ValidationResult) {
        if !other.valid {
            self.valid = false;
        }
        self.warnings.extend(other.warnings);
        self.errors.extend(other.errors);
    }
}

/// HDR format detected from codec string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HdrFormat {
    /// HEVC Main 10 profile (HDR10 or HDR10+).
    Hdr10,
    /// Dolby Vision (dvhe/dvav prefix).
    DolbyVision,
    /// HLG (Hybrid Log-Gamma) — HEVC Main 10 with HLG transfer.
    Hlg,
}

/// Check if a codec string indicates HDR content.
pub fn is_hdr_codec(codec_string: &str) -> bool {
    detect_hdr_format(codec_string).is_some()
}

/// Detect HDR format from a codec string.
pub fn detect_hdr_format(codec_string: &str) -> Option<HdrFormat> {
    let lower = codec_string.to_lowercase();

    // Dolby Vision: dvhe.* or dvav.*
    if lower.starts_with("dvhe.") || lower.starts_with("dvav.") {
        return Some(HdrFormat::DolbyVision);
    }

    // HEVC Main 10 profile: hev1.2.* or hvc1.2.*
    // Profile 2 = Main 10, which is the base for HDR10/HDR10+/HLG
    if lower.starts_with("hev1.2.") || lower.starts_with("hvc1.2.") {
        return Some(HdrFormat::Hdr10);
    }

    // AV1 with high bit depth (10+): av01.X.YYM.10 or higher
    if lower.starts_with("av01.") {
        let parts: Vec<&str> = lower.split('.').collect();
        if parts.len() >= 4 {
            // Fourth component is bit depth (e.g., "10", "12")
            if let Ok(depth) = parts[3].trim_end_matches(|c: char| !c.is_ascii_digit()).parse::<u32>() {
                if depth >= 10 {
                    return Some(HdrFormat::Hdr10);
                }
            }
        }
    }

    // VP9 profile 2 or 3 (10/12-bit): vp09.02.* or vp09.03.*
    if lower.starts_with("vp09.02.") || lower.starts_with("vp09.03.") {
        return Some(HdrFormat::Hdr10);
    }

    None
}

/// Validate codec+scheme compatibility for a single track.
pub fn validate_codec_scheme(
    codec_string: &str,
    _source_scheme: EncryptionScheme,
    target_scheme: EncryptionScheme,
) -> ValidationResult {
    let mut result = ValidationResult::ok();
    let lower = codec_string.to_lowercase();

    // Text tracks should not be encrypted
    if (lower == "wvtt" || lower == "stpp") && target_scheme.is_encrypted() {
        return result.with_error(format!(
            "text track codec '{codec_string}' cannot be encrypted (subtitles bypass encryption)"
        ));
    }

    // VP9 does not support CBCS pattern encryption
    if lower.starts_with("vp09.") && target_scheme == EncryptionScheme::Cbcs {
        return result.with_error(
            "VP9 does not support CBCS pattern encryption".to_string(),
        );
    }

    // AV1 + CBCS: limited device support
    if lower.starts_with("av01.") && target_scheme == EncryptionScheme::Cbcs {
        result = result.with_warning(
            "AV1 with CBCS has limited device support".to_string(),
        );
    }

    // HEVC + CENC: requires subsample encryption
    if (lower.starts_with("hev1.") || lower.starts_with("hvc1."))
        && target_scheme == EncryptionScheme::Cenc
    {
        result = result.with_warning(
            "HEVC with CENC requires subsample encryption for NAL unit compliance".to_string(),
        );
    }

    // Dolby Vision RPU preservation warning
    if (lower.starts_with("dvhe.") || lower.starts_with("dvav."))
        && target_scheme.is_encrypted()
    {
        result = result.with_warning(
            "Dolby Vision RPU NALs must survive encryption transform — verify output playback"
                .to_string(),
        );
    }

    // HDR metadata preservation warning
    if let Some(hdr_fmt) = detect_hdr_format(codec_string) {
        if target_scheme.is_encrypted() && hdr_fmt != HdrFormat::DolbyVision {
            result = result.with_warning(
                "HDR metadata (SEI NALs) must survive encryption transform — verify output playback"
                    .to_string(),
            );
        }
    }

    result
}

/// Validate container format against output formats.
///
/// TS container format is only supported with HLS — DASH does not support TS segments.
#[cfg(feature = "ts")]
pub fn validate_container_output_formats(
    container_format: ContainerFormat,
    output_formats: &[crate::manifest::types::OutputFormat],
) -> ValidationResult {
    let mut result = ValidationResult::ok();

    if matches!(container_format, ContainerFormat::Ts) {
        if output_formats.iter().any(|f| matches!(f, crate::manifest::types::OutputFormat::Dash)) {
            result = result.with_error(
                "TS container format is not supported with DASH output".to_string(),
            );
        }
    }

    result
}

/// Validate a complete repackage request before processing.
pub fn validate_repackage_request(
    source_scheme: EncryptionScheme,
    target_schemes: &[EncryptionScheme],
    _container_format: ContainerFormat,
    tracks: &[TrackInfo],
) -> ValidationResult {
    let mut result = ValidationResult::ok();

    // Must have at least one target scheme
    if target_schemes.is_empty() {
        return result.with_error("at least one target scheme is required".to_string());
    }

    // Validate each track against each target scheme
    for track in tracks {
        for target in target_schemes {
            let track_result = validate_codec_scheme(
                &track.codec_string,
                source_scheme,
                *target,
            );
            result.merge(track_result);
        }
    }

    result
}

/// Validate init segment structure.
pub fn validate_init_segment(init_data: &[u8]) -> ValidationResult {
    let mut result = ValidationResult::ok();

    if init_data.len() < 8 {
        return result.with_error("init segment too small".to_string());
    }

    let mut found_ftyp = false;
    let mut found_moov = false;
    let mut first_box = true;

    for box_result in iterate_boxes(init_data) {
        let header = match box_result {
            Ok(h) => h,
            Err(e) => {
                return result.with_error(format!("failed to parse box: {e}"));
            }
        };

        if first_box {
            if header.box_type != box_type::FTYP {
                result = result.with_warning("first box is not ftyp".to_string());
            }
            first_box = false;
        }

        if header.box_type == box_type::FTYP {
            found_ftyp = true;
        }
        if header.box_type == box_type::MOOV {
            found_moov = true;
            // Validate moov has at least one trak
            let payload_start = header.payload_offset() as usize;
            let payload_end = (header.offset + header.size) as usize;
            if payload_end <= init_data.len() {
                let moov_payload = &init_data[payload_start..payload_end];
                if find_child_box(moov_payload, &box_type::TRAK).is_none() {
                    result = result.with_error("moov box contains no trak".to_string());
                }
            }
        }
    }

    if !found_ftyp {
        result = result.with_error("missing ftyp box".to_string());
    }
    if !found_moov {
        result = result.with_error("missing moov box".to_string());
    }

    result
}

/// Validate media segment structure.
pub fn validate_media_segment(segment_data: &[u8], expected_encrypted: bool) -> ValidationResult {
    let mut result = ValidationResult::ok();

    if segment_data.len() < 8 {
        return result.with_error("media segment too small".to_string());
    }

    let mut found_moof = false;
    let mut found_mdat = false;

    for box_result in iterate_boxes(segment_data) {
        let header = match box_result {
            Ok(h) => h,
            Err(e) => {
                return result.with_error(format!("failed to parse box: {e}"));
            }
        };

        if header.box_type == box_type::MOOF {
            found_moof = true;
            // Check for traf and trun inside moof
            let payload_start = header.payload_offset() as usize;
            let payload_end = (header.offset + header.size) as usize;
            if payload_end <= segment_data.len() {
                let moof_payload = &segment_data[payload_start..payload_end];
                let traf = find_child_box(moof_payload, &box_type::TRAF);
                if traf.is_none() {
                    result = result.with_error("moof box contains no traf".to_string());
                } else if let Some(traf_header) = traf {
                    let traf_start = traf_header.payload_offset() as usize;
                    let traf_end = (traf_header.offset + traf_header.size) as usize;
                    if traf_end <= moof_payload.len() {
                        let traf_payload = &moof_payload[traf_start..traf_end];
                        if find_child_box(traf_payload, &box_type::TRUN).is_none() {
                            result = result.with_error("traf box contains no trun".to_string());
                        }
                        let has_senc = find_child_box(traf_payload, &box_type::SENC).is_some();
                        if expected_encrypted && !has_senc {
                            result = result.with_warning(
                                "expected encrypted segment but no senc box found".to_string(),
                            );
                        }
                    }
                }
            }
        }
        if header.box_type == box_type::MDAT {
            found_mdat = true;
        }
    }

    if !found_moof {
        result = result.with_error("missing moof box".to_string());
    }
    if !found_mdat {
        result = result.with_error("missing mdat box".to_string());
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::TrackType;

    // --- ValidationResult ---

    #[test]
    fn validation_result_ok() {
        let r = ValidationResult::ok();
        assert!(r.valid);
        assert!(r.warnings.is_empty());
        assert!(r.errors.is_empty());
    }

    #[test]
    fn validation_result_with_warning() {
        let r = ValidationResult::ok().with_warning("test warning");
        assert!(r.valid);
        assert_eq!(r.warnings.len(), 1);
        assert_eq!(r.warnings[0], "test warning");
    }

    #[test]
    fn validation_result_with_error() {
        let r = ValidationResult::ok().with_error("test error");
        assert!(!r.valid);
        assert_eq!(r.errors.len(), 1);
        assert_eq!(r.errors[0], "test error");
    }

    #[test]
    fn validation_result_merge() {
        let mut a = ValidationResult::ok().with_warning("w1");
        let b = ValidationResult::ok().with_error("e1").with_warning("w2");
        a.merge(b);
        assert!(!a.valid);
        assert_eq!(a.warnings.len(), 2);
        assert_eq!(a.errors.len(), 1);
    }

    #[test]
    fn validation_result_serde_roundtrip() {
        let r = ValidationResult::ok().with_warning("w").with_error("e");
        let json = serde_json::to_string(&r).unwrap();
        let parsed: ValidationResult = serde_json::from_str(&json).unwrap();
        assert!(!parsed.valid);
        assert_eq!(parsed.warnings, vec!["w"]);
        assert_eq!(parsed.errors, vec!["e"]);
    }

    // --- HDR detection ---

    #[test]
    fn detect_hdr_dolby_vision_dvhe() {
        assert_eq!(detect_hdr_format("dvhe.05.06"), Some(HdrFormat::DolbyVision));
    }

    #[test]
    fn detect_hdr_dolby_vision_dvav() {
        assert_eq!(detect_hdr_format("dvav.se.06"), Some(HdrFormat::DolbyVision));
    }

    #[test]
    fn detect_hdr_hevc_main10() {
        assert_eq!(detect_hdr_format("hev1.2.4.L120.90"), Some(HdrFormat::Hdr10));
    }

    #[test]
    fn detect_hdr_hvc1_main10() {
        assert_eq!(detect_hdr_format("hvc1.2.4.L93.B0"), Some(HdrFormat::Hdr10));
    }

    #[test]
    fn detect_hdr_av1_10bit() {
        assert_eq!(detect_hdr_format("av01.0.09M.10"), Some(HdrFormat::Hdr10));
    }

    #[test]
    fn detect_hdr_vp9_profile2() {
        assert_eq!(detect_hdr_format("vp09.02.10.10"), Some(HdrFormat::Hdr10));
    }

    #[test]
    fn detect_hdr_vp9_profile3() {
        assert_eq!(detect_hdr_format("vp09.03.10.12"), Some(HdrFormat::Hdr10));
    }

    #[test]
    fn detect_non_hdr_avc() {
        assert_eq!(detect_hdr_format("avc1.64001f"), None);
    }

    #[test]
    fn detect_non_hdr_aac() {
        assert_eq!(detect_hdr_format("mp4a.40.2"), None);
    }

    #[test]
    fn detect_non_hdr_hevc_main() {
        // Profile 1 = Main (SDR)
        assert_eq!(detect_hdr_format("hev1.1.4.L120.90"), None);
    }

    #[test]
    fn is_hdr_codec_true() {
        assert!(is_hdr_codec("hev1.2.4.L120.90"));
        assert!(is_hdr_codec("dvhe.05.06"));
    }

    #[test]
    fn is_hdr_codec_false() {
        assert!(!is_hdr_codec("avc1.64001f"));
        assert!(!is_hdr_codec("mp4a.40.2"));
    }

    // --- Codec+scheme validation ---

    #[test]
    fn validate_codec_scheme_text_encrypted_error() {
        let r = validate_codec_scheme("wvtt", EncryptionScheme::None, EncryptionScheme::Cenc);
        assert!(!r.valid);
        assert!(r.errors[0].contains("text track"));
    }

    #[test]
    fn validate_codec_scheme_stpp_encrypted_error() {
        let r = validate_codec_scheme("stpp", EncryptionScheme::None, EncryptionScheme::Cbcs);
        assert!(!r.valid);
    }

    #[test]
    fn validate_codec_scheme_text_clear_ok() {
        let r = validate_codec_scheme("wvtt", EncryptionScheme::None, EncryptionScheme::None);
        assert!(r.valid);
    }

    #[test]
    fn validate_codec_scheme_vp9_cbcs_error() {
        let r = validate_codec_scheme("vp09.00.10.08", EncryptionScheme::None, EncryptionScheme::Cbcs);
        assert!(!r.valid);
        assert!(r.errors[0].contains("VP9"));
    }

    #[test]
    fn validate_codec_scheme_vp9_cenc_ok() {
        let r = validate_codec_scheme("vp09.00.10.08", EncryptionScheme::None, EncryptionScheme::Cenc);
        assert!(r.valid);
    }

    #[test]
    fn validate_codec_scheme_av1_cbcs_warning() {
        let r = validate_codec_scheme("av01.0.04M.08", EncryptionScheme::None, EncryptionScheme::Cbcs);
        assert!(r.valid);
        assert!(r.warnings.iter().any(|w| w.contains("AV1")));
    }

    #[test]
    fn validate_codec_scheme_hevc_cenc_warning() {
        let r = validate_codec_scheme("hev1.1.4.L120.90", EncryptionScheme::None, EncryptionScheme::Cenc);
        assert!(r.valid);
        assert!(r.warnings.iter().any(|w| w.contains("HEVC")));
    }

    #[test]
    fn validate_codec_scheme_dolby_vision_warning() {
        let r = validate_codec_scheme("dvhe.05.06", EncryptionScheme::None, EncryptionScheme::Cenc);
        assert!(r.valid);
        assert!(r.warnings.iter().any(|w| w.contains("Dolby Vision")));
    }

    #[test]
    fn validate_codec_scheme_avc_cenc_ok() {
        let r = validate_codec_scheme("avc1.64001f", EncryptionScheme::None, EncryptionScheme::Cenc);
        assert!(r.valid);
        assert!(r.warnings.is_empty());
    }

    #[test]
    fn validate_codec_scheme_aac_clear_ok() {
        let r = validate_codec_scheme("mp4a.40.2", EncryptionScheme::Cbcs, EncryptionScheme::None);
        assert!(r.valid);
    }

    // --- Repackage request validation ---

    #[test]
    fn validate_repackage_request_empty_schemes() {
        let r = validate_repackage_request(
            EncryptionScheme::None,
            &[],
            ContainerFormat::Cmaf,
            &[],
        );
        assert!(!r.valid);
        assert!(r.errors[0].contains("at least one target scheme"));
    }

    #[test]
    fn validate_repackage_request_valid() {
        let tracks = vec![
            TrackInfo {
                track_type: TrackType::Video,
                track_id: 1,
                codec_string: "avc1.64001f".to_string(),
                timescale: 90000,
                kid: None,
                language: None,
                width: Some(1920),
                height: Some(1080),
            },
            TrackInfo {
                track_type: TrackType::Audio,
                track_id: 2,
                codec_string: "mp4a.40.2".to_string(),
                timescale: 44100,
                kid: None,
                language: None,
                width: None,
                height: None,
            },
        ];
        let r = validate_repackage_request(
            EncryptionScheme::None,
            &[EncryptionScheme::Cenc],
            ContainerFormat::Cmaf,
            &tracks,
        );
        assert!(r.valid);
    }

    #[test]
    fn validate_repackage_request_with_errors() {
        let tracks = vec![TrackInfo {
            track_type: TrackType::Video,
            track_id: 1,
            codec_string: "vp09.00.10.08".to_string(),
            timescale: 90000,
            kid: None,
            language: None,
            width: Some(1920),
            height: Some(1080),
        }];
        let r = validate_repackage_request(
            EncryptionScheme::None,
            &[EncryptionScheme::Cbcs],
            ContainerFormat::Cmaf,
            &tracks,
        );
        assert!(!r.valid);
    }

    #[test]
    fn validate_repackage_request_dual_scheme() {
        let tracks = vec![TrackInfo {
            track_type: TrackType::Video,
            track_id: 1,
            codec_string: "avc1.64001f".to_string(),
            timescale: 90000,
            kid: None,
            language: None,
            width: Some(1920),
            height: Some(1080),
        }];
        let r = validate_repackage_request(
            EncryptionScheme::Cbcs,
            &[EncryptionScheme::Cenc, EncryptionScheme::Cbcs],
            ContainerFormat::Cmaf,
            &tracks,
        );
        assert!(r.valid);
    }

    // --- Init segment validation ---

    #[test]
    fn validate_init_segment_too_small() {
        let r = validate_init_segment(&[0; 4]);
        assert!(!r.valid);
    }

    #[test]
    fn validate_init_segment_no_ftyp() {
        // Just a moov box with a trak child
        let mut data = Vec::new();
        // Build trak child
        let trak_payload = vec![0u8; 4];
        let trak_size = (8 + trak_payload.len()) as u32;
        let mut trak = Vec::new();
        trak.extend_from_slice(&trak_size.to_be_bytes());
        trak.extend_from_slice(b"trak");
        trak.extend_from_slice(&trak_payload);
        // Build moov
        let moov_size = (8 + trak.len()) as u32;
        data.extend_from_slice(&moov_size.to_be_bytes());
        data.extend_from_slice(b"moov");
        data.extend_from_slice(&trak);

        let r = validate_init_segment(&data);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("ftyp")));
    }

    #[test]
    fn validate_init_segment_no_moov() {
        // Just an ftyp box
        let mut data = Vec::new();
        let ftyp_payload = b"isom\x00\x00\x00\x00isom";
        let ftyp_size = (8 + ftyp_payload.len()) as u32;
        data.extend_from_slice(&ftyp_size.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(ftyp_payload);

        let r = validate_init_segment(&data);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("moov")));
    }

    #[test]
    fn validate_init_segment_moov_no_trak() {
        let mut data = Vec::new();
        // ftyp
        let ftyp_size: u32 = 20;
        data.extend_from_slice(&ftyp_size.to_be_bytes());
        data.extend_from_slice(b"ftyp");
        data.extend_from_slice(b"isom\x00\x00\x00\x00isom");
        // moov with mvhd only (no trak)
        let mvhd_payload = vec![0u8; 12];
        let mvhd_size = (8 + mvhd_payload.len()) as u32;
        let moov_inner_size = 8 + mvhd_payload.len();
        let moov_size = (8 + moov_inner_size) as u32;
        data.extend_from_slice(&moov_size.to_be_bytes());
        data.extend_from_slice(b"moov");
        data.extend_from_slice(&mvhd_size.to_be_bytes());
        data.extend_from_slice(b"mvhd");
        data.extend_from_slice(&mvhd_payload);

        let r = validate_init_segment(&data);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("no trak")));
    }

    // --- Media segment validation ---

    #[test]
    fn validate_media_segment_too_small() {
        let r = validate_media_segment(&[0; 4], false);
        assert!(!r.valid);
    }

    #[test]
    fn validate_media_segment_missing_moof() {
        // Just an mdat
        let mut data = Vec::new();
        let payload = vec![0u8; 16];
        let size = (8 + payload.len()) as u32;
        data.extend_from_slice(&size.to_be_bytes());
        data.extend_from_slice(b"mdat");
        data.extend_from_slice(&payload);

        let r = validate_media_segment(&data, false);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("moof")));
    }

    #[test]
    fn validate_media_segment_missing_mdat() {
        // moof with traf→trun inside
        let mut trun = Vec::new();
        let trun_size: u32 = 20;
        trun.extend_from_slice(&trun_size.to_be_bytes());
        trun.extend_from_slice(b"trun");
        trun.extend_from_slice(&[0u8; 12]);

        let mut traf = Vec::new();
        let traf_size = (8 + trun.len()) as u32;
        traf.extend_from_slice(&traf_size.to_be_bytes());
        traf.extend_from_slice(b"traf");
        traf.extend_from_slice(&trun);

        let mut moof = Vec::new();
        let moof_size = (8 + traf.len()) as u32;
        moof.extend_from_slice(&moof_size.to_be_bytes());
        moof.extend_from_slice(b"moof");
        moof.extend_from_slice(&traf);

        let r = validate_media_segment(&moof, false);
        assert!(!r.valid);
        assert!(r.errors.iter().any(|e| e.contains("mdat")));
    }

    // --- TS container format validation ---

    #[cfg(feature = "ts")]
    mod ts_compat_tests {
        use super::*;
        use crate::manifest::types::OutputFormat;

        #[test]
        fn validate_ts_with_dash_error() {
            let r = validate_container_output_formats(
                ContainerFormat::Ts,
                &[OutputFormat::Dash],
            );
            assert!(!r.valid);
            assert!(r.errors[0].contains("TS container format is not supported with DASH"));
        }

        #[test]
        fn validate_ts_with_hls_ok() {
            let r = validate_container_output_formats(
                ContainerFormat::Ts,
                &[OutputFormat::Hls],
            );
            assert!(r.valid);
        }

        #[test]
        fn validate_ts_with_dual_format_error() {
            let r = validate_container_output_formats(
                ContainerFormat::Ts,
                &[OutputFormat::Hls, OutputFormat::Dash],
            );
            assert!(!r.valid);
        }

        #[test]
        fn validate_cmaf_with_dash_ok() {
            let r = validate_container_output_formats(
                ContainerFormat::Cmaf,
                &[OutputFormat::Dash],
            );
            assert!(r.valid);
        }
    }
}
