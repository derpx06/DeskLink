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
    /// Random per process start ("restart epoch"). A peer presenting a
    /// different one has restarted, so any WebRTC peer of its previous process
    /// is dead and must be dropped at once rather than after the 15 s silence
    /// budget. Optional on the wire: older builds omit it.
    pub instance_id: Option<String>,
}

/// This process's restart epoch; see [`DeviceInfo::instance_id`].
pub fn process_instance_id() -> &'static str {
    static INSTANCE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    INSTANCE.get_or_init(|| uuid::Uuid::new_v4().simple().to_string())
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
            device_type: local_device_type().to_string(),
            protocol_version: PROTOCOL_VERSION,
            incoming_capabilities,
            outgoing_capabilities,
            instance_id: Some(process_instance_id().to_string()),
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
        if let Some(instance_id) = &self.instance_id {
            packet.set("instanceId", instance_id.clone());
        }
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
            instance_id: packet
                .get_str("instanceId")
                .filter(|id| !id.is_empty() && id.len() <= 64)
                .map(str::to_string),
        })
    }
}

/// "laptop" or "desktop" for this machine, so the phone shows the right
/// icon. Read from the firmware's SMBIOS chassis type, the same source
/// `hostnamectl` uses; anything unreadable counts as a desktop.
fn local_device_type() -> &'static str {
    let chassis = std::fs::read_to_string("/sys/class/dmi/id/chassis_type").unwrap_or_default();
    device_type_for_chassis(chassis.trim().parse().unwrap_or(0))
}

/// SMBIOS chassis types: 8 portable, 9 laptop, 10 notebook, 11 hand held,
/// 14 sub notebook, 30 tablet, 31 convertible, 32 detachable.
fn device_type_for_chassis(chassis: u32) -> &'static str {
    match chassis {
        8 | 9 | 10 | 11 | 14 | 30 | 31 | 32 => "laptop",
        _ => "desktop",
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
    #[test]
    fn laptops_and_convertibles_report_as_laptops() {
        for chassis in [8, 9, 10, 14, 31, 32] {
            assert_eq!(super::device_type_for_chassis(chassis), "laptop");
        }
        for chassis in [0, 3, 4, 7, 13, 35] {
            assert_eq!(super::device_type_for_chassis(chassis), "desktop");
        }
    }

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
        // The restart epoch survives the round trip and is this process's.
        assert_eq!(parsed.instance_id.as_deref(), Some(process_instance_id()));
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
        assert!(local
            .incoming_capabilities
            .contains(&"desklink.clipboard".to_string()));
        assert!(local
            .incoming_capabilities
            .contains(&"desklink.notification".to_string()));
        assert!(local
            .outgoing_capabilities
            .contains(&"desklink.findmyphone.request".to_string()));
        assert!(local
            .outgoing_capabilities
            .contains(&"desklink.lock.request".to_string()));
        assert!(local
            .outgoing_capabilities
            .contains(&"desklink.mousepad.request".to_string()));

        // Remote input is handled only under a portal-approved control lease.
        // Media control runs both ways.
        assert!(local
            .incoming_capabilities
            .contains(&"desklink.mousepad.request".to_string()));
        assert!(local
            .outgoing_capabilities
            .contains(&"desklink.mpris.request".to_string()));
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
