//! The sending half: queue, offer, blocks, windows, restarts.

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::time::{Duration, Instant};

use openssl::sha::Sha256;

use super::super::file_protocol::{
    blocks_for, code, hash_of_hashes, hex, id_bytes, next_pass, payload_capacity, reason, unhex32,
    validate_filename, FileAction, FileControl, FolderRef, FrameHeader, ACTIVE_PER_DIRECTION,
    BLOCK_BYTES, BLOCK_TRAILER_BYTES, BUCKET_SLOTS_PER_DIRECTION, FRAME_BYTES_DEFAULT,
    FRAME_BYTES_PROVEN, FRAME_HEADER_BYTES, IN_FLIGHT_TARGET, MAX_RESTARTS, MAX_REWINDS,
    MAX_TRANSFER_BYTES, UNACKED_MAX,
};
use super::store::{modified_ms, SendCheckpoint, Store};
use super::{board, row_for, words, Completed, Ending, Engine, Net, Outcome, Stall};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SendState {
    /// Waiting for one of the two slots (or for a `busy` retry).
    Queued,
    /// The offer is out; the receiver has not answered.
    Offered,
    /// Re-reading the first blocks of the source to check the receiver's
    /// `prefixSha256` (a resume after a restart: no hashes are kept on disk).
    Verifying,
    Sending,
    /// `done` is out; waiting for the receiver's `complete`.
    AwaitComplete,
}

/// A prefix being re-hashed from the source.
struct Verify {
    upto: u64,
    expect: [u8; 32],
    hashes: Vec<[u8; 32]>,
    /// The pass to send under if the prefix is right, and if it is not (then
    /// from block 0).
    pass_ok: u8,
    pass_bad: u8,
}

/// Why a block could not be read.
pub(super) enum Trouble {
    Gone,
    Changed,
    Failed(String),
}

/// The sender's side of a fast lane (W18) for one transfer.
enum LaneTx {
    /// No lane: the data channel carries it.
    Off,
    /// The receiver's accept carried a token: dial once the transfer is
    /// sending from a settled block.
    Pending { port: u16, token: String },
    /// The connection is being made on a thread of its own (3 s at most).
    Dialing {
        result: std::sync::mpsc::Receiver<Result<LaneWriter, String>>,
        since: Instant,
    },
    /// Frames go over the lane.
    Up(LaneWriter),
}

/// A lane connection and the thread that writes to it. Frames queue here
/// (counted, so the file thread knows how much waits) and go out in order.
struct LaneWriter {
    frames: std::sync::mpsc::Sender<Vec<u8>>,
    queued: std::sync::Arc<std::sync::atomic::AtomicU64>,
    failed: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// To close the socket from here.
    socket: Option<std::net::TcpStream>,
}

impl LaneWriter {
    fn start(stream: openssl::ssl::SslStream<std::net::TcpStream>) -> Self {
        use std::io::Write;
        use std::sync::atomic::Ordering;
        let (frames, inbox) = std::sync::mpsc::channel::<Vec<u8>>();
        let queued = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let failed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let socket = stream.get_ref().try_clone().ok();
        let (counted, flag) = (queued.clone(), failed.clone());
        let spawned = std::thread::Builder::new()
            .name("desklink-lane-out".into())
            .spawn(move || {
                let mut stream = stream;
                while let Ok(frame) = inbox.recv() {
                    let length = frame.len() as u64;
                    if stream.write_all(&frame).is_err() {
                        flag.store(true, Ordering::Release);
                        break;
                    }
                    counted.fetch_sub(length, Ordering::AcqRel);
                }
                let _ = stream.flush();
            });
        if spawned.is_err() {
            failed.store(true, Ordering::Release);
        }
        Self {
            frames,
            queued,
            failed,
            socket,
        }
    }

    fn send(&self, frame: Vec<u8>) -> Result<(), String> {
        use std::sync::atomic::Ordering;
        #[cfg(test)]
        let frame = {
            let mut frame = frame;
            if super::tests::CORRUPT_NEXT_LANE_FRAME.swap(false, Ordering::SeqCst)
                || super::tests::CORRUPT_EVERY_LANE_FRAME.load(Ordering::SeqCst)
            {
                // A bit flipped in memory before the lane: TLS would not notice.
                let at = frame.len() / 2;
                frame[at] ^= 0x40;
            }
            frame
        };
        if self.failed.load(Ordering::Acquire) {
            return Err("the lane failed".to_string());
        }
        let length = frame.len() as u64;
        self.queued.fetch_add(length, Ordering::AcqRel);
        self.frames.send(frame).map_err(|_| {
            self.queued.fetch_sub(length, Ordering::AcqRel);
            "the lane is closed".to_string()
        })
    }

    fn buffered(&self) -> u64 {
        self.queued.load(std::sync::atomic::Ordering::Acquire)
    }

