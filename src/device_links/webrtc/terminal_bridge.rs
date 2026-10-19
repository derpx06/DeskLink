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

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Once, OnceLock};
use std::time::Duration;

use portable_pty::CommandBuilder;

use super::terminal_protocol::{
    ChunkKind, TerminalAction, TerminalChunk, TerminalControl, TerminalSessionInfo,
    MAX_TERMINAL_CHUNK_BYTES, TERMINAL_CHUNK_MESSAGE_TYPE, TERMINAL_CONTROL_MESSAGE_TYPE,
    TERMINAL_NOT_ALLOWED,
};
use super::{WebRtcChannel, WebRtcEnvelope, WebRtcWireBinding};
use crate::device_links::config::terminal_access_allowed;
use crate::device_links::core::{DeviceManager, SessionBinding};
use crate::device_links::pty::registry::{idle_timeout, registry, TerminalRegistry};
use crate::device_links::pty::session::{PtySession, Resync};

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
    eprintln!(
        "[DeskLink] Terminal {what} device={device_id:?} terminal={:?}",
        terminal_id.unwrap_or("-")
    );
}

/// A client's view of one session.
///
/// Holds the session itself rather than looking it up by id each pump: a
/// session another device's list or a `create` has reaped from the registry —
/// because its shell exited — is still here, so its exit status is still
/// reported instead of the terminal simply vanishing.
struct Attachment {
    session: Arc<PtySession>,
    /// Next output offset this peer has yet to receive.
    next: u64,
    /// The size this peer was last told the session has.
    told_size: (u16, u16),
    /// The size this peer's screen wants, from its last `Resize`.
    viewport: Option<(u16, u16)>,
}

pub(crate) struct TerminalBridge {
    /// The connection this bridge serves. Messages that arrive bound to any
    /// other connection are stale, and [`Self::token`] is how a pump thread
    /// finds out that it has been superseded.
    wire: WebRtcWireBinding,
    token: u64,
    attached: HashMap<String, Attachment>,
    /// Everything waiting to go to the peer, in order.
    outbox: VecDeque<TerminalEvent>,
    outbox_bytes: usize,
    /// One "input was refused" report per burst, not one per keystroke chunk.
    input_refusal_reported: bool,
}

impl TerminalBridge {
    pub(crate) fn new(wire: WebRtcWireBinding, token: u64) -> Self {
        Self {
            wire,
            token,
            attached: HashMap::new(),
            outbox: VecDeque::new(),
            outbox_bytes: 0,
            input_refusal_reported: false,
        }
    }

    fn device(&self) -> &str {
        &self.wire.peer_device_id
    }

    /// Queues `events` if they fit, all or none.
    fn queue(&mut self, events: Vec<TerminalEvent>) -> Result<(), String> {
        let cost: usize = events.iter().map(TerminalEvent::cost).sum();
        if self.outbox_bytes + cost > MAX_OUTBOX_BYTES {
            return Err("DeskLink terminal output is backed up; try again shortly".to_string());
        }
        self.outbox_bytes += cost;
        self.outbox.extend(events);
        Ok(())
    }

    /// Queues something that must not be lost to the size cap: it is tiny, and
    /// it tells the peer why a request will not be honoured.
    fn queue_always(&mut self, event: TerminalEvent) {
        self.outbox_bytes += event.cost();
        self.outbox.push_back(event);
    }

    fn refuse(&mut self, terminal_id: Option<String>, message: &str) {
        let event = error_event(&self.wire, terminal_id, message.to_string());
        // Best effort: if even this cannot be queued the peer is not reading.
        let _ = self.queue(vec![event]);
    }

    /// Answers a request from a phone that has not been allowed a terminal.
    ///
    /// Reaches nothing but this bridge's outbox: no session is listed, created
    /// or attached, and no registry lock is taken.
    pub(crate) fn deny(&mut self, terminal_id: Option<String>) {
        audit("deny", self.device(), terminal_id.as_deref());
        self.refuse(terminal_id, TERMINAL_NOT_ALLOWED);
    }

    /// Handles an inbound `desklink.terminal.control.v1` message.
    ///
    /// The caller must have validated `control` against this bridge's wire
    /// binding and checked the user's consent first. Replies are queued for the
    /// pump; nothing is sent from here.
    pub(crate) fn handle_control(
        &mut self,
        control: &TerminalControl,
        registry: &mut TerminalRegistry,
    ) -> Result<(), String> {
        match control.action {
            TerminalAction::List => {
                let sessions = sessions_event(&self.wire, registry);
                self.queue(vec![sessions])
            }

            TerminalAction::Create => {
                let command = control.command.as_ref().map(|argv| {
                    // An argument vector, never a shell string: nothing here is
                    // word-split, glob-expanded or passed to `sh -c`.
                    CommandBuilder::from_argv(argv.iter().map(Into::into).collect())
                });
                let rows = control.rows.unwrap_or(24);
                let cols = control.cols.unwrap_or(80);
                match registry.create(command, rows, cols, &self.wire.peer_device_id) {
                    Ok(info) => {
                        audit("create", self.device(), Some(&info.id));
                        // The same session opens at the desk, so it can be
                        // carried on there (not from tests: no windows).
                        if !cfg!(test) {
                            crate::device_links::pty::desk::open_on_desktop(&info.id);
                        }
                        let mut reply = TerminalControl::new(TerminalAction::Created, &self.wire);
                        reply.terminal_id = Some(info.id);
                        reply.rows = Some(info.rows);
                        reply.cols = Some(info.cols);
                        let sessions = sessions_event(&self.wire, registry);
                        self.queue(vec![TerminalEvent::SendControl(reply), sessions])
                    }
                    Err(error) => {
                        audit("create-refused", self.device(), None);
                        self.refuse(None, &error);
                        Ok(())
                    }
                }
            }

            TerminalAction::Attach => {
                let id = require_id(control)?;
                let Some(session) = registry.get(id) else {
                    self.refuse(Some(id.to_string()), "No such DeskLink terminal");
                    return Ok(());
                };
                let (next, chunk) = match session.resync(control.offset, usize::MAX)? {
                    Resync::Bytes { from, data } => (
                        from + data.len() as u64,
                        TerminalChunk {
                            kind: ChunkKind::Output,
                            terminal_id: id.to_string(),
                            offset: from,
                            data,
                        },
                    ),
                    Resync::Snapshot { at, data } => (
                        at,
                        TerminalChunk {
                            kind: ChunkKind::Snapshot,
                            terminal_id: id.to_string(),
                            offset: at,
                            data,
                        },
                    ),
                };

                let mut reply = TerminalControl::new(TerminalAction::Attached, &self.wire);
                reply.terminal_id = Some(id.to_string());
                reply.offset = Some(chunk.offset);
                let (rows, cols) = session.size();
                reply.rows = Some(rows);
                reply.cols = Some(cols);

                // A snapshot carries only what the screen model represents;
                // this makes full-screen programs repaint the rest themselves.
                if chunk.kind == ChunkKind::Snapshot {
                    session.nudge_redraw();
                }

                // The screen, then the reply that says where the live stream
                // resumes — in that order, released by the pump against the
                // channel's budget rather than dumped on it here.
                let mut events = split_chunk_events(chunk);
                events.push(TerminalEvent::SendControl(reply));
                if let Err(error) = self.queue(events) {
                    self.refuse(Some(id.to_string()), &error);
                    return Ok(());
                }

                // Attaching claims the write lease when it is free. A second
                // device attaching still sees everything; it just cannot type.
                registry.claim_writer(id, &self.wire.peer_device_id);
                session.touch();
                audit("attach", self.device(), Some(id));
                self.attached.insert(
                    id.to_string(),
                    Attachment {
                        session,
                        next,
                        told_size: (rows, cols),
                        viewport: None,
                    },
                );
                Ok(())
            }

            TerminalAction::Detach => {
                let id = require_id(control)?;
                if let Some(attachment) = self.attached.remove(id) {
                    attachment.session.touch();
                    audit("detach", self.device(), Some(id));
                }
                // Leaving must hand the keyboard back, or nobody else can type
                // until this device disconnects altogether.
                registry.release_terminal_writer(id, &self.wire.peer_device_id);
                Ok(())
            }

            TerminalAction::Close => {
                let id = require_id(control)?;
                self.attached.remove(id);
                registry.close(id);
                audit("close", self.device(), Some(id));
                let mut reply = TerminalControl::new(TerminalAction::Closed, &self.wire);
                reply.terminal_id = Some(id.to_string());
                let sessions = sessions_event(&self.wire, registry);
                self.queue(vec![TerminalEvent::SendControl(reply), sessions])
            }

            TerminalAction::Resize => {
                let id = require_id(control)?;
                let (Some(rows), Some(cols)) = (control.rows, control.cols) else {
                    return Err("DeskLink terminal resize needs both dimensions".to_string());
                };
                let Some(attachment) = self.attached.get_mut(id) else {
                    return Ok(());
                };
                attachment.viewport = Some((rows, cols));
                // Only the writer resizes, and never over an open desk window:
                // a viewer's phone rotating must not reflow the screen out from
                // under whoever is typing.
                if !registry.is_writer(id, &self.wire.peer_device_id)
                    || attachment.session.desk_sets_size()
                {
                    // It has already redrawn at its own size; telling it the
                    // real one again puts it back.
                    attachment.told_size = (0, 0);
                    return Ok(());
                }
                attachment.session.resize(rows, cols)
            }

            // Host-authored actions. A peer sending one is not making a request;
            // dispatching on the action rather than on which fields are present
            // is what keeps a report from being mistaken for an instruction.
            TerminalAction::Sessions
            | TerminalAction::Created
            | TerminalAction::Attached
            | TerminalAction::Closed
            | TerminalAction::Error => Err(format!(
                "DeskLink terminal action {:?} is host-authored",
                control.action
            )),
        }
    }

