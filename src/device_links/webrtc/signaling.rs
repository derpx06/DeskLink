use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::device_links::packet::NetworkPacket;
use crate::protocol::desklink_v9::PACKET_TYPE_WEBRTC_SIGNAL_V1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SignalingMessage {
    pub signaling_version: u8,
    pub request_id: String,
    pub session_attempt_id: String,
    pub from_device_id: String,
    pub to_device_id: String,
    pub timestamp: i64,
    pub message_type: SignalingMessageType,
    pub payload: serde_json::Value,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SignalingMessageType {
    Offer,
    Answer,
    IceCandidate,
    EndOfCandidates,
    IceRestart,
    Close,
}

impl SignalingMessage {
    pub fn validate_for(
        &self,
        local_device_id: &str,
        now_millis: i64,
    ) -> Result<(), SignalingError> {
        if self.signaling_version != 1 {
            return Err(SignalingError::UnsupportedVersion(self.signaling_version));
        }
        if self.to_device_id != local_device_id {
            return Err(SignalingError::WrongDestination);
        }
        if self.from_device_id.is_empty()
            || self.request_id.is_empty()
            || self.session_attempt_id.is_empty()
            || self.signature.is_empty()
        {
            return Err(SignalingError::Malformed);
        }
        if (now_millis - self.timestamp).unsigned_abs() > 5 * 60 * 1000 {
            return Err(SignalingError::Expired);
        }
        Ok(())
    }

    pub fn to_legacy_packet(&self) -> Result<NetworkPacket, serde_json::Error> {
        let mut packet = NetworkPacket::new(PACKET_TYPE_WEBRTC_SIGNAL_V1);
        packet.body = serde_json::to_value(self)?
            .as_object()
            .cloned()
            .unwrap_or_default();
        Ok(packet)
    }

    pub fn from_legacy_packet(packet: &NetworkPacket) -> Result<Self, SignalingError> {
        if packet.packet_type != PACKET_TYPE_WEBRTC_SIGNAL_V1 {
            return Err(SignalingError::WrongPacketType);
        }
        serde_json::from_value(serde_json::Value::Object(packet.body.clone()))
            .map_err(|_| SignalingError::Malformed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalingError {
    UnsupportedVersion(u8),
    WrongDestination,
    WrongPacketType,
    Expired,
    Replay,
    Malformed,
    Transport(String),
}

impl std::fmt::Display for SignalingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebRTC signaling error: {self:?}")
    }
}

impl std::error::Error for SignalingError {}

pub trait SignalingTransport: Send + Sync {
    fn send(&self, message: SignalingMessage) -> Result<(), SignalingError>;
    fn close(&self);
}

/// Adapter for the already-authenticated LAN control link. It carries only
/// signed SDP/ICE metadata; feature data remains on the selected transport.
pub struct LocalLanSignalingTransport {
    sender: Arc<dyn Fn(NetworkPacket) -> Result<(), SignalingError> + Send + Sync>,
    replay_guard: ReplayGuard,
}

impl LocalLanSignalingTransport {
    pub fn new<F>(sender: F) -> Self
    where
        F: Fn(NetworkPacket) -> Result<(), SignalingError> + Send + Sync + 'static,
    {
        Self {
            sender: Arc::new(sender),
            replay_guard: ReplayGuard::default(),
        }
    }
}

impl SignalingTransport for LocalLanSignalingTransport {
    fn send(&self, message: SignalingMessage) -> Result<(), SignalingError> {
        self.replay_guard.accept(&message.request_id)?;
        let packet = message
            .to_legacy_packet()
            .map_err(|_| SignalingError::Malformed)?;
        (self.sender)(packet)
    }

    fn close(&self) {}
}

/// A small replay filter shared by LAN and cloud signaling adapters.
#[derive(Clone, Default)]
pub struct ReplayGuard {
    request_ids: Arc<Mutex<HashSet<String>>>,
}

impl ReplayGuard {
    pub fn accept(&self, request_id: &str) -> Result<(), SignalingError> {
        let mut ids = self
            .request_ids
            .lock()
            .map_err(|_| SignalingError::Transport("replay guard poisoned".into()))?;
        if !ids.insert(request_id.to_string()) {
            return Err(SignalingError::Replay);
        }
        if ids.len() > 4096 {
            let retained = ids.iter().take(2048).cloned().collect::<HashSet<_>>();
            *ids = retained;
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct InMemorySignalingTransport {
    sent: Arc<Mutex<Vec<SignalingMessage>>>,
    closed: Arc<Mutex<bool>>,
}

impl InMemorySignalingTransport {
    pub fn messages(&self) -> Vec<SignalingMessage> {
        self.sent
            .lock()
            .map(|messages| messages.clone())
            .unwrap_or_default()
    }
}

impl SignalingTransport for InMemorySignalingTransport {
    fn send(&self, message: SignalingMessage) -> Result<(), SignalingError> {
        if self
            .closed
            .lock()
            .map_err(|_| SignalingError::Transport("signaling lock poisoned".into()))?
            .to_owned()
        {
            return Err(SignalingError::Transport(
                "signaling transport is closed".into(),
            ));
        }
        self.sent
            .lock()
            .map_err(|_| SignalingError::Transport("signaling lock poisoned".into()))?
            .push(message);
        Ok(())
    }

    fn close(&self) {
        if let Ok(mut closed) = self.closed.lock() {
            *closed = true;
        }
    }
}

/// Configurable WSS signaling client. No endpoint is enabled by default.
pub struct CloudWebSocketSignaling {
    endpoint: String,
    socket: Mutex<
        Option<tungstenite::WebSocket<tungstenite::stream::MaybeTlsStream<std::net::TcpStream>>>,
    >,
}

impl CloudWebSocketSignaling {
    pub fn connect(endpoint: impl Into<String>) -> Result<Self, SignalingError> {
        let endpoint = endpoint.into();
        if !endpoint.starts_with("wss://") {
            return Err(SignalingError::Transport(
                "signaling endpoint must use wss://".into(),
            ));
        }
        let (socket, _) = tungstenite::connect(endpoint.as_str())
            .map_err(|error| SignalingError::Transport(error.to_string()))?;
        Ok(Self {
            endpoint,
            socket: Mutex::new(Some(socket)),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }
}

impl SignalingTransport for CloudWebSocketSignaling {
    fn send(&self, message: SignalingMessage) -> Result<(), SignalingError> {
        let bytes = serde_json::to_string(&message).map_err(|_| SignalingError::Malformed)?;
        let mut socket = self
            .socket
            .lock()
            .map_err(|_| SignalingError::Transport("socket lock poisoned".into()))?;
        let socket = socket
            .as_mut()
            .ok_or_else(|| SignalingError::Transport("signaling socket is closed".into()))?;
        socket
            .send(tungstenite::Message::Text(bytes.into()))
            .map_err(|error| SignalingError::Transport(error.to_string()))
    }

    fn close(&self) {
        if let Ok(mut socket) = self.socket.lock() {
            if let Some(socket) = socket.as_mut() {
                let _ = socket.close(None);
            }
            *socket = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message() -> SignalingMessage {
        SignalingMessage {
            signaling_version: 1,
            request_id: "req-1".into(),
            session_attempt_id: "attempt-1".into(),
            from_device_id: "desktop".into(),
            to_device_id: "phone".into(),
            timestamp: 1000,
            message_type: SignalingMessageType::Offer,
            payload: serde_json::json!({"sdp":"offer"}),
            signature: "signed".into(),
        }
    }

    #[test]
    fn signaling_round_trip_and_replay_guard() {
        let packet = message().to_legacy_packet().unwrap();
        let decoded = SignalingMessage::from_legacy_packet(&packet).unwrap();
        assert_eq!(decoded, message());
        let guard = ReplayGuard::default();
        guard.accept("req-1").unwrap();
        assert_eq!(guard.accept("req-1"), Err(SignalingError::Replay));
    }

    #[test]
    fn destination_and_expiry_are_rejected() {
        assert_eq!(
            message().validate_for("other", 1000),
            Err(SignalingError::WrongDestination)
        );
        assert_eq!(
            message().validate_for("phone", 400_001),
            Err(SignalingError::Expired)
        );
    }
}
