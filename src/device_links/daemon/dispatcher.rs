//! Source-aware packet authorization for the active DeskLink session.
//!
//! A feature handler must never need to infer whether a packet arrived on the
//! bootstrap TLS link or an authenticated WebRTC data channel. This module is
//! the one policy gate used before any handler mutates device state.

use crate::device_links::core::{DeviceManager, SessionBinding};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::features::is_initial_webrtc_feature;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_IDENTITY, PACKET_TYPE_PAIR, PACKET_TYPE_WEBRTC_SIGNAL_V1,
};
use crate::device_links::pairing::PairState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PacketSource {
    /// Discovery/identity, pairing, and signed SDP/ICE records on the
    /// bootstrap TLS connection.
    LanBootstrap,
    /// A packet decoded from an authenticated WebRTC data-channel envelope.
    WebRtc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DispatchError {
    StaleSession,
    UnpairedFeature,
    BootstrapOnWebRtc,
    FeatureOnLanAfterHandover,
    PayloadOnLan,
    PayloadOnWebRtc,
    FeatureNotEnabled,
    UnsupportedByDeskLink,
    UnsupportedByPeer,
}

impl std::fmt::Display for DispatchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::StaleSession => "DeskLink session was replaced before packet dispatch",
            Self::UnpairedFeature => "DeskLink rejected a feature packet before pairing",
            Self::BootstrapOnWebRtc => {
                "DeskLink rejected identity, pairing, or signaling from WebRTC"
            }
            Self::FeatureOnLanAfterHandover => {
                "DeskLink rejected LAN feature traffic after WebRTC handover"
            }
            Self::PayloadOnWebRtc => {
                "DeskLink rejected a legacy payload packet on the WebRTC event bridge"
            }
            Self::PayloadOnLan => {
                "DeskLink rejected a legacy payload packet on the LAN feature path"
            }
            Self::FeatureNotEnabled => {
                "DeskLink rejected a feature that is not enabled in the active WebRTC profile"
            }
            Self::UnsupportedByDeskLink => {
                "DeskLink rejected a packet with no active local handler"
            }
            Self::UnsupportedByPeer => {
                "DeskLink rejected a packet the paired device did not advertise"
            }
        };
        formatter.write_str(message)
    }
}

/// Performs only authorization. LAN is bootstrap-only; paired feature packets
/// are accepted exclusively from an authenticated WebRTC handover.
pub(crate) struct SharedPacketDispatcher;

