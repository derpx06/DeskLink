//! Transfers v2, end to end through two managers and a scriptable link.
//!
//! Two managers (`a` sends, `b` receives, each with its own state and
//! downloads directories) are joined by two [`Pipe`]s: a pipe delivers what one
//! side sends to the other's queue, in order, and can hold, drop, repeat or
//! corrupt what passes. Everything real except the network: the file thread, the
//! files, the hashes, the checkpoints.
//!
//! Layout: this file holds the rig every topic shares (`Pipe`, `Rig`, the
//! hand-driven `Hand`, payloads, waits and small helpers); the tests
//! are in one file per topic (`sizes`, `wire`, `damage`, `frames`, `cancel`,
//! `source_changes`, `stalls`, `reconnect`, `names`, `slots`, `wire_fixes`,
//! `placing`, `phone_offer`, `bucket_resume`, `file_thread`, `lane`, `stale`).

use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use super::super::file_protocol::{
    blocks_for, code, hash_of_hashes, hex, id_bytes, parse_frame, FileAction, FileControl,
    FrameHeader, BLOCK_BYTES, FRAME_BYTES_PROVEN, IN_FLIGHT_TARGET, UNACKED_MAX,
};
use super::super::transfer_board as board;
use super::{words, Completed, FileLink, Tunables, WebRtcTransferManager};
use crate::device_links::webrtc::WebRtcWireBinding;

mod bucket_resume;
mod cancel;
mod damage;
mod file_thread;
mod frames;
mod lane;
mod names;
mod phone_offer;
mod placing;
mod reconnect;
mod sizes;
mod slots;
mod source_changes;
mod stale;
mod stalls;
mod wire;
mod wire_fixes;

type Manager = WebRtcTransferManager;

/// A test sets this to flip one bit in the next frame a lane writes.
pub(super) static CORRUPT_NEXT_LANE_FRAME: AtomicBool = AtomicBool::new(false);
/// ...and this one, in every frame a lane writes.
pub(super) static CORRUPT_EVERY_LANE_FRAME: AtomicBool = AtomicBool::new(false);

fn wire_of(me: &str, peer: &str) -> WebRtcWireBinding {
    WebRtcWireBinding {
        sender_device_id: me.to_string(),
        peer_device_id: peer.to_string(),
        session_id: 7,
        generation: 1,
    }
}

/// Short timers: the real ones are seconds and minutes. Except the stall
/// note: a loaded test machine stalls a transfer for 400 ms easily, and the
/// informational pause, resume and note it adds broke the counts and notes
/// of tests about something else. The stall test shortens it ([`stalls`]).
fn fast() -> Tunables {
    Tunables {
        checkpoint_blocks: 2,
        checkpoint_after: Duration::from_millis(40),
        no_progress: Duration::from_secs(3600),
        storage_retry: Duration::from_millis(100),
        busy_retry: Duration::from_millis(100),
        report_every: Duration::from_millis(5),
        storage_slack: 0,
        ..Tunables::default()
    }
}

/// [`fast`] with a short stall time, for the tests of the stall note.
fn stalls() -> Tunables {
    Tunables {
        no_progress: Duration::from_millis(400),
        ..fast()
    }
}

