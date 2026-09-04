//! The `desklink.remote-session.v1` control plane.
//!
//! Screen sharing and remote control are negotiated here, on the authenticated
//! `desklink-control-v1` data channel, once the handover has reached
//! feature-ready. The `desklink.screen.*` packet types are **not** part of this:
//! they exist only so the Android application loads its screen plugin, and are
//! never sent.
//!
//! Two rules in this protocol are easy to get wrong and fatal if you do:
//!
//! * **One sequence counter.** The peer keeps a single `lastSequence` per lease
//!   and advances it from both control messages and remote-input packets, so
//!   both must draw from the same counter here. Two independent counters make
//!   every second message look like a replay.
//! * **The lease owner heartbeats.** A lease lives 30 seconds and is refreshed
//!   by a heartbeat or by inbound input. Whoever holds the lease sends the
//!   heartbeat — when this desktop is driving the phone, that is this desktop.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde_json::{Map, Value};
use uuid::Uuid;

use super::WebRtcWireBinding;

pub const MESSAGE_TYPE: &str = "desklink.remote-session.v1";
pub const VERSION: i64 = 1;

/// How long a control lease survives without a heartbeat or inbound input.
pub const LEASE_LIFETIME: Duration = Duration::from_secs(30);
/// How often the lease owner refreshes it. A third of the lifetime leaves room
/// for two lost heartbeats before the peer expires us.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(10);
/// Widest clock skew the peer tolerates on a control message.
const MAX_AGE: Duration = Duration::from_secs(60);

/// Body keys the peer requires on remote-input packets during a session.
pub const FIELD_REMOTE_SESSION_ID: &str = "desklinkRemoteSessionId";
pub const FIELD_LEASE_ID: &str = "desklinkLeaseId";
pub const FIELD_INPUT_SEQUENCE: &str = "desklinkInputSequence";

/// Which screen a session is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScreenDirection {
    PhoneToDesktop,
    DesktopToPhone,
}

impl ScreenDirection {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::PhoneToDesktop => "phone-to-desktop",
            Self::DesktopToPhone => "desktop-to-phone",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "phone-to-desktop" => Some(Self::PhoneToDesktop),
            "desktop-to-phone" => Some(Self::DesktopToPhone),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlKind {
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

impl ControlKind {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::RequestView => "request-view",
            Self::ViewGranted => "view-granted",
            Self::ViewDenied => "view-denied",
            Self::RequestControl => "request-control",
            Self::ControlGranted => "control-granted",
            Self::ControlDenied => "control-denied",
            Self::TakeoverRequest => "takeover-request",
            Self::TakeoverGranted => "takeover-granted",
            Self::Release => "release",
            Self::Heartbeat => "heartbeat",
            Self::ScreenReady => "screen-ready",
            Self::ScreenStopped => "screen-stopped",
            Self::ScreenError => "screen-error",
            Self::PauseLocked => "pause-locked",
            Self::Resume => "resume",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "request-view" => Self::RequestView,
            "view-granted" => Self::ViewGranted,
            "view-denied" => Self::ViewDenied,
            "request-control" => Self::RequestControl,
            "control-granted" => Self::ControlGranted,
            "control-denied" => Self::ControlDenied,
            "takeover-request" => Self::TakeoverRequest,
            "takeover-granted" => Self::TakeoverGranted,
            "release" => Self::Release,
            "heartbeat" => Self::Heartbeat,
            "screen-ready" => Self::ScreenReady,
            "screen-stopped" => Self::ScreenStopped,
            "screen-error" => Self::ScreenError,
            "pause-locked" => Self::PauseLocked,
            "resume" => Self::Resume,
            _ => return None,
        })
    }

    /// Whether this kind must carry a `direction`.
    const fn requires_direction(self) -> bool {
        matches!(
            self,
            Self::RequestView | Self::ViewGranted | Self::ScreenReady
        )
    }
}

