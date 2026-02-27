pub mod cbcs;
pub mod cenc;
pub mod cpix;
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

    pub fn system_id_name(id: &[u8; 16]) -> &'static str {
        if id == &WIDEVINE {
            "Widevine"
        } else if id == &PLAYREADY {
            "PlayReady"
        } else if id == &FAIRPLAY {
            "FairPlay"
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