    fn has_failed(&self) -> bool {
        self.failed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Ends the connection now (the writer thread finds the socket dead).
    fn close(&self) {
        if let Some(socket) = &self.socket {
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for LaneWriter {
    fn drop(&mut self) {
        self.close();
    }
}

/// One outgoing file.
/// Where the time of a send went (always measured; one line per transfer is
/// logged when it ends): reading the source, hashing it, the call that hands a
/// frame to the channel, and waiting (for the receiver's window, or for the
/// channel's buffer to drain).
#[derive(Debug, Default)]
struct Stages {
    started: Option<Instant>,
    read: Duration,
    hash: Duration,
    send: Duration,
    window: Duration,
    buffer: Duration,
    /// Blocked since, and whether on the window (else the buffer).
    blocked: Option<(Instant, bool)>,
}

impl Stages {
    /// Always: the clock reads per frame cost microseconds, and the one line
    /// per transfer is what says whether a slow send was the disk, the hash,
    /// the window or the network.
    fn on() -> bool {
        true
    }

    /// The send was held up by the window (`true`) or the buffer.
    fn blocked(&mut self, now: Instant, window: bool) {
        if self.blocked.is_none() {
            self.blocked = Some((now, window));
        }
    }

    /// A frame went out: any wait ends.
    fn flowing(&mut self, now: Instant) {
        if let Some((since, window)) = self.blocked.take() {
            let waited = now.saturating_duration_since(since);
            if window {
                self.window += waited;
            } else {
                self.buffer += waited;
            }
        }
    }

    /// "1000000000 bytes in 23.30 s = 42.9 MB/s: read 800 ms, …".
    fn summary(&self, bytes: u64) -> String {
        let seconds = self.started.map_or(0.0, |at| at.elapsed().as_secs_f64());
        let ms = |duration: Duration| duration.as_millis();
        format!(
            "{bytes} bytes in {seconds:.2} s = {:.1} MB/s: read {} ms, hash {} ms, \
             send call {} ms, waiting for the window {} ms, waiting for the buffer {} ms",
            bytes as f64 / seconds.max(0.001) / 1e6,
            ms(self.read),
            ms(self.hash),
            ms(self.send),
            ms(self.window),
            ms(self.buffer),
        )
    }
}

pub(super) struct Outgoing {
    stages: Stages,
    pub id: String,
    pub token: String,
    source: std::path::PathBuf,
    filename: String,
    total: u64,
    blocks_total: u64,
    mtime: i64,
    history: bool,
    clipboard_mime: Option<String>,
    folder: Option<FolderRef>,
    content_sha256: Option<String>,
    restarts: u32,
    state: SendState,
    seq: u64,
    retry_at: Option<Instant>,
    offered: bool,
    /// The largest message to send: the smaller of both sides' maxima.
    frame_max: usize,
    file: Option<File>,
    pass: u8,
    rewinds: u32,
    /// Hashes of the blocks sent so far (a block's hash exists once its last
    /// frame is out).
    hashes: Vec<[u8; 32]>,
    hasher: Option<Sha256>,
    next_offset: u64,
    /// Blocks the receiver has acknowledged (cumulative).
    acked: u64,
    /// Why the receiver told us to hold still, if it did.
    peer_pause: Option<String>,
    verify: Option<Verify>,
    /// Tests: the next read of the source fails.
    #[cfg(test)]
    pub(super) fail_read: bool,
    pub stall: Stall,
    last_report: Instant,
    reported: u64,
    /// The offer said a lane would do (W18).
    lane_offered: bool,
    lane: LaneTx,
    /// A lane failed for this transfer: it is not tried again until the
    /// receiver's next accept (a resume) brings a token.
    lane_failed: Option<Instant>,
    /// Lanes that failed in a row (a delivered block clears it).
    lane_failures: u32,
    /// Blocks refused on the lane since the last one accepted.
    lane_nacks: u32,
    /// When the receiver last acknowledged a block (for the lane's stall).
    lane_ack_at: Instant,
}

/// [`Outgoing::sync_key`] of a kept send.
fn sync_key_of(kept: &SendCheckpoint) -> Option<(String, String, String, u64)> {
    let folder = kept.folder.as_ref()?;
    Some((
        folder.folder.clone(),
        folder.relpath.clone(),
        kept.content_sha256.as_deref()?.to_ascii_lowercase(),
        kept.total_size,
    ))
}

impl Outgoing {
    /// Looks at the source and records the send, without reading it.
    pub fn prepare(
        store: &Store,
        source: &Path,
        clipboard_mime: Option<&str>,
        folder: Option<FolderRef>,
        content_sha256: Option<String>,
        fixed_ids: Option<(&str, &str)>,
    ) -> Result<Self, String> {
        let filename = match &folder {
            // The last segment of the synced path.
            Some(folder) => folder.relpath.rsplit('/').next().unwrap_or_default(),
            None => source
                .file_name()
                .and_then(|value| value.to_str())
                .filter(|value| !value.is_empty())
                .ok_or_else(|| "DeskLink shared file has no safe filename".to_string())?,
        }
        .to_string();
        // The peer refuses an unsafe name only after the offer, so refuse it
        // here before anything else.
        validate_filename(&filename)?;
        let metadata = std::fs::metadata(source)
            .map_err(|error| format!("Could not open DeskLink shared file: {error}"))?;
        // A directory, FIFO or device would never yield a finite file.
        if !metadata.is_file() {
            return Err("DeskLink shared path is not a regular file".to_string());
        }
        if metadata.len() > MAX_TRANSFER_BYTES {
            return Err("DeskLink does not send files over 4 GiB".to_string());
        }
        let source = std::path::absolute(source)
            .map_err(|error| format!("Could not resolve DeskLink shared file: {error}"))?;
        let outgoing = Self::new(
            SendCheckpoint {
                transfer_id: fixed_ids.map_or_else(
                    || uuid::Uuid::new_v4().hyphenated().to_string(),
                    |(id, _)| id.to_string(),
                ),
                device_id: String::new(),
                token: fixed_ids.map_or_else(
                    || uuid::Uuid::new_v4().simple().to_string(),
                    |(_, token)| token.to_string(),
                ),
                filename,
                total_size: metadata.len(),
                mtime: modified_ms(&metadata),
                source,
                // A clipboard copy is a temporary file, a synced file the
                // Bucket's: neither is in the history.
                history: clipboard_mime.is_none() && folder.is_none(),
                clipboard_mime: clipboard_mime.map(str::to_string),
                folder,
                content_sha256,
                restarts: 0,
            },
            0,
        );
        store.save_send(&outgoing.record())?;
        Ok(outgoing)
    }

    fn new(checkpoint: SendCheckpoint, seq: u64) -> Self {
        let now = Instant::now();
        Self {
            stages: Stages::default(),
            id: checkpoint.transfer_id,
            token: checkpoint.token,
            source: checkpoint.source,
            filename: checkpoint.filename,
            total: checkpoint.total_size,
            blocks_total: blocks_for(checkpoint.total_size),
            mtime: checkpoint.mtime,
            history: checkpoint.history,
            clipboard_mime: checkpoint.clipboard_mime,
            folder: checkpoint.folder,
            content_sha256: checkpoint.content_sha256,
            restarts: checkpoint.restarts,
            state: SendState::Queued,
            seq,
            retry_at: None,
            offered: false,
            frame_max: FRAME_BYTES_DEFAULT,
            file: None,
            pass: 0,
            rewinds: 0,
            hashes: Vec::new(),
            hasher: None,
            next_offset: 0,
            acked: 0,
            peer_pause: None,
            verify: None,
            #[cfg(test)]
            fail_read: false,
            stall: Stall::new(now),
            last_report: now,
            reported: 0,
            lane_offered: false,
            lane: LaneTx::Off,
            lane_failed: None,
            lane_failures: 0,
            lane_nacks: 0,
            lane_ack_at: now,
        }
    }

    /// What a synced file is for resuming: its folder, path, content hash and
    /// size, whatever transfer id it travels under.
    fn sync_key(&self) -> Option<(String, String, String, u64)> {
        let folder = self.folder.as_ref()?;
        Some((
            folder.folder.clone(),
            folder.relpath.clone(),
            self.content_sha256.as_deref()?.to_ascii_lowercase(),
            self.total,
        ))
    }

    fn record(&self) -> SendCheckpoint {
        SendCheckpoint {
            transfer_id: self.id.clone(),
            device_id: String::new(),
            token: self.token.clone(),
            filename: self.filename.clone(),
            total_size: self.total,
            mtime: self.mtime,
            source: self.source.clone(),
            history: self.history,
            clipboard_mime: self.clipboard_mime.clone(),
            folder: self.folder.clone(),
            content_sha256: self.content_sha256.clone(),
            restarts: self.restarts,
        }
    }

    pub fn is_active(&self) -> bool {
        !matches!(self.state, SendState::Queued)
    }

    /// Every block is acknowledged and the receiver has not yet said the file
    /// is placed.
    pub fn is_finishing(&self) -> bool {
        self.state == SendState::AwaitComplete
    }

    pub fn was_offered(&self) -> bool {
        self.offered
    }

    /// Whether the thread should poll quickly for room to send.
    pub fn is_pumping(&self) -> bool {
        match self.state {
            SendState::Verifying => true,
            SendState::Sending => self.next_offset < self.total && !self.is_held(),
            _ => false,
        }
    }

    /// The receiver asked us to hold still.
    fn is_held(&self) -> bool {
        self.peer_pause
            .as_deref()
            .is_some_and(|why| why != reason::NO_PROGRESS)
    }

    /// For the stall watch: waiting on purpose, not silent.
    pub fn is_waiting_on_peer(&self) -> bool {
        self.is_held()
    }

    fn can_send(&self) -> bool {
        self.state == SendState::Sending
            && !matches!(self.lane, LaneTx::Pending { .. } | LaneTx::Dialing { .. })
            && !self.is_held()
            && self.next_offset < self.total
            && self
                .next_offset
                .saturating_sub((self.acked * BLOCK_BYTES).min(self.total))
                < UNACKED_MAX
    }

    pub fn requeue(&mut self, retry_at: Instant) {
        // The lane goes with the connection it was agreed over.
        if let LaneTx::Up(writer) = &self.lane {
            writer.close();
        }
        self.lane = LaneTx::Off;
        self.state = SendState::Queued;
        self.offered = false;
        self.retry_at = Some(retry_at);
        board::set_note(&self.id, Some(&words::waiting_for_a_slot()));
    }

    pub fn discard(&self, store: &Store) {
        if let Err(error) = store.remove_send(&self.id) {
            debug_log!("[DeskLink] Could not remove a send's record: {error}");
        }
    }

    fn done_bytes(&self) -> u64 {
        // Blocks acknowledged, plus what of the current block is out.
        let acked = (self.acked * BLOCK_BYTES).min(self.total);
        let in_block = self
            .next_offset
            .saturating_sub(self.next_offset / BLOCK_BYTES * BLOCK_BYTES);
        (acked + in_block).min(self.total)
    }

    pub fn report(&mut self, now: Instant, every: Duration) {
        let done = self.done_bytes().max(self.reported);
        if done != self.reported && now.saturating_duration_since(self.last_report) >= every {
            board::progress(&self.id, done);
            self.reported = done;
            self.last_report = now;
        }
    }

    /// The offer message.
    fn offer_message(&mut self, net: &Net) -> FileControl {
        let mut offer = net.msg(FileAction::Offer, &self.id, &self.token);
        offer.filename = Some(self.filename.clone());
        offer.total_size = Some(self.total);
        offer.mtime = Some(self.mtime);
        offer.block_size = Some(BLOCK_BYTES);
        offer.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
        if let Some(mime) = &self.clipboard_mime {
            offer.clipboard = Some(true);
            offer.mime_type = Some(mime.clone());
        }
        if let Some(folder) = &self.folder {
            offer.folder = Some(folder.folder.clone());
            offer.relpath = Some(folder.relpath.clone());
            offer.content_sha256 = self.content_sha256.clone();
        }
        // A big file between two devices on the same network asks for the
        // fast lane (W18); the receiver's accept says whether it takes one.
        self.lane_offered = false;
        if self.total >= super::super::file_lane::MIN_FILE_BYTES
            && self.clipboard_mime.is_none()
            && net.link.lane_target().is_some()
        {
            offer.lane = Some(super::super::file_protocol::LaneField::offer());
            self.lane_offered = true;
        }
        offer
    }

    fn lane_gave_up(&mut self) {
        self.lane_failed = Some(Instant::now());
        self.lane_failures = self.lane_failures.saturating_add(1);
    }

    /// A transfer whose lane failed asks for one again (a re-offer: the
    /// receiver answers with where it is and a fresh token), after `retry`,
    /// doubling with each failure in a row, while the LAN path still holds.
    fn maybe_promote(&mut self, now: Instant, net: &Net, retry: Duration) {
        let Some(failed) = self.lane_failed else {
            return;
        };
        let wait = retry * 2u32.pow(self.lane_failures.saturating_sub(1).min(5));
        if self.state != SendState::Sending
            || !matches!(self.lane, LaneTx::Off)
            || !self.lane_offered
            || now.saturating_duration_since(failed) < wait
            || net.link.lane_target().is_none()
        {
            return;
        }
        self.lane_failed = None;
        self.state = SendState::Offered;
        net.send(self.offer_message(net));
    }

    /// The lane carries the frames now.
    fn on_lane(&self) -> bool {
        matches!(self.lane, LaneTx::Up(_))
    }

    /// Back to the data channel from the last acknowledged block, under the
    /// next pass (the receiver adopts it, as it does any rewind of ours).
    fn leave_lane(&mut self, net: &Net, why: &str) {
        if let LaneTx::Up(writer) = &self.lane {
            writer.close();
        }
        self.lane = LaneTx::Off;
        self.lane_gave_up();
        // Not moving (queued again, offered, verifying): nothing to carry on
        // from; the next accept starts it afresh, and must not be forced into
        // Sending here.
        if !matches!(self.state, SendState::Sending | SendState::AwaitComplete) {
            return;
        }
        crate::device_links::limits::log_limited(
            "file-lane-ended",
            &format!("A file lane ended ({why}); carrying on over WebRTC"),
        );
        board::set_lane(&self.id, Some(words::over_webrtc()));
        let (acked, pass) = (self.acked, next_pass(self.pass));
        self.begin_at(acked, pass);
        self.tell_lane(net, "webrtc");
    }

    /// Tells the receiver which lane the frames come over from now on, and
    /// from where.
    fn tell_lane(&self, net: &Net, kind: &str) {
        let mut message = net.msg(FileAction::Lane, &self.id, &self.token);
        message.lane = Some(super::super::file_protocol::LaneField::switch(
            kind,
            self.next_offset / BLOCK_BYTES,
            self.pass,
        ));
        net.send(message);
    }

    /// Tests: ten silent seconds with data unacknowledged, looked at while
    /// `held` by the peer or not. Returns whether the lane is still up and
    /// whether the transfer is sending.
    #[cfg(test)]
    pub(super) fn silent_lane_check(&mut self, held: bool, net: &Net) -> (bool, bool) {
        self.peer_pause = held.then(|| reason::STORAGE_FULL.to_string());
        self.next_offset = (self.acked + 1) * BLOCK_BYTES;
        self.lane_ack_at = Instant::now() - Duration::from_secs(10);
        self.lane_step(Instant::now(), net);
        (self.on_lane(), self.state == SendState::Sending)
    }

    /// One look at the lane: dial when the accept brought a token and the
    /// transfer is sending from a settled block, take the result of the dial,
    /// give up on a lane that failed or stopped being acknowledged.
    fn lane_step(&mut self, now: Instant, net: &Net) {
        if let LaneTx::Pending { .. } = &self.lane {
            if self.state != SendState::Sending {
                return;
            }
            let LaneTx::Pending { port, token } = std::mem::replace(&mut self.lane, LaneTx::Off)
            else {
                return;
            };
            let Some(target) = net.link.lane_target() else {
                return;
            };
            let body = super::super::file_lane::TokenBody {
                transfer_id: self.id.clone(),
                token,
                pass: self.pass,
                start_block: self.next_offset / BLOCK_BYTES,
            };
            let (tx, result) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("desklink-lane-dial".into())
                .spawn(move || {
                    let dialed = super::super::file_lane::dial(
                        &target.config,
                        &target.local_id,
                        &target.peer_id,
                        std::net::SocketAddr::new(target.addr, port),
                        &body,
                    )
                    .map(LaneWriter::start);
                    let _ = tx.send(dialed);
                });
            if spawned.is_ok() {
                self.lane = LaneTx::Dialing { result, since: now };
            } else {
                self.lane_gave_up();
            }
            return;
        }
        match &self.lane {
            LaneTx::Dialing { result, since } => match result.try_recv() {
                Ok(Ok(writer)) => {
                    self.lane = LaneTx::Up(writer);
                    self.lane_ack_at = now;
                    board::set_lane(&self.id, Some(words::fast_lane()));
                    self.tell_lane(net, "tcp");
                }
                Ok(Err(why)) => {
                    debug_log!("[DeskLink] No file lane ({why}); WebRTC carries it");
                    self.lane = LaneTx::Off;
                    self.lane_gave_up();
                    board::set_lane(&self.id, Some(words::over_webrtc()));
                }
                Err(std::sync::mpsc::TryRecvError::Empty)
                    if now.saturating_duration_since(*since)
                        > super::super::file_lane::HANDSHAKE_LIMIT * 2 =>
                {
                    self.lane = LaneTx::Off;
                    self.lane_gave_up();
                    board::set_lane(&self.id, Some(words::over_webrtc()));
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(_) => {
                    self.lane = LaneTx::Off;
                    self.lane_gave_up();
                    board::set_lane(&self.id, Some(words::over_webrtc()));
                }
            },
            LaneTx::Up(writer) => {
                // Silence is judged only while the transfer is meant to be
                // moving: a receiver that paused (storage, backlog, the
                // person) is not a dead lane, and the clock starts afresh
                // when it carries on.
                let active = self.state == SendState::Sending && !self.is_held();
                if !active {
                    self.lane_ack_at = now;
                }
                let unacked = self.next_offset > self.acked * BLOCK_BYTES;
                let stalled = active
                    && unacked
                    && now.saturating_duration_since(self.lane_ack_at)
                        > super::super::file_lane::STALL_LIMIT;
                if writer.has_failed() || stalled {
                    let why = if stalled { "stalled" } else { "write failed" };
                    self.leave_lane(net, why);
                }
            }
            _ => {}
        }
    }

    /// The receiver told us to hold still (or that it waits).
    pub fn on_peer_pause(&mut self, why: &str) {
        if why != reason::NO_PROGRESS {
            self.peer_pause = Some(why.to_string());
        }
        board::set_note(&self.id, super::note_for_peer_pause(why).as_deref());
    }

    /// A `resume`. Without a restart point it only clears what was shown;
    /// with one (the receiver's) we go back to it.
    pub fn on_peer_resume(&mut self, message: &FileControl) -> Result<(), Ending> {
        self.peer_pause = None;
        board::set_note(&self.id, None);
        let (Some(blocks), Some(prefix), Some(pass)) = (
            message.resume_block,
            message.prefix_sha256.as_deref().and_then(unhex32),
            message.pass,
        ) else {
            return Ok(());
        };
        self.restart_from(blocks, prefix, pass, next_pass(pass))
    }

    /// Goes back to `blocks`, if the receiver's `prefix` matches ours; else
    /// to the start under `pass_bad`.
    fn restart_from(
        &mut self,
        blocks: u64,
        prefix: [u8; 32],
        pass_ok: u8,
        pass_bad: u8,
    ) -> Result<(), Ending> {
        if blocks > self.blocks_total {
            return Err(Ending::internal(
                "The other device asked for a block that is not there",
            ));
        }
        self.stall.moved(Instant::now());
        if blocks == 0 {
            self.begin_at(
                0,
                if prefix == hash_of_hashes(&[]) {
                    pass_ok
                } else {
                    pass_bad
                },
            );
            return Ok(());
        }
        if self.hashes.len() as u64 >= blocks {
            let right = hash_of_hashes(&self.hashes[..blocks as usize]) == prefix;
            self.begin_at(
                if right { blocks } else { 0 },
                if right { pass_ok } else { pass_bad },
            );
            return Ok(());
        }
        // No hashes for those blocks in memory: read them again from the
        // source (hashing only, nothing sent), a block per round.
        self.state = SendState::Verifying;
        self.verify = Some(Verify {
            upto: blocks,
            expect: prefix,
            hashes: Vec::new(),
            pass_ok,
            pass_bad,
        });
        Ok(())
    }

    /// Sends from block `blocks` on, under `pass`.
    fn begin_at(&mut self, blocks: u64, pass: u8) {
        self.hashes.truncate(blocks as usize);
        self.hasher = None;
        self.next_offset = (blocks * BLOCK_BYTES).min(self.total);
        self.acked = blocks;
        self.pass = pass;
        self.state = SendState::Sending;
        self.verify = None;
        board::set_note(&self.id, None);
    }

    /// Goes back to block `blocks` after a refused or mismatched block.
    fn rewind_to(&mut self, blocks: u64, pass: u8) {
        self.hashes.truncate(blocks as usize);
        self.hasher = None;
        self.next_offset = (blocks * BLOCK_BYTES).min(self.total);
        self.acked = self.acked.min(blocks);
        self.pass = pass;
        self.state = SendState::Sending;
    }

    fn stat_source(&self) -> Result<(), Trouble> {
        match std::fs::metadata(&self.source) {
            Ok(metadata) if metadata.is_file() => {
                if metadata.len() == self.total && modified_ms(&metadata) == self.mtime {
                    Ok(())
                } else {
                    Err(Trouble::Changed)
                }
            }
            Ok(_) => Err(Trouble::Gone),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Err(Trouble::Gone),
            Err(error) => Err(Trouble::Failed(format!(
                "Could not read DeskLink shared file: {error}"
            ))),
        }
    }

    fn source_file(&mut self) -> Result<&File, Trouble> {
        if self.file.is_none() {
            let file = File::open(&self.source).map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    Trouble::Gone
                } else {
                    Trouble::Failed(format!("Could not read DeskLink shared file: {error}"))
                }
            })?;
            self.file = Some(file);
        }
        self.file
            .as_ref()
            .ok_or_else(|| Trouble::Failed("The shared file is not open".to_string()))
    }

    /// Reads and frames the next piece of the current block.
    fn next_frame(&mut self) -> Result<Vec<u8>, Trouble> {
        #[cfg(test)]
        if self.fail_read {
            return Err(Trouble::Failed(
                "The shared file could not be read".to_string(),
            ));
        }
        if self.next_offset % BLOCK_BYTES == 0 {
            // Before each block: the source must still be the offered file.
            self.stat_source()?;
            self.hasher = Some(Sha256::new());
        }
        let block = self.next_offset / BLOCK_BYTES;
        let block_end = ((block + 1) * BLOCK_BYTES).min(self.total);
        let length = (payload_capacity(self.frame_max) as u64).min(block_end - self.next_offset);
        let last = self.next_offset + length == block_end;
        let header = FrameHeader {
            last_of_block: last,
            pass: self.pass,
            transfer: id_bytes(&self.id).unwrap_or_default(),
            block_index: block as u32,
            offset: self.next_offset,
            length: length as u32,
        };
        let mut frame = Vec::with_capacity(
            FRAME_HEADER_BYTES + length as usize + if last { BLOCK_TRAILER_BYTES } else { 0 },
        );
        header.write(&mut frame);
        frame.resize(FRAME_HEADER_BYTES + length as usize, 0);
        let offset = self.next_offset;
        let timed = Stages::on().then(Instant::now);
        let read = self
            .source_file()?
            .read_exact_at(&mut frame[FRAME_HEADER_BYTES..], offset);
        if let Some(at) = timed {
            self.stages.read += at.elapsed();
            self.stages.started.get_or_insert(at);
        }
        match read {
            Ok(()) => {}
            // Shorter than offered: it changed.
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(Trouble::Changed)
            }
            Err(error) => {
                return Err(Trouble::Failed(format!(
                    "Could not read DeskLink shared file: {error}"
                )))
            }
        }
        let timed = Stages::on().then(Instant::now);
        if let Some(hasher) = self.hasher.as_mut() {
            hasher.update(&frame[FRAME_HEADER_BYTES..]);
        }
        if let Some(at) = timed {
            self.stages.hash += at.elapsed();
        }
        if last {
            let hash = self.hasher.take().map(Sha256::finish).unwrap_or_default();
            self.hashes.push(hash);
            frame.extend_from_slice(&hash);
        }
        self.next_offset += length;
        Ok(frame)
    }

    /// The frame just built could not be sent: back to the start of its
    /// block, to be read again.
    fn rollback_block(&mut self) {
        let block = (self.next_offset.saturating_sub(1)) / BLOCK_BYTES;
        self.hashes.truncate(block as usize);
        self.hasher = None;
        self.next_offset = block * BLOCK_BYTES;
    }

    /// One block of a prefix check: the next of `verify.upto`.
    fn verify_step(&mut self) -> Result<(), Trouble> {
        let Some(verify) = self.verify.as_ref() else {
            return Ok(());
        };
        let index = verify.hashes.len() as u64;
        if index == 0 {
            self.stat_source()?;
        }
        let start = index * BLOCK_BYTES;
        let length = BLOCK_BYTES.min(self.total - start) as usize;
        let mut block = vec![0u8; length];
        match self.source_file()?.read_exact_at(&mut block, start) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {
                return Err(Trouble::Changed)
            }
            Err(error) => {
                return Err(Trouble::Failed(format!(
                    "Could not read DeskLink shared file: {error}"
                )))
            }
        }
        let hash = openssl::sha::sha256(&block);
        let Some(verify) = self.verify.as_mut() else {
            return Ok(());
        };
        verify.hashes.push(hash);
        if verify.hashes.len() as u64 == verify.upto {
            let right = hash_of_hashes(&verify.hashes) == verify.expect;
            let (pass_ok, pass_bad) = (verify.pass_ok, verify.pass_bad);
            let hashes = std::mem::take(&mut verify.hashes);
            if right {
                self.hashes = hashes;
                let blocks = self.hashes.len() as u64;
                self.begin_at(blocks, pass_ok);
            } else {
                self.begin_at(0, pass_bad);
            }
        }
        Ok(())
    }
}

