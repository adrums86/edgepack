pub mod cbcs;
pub mod cenc;
pub mod cpix;
pub mod sample_cryptor;
pub mod scheme;
pub mod speke;

/// Well-known DRM system IDs (UUIDs).
pub mod system_ids {
    /// Widevine: edef8ba9-79d6-4ace-a3c8-27dcd51d21ed
    pub const WIDEVINE: [u8; 16] = [
        0xed, 0xef, 0x8b, 0xa9, 0x79, 0xd6, 0x4a, 0xce, 0xa3, 0xc8, 0x27, 0xdc, 0xd5, 0x1d,
        0x21, 0xed,
    ];

    /// PlayReady: 9a04f079-9840-4286-ab92-e65be0885f95
    pub const PLAYREADY: [u8; 16] = [
        0x9a, 0x04, 0xf0, 0x79, 0x98, 0x40, 0x42, 0x86, 0xab, 0x92, 0xe6, 0x5b, 0xe0, 0x88,
        0x5f, 0x95,
    ];

    /// FairPlay: 94ce86fb-07ff-4f43-adb8-93d2fa968ca2
    pub const FAIRPLAY: [u8; 16] = [
        0x94, 0xce, 0x86, 0xfb, 0x07, 0xff, 0x4f, 0x43, 0xad, 0xb8, 0x93, 0xd2, 0xfa, 0x96,
        0x8c, 0xa2,
    ];

    /// ClearKey: e2719d58-a985-b3c9-781a-b030af78d30e
    /// W3C EME ClearKey system (keys delivered in-band via JSON).
    pub const CLEARKEY: [u8; 16] = [
        0xe2, 0x71, 0x9d, 0x58, 0xa9, 0x85, 0xb3, 0xc9, 0x78, 0x1a, 0xb0, 0x30, 0xaf, 0x78,
        0xd3, 0x0e,
    ];

    pub fn system_id_name(id: &[u8; 16]) -> &'static str {
        if id == &WIDEVINE {
            "Widevine"
        } else if id == &PLAYREADY {
            "PlayReady"
        } else if id == &FAIRPLAY {
            "FairPlay"
        } else if id == &CLEARKEY {
            "ClearKey"
        } else {
            "Unknown"
        }
    }
}

/// Represents a content encryption key with its associated metadata.
#[derive(Debug, Clone)]
pub struct ContentKey {
    /// Key ID (KID) — 16-byte UUID identifying this key.
    pub kid: [u8; 16],
    /// The actual content encryption key (CEK), typically 16 bytes (AES-128).
    pub key: Vec<u8>,
    /// Optional IV (initialization vector). If None, IVs come from senc boxes.
    pub iv: Option<Vec<u8>>,
}

/// DRM-specific data for a particular DRM system (Widevine, PlayReady, etc.).
#[derive(Debug, Clone)]
pub struct DrmSystemData {
    /// DRM system UUID.
    pub system_id: [u8; 16],
    /// Key ID this data applies to.
    pub kid: [u8; 16],
    /// PSSH box data (the inner data, not the full box).
    pub pssh_data: Vec<u8>,
    /// Content protection data (for manifest signaling).
    pub content_protection_data: Option<String>,
}

/// The complete set of keys and DRM data needed for repackaging.
#[derive(Debug, Clone)]
pub struct DrmKeySet {
    /// Content keys indexed by usage (video, audio, etc.).
    pub keys: Vec<ContentKey>,
    /// DRM system-specific data for each system and key.
    pub drm_systems: Vec<DrmSystemData>,
}

/// Build the PSSH data payload for ClearKey DRM.
///
/// ClearKey PSSH contains a JSON object with base64url-encoded KIDs:
/// `{"kids":["base64url-kid-1","base64url-kid-2"]}`
///
/// This is the inner PSSH data (not the full PSSH box).
pub fn build_clearkey_pssh_data(kids: &[[u8; 16]]) -> Vec<u8> {
    let encoded_kids: Vec<String> = kids.iter().map(|kid| base64url_encode(kid)).collect();
    let json = format!(
        r#"{{"kids":[{}]}}"#,
        encoded_kids
            .iter()
            .map(|k| format!("\"{}\"", k))
            .collect::<Vec<_>>()
            .join(",")
    );
    json.into_bytes()
}