/// One `desklink.remote-session.v1` message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlMessage {
    pub kind: ControlKind,
    pub session_attempt_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub remote_session_id: String,
    pub sequence: i64,
    pub timestamp: i64,
    pub lease_id: Option<String>,
    pub direction: Option<ScreenDirection>,
    pub owner_device_id: Option<String>,
    pub lease_expires_at: Option<i64>,
    pub reason: Option<String>,
    pub screen_width: Option<i32>,
    pub screen_height: Option<i32>,
    pub screen_rotation: Option<i32>,
}

impl ControlMessage {
    pub fn new(
        kind: ControlKind,
        attempt_id: &str,
        wire: &WebRtcWireBinding,
        remote_session_id: &str,
        sequence: i64,
        timestamp: i64,
    ) -> Self {
        Self {
            kind,
            session_attempt_id: attempt_id.to_string(),
            device_id: wire.sender_device_id.clone(),
            session_id: wire.session_id,
            connection_generation: wire.generation,
            remote_session_id: remote_session_id.to_string(),
            sequence,
            timestamp,
            lease_id: None,
            direction: None,
            owner_device_id: None,
            lease_expires_at: None,
            reason: None,
            screen_width: None,
            screen_height: None,
            screen_rotation: None,
        }
    }

    /// Serialises to the peer's exact shape.
    ///
    /// Absent optional fields are **omitted**, never emitted as `null` or `""`:
    /// the peer reads them with `optString(...).takeIf { it.isNotEmpty() }`, so
    /// an explicit empty value is indistinguishable from absent and a `null`
    /// would be read as the string `"null"`.
    pub fn to_json(&self) -> Vec<u8> {
        let mut root = Map::new();
        root.insert("remoteSessionVersion".into(), Value::from(VERSION));
        root.insert("kind".into(), Value::from(self.kind.wire_name()));
        root.insert(
            "sessionAttemptId".into(),
            Value::from(self.session_attempt_id.clone()),
        );
        root.insert("deviceId".into(), Value::from(self.device_id.clone()));
        root.insert("sessionId".into(), Value::from(self.session_id));
        root.insert(
            "connectionGeneration".into(),
            Value::from(self.connection_generation),
        );
        root.insert(
            "remoteSessionId".into(),
            Value::from(self.remote_session_id.clone()),
        );
        root.insert("sequence".into(), Value::from(self.sequence));
        root.insert("timestamp".into(), Value::from(self.timestamp));
        if let Some(lease_id) = &self.lease_id {
            root.insert("leaseId".into(), Value::from(lease_id.clone()));
        }
        if let Some(direction) = self.direction {
            root.insert("direction".into(), Value::from(direction.wire_name()));
        }
        if let Some(owner) = &self.owner_device_id {
            root.insert("ownerDeviceId".into(), Value::from(owner.clone()));
        }
        if let Some(expires) = self.lease_expires_at {
            root.insert("leaseExpiresAt".into(), Value::from(expires));
        }
        if let Some(reason) = &self.reason {
            root.insert("reason".into(), Value::from(reason.clone()));
        }
        if let (Some(width), Some(height)) = (self.screen_width, self.screen_height) {
            root.insert("screenWidth".into(), Value::from(width));
            root.insert("screenHeight".into(), Value::from(height));
            if let Some(rotation) = self.screen_rotation {
                root.insert("screenRotation".into(), Value::from(rotation));
            }
        }
        serde_json::to_vec(&Value::Object(root)).unwrap_or_default()
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        let value: Value =
            serde_json::from_slice(bytes).map_err(|error| format!("Malformed control: {error}"))?;
        let object = value
            .as_object()
            .ok_or_else(|| "Remote-session control is not an object".to_string())?;

        let text = |key: &str| object.get(key).and_then(Value::as_str).map(str::to_string);
        let number = |key: &str| object.get(key).and_then(Value::as_i64);
        let optional_text = |key: &str| text(key).filter(|value| !value.is_empty());

        let kind = text("kind")
            .and_then(|kind| ControlKind::parse(&kind))
            .ok_or_else(|| "Unknown remote-session control kind".to_string())?;

        Ok(Self {
            kind,
            session_attempt_id: text("sessionAttemptId")
                .ok_or_else(|| "Remote-session control has no attempt".to_string())?,
            device_id: text("deviceId")
                .ok_or_else(|| "Remote-session control has no device".to_string())?,
            session_id: number("sessionId").unwrap_or_default() as u64,
            connection_generation: number("connectionGeneration").unwrap_or_default() as u64,
            remote_session_id: text("remoteSessionId")
                .ok_or_else(|| "Remote-session control has no session id".to_string())?,
            sequence: number("sequence").unwrap_or_default(),
            timestamp: number("timestamp").unwrap_or_default(),
            lease_id: optional_text("leaseId"),
            direction: optional_text("direction").and_then(|value| ScreenDirection::parse(&value)),
            owner_device_id: optional_text("ownerDeviceId"),
            lease_expires_at: number("leaseExpiresAt"),
            reason: optional_text("reason"),
            screen_width: number("screenWidth").map(|value| value as i32),
            screen_height: number("screenHeight").map(|value| value as i32),
            screen_rotation: number("screenRotation").map(|value| value as i32),
        })
    }