    /// Handles an inbound `desklink.terminal.chunk.v1` frame.
    pub(crate) fn handle_chunk(
        &mut self,
        chunk: &TerminalChunk,
        registry: &mut TerminalRegistry,
    ) -> Result<(), String> {
        if chunk.kind != ChunkKind::Input {
            return Err("DeskLink terminal host accepts only input chunks".to_string());
        }
        // Typing needs an attachment, and the keyboard. Being attached with the
        // lease free — because the previous writer detached — is enough to take
        // it; a viewer while somebody else holds it is dropped, not an error.
        let Some(attachment) = self.attached.get(&chunk.terminal_id) else {
            return Ok(());
        };
        if !registry.claim_writer(&chunk.terminal_id, &self.wire.peer_device_id) {
            return Ok(());
        }
        // The last desk window closed since this phone last resized: its
        // size again.
        if let Some(viewport) = attachment.viewport {
            if !attachment.session.desk_sets_size() && attachment.session.size() != viewport {
                let _ = attachment.session.resize(viewport.0, viewport.1);
            }
        }
        match attachment.session.write_input(&chunk.data) {
            Ok(()) => self.input_refusal_reported = false,
            Err(message) => {
                // Not a fault worth tearing anything down: the program simply is
                // not reading. Say so once per burst so the phone can show it.
                if !self.input_refusal_reported {
                    self.input_refusal_reported = true;
                    let id = chunk.terminal_id.clone();
                    self.refuse(Some(id), &message);
                }
            }
        }
        Ok(())
    }

    /// Everything that should go to the peer now, within `budget` bytes.
    ///
    /// `budget` is how much the caller is willing to queue right now — it comes
    /// from the data channel's buffered amount, which is the only real
    /// backpressure signal. Queued messages go first and strictly in order; live
    /// output is read from the ring only once they are all gone, and only as
    /// much as fits. What does not fit stays in the ring for the next pump:
    /// nothing is dropped and nothing is buffered here. One message may overshoot
    /// by less than a frame, so a small budget still makes progress.
    pub(crate) fn pump(
        &mut self,
        budget: usize,
        registry: &mut TerminalRegistry,
    ) -> Vec<TerminalEvent> {
        let mut events = Vec::new();
        let mut remaining = budget;

        while remaining > 0 {
            let Some(event) = self.outbox.pop_front() else {
                break;
            };
            let cost = event.cost();
            self.outbox_bytes = self.outbox_bytes.saturating_sub(cost);
            remaining = remaining.saturating_sub(cost);
            events.push(event);
        }
        // The loop above only stops early when the budget is spent, so anything
        // still queued means there is nothing left for live output this pump.
        if remaining == 0 {
            return events;
        }

        for id in self.attached.keys().cloned().collect::<Vec<_>>() {
            if remaining == 0 {
                break;
            }
            remaining = self.pump_terminal(&id, remaining, registry, &mut events);
        }
        events
    }

    /// Streams new output for one attached terminal; returns the budget left.
    fn pump_terminal(
        &mut self,
        id: &str,
        budget: usize,
        registry: &mut TerminalRegistry,
        events: &mut Vec<TerminalEvent>,
    ) -> usize {
        let Some(attachment) = self.attached.get(id) else {
            return budget;
        };
        let session = Arc::clone(&attachment.session);
        let mut next = attachment.next;
        let mut remaining = budget;

        // A desk window (or this phone) changed the size: the list carries
        // it, and goes ahead of the output drawn for it.
        let size = session.size();
        if size != attachment.told_size {
            if let Some(attachment) = self.attached.get_mut(id) {
                attachment.told_size = size;
            }
            let list = sessions_event(&self.wire, registry);
            remaining = remaining.saturating_sub(list.cost());
            events.push(list);
        }

        // The exit is read *before* the head. `finished` means the reader has
        // seen EOF, so a head read after it is final: the client is told the
        // shell ended only after it has been sent everything the shell wrote.
        let finished = session.finished();
        let head = session.head();

        while remaining > 0 && next < head {
            let limit = remaining.min(MAX_TERMINAL_CHUNK_BYTES);
            match session.resync(Some(next), limit) {
                Ok(Resync::Bytes { from, data }) => {
                    if data.is_empty() {
                        break;
                    }
                    next = from + data.len() as u64;
                    remaining = remaining.saturating_sub(data.len());
                    events.push(TerminalEvent::SendChunk(TerminalChunk {
                        kind: ChunkKind::Output,
                        terminal_id: id.to_string(),
                        offset: from,
                        data,
                    }));
                }
                // The peer fell so far behind that its offset aged out of the
                // ring. Sending a snapshot is the only correct answer: resuming
                // from the tail would start mid-escape-sequence. It is queued,
                // not sent here, because it may be many frames; the offset only
                // moves if it was accepted, so a full queue retries next pump.
                Ok(Resync::Snapshot { at, data }) => {
                    let snapshot = split_chunk_events(TerminalChunk {
                        kind: ChunkKind::Snapshot,
                        terminal_id: id.to_string(),
                        offset: at,
                        data,
                    });
                    if self.queue(snapshot).is_ok() {
                        next = at;
                    }
                    break;
                }
                Err(_) => break,
            }
        }

        // Caught up, and the program is gone: say so, with how it ended. A
        // session closed on purpose by another device has no status to give.
        let caught_up = next >= head;
        if caught_up && (finished.is_some() || session.is_closed()) {
            let mut closed = TerminalControl::new(TerminalAction::Closed, &self.wire);
            closed.terminal_id = Some(id.to_string());
            // A session closed on purpose was killed; the status of a killed
            // shell says nothing the phone should show.
            closed.exit_status = finished.filter(|_| !session.is_closed());
            // Through the outbox, behind anything just queued for this terminal.
            self.queue_always(TerminalEvent::SendControl(closed));
            self.attached.remove(id);
            registry.release_terminal_writer(id, &self.wire.peer_device_id);
            audit("ended", self.device(), Some(id));
            return remaining;
        }
        if let Some(attachment) = self.attached.get_mut(id) {
            attachment.next = next;
        }
        remaining
    }

    /// Takes this device's terminal access away, right now.
    ///
    /// Detaches every session, drops the write leases and forgets anything still
    /// queued for the peer, then tells it — one `closed` per terminal it was
    /// watching and one `error`, both carrying [`TERMINAL_NOT_ALLOWED`]. The
    /// sessions themselves are untouched: they belong to the desktop user and
    /// keep running.
    pub(crate) fn revoke(&mut self, registry: &mut TerminalRegistry) {
        self.outbox.clear();
        self.outbox_bytes = 0;
        for (id, attachment) in std::mem::take(&mut self.attached) {
            attachment.session.touch();
            audit("revoke-detach", self.device(), Some(&id));
            let mut closed = TerminalControl::new(TerminalAction::Closed, &self.wire);
            closed.terminal_id = Some(id);
            closed.error = Some(TERMINAL_NOT_ALLOWED.to_string());
            self.queue_always(TerminalEvent::SendControl(closed));
        }
        registry.release_writer(&self.wire.peer_device_id);
        let error = error_event(&self.wire, None, TERMINAL_NOT_ALLOWED.to_string());
        self.queue_always(error);
        audit("revoke", self.device(), None);
    }

    /// Forgets every attachment and lease, e.g. when the connection ends.
    pub(crate) fn detach_all(&mut self, registry: &mut TerminalRegistry) {
        for (id, attachment) in std::mem::take(&mut self.attached) {
            attachment.session.touch();
            audit("detach", self.device(), Some(&id));
        }
        self.outbox.clear();
        self.outbox_bytes = 0;
        registry.release_writer(&self.wire.peer_device_id);
    }

    fn attached_ids(&self) -> impl Iterator<Item = &String> {
        self.attached.keys()
    }

