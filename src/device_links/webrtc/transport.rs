use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gst::prelude::*;
use gstreamer as gst;

use super::channel::{ChannelError, DataChannelSpec};
use super::envelope::{EnvelopeError, MessageEnvelope};
use super::peer_connection::PeerConnection;

pub const MAX_QUEUED_MESSAGES: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IceServerConfig {
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<String>,
}

impl IceServerConfig {
    pub fn validate(&self) -> Result<(), TransportError> {
        for uri in self.stun_servers.iter().chain(self.turn_servers.iter()) {
            if !(uri.starts_with("stun://")
                || uri.starts_with("stuns://")
                || uri.starts_with("turn://")
                || uri.starts_with("turns://"))
            {
                return Err(TransportError::Gstreamer(format!(
                    "unsupported ICE server URI: {uri}"
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
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebRTC transport error: {self:?}")
    }
}
impl std::error::Error for TransportError {}

struct TransportInner {
    state: TransportState,
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
        Ok(Self {
            transport_id: format!("webrtc:{device_id}:{session_id}:{connection_generation}"),
            device_id,
            session_id,
            connection_generation,
            peer: WebRtcPeer::Element(peer),
            inner: Arc::new(Mutex::new(TransportInner {
                state: TransportState::New,
                queue: VecDeque::new(),
            })),
        })
    }

    /// Wrap the peer that completed the live SDP/ICE negotiation. The
    /// `DeviceManager` owns this only after its control data channel opens.
    pub fn from_peer(
        device_id: impl Into<String>,
        session_id: u64,
        connection_generation: u64,
        peer: Arc<PeerConnection>,
    ) -> Self {
        let device_id = device_id.into();
        Self {
            transport_id: format!("webrtc:{device_id}:{session_id}:{connection_generation}"),
            device_id,
            session_id,
            connection_generation,
            peer: WebRtcPeer::Connection(peer),
            inner: Arc::new(Mutex::new(TransportInner {
                state: TransportState::Connected,
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
            .validate(&self.device_id, self.session_id, self.connection_generation)
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
        inner.queue.push_back(bytes);
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
            &self.device_id,
            self.session_id,
            self.connection_generation,
            channel,
            message_type,
            payload,
            timestamp,
        )
        .map_err(TransportError::Envelope)?;
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
}
