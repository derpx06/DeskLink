//! Authenticated remote-view and remote-input session control.
//!
//! A DeskLink WebRTC peer may carry many feature packets, but it may carry
//! only one remote-control lease at a time.  This module deliberately keeps
//! that policy independent from GTK, portals, and GStreamer so a stale peer
//! generation cannot keep injecting input after a replacement connection.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::WebRtcWireBinding;

pub const REMOTE_SESSION_VERSION: u32 = 1;
pub const REMOTE_SESSION_MESSAGE_TYPE: &str = "desklink.remote-session.v1";
/// Body fields carried by the existing DeskLink mousepad/presenter packet
/// only when the packet is transported over an authenticated WebRTC input
/// channel.  They are deliberately not part of the legacy bootstrap protocol.
pub const REMOTE_SESSION_ID_FIELD: &str = "desklinkRemoteSessionId";
pub const LEASE_ID_FIELD: &str = "desklinkLeaseId";
pub const INPUT_SEQUENCE_FIELD: &str = "desklinkInputSequence";
const MAX_MESSAGE_AGE: Duration = Duration::from_secs(60);
const DEFAULT_LEASE_DURATION: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteSessionState {
    Idle,
    RequestingView,
    Viewing,
    RequestingControl,
    Controlling,
    PausedLocked,
    Reconnecting,
    Denied,
    Failed,
    Stopped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScreenDirection {
    PhoneToDesktop,
    DesktopToPhone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RemoteSessionControlKind {
    RequestView,
    ViewGranted,
    ViewDenied,
    RequestControl,
    ControlGranted,
    ControlDenied,
    TakeoverRequest,
    TakeoverGranted,
    Release,
    Heartbeat,
    ScreenReady,
    ScreenStopped,
    ScreenError,
    PauseLocked,
    Resume,
}

/// A generation-bound control message sent exclusively on
/// `desklink-control-v1` after mutual feature readiness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteSessionControlMessage {
    pub remote_session_version: u32,
    pub kind: RemoteSessionControlKind,
    pub session_attempt_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub remote_session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub direction: Option<ScreenDirection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner_device_id: Option<String>,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lease_expires_at: Option<i64>,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_height: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screen_rotation: Option<u16>,
}

impl RemoteSessionControlMessage {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        kind: RemoteSessionControlKind,
        attempt_id: impl Into<String>,
        binding: &WebRtcWireBinding,
        remote_session_id: impl Into<String>,
        sequence: u64,
        direction: Option<ScreenDirection>,
        lease_id: Option<String>,
        owner_device_id: Option<String>,
        lease_expires_at: Option<i64>,
        reason: Option<String>,
    ) -> Self {
        Self {
            remote_session_version: REMOTE_SESSION_VERSION,
            kind,
            session_attempt_id: attempt_id.into(),
            device_id: binding.sender_device_id.clone(),
            session_id: binding.session_id,
            connection_generation: binding.generation,
            remote_session_id: remote_session_id.into(),
            lease_id,
            direction,
            owner_device_id,
            sequence,
            lease_expires_at,
            timestamp: now_millis(),
            reason,
            screen_width: None,
            screen_height: None,
            screen_rotation: None,
        }
    }

    pub fn validate(
        &self,
        binding: &WebRtcWireBinding,
        attempt_id: &str,
        now: i64,
    ) -> Result<(), String> {
        if self.remote_session_version != REMOTE_SESSION_VERSION {
            return Err("Unsupported DeskLink remote-session version".to_string());
        }
        if self.session_attempt_id != attempt_id
            || self.device_id != binding.peer_device_id
            || self.session_id != binding.session_id
            || self.connection_generation != binding.generation
        {
            return Err("DeskLink remote-session binding mismatch".to_string());
        }
        if self.remote_session_id.is_empty()
            || Uuid::parse_str(&self.remote_session_id).is_err()
            || self.sequence == 0
        {
            return Err("Malformed DeskLink remote-session control message".to_string());
        }
        if now.abs_diff(self.timestamp) > MAX_MESSAGE_AGE.as_millis() as u64 {
            return Err("Expired DeskLink remote-session control message".to_string());
        }
        if let Some(reason) = &self.reason {
            if reason.len() > 1024 {
                return Err("DeskLink remote-session error is too large".to_string());
            }
        }
        if self.screen_width.is_some() != self.screen_height.is_some() {
            return Err("DeskLink remote screen geometry is incomplete".to_string());
        }
        if self.screen_rotation.is_some() && self.screen_width.is_none() {
            return Err("DeskLink remote screen rotation has no geometry".to_string());
        }
        if let (Some(width), Some(height)) = (self.screen_width, self.screen_height) {
            if width == 0 || height == 0 || width > 8192 || height > 8192 {
                return Err("DeskLink remote screen geometry is invalid".to_string());
            }
            if let Some(rotation) = self.screen_rotation {
                if !matches!(rotation, 0 | 90 | 180 | 270) {
                    return Err("DeskLink remote screen rotation is invalid".to_string());
                }
            }
        }
        Ok(())
    }

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > 16 * 1024 {
            return Err("DeskLink remote-session control message is too large".to_string());
        }
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct RemoteControlLease {
    pub remote_session_id: String,
    pub lease_id: String,
    pub owner_device_id: String,
    pub generation: u64,
    pub expires_at: i64,
    pub last_sequence: u64,
    pub pressed_buttons: u32,
    pub pressed_keys: Vec<String>,
}

impl RemoteControlLease {
    pub fn is_expired(&self, now: i64) -> bool {
        now >= self.expires_at
    }

    pub fn accept_input(
        &mut self,
        remote_session_id: &str,
        lease_id: &str,
        peer_device_id: &str,
        generation: u64,
        sequence: u64,
        now: i64,
    ) -> Result<(), String> {
        if self.is_expired(now) {
            return Err("DeskLink remote-control lease expired".to_string());
        }
        if self.remote_session_id != remote_session_id
            || self.lease_id != lease_id
            || self.owner_device_id != peer_device_id
            || self.generation != generation
        {
            return Err("DeskLink remote-control lease does not own this input".to_string());
        }
        if sequence <= self.last_sequence {
            return Err("Stale or replayed DeskLink remote-control input".to_string());
        }
        self.last_sequence = sequence;
        self.expires_at = now + DEFAULT_LEASE_DURATION.as_millis() as i64;
        Ok(())
    }
}

/// Per-device state machine used by the session owner. It deliberately has a
/// small API: callers can start/stop a view and accept a single, verified
/// control lease. Platform implementations perform portal or Android consent
/// *after* this model has accepted the request, never from raw input events.
#[derive(Debug, Clone)]
pub struct RemoteSession {
    pub state: RemoteSessionState,
    pub direction: Option<ScreenDirection>,
    pub remote_session_id: Option<String>,
    pub control_lease: Option<RemoteControlLease>,
    next_sequence: u64,
    heartbeat_generation: u64,
}

impl Default for RemoteSession {
    fn default() -> Self {
        Self {
            state: RemoteSessionState::Idle,
            direction: None,
            remote_session_id: None,
            control_lease: None,
            next_sequence: 1,
            heartbeat_generation: 0,
        }
    }
}

impl RemoteSession {
    pub fn start_view(&mut self, direction: ScreenDirection) -> String {
        self.release_input();
        let id = Uuid::new_v4().to_string();
        self.remote_session_id = Some(id.clone());
        self.direction = Some(direction);
        self.state = RemoteSessionState::RequestingView;
        id
    }

    pub fn next_sequence(&mut self) -> u64 {
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.checked_add(1).unwrap_or(1).max(1);
        sequence
    }

    pub fn mark_viewing(&mut self, remote_session_id: &str, direction: ScreenDirection) -> Result<(), String> {
        self.require_session(remote_session_id)?;
        self.direction = Some(direction);
        self.state = RemoteSessionState::Viewing;
        Ok(())
    }

    pub fn request_control(&mut self) -> Result<String, String> {
        if !matches!(
            self.state,
            RemoteSessionState::Viewing | RemoteSessionState::RequestingControl
        ) {
            return Err("DeskLink control requires an active remote view".to_string());
        }
        self.state = RemoteSessionState::RequestingControl;
        self.remote_session_id
            .clone()
            .ok_or_else(|| "DeskLink remote view has no session ID".to_string())
    }

    pub fn grant_control(
        &mut self,
        remote_session_id: &str,
        owner_device_id: String,
        generation: u64,
        now: i64,
    ) -> Result<RemoteControlLease, String> {
        self.require_session(remote_session_id)?;
        if !matches!(self.state, RemoteSessionState::Viewing | RemoteSessionState::RequestingControl | RemoteSessionState::Controlling) {
            return Err("DeskLink control can only be granted for an active view".to_string());
        }
        self.release_input();
        let lease = RemoteControlLease {
            remote_session_id: remote_session_id.to_string(),
            lease_id: Uuid::new_v4().to_string(),
            owner_device_id,
            generation,
            expires_at: now + DEFAULT_LEASE_DURATION.as_millis() as i64,
            last_sequence: 0,
            pressed_buttons: 0,
            pressed_keys: Vec::new(),
        };
        self.control_lease = Some(lease.clone());
        self.state = RemoteSessionState::Controlling;
        self.heartbeat_generation = self.heartbeat_generation.wrapping_add(1);
        Ok(lease)
    }

    pub fn accept_control_message(
        &mut self,
        message: &RemoteSessionControlMessage,
        peer_device_id: &str,
        generation: u64,
        now: i64,
    ) -> Result<(), String> {
        match message.kind {
            RemoteSessionControlKind::RequestView => {
                if self.control_lease.is_some() {
                    return Err("DeskLink remote session is controlled by another device".to_string());
                }
                self.remote_session_id = Some(message.remote_session_id.clone());
                self.direction = message.direction;
                self.state = RemoteSessionState::RequestingView;
            }
            RemoteSessionControlKind::ViewGranted | RemoteSessionControlKind::ScreenReady => {
                self.mark_viewing(
                    &message.remote_session_id,
                    message.direction.ok_or_else(|| "DeskLink remote view has no direction".to_string())?,
                )?;
            }
            RemoteSessionControlKind::RequestControl => {
                self.require_session(&message.remote_session_id)?;
                self.state = RemoteSessionState::RequestingControl;
            }
            RemoteSessionControlKind::ControlGranted | RemoteSessionControlKind::TakeoverGranted => {
                self.require_session(&message.remote_session_id)?;
                let lease_id = message.lease_id.clone().ok_or_else(|| "DeskLink control grant has no lease".to_string())?;
                let expires_at = message.lease_expires_at.ok_or_else(|| "DeskLink control grant has no expiry".to_string())?;
                if expires_at <= now {
                    return Err("DeskLink control grant already expired".to_string());
                }
                self.control_lease = Some(RemoteControlLease {
                    remote_session_id: message.remote_session_id.clone(),
                    lease_id,
                    owner_device_id: message.owner_device_id.clone().unwrap_or_else(|| peer_device_id.to_string()),
                    generation,
                    expires_at,
                    last_sequence: 0,
                    pressed_buttons: 0,
                    pressed_keys: Vec::new(),
                });
                self.state = RemoteSessionState::Controlling;
                self.heartbeat_generation = self.heartbeat_generation.wrapping_add(1);
            }
            RemoteSessionControlKind::Heartbeat => {
                if let (Some(lease), Some(lease_id)) = (&mut self.control_lease, &message.lease_id) {
                    lease.accept_input(
                        &message.remote_session_id,
                        lease_id,
                        peer_device_id,
                        generation,
                        message.sequence,
                        now,
                    )?;
                }
            }
            RemoteSessionControlKind::PauseLocked => {
                self.require_session(&message.remote_session_id)?;
                self.release_input();
                self.state = RemoteSessionState::PausedLocked;
            }
            RemoteSessionControlKind::Resume => {
                self.require_session(&message.remote_session_id)?;
                self.state = RemoteSessionState::Viewing;
            }
            RemoteSessionControlKind::Release
            | RemoteSessionControlKind::ScreenStopped
            | RemoteSessionControlKind::ViewDenied
            | RemoteSessionControlKind::ControlDenied
            | RemoteSessionControlKind::ScreenError => {
                self.release_input();
                self.state = match message.kind {
                    RemoteSessionControlKind::ViewDenied | RemoteSessionControlKind::ControlDenied => RemoteSessionState::Denied,
                    RemoteSessionControlKind::ScreenError => RemoteSessionState::Failed,
                    _ => RemoteSessionState::Stopped,
                };
            }
            RemoteSessionControlKind::TakeoverRequest => {
                self.require_session(&message.remote_session_id)?;
                self.state = RemoteSessionState::RequestingControl;
            }
        }
        Ok(())
    }

    pub fn release_input(&mut self) -> Option<RemoteControlLease> {
        let lease = self.control_lease.take();
        if lease.is_some() {
            self.heartbeat_generation = self.heartbeat_generation.wrapping_add(1);
        }
        lease
    }

    /// Stops input immediately while retaining the view identity. This is the
    /// state used when GNOME closes or revokes a portal session: the peer can
    /// show a recoverable "Retry permission" action without receiving any
    /// more pointer or key events from the stale lease.
    pub fn pause(&mut self) -> Option<String> {
        self.release_input();
        self.state = RemoteSessionState::PausedLocked;
        self.remote_session_id.clone()
    }

    /// Returns an epoch for a periodic heartbeat only when this endpoint owns
    /// the current input lease. A peer replacement, release, pause, or
    /// takeover changes the epoch, making a previously spawned worker inert.
    pub fn local_heartbeat_epoch(
        &self,
        local_device_id: &str,
        generation: u64,
    ) -> Option<u64> {
        let lease = self.control_lease.as_ref()?;
        (self.state == RemoteSessionState::Controlling
            && lease.owner_device_id == local_device_id
            && lease.generation == generation
            && !lease.is_expired(now_millis()))
        .then_some(self.heartbeat_generation)
    }

    /// Creates the state needed for one renewal heartbeat. The caller sends
    /// the message after releasing the session lock.
    pub fn prepare_local_heartbeat(
        &mut self,
        local_device_id: &str,
        generation: u64,
        heartbeat_epoch: u64,
    ) -> Option<(String, String, u64)> {
        if self.local_heartbeat_epoch(local_device_id, generation) != Some(heartbeat_epoch) {
            return None;
        }
        let (remote_session_id, lease_id) = {
            let lease = self.control_lease.as_ref()?;
            (lease.remote_session_id.clone(), lease.lease_id.clone())
        };
        let sequence = self.next_sequence();
        Some((remote_session_id, lease_id, sequence))
    }

    pub fn prepare_outbound_input(
        &mut self,
        local_device_id: &str,
        generation: u64,
    ) -> Result<(String, String, u64), String> {
        if self.state != RemoteSessionState::Controlling {
            return Err("DeskLink remote control is not active".to_string());
        }
        let lease = self
            .control_lease
            .as_ref()
            .ok_or_else(|| "DeskLink remote control has no active lease".to_string())?;
        if lease.owner_device_id != local_device_id || lease.generation != generation {
            return Err("DeskLink remote-control lease does not belong to this peer".to_string());
        }
        if lease.is_expired(now_millis()) {
            return Err("DeskLink remote-control lease expired".to_string());
        }
        Ok((
            lease.remote_session_id.clone(),
            lease.lease_id.clone(),
            self.next_sequence(),
        ))
    }

    pub fn stop(&mut self) -> Option<RemoteControlLease> {
        self.state = RemoteSessionState::Stopped;
        self.direction = None;
        self.remote_session_id = None;
        self.release_input()
    }

    fn require_session(&self, remote_session_id: &str) -> Result<(), String> {
        if self.remote_session_id.as_deref() != Some(remote_session_id) {
            return Err("DeskLink remote-session ID is stale".to_string());
        }
        Ok(())
    }
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn binding(sender: &str, peer: &str) -> WebRtcWireBinding {
        WebRtcWireBinding::from_attempt(sender, peer, "01234567-89ab-cdef-0123-456789abcdef")
            .unwrap()
    }

    #[test]
    fn a_stale_lease_cannot_accept_input_after_takeover() {
        let mut session = RemoteSession::default();
        let id = session.start_view(ScreenDirection::PhoneToDesktop);
        session.mark_viewing(&id, ScreenDirection::PhoneToDesktop).unwrap();
        let old = session.grant_control(&id, "phone".to_string(), 1, 10).unwrap();
        let current = session.grant_control(&id, "desktop".to_string(), 1, 10).unwrap();
        assert_ne!(old.lease_id, current.lease_id);
        assert!(session.control_lease.as_mut().unwrap().accept_input(
            &id, &old.lease_id, "phone", 1, 1, 11,
        ).is_err());
    }

    #[test]
    fn control_message_is_bound_to_the_authenticated_peer() {
        let local = binding("desktop", "phone");
        let remote = binding("phone", "desktop");
        let message = RemoteSessionControlMessage::new(
            RemoteSessionControlKind::RequestView,
            "01234567-89ab-cdef-0123-456789abcdef",
            &local,
            Uuid::new_v4().to_string(),
            1,
            Some(ScreenDirection::PhoneToDesktop),
            None,
            None,
            None,
            None,
        );
        let parsed = RemoteSessionControlMessage::from_json(&message.to_json().unwrap()).unwrap();
        parsed.validate(&remote, "01234567-89ab-cdef-0123-456789abcdef", now_millis()).unwrap();
    }

    #[test]
    fn pause_releases_control_and_blocks_new_input() {
        let mut session = RemoteSession::default();
        let id = session.start_view(ScreenDirection::DesktopToPhone);
        session
            .mark_viewing(&id, ScreenDirection::DesktopToPhone)
            .unwrap();
        session
            .grant_control(&id, "phone".to_string(), 1, now_millis())
            .unwrap();

        let pause = RemoteSessionControlMessage {
            remote_session_version: REMOTE_SESSION_VERSION,
            kind: RemoteSessionControlKind::PauseLocked,
            session_attempt_id: "attempt".to_string(),
            device_id: "phone".to_string(),
            session_id: 1,
            connection_generation: 1,
            remote_session_id: id,
            lease_id: None,
            direction: Some(ScreenDirection::DesktopToPhone),
            owner_device_id: None,
            sequence: 1,
            lease_expires_at: None,
            timestamp: now_millis(),
            reason: None,
            screen_width: None,
            screen_height: None,
            screen_rotation: None,
        };
        session
            .accept_control_message(&pause, "phone", 1, now_millis())
            .unwrap();

        assert_eq!(session.state, RemoteSessionState::PausedLocked);
        assert!(session.control_lease.is_none());
        assert!(session.prepare_outbound_input("phone", 1).is_err());
    }

    #[test]
    fn replayed_input_sequence_is_rejected_without_extending_the_lease() {
        let mut session = RemoteSession::default();
        let id = session.start_view(ScreenDirection::PhoneToDesktop);
        session
            .mark_viewing(&id, ScreenDirection::PhoneToDesktop)
            .unwrap();
        let lease = session
            .grant_control(&id, "phone".to_string(), 1, 100)
            .unwrap();
        let active = session.control_lease.as_mut().unwrap();
        active
            .accept_input(&id, &lease.lease_id, "phone", 1, 9, 101)
            .unwrap();
        let expiry_after_first_input = active.expires_at;
        assert!(active
            .accept_input(&id, &lease.lease_id, "phone", 1, 9, 102)
            .is_err());
        assert_eq!(active.expires_at, expiry_after_first_input);
    }

    #[test]
    fn portal_pause_preserves_view_identity_but_revokes_the_lease() {
        let mut session = RemoteSession::default();
        let id = session.start_view(ScreenDirection::DesktopToPhone);
        session
            .mark_viewing(&id, ScreenDirection::DesktopToPhone)
            .unwrap();
        session
            .grant_control(&id, "phone".to_string(), 1, now_millis())
            .unwrap();

        assert_eq!(session.pause().as_deref(), Some(id.as_str()));
        assert_eq!(session.state, RemoteSessionState::PausedLocked);
        assert_eq!(session.remote_session_id.as_deref(), Some(id.as_str()));
        assert!(session.control_lease.is_none());
    }

    #[test]
    fn screen_ready_geometry_is_bounded_and_round_trips() {
        let wire = binding("phone", "desktop");
        let mut message = RemoteSessionControlMessage::new(
            RemoteSessionControlKind::ScreenReady,
            "01234567-89ab-cdef-0123-456789abcdef",
            &wire,
            Uuid::new_v4().to_string(),
            1,
            Some(ScreenDirection::PhoneToDesktop),
            None,
            None,
            None,
            None,
        );
        message.screen_width = Some(1080);
        message.screen_height = Some(2400);
        message.screen_rotation = Some(0);
        let decoded = RemoteSessionControlMessage::from_json(&message.to_json().unwrap()).unwrap();
        assert_eq!(decoded.screen_width, Some(1080));
        assert_eq!(decoded.screen_height, Some(2400));
        assert!(decoded.validate(&binding("desktop", "phone"), "01234567-89ab-cdef-0123-456789abcdef", now_millis()).is_ok());

        message.screen_height = None;
        assert!(message
            .validate(&binding("desktop", "phone"), "01234567-89ab-cdef-0123-456789abcdef", now_millis())
            .is_err());
    }
}
