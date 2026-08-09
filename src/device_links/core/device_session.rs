use std::collections::HashSet;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use crate::device_links::pairing::PairingHandler;
use crate::device_links::webrtc::transfer_manager::WebRtcTransferManager;
use crate::device_links::webrtc::{DesktopWebRtcPeer, HandoverRuntime};

use super::SessionLink;

pub type SessionId = u64;
pub type ConnectionGeneration = u64;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    Authenticating,
    Ready,
    Disconnecting,
    Disconnected,
    Reconnecting,
    Terminated,
}

pub struct DeviceSession {
    pub session_id: SessionId,
    pub connection_generation: ConnectionGeneration,
    pub device_id: String,
    pub pairing: PairingHandler,
    pub state: SessionState,
    pub active_link: Option<Arc<SessionLink>>,
    /// The only WebRTC peer allowed to carry traffic for this logical session.
    /// It is replaced together with the bootstrap link generation.
    pub active_webrtc: Option<Arc<DesktopWebRtcPeer>>,
    /// Checkpoint owner shared by the peer worker and desktop commands. It is
    /// cleared with the peer so an old generation cannot resume a transfer on
    /// a replacement connection.
    pub transfer_manager: Option<Arc<WebRtcTransferManager>>,
    pub webrtc_attempt_id: Option<String>,
    /// Authentication and mutual feature-readiness state for the current
    /// WebRTC peer. This is reset whenever its transport generation changes.
    pub webrtc_handover: Option<HandoverRuntime>,
    pub seen_webrtc_request_ids: HashSet<String>,
    /// Bounded idempotency cache for authenticated data-channel envelopes.
    pub seen_webrtc_message_ids: HashSet<String>,
    pub cancellation: Arc<AtomicBool>,
    pub connected_at: Option<Instant>,
    pub last_disconnect_reason: Option<String>,
    pub reconnect_attempt: u32,
    pub reconnect_scheduled: bool,
    pub next_reconnect_at: Option<Instant>,
}

impl DeviceSession {
    pub fn binding(&self) -> Option<SessionBinding> {
        self.active_link.as_ref().map(|link| SessionBinding {
            device_id: self.device_id.clone(),
            session_id: self.session_id,
            generation: self.connection_generation,
            link: Arc::clone(link),
            cancellation: Arc::clone(&self.cancellation),
        })
    }

    /// Drops all state that is cryptographically bound to a particular paired
    /// relationship.  Pairing changes must never leave an old WebRTC peer or
    /// handover attempt behind: a later re-pair would otherwise see a peer as
    /// already installed and skip negotiation.
    pub fn clear_webrtc_state(&mut self) -> Option<Arc<DesktopWebRtcPeer>> {
        self.webrtc_attempt_id = None;
        self.webrtc_handover = None;
        self.transfer_manager = None;
        self.seen_webrtc_request_ids.clear();
        self.seen_webrtc_message_ids.clear();
        self.active_webrtc.take()
    }
}

#[derive(Clone)]
pub struct SessionBinding {
    pub device_id: String,
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
    pub link: Arc<SessionLink>,
    pub cancellation: Arc<AtomicBool>,
}

impl std::fmt::Debug for SessionBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionBinding")
            .field("device_id", &self.device_id)
            .field("session_id", &self.session_id)
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}