    /// Applies the peer's own acceptance rules to an inbound message.
    ///
    /// Mirrored from the Android controller so this desktop rejects exactly what
    /// the phone would reject. A message the phone would refuse must not be
    /// acted on here either — the two ends would then disagree about the
    /// session, and the phone gives no feedback when it drops one.
    pub fn validate(
        &self,
        wire: &WebRtcWireBinding,
        attempt_id: &str,
        now: i64,
    ) -> Result<(), String> {
        if self.session_attempt_id != attempt_id
            || self.device_id != wire.peer_device_id
            || self.session_id != wire.session_id
            || self.connection_generation != wire.generation
        {
            return Err("Remote-session control binding mismatch".to_string());
        }
        if Uuid::parse_str(&self.remote_session_id).is_err() {
            return Err("Remote-session id is not a UUID".to_string());
        }
        if self.sequence <= 0 {
            return Err("Remote-session sequence must be positive".to_string());
        }
        if now.abs_diff(self.timestamp) > MAX_AGE.as_millis() as u64 {
            return Err("Expired remote-session control".to_string());
        }
        if self.kind.requires_direction() && self.direction.is_none() {
            return Err(format!(
                "{} requires a screen direction",
                self.kind.wire_name()
            ));
        }
        if self
            .reason
            .as_ref()
            .is_some_and(|reason| reason.len() > 1024)
        {
            return Err("Remote-session reason is too long".to_string());
        }
        if let (Some(width), Some(height)) = (self.screen_width, self.screen_height) {
            if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
                return Err("Remote-session screen size is out of range".to_string());
            }
        } else if self.screen_width.is_some() || self.screen_height.is_some() {
            return Err("Remote-session screen size needs both dimensions".to_string());
        }
        if let Some(rotation) = self.screen_rotation {
            if self.screen_width.is_none() {
                return Err("Remote-session rotation needs a screen size".to_string());
            }
            if !matches!(rotation, 0 | 90 | 180 | 270) {
                return Err("Remote-session rotation is not a right angle".to_string());
            }
        }
        if matches!(
            self.kind,
            ControlKind::ControlGranted | ControlKind::TakeoverGranted
        ) {
            let expires = self
                .lease_expires_at
                .ok_or_else(|| "A granted lease needs an expiry".to_string())?;
            if expires <= now {
                return Err("A granted lease is already expired".to_string());
            }
            if self.lease_id.is_none() {
                return Err("A granted lease needs an identifier".to_string());
            }
        }
        Ok(())
    }
}

/// A control lease held by this desktop over the paired phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ControlLease {
    pub id: String,
    pub expires_at: i64,
}

/// What this desktop is currently doing with the paired device's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    RequestingView,
    Viewing,
    RequestingControl,
    Controlling,
    PausedLocked,
    Stopped,
}

/// The desktop half of one remote session.
///
/// Sequence numbers come from here for both control messages and input packets,
/// because the peer tracks a single `lastSequence` and advances it from either.
#[derive(Debug)]
pub struct RemoteSession {
    state: SessionState,
    remote_session_id: Option<String>,
    direction: Option<ScreenDirection>,
    lease: Option<ControlLease>,
    sequence: AtomicI64,
}

