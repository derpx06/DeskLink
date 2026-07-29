use crate::device_links::packet::{
    PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_MPRIS, PACKET_TYPE_MPRIS_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_NOTIFICATION, PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY,
    PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PING, PACKET_TYPE_RUNCOMMAND,
    PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SFTP, PACKET_TYPE_SFTP_REQUEST,
    PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_SYSTEMVOLUME, PACKET_TYPE_SYSTEMVOLUME_REQUEST,
    PACKET_TYPE_WEBRTC_SIGNAL_V1,
};

/// The capability contract for the desktop feature layer.  A packet is listed
/// only when the current desktop process has a handler that can consume it or
/// a command path that can construct it.  Remote input is included because
/// the authenticated WebRTC dispatcher owns the input handler; the runtime
/// backend still reports an error if the host cannot accept input.
#[derive(Debug, Clone)]
pub struct FeatureRegistry {
    incoming: Vec<String>,
    outgoing: Vec<String>,
}

impl FeatureRegistry {
    pub fn desktop() -> Self {
        Self {
            incoming: capabilities(&[
                PACKET_TYPE_PING,
                PACKET_TYPE_BATTERY,
                PACKET_TYPE_CLIPBOARD,
                PACKET_TYPE_CLIPBOARD_CONNECT,
                PACKET_TYPE_SHARE_REQUEST,
                PACKET_TYPE_MPRIS,
                PACKET_TYPE_NOTIFICATION,
                PACKET_TYPE_NOTIFICATION_REQUEST,
                PACKET_TYPE_SYSTEMVOLUME,
                PACKET_TYPE_RUNCOMMAND,
                PACKET_TYPE_SFTP,
                PACKET_TYPE_FINDMYPHONE_REQUEST,
                PACKET_TYPE_MOUSEPAD_REQUEST,
                PACKET_TYPE_WEBRTC_SIGNAL_V1,
            ]),
            outgoing: capabilities(&[
                PACKET_TYPE_PING,
                PACKET_TYPE_CLIPBOARD,
                PACKET_TYPE_SHARE_REQUEST,
                PACKET_TYPE_FINDMYPHONE_REQUEST,
                PACKET_TYPE_MPRIS_REQUEST,
                PACKET_TYPE_SFTP_REQUEST,
                PACKET_TYPE_NOTIFICATION_REQUEST,
                PACKET_TYPE_NOTIFICATION_REPLY,
                PACKET_TYPE_NOTIFICATION_ACTION,
                PACKET_TYPE_SYSTEMVOLUME_REQUEST,
                PACKET_TYPE_RUNCOMMAND_REQUEST,
                PACKET_TYPE_MOUSEPAD_REQUEST,
                PACKET_TYPE_WEBRTC_SIGNAL_V1,
            ]),
        }
    }

    pub fn incoming_capabilities(&self) -> &[String] {
        &self.incoming
    }

    pub fn outgoing_capabilities(&self) -> &[String] {
        &self.outgoing
    }
}

fn capabilities(values: &[&str]) -> Vec<String> {
    let mut result: Vec<_> = values.iter().map(|value| (*value).to_string()).collect();
    result.sort();
    result.dedup();
    result
}

#[cfg(test)]
mod tests {
    use super::FeatureRegistry;

    #[test]
    fn feature_registry_does_not_advertise_unavailable_handlers() {
        let registry = FeatureRegistry::desktop();

        assert!(registry
            .incoming_capabilities()
            .contains(&"desklink.ping".to_string()));
        assert!(registry
            .incoming_capabilities()
            .contains(&"desklink.mousepad.request".to_string()));
    }

    #[test]
    fn feature_registry_advertises_each_capability_once() {
        let registry = FeatureRegistry::desktop();
        let mut incoming = registry.incoming_capabilities().to_vec();
        incoming.sort();
        incoming.dedup();
        assert_eq!(incoming, registry.incoming_capabilities());
    }
}
