//! The control messages on the wire: [`ScreenDirection`], [`ControlKind`] and
//! [`ControlMessage`] with their parsing and encoding. Pure: no state.
//!
//! Sibling edges: none (the parent's constants and `cut_to_bytes`).

use super::*;

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
    /// An `audio-control` action.
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
            screen_width: number("screenWidth").and_then(|value| i32::try_from(value).ok()),
            screen_height: number("screenHeight").and_then(|value| i32::try_from(value).ok()),
            screen_rotation: number("screenRotation").and_then(|value| i32::try_from(value).ok()),
            editable: object.get("editable").and_then(Value::as_bool),
            screen_count: number("screenCount").map(|value| value.clamp(0, 16) as i32),
            password: optional_text("password"),
            max_dimension: number("maxDimension"),
            fps: number("fps"),
            action: optional_text("action"),
            audio_direction: optional_text("direction").filter(|_| kind.is_audio()),
            source: optional_text("source"),
            sample_rate: number("sampleRate").map(|value| value.clamp(0, 1 << 20) as i32),
            channels: number("channels").map(|value| value.clamp(0, 64) as i32),
            value: number("value").map(|value| value.clamp(-1, 1000) as i32),
            capture_kind: optional_text("captureKind"),
            target: optional_text("target"),
            text: text("text").map(|text| cut_to_bytes(text, MAX_CAPTURE_TEXT_BYTES)),
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
        // Forward compatibility: an audio action, audio direction, display
        // mode or audio source this build does not know is not a malformed
        // message. It passes here and its handler answers it as unsupported
        // (an `audio-denied` or a view denial) or ignores it, instead of the
        // whole message being dropped with the peer left waiting.
        if self.kind == ControlKind::AudioControl && self.action.is_none() {
            return Err("audio-control needs an action".to_string());
        }
        if self
            .text
            .as_ref()
            .is_some_and(|text| text.len() > MAX_CAPTURE_TEXT_BYTES)
        {
            return Err("Captured text is too long".to_string());
        }
        if self.pages.is_some_and(|pages| !(1..=500).contains(&pages)) {
            return Err("Capture page count is out of range".to_string());
        }
        if self.kind == ControlKind::AudioRequest && self.audio_direction.is_none() {
            return Err("audio-request needs a direction".to_string());
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
