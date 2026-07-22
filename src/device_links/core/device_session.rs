use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::session_link::Link;
use crate::device_links::pairing::{PairState, PairingHandler};

pub type SessionId = u64;
pub type ConnectionGeneration = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    Connecting,
    Authenticating,
    Ready,
    Disconnecting,
    Disconnected,
    Reconnecting,
    Terminated,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceConnectionState {
    Discovered,
    Connecting,
    Authenticating,
    Unpaired,
    Paired,
    Reconnecting { attempt: u32 },
    Unreachable,
}

#[derive(Clone)]
pub struct DeviceSession {
    pub session_id: SessionId,
    pub connection_generation: ConnectionGeneration,
    pub device_id: String,
    pub pairing: Arc<Mutex<PairingHandler>>,
    pub state: SessionState,
    pub active_link: Option<Arc<Link>>,
    pub cancellation: Arc<AtomicBool>,
    pub connected_at: Option<Instant>,
    pub last_disconnect_reason: Option<String>,
    pub last_change: Instant,
    pub reconnect_attempt: u32,
    pub reconnect_scheduled: bool,
    pub next_reconnect_at: Option<Instant>,
}

impl DeviceSession {
    pub fn new(
        session_id: SessionId,
        device_id: impl Into<String>,
        initially_paired: bool,
    ) -> Self {
        Self {
            session_id,
            connection_generation: 0,
            device_id: device_id.into(),
            pairing: Arc::new(Mutex::new(PairingHandler::new(initially_paired))),
            state: SessionState::Disconnected,
            active_link: None,
            cancellation: Arc::new(AtomicBool::new(false)),
            connected_at: None,
            last_disconnect_reason: None,
            last_change: Instant::now(),
            reconnect_attempt: 0,
            reconnect_scheduled: false,
            next_reconnect_at: None,
        }
    }

    pub fn transition(&mut self, state: SessionState) {
        self.state = state;
        self.last_change = Instant::now();
    }

    pub fn pair_state(&self) -> PairState {
        self.pairing
            .lock()
            .map(|pairing| pairing.state)
            .unwrap_or(PairState::NotPaired)
    }

    pub fn is_terminated(&self) -> bool {
        self.state == SessionState::Terminated
    }

    pub fn reconnect_due(&self, now: Instant) -> bool {
        !self.is_terminated()
            && self.pair_state() == PairState::Paired
            && !self.reconnect_scheduled
            && self
                .next_reconnect_at
                .is_some_and(|next_attempt| next_attempt <= now)
    }

    pub fn schedule_reconnect(&mut self, now: Instant) {
        if self.is_terminated() || self.pair_state() != PairState::Paired {
            return;
        }
        self.reconnect_scheduled = false;
        self.next_reconnect_at = Some(now);
    }

    pub fn reconnect_failed(&mut self, now: Instant) {
        self.reconnect_attempt = self.reconnect_attempt.saturating_add(1);
        let seconds = (1u64 << self.reconnect_attempt.min(6)).min(60);
        self.reconnect_scheduled = false;
        self.next_reconnect_at = Some(now + Duration::from_secs(seconds));
        self.transition(SessionState::Reconnecting);
    }

    pub fn reconnect_succeeded(&mut self) {
        self.reconnect_attempt = 0;
        self.reconnect_scheduled = false;
        self.next_reconnect_at = None;
    }

    pub fn clear_reconnect(&mut self) {
        self.reconnect_scheduled = false;
        self.next_reconnect_at = None;
        self.reconnect_attempt = 0;
    }
}
