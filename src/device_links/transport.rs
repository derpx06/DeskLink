//! Transport boundary for all paired DeskLink features.
//!
//! Discovery, identity, pairing, and signed WebRTC signaling belong to the
//! bootstrap link. Feature callers receive this handle instead, so they cannot
//! accidentally write ordinary packets to TLS or depend on GStreamer details.

use std::path::Path;
use std::sync::Arc;

use super::core::{DeviceManager, FeatureTransportSnapshot, SessionBinding};
use super::packet::NetworkPacket;

/// Session replacement and handover are normal recovery states. They should
/// update connection status, not masquerade as permanent feature failures in
/// the UI. The operation is retried against the next current generation.
pub(crate) fn should_surface_send_error(error: &str) -> bool {
    !error.contains("replaced by a newer session generation")
        && !error.contains("WebRTC feature transport is not ready")
        && !error.contains("Device is not connected")
        && !error.contains("Stale DeskLink session")
}

#[derive(Clone)]
pub(crate) struct FeatureTransport {
    sessions: Arc<DeviceManager>,
    snapshot: FeatureTransportSnapshot,
}

impl FeatureTransport {
    pub(crate) fn for_device(
        sessions: &Arc<DeviceManager>,
        device_id: &str,
    ) -> Result<Self, String> {
        let binding = sessions
            .current_binding(device_id)
            .ok_or_else(|| "Device is not connected".to_string())?;
        Self::for_binding(sessions, &binding)
    }

    pub(crate) fn for_binding(
        sessions: &Arc<DeviceManager>,
        binding: &SessionBinding,
    ) -> Result<Self, String> {
        let snapshot = sessions
            .feature_transport_snapshot(binding)
            .map_err(|error| error.to_string())?;
        if !snapshot.paired {
            return Err("Device must be paired before sending DeskLink features".to_string());
        }
        Ok(Self {
            sessions: Arc::clone(sessions),
            snapshot,
        })
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.snapshot.web_rtc_ready && self.sessions.is_current(&self.snapshot.binding)
    }

    pub(crate) fn send(&self, packet: &NetworkPacket) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::send_feature_packet(
            &self.sessions,
            &self.snapshot,
            packet,
        )
    }

    pub(crate) fn send_file(&self, path: &Path) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_file_transfer(
            &self.sessions,
            &self.snapshot,
            path,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::core::SessionLink;
    use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_PING};

    #[test]
    fn unknown_device_has_no_feature_transport() {
        let sessions = Arc::new(DeviceManager::new());
        assert!(FeatureTransport::for_device(&sessions, "missing").is_err());
    }

    #[test]
    fn paired_session_is_not_ready_before_authenticated_handover() {
        let sessions = Arc::new(DeviceManager::new());
        sessions
            .register_link(
                "phone-1".to_string(),
                SessionLink::test_placeholder("phone-1"),
                true,
            )
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();

        assert!(!transport.is_ready());
        assert!(transport
            .send(&NetworkPacket::new(PACKET_TYPE_PING))
            .unwrap_err()
            .contains("not ready"));
    }

    #[test]
    fn reconnect_transition_errors_are_not_presented_as_feature_failures() {
        assert!(!should_surface_send_error(
            "Connection was replaced by a newer session generation"
        ));
        assert!(!should_surface_send_error(
            "DeskLink WebRTC feature transport is not ready; paired features are not sent over LAN"
        ));
        assert!(!should_surface_send_error("Device is not connected"));
        assert!(should_surface_send_error("Remote command was rejected"));
    }
}
