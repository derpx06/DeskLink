//! One device's terminal state machine ([`TerminalBridge`]): its attachments,
//! the ordered outbox and its byte budget, control handling (open, attach,
//! input, resize, size claim, detach, close) and the events it answers with.
//! Touches no data channel and no global state.
//!
//! Sibling edges: none (the parent's events and limits). Outside: `pty`
//! (registry, launchers, desk), `terminal_protocol`, `config`.

use super::*;

/// A client's view of one session.
///
/// Holds the session itself rather than looking it up by id each pump: a
/// session another device's list or a `create` has reaped from the registry —
/// because its shell exited — is still here, so its exit status is still
/// reported instead of the terminal simply vanishing.
pub(super) struct Attachment {
    session: Arc<PtySession>,
    /// Next output offset this peer has yet to receive.
    next: u64,
    /// The size this peer was last told the session has.
    told_size: (u16, u16),
    /// Whether the phone was last told a desk window shows this session, and
    /// whether it holds the size.
    told_desk_window: (bool, bool),
    /// The size this peer's screen wants, from its last `Resize`.
    viewport: Option<(u16, u16)>,
}

pub(super) struct TerminalBridge {
    /// The connection this bridge serves. Messages that arrive bound to any
    /// other connection are stale, and [`Self::token`] is how a pump thread
    /// finds out that it has been superseded.
    pub(super) wire: WebRtcWireBinding,
    pub(super) token: u64,
    pub(super) attached: HashMap<String, Attachment>,
    /// Everything waiting to go to the peer, in order.
    pub(super) outbox: VecDeque<TerminalEvent>,
    outbox_bytes: usize,
    /// One "input was refused" report per burst, not one per keystroke chunk.
    input_refusal_reported: bool,
    /// Terminals another phone is attached to, refreshed before each use
    /// (for `sessions[].attached`).
    pub(super) elsewhere: HashSet<String>,
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
            elsewhere: HashSet::new(),
        }
    }

    pub(super) fn device(&self) -> &str {
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
    pub(super) fn queue_always(&mut self, event: TerminalEvent) {
        self.outbox_bytes += event.cost();
        self.outbox.push_back(event);
    }

    pub(super) fn refuse(&mut self, terminal_id: Option<String>, message: &str) {
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
    /// [`Self::handle_control_in`], resolving `create`'s folder here (tests).
    #[cfg(test)]
    pub(crate) fn handle_control(
        &mut self,
        control: &TerminalControl,
        registry: &mut TerminalRegistry,
    ) -> Result<(), String> {
        self.handle_control_in(control, registry, &Start::of(control))
    }

    /// Handles one control message; `start` is `create`'s folder, resolved
    /// by the caller before any lock was taken.
    pub(crate) fn handle_control_in(
        &mut self,
        control: &TerminalControl,
        registry: &mut TerminalRegistry,
        start: &Start,
    ) -> Result<(), String> {
        match control.action {
            TerminalAction::List => {
                let sessions = sessions_event(&self.wire, registry, &self.elsewhere);
                self.queue(vec![sessions])
            }

            TerminalAction::Create if matches!(start, Start::Bad) => {
                audit("create-refused", self.device(), None);
                self.refuse(None, BAD_CWD);
                Ok(())
            }

            TerminalAction::Create => {
                let command = control.command.as_ref().map(|argv| {
                    // An argument vector, never a shell string: nothing here is
                    // word-split, glob-expanded or passed to `sh -c`.
                    CommandBuilder::from_argv(argv.iter().map(Into::into).collect())
                });
                let rows = control.rows.unwrap_or(24);
                let cols = control.cols.unwrap_or(80);
                let cwd = match start {
                    Start::In(path) => Some(path.clone()),
                    _ => None,
                };
                let made = registry.create_in(
                    command,
                    rows,
                    cols,
                    &self.wire.peer_device_id,
                    cwd.as_deref(),
                    None,
                );
                match made {
                    Ok(info) => {
                        audit("create", self.device(), Some(&info.id));
                        // The phone that made it sets its size: the window that
                        // opens at the desk attaches without resizing it.
                        if let Some(session) = registry.get(&info.id) {
                            let _ = session.claim_for_phone(&self.wire.peer_device_id, rows, cols);
                        }
                        // The same session opens at the desk, so it can be
                        // carried on there (not from tests: no windows).
                        if !cfg!(test) {
                            crate::device_links::pty::desk::open_on_desktop(&info.id);
                        }
                        let mut reply = TerminalControl::new(TerminalAction::Created, &self.wire);
                        reply.terminal_id = Some(info.id);
                        reply.rows = Some(info.rows);
                        reply.cols = Some(info.cols);
                        let sessions = sessions_event(&self.wire, registry, &self.elsewhere);
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
                // The list first: it carries the size the screen below was drawn
                // for, which the phone may not have heard since it last looked.
                let mut events = vec![sessions_event(&self.wire, registry, &self.elsewhere)];
                events.extend(split_chunk_events(chunk));
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
                let desk_window = (session.has_desk_window(), session.phone_sets_size());
                self.attached.insert(
                    id.to_string(),
                    Attachment {
                        session,
                        next,
                        told_size: (rows, cols),
                        told_desk_window: desk_window,
                        viewport: None,
                    },
                );
                Ok(())
            }

            TerminalAction::Detach => {
                let id = require_id(control)?;
                if let Some(attachment) = self.attached.remove(id) {
                    attachment.session.touch();
                    attachment
                        .session
                        .release_phone_claim(&self.wire.peer_device_id);
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
                let sessions = sessions_event(&self.wire, registry, &self.elsewhere);
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
                let device = self.wire.peer_device_id.as_str();
                let writer = registry.is_writer(id, device);
                match control.claim_size {
                    // The person asked for the phone's size, or to give it back.
                    // Only the device that types may.
                    Some(true) if writer => attachment.session.claim_for_phone(device, rows, cols),
                    Some(false) if writer => {
                        attachment.session.release_phone_claim(device);
                        attachment.told_size = (0, 0);
                        Ok(())
                    }
                    // Only the writer resizes, and never over an open desk window
                    // that owns the size, nor another phone's claim (checked and
                    // resized in one step): a viewer's phone rotating must not
                    // reflow the screen out from under whoever is typing.
                    _ => {
                        let sized = writer
                            && attachment
                                .session
                                .resize_unless_held(device, rows, cols, false)?;
                        if !sized {
                            // It has not changed its own screen (the phone
                            // follows the host's size); telling it the real one
                            // again keeps that true.
                            attachment.told_size = (0, 0);
                        }
                        Ok(())
                    }
                }
            }

            // Host-authored actions. A peer sending one is not making a request;
            // dispatching on the action rather than on which fields are present
            // is what keeps a report from being mistaken for an instruction.
            TerminalAction::Sessions
            | TerminalAction::Created
            | TerminalAction::Finished
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
            // Never over a desk window's size, nor over another phone's claim.
            let _ = attachment.session.resize_unless_held(
                &self.wire.peer_device_id,
                viewport.0,
                viewport.1,
                true,
            );
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
        // So does a desk window opening or closing: the phone answers the
        // program's terminal queries only while no desk window does.
        let size = session.size();
        let desk_window = (session.has_desk_window(), session.phone_sets_size());
        if size != attachment.told_size || desk_window != attachment.told_desk_window {
            if let Some(attachment) = self.attached.get_mut(id) {
                attachment.told_size = size;
                attachment.told_desk_window = desk_window;
            }
            let list = sessions_event(&self.wire, registry, &self.elsewhere);
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
        self.end(registry, TERMINAL_NOT_ALLOWED);
    }

    /// Like [`Self::revoke`], but `reason` says why; access is not involved.
    pub(crate) fn end(&mut self, registry: &mut TerminalRegistry, reason: &str) {
        self.outbox.clear();
        self.outbox_bytes = 0;
        for (id, attachment) in std::mem::take(&mut self.attached) {
            attachment.session.touch();
            attachment
                .session
                .release_phone_claim(&self.wire.peer_device_id);
            audit("revoke-detach", self.device(), Some(&id));
            let mut closed = TerminalControl::new(TerminalAction::Closed, &self.wire);
            closed.terminal_id = Some(id);
            closed.error = Some(reason.to_string());
            self.queue_always(TerminalEvent::SendControl(closed));
        }
        registry.release_writer(&self.wire.peer_device_id);
        let error = error_event(&self.wire, None, reason.to_string());
        self.queue_always(error);
        audit("revoke", self.device(), None);
    }

    /// Forgets every attachment and lease, e.g. when the connection ends.
    pub(crate) fn detach_all(&mut self, registry: &mut TerminalRegistry) {
        for (id, attachment) in std::mem::take(&mut self.attached) {
            attachment.session.touch();
            attachment
                .session
                .release_phone_claim(&self.wire.peer_device_id);
            audit("detach", self.device(), Some(&id));
        }
        self.outbox.clear();
        self.outbox_bytes = 0;
        registry.release_writer(&self.wire.peer_device_id);
    }

    pub(super) fn attached_ids(&self) -> impl Iterator<Item = &String> {
        self.attached.keys()
    }

    #[cfg(test)]
    pub(super) fn offset_for(&self, id: &str) -> Option<u64> {
        self.attached.get(id).map(|attachment| attachment.next)
    }
}

/// The error a `create` gets when its folder is not a readable directory.
const BAD_CWD: &str = "bad-cwd";

/// Where a `create` asked to start, resolved; `None` when it asked for
/// nothing or for somewhere unusable.
/// Where a `create` starts. Resolved before any lock is taken: checking a
/// folder on a hung network mount blocks, and the bridges and registry locks
/// are also taken by the UI thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Start {
    /// No folder asked for (or not a `create`): the home folder.
    Home,
    In(std::path::PathBuf),
    /// Asked for a folder that is not an existing readable directory.
    Bad,
}

impl Start {
    pub(crate) fn of(control: &TerminalControl) -> Self {
        let Some(asked) = control.cwd.as_deref() else {
            return Self::Home;
        };
        if control.action != TerminalAction::Create {
            return Self::Home;
        }
        dirs::home_dir()
            .and_then(|home| crate::device_links::pty::registry::resolve_cwd(asked, &home))
            .map_or(Self::Bad, Self::In)
    }
}

/// Splits an oversized payload into wire-legal frames.
///
/// A snapshot of a large screen exceeds one frame, and the receiving VT parser
/// is stateful across frames, so cutting anywhere is safe.
pub(super) fn split_chunk_events(chunk: TerminalChunk) -> Vec<TerminalEvent> {
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
fn sessions_event(
    wire: &WebRtcWireBinding,
    registry: &mut TerminalRegistry,
    elsewhere: &HashSet<String>,
) -> TerminalEvent {
    let listed = registry.list();
    registry.reap();
    let mut message = TerminalControl::new(TerminalAction::Sessions, wire);
    let recent = if cfg!(test) {
        Vec::new()
    } else {
        crate::device_links::config::app_settings().recent_folders
    };
    message.folders = Some(crate::device_links::pty::registry::folders(
        &listed, &recent,
    ));
    message.sessions = Some(
        listed
            .into_iter()
            .map(|info| {
                // Attached by a phone other than the one asking.
                let attached = elsewhere.contains(&info.id);
                TerminalSessionInfo {
                    terminal_id: info.id,
                    title: info.title,
                    rows: info.rows,
                    cols: info.cols,
                    exit_status: info.exit_status,
                    desk_window: info.desk_window,
                    phone_size: info.phone_size,
                    attached,
                    cwd: info.cwd,
                    started_at: Some(info.started_at),
                    shell: info.shell,
                }
            })
            .collect(),
    );
    let launchers = crate::device_links::pty::launchers::installed();
    message.launchers = (!launchers.is_empty()).then_some(launchers);
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