impl Engine {
    /// A new send (or a resumed one): shown, queued, offered when a slot is
    /// free.
    pub(super) fn start_outgoing(&mut self, mut outgoing: Outgoing) {
        let Some(net) = self.net.clone() else {
            outgoing.discard(&self.store);
            return;
        };
        // A synced file is one transfer however often the link drops
        // ("Bucket resume", WEBRTC.md): asked for again, it is the send that
        // is live, or else the one a checkpoint keeps, offered again.
        if let Some(key) = outgoing.sync_key() {
            if self
                .outgoing
                .iter()
                .any(|live| live.id != outgoing.id && live.sync_key().as_ref() == Some(&key))
            {
                outgoing.discard(&self.store);
                return;
            }
            let kept: Vec<_> = self
                .store
                .stored_sends()
                .into_iter()
                .filter(|kept| {
                    kept.transfer_id != outgoing.id
                        && kept.device_id == net.wire.peer_device_id
                        && sync_key_of(kept).as_ref() == Some(&key)
                })
                .collect();
            if let Some(first) = kept.first() {
                // The new request's own record goes; the kept send carries on
                // under its id and token (any other with the key is dropped).
                outgoing.discard(&self.store);
                outgoing.id = first.transfer_id.clone();
                outgoing.token = first.token.clone();
                outgoing.restarts = first.restarts;
                for other in &kept[1..] {
                    let _ = self.store.remove_send(&other.transfer_id);
                }
            }
        }
        self.seq += 1;
        outgoing.seq = self.seq;
        // The record names its peer so only that device's sends are resumed.
        let mut record = outgoing.record();
        record.device_id = net.wire.peer_device_id.clone();
        if let Err(error) = self.store.save_send(&record) {
            debug_log!("[DeskLink] Could not record a send: {error}");
        }
        let row = row_for(
            &net.wire,
            &outgoing.id,
            &outgoing.filename,
            false,
            outgoing.total,
            0,
        );
        match &outgoing.folder {
            Some(folder) => board::begin_synced(super::TransferRow {
                name: folder.relpath.clone(),
                ..row
            }),
            None if outgoing.clipboard_mime.is_none() => {
                board::begin(row);
                board::set_retry_source(&outgoing.id, outgoing.source.clone());
            }
            None => {}
        }
        board::set_note(&outgoing.id, Some(&words::waiting_for_a_slot()));
        self.outgoing.push(outgoing);
    }

