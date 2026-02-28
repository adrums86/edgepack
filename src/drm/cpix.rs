use crate::drm::{ContentKey, DrmKeySet, DrmSystemData};
use crate::error::{EdgepackError, Result};
use base64::Engine;
use quick_xml::events::Event;
use quick_xml::Reader;

const CPIX_NS: &str = "urn:dashif:org:cpix";
const PSKC_NS: &str = "urn:ietf:params:xml:ns:keyprov:pskc";

/// Build a CPIX request document for SPEKE 2.0.
///
/// This creates a CPIX document requesting content keys for the given content ID,
/// with the specified DRM system IDs and key IDs.
pub fn build_cpix_request(
    content_id: &str,
    key_ids: &[[u8; 16]],
    system_ids: &[[u8; 16]],
) -> Result<String> {
    let b64 = &base64::engine::general_purpose::STANDARD;
    let mut xml = String::new();
    xml.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    xml.push_str(&format!(
        "<cpix:CPIX xmlns:cpix=\"{CPIX_NS}\" xmlns:pskc=\"{PSKC_NS}\" "
    ));
    xml.push_str(&format!(
        "id=\"{}\" contentId=\"{content_id}\">\n",
        uuid::Uuid::new_v4()
    ));

    // ContentKeyList — request keys for each KID
    xml.push_str("  <cpix:ContentKeyList>\n");
    for kid in key_ids {
        let kid_uuid = format_uuid(kid);
        xml.push_str(&format!(
            "    <cpix:ContentKey kid=\"{kid_uuid}\" commonEncryptionScheme=\"cenc\"/>\n"
        ));
    }
    xml.push_str("  </cpix:ContentKeyList>\n");

    // DRMSystemList — request data for each DRM system and key
    xml.push_str("  <cpix:DRMSystemList>\n");
    for system_id in system_ids {
        let sys_uuid = format_uuid(system_id);
        for kid in key_ids {
            let kid_uuid = format_uuid(kid);
            xml.push_str(&format!(
                "    <cpix:DRMSystem kid=\"{kid_uuid}\" systemId=\"{sys_uuid}\">\n"
            ));
            xml.push_str("      <cpix:PSSH/>\n");
            xml.push_str("      <cpix:ContentProtectionData/>\n");
            xml.push_str("    </cpix:DRMSystem>\n");
        }
    }
    xml.push_str("  </cpix:DRMSystemList>\n");

    // ContentKeyUsageRuleList
    xml.push_str("  <cpix:ContentKeyUsageRuleList>\n");
    for (i, kid) in key_ids.iter().enumerate() {
        let kid_uuid = format_uuid(kid);
        let intent = if i == 0 { "VIDEO" } else { "AUDIO" };
        xml.push_str(&format!(
            "    <cpix:ContentKeyUsageRule kid=\"{kid_uuid}\" intendedTrackType=\"{intent}\"/>\n"
        ));
    }
    xml.push_str("  </cpix:ContentKeyUsageRuleList>\n");

    xml.push_str("</cpix:CPIX>\n");
    let _ = b64; // suppress unused warning
    Ok(xml)
}

