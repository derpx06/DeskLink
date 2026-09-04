use std::collections::{HashSet, VecDeque};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use crate::device_links::pairing::PairingHandler;
use crate::device_links::webrtc::transfer_manager::WebRtcTransferManager;
use crate::device_links::webrtc::{DesktopWebRtcPeer, HandoverRuntime};

use super::SessionLink;

pub type SessionId = u64;
pub type ConnectionGeneration = u64;

/// Bounded, insertion-ordered set of identifiers already accepted once.
///
/// A plain `HashSet` was used before, and eviction removed whatever the
/// iterator yielded first — an arbitrary element, not the oldest. That could
/// discard a just-seen identifier and let its replay through. Android uses a
/// `LinkedHashSet` for the same job; this is the equivalent.
#[derive(Debug, Clone)]
pub struct ReplayCache {
    seen: HashSet<String>,
    order: VecDeque<String>,
    capacity: usize,
}

impl ReplayCache {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            seen: HashSet::new(),
            order: VecDeque::new(),
            capacity: capacity.max(1),
        }
    }

    /// Records an identifier, returning false if it has been seen before.
    pub fn accept(&mut self, id: String) -> bool {
        if !self.seen.insert(id.clone()) {
            return false;
        }
        self.order.push_back(id);
        while self.order.len() > self.capacity {
            if let Some(oldest) = self.order.pop_front() {
                self.seen.remove(&oldest);
            }
        }
        true
    }

    pub fn clear(&mut self) {
        self.seen.clear();
        self.order.clear();
    }
}

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
    /// Consecutive failed WebRTC rebuilds, used to pick the backoff rung.
    /// Deliberately separate from `reconnect_attempt`, which counts LAN
    /// reconnects and is reset by every bootstrap-socket replacement — sharing
    /// it dropped the WebRTC ladder back to its first rung whenever the socket
    /// flapped, which is exactly when the backoff matters most.
    pub webrtc_recovery_attempt: u32,
    /// Authentication and mutual feature-readiness state for the current
    /// WebRTC peer. This is reset whenever its transport generation changes.
    pub webrtc_handover: Option<HandoverRuntime>,
    pub seen_webrtc_request_ids: ReplayCache,
    /// Bounded idempotency cache for authenticated data-channel envelopes.
    pub seen_webrtc_message_ids: ReplayCache,
    pub cancellation: Arc<AtomicBool>,
    pub connected_at: Option<Instant>,
    pub last_disconnect_reason: Option<String>,
    pub reconnect_attempt: u32,
    pub reconnect_scheduled: bool,
    pub next_reconnect_at: Option<Instant>,
    /// When this side last told the peer it is not paired.
    ///
    /// Damped, because a peer that believes it is still paired will happily
    /// send clipboard updates at typing speed, and answering every one would
    /// turn a stale pairing into a packet storm. Android damps the identical
    /// notice at 5s (`PairingHandler.notifyPeerNotPaired`).
    pub last_not_paired_notice: Option<Instant>,
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

#[cfg(test)]
mod replay_cache_tests {
    use super::ReplayCache;

    #[test]
    fn an_identifier_is_accepted_exactly_once() {
        let mut cache = ReplayCache::with_capacity(8);
        assert!(cache.accept("a".to_string()));
        assert!(!cache.accept("a".to_string()));
    }

    /// Eviction must be oldest-first. A `HashSet` evicted an arbitrary element,
    /// which could discard a just-seen identifier and admit its replay.
    #[test]
    fn eviction_drops_the_oldest_identifier_first() {
        let mut cache = ReplayCache::with_capacity(3);
        for id in ["a", "b", "c"] {
            assert!(cache.accept(id.to_string()));
        }

        assert!(cache.accept("d".to_string()));

        // The three newest are still protected. Rejection does not re-insert,
        // so these checks leave the cache untouched.
        assert!(!cache.accept("b".to_string()));
        assert!(!cache.accept("c".to_string()));
        assert!(!cache.accept("d".to_string()));

        // "a" was the oldest and is the one that was dropped.
        assert!(cache.accept("a".to_string()));
    }

    #[test]
    fn clearing_forgets_every_identifier() {
        let mut cache = ReplayCache::with_capacity(4);
        cache.accept("a".to_string());
        cache.clear();
        assert!(cache.accept("a".to_string()));
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