    /// Offers the queued sends the two slots allow, oldest first.
    pub(super) fn promote_queued(&mut self, now: Instant) {
        let Some(net) = self.net.clone() else {
            return;
        };
        let active = self.outgoing.iter().filter(|o| o.is_active()).count();
        let mut bucket_free = BUCKET_SLOTS_PER_DIRECTION.saturating_sub(
            self.outgoing
                .iter()
                .filter(|o| o.is_active() && o.folder.is_some())
                .count(),
        );
        let mut free = ACTIVE_PER_DIRECTION.saturating_sub(active);
        if free == 0 {
            return;
        }
        let mut order: Vec<usize> = (0..self.outgoing.len())
            .filter(|&i| {
                self.outgoing[i].state == SendState::Queued
                    && self.outgoing[i].retry_at.is_none_or(|at| at <= now)
            })
            .collect();
        // What the person chose goes first; the Bucket never holds the last
        // slot.
        order.sort_by_key(|&i| (self.outgoing[i].folder.is_some(), self.outgoing[i].seq));
        for index in order {
            if free == 0 {
                break;
            }
            let outgoing = &mut self.outgoing[index];
            if outgoing.folder.is_some() {
                if bucket_free == 0 {
                    continue;
                }
                bucket_free -= 1;
            }
            net.send(outgoing.offer_message(&net));
            outgoing.state = SendState::Offered;
            outgoing.offered = true;
            outgoing.retry_at = None;
            outgoing.stall.moved(now);
            board::set_note(&outgoing.id, None);
            free -= 1;
        }
    }

