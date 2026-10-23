//! File transfers over WebRTC, version 2: blocks, hashes and one file thread.
//!
//! Everything slow happens on a thread of its own, per peer: reading and
//! hashing a sender's blocks, writing and hashing a receiver's frames,
//! checkpointing with `fsync`. The libwebrtc callback thread only queues a
//! frame; the peer's event worker only queues a control message. So a 1 GB
//! transfer cannot hold up a heartbeat.
//!
//! The wire is `file_protocol` (read its header comment first). In short:
//!
//! * A **sender** reads its source in place (no copy), re-checking size and
//!   mtime before every block, and sends 1 MiB blocks as frames while the data
//!   channel holds less than `IN_FLIGHT_TARGET` and less than `UNACKED_MAX`
//!   is unacknowledged. Two files move at once; the rest wait.
//! * A **receiver** writes each frame at its offset, hashes the block as it
//!   lands, compares with the block's trailer and acknowledges per block. It
//!   checkpoints (partial file synced, block hashes, then the checkpoint)
//!   every 8 blocks or 2 s, so a resume loses less than a block after a link
//!   drop and a couple of seconds after a crash. Nothing ever re-reads a file.
//! * Nothing has a deadline. A stall only shows as a note; a full disk, a
//!   backlog or a silent peer pause the transfer, and it goes on by itself.

mod recv;
#[cfg(test)]
mod replay;
mod send;
mod store;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::file_protocol::{
    code, id_bytes, reason, validate_relpath, FileAction, FileControl, FolderRef,
    CHECKPOINT_BLOCKS, CHECKPOINT_MS, FILE_QUEUE_MAX_BYTES, MAX_TRANSFER_BYTES, NO_PROGRESS_MS,
};
use super::transfer_board::{self as board, Outcome, TransferRow};
use super::WebRtcWireBinding;
use recv::Incoming;
use send::Outgoing;
use store::Store;

/// The largest clipboard image accepted or sent (contract C3).
pub const MAX_CLIPBOARD_IMAGE_BYTES: u64 = 32 * 1024 * 1024;

/// Whether an offer is a clipboard image this side places on its clipboard:
/// both share fields, PNG or JPEG (a phone's photo, F3), and within the size
/// cap.
fn is_clipboard_offer(message: &FileControl) -> bool {
    message.clipboard == Some(true)
        && matches!(
            message.mime_type.as_deref(),
            Some("image/png" | "image/jpeg")
        )
        && message
            .total_size
            .is_some_and(|size| size <= MAX_CLIPBOARD_IMAGE_BYTES)
}

/// What the manager sends through: one peer's file channels.
pub trait FileLink: Send + Sync {
    /// A control message, on `desklink-file-control-v1`.
    fn send_control(&self, control: &FileControl) -> Result<(), String>;
    /// One frame, on `desklink-file-data-v2`.
    fn send_frame(&self, frame: Vec<u8>) -> Result<(), String>;
    /// Bytes queued on the data channel and not yet on the network.
    fn buffered(&self) -> u64;
}

/// A transfer that finished.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completed {
    pub transfer_id: String,
    pub path: PathBuf,
    /// The photo or scan a desk asked for (F3), if this was one.
    pub capture_id: Option<String>,
    pub incoming: bool,
}

type CompletionHook = Box<dyn Fn(&Completed) + Send + Sync>;

/// A link and the binding its messages carry.
pub(super) struct Net {
    pub link: Arc<dyn FileLink>,
    pub wire: WebRtcWireBinding,
}

impl Net {
    /// A message for transfer `id` from this side.
    pub fn msg(&self, action: FileAction, id: &str, token: &str) -> FileControl {
        FileControl::new(&self.wire, action, id, token)
    }

    pub fn send(&self, control: FileControl) {
        if let Err(error) = self.link.send_control(&control) {
            crate::device_links::limits::log_limited(
                "file-control-send",
                &format!("A file control message was not sent: {error}"),
            );
        }
    }
}

/// How a transfer ended, for the peer and for the person.
pub(super) struct Ending {
    /// What the peer is told (`error.code`).
    pub code: &'static str,
    /// What the person reads.
    pub words: String,
    /// Whether the peer is told at all.
    pub tell_peer: bool,
}

