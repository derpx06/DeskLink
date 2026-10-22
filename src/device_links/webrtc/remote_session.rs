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

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

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
/// How long a request for the other side's screen may go unanswered. Long
/// enough for a person to read and approve a consent screen; short enough that
/// a request nobody answers does not read as "requesting" forever.
pub const VIEW_REQUEST_TIMEOUT: Duration = Duration::from_secs(150);
/// How long a request for control (or the host's decision on one) may wait.
pub const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Widest clock skew the peer tolerates on a control message.
const MAX_AGE: Duration = Duration::from_secs(60);

/// The longest text a capture result may carry (the phone's cap, F3).
pub const MAX_CAPTURE_TEXT_BYTES: usize = 32 * 1024;

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
    /// Desktop → phone report: a text input on the desktop gained or lost
    /// keyboard focus, so the phone can raise or drop its keyboard.
    InputFocus,
    /// Phone → desktop: show the next shared screen.
    SwitchScreen,
    /// Phone → desktop: the phone offers its own screen unasked. The desktop
    /// adopts the offered id and answers with an ordinary `request-view`
    /// (or `view-denied`, reason `busy`).
    OfferView,
    /// The phone as a webcam (contract C4), outside the screen session and
    /// with its own sequence: desk → phone `camera-request`, phone → desk
    /// `camera-ready`, either way `camera-control` and `camera-stopped`.
    CameraRequest,
    CameraReady,
    CameraControl,
    CameraStopped,
    /// Sound both ways (F1 A2), outside the screen session too. The
    /// requester is always the one who hears.
    AudioRequest,
    AudioReady,
    AudioControl,
    AudioStop,
    AudioDenied,
    /// Insert from the phone (F3): desk → phone `capture-request`, phone →
    /// desk `capture-result`, either way `capture-cancelled`. The capture id
    /// is the message's `remoteSessionId`.
    CaptureRequest,
    CaptureResult,
    CaptureCancelled,
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
            Self::InputFocus => "input-focus",
            Self::SwitchScreen => "switch-screen",
            Self::OfferView => "offer-view",
            Self::CameraRequest => "camera-request",
            Self::CameraReady => "camera-ready",
            Self::CameraControl => "camera-control",
            Self::CameraStopped => "camera-stopped",
            Self::AudioRequest => "audio-request",
            Self::AudioReady => "audio-ready",
            Self::AudioControl => "audio-control",
            Self::AudioStop => "audio-stop",
            Self::AudioDenied => "audio-denied",
            Self::CaptureRequest => "capture-request",
            Self::CaptureResult => "capture-result",
            Self::CaptureCancelled => "capture-cancelled",
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
            "input-focus" => Self::InputFocus,
            "switch-screen" => Self::SwitchScreen,
            "offer-view" => Self::OfferView,
            "camera-request" => Self::CameraRequest,
            "camera-ready" => Self::CameraReady,
            "camera-control" => Self::CameraControl,
            "camera-stopped" => Self::CameraStopped,
            "audio-request" => Self::AudioRequest,
            "audio-ready" => Self::AudioReady,
            "audio-control" => Self::AudioControl,
            "audio-stop" => Self::AudioStop,
            "audio-denied" => Self::AudioDenied,
            "capture-request" => Self::CaptureRequest,
            "capture-result" => Self::CaptureResult,
            "capture-cancelled" => Self::CaptureCancelled,
            _ => return None,
        })
    }

    /// Whether this kind must carry a `direction`.
    const fn requires_direction(self) -> bool {
        matches!(
            self,
            Self::RequestView | Self::ViewGranted | Self::ScreenReady | Self::OfferView
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
    /// `input-focus`: whether the focused element takes typing.
    pub editable: Option<bool>,
    /// `screen-ready`: how many screens are shared (the phone offers a
    /// switch when there is more than one).
    pub screen_count: Option<i32>,
    /// `request-view` / `request-control` / `takeover-request`: the session
    /// password, when the desktop has one set. See `session_lock`.
    pub password: Option<String>,
    /// `request-view` (desktop to phone): the phone's own landscape long
    /// side and the frame rate it wants (contract V1). Older phones omit
    /// both.
    pub max_dimension: Option<i64>,
    pub fps: Option<i64>,
    /// Camera (C4): "front" / "back", the picture's size, whether the
    /// microphone comes too, and a `camera-control` action.
    pub facing: Option<String>,
    pub width: Option<i32>,
    pub height: Option<i32>,
    pub audio: Option<bool>,
    pub action: Option<String>,
    /// Sound (F1): who sends ("desk-to-phone" / "phone-to-desk", on the wire
    /// as `direction` — never a screen direction), what ("system" / "mic"),
    /// its format, and an `audio-control` value (0..100).
    pub audio_direction: Option<String>,
    pub source: Option<String>,
    pub sample_rate: Option<i32>,
    pub channels: Option<i32>,
    pub value: Option<i32>,
    /// Insert from the phone (F3): `captureKind` (photo / scan / text / qr —
    /// `kind` is the message's own), `target` (clipboard / file), recognised
    /// or decoded `text`, and a photo or scan's `mimeType` and `pages`.
    pub capture_kind: Option<String>,
    pub target: Option<String>,
    pub text: Option<String>,
    pub mime_type: Option<String>,
    pub pages: Option<i32>,
    /// `request-view` / `screen-ready` (F2): "mirror" or "extend" (a second
    /// display sized `screenWidth`×`screenHeight`). Absent means mirror.
    pub display: Option<String>,
}

impl ControlKind {
    /// The webcam's kinds, handled apart from the screen session.
    pub const fn is_camera(self) -> bool {
        matches!(
            self,
            Self::CameraRequest | Self::CameraReady | Self::CameraControl | Self::CameraStopped
        )
    }

    /// Insert from the phone's kinds (F3), handled apart from the screen
    /// session.
    pub const fn is_capture(self) -> bool {
        matches!(
            self,
            Self::CaptureRequest | Self::CaptureResult | Self::CaptureCancelled
        )
    }

    /// Sound's kinds (F1), handled apart from the screen session.
    pub const fn is_audio(self) -> bool {
        matches!(
            self,
            Self::AudioRequest
                | Self::AudioReady
                | Self::AudioControl
                | Self::AudioStop
                | Self::AudioDenied
        )
    }
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
            editable: None,
            screen_count: None,
            max_dimension: None,
            fps: None,
            password: None,
            facing: None,
            width: None,
            height: None,
            audio: None,
            action: None,
            audio_direction: None,
            source: None,
            sample_rate: None,
            channels: None,
            value: None,
            capture_kind: None,
            target: None,
            text: None,
            mime_type: None,
            pages: None,
            display: None,
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
        if let Some(editable) = self.editable {
            root.insert("editable".into(), Value::from(editable));
        }
        if let Some(count) = self.screen_count {
            root.insert("screenCount".into(), Value::from(count));
        }
        if let Some(password) = &self.password {
            root.insert("password".into(), Value::from(password.clone()));
        }
        if let Some(max_dimension) = self.max_dimension {
            root.insert("maxDimension".into(), Value::from(max_dimension));
        }
        if let Some(fps) = self.fps {
            root.insert("fps".into(), Value::from(fps));
        }
        if let Some(facing) = &self.facing {
            root.insert("facing".into(), Value::from(facing.clone()));
        }
        if let (Some(width), Some(height)) = (self.width, self.height) {
            root.insert("width".into(), Value::from(width));
            root.insert("height".into(), Value::from(height));
        }
        if let Some(audio) = self.audio {
            root.insert("audio".into(), Value::from(audio));
        }
        if let Some(action) = &self.action {
            root.insert("action".into(), Value::from(action.clone()));
        }
        if let Some(direction) = &self.audio_direction {
            root.insert("direction".into(), Value::from(direction.clone()));
        }
        if let Some(source) = &self.source {
            root.insert("source".into(), Value::from(source.clone()));
        }
        if let Some(rate) = self.sample_rate {
            root.insert("sampleRate".into(), Value::from(rate));
        }
        if let Some(channels) = self.channels {
            root.insert("channels".into(), Value::from(channels));
        }
        if let Some(value) = self.value {
            root.insert("value".into(), Value::from(value));
        }
        if let Some(kind) = &self.capture_kind {
            root.insert("captureKind".into(), Value::from(kind.clone()));
        }
        if let Some(target) = &self.target {
            root.insert("target".into(), Value::from(target.clone()));
        }
        if let Some(text) = &self.text {
            root.insert("text".into(), Value::from(text.clone()));
        }
        if let Some(mime) = &self.mime_type {
            root.insert("mimeType".into(), Value::from(mime.clone()));
        }
        if let Some(pages) = self.pages {
            root.insert("pages".into(), Value::from(pages));
        }
        if let Some(display) = &self.display {
            root.insert("display".into(), Value::from(display.clone()));
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

        if number("remoteSessionVersion") != Some(VERSION) {
            return Err("Unsupported remote-session control version".to_string());
        }

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
            direction: optional_text("direction")
                .filter(|_| !kind.is_audio())
                .and_then(|value| ScreenDirection::parse(&value)),
            owner_device_id: optional_text("ownerDeviceId"),
            lease_expires_at: number("leaseExpiresAt"),
            reason: optional_text("reason"),
            screen_width: number("screenWidth").map(|value| value as i32),
            screen_height: number("screenHeight").map(|value| value as i32),
            screen_rotation: number("screenRotation").map(|value| value as i32),
            editable: object.get("editable").and_then(Value::as_bool),
            screen_count: number("screenCount").map(|value| value.clamp(0, 16) as i32),
            password: optional_text("password"),
            max_dimension: number("maxDimension"),
            fps: number("fps"),
            facing: optional_text("facing"),
            width: number("width").map(|value| value.clamp(0, 8192) as i32),
            height: number("height").map(|value| value.clamp(0, 8192) as i32),
            audio: object.get("audio").and_then(Value::as_bool),
            action: optional_text("action"),
            audio_direction: optional_text("direction").filter(|_| kind.is_audio()),
            source: optional_text("source"),
            sample_rate: number("sampleRate").map(|value| value.clamp(0, 1 << 20) as i32),
            channels: number("channels").map(|value| value.clamp(0, 64) as i32),
            value: number("value").map(|value| value.clamp(-1, 1000) as i32),
            capture_kind: optional_text("captureKind"),
            target: optional_text("target"),
            text: text("text"),
            mime_type: optional_text("mimeType"),
            pages: number("pages").map(|value| value.clamp(-1, 10_000) as i32),
            display: optional_text("display"),
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
        if self.password.as_ref().is_some_and(|password| {
            password.len() > crate::device_links::session_lock::MAX_PASSWORD_BYTES
        }) {
            return Err("Remote-session password is too long".to_string());
        }
        if let (Some(width), Some(height)) = (self.screen_width, self.screen_height) {
            if !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
                return Err("Remote-session screen size is out of range".to_string());
            }
        } else if self.screen_width.is_some() || self.screen_height.is_some() {
            return Err("Remote-session screen size needs both dimensions".to_string());
        }
        if self
            .facing
            .as_deref()
            .is_some_and(|facing| !matches!(facing, "front" | "back"))
        {
            return Err("Camera facing is neither front nor back".to_string());
        }
        if self.kind == ControlKind::CameraControl
            && !matches!(
                self.action.as_deref(),
                Some("flip" | "mic-on" | "mic-off" | "stop")
            )
        {
            return Err("Unknown camera action".to_string());
        }
        if self.kind == ControlKind::AudioControl
            && !matches!(self.action.as_deref(), Some("mute" | "unmute" | "volume"))
        {
            return Err("Unknown audio action".to_string());
        }
        if self
            .audio_direction
            .as_deref()
            .is_some_and(|direction| !matches!(direction, "desk-to-phone" | "phone-to-desk"))
        {
            return Err("Unknown audio direction".to_string());
        }
        if self
            .text
            .as_ref()
            .is_some_and(|text| text.len() > MAX_CAPTURE_TEXT_BYTES)
        {
            return Err("Captured text is too long".to_string());
        }
        if self
            .display
            .as_deref()
            .is_some_and(|display| !matches!(display, "mirror" | "extend"))
        {
            return Err("Unknown screen display mode".to_string());
        }
        if self.pages.is_some_and(|pages| !(1..=500).contains(&pages)) {
            return Err("Capture page count is out of range".to_string());
        }
        if self.kind == ControlKind::AudioRequest && self.audio_direction.is_none() {
            return Err("audio-request needs a direction".to_string());
        }
        if self
            .source
            .as_deref()
            .is_some_and(|source| !matches!(source, "system" | "mic"))
        {
            return Err("Unknown audio source".to_string());
        }
        if self
            .sample_rate
            .is_some_and(|rate| !(8_000..=192_000).contains(&rate))
            || self
                .channels
                .is_some_and(|channels| !(1..=2).contains(&channels))
            || self.value.is_some_and(|value| !(0..=100).contains(&value))
        {
            return Err("Audio format is out of range".to_string());
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

/// A host-approved lease for input arriving from the paired phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInputLease {
    pub id: String,
    pub owner_device_id: String,
    pub expires_at: i64,
    sequence_window: InputSequenceWindow,
}

/// Accepts a small amount of cross-channel reordering while rejecting replayed
/// input. Pointer motion and discrete actions intentionally use different
/// WebRTC data channels, so arrival order is not a reliable sequence order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct InputSequenceWindow {
    highest: i64,
    seen: HashSet<i64>,
}

impl InputSequenceWindow {
    const WIDTH: i64 = 128;

    fn accept(&mut self, sequence: i64) -> bool {
        if sequence <= 0 {
            return false;
        }
        if sequence > self.highest {
            self.highest = sequence;
            self.seen.insert(sequence);
            let oldest = self.highest.saturating_sub(Self::WIDTH - 1);
            self.seen.retain(|seen| *seen >= oldest);
            return true;
        }
        if self.highest - sequence >= Self::WIDTH {
            return false;
        }
        self.seen.insert(sequence)
    }
}

/// What this desktop is currently doing with the paired device's screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Idle,
    RequestingView,
    Viewing,
    RequestingControl,
    AuthorizingPeerControl,
    PeerControlling,
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
    peer_input_lease: Option<PeerInputLease>,
    sequence: AtomicI64,
    /// When `state` was entered, for [`Self::expire_pending`].
    state_since: Instant,
    /// The phone's screen in its own pixels, from its `screen-ready`: taps on
    /// the desktop's view are mapped into this space.
    phone_screen: Option<(i32, i32)>,
    /// Why the phone last refused control (e.g. its accessibility service is
    /// off), so the window can say so instead of "requesting" forever.
    control_denial: Option<String>,
    /// The last few sessions that ended: an offer for one of them is a
    /// leftover, never a new share.
    ended: std::collections::VecDeque<String>,
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
            peer_input_lease: None,
            // The peer requires `sequence > 0` and strictly increasing, so the
            // first value handed out must be 1.
            sequence: AtomicI64::new(0),
            state_since: Instant::now(),
            phone_screen: None,
            control_denial: None,
            ended: std::collections::VecDeque::new(),
        }
    }

    pub fn control_denial(&self) -> Option<&str> {
        self.control_denial.as_deref()
    }

    pub fn phone_screen(&self) -> Option<(i32, i32)> {
        self.phone_screen
    }

    pub fn set_phone_screen(&mut self, width: i32, height: i32) {
        if width > 0 && height > 0 {
            self.phone_screen = Some((width, height));
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

    pub fn peer_input_lease(&self) -> Option<&PeerInputLease> {
        self.peer_input_lease.as_ref()
    }

    /// Revokes a silent phone-owned input lease without ending the screen view.
    pub fn expire_peer_input_lease(&mut self, now: i64) -> bool {
        if self
            .peer_input_lease
            .as_ref()
            .is_some_and(|lease| lease.expires_at <= now)
        {
            self.peer_input_lease = None;
            if self.state == SessionState::PeerControlling {
                self.set_state(SessionState::Viewing);
            }
            true
        } else {
            false
        }
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
        self.peer_input_lease = None;
        self.set_state(SessionState::RequestingView);
        id
    }

    /// Takes up a phone's offer of its screen: a view request under the id
    /// the phone chose. Refused while any session is under way.
    /// `Err("stale")` for an offer of a session that already ended (a
    /// leftover arriving late): it is answered with nothing.
    pub fn adopt_offer(&mut self, id: &str) -> Result<(), String> {
        if self.ended.iter().any(|ended| ended == id) {
            return Err("stale".to_string());
        }
        if !matches!(self.state, SessionState::Idle | SessionState::Stopped) {
            return Err("busy".to_string());
        }
        self.remote_session_id = Some(id.to_string());
        self.direction = Some(ScreenDirection::PhoneToDesktop);
        self.lease = None;
        self.peer_input_lease = None;
        self.set_state(SessionState::RequestingView);
        Ok(())
    }

    pub fn begin_control(&mut self) -> Result<(), String> {
        if !matches!(
            self.state,
            SessionState::Viewing | SessionState::RequestingControl
        ) {
            return Err("Remote control needs an active view".to_string());
        }
        self.set_state(SessionState::RequestingControl);
        Ok(())
    }

    /// Applies a validated inbound message.
    ///
    /// A *control* denial keeps `remote_session_id` and the view, so control can
    /// be asked for again. A view denial or screen error ends the session on both
    /// sides, and messages that end a session are honoured only for the current
    /// one, so the two ends cannot fall out of step on a stale id.
    pub fn accept(&mut self, message: &ControlMessage, now: i64) -> Result<(), String> {
        let mismatched =
            self.remote_session_id.as_deref() != Some(message.remote_session_id.as_str());
        match message.kind {
            // The desktop reports its own focus; a phone claiming it is not a
            // report about anything this side can act on.
            ControlKind::InputFocus => {
                return Err("input-focus is reported by the desktop only".to_string());
            }
            // A request starts a session, and a heartbeat is checked against the
            // lease it quotes. An offer is answered by the coordinator.
            ControlKind::RequestView | ControlKind::Heartbeat | ControlKind::OfferView => {}
            // A message that *ends* a session is honoured only for the session
            // that is current. A delayed release, denial or error for one this
            // side already left must not stop the new one (and so its portal
            // capture). With nothing active there is nothing to disturb.
            ControlKind::Release
            | ControlKind::ScreenStopped
            | ControlKind::ViewDenied
            | ControlKind::ControlDenied
            | ControlKind::ScreenError => {
                if self.remote_session_id.is_some() && mismatched {
                    return Err("Remote-session id is stale".to_string());
                }
            }
            _ => {
                if mismatched {
                    return Err("Remote-session id is stale".to_string());
                }
            }
        }

        if matches!(
            message.kind,
            ControlKind::ControlGranted | ControlKind::TakeoverGranted
        ) && !matches!(
            self.state,
            SessionState::RequestingControl | SessionState::Controlling
        ) {
            return Err("Remote-control lease was not requested".to_string());
        }

        match message.kind {
            // Refused above; never applied. An offer is taken up with
            // `adopt_offer`, not here.
            ControlKind::InputFocus | ControlKind::OfferView => {}
            // The webcam is not the screen session: `camera` handles it.
            ControlKind::CameraRequest
            | ControlKind::CameraReady
            | ControlKind::CameraControl
            | ControlKind::CameraStopped
            | ControlKind::AudioRequest
            | ControlKind::AudioReady
            | ControlKind::AudioControl
            | ControlKind::AudioStop
            | ControlKind::AudioDenied
            | ControlKind::CaptureRequest
            | ControlKind::CaptureResult
            | ControlKind::CaptureCancelled => {
                return Err(
                    "Camera and sound controls are not part of the screen session".to_string(),
                );
            }
            // A request about the current desktop share; changes no state.
            ControlKind::SwitchScreen => {
                if mismatched || self.direction != Some(ScreenDirection::DesktopToPhone) {
                    return Err("Switching screens needs a desktop share in progress".to_string());
                }
            }
            ControlKind::RequestView => {
                self.remote_session_id = Some(message.remote_session_id.clone());
                self.direction = message.direction;
                self.lease = None;
                self.peer_input_lease = None;
                self.set_state(SessionState::RequestingView);
            }
            ControlKind::ViewGranted | ControlKind::ScreenReady => {
                self.direction = message.direction.or(self.direction);
                self.set_state(SessionState::Viewing);
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
                self.peer_input_lease = None;
                self.control_denial = None;
                self.set_state(SessionState::Controlling);
            }
            ControlKind::Heartbeat => {
                if let Some(lease) = &mut self.lease {
                    lease.expires_at = now + LEASE_LIFETIME.as_millis() as i64;
                } else if let Some(lease) = &mut self.peer_input_lease {
                    if lease.expires_at <= now {
                        self.peer_input_lease = None;
                        self.set_state(SessionState::Viewing);
                        return Err("Peer input lease has expired".to_string());
                    }
                    if message.lease_id.as_deref() != Some(lease.id.as_str())
                        || message.owner_device_id.as_deref()
                            != Some(lease.owner_device_id.as_str())
                        || !lease.sequence_window.accept(message.sequence)
                    {
                        return Err("Peer input lease heartbeat is stale or mismatched".to_string());
                    }
                    lease.expires_at = now + LEASE_LIFETIME.as_millis() as i64;
                }
            }
            ControlKind::PauseLocked => {
                self.lease = None;
                self.peer_input_lease = None;
                self.set_state(SessionState::PausedLocked);
            }
            ControlKind::Resume => self.state = SessionState::Viewing,
            // A denied view or a screen error ends the session: there is no
            // view left to show. (Mapping these to `Viewing` left the desktop's
            // dialog reading "Screen connected" after the phone said no.)
            ControlKind::ViewDenied | ControlKind::ScreenError => self.stop(),
            // Failing to grant *control* must not take a working view with it.
            ControlKind::ControlDenied => {
                self.lease = None;
                self.peer_input_lease = None;
                self.control_denial = Some(
                    message
                        .reason
                        .clone()
                        .unwrap_or_else(|| "The phone refused control".to_string()),
                );
                self.set_state(SessionState::Viewing);
            }
            ControlKind::Release | ControlKind::ScreenStopped => self.stop(),
            ControlKind::RequestControl | ControlKind::TakeoverRequest => {
                // A phone may ask to control this desktop only while viewing
                // the desktop's screen. The caller must still obtain a local
                // GNOME portal grant before it creates an input lease. Asking
                // again while already controlling (the phone retries every few
                // seconds until it sees the grant) is the same request: the
                // new grant replaces the lease. Refusing it made control flap.
                if matches!(
                    self.state,
                    SessionState::Viewing
                        | SessionState::AuthorizingPeerControl
                        | SessionState::PeerControlling
                ) && self.direction == Some(ScreenDirection::DesktopToPhone)
                {
                    self.set_state(SessionState::AuthorizingPeerControl);
                } else {
                    return Err("Phone control needs an active desktop-screen session".to_string());
                }
            }
        }
        Ok(())
    }

    fn set_state(&mut self, state: SessionState) {
        if self.state != state {
            self.state_since = Instant::now();
        }
        self.state = state;
    }

    /// Ends a request the other side never answered, returning the state it was
    /// stuck in.
    ///
    /// An unanswered *view* request ends the session. An unanswered *control*
    /// request only drops back to the view it was made on, so the user can ask
    /// again instead of losing the picture.
    pub fn expire_pending(&mut self, now: Instant) -> Option<SessionState> {
        let waited = now.saturating_duration_since(self.state_since);
        let previous = self.state;
        match previous {
            SessionState::RequestingView if waited >= VIEW_REQUEST_TIMEOUT => self.stop(),
            SessionState::RequestingControl | SessionState::AuthorizingPeerControl
                if waited >= CONTROL_REQUEST_TIMEOUT =>
            {
                self.lease = None;
                self.peer_input_lease = None;
                self.set_state(SessionState::Viewing);
            }
            _ => return None,
        }
        Some(previous)
    }

    /// [`Self::stop`], only while `id` is the current session: a share
    /// that is only now ending must not end the one that replaced it.
    pub fn stop_if_current(&mut self, id: &str) -> bool {
        let current = self.remote_session_id.as_deref() == Some(id);
        if current {
            self.stop();
        }
        current
    }

    pub fn stop(&mut self) {
        self.set_state(SessionState::Stopped);
        if let Some(id) = self.remote_session_id.take() {
            if self.ended.len() == 8 {
                self.ended.pop_front();
            }
            self.ended.push_back(id);
        }
        self.direction = None;
        self.lease = None;
        self.peer_input_lease = None;
    }

    /// Converts a pending host authorization into a revocable peer-owned lease.
    /// Call only after the platform portal has granted keyboard/pointer access.
    pub fn grant_peer_control(
        &mut self,
        lease_id: String,
        owner_device_id: String,
        expires_at: i64,
    ) -> Result<(), String> {
        if self.state != SessionState::AuthorizingPeerControl
            || self.direction != Some(ScreenDirection::DesktopToPhone)
        {
            return Err("Phone control has no host-approved desktop-screen request".to_string());
        }
        if lease_id.is_empty() || owner_device_id.is_empty() || expires_at <= 0 {
            return Err("Peer input lease is malformed".to_string());
        }
        self.lease = None;
        self.peer_input_lease = Some(PeerInputLease {
            id: lease_id,
            owner_device_id,
            expires_at,
            sequence_window: InputSequenceWindow::default(),
        });
        self.set_state(SessionState::PeerControlling);
        Ok(())
    }

    /// Validates a phone input packet before it can reach the portal adapter.
    /// The lease sequence is advanced only after all credentials pass.
    pub fn verify_peer_input(
        &mut self,
        packet: &crate::device_links::packet::NetworkPacket,
        owner_device_id: &str,
        now: i64,
    ) -> Result<(), String> {
        if self.state != SessionState::PeerControlling {
            return Err("DeskLink has no active host-approved input lease".to_string());
        }
        let session_id = self
            .remote_session_id
            .as_deref()
            .ok_or_else(|| "DeskLink remote input has no active session".to_string())?;
        let lease = self
            .peer_input_lease
            .as_mut()
            .ok_or_else(|| "DeskLink remote input has no active lease".to_string())?;
        if lease.owner_device_id != owner_device_id {
            return Err("DeskLink remote input came from a different device".to_string());
        }
        if lease.expires_at <= now {
            self.peer_input_lease = None;
            self.set_state(SessionState::Viewing);
            return Err("DeskLink remote-input lease has expired".to_string());
        }
        if packet.get_str(FIELD_REMOTE_SESSION_ID) != Some(session_id)
            || packet.get_str(FIELD_LEASE_ID) != Some(lease.id.as_str())
        {
            return Err("DeskLink remote input session or lease mismatch".to_string());
        }
        let sequence = packet
            .get_i64(FIELD_INPUT_SEQUENCE)
            .ok_or_else(|| "DeskLink remote input has no sequence".to_string())?;
        if !lease.sequence_window.accept(sequence) {
            return Err("Replayed DeskLink remote input".to_string());
        }
        lease.expires_at = now + LEASE_LIFETIME.as_millis() as i64;
        Ok(())
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
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        self.stamp_input_at(packet, now)
    }

    pub fn stamp_input_at(
        &self,
        packet: &mut crate::device_links::packet::NetworkPacket,
        now: i64,
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
        if lease.expires_at <= now {
            return Err("DeskLink remote-control lease has expired".to_string());
        }
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

    /// C4: the camera's kinds and fields cross the wire in the phone's
    /// shape, bad ones are refused, and the screen session never takes them.
    #[test]
    fn camera_messages_round_trip_and_stay_out_of_the_screen_session() {
        let id = Uuid::new_v4().to_string();
        let mut request = message(ControlKind::CameraRequest, &id);
        request.facing = Some("front".into());
        request.width = Some(1280);
        request.height = Some(720);
        request.fps = Some(30);
        request.audio = Some(false);
        let json: Value = serde_json::from_slice(&request.to_json()).unwrap();
        assert_eq!(json["kind"], "camera-request");
        assert_eq!(
            (
                json["facing"].as_str(),
                json["width"].as_i64(),
                json["height"].as_i64()
            ),
            (Some("front"), Some(1280), Some(720))
        );
        assert_eq!(
            (json["fps"].as_i64(), json["audio"].as_bool()),
            (Some(30), Some(false))
        );
        let parsed = ControlMessage::from_json(&request.to_json()).unwrap();
        assert_eq!(parsed, request);
        assert!(parsed.kind.is_camera());
        assert!(parsed
            .validate(&wire(), "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_ok());

        let mut sideways = parsed.clone();
        sideways.facing = Some("side".into());
        assert!(sideways
            .validate(&wire(), "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_err());
        let mut control = message(ControlKind::CameraControl, &id);
        control.action = Some("zoom".into());
        assert!(control
            .validate(&wire(), "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_err());
        control.action = Some("flip".into());
        assert!(control
            .validate(&wire(), "01234567-89ab-cdef-0123-456789abcdef", 1_000)
            .is_ok());

        let mut session = RemoteSession::new();
        assert!(session.accept(&parsed, 1_000).is_err());
        for kind in ["camera-ready", "camera-control", "camera-stopped"] {
            assert!(
                ControlKind::parse(kind).is_some_and(ControlKind::is_camera),
                "{kind}"
            );
        }
    }

    /// F1: audio's `direction` is its own (never a screen direction), its
    /// fields survive the wire, and what the phone would refuse is refused.
    #[test]
    fn audio_messages_round_trip_with_their_own_direction() {
        let id = Uuid::new_v4().to_string();
        let attempt = "01234567-89ab-cdef-0123-456789abcdef";
        let mut ready = message(ControlKind::AudioReady, &id);
        ready.audio_direction = Some("desk-to-phone".into());
        ready.source = Some("system".into());
        ready.sample_rate = Some(48_000);
        ready.channels = Some(2);
        let json: Value = serde_json::from_slice(&ready.to_json()).unwrap();
        assert_eq!(
            (
                json["direction"].as_str(),
                json["source"].as_str(),
                json["sampleRate"].as_i64(),
                json["channels"].as_i64()
            ),
            (Some("desk-to-phone"), Some("system"), Some(48_000), Some(2))
        );
        let parsed = ControlMessage::from_json(&ready.to_json()).unwrap();
        assert_eq!(parsed, ready);
        assert!(parsed.kind.is_audio() && parsed.direction.is_none());
        assert!(parsed.validate(&wire(), attempt, 1_000).is_ok());

        let mut request = message(ControlKind::AudioRequest, &id);
        assert!(request.validate(&wire(), attempt, 1_000).is_err());
        request.audio_direction = Some("sideways".into());
        assert!(request.validate(&wire(), attempt, 1_000).is_err());
        request.audio_direction = Some("phone-to-desk".into());
        request.source = Some("radio".into());
        assert!(request.validate(&wire(), attempt, 1_000).is_err());
        request.source = Some("mic".into());
        assert!(request.validate(&wire(), attempt, 1_000).is_ok());

        let mut control = message(ControlKind::AudioControl, &id);
        control.action = Some("volume".into());
        control.value = Some(101);
        assert!(control.validate(&wire(), attempt, 1_000).is_err());
        control.value = Some(40);
        assert!(control.validate(&wire(), attempt, 1_000).is_ok());
        for kind in ["audio-stop", "audio-denied"] {
            assert!(ControlKind::parse(kind).is_some_and(ControlKind::is_audio));
        }
    }

    /// The desktop reports text focus with `editable`; the field survives the
    /// wire, and a phone sending the report is refused, not acted on.
    #[test]
    fn input_focus_reports_round_trip_and_are_desktop_only() {
        let id = Uuid::new_v4().to_string();
        let mut report = message(ControlKind::InputFocus, &id);
        report.editable = Some(true);
        let parsed = ControlMessage::from_json(&report.to_json()).unwrap();
        assert_eq!(parsed.kind, ControlKind::InputFocus);
        assert_eq!(parsed.editable, Some(true));
        assert_eq!(
            ControlKind::parse("input-focus"),
            Some(ControlKind::InputFocus)
        );

        let mut session = RemoteSession::new();
        assert!(session.accept(&parsed, 1_000).is_err());
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
    fn unsupported_remote_session_versions_are_rejected() {
        let mut value: Value = serde_json::from_slice(
            &message(ControlKind::Release, &Uuid::new_v4().to_string()).to_json(),
        )
        .unwrap();
        value["remoteSessionVersion"] = Value::from(VERSION + 1);

        let bytes = serde_json::to_vec(&value).unwrap();
        assert!(ControlMessage::from_json(&bytes).is_err());
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
    fn a_phone_offer_is_adopted_under_its_id_unless_a_session_is_running() {
        let id = Uuid::new_v4().to_string();
        let mut offer = message(ControlKind::OfferView, &id);
        offer.direction = Some(ScreenDirection::PhoneToDesktop);
        let parsed = ControlMessage::from_json(&offer.to_json()).unwrap();
        assert_eq!(parsed.kind, ControlKind::OfferView);
        assert!(String::from_utf8(offer.to_json())
            .unwrap()
            .contains("\"offer-view\""));

        let mut session = RemoteSession::new();
        session.adopt_offer(&id).unwrap();
        assert_eq!(session.state(), SessionState::RequestingView);
        assert_eq!(session.remote_session_id.as_deref(), Some(id.as_str()));
        // The phone then grants the view it offered, under the same id.
        let mut granted = message(ControlKind::ViewGranted, &id);
        granted.direction = Some(ScreenDirection::PhoneToDesktop);
        session.accept(&granted, 1_000).unwrap();
        assert_eq!(session.state(), SessionState::Viewing);
        assert_eq!(session.adopt_offer("other").unwrap_err(), "busy");
        // Once it ends, a late copy of the same offer asks for nothing.
        session.stop();
        assert_eq!(session.adopt_offer(&id).unwrap_err(), "stale");
        assert!(session.adopt_offer("a-new-offer").is_ok());
    }

    #[test]
    fn one_counter_serves_control_messages_and_input() {
        let mut session = RemoteSession::new();
        let session_id = session.begin_view(ScreenDirection::PhoneToDesktop);
        assert_eq!(session.direction(), Some(ScreenDirection::PhoneToDesktop));
        session
            .accept(&message(ControlKind::ScreenReady, &session_id), 1_000)
            .unwrap();
        session.begin_control().unwrap();
        assert_eq!(session.state(), SessionState::RequestingControl);
        let granted = {
            let mut granted = message(ControlKind::ControlGranted, &session_id);
            granted.lease_id = Some("lease-1".to_string());
            granted.lease_expires_at = Some(90_000);
            granted
        };
        session.accept(&granted, 1_000).unwrap();

        let first = session.next_sequence();
        let mut packet =
            crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
        session.stamp_input_at(&mut packet, 1_000).unwrap();
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

    #[test]
    fn input_is_refused_after_the_control_lease_expires() {
        let mut session = RemoteSession::new();
        let id = session.begin_view(ScreenDirection::PhoneToDesktop);
        session
            .accept(&message(ControlKind::ScreenReady, &id), 1_000)
            .unwrap();
        session.begin_control().unwrap();
        let mut grant = message(ControlKind::ControlGranted, &id);
        grant.lease_id = Some("lease-1".to_string());
        grant.lease_expires_at = Some(2_000);
        session.accept(&grant, 1_000).unwrap();

        let mut packet =
            crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
        assert!(session.stamp_input_at(&mut packet, 2_000).is_err());
    }

    #[test]
    fn an_unsolicited_control_grant_cannot_activate_input() {
        let mut session = RemoteSession::new();
        let id = session.begin_view(ScreenDirection::PhoneToDesktop);
        session
            .accept(&message(ControlKind::ScreenReady, &id), 1_000)
            .unwrap();
        let mut grant = message(ControlKind::ControlGranted, &id);
        grant.lease_id = Some("lease-1".to_string());
        grant.lease_expires_at = Some(31_000);

        assert!(session.accept(&grant, 1_000).is_err());
        assert_eq!(session.state(), SessionState::Viewing);
    }

    #[test]
    fn a_phone_may_request_control_when_the_desktop_is_sharing_its_screen() {
        let mut session = RemoteSession::new();
        let session_id = Uuid::new_v4().to_string();
        let mut request_view = message(ControlKind::RequestView, &session_id);
        request_view.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request_view, 1_000).unwrap();

        let mut screen_ready = message(ControlKind::ScreenReady, &session_id);
        screen_ready.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&screen_ready, 1_001).unwrap();

        let mut request_control = message(ControlKind::RequestControl, &session_id);
        request_control.direction = Some(ScreenDirection::DesktopToPhone);
        let result = session.accept(&request_control, 1_002);

        assert!(
            result.is_ok(),
            "phone control request should enter host authorization: {result:?}"
        );
        assert_eq!(session.state(), SessionState::AuthorizingPeerControl);
    }

    /// The phone re-sends request-control every few seconds until it sees
    /// the grant; a repeat after the grant is the same request, not an error.
    #[test]
    fn a_repeated_control_request_after_the_grant_is_granted_again() {
        let mut session = RemoteSession::new();
        let session_id = Uuid::new_v4().to_string();
        let mut request_view = message(ControlKind::RequestView, &session_id);
        request_view.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request_view, 1_000).unwrap();
        let mut screen_ready = message(ControlKind::ScreenReady, &session_id);
        screen_ready.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&screen_ready, 1_001).unwrap();
        let mut request_control = message(ControlKind::RequestControl, &session_id);
        request_control.direction = Some(ScreenDirection::DesktopToPhone);

        session.accept(&request_control, 1_002).unwrap();
        session
            .grant_peer_control("lease-1".into(), "phone".into(), 60_000)
            .unwrap();
        assert_eq!(session.state(), SessionState::PeerControlling);

        session.accept(&request_control, 4_002).unwrap();
        session
            .grant_peer_control("lease-2".into(), "phone".into(), 64_000)
            .unwrap();
        assert_eq!(session.state(), SessionState::PeerControlling);
        assert_eq!(
            session
                .peer_input_lease
                .as_ref()
                .map(|lease| lease.id.as_str()),
            Some("lease-2"),
            "the new grant replaces the lease"
        );
        // Another session's id is still refused.
        let other = message(ControlKind::RequestControl, &Uuid::new_v4().to_string());
        assert!(session.accept(&other, 5_000).is_err());
    }

    #[test]
    fn a_share_ending_late_does_not_stop_the_one_that_replaced_it() {
        let mut session = RemoteSession::new();
        let old = session.begin_view(ScreenDirection::DesktopToPhone);
        let new = session.begin_view(ScreenDirection::DesktopToPhone);
        assert!(!session.stop_if_current(&old));
        assert_eq!(session.state(), SessionState::RequestingView);
        assert_eq!(session.remote_session_id(), Some(new.as_str()));
        assert!(session.stop_if_current(&new));
        assert_eq!(session.state(), SessionState::Stopped);
    }

    #[test]
    fn phone_control_is_not_authorized_during_a_phone_screen_session() {
        let mut session = RemoteSession::new();
        let session_id = Uuid::new_v4().to_string();
        let mut request_view = message(ControlKind::RequestView, &session_id);
        request_view.direction = Some(ScreenDirection::PhoneToDesktop);
        session.accept(&request_view, 1_000).unwrap();

        let mut screen_ready = message(ControlKind::ScreenReady, &session_id);
        screen_ready.direction = Some(ScreenDirection::PhoneToDesktop);
        session.accept(&screen_ready, 1_001).unwrap();

        let mut request_control = message(ControlKind::RequestControl, &session_id);
        request_control.direction = Some(ScreenDirection::PhoneToDesktop);

        assert!(session.accept(&request_control, 1_002).is_err());
        assert_eq!(session.state(), SessionState::Viewing);
    }

    #[test]
    fn phone_input_requires_an_explicit_peer_owned_lease_and_strict_sequence() {
        let mut session = RemoteSession::new();
        let id = Uuid::new_v4().to_string();
        let mut request_view = message(ControlKind::RequestView, &id);
        request_view.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request_view, 1_000).unwrap();
        let mut ready = message(ControlKind::ScreenReady, &id);
        ready.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&ready, 1_001).unwrap();
        let mut request = message(ControlKind::RequestControl, &id);
        request.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request, 1_002).unwrap();

        let mut packet =
            crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
        packet.set(FIELD_REMOTE_SESSION_ID, id.clone());
        packet.set(FIELD_LEASE_ID, "lease-from-host");
        packet.set(FIELD_INPUT_SEQUENCE, 1i64);
        assert!(session
            .verify_peer_input(&packet, "phone-device", 1_003)
            .is_err());

        session
            .grant_peer_control(
                "lease-from-host".to_string(),
                "phone-device".to_string(),
                31_003,
            )
            .unwrap();
        assert_eq!(session.state(), SessionState::PeerControlling);
        assert!(session
            .verify_peer_input(&packet, "phone-device", 1_003)
            .is_ok());
        assert!(session
            .verify_peer_input(&packet, "phone-device", 1_004)
            .is_err());
        assert!(session
            .verify_peer_input(&packet, "other-device", 1_004)
            .is_err());

        packet.set(FIELD_INPUT_SEQUENCE, 2i64);
        assert!(session
            .verify_peer_input(&packet, "phone-device", 31_003)
            .is_err());
    }

    #[test]
    fn unique_input_reordered_across_webrtc_channels_is_accepted_once() {
        let mut session = RemoteSession::new();
        let id = Uuid::new_v4().to_string();
        let mut request_view = message(ControlKind::RequestView, &id);
        request_view.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request_view, 1_000).unwrap();
        let mut ready = message(ControlKind::ScreenReady, &id);
        ready.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&ready, 1_001).unwrap();
        let mut request = message(ControlKind::RequestControl, &id);
        request.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request, 1_002).unwrap();
        session
            .grant_peer_control("lease".to_string(), "phone-device".to_string(), 31_002)
            .unwrap();

        let mut packet =
            crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
        packet.set(FIELD_REMOTE_SESSION_ID, id);
        packet.set(FIELD_LEASE_ID, "lease");
        packet.set(FIELD_INPUT_SEQUENCE, 4i64);
        assert!(session
            .verify_peer_input(&packet, "phone-device", 1_003)
            .is_ok());

        packet.set(FIELD_INPUT_SEQUENCE, 3i64);
        assert!(session
            .verify_peer_input(&packet, "phone-device", 1_004)
            .is_ok());

        packet.set(FIELD_INPUT_SEQUENCE, 4i64);
        assert!(session
            .verify_peer_input(&packet, "phone-device", 1_005)
            .is_err());
    }

    #[test]
    fn expired_peer_input_lease_cannot_be_revived_by_a_late_heartbeat() {
        let mut session = RemoteSession::new();
        let id = Uuid::new_v4().to_string();
        let mut request_view = message(ControlKind::RequestView, &id);
        request_view.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request_view, 1_000).unwrap();
        let mut ready = message(ControlKind::ScreenReady, &id);
        ready.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&ready, 1_001).unwrap();
        let mut request = message(ControlKind::RequestControl, &id);
        request.direction = Some(ScreenDirection::DesktopToPhone);
        session.accept(&request, 1_002).unwrap();
        session
            .grant_peer_control("lease".to_string(), "phone-device".to_string(), 2_000)
            .unwrap();

        let mut heartbeat = message(ControlKind::Heartbeat, &id);
        heartbeat.lease_id = Some("lease".to_string());
        heartbeat.owner_device_id = Some("phone-device".to_string());
        heartbeat.sequence = 2;
        assert!(session.accept(&heartbeat, 2_000).is_err());
        assert_eq!(session.state(), SessionState::Viewing);
        assert!(session.peer_input_lease().is_none());
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
        session
            .accept(&message(ControlKind::ScreenReady, &id), 1_000)
            .unwrap();
        session.begin_control().unwrap();
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

    /// A denied view is not a view. Mapping it to `Viewing` left the desktop's
    /// dialog reading "Screen connected · requesting control…" forever.
    #[test]
    fn a_view_denial_or_screen_error_ends_the_session() {
        for kind in [ControlKind::ViewDenied, ControlKind::ScreenError] {
            let mut session = RemoteSession::new();
            let id = session.begin_view(ScreenDirection::PhoneToDesktop);

            session.accept(&message(kind, &id), 1_000).unwrap();

            assert_eq!(session.state(), SessionState::Stopped, "{kind:?}");
            assert_eq!(session.remote_session_id(), None, "{kind:?}");
        }
    }

    /// A delayed release, denial or error for a session this side has already
    /// left must not stop the one it is in — and so must not tear down that
    /// session's portal capture either.
    #[test]
    fn a_late_terminal_message_for_another_session_changes_nothing() {
        for kind in [
            ControlKind::Release,
            ControlKind::ScreenStopped,
            ControlKind::ScreenError,
            ControlKind::ViewDenied,
            ControlKind::ControlDenied,
        ] {
            let mut session = RemoteSession::new();
            let current = session.begin_view(ScreenDirection::DesktopToPhone);

            let error = session
                .accept(&message(kind, &Uuid::new_v4().to_string()), 1_000)
                .unwrap_err();

            assert!(error.contains("stale"), "{kind:?}: {error}");
            assert_eq!(session.remote_session_id(), Some(current.as_str()));
            assert_eq!(session.state(), SessionState::RequestingView, "{kind:?}");
        }
    }

    /// With nothing active there is nothing for a stale message to disturb, so a
    /// duplicate release stays a harmless no-op.
    #[test]
    fn a_release_with_no_session_is_a_harmless_no_op() {
        let mut session = RemoteSession::new();
        session
            .accept(
                &message(ControlKind::Release, &Uuid::new_v4().to_string()),
                1_000,
            )
            .unwrap();
        assert_eq!(session.state(), SessionState::Stopped);
    }

    /// Nothing may stay in a "requesting" state forever: the other side can be
    /// silent (a phone whose consent screen nobody answers).
    #[test]
    fn an_unanswered_request_expires_and_a_live_view_never_does() {
        let mut session = RemoteSession::new();
        let start = std::time::Instant::now();
        session.begin_view(ScreenDirection::PhoneToDesktop);
        assert_eq!(
            session
                .expire_pending(start + VIEW_REQUEST_TIMEOUT - std::time::Duration::from_secs(1)),
            None,
            "still within its allowance"
        );
        assert_eq!(
            session
                .expire_pending(start + VIEW_REQUEST_TIMEOUT + std::time::Duration::from_secs(1)),
            Some(SessionState::RequestingView)
        );
        assert_eq!(session.state(), SessionState::Stopped);

        let mut viewing = RemoteSession::new();
        let id = viewing.begin_view(ScreenDirection::PhoneToDesktop);
        viewing
            .accept(&message(ControlKind::ScreenReady, &id), 1_000)
            .unwrap();
        assert_eq!(
            viewing
                .expire_pending(std::time::Instant::now() + std::time::Duration::from_secs(86_400)),
            None
        );
        assert_eq!(viewing.state(), SessionState::Viewing);
    }

    /// An unanswered control request falls back to the view, so the user can ask
    /// again — it must not take the whole view down with it.
    #[test]
    fn an_unanswered_control_request_falls_back_to_the_view() {
        let mut session = RemoteSession::new();
        let id = session.begin_view(ScreenDirection::PhoneToDesktop);
        session
            .accept(&message(ControlKind::ScreenReady, &id), 1_000)
            .unwrap();
        session.begin_control().unwrap();

        let expired = session.expire_pending(
            std::time::Instant::now() + CONTROL_REQUEST_TIMEOUT + std::time::Duration::from_secs(1),
        );

        assert_eq!(expired, Some(SessionState::RequestingControl));
        assert_eq!(session.state(), SessionState::Viewing);
        assert_eq!(session.remote_session_id(), Some(id.as_str()));
    }
}