    /// The receiver accepted: sending starts where it already has the file.
    pub(super) fn on_accept(&mut self, net: &Net, message: &FileControl) {
        let Some(index) = self.find_out(&message.transfer_id, &message.transfer_token) else {
            return;
        };
        let outgoing = &mut self.outgoing[index];
        if outgoing.state != SendState::Offered {
            return;
        }
        let theirs = message.max_frame_bytes.unwrap_or(FRAME_BYTES_DEFAULT);
        outgoing.frame_max = FRAME_BYTES_PROVEN.min(theirs);
        let (Some(blocks), Some(prefix)) = (
            message.resume_block,
            message.prefix_sha256.as_deref().and_then(unhex32),
        ) else {
            return;
        };
        // It is always valid to start earlier than the receiver says, never
        // later: a prefix that is not ours restarts at the beginning under
        // the next pass.
        let pass = outgoing.pass;
        if let Err(ending) = outgoing.restart_from(blocks, prefix, pass, next_pass(pass)) {
            return self.end_outgoing(index, Some(net), ending);
        }
        // The accept brings a lane (W18) when the receiver takes one: nothing
        // is sent until it is up (3 s at most) or has failed.
        let outgoing = &mut self.outgoing[index];
        if let LaneTx::Up(_) = outgoing.lane {
            outgoing.lane = LaneTx::Off;
        }
        outgoing.lane_failed = None;
        match message.lane.as_ref() {
            Some(lane) if outgoing.lane_offered && lane.kind.is_none() => {
                if let (Some(port), Some(token)) = (lane.port, lane.token.clone()) {
                    if port != 0 && net.link.lane_target().is_some() {
                        outgoing.lane = LaneTx::Pending { port, token };
                    }
                }
            }
            _ => board::set_lane(
                &outgoing.id,
                words::data_channel(net.link.relayed(), outgoing.lane_offered),
            ),
        }
    }

