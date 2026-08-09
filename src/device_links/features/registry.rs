use crate::device_links::packet::{PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_PING};

/// The production WebRTC profile is deliberately explicit. A capability
/// must not be advertised merely because an old LAN handler exists: paired
/// features have no LAN fallback, so only packet types with a working handler
/// on both applications may be offered here.
const INITIAL_WEBRTC_FEATURES: [&str; 2] = [PACKET_TYPE_PING, PACKET_TYPE_FINDMYPHONE_REQUEST];

pub(crate) fn is_webrtc_feature(packet_type: &str) -> bool {
    INITIAL_WEBRTC_FEATURES.contains(&packet_type)
}

/// Returns the enabled WebRTC subset of an authenticated identity capability
/// list.  The identity may describe future or legacy features, but the
/// handover must activate only the profile that both applications implement.
pub(crate) fn webrtc_capabilities(capabilities: &[String]) -> Vec<String> {
    let mut result: Vec<_> = capabilities
        .iter()
        .filter(|capability| is_webrtc_feature(capability))
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
            incoming: capabilities(&[PACKET_TYPE_PING, PACKET_TYPE_FINDMYPHONE_REQUEST]),
            outgoing: capabilities(&[PACKET_TYPE_PING, PACKET_TYPE_FINDMYPHONE_REQUEST]),
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
    fn stable_webrtc_profile_advertises_only_implemented_directions() {
        let registry = FeatureRegistry::desktop();
        let expected_incoming = vec![
            "desklink.findmyphone.request".to_string(),
            "desklink.ping".to_string(),
        ];
        let expected_outgoing = expected_incoming.clone();

        assert_eq!(registry.incoming_capabilities(), expected_incoming);
        assert_eq!(registry.outgoing_capabilities(), expected_outgoing);
    }
}