impl Ending {
    pub fn internal(words: impl Into<String>) -> Self {
        Self {
            code: code::INTERNAL,
            words: words.into(),
            tell_peer: true,
        }
    }
}

/// The timings, which tests shorten.
#[derive(Debug, Clone)]
pub(super) struct Tunables {
    pub checkpoint_blocks: u64,
    pub checkpoint_after: Duration,
    pub no_progress: Duration,
    pub storage_retry: Duration,
    pub busy_retry: Duration,
    /// Queued frame bytes at which receivers pause with `backlog`...
    pub queue_pause: usize,
    /// ...and below which they resume.
    pub queue_resume: usize,
    pub report_every: Duration,
    /// Free bytes a receiver wants before it resumes after "storage full".
    pub storage_slack: u64,
}

impl Default for Tunables {
    fn default() -> Self {
        Self {
            checkpoint_blocks: CHECKPOINT_BLOCKS,
            checkpoint_after: Duration::from_millis(CHECKPOINT_MS),
            no_progress: Duration::from_millis(NO_PROGRESS_MS),
            storage_retry: Duration::from_secs(5),
            busy_retry: Duration::from_secs(2),
            queue_pause: FILE_QUEUE_MAX_BYTES / 2,
            queue_resume: FILE_QUEUE_MAX_BYTES / 8,
            report_every: Duration::from_millis(250),
            storage_slack: 8 * 1024 * 1024,
        }
    }
}

/// Failures tests inject.
#[derive(Debug, Default)]
pub(super) struct Faults {
    /// Writes to partial files fail as if the disk were full.
    pub disk_full: bool,
    /// Free space as the receiver sees it, instead of the filesystem's.
    pub free_bytes: Option<u64>,
    /// A crash: the final checkpoint on shutdown is skipped.
    pub crash: bool,
}

/// Tracks whether a transfer has stopped moving. Not a deadline: after
/// `limit` of silence the first call reports `Began` (a note is shown and the
/// peer told, informationally), and the first movement after it `Ended`.
#[derive(Debug)]
pub(super) struct Stall {
    last_move: Instant,
    flagged: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum StallChange {
    None,
    Began,
    Ended,
}

impl Stall {
    pub fn new(now: Instant) -> Self {
        Self {
            last_move: now,
            flagged: false,
        }
    }

    /// Bytes moved.
    pub fn moved(&mut self, now: Instant) {
        self.last_move = now;
    }

