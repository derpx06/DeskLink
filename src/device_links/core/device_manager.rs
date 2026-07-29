use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::device_links::pairing::PairingHandler;
use crate::device_links::webrtc::transfer_manager::WebRtcTransferManager;
use crate::device_links::webrtc::{DesktopWebRtcPeer, HandoverRuntime, WebRtcWireBinding};

use super::{
    ConnectionGeneration, DeviceSession, SessionBinding, SessionId, SessionLink, SessionState,
};

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
    pub link: Option<Arc<SessionLink>>,
    pub webrtc: Option<Arc<DesktopWebRtcPeer>>,
}

/// A single, generation-checked rebuild attempt for a failed WebRTC peer.
///
/// The logical DeskLink session and its signed bootstrap link deliberately
/// survive this transition.  The returned peer must be closed only after the
/// session mutex has been released.
pub struct WebRtcRecoverySchedule {
    pub replaced_peer: Arc<DesktopWebRtcPeer>,
    pub delay: Duration,
}

pub struct DeviceManager {
    next_session_id: AtomicU64,
    sessions: Mutex<HashMap<String, DeviceSession>>,
}

fn webrtc_recovery_delay(attempt: u32) -> Duration {
    // Keep this sequence identical to Android's `WebRtcRecoveryPolicy`.
    // It is deliberately bounded: an unavailable portal/plugin must not leave
    // a background thread sleeping for minutes or retrying in a tight loop.
    const DELAYS: [u64; 6] = [1, 2, 4, 8, 16, 30];
    Duration::from_secs(DELAYS[usize::try_from(attempt)
        .unwrap_or(usize::MAX)
        .min(DELAYS.len() - 1)])
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            next_session_id: AtomicU64::new(1),
            sessions: Mutex::new(HashMap::new()),
        }
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
                let replaced_link = session.active_link.replace(Arc::clone(&link));
                let replaced_generation = replaced_link
                    .as_ref()
                    .map(|_| session.connection_generation);
                let replaced_webrtc = if replaced_link.is_some() {
                    session.webrtc_attempt_id = None;
                    session.webrtc_handover = None;
                    session.seen_webrtc_request_ids.clear();
                    session.seen_webrtc_message_ids.clear();
                    session.remote_session.stop();
                    session.transfer_manager = None;
                    session.active_webrtc.take()
                } else {
                    None
                };
                if replaced_link.is_some() {
                    session.cancellation.store(true, Ordering::Release);
                    session.connection_generation = session.connection_generation.saturating_add(1);
                    session.cancellation = Arc::new(AtomicBool::new(false));
                }
                if initially_paired
                    && session.pairing.state != crate::device_links::pairing::PairState::Paired
                {
                    session.pairing = PairingHandler::new(true);
                }
                session.state = SessionState::Ready;
                session.connected_at = Some(now);
                session.last_disconnect_reason = None;
                session.reconnect_attempt = 0;
                session.reconnect_scheduled = false;
                session.next_reconnect_at = None;
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
                    seen_webrtc_request_ids: Default::default(),
                    seen_webrtc_message_ids: Default::default(),
                    remote_session: Default::default(),
                    cancellation: Arc::new(AtomicBool::new(false)),
                    connected_at: Some(now),
                    last_disconnect_reason: None,
                    reconnect_attempt: 0,
                    reconnect_scheduled: false,
                    next_reconnect_at: None,
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

    /// Runs a generation-checked operation against the remote session model.
    /// The closure must not perform portal, GStreamer, TLS, or data-channel I/O
    /// while this lock is held; it may only decide whether that I/O is allowed.
    pub fn with_remote_session<R>(
        &self,
        binding: &SessionBinding,
        operation: impl FnOnce(&mut crate::device_links::webrtc::RemoteSession) -> R,
    ) -> Result<R, SessionError> {
        self.with_session(binding, |session| operation(&mut session.remote_session))
    }

    /// Returns whether this binding currently owns a mutually authenticated
    /// WebRTC feature transport.
    ///
    /// The TLS link in a [`SessionBinding`] is deliberately a bootstrap and
    /// signaling anchor.  It can disappear while the DTLS/SCTP peer remains
    /// healthy (for example when Android briefly suspends the LAN socket as
    /// its display turns off).  Callers handling that bootstrap loss must use
    /// this check before deciding whether the entire logical device session is
    /// actually disconnected.
    pub fn has_ready_webrtc(&self, binding: &SessionBinding) -> bool {
        self.with_session(binding, |session| {
            session.active_webrtc.is_some()
                && session
                    .webrtc_handover
                    .as_ref()
                    .is_some_and(|handover| handover.features_ready())
        })
        .unwrap_or(false)
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
                link: None,
                webrtc: None,
            };
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return DisconnectResult {
                was_current: false,
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
                link: None,
                webrtc: None,
            };
        }
        crate::device_links::webrtc::portal::RemotePortalRegistry::global().release(binding);
        session.state = SessionState::Disconnected;
        session.last_disconnect_reason = Some(reason);
        session.connected_at = None;
        session.cancellation.store(true, Ordering::Release);
        let link = session.active_link.take();
        session.webrtc_attempt_id = None;
        session.webrtc_handover = None;
        session.seen_webrtc_request_ids.clear();
        session.seen_webrtc_message_ids.clear();
        session.remote_session.stop();
        session.transfer_manager = None;
        let webrtc = session.active_webrtc.take();
        DisconnectResult {
            was_current: true,
            link,
            webrtc,
        }
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
        // A peer replacement invalidates any portal session bound to its old
        // generation before a new WebRTC transport can become authoritative.
        crate::device_links::webrtc::portal::RemotePortalRegistry::global().release(binding);
        self.with_session(binding, |session| {
            if session.pairing.state != crate::device_links::pairing::PairState::Paired {
                return Err(SessionError::NotPaired);
            }
            let replaced = session.active_webrtc.replace(Arc::new(peer));
            session.transfer_manager = Some(transfer_manager);
            session.webrtc_handover = Some(HandoverRuntime::new(attempt_id.clone(), wire_binding));
            session.webrtc_attempt_id = Some(attempt_id);
            session.seen_webrtc_request_ids.clear();
            session.seen_webrtc_message_ids.clear();
            session.remote_session.stop();
            // A remote-initiated replacement may arrive while this desktop is
            // waiting in `Reconnecting`. Installing it consumes that pending
            // retry; feature-ready later resets the backoff counter.
            session.reconnect_scheduled = false;
            session.next_reconnect_at = None;
            Ok(replaced)
        })?
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

    /// Marks only the current WebRTC transport as degraded while retaining the
    /// logical device session and its bootstrap binding.  This lets discovery
    /// establish a new signed LAN signaling link for recovery without treating
    /// a transient media/data-channel failure as an unpair or identity reset.
    pub fn degrade_webrtc_if_current(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
    ) -> Result<Option<Arc<DesktopWebRtcPeer>>, SessionError> {
        crate::device_links::webrtc::portal::RemotePortalRegistry::global().release(binding);
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            if let Some(handover) = session.webrtc_handover.as_mut() {
                handover.fail();
            }
            session.webrtc_attempt_id = None;
            session.transfer_manager = None;
            session.remote_session.stop();
            Ok(session.active_webrtc.take())
        })?
    }

    /// Atomically retires a terminal WebRTC peer and reserves exactly one
    /// bounded recovery attempt for the current device generation.  Duplicate
    /// GStreamer `failed`/`closed` callbacks therefore cannot create multiple
    /// peer connections or overlapping portal leases.
    pub fn begin_webrtc_recovery(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        reason: String,
    ) -> Result<Option<WebRtcRecoverySchedule>, SessionError> {
        crate::device_links::webrtc::portal::RemotePortalRegistry::global().release(binding);
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            if session.reconnect_scheduled {
                return Ok(None);
            }
            let Some(replaced_peer) = session.active_webrtc.take() else {
                return Ok(None);
            };

            if let Some(handover) = session.webrtc_handover.as_mut() {
                handover.fail();
            }
            session.webrtc_attempt_id = None;
            session.transfer_manager = None;
            session.remote_session.stop();
            session.seen_webrtc_request_ids.clear();
            session.seen_webrtc_message_ids.clear();
            session.state = SessionState::Reconnecting;
            session.last_disconnect_reason = Some(reason);
            session.reconnect_scheduled = true;
            let delay = webrtc_recovery_delay(session.reconnect_attempt);
            session.reconnect_attempt = session.reconnect_attempt.saturating_add(1);
            session.next_reconnect_at = Some(Instant::now() + delay);
            Ok(Some(WebRtcRecoverySchedule {
                replaced_peer,
                delay,
            }))
        })?
    }

    /// Claims the recovery delay previously scheduled by
    /// [`Self::begin_webrtc_recovery`].  A stale worker cannot claim a newer
    /// generation because this is checked against the immutable binding.
    pub fn claim_scheduled_webrtc_recovery(
        &self,
        binding: &SessionBinding,
        now: Instant,
    ) -> Result<bool, SessionError> {
        self.with_session(binding, |session| {
            if !session.reconnect_scheduled
                || session.next_reconnect_at.is_some_and(|at| at > now)
            {
                return false;
            }
            session.reconnect_scheduled = false;
            session.next_reconnect_at = None;
            true
        })
    }

    /// Re-arms recovery when creating a replacement peer itself fails (for
    /// example while PipeWire/WebRTC is briefly restarting).  It intentionally
    /// does not recreate a transport while the session lock is held.
    pub fn rearm_webrtc_recovery(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> Result<Option<Duration>, SessionError> {
        self.with_session(binding, |session| {
            if session.state == SessionState::Terminated
                || session.pairing.state != crate::device_links::pairing::PairState::Paired
                || session.active_webrtc.is_some()
                || session.reconnect_scheduled
            {
                return Ok(None);
            }
            session.state = SessionState::Reconnecting;
            session.last_disconnect_reason = Some(reason);
            session.reconnect_scheduled = true;
            let delay = webrtc_recovery_delay(session.reconnect_attempt);
            session.reconnect_attempt = session.reconnect_attempt.saturating_add(1);
            session.next_reconnect_at = Some(Instant::now() + delay);
            Ok(Some(delay))
        })?
    }

    /// Marks recovery complete only after mutual authenticated feature-ready.
    /// A DTLS connection alone is insufficient: feature traffic must remain
    /// blocked until the signed capability exchange has also completed.
    pub fn mark_webrtc_ready(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
    ) -> Result<bool, SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            let ready = session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready);
            if ready {
                session.state = SessionState::Ready;
                session.reconnect_scheduled = false;
                session.reconnect_attempt = 0;
                session.next_reconnect_at = None;
                session.last_disconnect_reason = None;
            }
            Ok(ready)
        })?
    }

    /// Reads the handover state from the session owner. A false value is a
    /// deliberate safe default: paired feature traffic remains blocked until
    /// both peers have completed their authenticated handover. It is never
    /// redirected to the LAN bootstrap connection.
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
            if !session.seen_webrtc_request_ids.insert(request_id) {
                return Err(SessionError::ReplayDetected);
            }
            while session.seen_webrtc_request_ids.len() > 4096 {
                let Some(oldest) = session.seen_webrtc_request_ids.iter().next().cloned() else {
                    break;
                };
                session.seen_webrtc_request_ids.remove(&oldest);
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
            if !session.seen_webrtc_message_ids.insert(message_id) {
                return Err(SessionError::ReplayDetected);
            }
            while session.seen_webrtc_message_ids.len() > 4096 {
                let Some(oldest) = session.seen_webrtc_message_ids.iter().next().cloned() else {
                    break;
                };
                session.seen_webrtc_message_ids.remove(&oldest);
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
        // Portal lease shutdown performs D-Bus work on its own actor. Capture
        // the current bindings while the session map is locked, then release
        // them only after the lock is dropped.
        let bindings = sessions
            .values()
            .filter_map(DeviceSession::binding)
            .collect::<Vec<_>>();
        let mut peers = Vec::new();
        let links = sessions
            .values_mut()
            .filter_map(|session| {
                session.state = SessionState::Terminated;
                session.reconnect_scheduled = false;
                session.next_reconnect_at = None;
                session.cancellation.store(true, Ordering::Release);
                session.webrtc_attempt_id = None;
                session.webrtc_handover = None;
                session.seen_webrtc_request_ids.clear();
                session.seen_webrtc_message_ids.clear();
                session.remote_session.stop();
                if let Some(peer) = session.active_webrtc.take() {
                    peers.push(peer);
                }
                session.transfer_manager = None;
                session.active_link.take()
        })
            .collect();
        drop(sessions);
        for binding in &bindings {
            crate::device_links::webrtc::portal::RemotePortalRegistry::global().release(binding);
        }
        for peer in peers {
            peer.close();
        }
        links
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
    use std::time::Duration;

    use super::*;

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
    fn pairing_state_survives_transport_replacement() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap();
        let second = manager
            .register_link("phone-1".to_string(), link("phone-1"), false)
            .unwrap();

        let paired = manager
            .with_session(&second.binding, |session| {
                session.pairing.state == crate::device_links::pairing::PairState::Paired
            })
            .unwrap();

        assert!(paired);
        assert_eq!(first.binding.session_id, second.binding.session_id);
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
    fn webrtc_recovery_rearm_is_single_claimed_and_bounded() {
        let manager = DeviceManager::new();
        let binding = manager
            .register_link("phone-1".to_string(), link("phone-1"), true)
            .unwrap()
            .binding;

        let delay = manager
            .rearm_webrtc_recovery(&binding, "temporary WebRTC failure".to_string())
            .unwrap()
            .unwrap();
        assert_eq!(delay, Duration::from_secs(1));
        assert!(manager
            .rearm_webrtc_recovery(&binding, "duplicate callback".to_string())
            .unwrap()
            .is_none());
        assert!(manager
            .claim_scheduled_webrtc_recovery(&binding, Instant::now() + delay)
            .unwrap());
        assert!(!manager
            .claim_scheduled_webrtc_recovery(&binding, Instant::now() + delay)
            .unwrap());
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
}
