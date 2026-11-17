//! The session registry ([`DeviceManager`]): the sole owner of live sessions.
//! Registration is generation-based: a newly authenticated transport
//! supersedes the old one, and callbacks from a replaced generation become
//! stale rather than erroring.
//!
//! Layout: this file holds the registry, registration, adoption, bindings,
//! liveness and reconnect leases; `outcomes` the result and status types,
//! `disconnects` and `webrtc_state` its disconnect paths and WebRTC state
//! (`impl DeviceManager` in each). No cycles. May depend on: `core` (session,
//! link), `webrtc`, `pairing`, `packet`, `connection_log`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::device_links::pairing::PairingHandler;
use crate::device_links::webrtc::transfer_manager::WebRtcTransferManager;
use crate::device_links::webrtc::{DesktopWebRtcPeer, HandoverRuntime, WebRtcWireBinding};

use super::LinkTransport;
use super::{
    ConnectionGeneration, DeviceSession, ReplayCache, SessionBinding, SessionId, SessionLink,
    SessionState,
};

mod disconnects;
mod outcomes;
mod webrtc_state;

// The crate-facing surface (everything else stays inside).
pub use outcomes::{
    Adoption, DeviceConnection, DisconnectResult, FeatureTransportSnapshot, Installed,
    LivenessTick, ReconnectLease, RegistrationResult, SessionError,
};

/// Identifiers retained per attempt for replay rejection.
const REPLAY_CACHE_CAPACITY: usize = 4096;

pub struct DeviceManager {
    next_session_id: AtomicU64,
    sessions: Mutex<HashMap<String, DeviceSession>>,
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            next_session_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Whether to tell the peer that this side does not consider it paired,
    /// recording the notice so the next one is damped.
    ///
    /// This is the self-healing half of unpairing, and it is why DeskLink needs
    /// no durable "owes an unpair" queue. When a peer misses a `pair:false` —
    /// because it was offline, or the send failed — it keeps behaving as paired
    /// and sends a clipboard update or a notification. That packet is refused,
    /// this fires, and the two sides converge on first use. Upstream KDE
    /// Connect has done it this way for years (`core/device.cpp`); the desktop
    /// was the only side missing it, so a one-sided unpair left the phone
    /// believing it was paired indefinitely.
    ///
    /// Refuses unless the session really is `NotPaired`, so it can never be
    /// reached through a rejection that was about something else — a WebRTC
    /// transport that is merely not ready yet, say — and turned into a
    /// spurious unpair.
    pub fn claim_not_paired_notice(
        &self,
        binding: &SessionBinding,
        interval: std::time::Duration,
    ) -> bool {
        let Ok(mut sessions) = self.sessions.lock() else {
            return false;
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return false;
        };
        if session.pairing.state != crate::device_links::pairing::PairState::NotPaired {
            return false;
        }
        let now = Instant::now();
        if session
            .last_not_paired_notice
            .is_some_and(|last| now.duration_since(last) < interval)
        {
            return false;
        }
        session.last_not_paired_notice = Some(now);
        true
    }

    /// Records that a packet arrived from this peer — any packet, not only a
    /// heartbeat. This is what makes a busy link cost nothing to keep alive.
    pub fn note_inbound(&self, device_id: &str) {
        if let Some(session) = self.reconciler_sessions().get_mut(device_id) {
            session.last_inbound_at = Instant::now();
        }
    }

    /// How long this peer has been silent, if there is a session for it.
    pub fn silence(&self, device_id: &str) -> Option<std::time::Duration> {
        self.reconciler_sessions()
            .get(device_id)
            .map(|session| session.last_inbound_at.elapsed())
    }