    /// Looks at the clock. `waiting`: the transfer is paused on purpose
    /// (storage, backlog, the peer's request) and is not "stalled".
    pub fn check(&mut self, now: Instant, limit: Duration, waiting: bool) -> StallChange {
        let silent = now.saturating_duration_since(self.last_move) >= limit && !waiting;
        match (silent, self.flagged) {
            (true, false) => {
                self.flagged = true;
                StallChange::Began
            }
            (false, true) => {
                self.flagged = false;
                StallChange::Ended
            }
            _ => StallChange::None,
        }
    }
}

struct Shared {
    /// Frame bytes handed to the file thread and not yet handled.
    queued: AtomicUsize,
    /// A frame did not fit the queue and was dropped: receivers restart.
    overflow: AtomicBool,
    queue_max: usize,
    attached: AtomicBool,
    hook: Mutex<Option<CompletionHook>>,
}

enum Cmd {
    Attach(WebRtcWireBinding, Arc<dyn FileLink>),
    Detach,
    Control(Box<FileControl>),
    Frame(Vec<u8>),
    Start(Box<Outgoing>),
    Cancel {
        id: String,
        code: &'static str,
        reply: SyncSender<Result<(), String>>,
    },
    ResumeSends,
    Wake,
    #[cfg(test)]
    Probe(Box<dyn FnOnce(&mut Engine) + Send>),
}

/// A restart-safe, bounded file transfer manager for one WebRTC peer. Cheap
/// to call from any thread: every method queues work for the file thread.
pub struct WebRtcTransferManager {
    commands: Sender<Cmd>,
    shared: Arc<Shared>,
    store: Store,
    thread: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl WebRtcTransferManager {
    pub fn new(state_root: PathBuf, destination_root: PathBuf) -> Result<Self, String> {
        Self::with(
            state_root,
            destination_root,
            Tunables::default(),
            FILE_QUEUE_MAX_BYTES,
        )
    }

    fn with(
        state_root: PathBuf,
        destination_root: PathBuf,
        tun: Tunables,
        queue_max: usize,
    ) -> Result<Self, String> {
        let store = Store::new(state_root, destination_root)?;
        let shared = Arc::new(Shared {
            queued: AtomicUsize::new(0),
            overflow: AtomicBool::new(false),
            queue_max,
            attached: AtomicBool::new(false),
            hook: Mutex::new(None),
        });
        let (commands, inbox) = mpsc::channel();
        let engine = Engine {
            store: store.clone(),
            shared: Arc::clone(&shared),
            tun,
            net: None,
            incoming: HashMap::new(),
            outgoing: Vec::new(),
            seq: 0,
            resumed: false,
            link_down_until: None,
            faults: Faults::default(),
            closed: std::collections::VecDeque::new(),
        };
        let thread = std::thread::Builder::new()
            .name("desklink-file".into())
            .spawn(move || engine.run(inbox))
            .map_err(|error| format!("Could not start the file transfer thread: {error}"))?;
        Ok(Self {
            commands,
            shared,
            store,
            thread: Mutex::new(Some(thread)),
        })
    }

    /// Binds the manager to a peer's channels. Until then nothing is sent.
    pub fn attach(&self, wire: WebRtcWireBinding, link: Arc<dyn FileLink>) {
        self.shared.attached.store(true, Ordering::Release);
        let _ = self.commands.send(Cmd::Attach(wire, link));
    }

    /// The peer is gone: receivers checkpoint at once, nothing more is sent.
    pub fn detach(&self) {
        self.shared.attached.store(false, Ordering::Release);
        let _ = self.commands.send(Cmd::Detach);
    }

    /// The data channel's buffer drained below its low threshold: a sender
    /// waiting for room tries again now.
    pub fn wake(&self) {
        let _ = self.commands.send(Cmd::Wake);
    }

    /// One control message of the peer (already unwrapped from its envelope).
    /// Validation happens on the file thread.
    pub fn on_control(&self, message: FileControl) {
        let _ = self.commands.send(Cmd::Control(Box::new(message)));
    }

    /// One binary message of the data channel. Does nothing but queue it. A
    /// message past the queue's bound is dropped and every receiver restarts
    /// at its last verified block: part of a stream is never lost silently.
    pub fn on_frame(&self, frame: Vec<u8>) {
        let length = frame.len();
        let queued = self.shared.queued.fetch_add(length, Ordering::AcqRel);
        if queued + length > self.shared.queue_max || self.shared.overflow.load(Ordering::Acquire) {
            self.shared.queued.fetch_sub(length, Ordering::AcqRel);
            self.shared.overflow.store(true, Ordering::Release);
            crate::device_links::limits::log_limited(
                "file-frame-overflow",
                "A peer sent file data faster than DeskLink could handle; its transfers restart",
            );
            let _ = self.commands.send(Cmd::Wake);
            return;
        }
        if self.commands.send(Cmd::Frame(frame)).is_err() {
            self.shared.queued.fetch_sub(length, Ordering::AcqRel);
        }
    }

    /// What to do when a transfer completes (a photo arrived, F3).
    pub fn set_completion_hook(&self, hook: impl Fn(&Completed) + Send + Sync + 'static) {
        if let Ok(mut slot) = self.shared.hook.lock() {
            *slot = Some(Box::new(hook));
        }
    }

    /// Offers again every send that a lost connection interrupted, so the
    /// peer carries on from what it already has instead of the user starting
    /// over. Once per connection.
    pub fn resume_sends(&self) {
        let _ = self.commands.send(Cmd::ResumeSends);
    }

    /// Sends `source` to the peer. Returns the transfer's id.
    pub fn start_send(&self, source: &Path) -> Result<String, String> {
        self.start(source, None, None, None)
    }

    /// Sends `source` as a clipboard copy (C3) of type `mime`: the peer
    /// places it on its clipboard, and it is not listed here either.
    pub fn start_send_as(&self, source: &Path, mime: Option<&str>) -> Result<String, String> {
        self.start(source, mime, None, None)
    }

    /// Sends a synced file (F8): offered with its pair, its path and its
    /// manifest content hash, not listed on the Transfers page.
    pub fn start_send_folder(
        &self,
        source: &Path,
        folder: FolderRef,
        content_sha256: &str,
    ) -> Result<String, String> {
        validate_relpath(&folder.relpath)?;
        if super::file_protocol::unhex32(content_sha256).is_none() {
            return Err("DeskLink synced file has no content hash".to_string());
        }
        self.start(source, None, Some(folder), Some(content_sha256.to_string()))
    }

    fn start(
        &self,
        source: &Path,
        clipboard_mime: Option<&str>,
        folder: Option<FolderRef>,
        content_sha256: Option<String>,
    ) -> Result<String, String> {
        if !self.shared.attached.load(Ordering::Acquire) {
            return Err("DeskLink WebRTC file transport is unavailable".to_string());
        }
        let outgoing = Outgoing::prepare(
            &self.store,
            source,
            clipboard_mime,
            folder,
            content_sha256,
            None,
        )?;
        let id = outgoing.id.clone();
        self.commands
            .send(Cmd::Start(Box::new(outgoing)))
            .map_err(|_| "DeskLink file transfer thread has stopped".to_string())?;
        Ok(id)
    }

    /// Ends a transfer at the user's request (or, with `code::REMOVED`, a
    /// Bucket item removed while it moves), in either direction, and tells
    /// the peer. Nothing is left on disk: a cancelled transfer is never
    /// resumed.
    pub fn cancel_transfer(&self, transfer_id: &str, why: &'static str) -> Result<(), String> {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(Cmd::Cancel {
                id: transfer_id.to_string(),
                code: why,
                reply,
            })
            .map_err(|_| "DeskLink file transfer thread has stopped".to_string())?;
        answer
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "DeskLink file transfer thread did not answer".to_string())?
    }

    /// Tests: sends `source` under the given id and token (transcripts).
    #[cfg(test)]
    fn start_send_fixed(&self, source: &Path, id: &str, token: &str) -> Result<String, String> {
        let outgoing = Outgoing::prepare(&self.store, source, None, None, None, Some((id, token)))?;
        self.commands
            .send(Cmd::Start(Box::new(outgoing)))
            .map_err(|_| "DeskLink file transfer thread has stopped".to_string())?;
        Ok(id.to_string())
    }

    /// Ends the file thread and waits for it: its final checkpoints are on
    /// disk when this returns.
    #[cfg(test)]
    fn stop(self) {
        let Self {
            commands, thread, ..
        } = self;
        drop(commands);
        if let Some(handle) = thread.lock().ok().and_then(|mut slot| slot.take()) {
            let _ = handle.join();
        }
    }

    /// Runs `job` on the file thread with the engine, for tests.
    #[cfg(test)]
    fn probe<T: Send + 'static>(&self, job: impl FnOnce(&mut Engine) -> T + Send + 'static) -> T {
        let (reply, answer) = mpsc::sync_channel(1);
        self.commands
            .send(Cmd::Probe(Box::new(move |engine| {
                let _ = reply.send(job(engine));
            })))
            .expect("the file thread runs");
        answer
            .recv_timeout(Duration::from_secs(10))
            .expect("the file thread answers")
    }
}

