use serde_json::Value;

use super::core::capability_registry::desktop_capabilities;
use super::packet::{NetworkPacket, PACKET_TYPE_IDENTITY, PROTOCOL_VERSION};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub protocol_version: i64,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
}

impl DeviceInfo {
    pub fn local(id: String, name: String) -> Self {
        let (incoming_capabilities, outgoing_capabilities) = desktop_capabilities();
        Self {
            id,
            name,
            device_type: "desktop".to_string(),
            protocol_version: PROTOCOL_VERSION,
            incoming_capabilities,
            outgoing_capabilities,
        }
    }

    pub fn to_identity_packet(&self, tcp_port: u16) -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        packet.set("deviceId", self.id.clone());
        packet.set("deviceName", self.name.clone());
        packet.set("deviceType", self.device_type.clone());
        packet.set("protocolVersion", self.protocol_version);
        packet.set("incomingCapabilities", self.incoming_capabilities.clone());
        packet.set("outgoingCapabilities", self.outgoing_capabilities.clone());
        packet.set("tcpPort", i64::from(tcp_port));
        packet
    }

    pub fn from_identity_packet(packet: &NetworkPacket) -> Option<Self> {
        if packet.packet_type != PACKET_TYPE_IDENTITY {
            return None;
        }
        let id = packet.get_str("deviceId")?.to_string();
        let name = filter_name(packet.get_str("deviceName")?);
        if !is_valid_device_id(&id) || name.is_empty() {
            return None;
        }
        let protocol_version = packet.get_i64("protocolVersion").unwrap_or(-1);
        if protocol_version != PROTOCOL_VERSION {
            return None;
        }
        Some(Self {
            id,
            name,
            device_type: packet
                .get_str("deviceType")
                .unwrap_or("unknown")
                .to_string(),
            protocol_version,
            incoming_capabilities: string_array(packet.body.get("incomingCapabilities")),
            outgoing_capabilities: string_array(packet.body.get("outgoingCapabilities")),
        })
    }
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn filter_name(input: &str) -> String {
    input
        .chars()
        .filter(|c| !"\"',;:.!?()[]<>".contains(*c))
        .take(32)
        .collect()
}

fn is_valid_device_id(id: &str) -> bool {
    (32..=38).contains(&id.len())
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_identity_packets() {
        let local = DeviceInfo::local("0123456789abcdef0123456789abcdef".into(), "DeskLink".into());
        let packet = local.to_identity_packet(1716);
        let parsed = DeviceInfo::from_identity_packet(&packet).unwrap();

        assert_eq!(parsed.id, local.id);
        assert_eq!(parsed.name, "DeskLink");
        assert_eq!(parsed.protocol_version, 9);
        assert!(parsed
            .incoming_capabilities
            .contains(&"desklink.ping".to_string()));
    }

    #[test]
    fn local_identity_advertises_screen_frame_receiver() {
        let local = DeviceInfo::local("0123456789abcdef0123456789abcdef".into(), "DeskLink".into());
        let (incoming, outgoing) = desktop_capabilities();
        assert_eq!(local.incoming_capabilities, incoming);
        assert_eq!(local.outgoing_capabilities, outgoing);
        assert!(local
            .incoming_capabilities
            .iter()
            .any(|capability| capability == "desklink.screen.frame"));
    }

    #[test]
    fn rejects_invalid_identity_packets() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        packet.set("deviceId", "not valid");
        packet.set("deviceName", "Phone");

        assert!(DeviceInfo::from_identity_packet(&packet).is_none());
    }
}
