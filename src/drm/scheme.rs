use serde::{Deserialize, Serialize};

/// Encryption scheme for media content.
///
/// Represents the two Common Encryption (CENC) protection schemes defined in
/// ISO/IEC 23001-7. The edgepack can accept either scheme as input and
/// produce either (or both) as output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EncryptionScheme {
    /// CBCS: AES-128-CBC with pattern encryption.
    /// Used by FairPlay and optionally by Widevine/PlayReady.
    /// Video uses pattern 1:9 (encrypt 1 block, skip 9). Audio uses 0:0 (encrypt all).
    Cbcs,
    /// CENC: AES-128-CTR full encryption.
    /// Used by Widevine and PlayReady. No pattern — all bytes encrypted.
    Cenc,
    /// None: unencrypted/clear content. No encryption or decryption is applied.
    None,
}

impl EncryptionScheme {
    /// Whether this scheme involves encryption (as opposed to clear/unencrypted content).
    pub fn is_encrypted(&self) -> bool {
        !matches!(self, EncryptionScheme::None)
    }

    /// Returns the 4-byte scheme type code used in ISOBMFF schm boxes.
    ///
    /// Callers must check `is_encrypted()` before using this value — `None` returns
    /// zeroed bytes as a defensive fallback since clear content has no schm box.
    pub fn scheme_type_bytes(&self) -> [u8; 4] {
        match self {
            EncryptionScheme::Cbcs => *b"cbcs",
            EncryptionScheme::Cenc => *b"cenc",
            EncryptionScheme::None => [0u8; 4],
        }
    }

    /// Parse an encryption scheme from a 4-byte scheme type code (from schm box).
    /// Returns None if the bytes don't match a known scheme.
    pub fn from_scheme_type(bytes: &[u8; 4]) -> Option<Self> {
        match bytes {
            b"cbcs" => Some(EncryptionScheme::Cbcs),
            b"cenc" => Some(EncryptionScheme::Cenc),
            _ => None,
        }
    }

    /// Parse an encryption scheme from a string value ("cenc", "cbcs", "none").
    pub fn from_str_value(s: &str) -> Option<Self> {
        match s {
            "cenc" => Some(EncryptionScheme::Cenc),
            "cbcs" => Some(EncryptionScheme::Cbcs),
            "none" => Some(EncryptionScheme::None),
            _ => None,
        }
    }

    /// Returns the scheme type as a string (for manifests and logging).
    pub fn scheme_type_str(&self) -> &'static str {
        match self {
            EncryptionScheme::Cbcs => "cbcs",
            EncryptionScheme::Cenc => "cenc",
            EncryptionScheme::None => "none",
        }
    }

    /// Returns the HLS EXT-X-KEY METHOD value for this scheme.
    ///
    /// - CBCS: `SAMPLE-AES` (AES-128-CBC pattern encryption)
    /// - CENC: `SAMPLE-AES-CTR` (AES-128-CTR full encryption)
    /// - None: `NONE` (defensive; never rendered since drm_info will be None for clear output)
    pub fn hls_method_string(&self) -> &'static str {
        match self {
            EncryptionScheme::Cbcs => "SAMPLE-AES",
            EncryptionScheme::Cenc => "SAMPLE-AES-CTR",
            EncryptionScheme::None => "NONE",
        }
    }

    /// Returns the default per-sample IV size in bytes for this scheme.
    ///
    /// - CBCS: 16 bytes (CBC requires 16-byte IVs)
    /// - CENC: 8 bytes (CTR counter block, upper 8 bytes of 16-byte nonce)
    /// - None: 0 (clear content has no IVs)
    pub fn default_iv_size(&self) -> u8 {
        match self {
            EncryptionScheme::Cbcs => 16,
            EncryptionScheme::Cenc => 8,
            EncryptionScheme::None => 0,
        }
    }

    /// Returns the default encryption pattern `(crypt_byte_block, skip_byte_block)` for video.
    ///
    /// - CBCS: (1, 9) — encrypt 1 of every 10 blocks
    /// - CENC: (0, 0) — no pattern, full encryption
    /// - None: (0, 0) — no pattern (no encryption)
    pub fn default_video_pattern(&self) -> (u8, u8) {
        match self {
            EncryptionScheme::Cbcs => (1, 9),
            EncryptionScheme::Cenc | EncryptionScheme::None => (0, 0),
        }
    }

    /// Returns the default encryption pattern for audio.
    ///
    /// Both schemes fully encrypt audio: (0, 0) means encrypt all complete blocks.
    pub fn default_audio_pattern(&self) -> (u8, u8) {
        (0, 0)
    }

    /// Whether this scheme uses pattern encryption (as opposed to full encryption).
    pub fn uses_pattern(&self) -> bool {
        matches!(self, EncryptionScheme::Cbcs)
    }

    /// Whether FairPlay DRM is applicable for this scheme.
    /// FairPlay only works with CBCS.
    pub fn supports_fairplay(&self) -> bool {
        matches!(self, EncryptionScheme::Cbcs)
    }
}