/// Bytes that do not repeat on any block boundary.
fn payload(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..length)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn eventually<T>(what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(40);
    loop {
        if let Some(found) = probe() {
            return found;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn steady(what: &str, wait: Duration, mut holds: impl FnMut() -> bool) {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        assert!(holds(), "{what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// What a pipe does to a frame or a message passing through it.
enum Fate {
    Deliver,
    Drop,
    Twice,
}

type FrameHook = Box<dyn FnMut(&mut Vec<u8>) -> Fate + Send>;
type ControlHook = Box<dyn FnMut(&mut FileControl) -> Fate + Send>;

enum Held {
    Control(Box<FileControl>),
    Frame(Vec<u8>),
}

/// One direction of a link: what one manager sends is queued for the other.
struct Pipe {
    to: Mutex<Weak<Manager>>,
    up: AtomicBool,
    hold: AtomicBool,
    held: Mutex<VecDeque<Held>>,
    buffered: AtomicU64,
    frames: AtomicUsize,
    frame_bytes: AtomicU64,
    controls: Mutex<Vec<FileControl>>,
    frame_hook: Mutex<Option<FrameHook>>,
    control_hook: Mutex<Option<ControlHook>>,
    /// Where a fast lane could go from this side (W18); `None`: no lane.
    lane: Mutex<Option<super::super::file_lane::LaneTarget>>,
    /// How often the engine asked this link whether it is relayed.
    relayed_asked: AtomicUsize,
}

impl Pipe {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            to: Mutex::new(Weak::new()),
            up: AtomicBool::new(true),
            hold: AtomicBool::new(false),
            held: Mutex::new(VecDeque::new()),
            buffered: AtomicU64::new(0),
            frames: AtomicUsize::new(0),
            frame_bytes: AtomicU64::new(0),
            controls: Mutex::new(Vec::new()),
            frame_hook: Mutex::new(None),
            control_hook: Mutex::new(None),
            lane: Mutex::new(None),
            relayed_asked: AtomicUsize::new(0),
        })
    }

    fn connect(&self, to: &Arc<Manager>) {
        *self.to.lock().unwrap() = Arc::downgrade(to);
    }

    fn target(&self) -> Option<Arc<Manager>> {
        self.to.lock().unwrap().upgrade()
    }

    fn deliver(&self, item: Held) {
        let Some(to) = self.target() else { return };
        match item {
            Held::Control(control) => to.on_control(*control),
            Held::Frame(frame) => to.on_frame(frame),
        }
    }

    /// Stops delivering (a phone that dozes): what is sent waits, in order.
    fn freeze(&self) {
        self.hold.store(true, Ordering::SeqCst);
    }

    /// Delivers what waited, in order.
    fn thaw(&self) {
        let mut held = self.held.lock().unwrap();
        self.hold.store(false, Ordering::SeqCst);
        while let Some(item) = held.pop_front() {
            self.deliver(item);
        }
    }

    fn log(&self) -> Vec<FileControl> {
        self.controls.lock().unwrap().clone()
    }

    fn sent(&self, action: FileAction) -> Vec<FileControl> {
        self.log()
            .into_iter()
            .filter(|control| control.action == action)
            .collect()
    }
}

impl FileLink for Pipe {
    fn send_control(&self, control: &FileControl) -> Result<(), String> {
        if !self.up.load(Ordering::SeqCst) {
            return Err("link down".to_string());
        }
        let mut control = control.clone();
        let fate = self
            .control_hook
            .lock()
            .unwrap()
            .as_mut()
            .map_or(Fate::Deliver, |hook| hook(&mut control));
        self.controls.lock().unwrap().push(control.clone());
        let mut held = self.held.lock().unwrap();
        let copies = match fate {
            Fate::Drop => 0,
            Fate::Deliver => 1,
            Fate::Twice => 2,
        };
        for _ in 0..copies {
            if self.hold.load(Ordering::SeqCst) {
                held.push_back(Held::Control(Box::new(control.clone())));
            } else {
                self.deliver(Held::Control(Box::new(control.clone())));
            }
        }
        Ok(())
    }

    fn send_frame(&self, mut frame: Vec<u8>) -> Result<(), String> {
        if !self.up.load(Ordering::SeqCst) {
            return Err("link down".to_string());
        }
        let fate = self
            .frame_hook
            .lock()
            .unwrap()
            .as_mut()
            .map_or(Fate::Deliver, |hook| hook(&mut frame));
        self.frames.fetch_add(1, Ordering::SeqCst);
        self.frame_bytes
            .fetch_add(frame.len() as u64, Ordering::SeqCst);
        let mut held = self.held.lock().unwrap();
        let copies = match fate {
            Fate::Drop => 0,
            Fate::Deliver => 1,
            Fate::Twice => 2,
        };
        for _ in 0..copies {
            if self.hold.load(Ordering::SeqCst) {
                held.push_back(Held::Frame(frame.clone()));
            } else {
                self.deliver(Held::Frame(frame.clone()));
            }
        }
        Ok(())
    }

    fn buffered(&self) -> u64 {
        self.buffered.load(Ordering::SeqCst)
    }

    fn lane_target(&self) -> Option<super::super::file_lane::LaneTarget> {
        self.lane.lock().unwrap().clone()
    }

    fn relayed(&self) -> bool {
        self.relayed_asked.fetch_add(1, Ordering::SeqCst);
        false
    }
}

/// Two managers and the link between them. `a` is "alpha", `b` is "beta".
struct Rig {
    base: PathBuf,
    a: Option<Arc<Manager>>,
    b: Option<Arc<Manager>>,
    /// What `a` sends (to `b`), and what `b` sends.
    ab: Arc<Pipe>,
    ba: Arc<Pipe>,
    done_a: Receiver<Completed>,
    done_b: Receiver<Completed>,
    tun_a: Tunables,
    tun_b: Tunables,
    queue_b: usize,
    /// The two device ids on the wire.
    ids: (String, String),
    /// Lane targets handed to the pipes at every connect (W18).
    lanes: Option<(
        super::super::file_lane::LaneTarget,
        super::super::file_lane::LaneTarget,
    )>,
}

impl Rig {
    fn new(label: &str) -> Self {
        Self::with(label, fast(), fast(), super::FILE_QUEUE_MAX_BYTES)
    }

    fn with(label: &str, tun_a: Tunables, tun_b: Tunables, queue_b: usize) -> Self {
        Self::build(
            label,
            tun_a,
            tun_b,
            queue_b,
            ("alpha".into(), "beta".into()),
            None,
        )
    }

    fn build(
        label: &str,
        tun_a: Tunables,
        tun_b: Tunables,
        queue_b: usize,
        ids: (String, String),
        lanes: Option<(
            super::super::file_lane::LaneTarget,
            super::super::file_lane::LaneTarget,
        )>,
    ) -> Self {
        let base = std::env::temp_dir().join(format!(
            "desklink-v2-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(base.join("src")).unwrap();
        let (_, done_a) = channel();
        let (_, done_b) = channel();
        let mut rig = Self {
            base,
            a: None,
            b: None,
            ab: Pipe::new(),
            ba: Pipe::new(),
            done_a,
            done_b,
            tun_a,
            tun_b,
            queue_b,
            ids,
            lanes,
        };
        rig.connect();
        rig
    }

    /// (Re)creates both managers on the same directories and a fresh link:
    /// what a reconnect, or a restart of the app, looks like.
    fn connect(&mut self) {
        let a = Arc::new(
            Manager::with(
                self.base.join("a-state"),
                self.base.join("a-dl"),
                self.tun_a.clone(),
                super::FILE_QUEUE_MAX_BYTES,
            )
            .unwrap(),
        );
        let b = Arc::new(
            Manager::with(
                self.base.join("b-state"),
                self.base.join("b-dl"),
                self.tun_b.clone(),
                self.queue_b,
            )
            .unwrap(),
        );
        self.ab = Pipe::new();
        self.ba = Pipe::new();
        self.ab.connect(&b);
        self.ba.connect(&a);
        let (tx_a, done_a) = channel();
        let (tx_b, done_b) = channel();
        let (tx_a, tx_b) = (Mutex::new(tx_a), Mutex::new(tx_b));
        a.set_completion_hook(move |completed| {
            let _ = tx_a.lock().unwrap().send(completed.clone());
        });
        b.set_completion_hook(move |completed| {
            let _ = tx_b.lock().unwrap().send(completed.clone());
        });
        self.done_a = done_a;
        self.done_b = done_b;
        if let Some((from_a, from_b)) = &self.lanes {
            *self.ab.lane.lock().unwrap() = Some(from_a.clone());
            *self.ba.lane.lock().unwrap() = Some(from_b.clone());
        }
        a.attach(wire_of(&self.ids.0, &self.ids.1), self.ab.clone());
        b.attach(wire_of(&self.ids.1, &self.ids.0), self.ba.clone());
        self.a = Some(a);
        self.b = Some(b);
    }

    fn a(&self) -> &Arc<Manager> {
        self.a.as_ref().unwrap()
    }

    fn b(&self) -> &Arc<Manager> {
        self.b.as_ref().unwrap()
    }

    /// Ends both file threads (final checkpoints written) before a
    /// reconnect. `crash`: the receiver dies without them.
    fn stop(&mut self, crash_receiver: bool) {
        if let Some(a) = self.a.take() {
            if let Some(a) = Arc::into_inner(a) {
                a.stop();
            }
        }
        if let Some(b) = self.b.take() {
            if crash_receiver {
                b.probe(|engine| engine.faults.crash = true);
            }
            if let Some(b) = Arc::into_inner(b) {
                b.stop();
            }
        }
    }

    fn source(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let path = self.base.join("src").join(name);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn state(&self, side: &str) -> PathBuf {
        self.base.join(format!("{side}-state"))
    }

    fn downloads(&self, side: &str) -> PathBuf {
        self.base.join(format!("{side}-dl"))
    }

    /// Waits for the receiver to publish a file; its path.
    fn received(&self) -> Completed {
        self.done_b
            .recv_timeout(Duration::from_secs(40))
            .expect("the file never arrived")
    }

    fn sent_ok(&self) -> Completed {
        self.done_a
            .recv_timeout(Duration::from_secs(40))
            .expect("the sender never heard the file was placed")
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop(false);
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

fn failed_with(id: &str) -> Option<String> {
    board::peek_finished(id)
        .into_iter()
        .find_map(|finished| match finished.outcome {
            board::Outcome::Failed(reason) => Some(reason),
            _ => None,
        })
}

/// How the transfer `id` ended on each side that recorded it (both sides do,
/// in these tests, under the same id).
fn outcomes(id: &str) -> Vec<board::Outcome> {
    board::peek_finished(id)
        .into_iter()
        .map(|finished| finished.outcome)
        .collect()
}

fn was_sent(id: &str) -> bool {
    outcomes(id)
        .iter()
        .any(|outcome| matches!(outcome, board::Outcome::Sent))
}

fn was_cancelled(id: &str) -> bool {
    outcomes(id)
        .iter()
        .any(|outcome| matches!(outcome, board::Outcome::Cancelled))
}
// ── Frames that should change nothing ──────────────────────────────────────

/// A receiver driven by hand: what a hostile or confused sender could do.
struct Hand {
    manager: Arc<Manager>,
    link: Arc<Pipe>,
    base: PathBuf,
}

impl Hand {
    fn new(label: &str) -> Self {
        Self::with_peer(label, "alpha")
    }

    fn with_peer(label: &str, peer: &str) -> Self {
        let base = std::env::temp_dir().join(format!(
            "desklink-v2-hand-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        let manager = Arc::new(
            Manager::with(
                base.join("state"),
                base.join("dl"),
                fast(),
                super::FILE_QUEUE_MAX_BYTES,
            )
            .unwrap(),
        );
        let link = Pipe::new();
        manager.attach(wire_of("beta", peer), link.clone());
        Self {
            manager,
            link,
            base,
        }
    }

    fn offer(&self, id: &str, name: &str, total: u64) -> FileControl {
        let mut offer =
            FileControl::new(&wire_of("alpha", "beta"), FileAction::Offer, id, "token0");
        offer.filename = Some(name.to_string());
        offer.total_size = Some(total);
        offer.mtime = Some(1_700_000_000_000);
        offer.block_size = Some(BLOCK_BYTES);
        offer.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
        offer
    }

    fn send(&self, control: FileControl) {
        self.manager.on_control(control);
    }

    fn reply(&self, action: FileAction) -> Option<FileControl> {
        self.link.sent(action).into_iter().next()
    }

    fn settle(&self) {
        // A round trip through the file thread.
        self.manager.probe(|_| ());
    }

    fn incoming(&self) -> Vec<String> {
        self.manager.probe(|engine| engine.incoming_ids())
    }
}

impl Drop for Hand {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

fn frame_of(id: &str, pass: u8, offset: u64, payload: &[u8], total: u64) -> Vec<u8> {
    let block = (offset / BLOCK_BYTES) as u32;
    let block_end = ((u64::from(block) + 1) * BLOCK_BYTES).min(total);
    let last = offset + payload.len() as u64 == block_end;
    let header = FrameHeader {
        last_of_block: last,
        pass,
        transfer: id_bytes(id).unwrap(),
        block_index: block,
        offset,
        length: payload.len() as u32,
    };
    let mut out = Vec::new();
    header.write(&mut out);
    out.extend_from_slice(payload);
    if last {
        // The hash of the block this payload ends (callers send whole blocks).
        out.extend_from_slice(&openssl::sha::sha256(payload));
    }
    out
}

/// The frames of block `index` of the file `file`, each at most one frame
/// long, the last with the hash (flipped when `damaged`).
fn block_frames(id: &str, pass: u8, file: &[u8], index: u64, damaged: bool) -> Vec<Vec<u8>> {
    let start = (index * BLOCK_BYTES) as usize;
    let end = (start + BLOCK_BYTES as usize).min(file.len());
    let block = &file[start..end];
    let hash = openssl::sha::sha256(block);
    let capacity = super::super::file_protocol::payload_capacity(FRAME_BYTES_PROVEN);
    let mut frames = Vec::new();
    let mut at = 0;
    while at < block.len() {
        let length = capacity.min(block.len() - at);
        let last = at + length == block.len();
        let header = FrameHeader {
            last_of_block: last,
            pass,
            transfer: id_bytes(id).unwrap(),
            block_index: index as u32,
            offset: (start + at) as u64,
            length: length as u32,
        };
        let mut frame = Vec::new();
        header.write(&mut frame);
        frame.extend_from_slice(&block[at..at + length]);
        if last {
            let mut trailer = hash;
            if damaged {
                trailer[0] ^= 1;
            }
            frame.extend_from_slice(&trailer);
        }
        frames.push(frame);
        at += length;
    }
    frames
}

thread_local! {
    static TEST_IDS: [&'static str; 4] = std::array::from_fn(|_| {
        &*Box::leak(uuid::Uuid::new_v4().hyphenated().to_string().into_boxed_str())
    });
}

/// Transfer ids of this test only (the board is shared by all of them).
fn id(n: usize) -> &'static str {
    TEST_IDS.with(|ids| ids[n - 1])
}

/// Blocks 0.. delivered then the link dies: a pulled cable.
fn pull_after_blocks(rig: &Rig, blocks: usize) {
    let (ab, ba) = (rig.ab.clone(), rig.ba.clone());
    let mut finished = 0usize;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        let parsed = parse_frame(frame).unwrap();
        if finished >= blocks {
            return Fate::Drop;
        }
        if parsed.header.last_of_block {
            finished += 1;
            if finished >= blocks {
                ab.up.store(false, Ordering::SeqCst);
                ba.up.store(false, Ordering::SeqCst);
            }
        }
        Fate::Deliver
    }));
}

fn scratch(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "desklink-{label}-{}",
        uuid::Uuid::new_v4().simple()
    ))
}

/// A three-block file, with the hashes of its blocks.
fn three_blocks(seed: u64) -> (Vec<u8>, Vec<[u8; 32]>) {
    let data = payload(2 * BLOCK_BYTES as usize + 777, seed);
    let hashes = data
        .chunks(BLOCK_BYTES as usize)
        .map(openssl::sha::sha256)
        .collect();
    (data, hashes)
}

/// Offers, sends every block of `data` and says `done`, as a sender would;
/// the transfer is `id`, its token `token0`.
fn send_whole(hand: &Hand, id: &str, name: &str, data: &[u8], hashes: &[[u8; 32]]) {
    hand.send(hand.offer(id, name, data.len() as u64));
    eventually("the accept", || {
        (!hand.link.sent(FileAction::Accept).is_empty()).then_some(())
    });
    for index in 0..hashes.len() as u64 {
        for frame in block_frames(id, 0, data, index, false) {
            hand.manager.on_frame(frame);
        }
    }
    say_done(hand, id, data.len() as u64, hashes);
}

fn say_done(hand: &Hand, id: &str, total: u64, hashes: &[[u8; 32]]) {
    let mut done = FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id, "token0");
    done.file_hash = Some(hex(&hash_of_hashes(hashes)));
    done.blocks = Some(hashes.len() as u64);
    done.total_size = Some(total);
    hand.send(done);
}
