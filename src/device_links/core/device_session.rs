use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use crate::device_links::pairing::PairingHandler;

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
