use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use super::device_session::{ConnectionGeneration, DeviceSession, SessionId, SessionState};
use super::session_link::Link;
use crate::device_links::webrtc::transport::WebRtcTransport;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    UnknownSession,
    StaleBinding,
    TerminatedSession,
    AlreadyClaimed,
    NotEligibleForReconnect,
}

#[derive(Clone)]
pub struct SessionBinding {
    pub device_id: String,
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
    pub link: Arc<Link>,
    pub cancellation: Arc<std::sync::atomic::AtomicBool>,
}

impl std::fmt::Debug for SessionBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionBinding")
            .field("device_id", &self.device_id)
            .field("session_id", &self.session_id)
            .field("generation", &self.generation)
            .finish()
    }
}

#[derive(Clone)]
pub struct RegistrationResult {
    pub binding: SessionBinding,
    pub replaced_link: Option<Arc<Link>>,
    pub replaced_generation: Option<ConnectionGeneration>,
    pub replaced_webrtc_transport: Option<Arc<WebRtcTransport>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisconnectResult {
    pub was_current: bool,
    pub reconnect_scheduled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectLease {
    pub device_id: String,
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
}

#[derive(Clone)]
pub struct WebRtcBinding {
    pub device_id: String,
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
    pub transport: Arc<WebRtcTransport>,
}

impl std::fmt::Debug for WebRtcBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WebRtcBinding")
            .field("device_id", &self.device_id)
            .field("session_id", &self.session_id)
            .field("generation", &self.generation)
            .finish()
    }
}

pub struct WebRtcRegistration {
    pub binding: WebRtcBinding,
    pub replaced_transport: Option<Arc<WebRtcTransport>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSessionSnapshot {
    pub session_id: SessionId,
    pub connection_generation: ConnectionGeneration,
    pub device_id: String,
    pub state: SessionState,
    pub pair_state: crate::device_links::pairing::PairState,
    pub reconnect_attempt: u32,
}

#[derive(Clone)]
pub struct DeviceManager {
    sessions: Arc<Mutex<HashMap<String, DeviceSession>>>,
    webrtc: Arc<Mutex<HashMap<String, WebRtcBinding>>>,
    next_session_id: Arc<AtomicU64>,
}

impl Default for DeviceManager {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceManager {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            webrtc: Arc::new(Mutex::new(HashMap::new())),
            next_session_id: Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn ensure(&self, device_id: &str) -> DeviceSession {
        let mut sessions = self.sessions.lock().expect("device session lock poisoned");
        sessions
            .entry(device_id.to_string())
            .or_insert_with(|| {
                let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
                DeviceSession::new(session_id, device_id, false)
            })
            .clone()
    }

    pub fn observe_device(&self, device_id: &str, initially_paired: bool) -> DeviceSession {
        let mut sessions = self.sessions.lock().expect("device session lock poisoned");
        let session = sessions.entry(device_id.to_string()).or_insert_with(|| {
            let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
            DeviceSession::new(session_id, device_id, initially_paired)
        });
        if initially_paired
            && session.active_link.is_none()
            && session.pair_state() != crate::device_links::pairing::PairState::Paired
        {
            session.pairing = Arc::new(Mutex::new(
                crate::device_links::pairing::PairingHandler::new(true),
            ));
        }
        if initially_paired && session.active_link.is_none() && session.next_reconnect_at.is_none()
        {
            session.schedule_reconnect(Instant::now());
        }
        session.clone()
    }

    pub fn schedule_reconnect(&self, device_id: &str, now: Instant) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(device_id) {
                session.schedule_reconnect(now);
            }
        }
    }