    /// One liveness pass over every session.
    ///
    /// Returns the links to probe and the links whose peer has been silent long
    /// enough to be declared gone. Both are bindings rather than ids so the
    /// caller can send and disconnect without re-taking the lock — sends must
    /// never happen under it.
    ///
    /// `now` is a parameter for the same reason `expire_pairing`'s is: it makes
    /// a thirty-second budget testable in a microsecond instead of thirty
    /// seconds, and the ground rules forbid a test that sleeps.
    pub fn liveness_tick(
        &self,
        now: Instant,
        idle: impl Fn(&str) -> std::time::Duration,
        dead: impl Fn(&str) -> std::time::Duration,
    ) -> LivenessTick {
        let mut tick = LivenessTick::default();
        for session in self.reconciler_sessions().values() {
            if session.state == SessionState::Terminated {
                continue;
            }
            let Some(binding) = session.binding() else {
                continue;
            };
            if binding.link.is_closed() {
                continue;
            }
            let silence = now.saturating_duration_since(session.last_inbound_at);
            if silence >= dead(&binding.device_id) {
                tick.dead.push(binding);
            } else if silence >= idle(&binding.device_id) {
                tick.probe.push(binding);
            }
        }
        tick
    }

    /// Links to phones that are not paired and are not pairing, open for
    /// `max` or longer: a stranger's socket that only idles holds a reader
    /// thread and a slot for ever, since its heartbeats keep it "alive".
    ///
    /// A phone that is paired in the trust store is never swept, whatever its
    /// session says: a paired phone redialling after a sleep or a network
    /// change must not be cut by this.
    pub fn idle_unpaired_links(
        &self,
        now: Instant,
        max: std::time::Duration,
        is_trusted: impl Fn(&str) -> bool,
    ) -> Vec<SessionBinding> {
        use crate::device_links::pairing::PairState;
        self.reconciler_sessions()
            .values()
            .filter(|session| {
                session.state != SessionState::Terminated
                    && !is_trusted(&session.device_id)
                    && session.pairing.state == PairState::NotPaired
                    && now.saturating_duration_since(session.linked_at) >= max
            })
            .filter_map(|session| session.binding())
            .filter(|binding| !binding.link.is_closed())
            .collect()
    }