    #[cfg(test)]
    fn offset_for(&self, id: &str) -> Option<u64> {
        self.attached.get(id).map(|attachment| attachment.next)
    }
}

/// Splits an oversized payload into wire-legal frames.
///
/// A snapshot of a large screen exceeds one frame, and the receiving VT parser
/// is stateful across frames, so cutting anywhere is safe.
fn split_chunk_events(chunk: TerminalChunk) -> Vec<TerminalEvent> {
    if chunk.data.len() <= MAX_TERMINAL_CHUNK_BYTES {
        return vec![TerminalEvent::SendChunk(chunk)];
    }
    let mut events = Vec::new();
    let mut offset = chunk.offset;
    for piece in chunk.data.chunks(MAX_TERMINAL_CHUNK_BYTES) {
        events.push(TerminalEvent::SendChunk(TerminalChunk {
            kind: chunk.kind,
            terminal_id: chunk.terminal_id.clone(),
            offset,
            data: piece.to_vec(),
        }));
        offset += piece.len() as u64;
    }
    events
}

/// Terminal bridges in progress, keyed by peer device id.
///
/// Shared for the same reason as `screen::streams` and
/// `remote_session::sessions`: the producer is a data-channel callback on the
/// peer worker thread and the consumers are the pump thread and the GTK main
/// loop. **Lock order is bridges, then registry** — everywhere.
pub(crate) fn bridges() -> &'static Mutex<HashMap<String, TerminalBridge>> {
    static BRIDGES: OnceLock<Mutex<HashMap<String, TerminalBridge>>> = OnceLock::new();
    BRIDGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Identifies one connection's claim on its device's bridge. A reconnect takes
/// a new one, and everything the old connection's pump thread does afterwards
/// is keyed by its own, now stale, token — so it can end its own attachments
/// and never the newer connection's.
static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Claims the bridge for a connection whose terminal channel just opened.
///
/// Called on the peer worker thread *before* the pump thread is spawned, so no
/// message on this connection can arrive ahead of it. Whatever an earlier
/// connection from the same device left attached is released first: a phone
/// that reconnects has no memory of what it was watching.
pub(crate) fn begin(wire: &WebRtcWireBinding) -> u64 {
    ensure_idle_reaper();
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let (Ok(mut open), Ok(mut registry)) = (bridges().lock(), registry().lock()) else {
        return token;
    };
    if let Some(previous) = open.get_mut(&wire.peer_device_id) {
        previous.detach_all(&mut registry);
    }
    open.insert(
        wire.peer_device_id.clone(),
        TerminalBridge::new(wire.clone(), token),
    );
    token
}

/// Releases a connection's attachments and leases when its pump ends.
///
/// Only if `token` is still the bridge's: a pump thread that notices it has been
/// replaced runs this a moment *after* the new connection may have attached,
/// and keyed by device id alone it detached that newer attachment.
pub(crate) fn end(device_id: &str, token: u64) {
    let (Ok(mut open), Ok(mut registry)) = (bridges().lock(), registry().lock()) else {
        return;
    };
    if open
        .get(device_id)
        .is_some_and(|bridge| bridge.token == token)
    {
        if let Some(mut bridge) = open.remove(device_id) {
            bridge.detach_all(&mut registry);
        }
    }
}

/// One pump for the connection that holds `token`, or `None` once it has been
/// superseded and its pump thread should stop.
pub(crate) fn pump_once(device_id: &str, token: u64, budget: usize) -> Option<Vec<TerminalEvent>> {
    let mut open = bridges().lock().ok()?;
    let bridge = open
        .get_mut(device_id)
        .filter(|bridge| bridge.token == token)?;
    let mut registry = registry().lock().ok()?;
    Some(bridge.pump(budget, &mut registry))
}

/// Runs `operation` on the bridge serving exactly `wire`, with the registry.
fn with_bridge<R>(
    wire: &WebRtcWireBinding,
    operation: impl FnOnce(&mut TerminalBridge, &mut TerminalRegistry) -> Result<R, String>,
) -> Result<R, String> {
    let mut open = bridges()
        .lock()
        .map_err(|_| "DeskLink terminal bridge lock poisoned".to_string())?;
    let bridge = open
        .get_mut(&wire.peer_device_id)
        .filter(|bridge| bridge.wire == *wire)
        .ok_or_else(|| "DeskLink terminal connection was replaced".to_string())?;
    let mut registry = registry()
        .lock()
        .map_err(|_| "DeskLink terminal registry lock poisoned".to_string())?;
    operation(bridge, &mut registry)
}

/// Handles one `desklink-terminal-v1` envelope from a peer.
///
/// Checked in this order, each before the next can matter:
///
/// 1. the channel and the handover — the peer must have finished
///    authenticating, the same gate the file handler applies; this also proves
///    the binding is the *current* one (`with_session` refuses a stale binding),
///    for input chunks as much as for control messages;
/// 2. the envelope's own binding;
/// 3. **the user's consent** — "Allow terminal access" for this phone. Without
///    it the request gets `terminal-not-allowed` and nothing else happens: no
///    session is listed, created or attached, no registry lock is taken.
///
/// Control messages go through the session replay cache; **chunks deliberately
/// do not**. That cache holds 4096 entries shared by every feature, and a busy
/// terminal would wrap it in about a minute, evicting the replay protection of
/// everything else. A chunk is safe without it: the channel is ordered and
/// reliable, and the envelope is already bound to device, session and
/// generation, so there is no replay for the cache to catch.
///
/// Replies are queued on the peer's bridge; the pump sends them.
pub(crate) fn process_envelope(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    envelope: WebRtcEnvelope,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    if envelope.channel != WebRtcChannel::Terminal.label() {
        return Err("DeskLink terminal message arrived on the wrong channel".to_string());
    }
    if !sessions
        .webrtc_features_ready(binding, attempt_id)
        .map_err(|error| error.to_string())?
    {
        return Err("DeskLink terminal message arrived before feature handover".to_string());
    }
    let payload = envelope.validate(wire)?;

    match envelope.message_type.as_str() {
        TERMINAL_CONTROL_MESSAGE_TYPE => {
            sessions
                .accept_webrtc_envelope(binding, attempt_id, envelope.message_id.clone())
                .map_err(|error| error.to_string())?;
            let control = TerminalControl::from_json(&payload)?;
            control.validate(wire)?;
            if !terminal_access_allowed(&wire.peer_device_id) {
                return with_open_bridge(wire, |bridge| bridge.deny(control.terminal_id.clone()));
            }
            with_bridge(wire, |bridge, registry| {
                bridge.handle_control(&control, registry)
            })
        }
        TERMINAL_CHUNK_MESSAGE_TYPE => {
            let chunk = TerminalChunk::decode(&payload)?;
            // Dropped without a reply: a phone typing into a terminal it has no
            // right to has already been told, and an answer per keystroke chunk
            // would only feed the channel a flood.
            if !terminal_access_allowed(&wire.peer_device_id) {
                return Ok(());
            }
            with_bridge(wire, |bridge, registry| {
                bridge.handle_chunk(&chunk, registry)
            })
        }
        _ => Err("Unsupported DeskLink terminal message type".to_string()),
    }
}

/// Like [`with_bridge`] but for operations that need no registry — a denial
/// must not depend on, or wait for, the machinery it is denying access to.
fn with_open_bridge(
    wire: &WebRtcWireBinding,
    operation: impl FnOnce(&mut TerminalBridge),
) -> Result<(), String> {
    let mut open = bridges()
        .lock()
        .map_err(|_| "DeskLink terminal bridge lock poisoned".to_string())?;
    let bridge = open
        .get_mut(&wire.peer_device_id)
        .filter(|bridge| bridge.wire == *wire)
        .ok_or_else(|| "DeskLink terminal connection was replaced".to_string())?;
    operation(bridge);
    Ok(())
}

/// Takes `device_id`'s terminal access away immediately.
///
/// Call after the setting has been turned off. See [`TerminalBridge::revoke`].
pub(crate) fn revoke_device(device_id: &str) {
    let (Ok(mut open), Ok(mut registry)) = (bridges().lock(), registry().lock()) else {
        return;
    };
    match open.get_mut(device_id) {
        Some(bridge) => bridge.revoke(&mut registry),
        None => registry.release_writer(device_id),
    }
}

/// How many terminals `device_id` is watching right now.
pub(crate) fn attached_count(device_id: &str) -> usize {
    bridges()
        .lock()
        .map(|open| {
            open.get(device_id)
                .map_or(0, |bridge| bridge.attached.len())
        })
        .unwrap_or(0)
}

/// Starts the background sweep that closes forgotten sessions, once.
fn ensure_idle_reaper() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(REAP_INTERVAL);
            sweep(idle_timeout());
        });
    });
}

