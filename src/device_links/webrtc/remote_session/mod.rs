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
//!
//! Layout: this file holds the constants and field names, `cut_to_bytes`, and
//! the per-device session registry; `message` is the wire model (pure),
//! `session` one device's state machine. No cycles.

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Map, Value};
use uuid::Uuid;

use super::WebRtcWireBinding;

mod message;
mod session;

// The crate-facing surface (everything else stays inside).
pub(crate) use message::{ControlKind, ControlMessage, ScreenDirection};
pub(crate) use session::{RemoteSession, SessionState};

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

/// `text` cut at the last whole character within `max` bytes. A phone that
/// counted characters (UTF-16 units) sends up to 32 K of them: for CJK text
/// that is three times the bytes allowed, and refusing the whole message left
/// the phone's capture closed as sent and this side waiting.
pub(crate) fn cut_to_bytes(mut text: String, max: usize) -> String {
    if text.len() > max {
        let mut end = max;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

/// Body keys the peer requires on remote-input packets during a session.
pub const FIELD_REMOTE_SESSION_ID: &str = "desklinkRemoteSessionId";
pub const FIELD_LEASE_ID: &str = "desklinkLeaseId";
pub const FIELD_INPUT_SEQUENCE: &str = "desklinkInputSequence";

/// Remote sessions in progress, keyed by device.
///
/// Shared for the same reason as the screen streams: the producer is a data
/// channel callback and the consumer is the GTK main loop.
/// Forgets the session state kept for `device_id` (no longer paired).
pub(crate) fn forget(device_id: &str) {
    if let Ok(mut open) = sessions().lock() {
        open.remove(device_id);
    }
}

pub fn sessions() -> &'static Mutex<std::collections::HashMap<String, RemoteSession>> {
    static SESSIONS: std::sync::OnceLock<Mutex<std::collections::HashMap<String, RemoteSession>>> =
        std::sync::OnceLock::new();
    SESSIONS.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

#[cfg(test)]
mod tests;
