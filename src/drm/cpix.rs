use crate::drm::{ContentKey, DrmKeySet, DrmSystemData};
use crate::error::{EdgePackagerError, Result};
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
                            .map_err(|e| EdgePackagerError::Cpix(format!("invalid base64 key: {e}")))?;
                        keys.push(ContentKey {
                            kid,
                            key: key_data,
                            iv: None,
                        });
                    }
                } else if in_pssh {
                    if let (Some(kid), Some(system_id)) = (current_kid, current_system_id) {
                        let pssh_data = b64.decode(text).map_err(|e| {
                            EdgePackagerError::Cpix(format!("invalid base64 PSSH: {e}"))
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
                return Err(EdgePackagerError::Cpix(format!("XML parse error: {e}")));
            }
            _ => {}
        }
        buf.clear();
    }

    if keys.is_empty() {
        return Err(EdgePackagerError::Cpix(
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
        return Err(EdgePackagerError::Cpix(format!(
            "invalid UUID: expected 32 hex chars, got {}",
            hex.len()
        )));
    }
    let mut bytes = [0u8; 16];
    for i in 0..16 {
        bytes[i] = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16)
            .map_err(|e| EdgePackagerError::Cpix(format!("invalid UUID hex: {e}")))?;
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
