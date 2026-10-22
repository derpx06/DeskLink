use serde::{Deserialize, Serialize};

use super::device_info::DeviceInfo;
use super::pairing::PairState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeviceStatus {
    Discovered,
    Connected,
    PairRequested,
    PairRequestedByPeer,
    Paired,
    Unreachable,
}

impl DeviceStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Discovered => "Discovered",
            Self::Connected => "Connected",
            Self::PairRequested => "Pairing requested",
            Self::PairRequestedByPeer => "Pair request",
            Self::Paired => "Paired",
            Self::Unreachable => "Unreachable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaStatus {
    pub player: String,
    pub player_list: Vec<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub is_playing: bool,
    pub volume: Option<i64>,
    pub position: Option<i64>,
    pub length: Option<i64>,
    pub can_seek: bool,
    pub album_art_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatteryStatus {
    pub current_charge: i64,
    pub is_charging: bool,
    pub battery_quantity: Option<i64>,
    pub threshold_event: Option<i64>,
    /// Milliseconds until full, while charging and when the phone knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub charge_time_remaining: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceNotification {
    pub id: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    pub ticker: String,
    pub is_clearable: bool,
    pub request_reply_id: Option<String>,
    pub actions: Vec<String>,
    /// Sent again on a resync (`silent: true`): shown in the list, never
    /// popped up.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub silent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeSink {
    pub name: String,
    pub description: String,
    pub volume: i64,
    pub max_volume: Option<i64>,
    pub muted: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemVolumeStatus {
    pub sinks: Vec<VolumeSink>,
}

/// A capture asked of the phone (F3), while open.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CaptureUse {
    /// photo / scan / text / qr.
    pub kind: String,
    /// The result said its file is on the way.
    pub receiving: bool,
}

/// Sound between this computer and the phone (F1).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SoundUse {
    /// This computer's "system" sound or "mic" goes to the phone (live).
    pub sending: Option<String>,
    /// The phone's "system" sound or "mic" comes here; `hearing_live` once
    /// it flows (else asked, waiting for the phone).
    pub hearing: Option<String>,
    pub hearing_live: bool,
}

/// The phone's camera in use here (C4).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CameraUse {
    /// Streaming (else asked, waiting for the phone).
    pub live: bool,
    pub facing: String,
    pub width: i32,
    pub height: i32,
}

/// One call on the phone, as its last snapshot said (T1).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PhoneCall {
    pub call_id: String,
    pub state: CallState,
    /// Sent only when the phone's "Show caller" is on (T3).
    pub contact_name: Option<String>,
    pub number: Option<String>,
    /// When it was answered or dialled (Unix milliseconds), for the timer.
    pub started_at: Option<i64>,
    pub incoming: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CallState {
    Ringing,
    Dialing,
    Talking,
    Held,
}

/// The phone's calls right now. Phone-owned and never stale: it is dropped
/// when the feature link goes, as a ringing call cannot be answered without
/// it (and would ring here forever).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CallStatus {
    /// Never empty: no call is `None` on the device.
    pub calls: Vec<PhoneCall>,
    pub muted: bool,
    pub silenced: bool,
}

impl CallStatus {
    /// The call to show and act on: one ringing here first, then the live one.
    pub fn primary(&self) -> &PhoneCall {
        let rank = |call: &PhoneCall| match call.state {
            CallState::Ringing if call.incoming => 0,
            CallState::Talking => 1,
            CallState::Dialing => 2,
            CallState::Ringing => 3,
            CallState::Held => 4,
        };
        self.calls
            .iter()
            .min_by_key(|call| rank(call))
            .expect("a call status always has a call")
    }

    /// The incoming call ringing now, if one is.
    pub fn ringing(&self) -> Option<&PhoneCall> {
        self.calls
            .iter()
            .find(|call| call.incoming && call.state == CallState::Ringing)
    }
}

impl PhoneCall {
    /// "Alice", "+1 555 0100", or "Incoming call" when the phone keeps the
    /// caller private.
    pub fn caller(&self) -> String {
        self.contact_name
            .clone()
            .or_else(|| self.number.clone())
            .unwrap_or_else(|| {
                if self.incoming {
                    "Incoming call".to_string()
                } else {
                    "Call".to_string()
                }
            })
    }
}

