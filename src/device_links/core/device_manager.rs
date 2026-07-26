use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::device_links::pairing::PairingHandler;

use super::{
    ConnectionGeneration, DeviceSession, SessionBinding, SessionId, SessionLink, SessionState,
};

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    UnknownSession,
    StaleBinding,
    TerminatedSession,
    AlreadyClaimed,
    NotEligibleForReconnect,
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
            Self::Poisoned => "Device session lock poisoned",
        };
        formatter.write_str(message)
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct RegistrationResult {
    pub binding: SessionBinding,
    pub replaced_link: Option<Arc<SessionLink>>,
    pub replaced_generation: Option<ConnectionGeneration>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectLease {
    pub device_id: String,
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
}

#[derive(Debug)]
pub struct DisconnectResult {
    pub was_current: bool,
    pub link: Option<Arc<SessionLink>>,
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
            };
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return DisconnectResult {
                was_current: false,
                link: None,
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
            };
        }
        session.state = SessionState::Disconnected;
        session.last_disconnect_reason = Some(reason);
        session.connected_at = None;
        session.cancellation.store(true, Ordering::Release);
        let link = session.active_link.take();
        DisconnectResult {
            was_current: true,
            link,
        }
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
        sessions
            .values_mut()
            .filter_map(|session| {
                session.state = SessionState::Terminated;
                session.reconnect_scheduled = false;
                session.next_reconnect_at = None;
                session.cancellation.store(true, Ordering::Release);
                session.active_link.take()
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
}
