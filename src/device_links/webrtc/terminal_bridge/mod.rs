//! Translates one peer's terminal requests into registry operations.
//!
//! A per-device state machine: it touches no data channel, exactly like
//! [`super::transfer_manager`]. Every message for the peer — replies, snapshots,
//! live output, the news that a shell exited — goes into one ordered outbox, and
//! the coordinator's pump thread is the only thing that drains it. That gives
//! three properties at once:
//!
//! * **order** — a reply, a screen snapshot and the live output that follows
//!   cannot overtake one another, which two senders on two threads could;
//! * **backpressure** — everything, snapshots and replies included, is released
//!   against the data channel's buffered-amount budget;
//! * **testability** — no peer connection is needed to exercise any of it.
//!
//! One bridge per connected device. Attachment is per-device, so two phones can
//! watch the same session while only one of them types — see
//! [`TerminalRegistry::claim_writer`].
//!
//! Terminal access is **consent-gated**: nothing here runs unless the user has
//! turned on "Allow terminal access" for that phone. [`process_envelope`] is the
//! one entry point and it checks before touching a bridge or the registry;
//! [`revoke_device`] is the way back out.
//!
//! Layout: this file holds the limits, [`TerminalEvent`] and the audit log;
//! `bridge` is one device's state machine and `registry` holds every device's
//! bridge, the pump's entry points and the envelope gates. No cycles.

use crate::device_links::connection_log::{self, ConnectionEvent};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use portable_pty::CommandBuilder;

use super::terminal_protocol::{
    ChunkKind, TerminalAction, TerminalChunk, TerminalControl, TerminalSessionInfo,
    MAX_TERMINAL_CHUNK_BYTES, TERMINAL_CHUNK_MESSAGE_TYPE, TERMINAL_CONTROL_MESSAGE_TYPE,
    TERMINAL_ENDED, TERMINAL_NOT_ALLOWED,
};
use super::{WebRtcChannel, WebRtcEnvelope, WebRtcWireBinding};
use crate::device_links::config::terminal_access_allowed;
use crate::device_links::core::{DeviceManager, SessionBinding};
use crate::device_links::pty::registry::{idle_timeout, registry, TerminalRegistry};
use crate::device_links::pty::session::{PtySession, Resync};

mod bridge;
mod registry;
use bridge::*;
#[cfg(test)]
use registry::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use bridge::Start;
#[cfg(debug_assertions)]
pub(crate) use registry::DEBUG_ATTACHED;
pub(crate) use registry::{
    announce_finished, attached_count, attached_devices, begin, end, end_device_sessions,
    process_envelope, pump_is_idle, pump_once, revoke_device, wait_for_work, wake_generation,
};

/// Output bytes pushed to one peer per pump when the channel is not congested.
///
/// The real limit is the caller's budget, derived from the data channel's
/// buffered amount; this only bounds how much one pump can do so a flooding
/// program cannot starve the other channels sharing the peer worker.
pub(crate) const DEFAULT_PUMP_BUDGET: usize = 64 * 1024;

/// What a control message is charged against the pump budget. Replies are small
/// and vary; a flat figure keeps the accounting honest without serialising
/// them twice.
const CONTROL_COST: usize = 512;

/// Queued-but-unsent bytes one peer may hold. A snapshot of a very large screen
/// is the biggest legitimate entry; a peer that keeps asking while it is not
/// reading its channel is refused instead of growing this without bound.
const MAX_OUTBOX_BYTES: usize = 8 * 1024 * 1024;

/// How often unattended sessions are checked against the idle limit.
const REAP_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
// Controls are rare next to chunks and the queue is bounded; boxing them would
// only add an allocation per control.
#[allow(clippy::large_enum_variant)]
pub(crate) enum TerminalEvent {
    SendControl(TerminalControl),
    SendChunk(TerminalChunk),
}

impl TerminalEvent {
    fn cost(&self) -> usize {
        match self {
            Self::SendControl(_) => CONTROL_COST,
            Self::SendChunk(chunk) => chunk.data.len().max(1),
        }
    }
}

/// One line of the audit trail: who did what to which terminal. Never contents.
///
/// Identifiers are printed with `{:?}` so a device id can never forge a second
/// log line.
fn audit(what: &str, device_id: &str, terminal_id: Option<&str>) {
    debug_log!(
        "[DeskLink] Terminal {what} device={device_id:?} terminal={:?}",
        terminal_id.unwrap_or("-")
    );
    match what {
        "attach" => connection_log::record(device_id, ConnectionEvent::TerminalAttached),
        "detach" | "revoke-detach" => {
            connection_log::record(device_id, ConnectionEvent::TerminalDetached)
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests;