    /// A block the receiver verified.
    pub(super) fn on_block_ack(&mut self, net: &Net, message: &FileControl) {
        let Some(index) = self.find_out(&message.transfer_id, &message.transfer_token) else {
            return;
        };
        let (Some(block), Some(theirs)) = (
            message.block_index,
            message.sha256.as_deref().and_then(unhex32),
        ) else {
            return;
        };
        let outgoing = &mut self.outgoing[index];
        let Some(ours) = outgoing.hashes.get(block as usize).copied() else {
            // An acknowledgement of a block not sent (yet): ignored.
            return;
        };
        outgoing.stall.moved(Instant::now());
        if ours != theirs {
            // The receiver verified a block that is not the one we read:
            // read and send it again.
            outgoing.rewinds += 1;
            if outgoing.rewinds > MAX_REWINDS {
                let ending = Ending {
                    code: code::HASH_MISMATCH,
                    words: words::damaged(),
                    tell_peer: true,
                };
                return self.end_outgoing(index, Some(net), ending);
            }
            let pass = next_pass(outgoing.pass);
            outgoing.rewind_to(block, pass);
            return;
        }
        outgoing.acked = outgoing.acked.max(block + 1);
        outgoing.lane_ack_at = Instant::now();
        outgoing.lane_nacks = 0;
        if outgoing.on_lane() {
            outgoing.lane_failures = 0;
        }
    }