impl std::fmt::Display for EncryptionScheme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.scheme_type_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheme_type_bytes_cbcs() {
        assert_eq!(EncryptionScheme::Cbcs.scheme_type_bytes(), *b"cbcs");
    }

    #[test]
    fn scheme_type_bytes_cenc() {
        assert_eq!(EncryptionScheme::Cenc.scheme_type_bytes(), *b"cenc");
    }

    #[test]
    fn from_scheme_type_cbcs() {
        assert_eq!(
            EncryptionScheme::from_scheme_type(b"cbcs"),
            Some(EncryptionScheme::Cbcs)
        );
    }

    #[test]
    fn from_scheme_type_cenc() {
        assert_eq!(
            EncryptionScheme::from_scheme_type(b"cenc"),
            Some(EncryptionScheme::Cenc)
        );
    }

    #[test]
    fn from_scheme_type_unknown() {
        assert_eq!(EncryptionScheme::from_scheme_type(b"abcd"), None);
        assert_eq!(EncryptionScheme::from_scheme_type(b"\0\0\0\0"), None);
    }

    #[test]
    fn scheme_type_str() {
        assert_eq!(EncryptionScheme::Cbcs.scheme_type_str(), "cbcs");
        assert_eq!(EncryptionScheme::Cenc.scheme_type_str(), "cenc");
    }

    #[test]
    fn hls_method_string_cbcs() {
        assert_eq!(EncryptionScheme::Cbcs.hls_method_string(), "SAMPLE-AES");
    }

    #[test]
    fn hls_method_string_cenc() {
        assert_eq!(EncryptionScheme::Cenc.hls_method_string(), "SAMPLE-AES-CTR");
    }

    #[test]
    fn default_iv_size_cbcs() {
        assert_eq!(EncryptionScheme::Cbcs.default_iv_size(), 16);
    }

    #[test]
    fn default_iv_size_cenc() {
        assert_eq!(EncryptionScheme::Cenc.default_iv_size(), 8);
    }

    #[test]
    fn default_video_pattern_cbcs() {
        assert_eq!(EncryptionScheme::Cbcs.default_video_pattern(), (1, 9));
    }

    #[test]
    fn default_video_pattern_cenc() {
        assert_eq!(EncryptionScheme::Cenc.default_video_pattern(), (0, 0));
    }

    #[test]
    fn default_audio_pattern_both() {
        assert_eq!(EncryptionScheme::Cbcs.default_audio_pattern(), (0, 0));
        assert_eq!(EncryptionScheme::Cenc.default_audio_pattern(), (0, 0));
    }

    #[test]
    fn uses_pattern() {
        assert!(EncryptionScheme::Cbcs.uses_pattern());
        assert!(!EncryptionScheme::Cenc.uses_pattern());
    }

    #[test]
    fn supports_fairplay() {
        assert!(EncryptionScheme::Cbcs.supports_fairplay());
        assert!(!EncryptionScheme::Cenc.supports_fairplay());
    }

    #[test]
    fn display_impl() {
        assert_eq!(format!("{}", EncryptionScheme::Cbcs), "cbcs");
        assert_eq!(format!("{}", EncryptionScheme::Cenc), "cenc");
    }

    #[test]
    fn serde_roundtrip_cbcs() {
        let scheme = EncryptionScheme::Cbcs;
        let json = serde_json::to_string(&scheme).unwrap();
        let parsed: EncryptionScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, scheme);
    }

    #[test]
    fn serde_roundtrip_cenc() {
        let scheme = EncryptionScheme::Cenc;
        let json = serde_json::to_string(&scheme).unwrap();
        let parsed: EncryptionScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, scheme);
    }

    #[test]
    fn equality_and_hash() {
        use std::collections::HashSet;
        let mut set = HashSet::new();
        set.insert(EncryptionScheme::Cbcs);
        set.insert(EncryptionScheme::Cenc);
        set.insert(EncryptionScheme::None);
        set.insert(EncryptionScheme::Cbcs); // duplicate
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn roundtrip_bytes() {
        for scheme in [EncryptionScheme::Cbcs, EncryptionScheme::Cenc] {
            let bytes = scheme.scheme_type_bytes();
            let parsed = EncryptionScheme::from_scheme_type(&bytes).unwrap();
            assert_eq!(parsed, scheme);
        }
    }

    // --- EncryptionScheme::None tests ---

    #[test]
    fn is_encrypted() {
        assert!(EncryptionScheme::Cbcs.is_encrypted());
        assert!(EncryptionScheme::Cenc.is_encrypted());
        assert!(!EncryptionScheme::None.is_encrypted());
    }

    #[test]
    fn scheme_type_bytes_none() {
        assert_eq!(EncryptionScheme::None.scheme_type_bytes(), [0u8; 4]);
    }

    #[test]
    fn scheme_type_str_none() {
        assert_eq!(EncryptionScheme::None.scheme_type_str(), "none");
    }

    #[test]
    fn hls_method_string_none() {
        assert_eq!(EncryptionScheme::None.hls_method_string(), "NONE");
    }

    #[test]
    fn display_impl_none() {
        assert_eq!(format!("{}", EncryptionScheme::None), "none");
    }

    #[test]
    fn default_iv_size_none() {
        assert_eq!(EncryptionScheme::None.default_iv_size(), 0);
    }

    #[test]
    fn default_video_pattern_none() {
        assert_eq!(EncryptionScheme::None.default_video_pattern(), (0, 0));
    }

    #[test]
    fn default_audio_pattern_none() {
        assert_eq!(EncryptionScheme::None.default_audio_pattern(), (0, 0));
    }

    #[test]
    fn uses_pattern_none() {
        assert!(!EncryptionScheme::None.uses_pattern());
    }

    #[test]
    fn supports_fairplay_none() {
        assert!(!EncryptionScheme::None.supports_fairplay());
    }

    #[test]
    fn serde_roundtrip_none() {
        let scheme = EncryptionScheme::None;
        let json = serde_json::to_string(&scheme).unwrap();
        let parsed: EncryptionScheme = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, scheme);
    }

    #[test]
    fn from_str_value_valid() {
        assert_eq!(EncryptionScheme::from_str_value("cenc"), Some(EncryptionScheme::Cenc));
        assert_eq!(EncryptionScheme::from_str_value("cbcs"), Some(EncryptionScheme::Cbcs));
        assert_eq!(EncryptionScheme::from_str_value("none"), Some(EncryptionScheme::None));
    }

    #[test]
    fn from_str_value_invalid() {
        assert_eq!(EncryptionScheme::from_str_value("aes256"), Option::None);
        assert_eq!(EncryptionScheme::from_str_value(""), Option::None);
        assert_eq!(EncryptionScheme::from_str_value("CENC"), Option::None);
    }

    #[test]
    fn from_str_value_roundtrip() {
        for scheme in [EncryptionScheme::Cenc, EncryptionScheme::Cbcs, EncryptionScheme::None] {
            let s = scheme.scheme_type_str();
            assert_eq!(EncryptionScheme::from_str_value(s), Some(scheme));
        }
    }

    #[test]
    fn from_scheme_type_does_not_return_none() {
        // None is never parsed from bytes — clear content has no schm box
        assert_eq!(EncryptionScheme::from_scheme_type(&[0u8; 4]), Option::None);
    }
}
