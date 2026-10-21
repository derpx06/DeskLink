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

/// Identifiers retained per attempt for replay rejection.
const REPLAY_CACHE_CAPACITY: usize = 4096;

/// Immutable transport data copied from the authoritative session while its
/// lock is held.  Callers must perform network I/O only after obtaining this
/// snapshot: GStreamer and TLS callbacks may synchronously trigger lifecycle
/// work, so holding the session mutex across a send can deadlock recovery.
#[derive(Clone)]
pub struct FeatureTransportSnapshot {
    pub binding: SessionBinding,
    pub paired: bool,
    pub web_rtc_peer: Option<Arc<DesktopWebRtcPeer>>,
    pub transfer_manager: Option<Arc<WebRtcTransferManager>>,
    pub web_rtc_wire: Option<WebRtcWireBinding>,
    pub web_rtc_ready: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    UnknownSession,
    StaleBinding,
    TerminatedSession,
    AlreadyClaimed,
    NotEligibleForReconnect,
    NotPaired,
    ReplayDetected,
    Poisoned,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnknownSession => "Device session is unknown",
            Self::StaleBinding => "Connection was replaced by a newer session generation",
            Self::TerminatedSession => "Device session is terminated",
            Self::AlreadyClaimed => "Reconnect is already in progress",
            Self::NotEligibleForReconnect => "Device session is not eligible for reconnect",
            Self::NotPaired => "DeskLink WebRTC requires a paired device session",
            Self::ReplayDetected => "DeskLink WebRTC signaling request was replayed",
            Self::Poisoned => "Device session lock poisoned",
        };
        formatter.write_str(message)
    }
}

#[allow(dead_code)]
pub struct RegistrationResult {
    pub binding: SessionBinding,
    pub replaced_link: Option<Arc<SessionLink>>,
    pub replaced_generation: Option<ConnectionGeneration>,
    pub replaced_webrtc: Option<Arc<DesktopWebRtcPeer>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectLease {
    pub device_id: String,
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
}

pub struct DisconnectResult {
    pub was_current: bool,
    /// The closed socket was only the LAN bootstrap/signaling path.  A
    /// mutually feature-ready WebRTC peer remains authoritative for the
    /// logical session and must not be cancelled with that socket.
    pub feature_transport_retained: bool,
    pub link: Option<Arc<SessionLink>>,
    pub webrtc: Option<Arc<DesktopWebRtcPeer>>,
}

/// The outcome of one liveness pass: who to probe, and who has gone.
#[derive(Default)]
pub struct LivenessTick {
    pub probe: Vec<SessionBinding>,
    pub dead: Vec<SessionBinding>,
}

/// A reachability reading for one device, taken under the session lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConnection {
    pub device_id: String,
    /// A bootstrap link exists and its socket has not been closed.
    pub linked: bool,
    pub paired: bool,
    pub feature_transport_ready: bool,
}