/// Everything the file thread owns.
struct Engine {
    store: Store,
    shared: Arc<Shared>,
    tun: Tunables,
    net: Option<Arc<Net>>,
    incoming: HashMap<[u8; 16], Incoming>,
    outgoing: Vec<Outgoing>,
    seq: u64,
    resumed: bool,
    /// A frame could not be sent (the channel is closing): wait a moment.
    link_down_until: Option<Instant>,
    faults: Faults,
    /// Transfers that ended or were refused lately, and how: what a frame
    /// that belongs to none is told apart by (the last 64).
    closed: std::collections::VecDeque<([u8; 16], &'static str)>,
}

impl Engine {
    /// Notes how the transfer `key` ended, for [`why_unknown`](Self::why_unknown).
    fn note_closed(&mut self, key: [u8; 16], how: &'static str) {
        self.closed.retain(|(known, _)| *known != key);
        if self.closed.len() >= 64 {
            self.closed.pop_front();
        }
        self.closed.push_back((key, how));
    }

    /// Why a frame for `key` has no transfer.
    fn why_unknown(&self, key: &[u8; 16]) -> &'static str {
        self.closed
            .iter()
            .rev()
            .find(|(known, _)| known == key)
            .map_or("never offered", |(_, how)| how)
    }

    /// An offer was refused: said in the log with its transfer id and why,
    /// remembered, and (unless the sender just has to wait, or the Bucket
    /// asked for nothing the person wants to hear about) shown in the list.
    fn refused(&mut self, message: &FileControl, why: &'static str, shown: Option<&str>) {
        crate::device_links::limits::log_limited(
            "file-offer-refused",
            &format!(
                "Refused file offer {} ({}): {why}",
                message.transfer_id,
                message.filename.as_deref().unwrap_or("no name")
            ),
        );
        if let Some(key) = id_bytes(&message.transfer_id) {
            self.note_closed(key, "refused");
        }
        if let (Some(words), Some(net)) = (shown, self.net.as_ref()) {
            board::record(board::Finished {
                id: message.transfer_id.clone(),
                device_id: net.wire.peer_device_id.clone(),
                name: message.filename.clone().unwrap_or_default(),
                incoming: true,
                outcome: Outcome::Failed(words.to_string()),
                retry: None,
            });
        }
    }