/// One pass of the idle reaper: closes sessions nobody is attached to that have
/// been quiet for `idle_for`, and drops ones whose program has exited.
///
/// Holds the bridges lock throughout, so nobody can attach to a session between
/// the attached set being read and that session being closed.
fn sweep(idle_for: Duration) -> Vec<String> {
    let Ok(open) = bridges().lock() else {
        return Vec::new();
    };
    let Ok(mut registry) = registry().lock() else {
        return Vec::new();
    };
    sweep_in(&open, &mut registry, idle_for)
}

fn sweep_in(
    open: &HashMap<String, TerminalBridge>,
    registry: &mut TerminalRegistry,
    idle_for: Duration,
) -> Vec<String> {
    let attached: HashSet<String> = open
        .values()
        .flat_map(|bridge| bridge.attached_ids().cloned())
        .collect();
    registry.reap();
    let closed = registry.reap_idle(&attached, idle_for);
    for id in &closed {
        audit("idle-close", "reaper", Some(id));
    }
    closed
}

fn require_id(control: &TerminalControl) -> Result<&str, String> {
    control
        .terminal_id
        .as_deref()
        .ok_or_else(|| "DeskLink terminal request needs a terminal id".to_string())
}

/// The session list.
///
/// Lists first and reaps after: reaping first hid the exit status of every
/// finished session from the one reply that could have shown it.
fn sessions_event(wire: &WebRtcWireBinding, registry: &mut TerminalRegistry) -> TerminalEvent {
    let listed = registry.list();
    registry.reap();
    let mut message = TerminalControl::new(TerminalAction::Sessions, wire);
    message.sessions = Some(
        listed
            .into_iter()
            .map(|info| TerminalSessionInfo {
                terminal_id: info.id,
                title: info.title,
                rows: info.rows,
                cols: info.cols,
                exit_status: info.exit_status,
            })
            .collect(),
    );
    TerminalEvent::SendControl(message)
}