    /// Adopt a replacement bootstrap socket into the existing session.
    ///
    /// Returns the *unchanged* binding when adopted, so the caller starts a
    /// reader on the same generation the WebRTC peer and handover are already
    /// bound to. Returns `None` when the session must go through
    /// [`DeviceManager::register_link`] instead.
    ///
    /// Two conditions, both load-bearing:
    ///
    /// * **A feature-ready WebRTC transport.** Without one there is nothing to
    ///   preserve, and a full re-registration is the correct, simpler answer.
    /// * **A closed incumbent.** A live socket still owns a reader thread
    ///   blocked inside `read_byte`; swapping underneath it would leave two
    ///   readers framing one stream, which is a worse failure than the rebuild
    ///   this avoids. `establish_secure_link`'s refresh guard already discards
    ///   the newcomer while the incumbent is both open and answering, so the
    ///   only case that reaches here is the one this exists for.
    ///
    /// The incumbent may still be open (it went silent: the caller only gets
    /// here when it is closed or not answering). It is swapped out in place —
    /// same link, same generation, so the WebRTC peer bound to them lives —
    /// and returned for the caller to shut down once this lock is released.
    /// Its reader sees the link's epoch change and leaves without reporting a
    /// disconnect.
    pub fn adopt_or_keep(
        &self,
        device_id: &str,
        transport: &Arc<dyn LinkTransport>,
        certificate_pem: &str,
        answering_within: std::time::Duration,
    ) -> Adoption {
        let Ok(mut sessions) = self.sessions.lock() else {
            return Adoption::Register;
        };
        let Some(session) = sessions.get_mut(device_id) else {
            return Adoption::Register;
        };
        if session.state == SessionState::Terminated
            || !session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready)
        {
            return Adoption::Register;
        }
        let Some(link) = session.active_link.clone() else {
            return Adoption::Register;
        };
        // The swap keeps the link's identity — its certificate among it — so
        // only a socket that authenticated with that same certificate may
        // carry it.
        if link.certificate_pem != certificate_pem {
            return Adoption::Refuse;
        }
        // Decided under the same lock as the swap: two sockets arriving
        // together cannot both be adopted, nor one kept and one adopted.
        if !link.is_closed() && session.last_inbound_at.elapsed() < answering_within {
            return Adoption::Keep;
        }
        let replaced = link.adopt(Arc::clone(transport));
        let epoch = link.epoch();
        session.state = SessionState::Ready;
        // A working socket is fresh evidence the peer is reachable, so a
        // recovery ladder exhausted while it was away starts over.
        session.webrtc_recovery_attempt = 0;
        session.last_disconnect_reason = None;
        // The adopted socket has, by definition, just completed a handshake.
        // Carrying the old one's staleness clock over would have the liveness
        // tick bury it before its first packet.
        session.last_inbound_at = Instant::now();
        match session.binding() {
            Some(binding) => Adoption::Adopted {
                binding,
                replaced,
                epoch,
            },
            None => Adoption::Register,
        }
    }

    /// Whether `device_id`'s link is open, carries `certificate_pem` and was
    /// heard from within `within`: a link that needs no replacement.
    pub fn answering_link(
        &self,
        device_id: &str,
        certificate_pem: &str,
        within: std::time::Duration,
    ) -> bool {
        let Ok(sessions) = self.sessions.lock() else {
            return false;
        };
        sessions.get(device_id).is_some_and(|session| {
            session.state != SessionState::Terminated
                && session.last_inbound_at.elapsed() < within
                && session.active_link.as_ref().is_some_and(|link| {
                    !link.is_closed() && link.certificate_pem == certificate_pem
                })
        })
    }

    /// The incumbent link's dialer and age, if it is open: what the link
    /// race rule compares a newly authenticated link with.
    pub fn incumbent_link(&self, device_id: &str) -> Option<(bool, std::time::Duration)> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(device_id)?;
        if session.state == SessionState::Terminated {
            return None;
        }
        let link = session.active_link.as_ref()?;
        (!link.is_closed()).then(|| (link.was_dialed_by_us(), link.age()))
    }

    pub fn register_link(
        &self,
        device_id: String,
        link: SessionLink,
        initially_paired: bool,
    ) -> Result<RegistrationResult, SessionError> {
        let link = Arc::new(link);
        let result = {
            let mut sessions = self.sessions.lock().map_err(|_| SessionError::Poisoned)?;
            let now = Instant::now();

            if let Some(session) = sessions.get_mut(&device_id) {
                if session.state == SessionState::Terminated {
                    return Err(SessionError::TerminatedSession);
                }
                let replaced_generation = Some(session.connection_generation);
                let replaced_link = session.active_link.replace(Arc::clone(&link));
                let replaced_webrtc = session.active_webrtc.take();

                // Every accepted connection after the first represents a new
                // transport generation, including a reconnect after
                // `disconnect_if_current` removed the old link.  Reusing the
                // cancelled token from the disconnected generation makes the
                // new packet reader stale before it starts and prevents
                // WebRTC bootstrap from ever running.
                session.cancellation.store(true, Ordering::Release);
                session.connection_generation = session.connection_generation.saturating_add(1);
                session.cancellation = Arc::new(AtomicBool::new(false));
                session.clear_webrtc_state();

                // A pairing negotiation belongs to the link that started it.
                //
                // The verification key the user is comparing is derived from
                // *that* link's certificate, and nothing recomputes it when a
                // link is replaced. So letting a new link inherit `Requested`
                // or `RequestedByPeer` lets whoever opened it collect an
                // acceptance the user granted to somebody else, and have its
                // own certificate written to the trust store — while both
                // screens still show the honest peer's key. That is
                // CVE-2020-26164, and on a LAN the device id it needs is
                // broadcast in cleartext.
                //
                // Abandoning is the safe direction: the user re-initiates and
                // compares a key that belongs to the link in front of them.
                if matches!(
                    session.pairing.state,
                    crate::device_links::pairing::PairState::Requested
                        | crate::device_links::pairing::PairState::RequestedByPeer
                ) {
                    session.pairing = PairingHandler::new(false);
                }

                // The trust store is the authority on every new link, in both
                // directions. Upgrading only meant a phone unpaired while it
                // was offline came back `Paired` on its next connection — and
                // with no stored certificate to pin, *any* certificate whose
                // CN matched its broadcast id was accepted as that phone.
                let paired =
                    session.pairing.state == crate::device_links::pairing::PairState::Paired;
                if initially_paired != paired
                    && !matches!(
                        session.pairing.state,
                        crate::device_links::pairing::PairState::Requested
                            | crate::device_links::pairing::PairState::RequestedByPeer
                    )
                {
                    session.pairing = PairingHandler::new(initially_paired);
                }
                session.state = SessionState::Ready;
                session.connected_at = Some(now);
                session.last_disconnect_reason = None;
                session.reconnect_attempt = 0;
                session.webrtc_recovery_attempt = 0;
                session.reconnect_scheduled = false;
                session.next_reconnect_at = None;
                // A new link has, by definition, just been heard from. Without
                // this the staleness clock would carry over from the socket
                // this one replaced and immediately declare the fresh link dead.
                session.last_inbound_at = now;
                session.linked_at = now;
                let binding = session
                    .binding()
                    .expect("registered session always has a link");
                RegistrationResult {
                    binding,
                    replaced_link,
                    replaced_generation,
                    replaced_webrtc,
                }
            } else {
                let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
                let session = DeviceSession {
                    session_id,
                    connection_generation: 1,
                    device_id: device_id.clone(),
                    pairing: PairingHandler::new(initially_paired),
                    state: SessionState::Ready,
                    active_link: Some(Arc::clone(&link)),
                    active_webrtc: None,
                    transfer_manager: None,
                    webrtc_attempt_id: None,
                    webrtc_handover: None,
                    webrtc_recovery_attempt: 0,
                    seen_webrtc_request_ids: ReplayCache::with_capacity(REPLAY_CACHE_CAPACITY),
                    seen_signal_request_ids: ReplayCache::with_capacity(REPLAY_CACHE_CAPACITY),
                    seen_webrtc_message_ids: ReplayCache::with_capacity(REPLAY_CACHE_CAPACITY),
                    cancellation: Arc::new(AtomicBool::new(false)),
                    connected_at: Some(now),
                    last_disconnect_reason: None,
                    reconnect_attempt: 0,
                    reconnect_scheduled: false,
                    next_reconnect_at: None,
                    last_not_paired_notice: None,
                    pending_features: Default::default(),
                    last_inbound_at: now,
                    linked_at: now,
                };
                let binding = session.binding().expect("new session always has a link");
                sessions.insert(device_id, session);
                RegistrationResult {
                    binding,
                    replaced_link: None,
                    replaced_generation: None,
                    replaced_webrtc: None,
                }
            }
        };
        if let Some(replaced_link) = &result.replaced_link {
            replaced_link.close();
        }
        Ok(result)
    }

    pub fn current_binding(&self, device_id: &str) -> Option<SessionBinding> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(device_id)?;
        if session.state == SessionState::Terminated {
            return None;
        }
        session.binding()
    }

    pub fn current_bindings(&self) -> Vec<SessionBinding> {
        self.sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .filter(|session| session.state != SessionState::Terminated)
                    .filter_map(DeviceSession::binding)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Whether `device_id` has a paired session whose WebRTC feature
    /// transport is ready: what an unpaired link claiming its id must never
    /// replace.
    pub fn holds_paired_transport(&self, device_id: &str) -> bool {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| {
                let session = sessions.get(device_id)?;
                Some(
                    session.state != SessionState::Terminated
                        && session.pairing.state == crate::device_links::pairing::PairState::Paired
                        && session
                            .webrtc_handover
                            .as_ref()
                            .is_some_and(HandoverRuntime::features_ready),
                )
            })
            .unwrap_or(false)
    }

    /// Whether `instance_id` shows the peer restarted since its current link
    /// was established. Unknown on either side (older builds) means no.
    pub fn peer_restarted(&self, device_id: &str, instance_id: Option<&str>) -> bool {
        let Some(instance_id) = instance_id else {
            return false;
        };
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| {
                sessions.get(device_id).and_then(|session| {
                    session
                        .active_link
                        .as_ref()
                        .and_then(|link| link.info.instance_id.clone())
                })
            })
            .is_some_and(|current| current != instance_id)
    }

    /// Whether this device still has a usable bootstrap socket.
    ///
    /// A session deliberately keeps its link installed after the socket closes
    /// while an authenticated WebRTC transport survives, because the WebRTC
    /// callbacks are generation-bound to a binding holding that exact link. So
    /// "a binding exists" does not mean "a connection exists", and discovery
    /// must not use the former to decide whether to reconnect: doing so refused
    /// every replacement connection for a device whose socket had died, and if
    /// its WebRTC peer later failed as well the device was wedged offline until
    /// the application restarted.
    /// Whether a replacement bootstrap connection would be accepted at all.
    ///
    /// `attach_bootstrap_link` discards a new socket while the current one is
    /// live and answering, so discovery must not keep dialling then: that was
    /// a full TCP connect, TLS handshake and certificate validation per beacon,
    /// thrown away.
    ///
    /// A surviving WebRTC peer no longer counts: without its bootstrap socket
    /// the desk has no way to thaw a phone its OS froze (screen off), and a
    /// replacement is adopted into the session without disturbing the peer
    /// (`attach_bootstrap_link`). Only a live socket that is still answering
    /// makes a new one pointless.
    pub fn wants_bootstrap_connection(&self, device_id: &str) -> bool {
        !self.has_live_bootstrap_link(device_id)
    }

    /// Whether `device_id`'s live bootstrap link is a relay link (it has no
    /// local address, unlike a LAN socket).
    pub fn bootstrap_is_relay(&self, device_id: &str) -> bool {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| {
                let session = sessions.get(device_id)?;
                let link = session.active_link.as_ref()?;
                Some(
                    session.state != SessionState::Terminated
                        && !link.is_closed()
                        && link.local_ip().is_none(),
                )
            })
            .unwrap_or(false)
    }

    /// `device_id`'s open link, when nothing has come from the peer for
    /// `quiet_for` or longer.
    pub fn silent_link(
        &self,
        device_id: &str,
        quiet_for: std::time::Duration,
    ) -> Option<SessionBinding> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(device_id)?;
        if session.state == SessionState::Terminated
            || session.last_inbound_at.elapsed() < quiet_for
        {
            return None;
        }
        session
            .binding()
            .filter(|binding| !binding.link.is_closed())
    }

    pub fn has_live_bootstrap_link(&self, device_id: &str) -> bool {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| {
                sessions.get(device_id).map(|session| {
                    session.state != SessionState::Terminated
                        && session
                            .active_link
                            .as_ref()
                            .is_some_and(|link| !link.is_closed())
                })
            })
            .unwrap_or(false)
    }

    /// Returns whether a device already has a mutually feature-ready WebRTC
    /// peer.  A later LAN socket is only a bootstrap/signaling candidate in
    /// that state and must not replace the live feature generation.
    #[cfg(test)]
    pub fn has_ready_webrtc(&self, device_id: &str) -> bool {
        self.sessions
            .lock()
            .ok()
            .and_then(|sessions| {
                sessions.get(device_id).map(|session| {
                    session
                        .webrtc_handover
                        .as_ref()
                        .is_some_and(HandoverRuntime::features_ready)
                })
            })
            .unwrap_or(false)
    }

    /// Takes a generation-checked, immutable view of the current transport.
    /// The snapshot is intentionally not a capability to mutate session
    /// state.  A caller must re-check [`Self::is_current`] immediately before
    /// I/O; the wire generation then protects the receiver if replacement
    /// races with that send.
    pub fn feature_transport_snapshot(
        &self,
        binding: &SessionBinding,
    ) -> Result<FeatureTransportSnapshot, SessionError> {
        self.with_session(binding, |session| {
            let (web_rtc_peer, transfer_manager, web_rtc_wire, web_rtc_ready) = match (
                session.active_webrtc.as_ref(),
                session.webrtc_handover.as_ref(),
            ) {
                (Some(peer), Some(handover)) => (
                    Some(Arc::clone(peer)),
                    session.transfer_manager.as_ref().cloned(),
                    Some(handover.wire_binding.clone()),
                    handover.features_ready(),
                ),
                _ => (None, None, None, false),
            };
            FeatureTransportSnapshot {
                binding: binding.clone(),
                paired: session.pairing.state == crate::device_links::pairing::PairState::Paired,
                web_rtc_peer,
                transfer_manager,
                web_rtc_wire,
                web_rtc_ready,
            }
        })
    }

    /// Whether a newer generation (or session) than `binding`'s now exists for
    /// its device: a redial registered a new link after `binding` ended.
    pub fn generation_replaced(&self, binding: &SessionBinding) -> bool {
        let Ok(sessions) = self.sessions.lock() else {
            return false;
        };
        sessions.get(&binding.device_id).is_some_and(|session| {
            session.state != SessionState::Terminated
                && (session.session_id != binding.session_id
                    || session.connection_generation > binding.generation)
        })
    }

    /// A sign of a paired phone with no link (the session was cut): the
    /// recovery ladder starts over. Returns the attempt it had reached, or
    /// `None` for an unknown or unpaired device.
    pub fn reset_recovery(&self, device_id: &str) -> Option<u32> {
        let mut sessions = self.sessions.lock().ok()?;
        let session = sessions.get_mut(device_id)?;
        if session.state == SessionState::Terminated
            || session.pairing.state != crate::device_links::pairing::PairState::Paired
        {
            return None;
        }
        Some(std::mem::take(&mut session.webrtc_recovery_attempt))
    }

    pub fn is_current(&self, binding: &SessionBinding) -> bool {
        if binding.cancellation.load(Ordering::Acquire) {
            return false;
        }
        let Ok(sessions) = self.sessions.lock() else {
            return false;
        };
        let Some(session) = sessions.get(&binding.device_id) else {
            return false;
        };
        session.state != SessionState::Terminated
            && session.session_id == binding.session_id
            && session.connection_generation == binding.generation
            && session
                .active_link
                .as_ref()
                .is_some_and(|link| Arc::ptr_eq(link, &binding.link))
            && Arc::ptr_eq(&session.cancellation, &binding.cancellation)
    }

    pub fn with_session<R>(
        &self,
        binding: &SessionBinding,
        operation: impl FnOnce(&mut DeviceSession) -> R,
    ) -> Result<R, SessionError> {
        let mut sessions = self.sessions.lock().map_err(|_| SessionError::Poisoned)?;
        let session = sessions
            .get_mut(&binding.device_id)
            .ok_or(SessionError::UnknownSession)?;
        if session.state == SessionState::Terminated {
            return Err(SessionError::TerminatedSession);
        }
        let is_current = session.session_id == binding.session_id
            && session.connection_generation == binding.generation
            && session
                .active_link
                .as_ref()
                .is_some_and(|link| Arc::ptr_eq(link, &binding.link))
            && Arc::ptr_eq(&session.cancellation, &binding.cancellation)
            && !binding.cancellation.load(Ordering::Acquire);
        if !is_current {
            return Err(SessionError::StaleBinding);
        }
        Ok(operation(session))
    }

    #[allow(dead_code)]
    pub fn claim_reconnect(&self, device_id: &str, now: Instant) -> Option<ReconnectLease> {
        let mut sessions = self.sessions.lock().ok()?;
        let session = sessions.get_mut(device_id)?;
        if session.state == SessionState::Terminated
            || session.pairing.state != crate::device_links::pairing::PairState::Paired
            || session.reconnect_scheduled
            || session.next_reconnect_at.is_some_and(|at| at > now)
        {
            return None;
        }
        session.state = SessionState::Reconnecting;
        session.reconnect_scheduled = true;
        Some(ReconnectLease {
            device_id: device_id.to_string(),
            session_id: session.session_id,
            generation: session.connection_generation,
        })
    }

    #[allow(dead_code)]
    pub fn reconnect_succeeded(&self, lease: &ReconnectLease) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(&lease.device_id) {
                if session.session_id == lease.session_id
                    && session.connection_generation == lease.generation
                    && session.state != SessionState::Terminated
                {
                    session.reconnect_scheduled = false;
                    session.reconnect_attempt = 0;
                    session.next_reconnect_at = None;
                }
            }
        }
    }

    #[allow(dead_code)]
    pub fn reconnect_failed(&self, lease: &ReconnectLease, reason: String) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(&lease.device_id) {
                if session.session_id == lease.session_id
                    && session.connection_generation == lease.generation
                    && session.state != SessionState::Terminated
                {
                    session.state = SessionState::Disconnected;
                    session.reconnect_scheduled = false;
                    session.reconnect_attempt = session.reconnect_attempt.saturating_add(1);
                    session.last_disconnect_reason = Some(reason);
                }
            }
        }
    }

    pub fn terminate_all(&self) -> Vec<Arc<SessionLink>> {
        let Ok(mut sessions) = self.sessions.lock() else {
            return Vec::new();
        };
        let mut peers = Vec::new();
        let links = sessions
            .values_mut()
            .filter_map(|session| {
                session.state = SessionState::Terminated;
                session.reconnect_scheduled = false;
                session.next_reconnect_at = None;
                session.cancellation.store(true, Ordering::Release);
                if let Some(peer) = session.clear_webrtc_state() {
                    peers.push(peer);
                }
                session.active_link.take()
            })
            .collect();
        drop(sessions);
        for peer in peers {
            peer.close();
        }
        links
    }

    /// One authoritative reachability reading per known device session.
    ///
    /// The device view used to be mutated from many places — discovery,
    /// handshake, disconnect handlers — and any of them could leave a stale
    /// "connected" behind. This is the single source the UI reconciles against.
    /// Abandons every pairing negotiation whose deadline has passed.
    ///
    /// Returns the affected device ids so the caller can clear their prompts.
    /// Driven from the status reconciler, which already ticks once a second —
    /// a pairing timeout does not need a thread of its own.
    pub fn expire_pairing(&self, now_secs: i64) -> Vec<String> {
        let mut sessions = self.reconciler_sessions();
        sessions
            .values_mut()
            .filter_map(|session| {
                if session.pairing.expire_if_due(now_secs) {
                    Some(session.device_id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// The session map for the 1 Hz reconciler, **recovering a poisoned lock**.
    ///
    /// Everywhere else a poisoned mutex becomes `SessionError::Poisoned` and the
    /// caller gives up, which is right for a one-shot command the user can
    /// retry. It is wrong here. `expire_pairing` and `connection_snapshot` are
    /// the reconciler's only inputs, and both used to answer "nothing" on a
    /// poisoned lock — so a single panic anywhere under this mutex silently and
    /// **permanently** stopped pairing timeouts from firing and froze every
    /// device's status at whatever it happened to be. The app kept running and
    /// kept lying.
    ///
    /// Recovering is safe: Rust poisons a mutex to warn that an invariant
    /// *might* be broken, not because the memory is unusable, and these two
    /// readers only copy fields out. A stale reading beats no readings forever.
    fn reconciler_sessions(&self) -> std::sync::MutexGuard<'_, HashMap<String, DeviceSession>> {
        self.sessions.lock().unwrap_or_else(|poisoned| {
            eprintln!(
                "[Daemon] Recovered a poisoned session lock; status reconciliation continues"
            );
            poisoned.into_inner()
        })
    }

    pub fn connection_snapshot(&self) -> Vec<DeviceConnection> {
        self.reconciler_sessions()
            .values()
            .map(|session| DeviceConnection {
                device_id: session.device_id.clone(),
                // A link the reader has already closed must not count as
                // reachable, otherwise the retained-bootstrap path keeps
                // reporting a live connection over a dead socket.
                linked: session.state != SessionState::Terminated
                    && session
                        .active_link
                        .as_ref()
                        .is_some_and(|link| !link.is_closed()),
                paired: session.pairing.state == crate::device_links::pairing::PairState::Paired,
                feature_transport_ready: session
                    .webrtc_handover
                    .as_ref()
                    .is_some_and(HandoverRuntime::features_ready),
                lan_silence: session
                    .active_link
                    .as_ref()
                    .filter(|link| !link.is_closed())
                    .map(|_| session.last_inbound_at.elapsed()),
            })
            .collect()
    }

    /// The ids of sessions with a live link that are not paired.
    pub fn unpaired_session_ids(&self) -> Vec<String> {
        self.sessions
            .lock()
            .map(|sessions| {
                sessions
                    .iter()
                    .filter(|(_, session)| {
                        session.active_link.is_some()
                            && session.pairing.state
                                != crate::device_links::pairing::PairState::Paired
                    })
                    .map(|(id, _)| id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests;