impl Default for RemoteSession {
    fn default() -> Self {
        Self::new()
    }
}

impl RemoteSession {
    pub fn new() -> Self {
        Self {
            state: SessionState::Idle,
            remote_session_id: None,
            direction: None,
            lease: None,
            // The peer requires `sequence > 0` and strictly increasing, so the
            // first value handed out must be 1.
            sequence: AtomicI64::new(0),
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    pub fn remote_session_id(&self) -> Option<&str> {
        self.remote_session_id.as_deref()
    }

    pub fn direction(&self) -> Option<ScreenDirection> {
        self.direction
    }

    pub fn lease(&self) -> Option<&ControlLease> {
        self.lease.as_ref()
    }

    /// The next value for a control message or an input packet.
    pub fn next_sequence(&self) -> i64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Starts a view request, minting the session id both ends will quote.
    pub fn begin_view(&mut self, direction: ScreenDirection) -> String {
        let id = Uuid::new_v4().to_string();
        self.remote_session_id = Some(id.clone());
        self.direction = Some(direction);
        self.lease = None;
        self.state = SessionState::RequestingView;
        id
    }

    pub fn begin_control(&mut self) -> Result<(), String> {
        if !matches!(
            self.state,
            SessionState::Viewing | SessionState::RequestingControl
        ) {
            return Err("Remote control needs an active view".to_string());
        }
        self.state = SessionState::RequestingControl;
        Ok(())
    }

    /// Applies a validated inbound message.
    ///
    /// A denial deliberately keeps `remote_session_id`: the peer does not clear
    /// its own on denial, and dropping ours would make every later message
    /// quote an id it rejects as stale — leaving the two ends unable to talk
    /// without a full restart.
    pub fn accept(&mut self, message: &ControlMessage, now: i64) -> Result<(), String> {
        if !matches!(
            message.kind,
            ControlKind::RequestView
                | ControlKind::Release
                | ControlKind::ScreenStopped
                | ControlKind::Heartbeat
                | ControlKind::ViewDenied
                | ControlKind::ControlDenied
                | ControlKind::ScreenError
        ) && self.remote_session_id.as_deref() != Some(message.remote_session_id.as_str())
        {
            return Err("Remote-session id is stale".to_string());
        }

        match message.kind {
            ControlKind::RequestView => {
                self.remote_session_id = Some(message.remote_session_id.clone());
                self.direction = message.direction;
                self.lease = None;
                self.state = SessionState::RequestingView;
            }
            ControlKind::ViewGranted | ControlKind::ScreenReady => {
                self.direction = message.direction.or(self.direction);
                self.state = SessionState::Viewing;
            }
            ControlKind::ControlGranted | ControlKind::TakeoverGranted => {
                let id = message
                    .lease_id
                    .clone()
                    .ok_or_else(|| "A granted lease needs an identifier".to_string())?;
                let expires_at = message
                    .lease_expires_at
                    .ok_or_else(|| "A granted lease needs an expiry".to_string())?;
                self.lease = Some(ControlLease { id, expires_at });
                self.state = SessionState::Controlling;
            }
            ControlKind::Heartbeat => {
                if let Some(lease) = &mut self.lease {
                    lease.expires_at = now + LEASE_LIFETIME.as_millis() as i64;
                }
            }
            ControlKind::PauseLocked => {
                self.lease = None;
                self.state = SessionState::PausedLocked;
            }
            ControlKind::Resume => self.state = SessionState::Viewing,
            ControlKind::ViewDenied | ControlKind::ControlDenied | ControlKind::ScreenError => {
                self.lease = None;
                self.state = SessionState::Viewing;
            }
            ControlKind::Release | ControlKind::ScreenStopped => self.stop(),
            ControlKind::RequestControl | ControlKind::TakeoverRequest => {
                // The desktop is the controller, not a control target: it has
                // no input injection surface to lend the phone.
                return Err("This desktop does not grant remote control".to_string());
            }
        }
        Ok(())
    }

    pub fn stop(&mut self) {
        self.state = SessionState::Stopped;
        self.remote_session_id = None;
        self.direction = None;
        self.lease = None;
    }

    /// Whether a heartbeat is due, and the lease it should quote.
    ///
    /// Only the lease owner heartbeats. When this desktop drives the phone it
    /// holds the lease, so nothing else will keep the session alive.
    pub fn heartbeat_lease(&self, now: i64) -> Option<&ControlLease> {
        if self.state != SessionState::Controlling {
            return None;
        }
        self.lease.as_ref().filter(|lease| lease.expires_at > now)
    }

    /// Stamps an outgoing remote-input packet with its lease credentials.
    ///
    /// Without these the peer drops the packet: it checks the session id, the
    /// lease id, and a strictly increasing sequence drawn from the same counter
    /// as the heartbeats.
    pub fn stamp_input(
        &self,
        packet: &mut crate::device_links::packet::NetworkPacket,
    ) -> Result<(), String> {
        if self.state != SessionState::Controlling {
            return Err("DeskLink remote control is not active".to_string());
        }
        let session_id = self
            .remote_session_id
            .as_ref()
            .ok_or_else(|| "DeskLink remote control has no session".to_string())?;
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| "DeskLink remote control has no lease".to_string())?;
        packet.set(FIELD_REMOTE_SESSION_ID, session_id.clone());
        packet.set(FIELD_LEASE_ID, lease.id.clone());
        packet.set(FIELD_INPUT_SEQUENCE, self.next_sequence());
        Ok(())
    }
}

/// Remote sessions in progress, keyed by device.
///
/// Shared for the same reason as the screen streams: the producer is a data
/// channel callback and the consumer is the GTK main loop.
pub fn sessions() -> &'static Mutex<std::collections::HashMap<String, RemoteSession>> {
    static SESSIONS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, RemoteSession>>> =
        std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire() -> WebRtcWireBinding {
        WebRtcWireBinding::from_attempt("desktop", "phone", "01234567-89ab-cdef-0123-456789abcdef")
            .unwrap()
    }