impl DeviceConnection {
    /// A device is reachable over the bootstrap link, the authenticated WebRTC
    /// transport, or both. Losing only one of the two is not a disconnect.
    pub fn is_reachable(&self) -> bool {
        self.linked || self.feature_transport_ready
    }
}

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
        idle: std::time::Duration,
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
            } else if silence >= idle {
                tick.probe.push(binding);
            }
        }
        tick
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
    pub fn adopt_bootstrap_link(
        &self,
        device_id: &str,
        transport: &Arc<dyn LinkTransport>,
    ) -> Option<SessionBinding> {
        let mut sessions = self.sessions.lock().ok()?;
        let session = sessions.get_mut(device_id)?;
        if session.state == SessionState::Terminated {
            return None;
        }
        if !session
            .webrtc_handover
            .as_ref()
            .is_some_and(HandoverRuntime::features_ready)
        {
            return None;
        }
        let link = session.active_link.as_ref()?;
        if !link.is_closed() {
            return None;
        }
        link.adopt(Arc::clone(transport));
        session.state = SessionState::Ready;
        // A working socket is fresh evidence the peer is reachable, so a
        // recovery ladder exhausted while it was away starts over.
        session.webrtc_recovery_attempt = 0;
        session.last_disconnect_reason = None;
        // The adopted socket has, by definition, just completed a handshake.
        // Carrying the old one's staleness clock over would have the liveness
        // tick bury it before its first packet.
        session.last_inbound_at = Instant::now();
        session.binding()
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

    pub fn disconnect_if_current(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> DisconnectResult {
        let Ok(mut sessions) = self.sessions.lock() else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let matches = session.session_id == binding.session_id
            && session.connection_generation == binding.generation
            && session
                .active_link
                .as_ref()
                .is_some_and(|link| Arc::ptr_eq(link, &binding.link))
            && Arc::ptr_eq(&session.cancellation, &binding.cancellation);
        if !matches || session.state == SessionState::Terminated {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        }
        session.state = SessionState::Disconnected;
        session.last_disconnect_reason = Some(reason);
        session.connected_at = None;
        session.cancellation.store(true, Ordering::Release);
        let link = session.active_link.take();
        let webrtc = session.clear_webrtc_state();
        DisconnectResult {
            was_current: true,
            feature_transport_retained: false,
            link,
            webrtc,
        }
    }

    /// Handles loss of the LAN discovery/pairing/signaling socket without
    /// conflating it with loss of an already authenticated WebRTC feature
    /// peer.  The current binding intentionally retains its link identity:
    /// WebRTC callbacks are generation-bound to that binding, while the
    /// socket itself is closed after this manager lock is released.
    pub fn bootstrap_disconnected_if_current(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> DisconnectResult {
        let Ok(mut sessions) = self.sessions.lock() else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let matches = session.session_id == binding.session_id
            && session.connection_generation == binding.generation
            && session
                .active_link
                .as_ref()
                .is_some_and(|link| Arc::ptr_eq(link, &binding.link))
            && Arc::ptr_eq(&session.cancellation, &binding.cancellation);
        if !matches || session.state == SessionState::Terminated {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        }

        // `HandoverRuntime::features_ready` is reached only after the
        // installed peer has completed mutual authentication, capabilities,
        // and feature-ready confirmation.  Peer and handover are installed
        // and cleared together by this manager.
        let web_rtc_ready = session
            .webrtc_handover
            .as_ref()
            .is_some_and(HandoverRuntime::features_ready);
        if web_rtc_ready {
            // Do not cancel the generation or clear peer/handover state.  The
            // retained Arc is returned only so the caller can close the dead
            // bootstrap socket without doing I/O under the session lock.
            //
            // The link is deliberately *kept* on the session: WebRTC callbacks
            // are generation-bound to a binding that holds this exact Arc, and
            // taking it here would make every one of them stale. `SessionLink`
            // records its own closed flag instead, so `connection_snapshot` and
            // the signaling path can both tell that the socket is gone while
            // the binding identity stays intact.
            session.state = SessionState::Ready;
            session.last_disconnect_reason = None;
            return DisconnectResult {
                was_current: true,
                feature_transport_retained: true,
                link: session.active_link.as_ref().cloned(),
                webrtc: None,
            };
        }
        drop(sessions);
        self.disconnect_if_current(binding, reason)
    }

    /// Atomically installs the only current WebRTC peer for a current DeskLink
    /// session. The caller closes a replaced peer after this manager lock is
    /// released so GStreamer callbacks cannot deadlock the session core.
    pub fn install_webrtc_peer(
        &self,
        binding: &SessionBinding,
        attempt_id: String,
        wire_binding: WebRtcWireBinding,
        peer: DesktopWebRtcPeer,
        transfer_manager: Arc<WebRtcTransferManager>,
    ) -> Result<Option<Arc<DesktopWebRtcPeer>>, SessionError> {
        self.with_session(binding, |session| {
            if session.pairing.state != crate::device_links::pairing::PairState::Paired {
                return Err(SessionError::NotPaired);
            }
            if session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready)
            {
                crate::device_links::connection_log::note_ready(&session.device_id, false);
            }
            let replaced = session.active_webrtc.replace(Arc::new(peer));
            session.transfer_manager = Some(transfer_manager);
            session.webrtc_handover = Some(HandoverRuntime::new(attempt_id.clone(), wire_binding));
            session.webrtc_attempt_id = Some(attempt_id);
            session.seen_webrtc_request_ids.clear();
            session.seen_webrtc_message_ids.clear();
            Ok(replaced)
        })?
    }

    /// The feature-ready WebRTC peer for `device_id`, and whether its
    /// bootstrap link is the relay (so a host-to-host pair is an internet
    /// IPv6 path, not the LAN).
    pub fn ready_peer(&self, device_id: &str) -> Option<(Arc<DesktopWebRtcPeer>, bool)> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(device_id)?;
        if !session
            .webrtc_handover
            .as_ref()
            .is_some_and(HandoverRuntime::features_ready)
        {
            return None;
        }
        let via_relay = session
            .active_link
            .as_ref()
            .is_some_and(|link| link.local_ip().is_none());
        Some((Arc::clone(session.active_webrtc.as_ref()?), via_relay))
    }

    pub fn active_webrtc_peer(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
    ) -> Result<Arc<DesktopWebRtcPeer>, SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            session
                .active_webrtc
                .as_ref()
                .cloned()
                .ok_or(SessionError::UnknownSession)
        })?
    }

    /// Returns the signed attempt currently associated with this transport
    /// generation.  Bootstrap SDP/ICE messages must never replace it merely
    /// because they arrived later on a retried TLS socket.
    pub fn active_webrtc_attempt(
        &self,
        binding: &SessionBinding,
    ) -> Result<Option<String>, SessionError> {
        self.with_session(binding, |session| Ok(session.webrtc_attempt_id.clone()))?
    }

    /// Reads the handover state from the session owner. A false value is a
    /// deliberate safe default: LAN feature traffic remains available only
    /// until both peers have completed their authenticated handover.
    #[allow(dead_code)] // Consumed by the shared feature-dispatcher slice.
    pub fn webrtc_features_ready(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
    ) -> Result<bool, SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            Ok(session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready))
        })?
    }

    /// Invalidates one WebRTC attempt without dropping the authenticated LAN
    /// bootstrap session. Only the current attempt may clear the peer; stale
    /// GStreamer callbacks receive `StaleBinding` and cannot affect a newer
    /// generation.
    pub fn clear_webrtc_if_current(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        reason: String,
    ) -> Result<Option<Arc<DesktopWebRtcPeer>>, SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            session.last_disconnect_reason = Some(reason);
            Ok(session.clear_webrtc_state())
        })?
    }

    /// Drops the pairing state of a device that has no live link, so an unpair
    /// done while it was offline cannot be undone by its session on return.
    /// Returns the session's WebRTC peer, if any, for the caller to close
    /// outside this lock — a revoked pairing must not leave a peer running.
    pub fn forget_pairing(&self, device_id: &str) -> Option<Arc<DesktopWebRtcPeer>> {
        let mut sessions = self.sessions.lock().ok()?;
        let session = sessions.get_mut(device_id)?;
        session.pairing = PairingHandler::new(false);
        session.pending_features.clear();
        session.clear_webrtc_state()
    }

    /// Holds a feature until WebRTC is ready, evicting the oldest past
    /// `PENDING_FEATURE_LIMIT`. Returns `false` without queueing when the
    /// handover is already feature-ready: the check and the push share one
    /// lock, so a packet can never land in the queue after the flush ran.
    pub fn queue_feature(
        &self,
        binding: &SessionBinding,
        packet: crate::device_links::packet::NetworkPacket,
    ) -> Result<bool, SessionError> {
        self.with_session(binding, |session| {
            if session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready)
            {
                return false;
            }
            if session.pending_features.len() >= super::device_session::PENDING_FEATURE_LIMIT {
                session.pending_features.pop_front();
            }
            session.pending_features.push_back((Instant::now(), packet));
            true
        })
    }

    /// Drains the queue, dropping anything older than `PENDING_FEATURE_MAX_AGE`.
    pub fn take_pending_features(
        &self,
        binding: &SessionBinding,
    ) -> Vec<crate::device_links::packet::NetworkPacket> {
        self.with_session(binding, |session| {
            session
                .pending_features
                .drain(..)
                .filter(|(queued_at, packet)| {
                    let fresh =
                        queued_at.elapsed() <= super::device_session::PENDING_FEATURE_MAX_AGE;
                    if !fresh {
                        debug_log!("[DeskLink] Dropped stale queued {}", packet.packet_type);
                    }
                    fresh
                })
                .map(|(_, packet)| packet)
                .collect()
        })
        .unwrap_or_default()
    }

    /// Revokes the local pairing state and every WebRTC object derived from
    /// it.  The returned peer is deliberately closed by the caller only after
    /// the session lock has been released, because peer callbacks can trigger
    /// lifecycle work.
    pub fn revoke_pairing(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> Result<
        (
            crate::device_links::packet::NetworkPacket,
            Option<Arc<DesktopWebRtcPeer>>,
        ),
        SessionError,
    > {
        self.with_session(binding, |session| {
            let packet = session.pairing.reject_packet();
            session.last_disconnect_reason = Some(reason);
            session.pending_features.clear();
            (packet, session.clear_webrtc_state())
        })
    }

    /// Serializes all mutations of the WebRTC handover runtime through the
    /// authoritative session. It prevents a callback from a replaced peer
    /// generation from changing current transport policy.
    pub fn with_webrtc_handover<R>(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        operation: impl FnOnce(&mut HandoverRuntime) -> Result<R, String>,
    ) -> Result<R, String> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err("DeskLink WebRTC session was replaced".to_string());
            }
            let handover = session
                .webrtc_handover
                .as_mut()
                .ok_or_else(|| "DeskLink WebRTC handover is not initialized".to_string())?;
            operation(handover)
        })
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
    }

    /// Returns the active peer only once for a signed signaling request ID.
    /// The bounded replay cache belongs to the session rather than a packet
    /// reader so duplicate LAN links cannot bypass it.
    pub fn accept_webrtc_signal(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        request_id: String,
    ) -> Result<Arc<DesktopWebRtcPeer>, SessionError> {
        self.with_session(binding, |session| {
            if session.pairing.state != crate::device_links::pairing::PairState::Paired {
                return Err(SessionError::NotPaired);
            }
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            if !session.seen_webrtc_request_ids.accept(request_id) {
                return Err(SessionError::ReplayDetected);
            }
            session
                .active_webrtc
                .as_ref()
                .cloned()
                .ok_or(SessionError::UnknownSession)
        })?
    }

    /// Accepts a data-channel envelope message ID exactly once for the active
    /// WebRTC attempt. The cache is session-owned so a stale peer callback
    /// cannot bypass replay protection after a link replacement.
    pub fn accept_webrtc_envelope(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        message_id: String,
    ) -> Result<(), SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            if !session.seen_webrtc_message_ids.accept(message_id) {
                return Err(SessionError::ReplayDetected);
            }
            Ok(())
        })?
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
            })
            .collect()
    }

    pub fn unpaired_session_count(&self) -> usize {
        self.sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .filter(|session| {
                        session.active_link.is_some()
                            && session.pairing.state
                                != crate::device_links::pairing::PairState::Paired
                    })
                    .count()
            })
            .unwrap_or(0)
    }
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mark_features_ready(manager: &DeviceManager, binding: &SessionBinding) {
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let wire =
            WebRtcWireBinding::from_attempt("desktop", &binding.device_id, &attempt_id).unwrap();
        manager
            .with_session(binding, |session| {
                let mut handover = HandoverRuntime::new(attempt_id.clone(), wire);
                handover.record_offer("offer".to_string()).unwrap();
                handover.record_answer("answer".to_string()).unwrap();
                handover.mark_local_authenticated().unwrap();
                handover.mark_remote_authenticated().unwrap();
                handover.mark_local_capabilities_confirmed().unwrap();
                handover.mark_remote_capabilities_confirmed().unwrap();
                handover.mark_local_feature_ready().unwrap();
                handover.mark_remote_feature_ready().unwrap();
                session.webrtc_handover = Some(handover);
            })
            .unwrap();
    }

    fn link(device_id: &str) -> SessionLink {
        SessionLink::test_placeholder(device_id)
    }

    #[test]
    fn a_new_manager_has_no_current_binding() {
        let manager = DeviceManager::new();

        assert!(manager.current_binding("phone-1").is_none());
    }

    #[test]
    fn replacement_keeps_session_id_and_makes_old_binding_stale() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap();
        let second = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap();

        assert_eq!(first.binding.session_id, second.binding.session_id);
        assert_eq!(second.binding.generation, first.binding.generation + 1);
        assert!(!manager.is_current(&first.binding));
        assert!(manager.is_current(&second.binding));
        assert!(second.replaced_link.unwrap().is_closed());
        assert!(first.binding.cancellation.load(Ordering::Acquire));
    }

    #[test]
    fn transport_snapshot_becomes_stale_after_link_replacement() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let snapshot = manager.feature_transport_snapshot(&first).unwrap();
        let replacement = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;

        assert!(snapshot.paired);
        assert!(!snapshot.web_rtc_ready);
        assert!(!manager.is_current(&snapshot.binding));
        assert!(manager.is_current(&replacement));
    }

    #[test]
    fn stale_disconnect_cannot_clear_a_replacement() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap();
        let second = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap();

        let stale = manager.disconnect_if_current(&first.binding, "old reader ended".to_string());

        assert!(!stale.was_current);
        assert!(manager.is_current(&second.binding));
    }

    #[test]
    fn pairing_state_follows_the_trust_store_across_transport_replacement() {
        let manager = DeviceManager::new();
        let paired = |binding: &SessionBinding| {
            manager
                .with_session(binding, |session| {
                    session.pairing.state == crate::device_links::pairing::PairState::Paired
                })
                .unwrap()
        };
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap();
        let second = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap();
        assert!(paired(&second.binding));
        assert_eq!(first.binding.session_id, second.binding.session_id);

        // Unpaired while it was away: the next link must not resume `Paired`,
        // or any certificate claiming this id would be accepted unpinned.
        let third = manager
            .register_link("phone-1".to_string(), link("phone-1"), false)
            .unwrap();
        assert!(!paired(&third.binding));
    }

    #[test]
    fn only_one_reconnect_lease_can_be_claimed() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let disconnected = manager.disconnect_if_current(&binding, "network lost".to_string());
        assert!(disconnected.was_current);

        assert!(manager.claim_reconnect("phone-1", Instant::now()).is_some());
        assert!(manager.claim_reconnect("phone-1", Instant::now()).is_none());
    }

    #[test]
    fn registration_after_disconnect_creates_a_live_new_generation() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let disconnected =
            manager.disconnect_if_current(&first, "bootstrap socket closed".to_string());
        assert!(disconnected.was_current);
        assert!(first.cancellation.load(Ordering::Acquire));

        let reconnected = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;

        assert_eq!(reconnected.session_id, first.session_id);
        assert_eq!(reconnected.generation, first.generation + 1);
        assert!(!reconnected.cancellation.load(Ordering::Acquire));
        assert!(manager.is_current(&reconnected));
    }

    /// The desktop discards a refreshed bootstrap socket while its WebRTC peer
    /// is feature-ready, so a discovery refresh does not tear down every paired
    /// feature. That is right only while the retained link is still open.
    ///
    /// Once the phone has closed the old socket, keeping it and discarding the
    /// fresh one leaves this side with **no signalling channel** — and when the
    /// WebRTC peer subsequently fails, the `RestartRequest` it must send dies
    /// with "Session link is closed" against a socket the peer already
    /// replaced. Observed on hardware as a device stuck on "Connecting"
    /// forever. `finish_secure_link` therefore requires both conditions.
    #[test]
    fn a_refreshed_socket_is_only_discarded_while_the_retained_link_is_open() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let wire = WebRtcWireBinding::from_attempt("desktop", "phone-1", &attempt_id).unwrap();
        manager
            .with_session(&binding, |session| {
                let mut handover = HandoverRuntime::new(attempt_id.clone(), wire);
                handover.record_offer("offer".to_string()).unwrap();
                handover.record_answer("answer".to_string()).unwrap();
                handover.mark_local_authenticated().unwrap();
                handover.mark_remote_authenticated().unwrap();
                handover.mark_local_capabilities_confirmed().unwrap();
                handover.mark_remote_capabilities_confirmed().unwrap();
                handover.mark_local_feature_ready().unwrap();
                handover.mark_remote_feature_ready().unwrap();
                session.webrtc_handover = Some(handover);
            })
            .unwrap();

        // Healthy: WebRTC is ready and the bootstrap socket is alive, so a
        // refresh is redundant and is correctly thrown away.
        assert!(manager.has_ready_webrtc("phone-1"));
        assert!(manager.has_live_bootstrap_link("phone-1"));

        // The phone replaces its short-lived socket. WebRTC is still ready, but
        // the retained link is now dead.
        binding.link.close();
        assert!(
            manager.has_ready_webrtc("phone-1"),
            "the authenticated WebRTC transport survives the bootstrap socket"
        );
        assert!(
            !manager.has_live_bootstrap_link("phone-1"),
            "a refreshed socket must now be adopted, or signalling has nowhere to go"
        );
    }

    /// The other half of the rule above: once the retained socket is dead, the
    /// replacement must be *adopted*, not registered.
    ///
    /// `register_link` bumps the generation and calls `clear_webrtc_state`
    /// unconditionally, so registering here tore down a working, authenticated
    /// transport every time the phone refreshed its bootstrap socket. The
    /// handover that followed dropped its own bootstrap socket, which brought
    /// the phone straight back — a loop that pinned the UI on "Connecting"
    /// while battery and notifications kept arriving over the link underneath.
    #[test]
    fn a_replacement_socket_is_adopted_without_disturbing_a_ready_webrtc_transport() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        mark_features_ready(&manager, &binding);
        binding.link.close();

        let replacement: Arc<dyn LinkTransport> =
            Arc::new(crate::device_links::core::link_transport::DeadTransport);
        let adopted = manager
            .adopt_bootstrap_link("phone-1", &replacement)
            .expect("a closed link with a ready WebRTC transport is adoptable");

        assert_eq!(
            adopted.generation, binding.generation,
            "adoption must not bump the generation the WebRTC peer is bound to"
        );
        assert!(
            Arc::ptr_eq(&adopted.link, &binding.link),
            "the link identity every WebRTC callback compares against must survive"
        );
        assert!(
            manager.is_current(&binding),
            "the old binding stays current"
        );
        assert!(
            manager.has_ready_webrtc("phone-1"),
            "the working transport must not be torn down by a socket refresh"
        );
        assert!(
            manager.has_live_bootstrap_link("phone-1"),
            "signalling must have somewhere to go again"
        );
    }

    /// Adoption is for preserving something. With no feature-ready transport
    /// there is nothing to preserve and a plain registration is correct — and
    /// a live incumbent still owns a reader thread, so swapping underneath it
    /// would leave two readers framing one stream.
    #[test]
    fn adoption_is_refused_without_a_ready_transport_or_against_a_live_socket() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let replacement: Arc<dyn LinkTransport> =
            Arc::new(crate::device_links::core::link_transport::DeadTransport);

        binding.link.close();
        assert!(
            manager
                .adopt_bootstrap_link("phone-1", &replacement)
                .is_none(),
            "no WebRTC transport to preserve"
        );

        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        mark_features_ready(&manager, &binding);
        assert!(
            manager
                .adopt_bootstrap_link("phone-1", &replacement)
                .is_none(),
            "a live incumbent still owns its reader thread"
        );
    }

    #[test]
    fn bootstrap_disconnect_preserves_feature_ready_generation() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let attempt_id = uuid::Uuid::new_v4().to_string();
        let wire = WebRtcWireBinding::from_attempt("desktop", "phone-1", &attempt_id).unwrap();
        manager
            .with_session(&binding, |session| {
                let mut handover = HandoverRuntime::new(attempt_id.clone(), wire);
                handover.record_offer("offer".to_string()).unwrap();
                handover.record_answer("answer".to_string()).unwrap();
                handover.mark_local_authenticated().unwrap();
                handover.mark_remote_authenticated().unwrap();
                handover.mark_local_capabilities_confirmed().unwrap();
                handover.mark_remote_capabilities_confirmed().unwrap();
                handover.mark_local_feature_ready().unwrap();
                handover.mark_remote_feature_ready().unwrap();
                session.webrtc_handover = Some(handover);
                // The concrete GStreamer peer is not needed by this manager
                // test. Presence is represented by retaining the handover
                // runtime, while production only reaches this state with an
                // installed peer.
            })
            .unwrap();

        let disconnected = manager
            .bootstrap_disconnected_if_current(&binding, "bootstrap socket closed".to_string());
        assert!(disconnected.was_current);
        assert!(disconnected.feature_transport_retained);
        assert!(manager.is_current(&binding));
        assert!(!binding.cancellation.load(Ordering::Acquire));
        assert_eq!(
            manager
                .with_session(&binding, |session| session.connection_generation)
                .unwrap(),
            binding.generation,
        );
    }

    /// Discovery decides whether to reconnect from `has_live_bootstrap_link`,
    /// not from the presence of a binding. A session keeps a closed link
    /// installed while WebRTC outlives the socket, and using binding presence
    /// meant such a device was never offered a replacement connection — so once
    /// its WebRTC peer failed too, it stayed offline until a restart.
    #[test]
    fn a_closed_bootstrap_link_does_not_count_as_a_live_connection() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        assert!(manager.has_live_bootstrap_link("phone-1"));

        binding.link.close();

        assert!(!manager.has_live_bootstrap_link("phone-1"));
        // The binding itself deliberately survives so WebRTC callbacks that are
        // generation-bound to it stay valid.
        assert!(manager.current_binding("phone-1").is_some());
    }

    #[test]
    fn an_unknown_device_has_no_live_bootstrap_link() {
        let manager = DeviceManager::new();
        assert!(!manager.has_live_bootstrap_link("phone-1"));
    }

    /// The reconciler reads reachability from here, so a closed socket with no
    /// WebRTC transport must read as unreachable.
    #[test]
    fn connection_snapshot_reports_a_closed_socket_as_unreachable() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;

        let snapshot = manager.connection_snapshot();
        assert_eq!(snapshot.len(), 1);
        assert!(snapshot[0].linked);
        assert!(snapshot[0].paired);
        assert!(snapshot[0].is_reachable());

        binding.link.close();

        let snapshot = manager.connection_snapshot();
        assert!(!snapshot[0].linked);
        assert!(!snapshot[0].feature_transport_ready);
        assert!(!snapshot[0].is_reachable());
    }

    #[test]
    fn stale_webrtc_failure_cannot_clear_a_new_attempt() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        manager
            .with_session(&binding, |session| {
                session.webrtc_attempt_id = Some("current-attempt".to_string());
            })
            .unwrap();

        assert!(matches!(
            manager.clear_webrtc_if_current(
                &binding,
                "stale-attempt",
                "old peer failed".to_string(),
            ),
            Err(SessionError::StaleBinding)
        ));
        assert_eq!(
            manager
                .with_session(&binding, |session| session.webrtc_attempt_id.clone())
                .unwrap(),
            Some("current-attempt".to_string()),
        );
    }

    #[test]
    fn unpairing_clears_the_active_webrtc_attempt() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        manager
            .with_session(&binding, |session| {
                session.webrtc_attempt_id = Some("old-attempt".to_string());
            })
            .unwrap();

        manager
            .revoke_pairing(&binding, "user unpaired the device".to_string())
            .unwrap();

        assert_eq!(manager.active_webrtc_attempt(&binding).unwrap(), None);
    }

    #[test]
    fn terminate_all_cancels_each_current_binding() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let second = manager
            .register_link("phone-2".to_string(), link("phone-2"), false)
            .unwrap()
            .binding;

        let links = manager.terminate_all();

        assert_eq!(links.len(), 2);
        assert!(first.cancellation.load(Ordering::Acquire));
        assert!(second.cancellation.load(Ordering::Acquire));
        assert!(manager.current_binding("phone-1").is_none());
        assert!(manager.current_binding("phone-2").is_none());
    }

    #[test]
    fn replacement_discards_the_previous_handover_state() {
        use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};

        let manager = DeviceManager::new();
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        manager
            .with_session(&first, |session| {
                let wire = WebRtcWireBinding::from_attempt(
                    "desktop",
                    "phone-1",
                    "01234567-89ab-cdef-0123-456789abcdef",
                )
                .unwrap();
                session.webrtc_handover = Some(HandoverRuntime::new("attempt".to_string(), wire));
            })
            .unwrap();

        let replacement = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        let handover_present = manager
            .with_session(&replacement, |session| session.webrtc_handover.is_some())
            .unwrap();

        assert!(!handover_present);
    }

    #[test]
    fn a_webrtc_envelope_is_accepted_only_once_per_attempt() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;
        manager
            .with_session(&binding, |session| {
                session.webrtc_attempt_id = Some("attempt".to_string());
            })
            .unwrap();

        manager
            .accept_webrtc_envelope(&binding, "attempt", "message-1".to_string())
            .unwrap();
        assert_eq!(
            manager
                .accept_webrtc_envelope(&binding, "attempt", "message-1".to_string())
                .unwrap_err(),
            SessionError::ReplayDetected
        );
    }

    #[test]
    fn a_feature_is_not_queued_once_the_handover_is_ready() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone".to_string(), link("phone"), true)
            .unwrap()
            .binding;
        let ping = crate::device_links::packet::NetworkPacket::new("desklink.ping");
        assert!(manager.queue_feature(&binding, ping.clone()).unwrap());
        mark_features_ready(&manager, &binding);
        // Ready: the caller must send directly, so nothing strands in the queue.
        assert!(!manager.queue_feature(&binding, ping).unwrap());
        assert_eq!(manager.take_pending_features(&binding).len(), 1);
    }

    #[test]
    fn stale_queued_features_are_dropped_not_delivered() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone".to_string(), link("phone"), true)
            .unwrap()
            .binding;
        manager
            .with_session(&binding, |session| {
                let old = Instant::now()
                    - super::super::device_session::PENDING_FEATURE_MAX_AGE
                    - std::time::Duration::from_secs(1);
                session.pending_features.push_back((
                    old,
                    crate::device_links::packet::NetworkPacket::new("desklink.lock.request"),
                ));
            })
            .unwrap();
        manager
            .queue_feature(
                &binding,
                crate::device_links::packet::NetworkPacket::new("desklink.ping"),
            )
            .unwrap();
        let taken = manager.take_pending_features(&binding);
        assert_eq!(taken.len(), 1);
        assert_eq!(taken[0].packet_type, "desklink.ping");
    }

    #[test]
    fn a_new_instance_id_means_the_peer_restarted() {
        let manager = DeviceManager::new();
        let mut first = link("phone");
        first.info.instance_id = Some("epoch-a".to_string());
        manager
            .register_link("phone".to_string(), first, true)
            .unwrap();

        assert!(!manager.peer_restarted("phone", Some("epoch-a")));
        assert!(manager.peer_restarted("phone", Some("epoch-b")));
        // An older build that sends no epoch never counts as a restart.
        assert!(!manager.peer_restarted("phone", None));
        assert!(!manager.peer_restarted("unknown", Some("epoch-b")));
    }

    #[test]
    fn a_new_link_resets_an_exhausted_recovery_ladder() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone".to_string(), link("phone"), true)
            .unwrap()
            .binding;
        manager
            .with_session(&binding, |session| session.webrtc_recovery_attempt = 6)
            .unwrap();
        let replaced = manager
            .register_link("phone".to_string(), link("phone"), true)
            .unwrap()
            .binding;
        assert_eq!(
            manager
                .with_session(&replaced, |session| session.webrtc_recovery_attempt)
                .unwrap(),
            0
        );
    }
}