    /// The receiver refused a block (it arrived damaged): sent again.
    pub(super) fn on_block_nack(&mut self, net: &Net, message: &FileControl) {
        let Some(index) = self.find_out(&message.transfer_id, &message.transfer_token) else {
            return;
        };
        let (Some(block), Some(pass)) = (message.block_index, message.pass) else {
            return;
        };
        let outgoing = &mut self.outgoing[index];
        if message.reason.as_deref() == Some(super::super::file_protocol::reason::LANE) {
            // The lane ended under the receiver: carry on over the data
            // channel from its last verified block; no damage, no rewind.
            if block > outgoing.hashes.len() as u64 {
                return;
            }
            outgoing.stall.moved(Instant::now());
            if outgoing.on_lane() {
                outgoing.leave_lane(net, "closed by the receiver");
            }
            return;
        }
        if outgoing.on_lane() && block <= outgoing.hashes.len() as u64 {
            // A block that failed its hash on the lane: sent once more on the
            // lane; failing again, the lane is not to be trusted and the data
            // channel takes over (never the three rewinds and an error).
            outgoing.lane_nacks += 1;
            outgoing.stall.moved(Instant::now());
            if outgoing.lane_nacks >= 2 {
                outgoing.lane_nacks = 0;
                outgoing.leave_lane(net, "a block failed its hash twice");
            } else {
                outgoing.rewind_to(block, pass);
            }
            return;
        }
        outgoing.rewinds += 1;
        if outgoing.rewinds > MAX_REWINDS || block > outgoing.hashes.len() as u64 {
            let ending = Ending {
                code: code::HASH_MISMATCH,
                words: words::damaged(),
                tell_peer: true,
            };
            return self.end_outgoing(index, Some(net), ending);
        }
        outgoing.stall.moved(Instant::now());
        outgoing.rewind_to(block, pass);
    }

    /// The receiver published the file: this send is over.
    pub(super) fn on_complete(&mut self, message: &FileControl) {
        let Some(index) = self.find_out(&message.transfer_id, &message.transfer_token) else {
            return;
        };
        let outgoing = &self.outgoing[index];
        let ours = hex(&hash_of_hashes(&outgoing.hashes));
        if outgoing.state != SendState::AwaitComplete
            || message.file_hash.as_deref().map(str::to_lowercase) != Some(ours)
        {
            return;
        }
        let outgoing = self.outgoing.remove(index);
        let id = outgoing.id.clone();
        // One line per transfer, but not one per small Bucket file: a sync of
        // many files would fill the journal. A Bucket file is told when it was
        // big or slow (the lines that explain a slow sync).
        let worth_a_line = outgoing.folder.is_none()
            || outgoing.total >= 8 << 20
            || outgoing
                .stages
                .started
                .is_some_and(|at| at.elapsed() >= Duration::from_secs(2));
        if worth_a_line {
            // No file name: this goes to the journal, which outlives the file.
            eprintln!(
                "[DeskLink] Sent transfer {}: {}",
                outgoing.id.chars().take(8).collect::<String>(),
                outgoing.stages.summary(outgoing.total)
            );
        }
        outgoing.discard(&self.store);
        board::finish(&id, Outcome::Sent);
        if outgoing.history {
            crate::device_links::transfer_log::record_sent(
                self.net
                    .as_ref()
                    .map_or("", |net| net.wire.peer_device_id.as_str()),
                &outgoing.filename,
                &outgoing.source,
                outgoing.total,
            );
        }
        self.announce(Completed {
            transfer_id: id,
            path: std::path::PathBuf::from(&outgoing.filename),
            capture_id: None,
            incoming: false,
        });
    }

    fn find_out(&self, id: &str, token: &str) -> Option<usize> {
        self.outgoing
            .iter()
            .position(|outgoing| outgoing.id == id && outgoing.token == token)
    }

    /// Re-reads the first blocks of a source whose receiver holds them.
    pub(super) fn verify_prefixes(&mut self) {
        let mut troubled = Vec::new();
        for (index, outgoing) in self.outgoing.iter_mut().enumerate() {
            if outgoing.state != SendState::Verifying {
                continue;
            }
            if let Err(trouble) = outgoing.verify_step() {
                troubled.push((index, trouble));
            }
        }
        for (index, trouble) in troubled.into_iter().rev() {
            self.source_trouble(index, trouble);
        }
    }

    /// Every block acknowledged: say so (`done`), once the source is checked
    /// one last time.
    pub(super) fn finish_acknowledged(&mut self) {
        let Some(net) = self.net.clone() else {
            return;
        };
        let mut troubled = Vec::new();
        for (index, outgoing) in self.outgoing.iter_mut().enumerate() {
            if outgoing.state != SendState::Sending
                || outgoing.acked < outgoing.blocks_total
                || outgoing.hashes.len() as u64 != outgoing.blocks_total
            {
                continue;
            }
            if let Err(trouble) = outgoing.stat_source() {
                troubled.push((index, trouble));
                continue;
            }
            let mut done = net.msg(FileAction::Done, &outgoing.id, &outgoing.token);
            done.file_hash = Some(hex(&hash_of_hashes(&outgoing.hashes)));
            done.blocks = Some(outgoing.blocks_total);
            done.total_size = Some(outgoing.total);
            net.send(done);
            outgoing.state = SendState::AwaitComplete;
            outgoing.stall.moved(Instant::now());
            // Every block is acknowledged but the file is not placed yet: it
            // is not "sent" until the receiver says it is (`complete`).
            board::set_note(&outgoing.id, Some(&words::finishing()));
        }
        for (index, trouble) in troubled.into_iter().rev() {
            self.source_trouble(index, trouble);
        }
    }