/// Base64url encoding (no padding) per RFC 4648 §5.
fn base64url_encode(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::system_ids::*;

    #[test]
    fn widevine_system_id_correct() {
        assert_eq!(WIDEVINE[0], 0xed);
        assert_eq!(WIDEVINE[15], 0xed);
        assert_eq!(WIDEVINE.len(), 16);
    }

    #[test]
    fn playready_system_id_correct() {
        assert_eq!(PLAYREADY[0], 0x9a);
        assert_eq!(PLAYREADY[15], 0x95);
        assert_eq!(PLAYREADY.len(), 16);
    }

    #[test]
    fn fairplay_system_id_correct() {
        assert_eq!(FAIRPLAY[0], 0x94);
        assert_eq!(FAIRPLAY[15], 0xa2);
        assert_eq!(FAIRPLAY.len(), 16);
    }

    #[test]
    fn system_id_name_widevine() {
        assert_eq!(system_id_name(&WIDEVINE), "Widevine");
    }

    #[test]
    fn system_id_name_playready() {
        assert_eq!(system_id_name(&PLAYREADY), "PlayReady");
    }

    #[test]
    fn system_id_name_fairplay() {
        assert_eq!(system_id_name(&FAIRPLAY), "FairPlay");
    }

    #[test]
    fn system_id_name_unknown() {
        assert_eq!(system_id_name(&[0u8; 16]), "Unknown");
    }

    #[test]
    fn clearkey_system_id_correct() {
        assert_eq!(CLEARKEY[0], 0xe2);
        assert_eq!(CLEARKEY[15], 0x0e);
        assert_eq!(CLEARKEY.len(), 16);
    }

    #[test]
    fn system_id_name_clearkey() {
        assert_eq!(system_id_name(&CLEARKEY), "ClearKey");
    }

    #[test]
    fn all_system_ids_are_distinct() {
        assert_ne!(WIDEVINE, PLAYREADY);
        assert_ne!(WIDEVINE, FAIRPLAY);
        assert_ne!(WIDEVINE, CLEARKEY);
        assert_ne!(PLAYREADY, FAIRPLAY);
        assert_ne!(PLAYREADY, CLEARKEY);
        assert_ne!(FAIRPLAY, CLEARKEY);
    }

    #[test]
    fn content_key_construction() {
        let key = super::ContentKey {
            kid: [1u8; 16],
            key: vec![2u8; 16],
            iv: Some(vec![3u8; 16]),
        };
        assert_eq!(key.kid, [1u8; 16]);
        assert_eq!(key.key.len(), 16);
        assert!(key.iv.is_some());
    }

    #[test]
    fn content_key_without_iv() {
        let key = super::ContentKey {
            kid: [0u8; 16],
            key: vec![0u8; 16],
            iv: None,
        };
        assert!(key.iv.is_none());
    }

    #[test]
    fn drm_key_set_construction() {
        let ks = super::DrmKeySet {
            keys: vec![super::ContentKey {
                kid: [1u8; 16],
                key: vec![2u8; 16],
                iv: None,
            }],
            drm_systems: vec![super::DrmSystemData {
                system_id: WIDEVINE,
                kid: [1u8; 16],
                pssh_data: vec![0xDE, 0xAD],
                content_protection_data: None,
            }],
        };
        assert_eq!(ks.keys.len(), 1);
        assert_eq!(ks.drm_systems.len(), 1);
    }

    #[test]
    fn clearkey_pssh_data_single_kid() {
        use base64::Engine;
        let kid = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08,
                   0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f, 0x10];
        let data = super::build_clearkey_pssh_data(&[kid]);
        let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
        let kids = json["kids"].as_array().unwrap();
        assert_eq!(kids.len(), 1);
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(kids[0].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, kid);
    }

    #[test]
    fn clearkey_pssh_data_multi_kid() {
        use base64::Engine;
        let kid1 = [0xAA; 16];
        let kid2 = [0xBB; 16];
        let data = super::build_clearkey_pssh_data(&[kid1, kid2]);
        let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
        let kids = json["kids"].as_array().unwrap();
        assert_eq!(kids.len(), 2);
        for (i, expected) in [kid1, kid2].iter().enumerate() {
            let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(kids[i].as_str().unwrap())
                .unwrap();
            assert_eq!(decoded.as_slice(), expected);
        }
    }

    #[test]
    fn clearkey_pssh_data_is_valid_json() {
        let kid = [0x00; 16];
        let data = super::build_clearkey_pssh_data(&[kid]);
        let json_str = std::str::from_utf8(&data).unwrap();
        assert!(json_str.starts_with('{'));
        assert!(json_str.ends_with('}'));
        assert!(json_str.contains("\"kids\""));
        assert!(!json_str.contains('='));
    }

    #[test]
    fn clearkey_pssh_data_empty_kids() {
        let data = super::build_clearkey_pssh_data(&[]);
        let json: serde_json::Value = serde_json::from_slice(&data).unwrap();
        let kids = json["kids"].as_array().unwrap();
        assert!(kids.is_empty());
    }
}
