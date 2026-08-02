use crate::device_links::packet::{
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_PING, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_WEBRTC_SIGNAL_V1,
};

/// The first production WebRTC profile is deliberately small.  A capability
/// must not be advertised merely because an old LAN handler exists: paired
/// features have no LAN fallback, so only features proven on the authenticated
/// WebRTC path may be offered here.
const INITIAL_WEBRTC_FEATURES: [&str; 3] = [
    PACKET_TYPE_PING,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    PACKET_TYPE_SHARE_REQUEST,
];

pub(crate) fn is_initial_webrtc_feature(packet_type: &str) -> bool {
    INITIAL_WEBRTC_FEATURES.contains(&packet_type)
}

/// Returns the enabled WebRTC subset of an authenticated identity capability
/// list.  The identity may describe future or legacy features, but the
/// handover must activate only the profile that both applications implement.
pub(crate) fn initial_webrtc_capabilities(capabilities: &[String]) -> Vec<String> {
    let mut result: Vec<_> = capabilities
        .iter()
        .filter(|capability| is_initial_webrtc_feature(capability))
        .cloned()
        .collect();
    result.sort();
    result.dedup();
    result
}

/// The capability contract for the desktop feature layer.  A packet is listed
/// only when the current desktop process has a handler that can consume it or
/// a command path that can construct it.  Portal and host-backend dependent
/// features are deliberately absent until their adapters are installed.
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
                PACKET_TYPE_FINDMYPHONE_REQUEST,
                PACKET_TYPE_SHARE_REQUEST,
                PACKET_TYPE_WEBRTC_SIGNAL_V1,
            ]),
            outgoing: capabilities(&[
                PACKET_TYPE_PING,
                PACKET_TYPE_FINDMYPHONE_REQUEST,
                PACKET_TYPE_SHARE_REQUEST,
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
        assert!(!registry
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

    #[test]
    fn initial_webrtc_profile_advertises_file_sharing_with_ping_and_find_phone() {
        let registry = FeatureRegistry::desktop();
        let expected = vec![
            "desklink.findmyphone.request".to_string(),
            "desklink.ping".to_string(),
            "desklink.share.request".to_string(),
            "desklink.webrtc.signal.v1".to_string(),
        ];

        assert_eq!(registry.incoming_capabilities(), expected);
        assert_eq!(registry.outgoing_capabilities(), expected);
    }
}