    fn run(mut self, inbox: Receiver<Cmd>) {
        loop {
            let wait = self.wait_hint();
            match inbox.recv_timeout(wait) {
                Ok(command) => self.handle(command),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            // Whatever else is already waiting, before the next round of
            // sending: acknowledgements must not queue behind frames.
            for _ in 0..512 {
                match inbox.try_recv() {
                    Ok(command) => self.handle(command),
                    Err(_) => break,
                }
            }
            crate::guard("file transfer", (), || self.tick());
        }
        self.shutdown();
    }

    /// How long the thread may sleep: briefly while a sender may be waiting
    /// for room in the channel, else long enough for the timers.
    fn wait_hint(&self) -> Duration {
        if self.outgoing.iter().any(Outgoing::is_pumping) {
            Duration::from_millis(2)
        } else {
            Duration::from_millis(50)
        }
    }

    fn handle(&mut self, command: Cmd) {
        crate::guard("file transfer", (), || match command {
            Cmd::Attach(wire, link) => {
                self.net = Some(Arc::new(Net { link, wire }));
            }
            Cmd::Detach => {
                self.checkpoint_all();
                self.net = None;
            }
            Cmd::Control(message) => self.on_control(*message),
            Cmd::Frame(frame) => {
                self.shared.queued.fetch_sub(frame.len(), Ordering::AcqRel);
                self.on_frame(&frame);
            }
            Cmd::Start(outgoing) => self.start_outgoing(*outgoing),
            Cmd::Cancel { id, code, reply } => {
                let _ = reply.send(self.cancel(&id, code));
            }
            Cmd::ResumeSends => self.resume_sends(),
            Cmd::Wake => {}
            #[cfg(test)]
            Cmd::Probe(job) => job(self),
        });
    }

    /// One round of everything that is not a reaction to a message.
    fn tick(&mut self) {
        let now = Instant::now();
        self.restart_after_overflow();
        self.apply_backlog();
        self.retry_after_storage_full(now);
        self.promote_queued(now);
        self.verify_prefixes();
        self.finish_acknowledged();
        self.pump_sends();
        self.checkpoint_due(now);
        self.report_progress(now);
        self.watch_for_stalls(now);
    }

    fn on_control(&mut self, message: FileControl) {
        let Some(net) = self.net.clone() else {
            return;
        };
        if let Err(error) = message.validate(&net.wire) {
            crate::device_links::limits::log_limited(
                "file-control-rejected",
                &format!("Rejected a file control message: {error}"),
            );
            // An offer that cannot be accepted is answered, not dropped (the
            // sender would wait for ever), when it is plainly from the peer.
            let from_peer = message.device_id == net.wire.peer_device_id
                && message.session_id == net.wire.session_id
                && message.connection_generation == net.wire.generation
                && id_bytes(&message.transfer_id).is_some()
                && !message.transfer_token.is_empty()
                && message.transfer_token.len() <= 256
                && message.transfer_token.is_ascii();
            if message.action == FileAction::Offer && from_peer {
                let too_big = message
                    .total_size
                    .is_some_and(|size| size > MAX_TRANSFER_BYTES);
                self.refused(
                    &message,
                    "the offer was not valid",
                    Some(if too_big {
                        "That file is over 4 GiB, which DeskLink does not receive."
                    } else {
                        "The phone sent a file DeskLink could not accept."
                    }),
                );
                let mut refusal = net.msg(
                    FileAction::Error,
                    &message.transfer_id,
                    &message.transfer_token,
                );
                if message
                    .total_size
                    .is_some_and(|size| size > MAX_TRANSFER_BYTES)
                {
                    refusal.code = Some(code::TOO_LARGE.to_string());
                } else {
                    refusal.code = Some(code::REFUSED.to_string());
                    refusal.message = Some("DeskLink cannot accept that file".to_string());
                }
                net.send(refusal);
            }
            return;
        }
        match message.action {
            FileAction::Offer => self.on_offer(&net, message),
            FileAction::Accept => self.on_accept(&net, &message),
            FileAction::BlockAck => self.on_block_ack(&net, &message),
            FileAction::BlockNack => self.on_block_nack(&net, &message),
            FileAction::Done => self.on_done(&net, &message),
            FileAction::Complete => self.on_complete(&message),
            FileAction::Pause => self.on_pause(&message),
            FileAction::Resume => self.on_resume(&net, &message),
            FileAction::Cancel => self.on_cancel(&message),
            FileAction::Error => self.on_error(&message),
        }
    }

    /// Whether `token` opens the transfer `id`, in either direction.
    fn owns(&self, id: &str, token: &str) -> bool {
        id_bytes(id).is_some_and(|key| {
            self.incoming
                .get(&key)
                .is_some_and(|incoming| incoming.token == token)
        }) || self
            .outgoing
            .iter()
            .any(|outgoing| outgoing.id == id && outgoing.token == token)
    }

    /// A peer's `pause`: a sender holds its frames; a receiver just shows it.
    fn on_pause(&mut self, message: &FileControl) {
        if !self.owns(&message.transfer_id, &message.transfer_token) {
            return;
        }
        let why = message.reason.clone().unwrap_or_default();
        match self
            .outgoing
            .iter_mut()
            .find(|outgoing| outgoing.id == message.transfer_id)
        {
            Some(outgoing) => outgoing.on_peer_pause(&why),
            None => board::set_note(&message.transfer_id, note_for_peer_pause(&why).as_deref()),
        }
    }

    /// A peer's `resume`. With a restart point (a receiver's) the sender goes
    /// back to it; without one it only clears what was shown.
    fn on_resume(&mut self, net: &Net, message: &FileControl) {
        if !self.owns(&message.transfer_id, &message.transfer_token) {
            return;
        }
        let Some(index) = self
            .outgoing
            .iter()
            .position(|outgoing| outgoing.id == message.transfer_id)
        else {
            board::set_note(&message.transfer_id, None);
            return;
        };
        if let Err(ending) = self.outgoing[index].on_peer_resume(message) {
            self.end_outgoing(index, Some(net), ending);
        }
    }

    /// A peer's `cancel`: both sides clean up. Nothing is sent back.
    fn on_cancel(&mut self, message: &FileControl) {
        let outcome = match message.code.as_deref() {
            Some(code::SOURCE_GONE) => Outcome::Failed(words::source_gone()),
            Some(code::SOURCE_CHANGED) => Outcome::Failed(words::source_changed_once()),
            _ => Outcome::Cancelled,
        };
        self.drop_transfer(&message.transfer_id, &message.transfer_token, outcome);
    }

    /// A peer's `error`: the transfer ends like a cancellation. Nothing is
    /// sent back (answering an error with an error would never stop), except
    /// that `busy` is not an end at all: the sender waits and offers again.
    fn on_error(&mut self, message: &FileControl) {
        if message.code.as_deref() == Some(code::BUSY) {
            let retry = Instant::now() + self.tun.busy_retry;
            if let Some(outgoing) = self.outgoing.iter_mut().find(|outgoing| {
                outgoing.id == message.transfer_id && outgoing.token == message.transfer_token
            }) {
                outgoing.requeue(retry);
            }
            return;
        }
        self.drop_transfer(
            &message.transfer_id,
            &message.transfer_token,
            Outcome::Failed(words::peer_failure(message)),
        );
    }

    /// Removes a transfer the peer ended, in either direction, once its token
    /// is proven. Anything else changes nothing.
    fn drop_transfer(&mut self, id: &str, token: &str, outcome: Outcome) {
        if let Some(key) = id_bytes(id) {
            if self
                .incoming
                .get(&key)
                .is_some_and(|incoming| incoming.token == token)
            {
                if let Some(incoming) = self.incoming.remove(&key) {
                    incoming.discard(&self.store);
                }
                self.note_closed(key, "ended by the peer");
                board::finish(id, outcome);
                return;
            }
        }
        if let Some(index) = self
            .outgoing
            .iter()
            .position(|outgoing| outgoing.id == id && outgoing.token == token)
        {
            let outgoing = self.outgoing.remove(index);
            outgoing.discard(&self.store);
            board::finish(id, outcome);
        }
    }

    /// The user's (or the Bucket's) cancel.
    fn cancel(&mut self, id: &str, why: &'static str) -> Result<(), String> {
        let tell = |net: &Option<Arc<Net>>, token: &str| {
            if let Some(net) = net {
                let mut message = net.msg(FileAction::Cancel, id, token);
                message.code = Some(why.to_string());
                net.send(message);
            }
        };
        if let Some(key) = id_bytes(id) {
            if let Some(incoming) = self.incoming.remove(&key) {
                self.note_closed(key, "cancelled here");
                tell(&self.net, &incoming.token);
                incoming.discard(&self.store);
                board::finish(id, Outcome::Cancelled);
                return Ok(());
            }
        }
        if let Some(index) = self.outgoing.iter().position(|outgoing| outgoing.id == id) {
            let outgoing = self.outgoing.remove(index);
            // A send that never left the queue was never offered: no one to
            // tell.
            if outgoing.was_offered() {
                tell(&self.net, &outgoing.token);
            }
            outgoing.discard(&self.store);
            board::finish(id, Outcome::Cancelled);
            return Ok(());
        }
        Err("That file transfer is not running".to_string())
    }

    /// Progress to the Transfers page, at most a few times a second.
    fn report_progress(&mut self, now: Instant) {
        let every = self.tun.report_every;
        for incoming in self.incoming.values_mut() {
            incoming.report(now, every);
        }
        for outgoing in &mut self.outgoing {
            outgoing.report(now, every);
        }
    }

    /// No byte for a long while: said once, to the person and to the peer
    /// (informationally), and taken back when bytes move again. Never a
    /// deadline: a phone that sleeps for hours resumes where it stopped.
    fn watch_for_stalls(&mut self, now: Instant) {
        let limit = self.tun.no_progress;
        let net = self.net.clone();
        let tell = |net: &Option<Arc<Net>>,
                    change: &StallChange,
                    id: &str,
                    token: &str,
                    finishing: bool| {
            let Some(net) = net else { return };
            match change {
                StallChange::Began => {
                    let mut message = net.msg(FileAction::Pause, id, token);
                    message.reason = Some(reason::NO_PROGRESS.to_string());
                    net.send(message);
                }
                StallChange::Ended => {
                    net.send(net.msg(FileAction::Resume, id, token));
                }
                StallChange::None => return,
            }
            if let Some(note) = stall_note(change, finishing) {
                board::set_note(id, note.as_deref());
            }
        };
        for incoming in self.incoming.values_mut() {
            let change = incoming.stall.check(now, limit, incoming.is_paused());
            tell(&net, &change, &incoming.id, &incoming.token, false);
        }
        for outgoing in &mut self.outgoing {
            if !outgoing.is_active() {
                continue;
            }
            let change = outgoing
                .stall
                .check(now, limit, outgoing.is_waiting_on_peer());
            tell(
                &net,
                &change,
                &outgoing.id,
                &outgoing.token,
                outgoing.is_finishing(),
            );
        }
    }

    /// Final checkpoints, then the thread ends.
    fn shutdown(&mut self) {
        self.checkpoint_all();
    }

    fn checkpoint_all(&mut self) {
        if self.faults.crash {
            return;
        }
        for incoming in self.incoming.values_mut() {
            incoming.checkpoint_now(&self.store);
        }
    }

    fn announce(&self, completed: Completed) {
        if let Ok(hook) = self.shared.hook.lock() {
            if let Some(hook) = hook.as_ref() {
                hook(&completed);
            }
        }
    }
}

/// The note a stall puts on a transfer's row, if it changes it: `Some(note)`
/// to set (`None` clears). A send whose last block is acknowledged and that
/// waits for the receiver's `complete` keeps "Finishing…" until `complete` or
/// `error`: no stall note covers it, and a stall's end puts it back.
pub(super) fn stall_note(change: &StallChange, finishing: bool) -> Option<Option<String>> {
    match (change, finishing) {
        (StallChange::None, _) => None,
        (StallChange::Began, true) => None,
        (StallChange::Ended, true) => Some(Some(words::finishing())),
        (StallChange::Began, false) => Some(Some(words::no_progress())),
        (StallChange::Ended, false) => Some(None),
    }
}

/// What a person reads for a reason a transfer ended or waits.
pub(super) mod words {
    use super::{code, FileControl};