    fn message(kind: ControlKind, session: &str) -> ControlMessage {
        let mut wire = wire();
        // Inbound messages carry the peer's device id in `deviceId`.
        std::mem::swap(&mut wire.sender_device_id, &mut wire.peer_device_id);
        ControlMessage::new(
            kind,
            "01234567-89ab-cdef-0123-456789abcdef",
            &wire,
            session,
            1,
            1_000,
        )
    }

    #[test]
    fn optional_fields_are_omitted_rather_than_emitted_empty() {
        let json = message(ControlKind::Release, &Uuid::new_v4().to_string()).to_json();
        let value: Value = serde_json::from_slice(&json).unwrap();
        let object = value.as_object().unwrap();

        // The peer reads these with `takeIf { isNotEmpty() }`, so an explicit
        // empty string or null is not equivalent to absence.
        for absent in ["leaseId", "direction", "ownerDeviceId", "reason"] {
            assert!(!object.contains_key(absent), "{absent} must be omitted");
        }
        assert_eq!(object["remoteSessionVersion"], Value::from(1));
        assert_eq!(object["kind"], Value::from("release"));
    }

    #[test]
    fn a_control_message_round_trips() {
        let mut original = message(ControlKind::ScreenReady, &Uuid::new_v4().to_string());
        original.direction = Some(ScreenDirection::PhoneToDesktop);
        original.screen_width = Some(1080);
        original.screen_height = Some(2400);
        original.screen_rotation = Some(90);

        let parsed = ControlMessage::from_json(&original.to_json()).unwrap();

        assert_eq!(parsed, original);
    }

    #[test]
    fn validation_mirrors_the_peers_rules() {
        let session = Uuid::new_v4().to_string();
        let wire = wire();

        let mut ok = message(ControlKind::ViewGranted, &session);
        ok.direction = Some(ScreenDirection::DesktopToPhone);
        assert!(ok
            .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_ok());

        // Direction is mandatory on this kind.
        let mut missing_direction = ok.clone();
        missing_direction.direction = None;
        assert!(missing_direction
            .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_err());

        // Sequence must be positive and the clock within a minute.
        let mut bad_sequence = ok.clone();
        bad_sequence.sequence = 0;
        assert!(bad_sequence
            .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_err());
        assert!(ok
            .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 200_000)
            .is_err());

