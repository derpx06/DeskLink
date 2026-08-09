use serde_json::Value;

use super::features::FeatureRegistry;
use super::packet::{
    NetworkPacket, PACKET_TYPE_IDENTITY, PACKET_TYPE_WEBRTC_SIGNAL_V1, PROTOCOL_VERSION,
};

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
        let registry = FeatureRegistry::desktop();
        let mut incoming_capabilities = registry.incoming_capabilities().to_vec();
        let mut outgoing_capabilities = registry.outgoing_capabilities().to_vec();

        // WebRTC signaling is bootstrap transport metadata, not a feature
        // handler.  It must be present in every DeskLink v9 identity so a
        // peer can negotiate a fresh data-channel session after re-pairing or
        // a bootstrap socket replacement.
        incoming_capabilities.push(PACKET_TYPE_WEBRTC_SIGNAL_V1.to_string());
        outgoing_capabilities.push(PACKET_TYPE_WEBRTC_SIGNAL_V1.to_string());
        incoming_capabilities.sort();
        incoming_capabilities.dedup();
        outgoing_capabilities.sort();
        outgoing_capabilities.dedup();
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
        Some(Self {
            id,
            name,
            device_type: packet
                .get_str("deviceType")
                .unwrap_or("unknown")
                .to_string(),
            protocol_version: packet.get_i64("protocolVersion").unwrap_or(-1),
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
    use crate::device_links::packet::PACKET_TYPE_PING;

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
            .contains(&"desklink.ping".into()));
    }

    #[test]
    fn local_identity_advertises_only_registered_capabilities() {
        let local = DeviceInfo::local("0123456789abcdef0123456789abcdef".into(), "DeskLink".into());

        assert!(local
            .incoming_capabilities
            .contains(&PACKET_TYPE_PING.to_string()));
        assert!(!local
            .incoming_capabilities
            .contains(&"desklink.mousepad.request".to_string()));
        assert!(local
            .outgoing_capabilities
            .contains(&"desklink.findmyphone.request".to_string()));
        assert!(!local
            .outgoing_capabilities
            .contains(&"desklink.lock.request".to_string()));
        assert!(!local
            .outgoing_capabilities
            .contains(&"desklink.systemvolume.request".to_string()));
    }

    #[test]
    fn local_identity_always_advertises_webrtc_bootstrap_signaling() {
        let local = DeviceInfo::local("0123456789abcdef0123456789abcdef".into(), "DeskLink".into());

        assert!(local
            .incoming_capabilities
            .contains(&"desklink.webrtc.signal.v1".to_string()));
        assert!(local
            .outgoing_capabilities
            .contains(&"desklink.webrtc.signal.v1".to_string()));
    }

    #[test]
    fn rejects_invalid_identity_packets() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        packet.set("deviceId", "not valid");
        packet.set("deviceName", "Phone");

        assert!(DeviceInfo::from_identity_packet(&packet).is_none());
    }
}