    pub fn register_link(
        &self,
        device_id: String,
        link: Link,
        initially_paired: bool,
    ) -> Result<RegistrationResult, SessionError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SessionError::UnknownSession)?;
        let session = sessions.entry(device_id.clone()).or_insert_with(|| {
            let session_id = self.next_session_id.fetch_add(1, Ordering::Relaxed);
            DeviceSession::new(session_id, device_id.clone(), initially_paired)
        });
        if session.is_terminated() {
            return Err(SessionError::TerminatedSession);
        }

        let replaced_link = session.active_link.take();
        let replaced_generation = if replaced_link.is_some() {
            Some(session.connection_generation)
        } else {
            None
        };
        if replaced_link.is_some() {
            session.cancellation.store(true, Ordering::SeqCst);
        }
        session.connection_generation = session.connection_generation.saturating_add(1);
        session.cancellation = Arc::new(std::sync::atomic::AtomicBool::new(false));
        session.active_link = Some(Arc::new(link));
        session.connected_at = Some(Instant::now());
        session.last_disconnect_reason = None;
        session.clear_reconnect();
        session.transition(SessionState::Ready);
        let binding = SessionBinding {
            device_id: device_id.clone(),
            session_id: session.session_id,
            generation: session.connection_generation,
            link: session
                .active_link
                .as_ref()
                .expect("active link installed")
                .clone(),
            cancellation: Arc::clone(&session.cancellation),
        };
        let replaced_webrtc_transport = self
            .webrtc
            .lock()
            .ok()
            .and_then(|mut bindings| bindings.remove(&device_id))
            .map(|binding| binding.transport);
        Ok(RegistrationResult {
            binding,
            replaced_link,
            replaced_generation,
            replaced_webrtc_transport,
        })
    }

    pub fn current_binding(&self, device_id: &str) -> Option<SessionBinding> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(device_id)?;
        let link = session.active_link.as_ref()?.clone();
        Some(SessionBinding {
            device_id: device_id.to_string(),
            session_id: session.session_id,
            generation: session.connection_generation,
            link,
            cancellation: Arc::clone(&session.cancellation),
        })
    }

    pub fn is_current(&self, binding: &SessionBinding) -> bool {
        let Some(session) = self
            .sessions
            .lock()
            .ok()
            .and_then(|sessions| sessions.get(&binding.device_id).cloned())
        else {
            return false;
        };
        session.session_id == binding.session_id
            && session.connection_generation == binding.generation
            && session
                .active_link
                .as_ref()
                .is_some_and(|link| Arc::ptr_eq(link, &binding.link))
            && Arc::ptr_eq(&session.cancellation, &binding.cancellation)
            && !session.is_terminated()
    }

    pub fn with_session<R>(
        &self,
        binding: &SessionBinding,
        operation: impl FnOnce(&mut DeviceSession) -> R,
    ) -> Result<R, SessionError> {
        let mut sessions = self
            .sessions
            .lock()
            .map_err(|_| SessionError::UnknownSession)?;
        let session = sessions
            .get_mut(&binding.device_id)
            .ok_or(SessionError::UnknownSession)?;
        if session.is_terminated() {
            return Err(SessionError::TerminatedSession);
        }
        if session.session_id != binding.session_id
            || session.connection_generation != binding.generation
            || session
                .active_link
                .as_ref()
                .is_none_or(|link| !Arc::ptr_eq(link, &binding.link))
        {
            return Err(SessionError::StaleBinding);
        }
        Ok(operation(session))
    }

    pub fn with_pairing<R>(
        &self,
        binding: &SessionBinding,
        operation: impl FnOnce(&mut crate::device_links::pairing::PairingHandler) -> R,
    ) -> Result<R, SessionError> {
        let result = self.with_session(binding, |session| match session.pairing.lock() {
            Ok(mut pairing) => Ok(operation(&mut pairing)),
            Err(_) => Err(()),
        })?;
        result.map_err(|_| SessionError::UnknownSession)
    }

    pub fn pairing_state(
        &self,
        binding: &SessionBinding,
    ) -> Result<crate::device_links::pairing::PairState, SessionError> {
        self.with_session(binding, |session| session.pair_state())
    }

    pub fn verification_key(&self, binding: &SessionBinding) -> Result<String, SessionError> {
        self.with_session(binding, |session| {
            crate::device_links::pairing::PairingHandler::verification_key(
                &binding.link.local_public_der,
                &binding.link.remote_public_der,
                session
                    .pairing
                    .lock()
                    .map(|pairing| pairing.timestamp())
                    .unwrap_or_default(),
            )
        })
    }

    pub fn disconnect_if_current(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> DisconnectResult {
        let Ok(mut sessions) = self.sessions.lock() else {
            return DisconnectResult {
                was_current: false,
                reconnect_scheduled: false,
            };
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return DisconnectResult {
                was_current: false,
                reconnect_scheduled: false,
            };
        };
        if session.session_id != binding.session_id
            || session.connection_generation != binding.generation
            || session
                .active_link
                .as_ref()
                .is_none_or(|link| !Arc::ptr_eq(link, &binding.link))
        {
            return DisconnectResult {
                was_current: false,
                reconnect_scheduled: false,
            };
        }
        session.active_link = None;
        session.cancellation.store(true, Ordering::SeqCst);
        session.last_disconnect_reason = Some(reason);
        session.connected_at = None;
        session.transition(SessionState::Disconnected);
        if session.pair_state() == crate::device_links::pairing::PairState::Paired {
            session.schedule_reconnect(Instant::now());
        }
        DisconnectResult {
            was_current: true,
            reconnect_scheduled: session.next_reconnect_at.is_some(),
        }
    }

    /// Installs one WebRTC transport only for the currently authenticated LAN
    /// generation. The caller can later replace the LAN transport after the
    /// WebRTC transcript and channel negotiation have completed.
    pub fn register_webrtc_transport(
        &self,
        binding: &SessionBinding,
        transport: WebRtcTransport,
    ) -> Result<WebRtcRegistration, SessionError> {
        if !self.is_current(binding) {
            return Err(SessionError::StaleBinding);
        }
        let transport = Arc::new(transport);
        let web_rtc_binding = WebRtcBinding {
            device_id: binding.device_id.clone(),
            session_id: binding.session_id,
            generation: binding.generation,
            transport,
        };
        let replaced_binding = self
            .webrtc
            .lock()
            .map_err(|_| SessionError::UnknownSession)?
            .insert(binding.device_id.clone(), web_rtc_binding.clone());
        if let Some(previous) = &replaced_binding {
            previous.transport.close();
        }
        Ok(WebRtcRegistration {
            binding: web_rtc_binding,
            replaced_transport: replaced_binding.map(|binding| binding.transport),
        })
    }

    pub fn current_webrtc_binding(&self, device_id: &str) -> Option<WebRtcBinding> {
        self.webrtc.lock().ok()?.get(device_id).cloned()
    }

    pub fn is_current_webrtc(&self, binding: &WebRtcBinding) -> bool {
        let Some(current_session) = self.current_binding(&binding.device_id) else {
            return false;
        };
        current_session.session_id == binding.session_id
            && current_session.generation == binding.generation
            && self
                .webrtc
                .lock()
                .ok()
                .and_then(|bindings| bindings.get(&binding.device_id).cloned())
                .is_some_and(|current| {
                    current.session_id == binding.session_id
                        && current.generation == binding.generation
                        && Arc::ptr_eq(&current.transport, &binding.transport)
                })
    }

    pub fn clear_webrtc_if_current(&self, binding: &WebRtcBinding) -> bool {
        let removed = self.webrtc.lock().ok().and_then(|mut bindings| {
            let current = bindings.get(&binding.device_id)?;
            if current.session_id != binding.session_id
                || current.generation != binding.generation
                || !Arc::ptr_eq(&current.transport, &binding.transport)
            {
                return None;
            }
            bindings.remove(&binding.device_id)
        });
        if let Some(current) = removed {
            current.transport.close();
            true
        } else {
            false
        }
    }

    pub fn terminate_all_webrtc(&self) -> Vec<Arc<WebRtcTransport>> {
        let transports: Vec<Arc<WebRtcTransport>> = self
            .webrtc
            .lock()
            .map(|mut bindings| {
                bindings
                    .drain()
                    .map(|(_, binding)| binding.transport)
                    .collect()
            })
            .unwrap_or_default();
        transports.iter().for_each(|transport| transport.close());
        transports
    }

    pub fn claim_reconnect(&self, device_id: &str, now: Instant) -> Option<ReconnectLease> {
        if let Ok(mut sessions) = self.sessions.lock() {
            let session = sessions.get_mut(device_id)?;
            if session.is_terminated() || session.active_link.is_some() {
                return None;
            }
            if !session.reconnect_due(now) {
                return None;
            }
            session.reconnect_scheduled = true;
            session.transition(SessionState::Reconnecting);
            return Some(ReconnectLease {
                device_id: device_id.to_string(),
                session_id: session.session_id,
                generation: session.connection_generation,
            });
        }
        None
    }

    pub fn reconnect_succeeded(&self, lease: &ReconnectLease) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(&lease.device_id) {
                if session.session_id == lease.session_id
                    && session.connection_generation == lease.generation
                {
                    session.reconnect_succeeded();
                }
            }
        }
    }

    pub fn reconnect_failed(&self, lease: &ReconnectLease, _reason: String) {
        if let Ok(mut sessions) = self.sessions.lock() {
            if let Some(session) = sessions.get_mut(&lease.device_id) {
                if session.session_id == lease.session_id
                    && session.connection_generation == lease.generation
                {
                    session.reconnect_failed(Instant::now());
                }
            }
        }
    }

    pub fn terminate_all(&self) -> Vec<Arc<Link>> {
        let Ok(mut sessions) = self.sessions.lock() else {
            return Vec::new();
        };
        let mut links = Vec::new();
        for session in sessions.values_mut() {
            session.transition(SessionState::Terminated);
            session.cancellation.store(true, Ordering::SeqCst);
            session.clear_reconnect();
            if let Some(link) = session.active_link.take() {
                links.push(link);
            }
        }
        links
    }

    pub fn sessions_snapshot(&self) -> Vec<DeviceSessionSnapshot> {
        self.sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .map(|session| DeviceSessionSnapshot {
                        session_id: session.session_id,
                        connection_generation: session.connection_generation,
                        device_id: session.device_id.clone(),
                        state: session.state.clone(),
                        pair_state: session.pair_state(),
                        reconnect_attempt: session.reconnect_attempt,
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn unpaired_count(&self) -> usize {
        self.sessions
            .lock()
            .map(|sessions| {
                sessions
                    .values()
                    .filter(|session| {
                        session.pair_state() != crate::device_links::pairing::PairState::Paired
                    })
                    .count()
            })
            .unwrap_or_default()
    }

    pub fn get(&self, device_id: &str) -> Option<DeviceSession> {
        self.sessions.lock().ok()?.get(device_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::Link;
    use super::*;
    use crate::device_links::device_info::DeviceInfo;
    use crate::device_links::pairing::PairState;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    const DEVICE_ID: &str = "0123456789abcdef0123456789abcdef";

    fn link() -> Link {
        Link::test_link(DeviceInfo::local(
            DEVICE_ID.to_string(),
            "DeskLink test".to_string(),
        ))
    }

    #[test]
    fn first_registration_creates_one_ready_session() {
        let manager = DeviceManager::new();

        let registration = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("first link should register");
        let snapshots = manager.sessions_snapshot();

        assert_eq!(snapshots.len(), 1);
        assert_eq!(snapshots[0].session_id, registration.binding.session_id);
        assert_eq!(snapshots[0].connection_generation, 1);
        assert_eq!(snapshots[0].state, SessionState::Ready);
        assert_eq!(snapshots[0].pair_state, PairState::Paired);
        assert!(manager.is_current(&registration.binding));
    }

    #[test]
    fn replacement_keeps_session_id_and_increments_generation() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("first link should register");
        let second = manager
            .register_link(DEVICE_ID.to_string(), link(), false)
            .expect("replacement link should register");

        assert_eq!(first.binding.session_id, second.binding.session_id);
        assert_eq!(first.binding.generation, 1);
        assert_eq!(second.binding.generation, 2);
        assert_eq!(second.replaced_generation, Some(1));
        assert!(second.replaced_link.is_some());
        assert!(first.binding.cancellation.load(Ordering::SeqCst));
        assert!(!second.binding.cancellation.load(Ordering::SeqCst));
        assert!(!manager.is_current(&first.binding));
        assert!(manager.is_current(&second.binding));
        assert_eq!(
            manager.get(DEVICE_ID).unwrap().pair_state(),
            PairState::Paired
        );
    }

    #[test]
    fn one_webrtc_transport_is_owned_per_current_session_generation() {
        let manager = DeviceManager::new();
        let lan = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("LAN link should register");
        let first = manager
            .register_webrtc_transport(
                &lan.binding,
                WebRtcTransport::new(DEVICE_ID, lan.binding.session_id, lan.binding.generation)
                    .unwrap(),
            )
            .unwrap();
        assert!(manager.is_current_webrtc(&first.binding));

        let second = manager
            .register_webrtc_transport(
                &lan.binding,
                WebRtcTransport::new(DEVICE_ID, lan.binding.session_id, lan.binding.generation)
                    .unwrap(),
            )
            .unwrap();
        assert!(second.replaced_transport.is_some());
        assert!(!manager.is_current_webrtc(&first.binding));
        assert!(manager.is_current_webrtc(&second.binding));
        assert!(!manager.clear_webrtc_if_current(&first.binding));
        assert!(manager.clear_webrtc_if_current(&second.binding));
        assert!(manager.current_webrtc_binding(DEVICE_ID).is_none());
    }

    #[test]
    fn stale_binding_cannot_disconnect_or_mutate_session() {
        let manager = DeviceManager::new();
        let first = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("first link should register");
        let second = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("replacement link should register");

        assert_eq!(
            manager.disconnect_if_current(&first.binding, "stale".to_string()),
            DisconnectResult {
                was_current: false,
                reconnect_scheduled: false,
            }
        );
        assert_eq!(
            manager.with_session(&first.binding, |_| ()),
            Err(SessionError::StaleBinding)
        );
        assert!(manager.is_current(&second.binding));
        assert_eq!(manager.get(DEVICE_ID).unwrap().state, SessionState::Ready);
    }

    #[test]
    fn current_disconnect_schedules_reconnect_only_for_paired_session() {
        let manager = DeviceManager::new();
        let paired = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("paired link should register");
        let result = manager.disconnect_if_current(&paired.binding, "network loss".to_string());

        assert!(result.was_current);
        assert!(result.reconnect_scheduled);
        let session = manager.get(DEVICE_ID).unwrap();
        assert_eq!(session.state, SessionState::Disconnected);
        assert_eq!(
            session.last_disconnect_reason.as_deref(),
            Some("network loss")
        );

        let unpaired_id = "abcdefabcdefabcdefabcdefabcdefab";
        let unpaired = manager
            .register_link(unpaired_id.to_string(), link(), false)
            .expect("unpaired link should register");
        let result = manager.disconnect_if_current(&unpaired.binding, "closed".to_string());
        assert!(result.was_current);
        assert!(!result.reconnect_scheduled);
        assert!(manager
            .claim_reconnect(unpaired_id, Instant::now())
            .is_none());
    }

    #[test]
    fn only_one_reconnect_lease_can_be_claimed() {
        let manager = DeviceManager::new();
        manager.observe_device(DEVICE_ID, true);
        let now = Instant::now() + Duration::from_millis(1);

        let lease = manager
            .claim_reconnect(DEVICE_ID, now)
            .expect("paired session should have one due lease");
        assert!(manager.claim_reconnect(DEVICE_ID, now).is_none());
        assert_eq!(lease.session_id, manager.get(DEVICE_ID).unwrap().session_id);
        assert_eq!(
            manager.get(DEVICE_ID).unwrap().state,
            SessionState::Reconnecting
        );
    }

    #[test]
    fn successful_registration_invalidates_old_reconnect_lease() {
        let manager = DeviceManager::new();
        manager.observe_device(DEVICE_ID, true);
        let lease = manager
            .claim_reconnect(DEVICE_ID, Instant::now())
            .expect("reconnect lease should be claimable");
        let registration = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("reconnect should register");

        assert_eq!(registration.binding.generation, lease.generation + 1);
        manager.reconnect_failed(&lease, "late failure".to_string());
        let session = manager.get(DEVICE_ID).unwrap();
        assert_eq!(session.state, SessionState::Ready);
        assert_eq!(session.reconnect_attempt, 0);
        assert!(session.next_reconnect_at.is_none());
    }

    #[test]
    fn stale_reconnect_lease_cannot_update_newer_generation() {
        let manager = DeviceManager::new();
        manager.observe_device(DEVICE_ID, true);
        let lease = manager
            .claim_reconnect(DEVICE_ID, Instant::now())
            .expect("reconnect lease should be claimable");
        let registration = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("new generation should register");

        manager.reconnect_failed(&lease, "stale failure".to_string());
        assert!(manager.is_current(&registration.binding));
        assert_eq!(manager.get(DEVICE_ID).unwrap().state, SessionState::Ready);
    }

    #[test]
    fn terminated_sessions_reject_new_links_and_reconnect() {
        let manager = DeviceManager::new();
        let registration = manager
            .register_link(DEVICE_ID.to_string(), link(), true)
            .expect("link should register");
        let terminated_links = manager.terminate_all();

        assert_eq!(terminated_links.len(), 1);
        assert!(registration.binding.cancellation.load(Ordering::SeqCst));
        assert!(manager.current_binding(DEVICE_ID).is_none());
        assert!(manager.claim_reconnect(DEVICE_ID, Instant::now()).is_none());
        assert!(matches!(
            manager.register_link(DEVICE_ID.to_string(), link(), true),
            Err(SessionError::TerminatedSession)
        ));
    }

    #[test]
    fn concurrent_registration_converges_to_one_current_binding() {
        let manager = Arc::new(DeviceManager::new());
        let workers = (0..8)
            .map(|_| {
                let manager = Arc::clone(&manager);
                thread::spawn(move || {
                    manager
                        .register_link(DEVICE_ID.to_string(), link(), true)
                        .expect("concurrent registration should succeed")
                })
            })
            .collect::<Vec<_>>();
        let registrations = workers
            .into_iter()
            .map(|worker| worker.join().expect("registration worker should finish"))
            .collect::<Vec<_>>();

        let current = manager
            .current_binding(DEVICE_ID)
            .expect("one current binding should remain");
        assert_eq!(manager.sessions_snapshot().len(), 1);
        assert_eq!(current.generation, 8);
        assert_eq!(
            registrations
                .iter()
                .filter(|registration| manager.is_current(&registration.binding))
                .count(),
            1
        );
    }
}
