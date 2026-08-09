use crate::device_links::device::ContactSummary;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
    PACKET_TYPE_CONTACTS_REQUEST_VCARDS_BY_UID,
};

pub fn request_all_uids_timestamps() -> NetworkPacket {
    NetworkPacket::new(PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS)
}

pub fn request_vcards(uids: &[String]) -> Result<NetworkPacket, String> {
    if uids.is_empty() || uids.len() > 1024 {
        return Err("Contact UID list is empty or too large".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_CONTACTS_REQUEST_VCARDS_BY_UID);
    packet.set("uids", uids.to_vec());
    Ok(packet)
}

pub fn parse_vcards(packet: &NetworkPacket) -> Result<Vec<ContactSummary>, String> {
    let uids = packet
        .body
        .get("uids")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "Contacts response has no UID list".to_string())?;
    let mut contacts = Vec::new();
    for uid in uids.iter().filter_map(|value| value.as_str()) {
        let Some(vcard) = packet.body.get(uid).and_then(|value| value.as_str()) else {
            continue;
        };
        contacts.push(parse_vcard(uid, vcard)?);
    }
    Ok(contacts)
}

pub fn parse_vcard(uid: &str, value: &str) -> Result<ContactSummary, String> {
    if uid.is_empty() || uid.len() > 256 {
        return Err("Contact UID is invalid".to_string());
    }
    let lines = unfold_lines(value);
    let name = property_value(&lines, "FN").unwrap_or_else(|| "Unknown contact".to_string());
    let phones = property_values(&lines, "TEL");
    let emails = property_values(&lines, "EMAIL");
    if name.len() > 512 || phones.len() > 64 || emails.len() > 64 {
        return Err("Contact record is too large".to_string());
    }
    Ok(ContactSummary {
        uid: uid.to_string(),
        name,
        phones,
        emails,
    })
}

fn unfold_lines(value: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for line in value.replace("\r\n", "\n").split('\n') {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(previous) = lines.last_mut() {
                previous.push_str(line.trim_start());
            }
        } else {
            lines.push(line.to_string());
        }
    }
    lines
}

fn property_values(lines: &[String], property: &str) -> Vec<String> {
    lines
        .iter()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let key = key.split(';').next()?.to_ascii_uppercase();
            (key == property).then(|| value.trim().to_string())
        })
        .filter(|value| !value.is_empty())
        .collect()
}

fn property_value(lines: &[String], property: &str) -> Option<String> {
    property_values(lines, property).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_folded_vcard_fields() {
        let contact = parse_vcard(
            "42",
            "BEGIN:VCARD\r\nFN:Alice Example\r\nTEL;TYPE=cell:+123\r\nEMAIL:a@b.example\r\nNOTE:fold\r\n ed\r\nEND:VCARD",
        )
        .unwrap();
        assert_eq!(contact.name, "Alice Example");
        assert_eq!(contact.phones, vec!["+123"]);
        assert_eq!(contact.emails, vec!["a@b.example"]);
    }

    #[test]
    fn rejects_empty_contact_requests() {
        assert!(request_vcards(&[]).is_err());
    }
}