    /// Frames, while the channel has room and the receiver is not too far
    /// behind. A round is bounded so acknowledgements are never starved.
    pub(super) fn pump_sends(&mut self) {
        let Some(net) = self.net.clone() else {
            return;
        };
        let now = Instant::now();
        for outgoing in &mut self.outgoing {
            outgoing.lane_step(now, &net);
            outgoing.maybe_promote(now, &net, self.tun.lane_retry);
        }
        if self.link_down_until.is_some_and(|until| until > now) {
            return;
        }
        let deadline = now + Duration::from_millis(15);
        let mut troubled = Vec::new();
        'rounds: loop {
            let mut sent = false;
            for (index, outgoing) in self.outgoing.iter_mut().enumerate() {
                // One that met trouble this call is dealt with after it.
                if troubled.iter().any(|(troubled, _)| *troubled == index) {
                    continue;
                }
                for _ in 0..16 {
                    let window_open = outgoing.can_send();
                    let queued = match &outgoing.lane {
                        LaneTx::Up(writer) => writer.buffered(),
                        _ => net.link.buffered(),
                    };
                    if !window_open || queued >= IN_FLIGHT_TARGET {
                        if Stages::on() && outgoing.state == SendState::Sending {
                            outgoing.stages.blocked(now, !window_open);
                        }
                        break;
                    }
                    match outgoing.next_frame() {
                        Ok(frame) => {
                            let timed = Stages::on().then(Instant::now);
                            let sent_frame = match &outgoing.lane {
                                LaneTx::Up(writer) => writer.send(frame),
                                _ => net.link.send_frame(frame),
                            };
                            if let Some(at) = timed {
                                outgoing.stages.send += at.elapsed();
                                outgoing.stages.flowing(at);
                            }
                            if sent_frame.is_err() && outgoing.on_lane() {
                                // The lane broke under the write: not the
                                // peer's link. Carry on over the data channel.
                                outgoing.leave_lane(&net, "write failed");
                                break;
                            }
                            if let Err(error) = sent_frame {
                                outgoing.rollback_block();
                                crate::device_links::limits::log_limited(
                                    "file-frame-send",
                                    &format!("A file frame was not sent: {error}"),
                                );
                                self.link_down_until = Some(now + Duration::from_millis(250));
                                break 'rounds;
                            }
                            outgoing.stall.moved(now);
                            sent = true;
                        }
                        Err(trouble) => {
                            troubled.push((index, trouble));
                            break;
                        }
                    }
                }
            }
            if !sent || Instant::now() >= deadline {
                break;
            }
        }
        for (index, trouble) in troubled.into_iter().rev() {
            self.source_trouble(index, trouble);
        }
    }

    /// The source is gone or changed (or unreadable): the receiver is told,
    /// both sides clean up, and a changed ordinary file begins again under a
    /// new id, up to twice.
    fn source_trouble(&mut self, index: usize, trouble: Trouble) {
        let net = self.net.clone();
        let outgoing = self.outgoing.remove(index);
        let id = outgoing.id.clone();
        let tell = |why: &str, text: &str| {
            if let (Some(net), true) = (&net, outgoing.offered) {
                let mut cancel = net.msg(FileAction::Cancel, &outgoing.id, &outgoing.token);
                cancel.code = Some(why.to_string());
                cancel.message = Some(text.to_string());
                net.send(cancel);
            }
        };
        match trouble {
            Trouble::Gone => {
                tell(code::SOURCE_GONE, &words::source_gone());
                outgoing.discard(&self.store);
                board::finish(&id, Outcome::Failed(words::source_gone()));
            }
            Trouble::Failed(text) => {
                // A failure, not a deliberate cancel: `error`, so the
                // receiver (which rejects an unknown cancel code) frees its slot.
                if let (Some(net), true) = (&net, outgoing.offered) {
                    let mut error = net.msg(FileAction::Error, &outgoing.id, &outgoing.token);
                    error.code = Some(code::INTERNAL.to_string());
                    error.message = Some(text.chars().take(256).collect());
                    net.send(error);
                }
                outgoing.discard(&self.store);
                board::finish(&id, Outcome::Failed(text));
            }
            Trouble::Changed => {
                tell(code::SOURCE_CHANGED, &words::source_changed_once());
                let again = outgoing.folder.is_none()
                    && outgoing.clipboard_mime.is_none()
                    && outgoing.restarts < MAX_RESTARTS;
                let restarted = again
                    .then(|| self.restart_changed(&outgoing))
                    .and_then(Result::ok);
                outgoing.discard(&self.store);
                match restarted {
                    // The same row carries on under the new id.
                    Some(()) => board::discard(&id),
                    None => {
                        let words = if outgoing.restarts >= MAX_RESTARTS {
                            words::source_changed_for_good()
                        } else {
                            words::source_changed_once()
                        };
                        board::finish(&id, Outcome::Failed(words));
                    }
                }
            }
        }
    }

    /// Begins `old` again, from its source as it is now, as a new transfer.
    fn restart_changed(&mut self, old: &Outgoing) -> Result<(), String> {
        let metadata = std::fs::metadata(&old.source)
            .map_err(|error| format!("Could not open DeskLink shared file: {error}"))?;
        if !metadata.is_file() || metadata.len() > MAX_TRANSFER_BYTES {
            return Err("The file can no longer be sent".to_string());
        }
        let mut record = old.record();
        record.transfer_id = uuid::Uuid::new_v4().hyphenated().to_string();
        record.token = uuid::Uuid::new_v4().simple().to_string();
        record.total_size = metadata.len();
        record.mtime = modified_ms(&metadata);
        record.restarts = old.restarts + 1;
        let fresh = Outgoing::new(record, 0);
        self.start_outgoing(fresh);
        Ok(())
    }

    /// Ends a send badly: the receiver is told (when it was ever told of
    /// it), the record goes and the person is shown why.
    pub(super) fn end_outgoing(&mut self, index: usize, net: Option<&Net>, ending: Ending) {
        let outgoing = self.outgoing.remove(index);
        debug_log!(
            "[DeskLink] Ending outgoing transfer {}: {}",
            outgoing.id,
            ending.words
        );
        if let (Some(net), true, true) = (net, ending.tell_peer, outgoing.offered) {
            let mut error = net.msg(FileAction::Error, &outgoing.id, &outgoing.token);
            error.code = Some(ending.code.to_string());
            error.message = Some(ending.words.chars().take(256).collect());
            net.send(error);
        }
        let id = outgoing.id.clone();
        outgoing.discard(&self.store);
        board::finish(&id, Outcome::Failed(ending.words));
    }

    /// Offers again what a lost connection interrupted, under the same ids,
    /// so the receiver answers with where it stopped. Once per connection;
    /// a send whose source is gone or changed is dropped, not retried for
    /// ever.
    pub(super) fn resume_sends(&mut self) {
        if std::mem::replace(&mut self.resumed, true) {
            return;
        }
        let Some(net) = self.net.clone() else {
            return;
        };
        for stored in self.store.stored_sends() {
            if stored.device_id != net.wire.peer_device_id
                || self.outgoing.iter().any(|o| o.id == stored.transfer_id)
            {
                continue;
            }
            let intact = match std::fs::metadata(&stored.source) {
                Ok(metadata) => {
                    metadata.is_file()
                        && metadata.len() == stored.total_size
                        && modified_ms(&metadata) == stored.mtime
                }
                Err(_) => false,
            };
            if !intact {
                let _ = self.store.remove_send(&stored.transfer_id);
                if stored.history {
                    let words = if stored.source.exists() {
                        words::source_changed_once()
                    } else {
                        words::source_gone()
                    };
                    board::record(board::Finished {
                        id: stored.transfer_id.clone(),
                        device_id: net.wire.peer_device_id.clone(),
                        name: stored.filename.clone(),
                        incoming: false,
                        outcome: Outcome::Failed(words),
                        retry: stored.source.exists().then(|| stored.source.clone()),
                    });
                }
                continue;
            }
            self.start_outgoing(Outgoing::new(stored, 0));
        }
    }
}
