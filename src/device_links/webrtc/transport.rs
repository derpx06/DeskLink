use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;

use super::channel::{ChannelError, DataChannelSpec};
use super::envelope::{EnvelopeError, MessageEnvelope};
use super::handover::{HandoverMessage, HandoverState};
use super::packet_bridge::{encode_packet, PacketBridgeError};
use super::peer_connection::PeerConnection;
use super::wire_binding::WebRtcWireBinding;
use crate::device_links::packet::NetworkPacket;

pub const MAX_QUEUED_MESSAGES: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IceServerConfig {
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<String>,
}

impl IceServerConfig {
    pub fn parse(stun: &str, turn: &str) -> Result<Self, TransportError> {
        fn split(value: &str) -> Vec<String> {
            value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        }
        let value = Self {
            stun_servers: split(stun),
            turn_servers: split(turn),
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), TransportError> {
        if self.stun_servers.len() > 8 || self.turn_servers.len() > 8 {
            return Err(TransportError::Gstreamer(
                "at most eight STUN and eight TURN servers may be configured".to_string(),
            ));
        }
        for uri in &self.stun_servers {
            if !(uri.starts_with("stun://") || uri.starts_with("stuns://")) {
                return Err(TransportError::Gstreamer(format!(
                    "unsupported STUN server URI: {uri}"
                )));
            }
        }
        for uri in &self.turn_servers {
            if !(uri.starts_with("turn://") || uri.starts_with("turns://")) {
                return Err(TransportError::Gstreamer(format!(
                    "unsupported TURN server URI: {uri}"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportState {
    New,
    Gathering,
    Checking,
    Connected,
    Degraded,
    Closed,
    Failed,
}

#[derive(Debug)]
pub enum TransportError {
    Gstreamer(String),
    Closed,
    Backpressure,
    Channel(ChannelError),
    Envelope(EnvelopeError),
    PacketBridge(PacketBridgeError),
    HandoverIncomplete,
    InvalidHandoverTransition,
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebRTC transport error: {self:?}")
    }
}
impl std::error::Error for TransportError {}

struct TransportInner {
    state: TransportState,
    handover: HandoverState,
    queue: VecDeque<Vec<u8>>,
}

/// Desktop WebRTC transport boundary.
///
/// The GStreamer element is created here so runtime/plugin availability is
/// detected before a session is handed over from LAN. Data-channel signaling
/// and SDP/ICE callbacks are attached in the peer-connection adapter; the
/// bounded queue and generation validation live at this boundary.
pub struct WebRtcTransport {
    pub transport_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub wire_binding: WebRtcWireBinding,
    peer: WebRtcPeer,
    inner: Arc<Mutex<TransportInner>>,
}

enum WebRtcPeer {
    Element(gst::Element),
    Connection(Arc<PeerConnection>),
}

impl WebRtcTransport {
    pub fn new(
        device_id: impl Into<String>,
        session_id: u64,
        connection_generation: u64,
    ) -> Result<Self, TransportError> {
        Self::new_with_ice_config(
            device_id,
            session_id,
            connection_generation,
            &IceServerConfig::default(),
        )
    }

    pub fn new_with_ice_config(
        device_id: impl Into<String>,
        session_id: u64,
        connection_generation: u64,
        ice: &IceServerConfig,
    ) -> Result<Self, TransportError> {
        ice.validate()?;
        gst::init().map_err(|error| TransportError::Gstreamer(error.to_string()))?;
        let peer = gst::ElementFactory::make("webrtcbin")
            .name("desklink-webrtcbin")
            .build()
            .map_err(|error| TransportError::Gstreamer(error.to_string()))?;
        if let Some(stun) = ice.stun_servers.first() {
            peer.set_property("stun-server", stun);
        }
        if let Some(turn) = ice.turn_servers.first() {
            peer.set_property("turn-server", turn);
        }
        let device_id = device_id.into();
        let wire_binding = WebRtcWireBinding {
            sender_device_id: device_id.clone(),
            peer_device_id: device_id.clone(),
            session_id,
            generation: connection_generation,
        };
        Ok(Self {
            transport_id: format!("webrtc:{device_id}:{session_id}:{connection_generation}"),
            device_id,
            session_id,
            connection_generation,
            wire_binding,
            peer: WebRtcPeer::Element(peer),
            inner: Arc::new(Mutex::new(TransportInner {
                state: TransportState::New,
                handover: HandoverState::Negotiating,
                queue: VecDeque::new(),
            })),
        })
    }

    /// Wrap the peer that completed the live SDP/ICE negotiation. The
    /// `DeviceManager` owns this only after its control data channel opens.
    pub fn from_peer(wire_binding: WebRtcWireBinding, peer: Arc<PeerConnection>) -> Self {
        let device_id = wire_binding.peer_device_id.clone();
        Self {
            transport_id: format!(
                "webrtc:{device_id}:{}:{}",
                wire_binding.session_id, wire_binding.generation
            ),
            device_id,
            session_id: wire_binding.session_id,
            connection_generation: wire_binding.generation,
            wire_binding,
            peer: WebRtcPeer::Connection(peer),
            inner: Arc::new(Mutex::new(TransportInner {
                state: TransportState::Connected,
                handover: HandoverState::Negotiating,
                queue: VecDeque::new(),
            })),
        }
    }

    pub fn peer(&self) -> &gst::Element {
        match &self.peer {
            WebRtcPeer::Element(peer) => peer,
            WebRtcPeer::Connection(peer) => peer.element(),
        }
    }

    pub fn state(&self) -> TransportState {
        self.inner
            .lock()
            .map(|inner| inner.state)
            .unwrap_or(TransportState::Failed)
    }

    pub fn handover_state(&self) -> HandoverState {
        self.inner
            .lock()
            .map(|inner| inner.handover)
            .unwrap_or(HandoverState::Failed)
    }

    pub fn features_allowed(&self) -> bool {
        self.state() == TransportState::Connected && self.handover_state().features_allowed()
    }

    pub fn advance_handover(
        &self,
        message: HandoverMessage,
    ) -> Result<HandoverState, TransportError> {
        let mut inner = self.inner.lock().map_err(|_| TransportError::Closed)?;
        let next = inner
            .handover
            .receive(message)
            .map_err(|_| TransportError::InvalidHandoverTransition)?;
        inner.handover = next;
        Ok(next)
    }

    pub fn set_state(&self, state: TransportState) -> Result<(), TransportError> {
        let mut inner = self.inner.lock().map_err(|_| TransportError::Closed)?;
        if matches!(inner.state, TransportState::Closed | TransportState::Failed)
            && state != TransportState::Closed
        {
            return Err(TransportError::Closed);
        }
        inner.state = state;
        Ok(())
    }

    pub fn validate_channel(
        &self,
        label: &str,
        ordered: bool,
        max_retransmits: Option<u16>,
    ) -> Result<DataChannelSpec, TransportError> {
        DataChannelSpec::validate_remote(label, ordered, max_retransmits)
            .map_err(TransportError::Channel)
    }

    pub fn enqueue(&self, envelope: &MessageEnvelope) -> Result<(), TransportError> {
        envelope
            .validate(
                &self.wire_binding.sender_device_id,
                self.wire_binding.session_id,
                self.wire_binding.generation,
            )
            .map_err(TransportError::Envelope)?;
        let bytes = serde_json::to_vec(envelope)
            .map_err(|_| TransportError::Envelope(EnvelopeError::Malformed))?;
        let mut inner = self.inner.lock().map_err(|_| TransportError::Closed)?;
        if matches!(inner.state, TransportState::Closed | TransportState::Failed) {
            return Err(TransportError::Closed);
        }
        if inner.queue.len() >= MAX_QUEUED_MESSAGES {
            return Err(TransportError::Backpressure);
        }
        match &self.peer {
            WebRtcPeer::Connection(peer) => {
                let channel = DataChannelSpec::for_label(&envelope.channel).ok_or_else(|| {
                    TransportError::Envelope(EnvelopeError::UnknownChannel(
                        envelope.channel.clone(),
                    ))
                })?;
                peer.send_envelope(&channel, envelope)
                    .map_err(TransportError::Gstreamer)?;
            }
            WebRtcPeer::Element(_) => inner.queue.push_back(bytes),
        }
        Ok(())
    }

    pub fn enqueue_payload(
        &self,
        channel: &DataChannelSpec,
        message_type: &str,
        payload: &[u8],
        timestamp: i64,
    ) -> Result<(), TransportError> {
        let envelope = MessageEnvelope::new(
            &self.wire_binding.sender_device_id,
            self.wire_binding.session_id,
            self.wire_binding.generation,
            channel,
            message_type,
            payload,
            timestamp,
        )
        .map_err(TransportError::Envelope)?;
        self.enqueue(&envelope)
    }

    /// Sends an existing paired DeskLink feature packet on its fixed data
    /// channel. Pairing, SDP/ICE signaling, and payload packets are rejected
    /// by the bridge and must retain their dedicated paths.
    pub fn send_packet(
        &self,
        packet: &NetworkPacket,
        timestamp: i64,
    ) -> Result<(), TransportError> {
        if !self.features_allowed() {
            return Err(TransportError::HandoverIncomplete);
        }
        let envelope = encode_packet(&self.wire_binding, packet, timestamp)
            .map_err(TransportError::PacketBridge)?;
        self.enqueue(&envelope)
    }

    pub fn drain_queue(&self) -> Vec<Vec<u8>> {
        self.inner
            .lock()
            .map(|mut inner| inner.queue.drain(..).collect())
            .unwrap_or_default()
    }

    pub fn close(&self) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.state = TransportState::Closed;
            inner.queue.clear();
        }
        match &self.peer {
            WebRtcPeer::Element(peer) => {
                let _ = peer.set_state(gst::State::Null);
            }
            WebRtcPeer::Connection(peer) => peer.close(),
        }
    }
}

impl Drop for WebRtcTransport {
    fn drop(&mut self) {
        self.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::webrtc::channel::DataChannelSpec;

    #[test]
    fn generation_binding_is_checked_before_queueing() {
        let transport = WebRtcTransport::new("phone", 7, 2).unwrap();
        let envelope =
            MessageEnvelope::new("phone", 7, 1, &DataChannelSpec::CONTROL, "ping", b"x", 1)
                .unwrap();
        assert!(matches!(
            transport.enqueue(&envelope),
            Err(TransportError::Envelope(EnvelopeError::StaleGeneration))
        ));
    }

    #[test]
    fn paired_packet_is_enveloped_on_the_events_channel() {
        let transport = WebRtcTransport::new("phone", 7, 2).unwrap();
        let mut packet = NetworkPacket::new("desklink.clipboard");
        packet.set("content", "DeskLink");

        assert!(matches!(
            transport.send_packet(&packet, 1),
            Err(TransportError::HandoverIncomplete)
        ));
        transport
            .advance_handover(crate::device_links::webrtc::handover::HandoverMessage::Authenticated)
            .unwrap();
        transport
            .advance_handover(crate::device_links::webrtc::handover::HandoverMessage::Capabilities)
            .unwrap();
        transport
            .advance_handover(crate::device_links::webrtc::handover::HandoverMessage::FeatureReady)
            .unwrap();
        transport.set_state(TransportState::Connected).unwrap();
        transport.send_packet(&packet, 1).unwrap();
        let encoded = transport.drain_queue();
        assert_eq!(encoded.len(), 1);
        let envelope: MessageEnvelope = serde_json::from_slice(&encoded[0]).unwrap();
        assert_eq!(envelope.channel, DataChannelSpec::EVENTS.label);
    }

    #[test]
    fn ice_configuration_is_explicit_and_scheme_checked() {
        let config = IceServerConfig::parse(
            "stun://one.example:3478, stun://two.example:3478",
            "turns://user:credential@relay.example:5349",
        )
        .unwrap();
        assert_eq!(config.stun_servers.len(), 2);
        assert_eq!(config.turn_servers.len(), 1);
        assert!(IceServerConfig::parse("https://invalid.example", "").is_err());
        assert!(IceServerConfig::parse("", "stun://wrong.example").is_err());
    }
}