    /// Between the sender's `done` and the receiver's `complete`.
    pub fn finishing() -> String {
        "Finishing…".to_string()
    }

    pub fn source_gone() -> String {
        "The file no longer exists.".to_string()
    }

    pub fn source_changed_once() -> String {
        "The file changed while it was being sent.".to_string()
    }

    pub fn source_changed_for_good() -> String {
        "The file keeps changing, so it was not sent.".to_string()
    }

    pub fn damaged() -> String {
        "The file got damaged on the way and could not be fixed. Send it again.".to_string()
    }

    pub fn storage_full() -> String {
        "Storage is full. The transfer continues when there is space.".to_string()
    }

    pub fn waiting_for_a_slot() -> String {
        "Waiting for other transfers to finish".to_string()
    }

    pub fn no_progress() -> String {
        "Waiting for the other device to answer".to_string()
    }

    /// The reason the other device gave for ending a transfer, in words.
    pub fn peer_failure(message: &FileControl) -> String {
        match message.code.as_deref() {
            Some(code::HASH_MISMATCH) => damaged(),
            Some(code::SOURCE_GONE) => source_gone(),
            Some(code::SOURCE_CHANGED) => source_changed_for_good(),
            Some(code::TOO_LARGE) => "The file is too large to send.".to_string(),
            _ => super::store::peer_reason(message.message.as_deref()),
        }
    }
}

/// What to show when the peer paused a transfer of ours.
fn note_for_peer_pause(why: &str) -> Option<String> {
    match why {
        reason::STORAGE_FULL => Some("Waiting: the other device has no space".to_string()),
        reason::USER => Some("Paused on the other device".to_string()),
        reason::NO_PROGRESS => Some(words::no_progress()),
        _ => None,
    }
}

/// A frame that did not belong to anything is dropped, and said so (rarely).
fn note_dropped(kind: &str) {
    crate::device_links::limits::log_limited(
        "file-frame-dropped",
        &format!("Dropped a file frame ({kind})"),
    );
}

/// A row for the Transfers page.
fn row_for(
    wire: &WebRtcWireBinding,
    id: &str,
    name: &str,
    incoming: bool,
    total: u64,
    done: u64,
) -> TransferRow {
    TransferRow {
        id: id.to_string(),
        device_id: wire.peer_device_id.clone(),
        name: name.to_string(),
        incoming,
        done,
        total,
        preparing: false,
        paused: false,
    }
}