        // A grant without an expiry, or already expired, is refused.
        let mut grant = message(ControlKind::ControlGranted, &session);
        grant.lease_id = Some("lease".to_string());
        assert!(grant
            .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_err());
        grant.lease_expires_at = Some(500);
        assert!(grant
            .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_err());
        grant.lease_expires_at = Some(60_000);
        assert!(grant
            .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_ok());
    }

    /// The peer keeps one `lastSequence` per lease and advances it from both
    /// control messages and input packets, so both must share this counter.
    #[test]
    fn one_counter_serves_control_messages_and_input() {
        let mut session = RemoteSession::new();
        session.begin_view(ScreenDirection::DesktopToPhone);
        let granted = {
            let mut granted = message(
                ControlKind::ControlGranted,
                session.remote_session_id().unwrap(),
            );
            granted.lease_id = Some("lease-1".to_string());
            granted.lease_expires_at = Some(90_000);
            granted
        };
        session.accept(&granted, 1_000).unwrap();

        let first = session.next_sequence();
        let mut packet =
            crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
        session.stamp_input(&mut packet).unwrap();
        let stamped = packet.get_i64(FIELD_INPUT_SEQUENCE).unwrap();
        let third = session.next_sequence();

        assert_eq!(first, 1);
        assert_eq!(stamped, 2, "input must draw from the same counter");
        assert_eq!(third, 3);
        assert_eq!(packet.get_str(FIELD_LEASE_ID), Some("lease-1"));
    }

    #[test]
    fn input_is_refused_without_an_active_lease() {
        let mut session = RemoteSession::new();
        let mut packet =
            crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
        assert!(session.stamp_input(&mut packet).is_err());

        session.begin_view(ScreenDirection::DesktopToPhone);
        assert!(session.stamp_input(&mut packet).is_err());
    }

    /// The peer does not clear its own session id when it denies, so neither
    /// may this side — otherwise every later message is rejected as stale.
    #[test]
    fn a_denial_keeps_the_session_id() {
        let mut session = RemoteSession::new();
        let id = session.begin_view(ScreenDirection::PhoneToDesktop);
        session
            .accept(&message(ControlKind::ControlDenied, &id), 1_000)
            .unwrap();

        assert_eq!(session.remote_session_id(), Some(id.as_str()));
        assert_eq!(session.state(), SessionState::Viewing);
    }

    #[test]
    fn a_release_ends_the_session() {
        let mut session = RemoteSession::new();
        let id = session.begin_view(ScreenDirection::PhoneToDesktop);
        session
            .accept(&message(ControlKind::Release, &id), 1_000)
            .unwrap();

        assert_eq!(session.state(), SessionState::Stopped);
        assert_eq!(session.remote_session_id(), None);
    }

    #[test]
    fn only_the_lease_owner_heartbeats() {
        let mut session = RemoteSession::new();
        assert!(session.heartbeat_lease(1_000).is_none());

        let id = session.begin_view(ScreenDirection::DesktopToPhone);
        let mut granted = message(ControlKind::ControlGranted, &id);
        granted.lease_id = Some("lease-1".to_string());
        granted.lease_expires_at = Some(31_000);
        session.accept(&granted, 1_000).unwrap();

        assert!(session.heartbeat_lease(1_000).is_some());
        // An expired lease is not worth heartbeating; it needs re-granting.
        assert!(session.heartbeat_lease(40_000).is_none());
    }

    #[test]
    fn a_stale_session_id_is_rejected() {
        let mut session = RemoteSession::new();
        session.begin_view(ScreenDirection::PhoneToDesktop);

        let error = session
            .accept(
                &message(ControlKind::ViewGranted, &Uuid::new_v4().to_string()),
                1_000,
            )
            .unwrap_err();

        assert!(error.contains("stale"));
    }
}