impl SharedPacketDispatcher {
    pub(crate) fn authorize(
        source: PacketSource,
        binding: &SessionBinding,
        sessions: &DeviceManager,
        local_info: &DeviceInfo,
        packet: &NetworkPacket,
    ) -> Result<(), DispatchError> {
        let is_bootstrap = matches!(
            packet.packet_type.as_str(),
            PACKET_TYPE_IDENTITY | PACKET_TYPE_PAIR | PACKET_TYPE_WEBRTC_SIGNAL_V1
        );
        if is_bootstrap {
            return if source == PacketSource::LanBootstrap {
                Ok(())
            } else {
                Err(DispatchError::BootstrapOnWebRtc)
            };
        }

        if source == PacketSource::WebRtc
            && (packet.payload_size.is_some() || packet.payload_transfer_info.is_some())
        {
            // File data will move to its dedicated bounded file-data channel.
            // A legacy TCP payload descriptor can never be smuggled over the
            // generic packet bridge.
            return Err(DispatchError::PayloadOnWebRtc);
        }
        if source == PacketSource::LanBootstrap
            && (packet.payload_size.is_some() || packet.payload_transfer_info.is_some())
        {
            // File bytes are WebRTC file-data messages only.  Reject the
            // legacy TCP payload descriptor before any LAN handler can open a
            // socket or mutate the destination filesystem.
            return Err(DispatchError::PayloadOnLan);
        }

        let result = sessions.with_session(binding, |session| {
            let paired = session.pairing.state == PairState::Paired;
            let feature_ready = session
                .webrtc_handover
                .as_ref()
                .is_some_and(|handover| handover.features_ready());
            let peer_advertises = session.active_link.as_ref().is_some_and(|link| {
                link.info
                    .outgoing_capabilities
                    .iter()
                    .any(|capability| capability == &packet.packet_type)
            });
            (paired, feature_ready, peer_advertises)
        });
        let (paired, feature_ready, peer_advertises) =
            result.map_err(|_| DispatchError::StaleSession)?;

        if !paired {
            return Err(DispatchError::UnpairedFeature);
        }
        if source == PacketSource::LanBootstrap {
            return Err(DispatchError::FeatureOnLanAfterHandover);
        }
        if source == PacketSource::WebRtc && !feature_ready {
            return Err(DispatchError::UnpairedFeature);
        }
        if source == PacketSource::WebRtc && !is_initial_webrtc_feature(&packet.packet_type) {
            return Err(DispatchError::FeatureNotEnabled);
        }
        if !local_info
            .incoming_capabilities
            .iter()
            .any(|capability| capability == &packet.packet_type)
        {
            return Err(DispatchError::UnsupportedByDeskLink);
        }
        if !peer_advertises {
            return Err(DispatchError::UnsupportedByPeer);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::device_links::core::{DeviceManager, SessionLink};
    use crate::device_links::packet::{
        NetworkPacket, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_PAIR, PACKET_TYPE_PING,
    };
    use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};

    use super::*;

    fn local() -> DeviceInfo {
        DeviceInfo::local("d".repeat(32), "DeskLink".to_string())
    }

    fn paired_binding(manager: &DeviceManager, peer_capabilities: Vec<String>) -> SessionBinding {
        let mut link = SessionLink::test_placeholder("p1");
        link.info.outgoing_capabilities = peer_capabilities;
        manager
            .register_link("p1".to_string(), link, true)
            .unwrap()
            .binding
    }

    fn make_feature_ready(runtime: &mut HandoverRuntime) {
        runtime.record_offer("offer".to_string()).unwrap();
        runtime.record_answer("answer".to_string()).unwrap();
        runtime.mark_local_authenticated().unwrap();
        runtime.mark_remote_authenticated().unwrap();
        runtime.mark_local_capabilities_confirmed().unwrap();
        runtime.mark_remote_capabilities_confirmed().unwrap();
        runtime.mark_local_feature_ready().unwrap();
        runtime.mark_remote_feature_ready().unwrap();
    }

    #[test]
    fn rejects_feature_packets_before_pairing() {
        let manager = DeviceManager::new();
        let mut link = SessionLink::test_placeholder("p1");
        link.info.outgoing_capabilities = vec![PACKET_TYPE_CLIPBOARD.to_string()];
        let binding = manager
            .register_link("p1".to_string(), link, false)
            .unwrap()
            .binding;

        let error = SharedPacketDispatcher::authorize(
            PacketSource::LanBootstrap,
            &binding,
            &manager,
            &local(),
            &NetworkPacket::new(PACKET_TYPE_CLIPBOARD),
        )
        .unwrap_err();

        assert_eq!(error, DispatchError::UnpairedFeature);
    }

    #[test]
    fn bootstrap_pair_is_never_accepted_on_webrtc() {
        let manager = DeviceManager::new();
        let binding = paired_binding(&manager, Vec::new());

        let error = SharedPacketDispatcher::authorize(
            PacketSource::WebRtc,
            &binding,
            &manager,
            &local(),
            &NetworkPacket::new(PACKET_TYPE_PAIR),
        )
        .unwrap_err();

        assert_eq!(error, DispatchError::BootstrapOnWebRtc);
    }

    #[test]
    fn rejects_lan_features_before_handover() {
        let manager = DeviceManager::new();
        let binding = paired_binding(&manager, Vec::new());

        let error = SharedPacketDispatcher::authorize(
            PacketSource::LanBootstrap,
            &binding,
            &manager,
            &local(),
            &NetworkPacket::new(PACKET_TYPE_CLIPBOARD),
        )
        .unwrap_err();

        assert_eq!(error, DispatchError::FeatureOnLanAfterHandover);
    }

    #[test]
    fn rejects_lan_features_and_non_profile_webrtc_features_after_handover() {
        let manager = DeviceManager::new();
        let binding = paired_binding(&manager, vec![PACKET_TYPE_CLIPBOARD.to_string()]);
        manager
            .with_session(&binding, |session| {
                let wire = WebRtcWireBinding::from_attempt(
                    "desktop",
                    "phone",
                    "01234567-89ab-cdef-0123-456789abcdef",
                )
                .unwrap();
                let mut runtime = HandoverRuntime::new("attempt".to_string(), wire);
                make_feature_ready(&mut runtime);
                session.webrtc_handover = Some(runtime);
            })
            .unwrap();
        let packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);

        assert_eq!(
            SharedPacketDispatcher::authorize(
                PacketSource::LanBootstrap,
                &binding,
                &manager,
                &local(),
                &packet,
            )
            .unwrap_err(),
            DispatchError::FeatureOnLanAfterHandover
        );
        assert_eq!(
            SharedPacketDispatcher::authorize(
            PacketSource::WebRtc,
            &binding,
            &manager,
            &local(),
            &packet,
        )
        .unwrap_err(),
            DispatchError::FeatureNotEnabled
        );
    }

    #[test]
    fn allows_ping_over_webrtc_after_handover() {
        let manager = DeviceManager::new();
        let binding = paired_binding(&manager, vec![PACKET_TYPE_PING.to_string()]);
        manager
            .with_session(&binding, |session| {
                let wire = WebRtcWireBinding::from_attempt(
                    "desktop",
                    "phone",
                    "01234567-89ab-cdef-0123-456789abcdef",
                )
                .unwrap();
                let mut runtime = HandoverRuntime::new("attempt".to_string(), wire);
                make_feature_ready(&mut runtime);
                session.webrtc_handover = Some(runtime);
            })
            .unwrap();

        assert!(SharedPacketDispatcher::authorize(
            PacketSource::WebRtc,
            &binding,
            &manager,
            &local(),
            &NetworkPacket::new(PACKET_TYPE_PING),
        )
        .is_ok());
    }

    #[test]
    fn rejects_legacy_payload_descriptors_on_lan() {
        let manager = DeviceManager::new();
        let binding = paired_binding(&manager, vec![PACKET_TYPE_CLIPBOARD.to_string()]);
        let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
        packet.payload_size = Some(4);

        assert_eq!(
            SharedPacketDispatcher::authorize(
                PacketSource::LanBootstrap,
                &binding,
                &manager,
                &local(),
                &packet,
            )
            .unwrap_err(),
            DispatchError::PayloadOnLan
        );
    }
}