fn error_event(
    wire: &WebRtcWireBinding,
    terminal_id: Option<String>,
    error: String,
) -> TerminalEvent {
    let mut message = TerminalControl::new(TerminalAction::Error, wire);
    message.terminal_id = terminal_id;
    message.error = Some(error);
    TerminalEvent::SendControl(message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::config::Config;
    use crate::device_links::core::SessionLink;
    use crate::device_links::device_info::DeviceInfo;
    use crate::device_links::webrtc::HandoverRuntime;
    use std::time::{Duration, Instant};
    use uuid::Uuid;

    fn wire(peer: &str) -> WebRtcWireBinding {
        WebRtcWireBinding {
            sender_device_id: "desktop-1".to_string(),
            peer_device_id: peer.to_string(),
            session_id: 7,
            generation: 3,
        }
    }

    fn new_bridge(peer: &str) -> TerminalBridge {
        TerminalBridge::new(wire(peer), NEXT_TOKEN.fetch_add(1, Ordering::Relaxed))
    }

    fn request(action: TerminalAction, wire: &WebRtcWireBinding) -> TerminalControl {
        let mut control = TerminalControl::new(action, wire);
        control.device_id = wire.peer_device_id.clone();
        control
    }

    /// Waits for a line of input, prints `text`, then stays alive.
    ///
    /// Two things here are load-bearing:
    ///
    /// * The sleep: a program that exits immediately can be reaped by the
    ///   `Sessions` reply that `Create` itself emits, so the following `Attach`
    ///   would race against the reaper and intermittently find nothing.
    /// * The `read`: printing straight away races against `Attach`. If the text
    ///   lands first it arrives in the attach *snapshot* and the pump correctly
    ///   has nothing new to stream, so a test waiting on the pump fails under
    ///   load. Blocking until [`poke`] makes the output arrive strictly after
    ///   the attach, which is the ordering these tests are about.
    fn printing_command(text: &str) -> Vec<String> {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            format!("read line; printf '{text}'; sleep 30"),
        ]
    }

    fn sleeping_command() -> Vec<String> {
        vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()]
    }

    fn input(id: &str, bytes: &[u8]) -> TerminalChunk {
        TerminalChunk {
            kind: ChunkKind::Input,
            terminal_id: id.to_string(),
            offset: 0,
            data: bytes.to_vec(),
        }
    }

    /// Sends the newline a [`printing_command`] session is waiting for.
    fn poke(bridge: &mut TerminalBridge, registry: &mut TerminalRegistry, id: &str) {
        bridge.handle_chunk(&input(id, b"\n"), registry).unwrap();
    }

    fn create(
        bridge: &mut TerminalBridge,
        registry: &mut TerminalRegistry,
        command: Option<Vec<String>>,
    ) -> String {
        let mut control = request(TerminalAction::Create, &bridge.wire);
        control.command = command;
        control.rows = Some(24);
        control.cols = Some(80);
        bridge.handle_control(&control, registry).unwrap();
        match &bridge.pump(DEFAULT_PUMP_BUDGET, registry)[0] {
            TerminalEvent::SendControl(reply) => {
                assert_eq!(reply.action, TerminalAction::Created);
                reply.terminal_id.clone().unwrap()
            }
            other => panic!("expected Created, got {other:?}"),
        }
    }

    fn attach(
        bridge: &mut TerminalBridge,
        registry: &mut TerminalRegistry,
        id: &str,
    ) -> Vec<TerminalEvent> {
        let mut control = request(TerminalAction::Attach, &bridge.wire);
        control.terminal_id = Some(id.to_string());
        bridge.handle_control(&control, registry).unwrap();
        bridge.pump(usize::MAX / 2, registry)
    }

    fn control_request(
        bridge: &mut TerminalBridge,
        registry: &mut TerminalRegistry,
        action: TerminalAction,
        id: &str,
    ) {
        let mut control = request(action, &bridge.wire);
        control.terminal_id = Some(id.to_string());
        bridge.handle_control(&control, registry).unwrap();
    }

    fn output_of(events: &[TerminalEvent]) -> Vec<u8> {
        events
            .iter()
            .filter_map(|event| match event {
                TerminalEvent::SendChunk(chunk) => Some(chunk.data.clone()),
                TerminalEvent::SendControl(_) => None,
            })
            .flatten()
            .collect()
    }

    fn pump_until_output(bridge: &mut TerminalBridge, registry: &mut TerminalRegistry) -> Vec<u8> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            seen.extend(output_of(&bridge.pump(DEFAULT_PUMP_BUDGET, registry)));
            if !seen.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        seen
    }

    /// Pumps until `wanted` matches an event, returning everything seen.
    fn pump_until(
        bridge: &mut TerminalBridge,
        registry: &mut TerminalRegistry,
        wanted: impl Fn(&TerminalEvent) -> bool,
    ) -> Vec<TerminalEvent> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut seen = Vec::new();
        while Instant::now() < deadline {
            seen.extend(bridge.pump(DEFAULT_PUMP_BUDGET, registry));
            if seen.iter().any(&wanted) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        seen
    }

    fn is_control(action: TerminalAction) -> impl Fn(&TerminalEvent) -> bool {
        move |event| matches!(event, TerminalEvent::SendControl(c) if c.action == action)
    }

    #[test]
    fn listing_reports_created_sessions() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(&mut bridge, &mut registry, Some(sleeping_command()));

        bridge
            .handle_control(&request(TerminalAction::List, &bridge.wire), &mut registry)
            .unwrap();
        match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
            TerminalEvent::SendControl(reply) => {
                let sessions = reply.sessions.as_ref().unwrap();
                assert_eq!(sessions.len(), 1);
                assert_eq!(sessions[0].terminal_id, id);
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
        registry.close(&id);
    }

    #[test]
    fn attaching_returns_a_screen_and_then_streams_output() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(
            &mut bridge,
            &mut registry,
            Some(printing_command("bridge-hello")),
        );

        // The screen first, then the reply naming where the stream resumes.
        let events = attach(&mut bridge, &mut registry, &id);
        assert!(matches!(&events[0], TerminalEvent::SendChunk(c) if c.kind == ChunkKind::Snapshot));
        assert!(matches!(
            events.last(),
            Some(TerminalEvent::SendControl(c)) if c.action == TerminalAction::Attached
        ));

        poke(&mut bridge, &mut registry, &id);
        let seen = pump_until_output(&mut bridge, &mut registry);
        assert!(
            String::from_utf8_lossy(&seen).contains("bridge-hello"),
            "output never streamed: {seen:?}"
        );
        registry.close(&id);
    }

    /// An open desk window owns the size: the phone's resizes and typing never
    /// squeeze it, the phone is told the desk's size to scale, and the phone
    /// gets its own size back once the desk window closes.
    #[test]
    fn an_open_desk_window_owns_the_size_and_the_phone_is_told() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(&mut bridge, &mut registry, Some(sleeping_command()));
        attach(&mut bridge, &mut registry, &id);
        let phone_resize = |bridge: &mut TerminalBridge, registry: &mut TerminalRegistry| {
            let mut resize = request(TerminalAction::Resize, &bridge.wire);
            resize.terminal_id = Some(id.clone());
            resize.rows = Some(30);
            resize.cols = Some(50);
            bridge.handle_control(&resize, registry).unwrap();
        };
        phone_resize(&mut bridge, &mut registry);

        let told = |events: Vec<TerminalEvent>| {
            events.into_iter().find_map(|event| match event {
                TerminalEvent::SendControl(c) if c.action == TerminalAction::Sessions => c
                    .sessions
                    .and_then(|list| list.into_iter().find(|s| s.terminal_id == id))
                    .map(|s| (s.rows, s.cols)),
                _ => None,
            })
        };
        assert_eq!(
            told(bridge.pump(usize::MAX / 2, &mut registry)),
            Some((30, 50))
        );

        // A desk window opens at its own size.
        let session = registry.get(&id).unwrap();
        session.desk_window_opened();
        session.resize(40, 120).unwrap();
        assert_eq!(
            told(bridge.pump(usize::MAX / 2, &mut registry)),
            Some((40, 120))
        );
        assert_eq!(
            told(bridge.pump(usize::MAX / 2, &mut registry)),
            None,
            "told once"
        );

        // The phone's keyboard opens, and it types: the desk keeps its size,
        // and the phone is told it again so it does not draw at its own.
        phone_resize(&mut bridge, &mut registry);
        bridge
            .handle_chunk(&input(&id, b"x"), &mut registry)
            .unwrap();
        assert_eq!(session.size(), (40, 120));
        assert_eq!(
            told(bridge.pump(usize::MAX / 2, &mut registry)),
            Some((40, 120))
        );

        // The desk window closes; the phone's next keystroke takes its size.
        session.desk_window_closed();
        bridge
            .handle_chunk(&input(&id, b"x"), &mut registry)
            .unwrap();
        assert_eq!(session.size(), (30, 50));
        assert_eq!(
            told(bridge.pump(usize::MAX / 2, &mut registry)),
            Some((30, 50))
        );
        registry.close(&id);
    }

    /// The pump must not re-send bytes it already sent, or the client renders
    /// every chunk twice.
    #[test]
    fn the_pump_advances_the_offset_and_does_not_repeat() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(&mut bridge, &mut registry, Some(printing_command("abc")));
        attach(&mut bridge, &mut registry, &id);

        poke(&mut bridge, &mut registry, &id);
        // The program's own output, not just the echoed newline that may come
        // first, so the stream is really at rest before it is checked.
        let mut seen = Vec::new();
        for event in pump_until(
            &mut bridge,
            &mut registry,
            |event| matches!(event, TerminalEvent::SendChunk(c) if c.data.ends_with(b"abc")),
        ) {
            seen.push(event);
        }
        assert!(!seen.is_empty(), "the program's output never arrived");
        let settled = bridge.offset_for(&id).unwrap();
        assert!(bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry).is_empty());
        assert_eq!(bridge.offset_for(&id), Some(settled));
        registry.close(&id);
    }

    /// Backpressure: when the caller's budget is small the pump sends less and
    /// leaves the rest in the ring, rather than queueing it somewhere unbounded.
    #[test]
    fn the_pump_respects_its_budget() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(
            &mut bridge,
            &mut registry,
            Some(printing_command("0123456789")),
        );
        attach(&mut bridge, &mut registry, &id);

        poke(&mut bridge, &mut registry, &id);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut first = Vec::new();
        while Instant::now() < deadline && first.is_empty() {
            first = output_of(&bridge.pump(4, &mut registry));
            if first.is_empty() {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        assert!(!first.is_empty());
        assert!(first.len() <= 4, "budget ignored: sent {}", first.len());

        // What was held back is still there, and arrives in order — four bytes
        // at a time, however long that takes the program to produce it.
        let mut all = first;
        while Instant::now() < deadline && !String::from_utf8_lossy(&all).contains("0123456789") {
            let more = output_of(&bridge.pump(4, &mut registry));
            assert!(more.len() <= 4, "budget ignored: sent {}", more.len());
            all.extend(more);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            String::from_utf8_lossy(&all).contains("0123456789"),
            "held-back output was lost: {all:?}"
        );
        registry.close(&id);
    }

    /// A 200x200 screen is several frames of snapshot. Attaching used to send
    /// every one of them at once, ignoring the channel's buffered amount.
    #[test]
    fn a_snapshot_is_released_against_the_budget_not_all_at_once() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let mut control = request(TerminalAction::Create, &bridge.wire);
        control.command = Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "i=0; while [ $i -lt 200 ]; do printf '%0200d\\n' $i; i=$((i+1)); done; sleep 30"
                .into(),
        ]);
        control.rows = Some(200);
        control.cols = Some(200);
        bridge.handle_control(&control, &mut registry).unwrap();
        let id = match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
            TerminalEvent::SendControl(reply) => reply.terminal_id.clone().unwrap(),
            other => panic!("expected Created, got {other:?}"),
        };
        let session = registry.get(&id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.head() < 40_000 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        let mut attach_request = request(TerminalAction::Attach, &bridge.wire);
        attach_request.terminal_id = Some(id.clone());
        bridge
            .handle_control(&attach_request, &mut registry)
            .unwrap();
        assert_eq!(bridge.attached.len(), 1);

        // A one-byte budget releases one frame per pump, never the lot.
        let mut frames = 0;
        let mut finished = false;
        for _ in 0..200 {
            let events = bridge.pump(1, &mut registry);
            assert!(
                events.len() <= 1,
                "budget of 1 released {} events",
                events.len()
            );
            frames += events
                .iter()
                .filter(|event| matches!(event, TerminalEvent::SendChunk(_)))
                .count();
            if events.iter().any(is_control(TerminalAction::Attached)) {
                finished = true;
                break;
            }
        }
        assert!(finished, "the attach reply never arrived");
        assert!(frames >= 2, "expected a multi-frame snapshot, got {frames}");
        registry.close(&id);
    }

    /// Single writer, many viewers: the second device may watch but its
    /// keystrokes must never reach the PTY.
    #[test]
    fn only_the_first_attached_device_may_type() {
        let mut registry = TerminalRegistry::new();
        let mut first = new_bridge("phone-1");
        let mut second = new_bridge("phone-2");

        let id = create(
            &mut first,
            &mut registry,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "read line; printf 'got:%s' \"$line\"".into(),
            ]),
        );
        attach(&mut first, &mut registry, &id);
        attach(&mut second, &mut registry, &id);

        assert!(registry.is_writer(&id, "phone-1"));
        assert!(!registry.is_writer(&id, "phone-2"));

        second
            .handle_chunk(&input(&id, b"nope\n"), &mut registry)
            .unwrap();
        first
            .handle_chunk(&input(&id, b"yes\n"), &mut registry)
            .unwrap();

        let seen = pump_until_output(&mut first, &mut registry);
        let text = String::from_utf8_lossy(&seen);
        assert!(
            text.contains("got:yes"),
            "writer's input did not land: {text}"
        );
        assert!(
            !text.contains("nope"),
            "viewer's input reached the PTY: {text}"
        );
        registry.close(&id);
    }

    /// `Detach` used to leave the lease behind, so another device could not type
    /// until the first one disconnected altogether.
    #[test]
    fn detaching_hands_the_keyboard_to_the_next_device() {
        let mut registry = TerminalRegistry::new();
        let mut first = new_bridge("phone-1");
        let mut second = new_bridge("phone-2");
        let id = create(
            &mut first,
            &mut registry,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "read line; printf 'got:%s' \"$line\"".into(),
            ]),
        );
        attach(&mut first, &mut registry, &id);
        attach(&mut second, &mut registry, &id);
        assert!(registry.is_writer(&id, "phone-1"));

        control_request(&mut first, &mut registry, TerminalAction::Detach, &id);
        assert!(
            !registry.is_writer(&id, "phone-1"),
            "the lease outlived Detach"
        );
        assert_eq!(first.attached.len(), 0);

        // The second phone, still attached, can now type.
        second
            .handle_chunk(&input(&id, b"mine\n"), &mut registry)
            .unwrap();
        assert!(registry.is_writer(&id, "phone-2"));
        let seen = pump_until_output(&mut second, &mut registry);
        assert!(
            String::from_utf8_lossy(&seen).contains("got:mine"),
            "the second phone's input did not land: {seen:?}"
        );
        registry.close(&id);
    }

    /// Once detached, output the program produces afterwards is not sent.
    #[test]
    fn after_a_detach_the_pump_streams_nothing_more() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(&mut bridge, &mut registry, Some(printing_command("later")));
        attach(&mut bridge, &mut registry, &id);
        control_request(&mut bridge, &mut registry, TerminalAction::Detach, &id);

        // Detaching twice, or something never attached, is not an error.
        control_request(&mut bridge, &mut registry, TerminalAction::Detach, &id);

        let session = registry.get(&id).unwrap();
        session.write_input(b"\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.head() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(session.head() > 0, "the program never produced output");
        assert!(bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry).is_empty());
        registry.close(&id);
    }

    /// Pasting into a program that is not reading must neither block the caller
    /// nor bury the phone in errors: the overflow is dropped, and reported once.
    #[test]
    fn refused_input_is_reported_once_and_never_blocks() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(
            &mut bridge,
            &mut registry,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "stty raw -echo; echo ready; sleep 30".into(),
            ]),
        );
        attach(&mut bridge, &mut registry, &id);
        // `ready` is echoed after `stty raw -echo` has run, so once it is there
        // the terminal really is in the mode that pushes back on writers.
        let session = registry.get(&id).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.head() == 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(session.head() > 0, "the shell never became ready");

        let started = Instant::now();
        let chunk = input(&id, &vec![b'x'; MAX_TERMINAL_CHUNK_BYTES]);
        for _ in 0..300 {
            bridge.handle_chunk(&chunk, &mut registry).unwrap();
        }
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "handle_chunk blocked for {:?}",
            started.elapsed()
        );
        let refusals = bridge
            .pump(usize::MAX / 2, &mut registry)
            .into_iter()
            .filter(|event| matches!(event, TerminalEvent::SendControl(c) if c.action == TerminalAction::Error))
            .count();
        // Not exactly one: a write that succeeds re-arms the report, and a busy
        // machine can let the writer thread free a queue slot mid-burst. What
        // matters is that the phone is told, and not once per dropped chunk
        // (about 235 here).
        assert!(refusals >= 1, "the phone was never told input was dropped");
        assert!(refusals < 20, "{refusals} refusals for one burst");
        registry.close(&id);
    }

    /// A shell that exits by itself must reach the phone promptly, with its
    /// status, and after the last thing it printed.
    #[test]
    fn an_exiting_shell_reports_its_status_after_its_last_output() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(
            &mut bridge,
            &mut registry,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "read line; printf 'goodbye'; exit 3".into(),
            ]),
        );
        attach(&mut bridge, &mut registry, &id);
        poke(&mut bridge, &mut registry, &id);

        let events = pump_until(
            &mut bridge,
            &mut registry,
            is_control(TerminalAction::Closed),
        );
        let closed_at = events
            .iter()
            .position(is_control(TerminalAction::Closed))
            .expect("the phone was never told the shell exited");
        match &events[closed_at] {
            TerminalEvent::SendControl(closed) => {
                assert_eq!(closed.terminal_id.as_deref(), Some(id.as_str()));
                assert_eq!(closed.exit_status, Some(3));
            }
            other => panic!("{other:?}"),
        }
        let before = output_of(&events[..closed_at]);
        assert!(
            String::from_utf8_lossy(&before).contains("goodbye"),
            "closed arrived before the shell's last output: {before:?}"
        );
        assert_eq!(bridge.attached.len(), 0, "an ended terminal stays attached");
        assert!(!registry.is_writer(&id, "phone-1"));
    }

    /// The exit is still reported when another device's `list` or a `create`
    /// reaped the session from the registry first.
    #[test]
    fn the_exit_is_reported_even_if_the_registry_reaped_the_session() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(
            &mut bridge,
            &mut registry,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "read line; exit 4".into(),
            ]),
        );
        attach(&mut bridge, &mut registry, &id);
        poke(&mut bridge, &mut registry, &id);

        let deadline = Instant::now() + Duration::from_secs(10);
        while registry.reap().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(registry.get(&id).is_none(), "the session should be reaped");

        let events = pump_until(
            &mut bridge,
            &mut registry,
            is_control(TerminalAction::Closed),
        );
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::SendControl(c)
                if c.action == TerminalAction::Closed && c.exit_status == Some(4)
        )));
    }

    /// The session list used to reap before listing, so `exit_status` was never
    /// anything but `None`.
    #[test]
    fn a_finished_session_is_listed_once_with_its_status() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(
            &mut bridge,
            &mut registry,
            Some(vec![
                "/bin/sh".into(),
                "-c".into(),
                "read line; exit 5".into(),
            ]),
        );
        // Let it exit only now, so nothing has reaped it before this test looks.
        let session = registry.get(&id).unwrap();
        session.write_input(b"\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while session.exit_status().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }

        bridge
            .handle_control(&request(TerminalAction::List, &bridge.wire), &mut registry)
            .unwrap();
        let listed = match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
            TerminalEvent::SendControl(reply) => reply.sessions.clone().unwrap(),
            other => panic!("expected Sessions, got {other:?}"),
        };
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].exit_status, Some(5));

        // Having been reported, it is gone.
        bridge
            .handle_control(&request(TerminalAction::List, &bridge.wire), &mut registry)
            .unwrap();
        match &bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry)[0] {
            TerminalEvent::SendControl(reply) => {
                assert!(reply.sessions.as_ref().unwrap().is_empty())
            }
            other => panic!("expected Sessions, got {other:?}"),
        }
    }

    /// Turning the setting off must cut the phone off at once — without ending
    /// the sessions, which belong to the desktop user.
    #[test]
    fn revoking_detaches_frees_the_lease_and_leaves_the_session_running() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let id = create(&mut bridge, &mut registry, Some(printing_command("secret")));
        attach(&mut bridge, &mut registry, &id);
        assert!(registry.is_writer(&id, "phone-1"));

        // Output is already on its way when access is withdrawn.
        poke(&mut bridge, &mut registry, &id);
        std::thread::sleep(Duration::from_millis(150));
        bridge.revoke(&mut registry);

        assert_eq!(bridge.attached.len(), 0);
        assert!(!registry.is_writer(&id, "phone-1"), "the lease survived");
        let events = bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry);
        assert!(
            output_of(&events).is_empty(),
            "output kept streaming after revocation"
        );
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::SendControl(c)
                if c.action == TerminalAction::Closed
                    && c.terminal_id.as_deref() == Some(id.as_str())
                    && c.error.as_deref() == Some(TERMINAL_NOT_ALLOWED)
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            TerminalEvent::SendControl(c)
                if c.action == TerminalAction::Error
                    && c.error.as_deref() == Some(TERMINAL_NOT_ALLOWED)
        )));

        // Nothing more is ever streamed to it, and typing goes nowhere.
        assert!(bridge.pump(DEFAULT_PUMP_BUDGET, &mut registry).is_empty());
        bridge
            .handle_chunk(&input(&id, b"still here?\n"), &mut registry)
            .unwrap();

        // The session is still running, still listed, and another device can
        // pick it up.
        let session = registry.get(&id).expect("revocation ended the session");
        assert!(session.exit_status().is_none());
        assert!(!session.is_closed());
        let mut other = new_bridge("phone-2");
        attach(&mut other, &mut registry, &id);
        assert!(registry.is_writer(&id, "phone-2"));
        registry.close(&id);
    }

    /// The host never accepts output or snapshot frames from a client — those
    /// are host-authored, and honouring one would let a peer forge screen state.
    #[test]
    fn the_host_refuses_client_authored_output() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        for kind in [ChunkKind::Output, ChunkKind::Snapshot] {
            let forged = TerminalChunk {
                kind,
                terminal_id: "abc".to_string(),
                offset: 0,
                data: b"x".to_vec(),
            };
            assert!(bridge.handle_chunk(&forged, &mut registry).is_err());
        }
    }

    /// Same rule for control: a peer replaying a host-authored action is not
    /// making a request.
    #[test]
    fn the_host_refuses_host_authored_actions() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        for action in [
            TerminalAction::Sessions,
            TerminalAction::Created,
            TerminalAction::Attached,
            TerminalAction::Closed,
            TerminalAction::Error,
        ] {
            assert!(
                bridge
                    .handle_control(&request(action, &bridge.wire), &mut registry)
                    .is_err(),
                "{action:?} should be refused"
            );
        }
    }

    #[test]
    fn attaching_to_an_unknown_session_reports_an_error() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let events = attach(
            &mut bridge,
            &mut registry,
            "00000000-0000-0000-0000-000000000000",
        );
        match &events[0] {
            TerminalEvent::SendControl(reply) => {
                assert_eq!(reply.action, TerminalAction::Error);
                assert!(reply.error.is_some());
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn an_oversized_payload_is_split_into_wire_legal_frames() {
        let chunk = TerminalChunk {
            kind: ChunkKind::Snapshot,
            terminal_id: "abc".to_string(),
            offset: 100,
            data: vec![7; MAX_TERMINAL_CHUNK_BYTES * 2 + 5],
        };
        let events = split_chunk_events(chunk);
        assert_eq!(events.len(), 3);
        let mut expected_offset = 100;
        for event in &events {
            let TerminalEvent::SendChunk(chunk) = event else {
                panic!("expected chunks");
            };
            assert!(chunk.data.len() <= MAX_TERMINAL_CHUNK_BYTES);
            assert_eq!(chunk.offset, expected_offset);
            assert!(chunk.encode().is_ok(), "split frame must be wire-legal");
            expected_offset += chunk.data.len() as u64;
        }
    }

    /// `title` comes from a client-supplied argv[0] and rides in every session
    /// list. Unbounded, thirty-two of them made a list larger than an envelope
    /// payload, which the pump could not send — and it stopped. Titles are cut
    /// to `MAX_TITLE_CHARS` (see `pty::session`), which this pins the result of.
    #[test]
    fn the_biggest_possible_session_list_fits_an_envelope() {
        use crate::device_links::pty::session::MAX_TITLE_CHARS;

        let wire = wire("phone-1");
        let mut message = TerminalControl::new(TerminalAction::Sessions, &wire);
        message.sessions = Some(
            (0..crate::device_links::pty::registry::MAX_SESSIONS)
                .map(|_| TerminalSessionInfo {
                    terminal_id: Uuid::new_v4().to_string(),
                    // Multi-byte characters: the limit is in characters, the
                    // payload in bytes.
                    title: "\u{10348}".repeat(MAX_TITLE_CHARS),
                    rows: 1000,
                    cols: 1000,
                    exit_status: Some(u32::MAX),
                })
                .collect(),
        );
        let payload = message.to_json().unwrap();
        let envelope = WebRtcEnvelope::create(
            &wire,
            WebRtcChannel::Terminal,
            TERMINAL_CONTROL_MESSAGE_TYPE,
            &payload,
            1,
        )
        .unwrap();
        assert!(
            envelope.to_json().unwrap().len() < crate::device_links::webrtc::MAX_ENVELOPE_BYTES
        );
    }

    // ---- the connection lifecycle: reconnect races and the idle reaper ----

    fn unique(prefix: &str) -> String {
        format!("{prefix}-{}", Uuid::new_v4().simple())
    }

    fn wire_attempt(peer: &str) -> WebRtcWireBinding {
        WebRtcWireBinding::from_attempt("desktop-1", peer, &Uuid::new_v4().to_string()).unwrap()
    }

    fn attach_through(wire: &WebRtcWireBinding, id: &str) {
        let mut control = request(TerminalAction::Attach, wire);
        control.terminal_id = Some(id.to_string());
        with_bridge(wire, |bridge, registry| {
            bridge.handle_control(&control, registry)
        })
        .unwrap();
    }

    fn create_through(wire: &WebRtcWireBinding) -> String {
        let mut control = request(TerminalAction::Create, wire);
        control.command = Some(sleeping_command());
        with_bridge(wire, |bridge, registry| {
            bridge.handle_control(&control, registry)
        })
        .unwrap();
        let events = pump_once(
            &wire.peer_device_id,
            current_token(wire),
            DEFAULT_PUMP_BUDGET,
        )
        .expect("bridge");
        match &events[0] {
            TerminalEvent::SendControl(reply) => reply.terminal_id.clone().unwrap(),
            other => panic!("expected Created, got {other:?}"),
        }
    }

    fn current_token(wire: &WebRtcWireBinding) -> u64 {
        bridges().lock().unwrap()[&wire.peer_device_id].token
    }

    fn close_globally(id: &str) {
        registry().lock().unwrap().close(id);
    }

    /// The old pump thread notices it has been replaced a tick after the new
    /// connection attached, and its cleanup — keyed by device id — detached
    /// the *new* attachment and freed its lease.
    #[test]
    fn a_stale_pumps_cleanup_cannot_detach_the_newer_connection() {
        let phone = unique("reconnect");
        let old_wire = wire_attempt(&phone);
        let old_token = begin(&old_wire);
        let id = create_through(&old_wire);
        attach_through(&old_wire, &id);
        assert_eq!(attached_count(&phone), 1);

        // The phone reconnects and re-attaches.
        let new_wire = wire_attempt(&phone);
        let new_token = begin(&new_wire);
        assert_ne!(old_token, new_token);
        attach_through(&new_wire, &id);
        assert_eq!(attached_count(&phone), 1);
        assert!(registry().lock().unwrap().is_writer(&id, &phone));

        // The old pump thread, only now, finds out and cleans up after itself.
        assert!(pump_once(&phone, old_token, DEFAULT_PUMP_BUDGET).is_none());
        end(&phone, old_token);

        assert_eq!(attached_count(&phone), 1, "the new attachment was detached");
        assert!(
            registry().lock().unwrap().is_writer(&id, &phone),
            "the new connection lost its write lease"
        );
        assert!(pump_once(&phone, new_token, DEFAULT_PUMP_BUDGET).is_some());

        // Messages still bound to the old connection are refused.
        let stale = with_bridge(&old_wire, |_, _| Ok(()));
        assert!(stale.is_err());

        end(&phone, new_token);
        assert_eq!(attached_count(&phone), 0);
        assert!(!registry().lock().unwrap().is_writer(&id, &phone));
        close_globally(&id);
    }

    #[test]
    fn a_new_connection_starts_with_nothing_attached() {
        let phone = unique("fresh");
        let first = wire_attempt(&phone);
        let token = begin(&first);
        let id = create_through(&first);
        attach_through(&first, &id);

        let second = wire_attempt(&phone);
        begin(&second);
        assert_eq!(
            attached_count(&phone),
            0,
            "a reconnect inherited attachments"
        );
        assert!(!registry().lock().unwrap().is_writer(&id, &phone));
        end(&phone, token); // stale: a no-op
        close_globally(&id);
        end(&phone, current_token(&second));
    }

    /// What "attached" means to the reaper is what the bridges say. Run against
    /// a private registry and bridge map: the sweep is indiscriminate by
    /// design, and the process-wide ones are shared with every other test.
    #[test]
    fn the_idle_reaper_closes_forgotten_sessions_but_not_watched_ones() {
        let mut registry = TerminalRegistry::new();
        let mut bridge = new_bridge("phone-1");
        let watched = create(&mut bridge, &mut registry, Some(sleeping_command()));
        let forgotten = create(&mut bridge, &mut registry, Some(sleeping_command()));
        attach(&mut bridge, &mut registry, &watched);
        let open = HashMap::from([("phone-1".to_string(), bridge)]);

        // Recent activity keeps even an unattended session.
        assert!(sweep_in(&open, &mut registry, Duration::from_secs(3600)).is_empty());

        let closed = sweep_in(&open, &mut registry, Duration::ZERO);
        assert_eq!(closed, vec![forgotten.clone()]);
        assert!(
            registry.get(&watched).is_some(),
            "a watched session was closed"
        );
        assert!(registry.get(&forgotten).is_none());
        registry.close(&watched);
    }

    // ---- process_envelope: consent, handover and binding gates ----

    /// A paired phone's connection as `process_envelope` sees it.
    struct Connected {
        sessions: Arc<DeviceManager>,
        binding: SessionBinding,
        attempt_id: String,
        /// The desktop's view of the wire, as the coordinator holds it.
        wire: WebRtcWireBinding,
        /// The phone's view of the same wire, which its envelopes are built from.
        phone_wire: WebRtcWireBinding,
        token: u64,
    }

    fn connect(phone: &str, features_ready: bool) -> Connected {
        let sessions = Arc::new(DeviceManager::new());
        let binding = sessions
            .register_link(
                phone.to_string(),
                SessionLink::test_placeholder(phone),
                true,
            )
            .unwrap()
            .binding;
        let attempt_id = Uuid::new_v4().to_string();
        let wire = WebRtcWireBinding::from_attempt("desktop-1", phone, &attempt_id).unwrap();
        sessions
            .with_session(&binding, |session| {
                let mut handover = HandoverRuntime::new(attempt_id.clone(), wire.clone());
                if features_ready {
                    handover.record_offer("offer".to_string()).unwrap();
                    handover.record_answer("answer".to_string()).unwrap();
                    handover.mark_local_authenticated().unwrap();
                    handover.mark_remote_authenticated().unwrap();
                    handover.mark_local_capabilities_confirmed().unwrap();
                    handover.mark_remote_capabilities_confirmed().unwrap();
                    handover.mark_local_feature_ready().unwrap();
                    handover.mark_remote_feature_ready().unwrap();
                }
                session.webrtc_attempt_id = Some(attempt_id.clone());
                session.webrtc_handover = Some(handover);
            })
            .unwrap();
        let phone_wire = WebRtcWireBinding {
            sender_device_id: phone.to_string(),
            peer_device_id: "desktop-1".to_string(),
            session_id: wire.session_id,
            generation: wire.generation,
        };
        let token = begin(&wire);
        Connected {
            sessions,
            binding,
            attempt_id,
            wire,
            phone_wire,
            token,
        }
    }

    impl Connected {
        fn control_envelope(&self, action: TerminalAction, id: Option<&str>) -> WebRtcEnvelope {
            let mut control = TerminalControl::new(action, &self.phone_wire);
            control.terminal_id = id.map(str::to_string);
            control.command = (action == TerminalAction::Create).then(sleeping_command);
            WebRtcEnvelope::create(
                &self.phone_wire,
                WebRtcChannel::Terminal,
                TERMINAL_CONTROL_MESSAGE_TYPE,
                &control.to_json().unwrap(),
                1,
            )
            .unwrap()
        }

        fn chunk_envelope(&self, id: &str, bytes: &[u8]) -> WebRtcEnvelope {
            WebRtcEnvelope::create(
                &self.phone_wire,
                WebRtcChannel::Terminal,
                TERMINAL_CHUNK_MESSAGE_TYPE,
                &input(id, bytes).encode().unwrap(),
                1,
            )
            .unwrap()
        }

        fn deliver(&self, envelope: WebRtcEnvelope) -> Result<(), String> {
            process_envelope(
                &self.binding,
                &self.attempt_id,
                &self.wire,
                envelope,
                &self.sessions,
            )
        }

        fn replies(&self) -> Vec<TerminalEvent> {
            pump_once(&self.wire.peer_device_id, self.token, DEFAULT_PUMP_BUDGET)
                .unwrap_or_default()
        }

        fn sessions_owned(&self) -> usize {
            registry()
                .lock()
                .unwrap()
                .owned_by(&self.wire.peer_device_id)
        }
    }

    /// Turns the setting on or off the way the settings switch does: through a
    /// real `Config`, which is also what keeps the process-wide view in step.
    fn set_consent(phone: &str, allowed: bool) {
        let dir = std::env::temp_dir().join(format!("desklink-consent-{}", Uuid::new_v4()));
        let mut config = Config::load_from_dir(dir.clone()).unwrap();
        config
            .trust_device(
                &DeviceInfo::local(phone.to_string(), "Test phone".to_string()),
                "cert".to_string(),
            )
            .unwrap();
        config.set_terminal_access(phone, allowed).unwrap();
        let _ = std::fs::remove_dir_all(dir);
    }

    fn error_of(events: &[TerminalEvent]) -> Option<String> {
        events.iter().find_map(|event| match event {
            TerminalEvent::SendControl(c) if c.action == TerminalAction::Error => c.error.clone(),
            _ => None,
        })
    }

    /// The default: a paired phone that has not been allowed a terminal gets
    /// told so, and nothing is created.
    #[test]
    fn a_phone_without_consent_is_denied_and_creates_nothing() {
        let phone = unique("denied");
        let connection = connect(&phone, true);
        let before = connection.sessions_owned();

        connection
            .deliver(connection.control_envelope(TerminalAction::Create, None))
            .unwrap();
        connection
            .deliver(connection.control_envelope(TerminalAction::List, None))
            .unwrap();

        let events = connection.replies();
        assert_eq!(
            error_of(&events).as_deref(),
            Some("terminal-not-allowed"),
            "{events:?}"
        );
        // Both requests were answered with a denial; neither did any work.
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, TerminalEvent::SendControl(c) if c.action == TerminalAction::Error))
                .count(),
            2
        );
        assert!(!events.iter().any(is_control(TerminalAction::Created)));
        assert!(!events.iter().any(is_control(TerminalAction::Sessions)));
        assert_eq!(connection.sessions_owned(), before);
        end(&phone, connection.token);
    }

    #[test]
    fn a_phone_with_consent_can_create_and_type() {
        let phone = unique("allowed");
        set_consent(&phone, true);
        let connection = connect(&phone, true);

        connection
            .deliver(connection.control_envelope(TerminalAction::Create, None))
            .unwrap();
        let events = connection.replies();
        let id = events
            .iter()
            .find_map(|event| match event {
                TerminalEvent::SendControl(c) if c.action == TerminalAction::Created => {
                    c.terminal_id.clone()
                }
                _ => None,
            })
            .expect("Created");
        assert!(error_of(&events).is_none(), "{events:?}");

        connection
            .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
            .unwrap();
        connection
            .deliver(connection.chunk_envelope(&id, b"hello\n"))
            .unwrap();
        assert_eq!(attached_count(&phone), 1);
        assert!(registry().lock().unwrap().is_writer(&id, &phone));

        end(&phone, connection.token);
        close_globally(&id);
    }

    /// Consent is checked on every message, not remembered from the last one:
    /// a chunk from a phone whose access was withdrawn must not reach the PTY,
    /// even if it is somehow still attached.
    #[test]
    fn input_from_a_phone_whose_consent_is_gone_never_reaches_the_pty() {
        let phone = unique("withdrawn");
        set_consent(&phone, true);
        let connection = connect(&phone, true);
        connection
            .deliver(connection.control_envelope(TerminalAction::Create, None))
            .unwrap();
        let id = connection
            .replies()
            .iter()
            .find_map(|event| match event {
                TerminalEvent::SendControl(c) if c.action == TerminalAction::Created => {
                    c.terminal_id.clone()
                }
                _ => None,
            })
            .unwrap();
        connection
            .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
            .unwrap();
        connection.replies();
        let session = registry().lock().unwrap().get(&id).unwrap();

        // Consent goes away with the phone still attached (the setting was
        // written but `revoke_device` has not run yet).
        set_consent(&phone, false);
        let idle = session.idle_for();
        std::thread::sleep(Duration::from_millis(50));
        connection
            .deliver(connection.chunk_envelope(&id, b"rm -rf ~\n"))
            .unwrap();
        assert!(
            session.idle_for() >= idle,
            "input was written after consent was withdrawn"
        );

        end(&phone, connection.token);
        close_globally(&id);
    }

    #[test]
    fn revoke_device_cuts_off_an_attached_phone_and_keeps_its_sessions() {
        let phone = unique("revoked");
        set_consent(&phone, true);
        let connection = connect(&phone, true);
        connection
            .deliver(connection.control_envelope(TerminalAction::Create, None))
            .unwrap();
        let id = connection
            .replies()
            .iter()
            .find_map(|event| match event {
                TerminalEvent::SendControl(c) if c.action == TerminalAction::Created => {
                    c.terminal_id.clone()
                }
                _ => None,
            })
            .unwrap();
        connection
            .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
            .unwrap();
        connection.replies();
        assert_eq!(attached_count(&phone), 1);

        set_consent(&phone, false);
        revoke_device(&phone);

        assert_eq!(attached_count(&phone), 0);
        assert!(!registry().lock().unwrap().is_writer(&id, &phone));
        let events = connection.replies();
        assert!(events.iter().any(is_control(TerminalAction::Closed)));
        assert_eq!(error_of(&events).as_deref(), Some("terminal-not-allowed"));
        assert!(
            registry().lock().unwrap().get(&id).is_some(),
            "revoking access ended the session"
        );

        // And it stays cut off.
        connection
            .deliver(connection.control_envelope(TerminalAction::Attach, Some(&id)))
            .unwrap();
        assert_eq!(attached_count(&phone), 0);
        end(&phone, connection.token);
        close_globally(&id);
    }

    #[test]
    fn a_connection_that_has_not_finished_the_handover_is_rejected() {
        let phone = unique("unready");
        set_consent(&phone, true);
        let connection = connect(&phone, false);
        let before = connection.sessions_owned();

        assert!(connection
            .deliver(connection.control_envelope(TerminalAction::Create, None))
            .is_err());
        assert!(connection
            .deliver(connection.chunk_envelope("abc", b"x"))
            .is_err());
        assert_eq!(connection.sessions_owned(), before);
        end(&phone, connection.token);
    }

    /// A binding that has been replaced must be refused for input chunks as
    /// much as for control messages; chunks used to skip the check.
    #[test]
    fn a_stale_binding_is_rejected_for_controls_and_for_chunks() {
        let phone = unique("stale");
        set_consent(&phone, true);
        let connection = connect(&phone, true);
        // The transport is replaced under the connection.
        connection
            .sessions
            .register_link(phone.clone(), SessionLink::test_placeholder(&phone), true)
            .unwrap();
        assert!(!connection.sessions.is_current(&connection.binding));

        assert!(connection
            .deliver(connection.control_envelope(TerminalAction::Create, None))
            .is_err());
        assert!(connection
            .deliver(connection.chunk_envelope("abc", b"x"))
            .is_err());
        assert_eq!(attached_count(&phone), 0);
        end(&phone, connection.token);
    }

    #[test]
    fn an_envelope_on_the_wrong_channel_is_rejected() {
        let phone = unique("channel");
        set_consent(&phone, true);
        let connection = connect(&phone, true);
        let mut envelope = connection.control_envelope(TerminalAction::List, None);
        envelope.channel = WebRtcChannel::Events.label().to_string();
        assert!(connection.deliver(envelope).is_err());
        end(&phone, connection.token);
    }
}
