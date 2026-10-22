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

    /// Sends a paired feature over the authenticated WebRTC transport, or
    /// queues it until the handover completes. LAN never carries features:
    /// when it did, the two apps disagreed about which transport was live and
    /// packets were dropped in silence. The queue is flushed in order by
    /// `flush_pending_features` once WebRTC is ready.
    pub(crate) fn send(&self, packet: &NetworkPacket) -> Result<(), String> {
        if self.is_ready() {
            return crate::device_links::webrtc::coordinator::send_feature_packet(
                &self.sessions,
                &self.snapshot,
                packet,
            );
        }
        if packet.payload_size.is_some() || packet.payload_transfer_info.is_some() {
            return Err(
                "DeskLink file transfer requires an authenticated WebRTC file channel".to_string(),
            );
        }
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
        let queued = self
            .sessions
            .queue_feature(&self.snapshot.binding, packet.clone())
            .map_err(|error| error.to_string())?;
        if queued {
            return Ok(());
        }
        // The handover finished after this handle's snapshot was taken: send
        // on a fresh one rather than queueing behind a flush that already ran.
        let fresh = Self::for_binding(&self.sessions, &self.snapshot.binding)?;
        if !fresh.is_ready() {
            return Err("WebRTC feature transport is not ready".to_string());
        }
        fresh.send(packet)
    }

    pub(crate) fn send_file(&self, path: &Path) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_file_transfer(
            &self.sessions,
            &self.snapshot,
            path,
        )
    }

    /// Sends an image the user copied here, for the phone's clipboard (C3).
    pub(crate) fn send_clipboard_image(&self, path: &Path) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_clipboard_transfer(
            &self.sessions,
            &self.snapshot,
            path,
            "image/png",
        )
    }

    /// Asks the phone for its camera (C4).
    pub(crate) fn start_camera(
        &self,
        facing: &str,
        width: i32,
        height: i32,
        audio: bool,
    ) -> Result<(), String> {
        let device_id = self.snapshot.binding.device_id.clone();
        crate::device_links::webrtc::coordinator::send_camera_message(
            &self.sessions,
            &self.snapshot,
            |build| {
                Some(crate::device_links::webrtc::camera::begin(
                    &device_id, facing, width, height, 30, audio, build,
                ))
            },
        )
    }

    /// Sound on or off (F1): see `coordinator::audio_switch`.
    pub(crate) fn sound(&self, what: &str, on: bool) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::audio_switch(
            &self.sessions,
            &self.snapshot,
            what,
            on,
        )
    }

    /// `flip` or `stop` for the running camera session.
    pub(crate) fn camera_control(&self, action: &str) -> Result<(), String> {
        let device_id = self.snapshot.binding.device_id.clone();
        let camera = crate::device_links::webrtc::camera::status(&device_id);
        if let Some(camera) = camera.filter(|_| action == "stop") {
            if crate::device_links::webrtc::audio_session::end_camera_microphone(
                &device_id, &camera.id,
            ) {
                if let Some(peer) = self.snapshot.web_rtc_peer.as_ref() {
                    peer.stop_audio_playback();
                }
            }
        }
        crate::device_links::webrtc::coordinator::send_camera_message(
            &self.sessions,
            &self.snapshot,
            |build| crate::device_links::webrtc::camera::control(&device_id, action, build),
        )
    }

    pub(crate) fn cancel_transfer(&self, transfer_id: &str) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::cancel_file_transfer(
            &self.sessions,
            &self.snapshot,
            transfer_id,
        )
    }

    pub(crate) fn start_phone_remote_control(&self) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_phone_remote_control(
            &self.sessions,
            &self.snapshot,
        )
    }

    pub(crate) fn request_phone_control(&self) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::request_phone_control(
            &self.sessions,
            &self.snapshot,
        )
    }

    pub(crate) fn stop_phone_remote_control(&self) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::stop_phone_remote_control(
            &self.sessions,
            &self.snapshot,
        )
    }

    pub(crate) fn send_remote_input(&self, mut packet: NetworkPacket) -> Result<(), String> {
        if !self.is_ready() {
            return Err("DeskLink remote control needs a ready WebRTC session".to_string());
        }
        crate::device_links::webrtc::coordinator::stamp_remote_input(
            &self.snapshot.binding.device_id,
            &mut packet,
        )?;
        crate::device_links::webrtc::coordinator::send_feature_packet(
            &self.sessions,
            &self.snapshot,
            &packet,
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

        // WebRTC is not ready, so the send is queued rather than written to
        // LAN, and comes back out in order for the flush on feature-ready.
        assert!(!transport.is_ready());
        let mut second = NetworkPacket::new(PACKET_TYPE_PING);
        second.set("message", "second");
        transport
            .send(&NetworkPacket::new(PACKET_TYPE_PING))
            .unwrap();
        transport.send(&second).unwrap();
        let binding = sessions.current_binding("phone-1").unwrap();
        let queued = sessions.take_pending_features(&binding);
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[1].get_str("message"), Some("second"));
        assert!(sessions.take_pending_features(&binding).is_empty());
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
                crate::device_links::packet::PACKET_TYPE_SYSTEMVOLUME_REQUEST,
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