/// Parse a CPIX response document and extract content keys and DRM system data.
pub fn parse_cpix_response(xml_data: &[u8]) -> Result<DrmKeySet> {
    let b64 = &base64::engine::general_purpose::STANDARD;
    let mut reader = Reader::from_reader(xml_data);
    reader.config_mut().trim_text(true);

    let mut keys: Vec<ContentKey> = Vec::new();
    let mut drm_systems: Vec<DrmSystemData> = Vec::new();

    let mut buf = Vec::new();
    let mut current_kid: Option<[u8; 16]> = None;
    let mut current_system_id: Option<[u8; 16]> = None;
    let mut in_secret_value = false;
    let mut in_pssh = false;
    let mut in_cp_data = false;

    loop {
        match reader.read_event_into(&mut buf) {
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let local_name = e.local_name();
                let name = std::str::from_utf8(local_name.as_ref()).unwrap_or("");

                match name {
                    "ContentKey" => {
                        if let Some(kid_attr) = find_attribute(e, b"kid") {
                            current_kid = Some(parse_uuid(&kid_attr)?);
                        }
                    }
                    "Secret" | "PlainValue" => {
                        // Inside pskc:Secret/pskc:PlainValue — the actual key value
                        if name == "PlainValue" {
                            in_secret_value = true;
                        }
                    }
                    "DRMSystem" => {
                        if let Some(kid_attr) = find_attribute(e, b"kid") {
                            current_kid = Some(parse_uuid(&kid_attr)?);
                        }
                        if let Some(sys_attr) = find_attribute(e, b"systemId") {
                            current_system_id = Some(parse_uuid(&sys_attr)?);
                        }
                    }
                    "PSSH" => {
                        in_pssh = true;
                    }
                    "ContentProtectionData" => {
                        in_cp_data = true;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                let text = e.unescape().unwrap_or_default().to_string();
                let text = text.trim();
                if text.is_empty() {
                    continue;
                }

                if in_secret_value {
                    if let Some(kid) = current_kid {
                        let key_data = b64
                            .decode(text)
                            .map_err(|e| EdgepackError::Cpix(format!("invalid base64 key: {e}")))?;
                        keys.push(ContentKey {
                            kid,
                            key: key_data,
                            iv: None,
                        });
                    }
                } else if in_pssh {
                    if let (Some(kid), Some(system_id)) = (current_kid, current_system_id) {
                        let pssh_data = b64.decode(text).map_err(|e| {
                            EdgepackError::Cpix(format!("invalid base64 PSSH: {e}"))
                        })?;
                        // Check if we already have an entry for this system_id+kid
                        if let Some(existing) = drm_systems
                            .iter_mut()
                            .find(|d| d.system_id == system_id && d.kid == kid)
                        {
                            existing.pssh_data = pssh_data;
                        } else {
                            drm_systems.push(DrmSystemData {
                                system_id,
                                kid,
                                pssh_data,
                                content_protection_data: None,
                            });
                        }
                    }
                } else if in_cp_data {
                    if let (Some(kid), Some(system_id)) = (current_kid, current_system_id) {
                        if let Some(existing) = drm_systems
                            .iter_mut()
                            .find(|d| d.system_id == system_id && d.kid == kid)
                        {
                            existing.content_protection_data = Some(text.to_string());
                        }
                    }
                }
            }
            Ok(Event::End(ref e)) => {
                let local_name = e.local_name();
                let name = std::str::from_utf8(local_name.as_ref()).unwrap_or("");
                match name {
                    "PlainValue" => in_secret_value = false,
                    "PSSH" => in_pssh = false,
                    "ContentProtectionData" => in_cp_data = false,
                    "ContentKey" => current_kid = None,
                    "DRMSystem" => {
                        current_kid = None;
                        current_system_id = None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(EdgepackError::Cpix(format!("XML parse error: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    if keys.is_empty() {
        return Err(EdgepackError::Cpix(
            "no content keys found in CPIX response".into(),
        ));
    }

    Ok(DrmKeySet { keys, drm_systems })
}

/// Format a 16-byte UUID as a standard hyphenated string.
pub fn format_uuid(bytes: &[u8; 16]) -> String {
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3],
        bytes[4], bytes[5],
        bytes[6], bytes[7],
        bytes[8], bytes[9],
        bytes[10], bytes[11], bytes[12], bytes[13], bytes[14], bytes[15]
    )
}

/// Parse a UUID string (with or without hyphens) into a 16-byte array.
pub fn parse_uuid(s: &str) -> Result<[u8; 16]> {
    let hex: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if hex.len() != 32 {
        return Err(EdgepackError::Cpix(format!(
            "invalid UUID: expected 32 hex chars, got {}",
            hex.len()
        )));
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| EdgepackError::Cpix(format!("invalid UUID hex: {e}")))?;
    }
    Ok(bytes)
}

fn find_attribute(
    e: &quick_xml::events::BytesStart,
    name: &[u8],
) -> Option<String> {
    e.attributes().flatten().find_map(|attr| {
        if attr.key.as_ref() == name {
            String::from_utf8(attr.value.to_vec()).ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drm::system_ids;

    #[test]
    fn format_uuid_zero() {
        let bytes = [0u8; 16];
        assert_eq!(
            format_uuid(&bytes),
            "00000000-0000-0000-0000-000000000000"
        );
    }

    #[test]
    fn format_uuid_widevine() {
        let uuid = format_uuid(&system_ids::WIDEVINE);
        assert_eq!(uuid, "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed");
    }

    #[test]
    fn format_uuid_playready() {
        let uuid = format_uuid(&system_ids::PLAYREADY);
        assert_eq!(uuid, "9a04f079-9840-4286-ab92-e65be0885f95");
    }

    #[test]
    fn parse_uuid_with_hyphens() {
        let bytes = parse_uuid("edef8ba9-79d6-4ace-a3c8-27dcd51d21ed").unwrap();
        assert_eq!(bytes, system_ids::WIDEVINE);
    }

    #[test]
    fn parse_uuid_without_hyphens() {
        let bytes = parse_uuid("edef8ba979d64acea3c827dcd51d21ed").unwrap();
        assert_eq!(bytes, system_ids::WIDEVINE);
    }

    #[test]
    fn parse_uuid_roundtrip() {
        let original = system_ids::PLAYREADY;
        let formatted = format_uuid(&original);
        let parsed = parse_uuid(&formatted).unwrap();
        assert_eq!(parsed, original);
    }

    #[test]
    fn parse_uuid_too_short() {
        let result = parse_uuid("abcdef");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("expected 32 hex chars"));
    }

    #[test]
    fn parse_uuid_too_long() {
        let result = parse_uuid("edef8ba979d64acea3c827dcd51d21edFF");
        assert!(result.is_err());
    }

    #[test]
    fn build_cpix_request_single_key_single_system() {
        let kid = [0x01u8; 16];
        let system = system_ids::WIDEVINE;
        let xml = build_cpix_request("test-content", &[kid], &[system]).unwrap();

        assert!(xml.contains("<?xml version="));
        assert!(xml.contains("urn:dashif:org:cpix"));
        assert!(xml.contains("contentId=\"test-content\""));
        assert!(xml.contains("ContentKeyList"));
        assert!(xml.contains("ContentKey"));
        assert!(xml.contains("commonEncryptionScheme=\"cenc\""));
        assert!(xml.contains("DRMSystemList"));
        assert!(xml.contains("DRMSystem"));
        assert!(xml.contains("PSSH"));
        assert!(xml.contains("ContentKeyUsageRuleList"));
        assert!(xml.contains("intendedTrackType=\"VIDEO\""));
    }

    #[test]
    fn build_cpix_request_multiple_keys() {
        let kid1 = [0x01u8; 16];
        let kid2 = [0x02u8; 16];
        let xml = build_cpix_request(
            "test",
            &[kid1, kid2],
            &[system_ids::WIDEVINE],
        )
        .unwrap();

        // First key should be VIDEO, second AUDIO
        assert!(xml.contains("intendedTrackType=\"VIDEO\""));
        assert!(xml.contains("intendedTrackType=\"AUDIO\""));
    }

    #[test]
    fn build_cpix_request_multiple_systems() {
        let kid = [0x01u8; 16];
        let xml = build_cpix_request(
            "test",
            &[kid],
            &[system_ids::WIDEVINE, system_ids::PLAYREADY],
        )
        .unwrap();

        let wv_uuid = format_uuid(&system_ids::WIDEVINE);
        let pr_uuid = format_uuid(&system_ids::PLAYREADY);
        assert!(xml.contains(&wv_uuid));
        assert!(xml.contains(&pr_uuid));
    }

    #[test]
    fn build_cpix_request_is_valid_xml() {
        let kid = [0x01u8; 16];
        let xml = build_cpix_request("test", &[kid], &[system_ids::WIDEVINE]).unwrap();
        // Verify it can be parsed as XML
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buf = Vec::new();
        loop {
            match reader.read_event_into(&mut buf) {
                Ok(quick_xml::events::Event::Eof) => break,
                Err(e) => panic!("Invalid XML: {e}"),
                _ => {}
            }
            buf.clear();
        }
    }

    fn build_sample_cpix_response() -> String {
        let b64 = &base64::engine::general_purpose::STANDARD;
        let key_b64 = b64.encode([0xAA; 16]);
        let pssh_b64 = b64.encode([0xBB; 32]);
        let kid_uuid = format_uuid(&[0x01; 16]);
        let wv_uuid = format_uuid(&system_ids::WIDEVINE);

        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<cpix:CPIX xmlns:cpix="urn:dashif:org:cpix" xmlns:pskc="urn:ietf:params:xml:ns:keyprov:pskc">
  <cpix:ContentKeyList>
    <cpix:ContentKey kid="{kid_uuid}">
      <cpix:Data>
        <pskc:Secret>
          <pskc:PlainValue>{key_b64}</pskc:PlainValue>
        </pskc:Secret>
      </cpix:Data>
    </cpix:ContentKey>
  </cpix:ContentKeyList>
  <cpix:DRMSystemList>
    <cpix:DRMSystem kid="{kid_uuid}" systemId="{wv_uuid}">
      <cpix:PSSH>{pssh_b64}</cpix:PSSH>
    </cpix:DRMSystem>
  </cpix:DRMSystemList>
</cpix:CPIX>"#
        )
    }

    #[test]
    fn parse_cpix_response_extracts_key() {
        let xml = build_sample_cpix_response();
        let result = parse_cpix_response(xml.as_bytes()).unwrap();

        assert_eq!(result.keys.len(), 1);
        assert_eq!(result.keys[0].kid, [0x01; 16]);
        assert_eq!(result.keys[0].key, vec![0xAA; 16]);
        assert!(result.keys[0].iv.is_none());
    }

    #[test]
    fn parse_cpix_response_extracts_drm_system() {
        let xml = build_sample_cpix_response();
        let result = parse_cpix_response(xml.as_bytes()).unwrap();

        assert_eq!(result.drm_systems.len(), 1);
        assert_eq!(result.drm_systems[0].system_id, system_ids::WIDEVINE);
        assert_eq!(result.drm_systems[0].kid, [0x01; 16]);
        assert_eq!(result.drm_systems[0].pssh_data, vec![0xBB; 32]);
    }

    #[test]
    fn parse_cpix_response_no_keys_errors() {
        let xml = r#"<?xml version="1.0"?><cpix:CPIX xmlns:cpix="urn:dashif:org:cpix"></cpix:CPIX>"#;
        let result = parse_cpix_response(xml.as_bytes());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no content keys"));
    }

    #[test]
    fn parse_cpix_response_invalid_xml_errors() {
        let result = parse_cpix_response(b"this is not xml < >");
        // Should either parse with no keys (and error) or fail with XML error
        assert!(result.is_err());
    }

    #[test]
    fn parse_cpix_response_multiple_keys() {
        let b64 = &base64::engine::general_purpose::STANDARD;
        let key1_b64 = b64.encode([0xAA; 16]);
        let key2_b64 = b64.encode([0xCC; 16]);
        let kid1 = format_uuid(&[0x01; 16]);
        let kid2 = format_uuid(&[0x02; 16]);

        let xml = format!(
            r#"<?xml version="1.0"?>
<cpix:CPIX xmlns:cpix="urn:dashif:org:cpix" xmlns:pskc="urn:ietf:params:xml:ns:keyprov:pskc">
  <cpix:ContentKeyList>
    <cpix:ContentKey kid="{kid1}">
      <cpix:Data><pskc:Secret><pskc:PlainValue>{key1_b64}</pskc:PlainValue></pskc:Secret></cpix:Data>
    </cpix:ContentKey>
    <cpix:ContentKey kid="{kid2}">
      <cpix:Data><pskc:Secret><pskc:PlainValue>{key2_b64}</pskc:PlainValue></pskc:Secret></cpix:Data>
    </cpix:ContentKey>
  </cpix:ContentKeyList>
</cpix:CPIX>"#
        );

        let result = parse_cpix_response(xml.as_bytes()).unwrap();
        assert_eq!(result.keys.len(), 2);
        assert_eq!(result.keys[0].key, vec![0xAA; 16]);
        assert_eq!(result.keys[1].key, vec![0xCC; 16]);
    }
}
