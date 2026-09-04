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
        // A link closed underneath a send is a transport transition, not a
        // feature failure. Discovery installs a replacement within seconds, and
        // surfacing it turned a dead socket into a banner and a desktop
        // notification once per second from the clipboard poller.
        && !error.contains("Session link is closed")
        && !error.contains("Session link has no network stream")
        // Matches both send paths. The WebRTC path says "DeskLink WebRTC
        // feature profile" and the bootstrap path says "DeskLink feature
        // profile", so the common substring is the one to test.
        && !error.contains("is not enabled in the DeskLink")
        && !error.contains("DeskLink has no outgoing handler for")
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

    /// Sends a paired feature over the authenticated WebRTC transport, falling
    /// back to the pinned-TLS bootstrap link while the handover is still in
    /// progress.
    ///
    /// The fallback is deliberate and bounded. LAN is already mutually
    /// authenticated with pinned certificates, so it is a safe carrier for
    /// ordinary feature packets; without it every feature would be dead on any
    /// network where ICE cannot complete. File bytes are excluded — they stay
    /// on the bounded WebRTC file-data channel, because the legacy TCP payload
    /// socket has been removed on both platforms.
    pub(crate) fn send(&self, packet: &NetworkPacket) -> Result<(), String> {
        if self.is_ready() {
            return crate::device_links::webrtc::coordinator::send_feature_packet(
                &self.sessions,
                &self.snapshot,
                packet,
            );
        }
        self.send_over_bootstrap(packet)
    }

    fn send_over_bootstrap(&self, packet: &NetworkPacket) -> Result<(), String> {
        if packet.payload_size.is_some() || packet.payload_transfer_info.is_some() {
            return Err(
                "DeskLink file transfer requires an authenticated WebRTC file channel".to_string(),
            );
        }
        // Apply the same outgoing contract the WebRTC path applies. Testing the
        // union of both directions would let an incoming-only type through here
        // and not there, so the two send paths would disagree about what is
        // legal and the peer would silently drop it as unadvertised.
        let registry = crate::device_links::features::FeatureRegistry::desktop();
        if !registry
            .outgoing_capabilities()
            .iter()
            .any(|capability| capability == &packet.packet_type)
        {
            return Err(format!(
                "{} is not enabled in the DeskLink feature profile",
                packet.packet_type
            ));
        }
        if !self.sessions.is_current(&self.snapshot.binding) {
            return Err("Connection was replaced by a newer session generation".to_string());
        }
        let link = &self.snapshot.binding.link;
        if !link
            .info
            .incoming_capabilities
            .iter()
            .any(|capability| capability == &packet.packet_type)
        {
            return Err(format!(
                "The paired device does not accept {}",
                packet.packet_type
            ));
        }
        link.send_packet(packet)
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
        let mut link = SessionLink::test_placeholder("phone-1");
        link.info.incoming_capabilities = vec![PACKET_TYPE_PING.to_string()];
        sessions
            .register_link("phone-1".to_string(), link, true)
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();

        // WebRTC is not ready, so the send falls through to the bootstrap link.
        // The placeholder has no socket, which is how far this test can drive it.
        assert!(!transport.is_ready());
        assert!(transport
            .send(&NetworkPacket::new(PACKET_TYPE_PING))
            .unwrap_err()
            .contains("no network stream"));
    }

    #[test]
    fn bootstrap_fallback_refuses_packets_outside_the_feature_profile() {
        let sessions = Arc::new(DeviceManager::new());
        sessions
            .register_link(
                "phone-1".to_string(),
                SessionLink::test_placeholder("phone-1"),
                true,
            )
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();

        let error = transport
            .send(&NetworkPacket::new(
                crate::device_links::packet::PACKET_TYPE_MPRIS_REQUEST,
            ))
            .unwrap_err();
        assert!(error.contains("not enabled in the DeskLink feature profile"));
    }

    #[test]
    fn bootstrap_fallback_never_carries_file_payloads() {
        let sessions = Arc::new(DeviceManager::new());
        let mut link = SessionLink::test_placeholder("phone-1");
        link.info.incoming_capabilities =
            vec![crate::device_links::packet::PACKET_TYPE_SHARE_REQUEST.to_string()];
        sessions
            .register_link("phone-1".to_string(), link, true)
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();

        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_REQUEST);
        packet.payload_size = Some(1024);
        assert!(transport
            .send(&packet)
            .unwrap_err()
            .contains("authenticated WebRTC file channel"));
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