/// A phone's use of this computer's screen.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesktopSession {
    #[default]
    None,
    Viewing,
    Controlling,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub address: String,
    pub protocol_version: i64,
    pub incoming_capabilities: Vec<String>,
    pub outgoing_capabilities: Vec<String>,
    pub status: DeviceStatus,
    pub trusted: bool,
    /// True only while an authenticated WebRTC feature transport is installed.
    /// `status` answers "is this device reachable at all"; this answers "is the
    /// paired feature path fully negotiated", which the two used to conflate.
    pub feature_transport_ready: bool,
    /// The user's "Allow terminal access" choice for this device. Filled in from
    /// the persisted setting each time the daemon hands out views, never cached
    /// here, so what the switch shows is what is enforced.
    #[serde(default)]
    pub terminal_access: bool,
    /// How many terminals this device is watching right now.
    #[serde(default)]
    pub terminal_sessions_attached: usize,
    /// What this device is doing with this computer's screen right now.
    #[serde(default)]
    pub desktop_session: DesktopSession,
    /// How the WebRTC connection reaches the device: `"lan"`, `"direct"`
    /// (over the internet) or `"turn"`; `None` while not connected.
    #[serde(default)]
    pub connection_path: Option<String>,
    /// The selected ICE pair's round-trip time, in milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rtt_ms: Option<u32>,
    /// When the feature link was last lost (Unix seconds), while it stays
    /// lost: the phone-owned caches (battery, media, notifications) are then
    /// stale — kept, shown as "last seen", never acted on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    pub verification_key: Option<String>,
    pub last_error: Option<String>,
    pub media_status: Option<MediaStatus>,
    pub battery_status: Option<BatteryStatus>,
    pub notifications: Vec<DeviceNotification>,
    pub volume_status: Option<SystemVolumeStatus>,
    /// The phone's calls; `None` when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_status: Option<CallStatus>,
    /// The phone's camera as this computer's webcam (C4), while asked or live.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub camera: Option<CameraUse>,
    /// Insert from the phone (F3), while a capture is open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureUse>,
    /// Sound both ways (F1), while any flows or is asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sound: Option<SoundUse>,
    /// Whether a text field has focus on the phone ("Type on phone", C2);
    /// `None` until it says, and while its link is down.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_focused: Option<bool>,
}

/// The user-facing words for a `connection_path`, the one mapping every
/// surface uses (device page, history, remote-control header; the GNOME
/// Shell extension mirrors it). `None` for an unknown path.
pub fn connection_path_label(path: &str) -> Option<&'static str> {
    match path {
        "lan" => Some("Local network"),
        "direct" => Some("Over the internet"),
        "turn" => Some("Through a relay server"),
        _ => None,
    }
}

impl DeviceView {
    pub fn from_info(info: &DeviceInfo, address: String, paired: bool) -> Self {
        Self {
            id: info.id.clone(),
            name: info.name.clone(),
            device_type: info.device_type.clone(),
            address,
            protocol_version: info.protocol_version,
            incoming_capabilities: info.incoming_capabilities.clone(),
            outgoing_capabilities: info.outgoing_capabilities.clone(),
            trusted: paired,
            // Discovery proves the device exists, never that it is connected.
            // Reachability is reconciled from the session manager, which is the
            // only component that knows whether a link is actually alive.
            status: DeviceStatus::Discovered,
            feature_transport_ready: false,
            terminal_access: false,
            terminal_sessions_attached: 0,
            desktop_session: DesktopSession::None,
            connection_path: None,
            rtt_ms: None,
            last_seen: None,
            verification_key: None,
            last_error: None,
            media_status: None,
            battery_status: None,
            notifications: Vec::new(),
            volume_status: None,
            call_status: None,
            keyboard_focused: None,
            camera: None,
            sound: None,
            capture: None,
        }
    }

    pub fn apply_pair_state(&mut self, state: PairState) {
        self.trusted = state == PairState::Paired;
        self.status = match state {
            PairState::NotPaired => DeviceStatus::Connected,
            PairState::Requested => DeviceStatus::PairRequested,
            PairState::RequestedByPeer => DeviceStatus::PairRequestedByPeer,
            PairState::Paired => DeviceStatus::Paired,
        };
    }
}
