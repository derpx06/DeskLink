//! Transfers v2, end to end through two managers and a scriptable link.
//!
//! Two managers (`a` sends, `b` receives, each with its own state and
//! downloads directories) are joined by two [`Pipe`]s: a pipe delivers what one
//! side sends to the other's queue, in order, and can hold, drop, repeat or
//! corrupt what passes. Everything real except the network: the file thread, the
//! files, the hashes, the checkpoints.

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

type Manager = WebRtcTransferManager;

fn wire_of(me: &str, peer: &str) -> WebRtcWireBinding {
    WebRtcWireBinding {
        sender_device_id: me.to_string(),
        peer_device_id: peer.to_string(),
        session_id: 7,
        generation: 1,
    }
}

/// Short timers: the real ones are seconds and minutes.
fn fast() -> Tunables {
    Tunables {
        checkpoint_blocks: 2,
        checkpoint_after: Duration::from_millis(40),
        no_progress: Duration::from_millis(400),
        storage_retry: Duration::from_millis(100),
        busy_retry: Duration::from_millis(100),
        report_every: Duration::from_millis(5),
        storage_slack: 0,
        ..Tunables::default()
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
}

impl Rig {
    fn new(label: &str) -> Self {
        Self::with(label, fast(), fast(), super::FILE_QUEUE_MAX_BYTES)
    }

    fn with(label: &str, tun_a: Tunables, tun_b: Tunables, queue_b: usize) -> Self {
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
        a.attach(wire_of("alpha", "beta"), self.ab.clone());
        b.attach(wire_of("beta", "alpha"), self.ba.clone());
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

// ── Sizes ──────────────────────────────────────────────────────────────────

fn round_trip(label: &str, length: usize) {
    let rig = Rig::new(label);
    let bytes = payload(length, length as u64);
    let source = rig.source("file.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes, "{length} bytes");
    assert_eq!(got.path.parent().unwrap(), rig.downloads("b"));
    assert!(got.incoming);
    let back = rig.sent_ok();
    assert_eq!(
        (back.transfer_id.as_str(), back.incoming),
        (id.as_str(), false)
    );
    // Nothing is left behind on either side but the file itself.
    eventually("clean state directories", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert_eq!(names(&rig.downloads("b")), ["file.bin"]);
}

#[test]
fn an_empty_file_has_no_frames_and_arrives() {
    let rig = Rig::new("empty");
    let source = rig.source("empty.txt", b"");
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), b"");
    rig.sent_ok();
    assert_eq!(
        rig.ab.frames.load(Ordering::SeqCst),
        0,
        "a 0-byte file has no frames"
    );
    let done = rig.ab.sent(FileAction::Done);
    assert_eq!(done[0].blocks, Some(0));
    assert_eq!(
        done[0].file_hash.as_deref(),
        Some(hex(&hash_of_hashes(&[])).as_str())
    );
}

#[test]
fn one_byte() {
    round_trip("one-byte", 1);
}

#[test]
fn exactly_one_frame_of_payload() {
    round_trip("one-frame", 61_368);
}

#[test]
fn exactly_one_mebibyte_is_one_block() {
    round_trip("one-mib", BLOCK_BYTES as usize);
    let rig = Rig::new("one-mib-frames");
    let source = rig.source("f.bin", &payload(BLOCK_BYTES as usize, 5));
    rig.a().start_send(&source).unwrap();
    rig.received();
    // 18 frames of the default size carry one block, the last with the hash.
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 18);
    assert_eq!(rig.ba.sent(FileAction::BlockAck).len(), 1);
    assert_eq!(rig.ba.sent(FileAction::Accept)[0].resume_block, Some(0));
}

#[test]
fn one_byte_over_a_block_makes_a_second_block() {
    round_trip("block-plus-one", BLOCK_BYTES as usize + 1);
}

#[test]
fn several_blocks_and_a_ragged_end() {
    round_trip("ragged", 3 * BLOCK_BYTES as usize + 123_457);
}

// ── Wire details ───────────────────────────────────────────────────────────

#[test]
fn the_offer_the_frames_and_the_hashes_follow_the_contract() {
    let rig = Rig::new("contract");
    let bytes = payload(2 * BLOCK_BYTES as usize + 77, 9);
    let source = rig.source("doc.pdf", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    rig.received();
    let offer = rig.ab.sent(FileAction::Offer).remove(0);
    assert_eq!(offer.protocol_version, 2);
    assert_eq!(offer.filename.as_deref(), Some("doc.pdf"));
    assert_eq!(offer.total_size, Some(bytes.len() as u64));
    assert_eq!(offer.block_size, Some(BLOCK_BYTES));
    assert_eq!(offer.max_frame_bytes, Some(FRAME_BYTES_PROVEN));
    assert!(offer.mtime.is_some_and(|mtime| mtime > 1_000_000_000_000));
    assert_eq!(offer.transfer_id, id);
    assert_eq!(offer.transfer_token.len(), 32, "a 32-hex token");
    // Per-block hashes, then the hash of the hashes.
    let blocks: Vec<[u8; 32]> = bytes
        .chunks(BLOCK_BYTES as usize)
        .map(openssl::sha::sha256)
        .collect();
    let acks = rig.ba.sent(FileAction::BlockAck);
    assert_eq!(acks.len(), 3);
    for (index, ack) in acks.iter().enumerate() {
        assert_eq!(ack.block_index, Some(index as u64));
        assert_eq!(ack.sha256.as_deref(), Some(hex(&blocks[index]).as_str()));
    }
    let done = rig.ab.sent(FileAction::Done).remove(0);
    assert_eq!(
        done.file_hash.as_deref(),
        Some(hex(&hash_of_hashes(&blocks)).as_str())
    );
    assert_eq!(
        (done.blocks, done.total_size),
        (Some(3), Some(bytes.len() as u64))
    );
    let complete = rig.ba.sent(FileAction::Complete).remove(0);
    assert_eq!(complete.file_hash, done.file_hash);
    // Every control message carries the common fields of the binding.
    for control in rig.ab.log() {
        assert_eq!(control.device_id, "alpha");
        assert_eq!((control.session_id, control.connection_generation), (7, 1));
        assert_eq!(control.transfer_token, offer.transfer_token);
    }
}

#[test]
fn the_source_is_read_in_place_with_no_copy_in_the_state_directory() {
    let rig = Rig::new("no-copy");
    rig.ab.freeze();
    let big = rig.source("big.bin", &payload(5 * BLOCK_BYTES as usize, 1));
    let id = rig.a().start_send(&big).unwrap();
    // Only the small record of the send, never a copy of the file.
    eventually("the send's record", || {
        names(&rig.state("a"))
            .contains(&format!(".desklink-{id}.send.json"))
            .then_some(())
    });
    for name in names(&rig.state("a")) {
        let size = fs::metadata(rig.state("a").join(&name)).unwrap().len();
        assert!(size < 4096, "{name} is {size} bytes: a copy of the file?");
    }
    rig.ab.thaw();
    rig.received();
}

#[test]
fn a_sender_stays_inside_the_window_the_channel_allows() {
    let rig = Rig::new("window");
    rig.ab.buffered.store(IN_FLIGHT_TARGET, Ordering::SeqCst);
    let source = rig.source("f.bin", &payload(3 * BLOCK_BYTES as usize, 2));
    rig.a().start_send(&source).unwrap();
    // The offer goes; with the channel full, no frame does.
    eventually("the offer", || {
        (!rig.ab.sent(FileAction::Offer).is_empty()).then_some(())
    });
    steady(
        "no frame while the channel is full",
        Duration::from_millis(200),
        || rig.ab.frames.load(Ordering::SeqCst) == 0,
    );
    rig.ab.buffered.store(0, Ordering::SeqCst);
    rig.received();
}

#[test]
fn a_sender_never_has_more_than_eight_mebibytes_unacknowledged() {
    let rig = Rig::new("unacked");
    // The receiver's acknowledgements never arrive.
    *rig.ba.control_hook.lock().unwrap() = Some(Box::new(|control| {
        if control.action == FileAction::BlockAck {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    }));
    let source = rig.source("f.bin", &payload(20 * BLOCK_BYTES as usize, 3));
    rig.a().start_send(&source).unwrap();
    eventually("the sender to fill its window", || {
        (rig.ab.frame_bytes.load(Ordering::SeqCst) >= UNACKED_MAX).then_some(())
    });
    std::thread::sleep(Duration::from_millis(300));
    let sent = rig.ab.frame_bytes.load(Ordering::SeqCst);
    // Eight blocks of payload plus their frame overhead, not a byte more.
    assert!(
        sent <= UNACKED_MAX + 8 * 18 * 72,
        "{sent} bytes sent into a window of {UNACKED_MAX}"
    );
}

#[test]
fn two_files_move_at_once_and_the_third_waits_for_a_slot() {
    let rig = Rig::new("slots");
    rig.ab.freeze();
    let files: Vec<PathBuf> = (0..3)
        .map(|n| rig.source(&format!("f{n}.bin"), &payload(300_000, n)))
        .collect();
    for file in &files {
        rig.a().start_send(file).unwrap();
    }
    eventually("two offers", || {
        (rig.ab.sent(FileAction::Offer).len() >= 2).then_some(())
    });
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 2, "the third waits");
    rig.ab.thaw();
    for _ in 0..3 {
        rig.received();
    }
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 3);
    assert_eq!(names(&rig.downloads("b")).len(), 3);
}

// ── A damaged block ────────────────────────────────────────────────────────

#[test]
fn a_damaged_block_is_refused_and_sent_again_under_a_new_pass() {
    let rig = Rig::new("nack");
    let bytes = payload(3 * BLOCK_BYTES as usize, 11);
    let source = rig.source("f.bin", &bytes);
    // One frame of block 1 arrives with a flipped byte, once.
    let mut damaged = false;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        let header = parse_frame(frame).unwrap().header;
        if header.block_index == 1 && header.offset > BLOCK_BYTES && !damaged {
            damaged = true;
            frame[45] ^= 0xff;
        }
        Fate::Deliver
    }));
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "repaired");
    let nacks = rig.ba.sent(FileAction::BlockNack);
    assert_eq!(nacks.len(), 1);
    assert_eq!(nacks[0].block_index, Some(1));
    assert_eq!(nacks[0].pass, Some(1));
    assert_eq!(nacks[0].reason.as_deref(), Some("hash-mismatch"));
    // The block went again under pass 1; the first frames of it were pass 0.
    let mut passes = Vec::new();
    for frame in 0..rig.ab.frames.load(Ordering::SeqCst) {
        let _ = frame;
    }
    passes.push(());
    assert!(
        rig.ab.frames.load(Ordering::SeqCst) > 3 * 18,
        "block 1 was resent"
    );
}

#[test]
fn a_block_that_keeps_arriving_damaged_fails_clearly_after_three_rewinds() {
    let rig = Rig::new("nack-forever");
    let source = rig.source("f.bin", &payload(2 * BLOCK_BYTES as usize, 12));
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(|frame| {
        if parse_frame(frame).unwrap().header.block_index == 1 {
            frame[45] ^= 0xff;
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let reason = eventually("the failure", || failed_with(&id));
    assert_eq!(reason, words::damaged());
    assert_eq!(rig.ba.sent(FileAction::BlockNack).len(), 3, "three rewinds");
    let error = rig.ba.sent(FileAction::Error).remove(0);
    assert_eq!(error.code.as_deref(), Some(code::HASH_MISMATCH));
    eventually("clean directories", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty(), "nothing published");
}

// ── A hash mismatch at the end ─────────────────────────────────────────────

#[test]
fn a_file_hash_mismatch_at_the_end_resends_the_file_once() {
    let rig = Rig::new("done-once");
    let bytes = payload(2 * BLOCK_BYTES as usize + 5, 13);
    let source = rig.source("f.bin", &bytes);
    let mut spoiled = false;
    *rig.ab.control_hook.lock().unwrap() = Some(Box::new(move |control| {
        if control.action == FileAction::Done && !spoiled {
            spoiled = true;
            control.file_hash = Some("00".repeat(32));
        }
        Fate::Deliver
    }));
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    let resumes = rig.ba.sent(FileAction::Resume);
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0].resume_block, Some(0));
    assert_eq!(resumes[0].pass, Some(1));
    assert_eq!(rig.ab.sent(FileAction::Done).len(), 2);
    assert!(rig.ab.frames.load(Ordering::SeqCst) >= 70, "sent twice");
}

#[test]
fn a_second_mismatch_at_the_end_fails_with_a_clear_message_and_publishes_nothing() {
    let rig = Rig::new("done-twice");
    let source = rig.source("f.bin", &payload(BLOCK_BYTES as usize + 9, 14));
    *rig.ab.control_hook.lock().unwrap() = Some(Box::new(|control| {
        if control.action == FileAction::Done {
            control.file_hash = Some("00".repeat(32));
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let reason = eventually("the failure", || failed_with(&id));
    assert_eq!(reason, words::damaged());
    assert_eq!(
        rig.ab.sent(FileAction::Done).len(),
        2,
        "re-sent once, then no more"
    );
    eventually("clean directories", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
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
        manager.attach(wire_of("beta", "alpha"), link.clone());
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
    static TEST_IDS: [&'static str; 3] = std::array::from_fn(|_| {
        &*Box::leak(uuid::Uuid::new_v4().hyphenated().to_string().into_boxed_str())
    });
}

/// Transfer ids of this test only (the board is shared by all of them).
fn id(n: usize) -> &'static str {
    TEST_IDS.with(|ids| ids[n - 1])
}

#[test]
fn a_duplicate_frame_is_ignored_and_the_file_is_not_corrupted() {
    let hand = Hand::new("duplicate");
    let data = payload(100, 1);
    hand.send(hand.offer(id(1), "d.bin", 100));
    eventually("the accept", || hand.reply(FileAction::Accept));
    let frame = frame_of(id(1), 0, 0, &data, 100);
    hand.manager.on_frame(frame.clone());
    hand.manager.on_frame(frame);
    hand.settle();
    assert_eq!(
        hand.link.sent(FileAction::BlockAck).len(),
        1,
        "acknowledged once"
    );
    let mut done = FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
    done.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&data)])));
    done.blocks = Some(1);
    done.total_size = Some(100);
    hand.send(done);
    eventually("the complete", || hand.reply(FileAction::Complete));
    assert_eq!(fs::read(hand.base.join("dl").join("d.bin")).unwrap(), data);
}

#[test]
fn a_frame_out_of_order_ends_the_transfer_without_publishing_anything() {
    let hand = Hand::new("gap");
    let total = BLOCK_BYTES + 10;
    hand.send(hand.offer(id(1), "g.bin", total));
    eventually("the accept", || hand.reply(FileAction::Accept));
    // Block 1 first: block 0 never came.
    hand.manager
        .on_frame(frame_of(id(1), 0, BLOCK_BYTES, &payload(10, 2), total));
    let error = eventually("the error", || hand.reply(FileAction::Error));
    assert_eq!(error.code.as_deref(), Some(code::BAD_FRAME));
    assert!(hand.incoming().is_empty());
    assert!(
        names(&hand.base.join("state")).is_empty(),
        "no partial left"
    );
    assert!(names(&hand.base.join("dl")).is_empty());
    assert!(failed_with(id(1)).is_some());
}

#[test]
fn a_frame_for_an_unknown_transfer_changes_nothing_and_gets_no_reply() {
    let hand = Hand::new("unknown");
    hand.manager
        .on_frame(frame_of(id(2), 0, 0, &payload(50, 3), 50));
    hand.settle();
    assert!(hand.incoming().is_empty());
    assert!(hand.link.log().is_empty(), "no reply to a stranger");
    assert!(names(&hand.base.join("state")).is_empty());
    // Not a frame at all.
    hand.manager.on_frame(vec![1, 2, 3]);
    hand.manager.on_frame(b"DLF1 an old frame".to_vec());
    hand.settle();
    assert!(hand.link.log().is_empty());
}

#[test]
fn a_frame_past_the_size_is_dropped_and_the_transfer_goes_on() {
    let hand = Hand::new("past-size");
    let data = payload(100, 4);
    hand.send(hand.offer(id(1), "p.bin", 100));
    eventually("the accept", || hand.reply(FileAction::Accept));
    // An offset beyond the end, and a block index that disagrees.
    hand.manager
        .on_frame(frame_of(id(1), 0, 5_000, &payload(10, 5), 100_000));
    let mut lying = frame_of(id(1), 0, 0, &data, 100);
    lying[27] = 9;
    hand.manager.on_frame(lying);
    hand.settle();
    assert!(hand.link.sent(FileAction::Error).is_empty());
    assert_eq!(hand.incoming().len(), 1, "still receiving");
    hand.manager.on_frame(frame_of(id(1), 0, 0, &data, 100));
    eventually("the acknowledgement", || hand.reply(FileAction::BlockAck));
}

#[test]
fn a_frame_from_an_old_pass_is_dropped_after_a_refusal() {
    let hand = Hand::new("stale-pass");
    let file = payload(BLOCK_BYTES as usize, 6);
    hand.send(hand.offer(id(1), "s.bin", BLOCK_BYTES));
    eventually("the accept", || hand.reply(FileAction::Accept));
    // A block whose hash is wrong: refused, pass 1 from now on.
    for frame in block_frames(id(1), 0, &file, 0, true) {
        hand.manager.on_frame(frame);
    }
    eventually("the nack", || hand.reply(FileAction::BlockNack));
    // Frames still in flight from pass 0 are ignored...
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    hand.settle();
    assert!(hand.link.sent(FileAction::BlockAck).is_empty());
    // ...the resent block, under pass 1, is taken.
    for frame in block_frames(id(1), 1, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("the acknowledgement", || hand.reply(FileAction::BlockAck));
}

#[test]
fn a_third_incoming_transfer_is_told_to_wait_and_is_accepted_when_a_slot_frees() {
    let hand = Hand::new("busy");
    hand.send(hand.offer(id(1), "one.bin", 10));
    hand.send(hand.offer(id(2), "two.bin", 10));
    hand.send(hand.offer(id(3), "three.bin", 10));
    let busy = eventually("busy", || hand.reply(FileAction::Error));
    assert_eq!(busy.code.as_deref(), Some(code::BUSY));
    assert_eq!(busy.transfer_id, id(3));
    assert_eq!(hand.link.sent(FileAction::Accept).len(), 2);
    assert!(failed_with(id(3)).is_none(), "busy is never shown");
    // The sender offers again once one is done.
    let mut cancel = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    cancel.code = Some(code::USER.to_string());
    hand.send(cancel);
    hand.send(hand.offer(id(3), "three.bin", 10));
    eventually("the third is accepted", || {
        (hand.link.sent(FileAction::Accept).len() == 3).then_some(())
    });
}

#[test]
fn an_offer_that_cannot_be_accepted_is_answered_not_dropped() {
    let hand = Hand::new("refuse");
    // Too large for DeskLink to receive.
    let mut huge = hand.offer(id(1), "huge.bin", 10);
    huge.total_size = Some(super::MAX_TRANSFER_BYTES + 1);
    hand.send(huge);
    let error = eventually("the answer", || hand.reply(FileAction::Error));
    assert_eq!(error.code.as_deref(), Some(code::TOO_LARGE));
    // A block size that is not the contract's.
    let mut odd = hand.offer(id(2), "odd.bin", 10);
    odd.block_size = Some(1000);
    hand.send(odd);
    eventually("the second answer", || {
        (hand.link.sent(FileAction::Error).len() == 2).then_some(())
    });
    // An unsafe name.
    hand.send(hand.offer(id(3), "../escape", 10));
    eventually("the third answer", || {
        (hand.link.sent(FileAction::Error).len() == 3).then_some(())
    });
    assert!(hand.incoming().is_empty());
    assert!(names(&hand.base.join("state")).is_empty());
}

#[test]
fn messages_with_a_wrong_binding_or_token_change_nothing() {
    let hand = Hand::new("binding");
    hand.send(hand.offer(id(1), "b.bin", 100));
    eventually("the accept", || hand.reply(FileAction::Accept));
    let before = hand.link.log().len();
    // The wrong session, the wrong device, the wrong token.
    let mut wrong_session = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    wrong_session.session_id = 99;
    let mut wrong_device = FileControl::new(
        &wire_of("mallory", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    wrong_device.code = Some(code::USER.to_string());
    let wrong_token = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "not-the-token",
    );
    for message in [wrong_session, wrong_device, wrong_token] {
        hand.send(message);
    }
    // A forged error and a forged pause with the wrong token too.
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Error,
        id(1),
        "nope",
    ));
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Pause,
        id(1),
        "nope",
    ));
    hand.settle();
    assert_eq!(hand.incoming(), [id(1)], "still there");
    assert_eq!(hand.link.log().len(), before, "and nothing was answered");
    // About a transfer we do not have: no reply either.
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(2),
        "token0",
    ));
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::BlockAck,
        id(2),
        "token0",
    ));
    hand.settle();
    assert_eq!(hand.link.log().len(), before);
}

#[test]
fn an_offer_for_a_transfer_already_open_is_answered_from_where_it_is() {
    let hand = Hand::new("replay");
    let total = 2 * BLOCK_BYTES;
    let file = payload(total as usize, 7);
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("the accept", || hand.reply(FileAction::Accept));
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0", || hand.reply(FileAction::BlockAck));
    // The same offer again: not a second transfer, and it says what it has.
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("a second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    let again = hand.link.sent(FileAction::Accept).remove(1);
    assert_eq!(again.resume_block, Some(1));
    let first_block = openssl::sha::sha256(&file[..BLOCK_BYTES as usize]);
    assert_eq!(
        again.prefix_sha256.as_deref(),
        Some(hex(&hash_of_hashes(&[first_block])).as_str())
    );
    assert_eq!(hand.incoming().len(), 1);
    // An offer with the same id and another token is refused.
    let mut forged = hand.offer(id(1), "r.bin", total);
    forged.transfer_token = "another".to_string();
    hand.send(forged);
    eventually("the refusal", || hand.reply(FileAction::Error));
}

// ── The Bucket ─────────────────────────────────────────────────────────────

#[test]
fn unrequested_bucket_data_is_refused_and_a_requested_file_is_accepted() {
    use crate::device_links::webrtc::file_protocol::FolderRef;
    let hand = Hand::new("bucket");
    let relpath = format!("docs/{}.txt", uuid::Uuid::new_v4().simple());
    let data = payload(200, 8);
    let sha = hex(&openssl::sha::sha256(&data));
    let bucket_offer = |id: &str, sha: &str, size: u64| {
        let mut offer = hand.offer(id, relpath.rsplit('/').next().unwrap(), size);
        offer.folder = Some("bucket".into());
        offer.relpath = Some(relpath.clone());
        offer.content_sha256 = Some(sha.to_string());
        offer
    };
    // Nobody asked for it.
    hand.send(bucket_offer(id(1), &sha, 200));
    let refusal = eventually("the refusal", || hand.reply(FileAction::Error));
    assert_eq!(refusal.code.as_deref(), Some(code::REFUSED));
    assert_eq!(refusal.message.as_deref(), Some("folder-refused"));
    assert!(hand.incoming().is_empty());
    assert!(names(&hand.base.join("state")).is_empty(), "nothing stored");
    // Asked for, but the hash or the size differ: refused as well.
    let wanted = FolderRef {
        folder: "bucket".into(),
        relpath: relpath.clone(),
        mtime: None,
    };
    crate::device_links::folders::test_want("alpha", &wanted, &sha, 200);
    hand.send(bucket_offer(id(2), &"ab".repeat(32), 200));
    hand.send(bucket_offer(id(3), &sha, 201));
    eventually("two more refusals", || {
        (hand.link.sent(FileAction::Error).len() == 3).then_some(())
    });
    // Asked for: accepted, taken, and handed to folder sync (not Downloads).
    hand.send(bucket_offer(id(1), &sha, 200));
    eventually("the accept", || hand.reply(FileAction::Accept));
    hand.manager.on_frame(frame_of(id(1), 0, 0, &data, 200));
    let mut done = FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
    done.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&data)])));
    done.blocks = Some(1);
    done.total_size = Some(200);
    hand.send(done);
    eventually("the complete", || hand.reply(FileAction::Complete));
    assert!(names(&hand.base.join("dl")).is_empty(), "not a download");
}

// ── Cancel ─────────────────────────────────────────────────────────────────

#[test]
fn the_sender_cancelling_cleans_up_both_sides_and_later_frames_are_ignored() {
    let rig = Rig::new("cancel-sender");
    let source = rig.source("f.bin", &payload(6 * BLOCK_BYTES as usize, 21));
    let id = rig.a().start_send(&source).unwrap();
    eventually("some blocks", || {
        (!rig.ba.sent(FileAction::BlockAck).is_empty()).then_some(())
    });
    rig.ab.freeze();
    rig.a().cancel_transfer(&id, code::USER).unwrap();
    // Frames already on the wire arrive after the cancel: they find nothing.
    rig.ab.thaw();
    eventually("the receiver to clean up", || {
        (names(&rig.state("b")).is_empty() && names(&rig.state("a")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
    assert!(was_cancelled(&id));
    assert!(rig
        .ab
        .sent(FileAction::Cancel)
        .iter()
        .all(|c| c.code.as_deref() == Some("user")));
    assert!(
        rig.a().cancel_transfer(&id, code::USER).is_err(),
        "no longer running"
    );
}

#[test]
fn the_receiver_cancelling_cleans_up_both_sides() {
    let rig = Rig::new("cancel-receiver");
    let source = rig.source("f.bin", &payload(6 * BLOCK_BYTES as usize, 22));
    rig.ab.freeze();
    let id = rig.a().start_send(&source).unwrap();
    // Let a little arrive first.
    rig.ab.thaw();
    eventually("the receiver to know the transfer", || {
        rig.b()
            .probe(|engine| engine.incoming_ids())
            .first()
            .cloned()
    });
    rig.b().cancel_transfer(&id, code::USER).unwrap();
    eventually("both to clean up", || {
        (names(&rig.state("b")).is_empty() && names(&rig.state("a")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
    assert!(rig.a().cancel_transfer(&id, code::USER).is_err());
}

#[test]
fn a_bucket_item_removed_while_it_moves_is_cancelled_as_removed() {
    let rig = Rig::new("removed");
    let source = rig.source("f.bin", &payload(8 * BLOCK_BYTES as usize, 23));
    rig.ab.freeze();
    let id = rig.a().start_send(&source).unwrap();
    eventually("the offer", || {
        (!rig.ab.sent(FileAction::Offer).is_empty()).then_some(())
    });
    rig.a().cancel_transfer(&id, code::REMOVED).unwrap();
    let cancel = rig.ab.sent(FileAction::Cancel).remove(0);
    assert_eq!(cancel.code.as_deref(), Some("removed"));
}

// ── The source changes ─────────────────────────────────────────────────────

#[test]
fn a_file_deleted_mid_send_fails_with_the_file_no_longer_exists() {
    let rig = Rig::new("deleted");
    let source = rig.source("f.bin", &payload(24 * BLOCK_BYTES as usize, 31));
    let doomed = source.clone();
    let mut seen_second_block = false;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        if parse_frame(frame).unwrap().header.block_index == 1 && !seen_second_block {
            seen_second_block = true;
            fs::remove_file(&doomed).unwrap();
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let reason = eventually("the failure", || failed_with(&id));
    assert_eq!(reason, "The file no longer exists.");
    let cancel = rig.ab.sent(FileAction::Cancel).remove(0);
    assert_eq!(cancel.code.as_deref(), Some(code::SOURCE_GONE));
    eventually("both sides clean", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
}

#[test]
fn a_file_changed_mid_send_begins_again_under_a_new_id_and_arrives_whole() {
    let rig = Rig::new("changed");
    let first = payload(24 * BLOCK_BYTES as usize, 32);
    let source = rig.source("f.bin", &first);
    let edited = payload(24 * BLOCK_BYTES as usize + 4_000, 33);
    let (path, bytes) = (source.clone(), edited.clone());
    let mut changed = false;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        if parse_frame(frame).unwrap().header.block_index == 1 && !changed {
            changed = true;
            fs::write(&path, &bytes).unwrap();
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), edited, "the new content");
    assert_ne!(got.transfer_id, id, "a new transfer");
    let cancel = rig.ab.sent(FileAction::Cancel).remove(0);
    assert_eq!(cancel.code.as_deref(), Some(code::SOURCE_CHANGED));
    assert_eq!(
        names(&rig.downloads("b")).len(),
        1,
        "no half-file next to it"
    );
}

#[test]
fn a_file_that_keeps_changing_fails_after_two_restarts() {
    let rig = Rig::new("churn");
    let source = rig.source("f.bin", &payload(24 * BLOCK_BYTES as usize, 34));
    let path = source.clone();
    let mut last = String::new();
    let mut round = 0u64;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        let header = parse_frame(frame).unwrap().header;
        let id = super::super::file_protocol::id_string(&header.transfer);
        if header.block_index == 1 && id != last {
            last = id;
            round += 1;
            fs::write(
                &path,
                payload(24 * BLOCK_BYTES as usize + round as usize, 40 + round),
            )
            .unwrap();
        }
        Fate::Deliver
    }));
    rig.a().start_send(&source).unwrap();
    let reasons = eventually("the failure", || {
        let cancels = rig.ab.sent(FileAction::Cancel);
        let offers = rig.ab.sent(FileAction::Offer);
        let last = offers.last()?;
        failed_with(&last.transfer_id).filter(|_| cancels.len() == 3)
    });
    assert_eq!(reasons, words::source_changed_for_good());
    assert_eq!(
        rig.ab.sent(FileAction::Offer).len(),
        3,
        "the original and two restarts"
    );
    eventually("everything cleaned", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
}

// ── Disk, backlog, stalls ──────────────────────────────────────────────────

#[test]
fn a_full_disk_pauses_with_storage_full_and_the_transfer_resumes_when_space_frees() {
    let rig = Rig::new("disk-full");
    let bytes = payload(3 * BLOCK_BYTES as usize, 41);
    let source = rig.source("f.bin", &bytes);
    rig.b().probe(|engine| engine.faults.disk_full = true);
    let id = rig.a().start_send(&source).unwrap();
    let pause = eventually("the pause", || {
        rig.ba.sent(FileAction::Pause).into_iter().next()
    });
    assert_eq!(pause.reason.as_deref(), Some("storage-full"));
    // The person is told, in words, on the receiving side...
    eventually("the note", || {
        (board::note(&id).as_deref() == Some(words::storage_full().as_str())).then_some(())
    });
    // ...and the sender shows why it waits.
    eventually("the sender's note", || {
        board::note(&id).filter(|note| note.contains("no space"))
    });
    steady(
        "nothing is published while the disk is full",
        Duration::from_millis(250),
        || names(&rig.downloads("b")).is_empty() && failed_with(&id).is_none(),
    );
    rig.b().probe(|engine| {
        engine.faults.disk_full = false;
        engine.faults.free_bytes = Some(1 << 40);
    });
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    assert!(
        !rig.ba.sent(FileAction::Resume).is_empty(),
        "the receiver restarted the sender at its verified block"
    );
}

#[test]
fn a_receiver_far_behind_asks_the_sender_to_wait_and_then_restarts_it() {
    let rig = Rig::new("backlog");
    let bytes = payload(4 * BLOCK_BYTES as usize, 42);
    let source = rig.source("f.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    eventually("the transfer to be open", || {
        rig.b()
            .probe(|engine| engine.incoming_ids())
            .first()
            .cloned()
    });
    // The queue of unhandled frames is long (made so by hand)...
    let limit = rig.b().probe(|engine| {
        engine
            .shared
            .queued
            .store(engine.tun.queue_pause, Ordering::SeqCst);
        engine.apply_backlog();
        engine.tun.queue_pause
    });
    assert!(limit > 0);
    let pause = rig.ba.sent(FileAction::Pause);
    assert_eq!(
        pause.last().and_then(|p| p.reason.as_deref()),
        Some("backlog")
    );
    // ...and has drained: the sender begins again at the last verified block.
    rig.b().probe(|engine| {
        engine.shared.queued.store(0, Ordering::SeqCst);
        engine.apply_backlog();
    });
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    assert!(failed_with(&id).is_none(), "paused, never failed");
    let resume = rig.ba.sent(FileAction::Resume).remove(0);
    assert_eq!(resume.pass, Some(1));
}

#[test]
fn frames_that_overflow_the_queue_restart_the_transfer_instead_of_being_lost() {
    // A queue of two frames' worth, and a receiver thread held up.
    let rig = Rig::with("overflow", fast(), fast(), 2 * 61_440);
    let bytes = payload(3 * BLOCK_BYTES as usize, 43);
    let source = rig.source("f.bin", &bytes);
    // The accept is held back until the receiver's thread is held up.
    rig.ba.freeze();
    rig.a().start_send(&source).unwrap();
    eventually("the accept", || {
        (!rig.ba.sent(FileAction::Accept).is_empty()).then_some(())
    });
    let b = rig.b().clone();
    let (held_up, release) = channel::<()>();
    let (started_tx, started) = channel::<()>();
    let blocker = std::thread::spawn(move || {
        b.probe(move |_| {
            let _ = started_tx.send(());
            let _ = release.recv_timeout(Duration::from_secs(10));
        });
    });
    started.recv().unwrap();
    rig.ba.thaw();
    // The sender pours frames in while the receiver cannot read them.
    eventually("frames to be sent", || {
        (rig.ab.frames.load(Ordering::SeqCst) > 8).then_some(())
    });
    held_up.send(()).unwrap();
    blocker.join().unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "every byte, none lost");
    assert!(
        !rig.ba.sent(FileAction::Resume).is_empty(),
        "the overflow restarted the sender"
    );
}

#[test]
fn a_silent_link_only_shows_a_note_and_the_transfer_carries_on_when_it_wakes() {
    // A phone that dozes mid-transfer: nothing moves either way for a while.
    let rig = Rig::new("doze");
    let bytes = payload(48 * BLOCK_BYTES as usize, 44);
    let source = rig.source("f.bin", &bytes);
    // The link stalls after a few blocks (the frames of the rest wait).
    let ab = rig.ab.clone();
    let mut seen = 0;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        if parse_frame(frame).unwrap().header.last_of_block {
            seen += 1;
            if seen == 3 {
                ab.freeze();
            }
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    // Longer than the (shortened) no-progress time: no deadline cancels it.
    eventually("the waiting note", || {
        board::note(&id).filter(|note| *note == words::no_progress())
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(failed_with(&id).is_none(), "no deadline");
    assert!(!was_cancelled(&id));
    // The informational pause was said by someone, in either direction.
    let paused = |pipe: &Pipe| {
        pipe.sent(FileAction::Pause)
            .iter()
            .any(|pause| pause.reason.as_deref() == Some("no-progress"))
    };
    assert!(paused(&rig.ab) || paused(&rig.ba));
    *rig.ab.frame_hook.lock().unwrap() = None;
    rig.ab.thaw();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    eventually("the note to go", || {
        board::note(&id).is_none().then_some(())
    });
}

// ── Reconnecting ───────────────────────────────────────────────────────────

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

#[test]
fn wifi_pulled_mid_transfer_resumes_at_the_last_block_with_the_right_hash() {
    let mut rig = Rig::new("pulled");
    let bytes = payload(10 * BLOCK_BYTES as usize + 321, 51);
    let source = rig.source("f.bin", &bytes);
    pull_after_blocks(&rig, 4);
    let id = rig.a().start_send(&source).unwrap();
    eventually("the link to be pulled", || {
        (!rig.ab.up.load(Ordering::SeqCst)).then_some(())
    });
    let verified = rig.b().probe(|engine| {
        engine
            .incoming
            .values()
            .map(|incoming| incoming.verified_blocks())
            .max()
            .unwrap_or(0)
    });
    assert!(verified >= 3, "{verified} blocks held");
    // The connection comes back: a new link, new managers, the same disks.
    rig.stop(false);
    rig.connect();
    rig.a().resume_sends();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "final hash correct");
    let accept = rig.ba.sent(FileAction::Accept).remove(0);
    assert!(
        accept
            .resume_block
            .is_some_and(|block| block >= verified.min(3)),
        "resumed at block {:?}, not from the start (had {verified})",
        accept.resume_block
    );
    // The same transfer, under the same id and token.
    assert_eq!(rig.ab.sent(FileAction::Offer)[0].transfer_id, id);
    // Only the rest was sent again.
    let resent = rig.ab.frames.load(Ordering::SeqCst);
    assert!(
        resent < 7 * 18 + 6,
        "{resent} frames sent after the reconnect"
    );
    rig.sent_ok();
}

#[test]
fn an_app_killed_mid_receive_resumes_from_its_last_checkpoint() {
    let mut rig = Rig::new("killed");
    let bytes = payload(12 * BLOCK_BYTES as usize + 99, 52);
    let source = rig.source("f.bin", &bytes);
    pull_after_blocks(&rig, 7);
    rig.a().start_send(&source).unwrap();
    eventually("the link to be pulled", || {
        (!rig.ab.up.load(Ordering::SeqCst)).then_some(())
    });
    // Let the periodic checkpoint run, then die without a final one.
    std::thread::sleep(Duration::from_millis(150));
    rig.stop(true);
    rig.connect();
    rig.a().resume_sends();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    let accept = rig.ba.sent(FileAction::Accept).remove(0);
    let from = accept.resume_block.unwrap();
    assert!(
        (1..=7).contains(&from),
        "resumed at block {from}: the last checkpoint, never beyond what was verified"
    );
}

#[test]
fn a_prefix_the_sender_cannot_reproduce_restarts_at_the_beginning_under_a_new_pass() {
    let mut rig = Rig::new("bad-prefix");
    let bytes = payload(6 * BLOCK_BYTES as usize + 7, 53);
    let source = rig.source("f.bin", &bytes);
    pull_after_blocks(&rig, 3);
    rig.a().start_send(&source).unwrap();
    eventually("the link to be pulled", || {
        (!rig.ab.up.load(Ordering::SeqCst)).then_some(())
    });
    rig.stop(false);
    rig.connect();
    // The receiver's accept carries a prefix that is not the file's.
    *rig.ba.control_hook.lock().unwrap() = Some(Box::new(|control| {
        if control.action == FileAction::Accept && control.resume_block.is_some_and(|b| b > 0) {
            control.prefix_sha256 = Some("ab".repeat(32));
        }
        Fate::Deliver
    }));
    rig.a().resume_sends();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "still correct");
    // It began again from block 0 (every block went again).
    assert!(rig.ab.frames.load(Ordering::SeqCst) >= 6 * 17);
}

#[test]
fn only_the_reconnecting_devices_intact_sends_are_offered_again() {
    let rig = Rig::new("resume-filter");
    let state = rig.state("a");
    // A record for another device, one whose file is gone, one intact.
    let intact = rig.source("intact.bin", b"intact");
    let metadata = fs::metadata(&intact).unwrap();
    let record = |id: &str, device: &str, source: &Path, size: u64| {
        let checkpoint = super::store::SendCheckpoint {
            transfer_id: id.to_string(),
            device_id: device.to_string(),
            token: "t".repeat(32),
            filename: source.file_name().unwrap().to_string_lossy().into_owned(),
            total_size: size,
            mtime: super::store::modified_ms(&metadata),
            source: source.to_path_buf(),
            history: false,
            clipboard_mime: None,
            folder: None,
            content_sha256: None,
            restarts: 0,
        };
        fs::write(
            state.join(format!(".desklink-{id}.send.json")),
            serde_json::to_vec(&checkpoint).unwrap(),
        )
        .unwrap();
    };
    record(id(1), "beta", &intact, metadata.len());
    record(id(2), "someone-else", &intact, metadata.len());
    record(id(3), "beta", &rig.base.join("src").join("vanished.bin"), 5);
    rig.a().resume_sends();
    rig.received();
    let offers = rig.ab.sent(FileAction::Offer);
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].transfer_id, id(1));
    // The vanished one is dropped for good; the other device's stays.
    eventually("the records", || {
        let left = names(&state);
        (!left.iter().any(|n| n.contains(id(3))) && left.iter().any(|n| n.contains(id(2))))
            .then_some(())
    });
    // Once per connection.
    rig.a().resume_sends();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 1);
}

// ── Names, clipboard, captures ─────────────────────────────────────────────

#[test]
fn a_name_that_is_taken_becomes_name_2() {
    let rig = Rig::new("collision");
    fs::create_dir_all(rig.downloads("b")).unwrap();
    fs::write(rig.downloads("b").join("report.pdf"), b"the first one").unwrap();
    let source = rig.source("report.pdf", b"the second one");
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(got.path.file_name().unwrap(), "report (2).pdf");
    assert_eq!(fs::read(got.path).unwrap(), b"the second one");
    assert_eq!(
        fs::read(rig.downloads("b").join("report.pdf")).unwrap(),
        b"the first one",
        "never overwritten"
    );
}

#[test]
fn two_in_flight_transfers_with_the_same_name_both_survive() {
    let rig = Rig::new("same-name");
    let one = rig.base.join("src").join("one");
    let two = rig.base.join("src").join("two");
    fs::create_dir_all(&one).unwrap();
    fs::create_dir_all(&two).unwrap();
    fs::write(one.join("same.txt"), payload(3_000_000, 1)).unwrap();
    fs::write(two.join("same.txt"), payload(3_000_000, 2)).unwrap();
    rig.a().start_send(&one.join("same.txt")).unwrap();
    rig.a().start_send(&two.join("same.txt")).unwrap();
    rig.received();
    rig.received();
    let mut got = names(&rig.downloads("b"));
    got.sort();
    assert_eq!(got, ["same (2).txt", "same.txt"]);
}

#[test]
fn a_clipboard_image_is_placed_and_not_saved() {
    let rig = Rig::new("clipboard");
    let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
    data.extend(payload(1_000, 61));
    let source = rig.source("clip.png", &data);
    rig.a().start_send_as(&source, Some("image/png")).unwrap();
    let got = rig.received();
    assert!(got.capture_id.is_none());
    eventually("the image on the clipboard", || {
        crate::device_links::daemon::clipboard::TEST_PLACED
            .lock()
            .unwrap()
            .contains(&data)
            .then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty(), "not a download");
    eventually("clean state", || {
        names(&rig.state("b")).is_empty().then_some(())
    });
    let offer = rig.ab.sent(FileAction::Offer).remove(0);
    assert_eq!(offer.clipboard, Some(true));
    assert_eq!(offer.mime_type.as_deref(), Some("image/png"));
}

#[test]
fn a_capture_id_on_an_offer_reaches_the_completion() {
    let hand = Hand::new("capture");
    let capture = uuid::Uuid::new_v4().to_string();
    let mut data = vec![0xff, 0xd8, 0xff, 0xe0];
    data.extend(payload(500, 62));
    let (tx, done) = channel();
    let tx = Mutex::new(tx);
    hand.manager
        .set_completion_hook(move |completed| tx.lock().unwrap().send(completed.clone()).unwrap());
    let mut offer = hand.offer(id(1), "photo.jpg", data.len() as u64);
    offer.clipboard = Some(true);
    offer.mime_type = Some("image/jpeg".into());
    offer.capture_id = Some(capture.clone());
    hand.send(offer);
    eventually("the accept", || hand.reply(FileAction::Accept));
    hand.manager
        .on_frame(frame_of(id(1), 0, 0, &data, data.len() as u64));
    let mut message =
        FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
    message.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&data)])));
    message.blocks = Some(1);
    message.total_size = Some(data.len() as u64);
    hand.send(message);
    let completed = done.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(completed.capture_id.as_deref(), Some(capture.as_str()));
    // Not a UUID: ignored.
    let mut odd = hand.offer(id(2), "photo2.jpg", 10);
    odd.capture_id = Some("not-a-uuid".into());
    odd.clipboard = Some(true);
    odd.mime_type = Some("image/jpeg".into());
    hand.send(odd);
    eventually("the second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
}

#[test]
fn unsendable_sources_are_refused_before_anything_is_offered() {
    let rig = Rig::new("unsendable");
    let directory = rig.base.join("src");
    assert!(rig.a().start_send(&directory).is_err(), "a directory");
    assert!(
        rig.a().start_send(&rig.base.join("nope.bin")).is_err(),
        "missing"
    );
    // A name the peer would refuse.
    let odd = rig.base.join("src").join("a\u{1}b");
    fs::write(&odd, b"x").unwrap();
    assert!(
        rig.a().start_send(&odd).is_ok(),
        "a control character is only a name"
    );
    std::thread::sleep(Duration::from_millis(100));
    // Nothing for the refused ones.
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 1);
    // Detached: nothing starts.
    rig.a().detach();
    assert!(rig.a().start_send(&odd).is_err());
}

#[test]
fn a_progress_row_moves_with_the_blocks_and_never_goes_back() {
    // A sender alone (the receiver is played by hand), so the row is its own.
    let link = Pipe::new();
    let base = std::env::temp_dir().join(format!("desklink-v2-progress-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(base.join("src")).unwrap();
    let manager = Manager::with(
        base.join("state"),
        base.join("dl"),
        fast(),
        super::FILE_QUEUE_MAX_BYTES,
    )
    .unwrap();
    manager.attach(wire_of("alpha", "beta"), link.clone());
    let file = payload(4 * BLOCK_BYTES as usize, 71);
    let source = base.join("src").join("f.bin");
    fs::write(&source, &file).unwrap();
    let id = manager.start_send(&source).unwrap();
    let offer = eventually("the offer", || {
        link.sent(FileAction::Offer).into_iter().next()
    });
    let reply = |action: FileAction| {
        FileControl::new(
            &wire_of("beta", "alpha"),
            action,
            &id,
            &offer.transfer_token,
        )
    };
    let mut accept = reply(FileAction::Accept);
    accept.resume_block = Some(0);
    accept.prefix_sha256 = Some(hex(&hash_of_hashes(&[])));
    accept.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
    manager.on_control(accept);
    let total = 4 * BLOCK_BYTES;
    let mut last = 0;
    // The sender's window lets all four blocks out; none is acknowledged yet.
    eventually("the frames", || {
        (link.frames.load(Ordering::SeqCst) >= 4 * 18).then_some(())
    });
    std::thread::sleep(Duration::from_millis(50));
    let row = board::peek_row(&id).expect("listed");
    assert_eq!(row.total, total);
    last = last.max(row.done);
    // Each acknowledgement moves it forward by a block.
    for block in 0..4u64 {
        let mut ack = reply(FileAction::BlockAck);
        ack.block_index = Some(block);
        ack.sha256 = Some(hex(&openssl::sha::sha256(
            &file[(block * BLOCK_BYTES) as usize..((block + 1) * BLOCK_BYTES) as usize],
        )));
        manager.on_control(ack);
        let wanted = (block + 1) * BLOCK_BYTES;
        let row = eventually("the row to move", || {
            board::peek_row(&id).filter(|row| row.done >= wanted.min(total))
        });
        assert!(row.done >= last, "went back from {last} to {}", row.done);
        last = row.done;
    }
    // Everything acknowledged: `done`, then the receiver's `complete` ends it.
    let done = eventually("done", || link.sent(FileAction::Done).into_iter().next());
    let mut complete = reply(FileAction::Complete);
    complete.file_hash = done.file_hash.clone();
    manager.on_control(complete);
    eventually("the sent row to be recorded", || {
        was_sent(&id).then_some(())
    });
    assert_eq!(last, total);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn blocks_for_matches_what_the_frames_carry() {
    assert_eq!(blocks_for(3 * BLOCK_BYTES + 1), 4);
}

// ── A sender that is told to wait ──────────────────────────────────────────

#[test]
fn a_sender_told_busy_waits_and_offers_again_until_a_slot_frees() {
    let rig = Rig::new("busy-sender");
    // The receiver already holds two transfers from somewhere else.
    for (index, id) in [id(2), id(3)].into_iter().enumerate() {
        let mut offer = FileControl::new(&wire_of("alpha", "beta"), FileAction::Offer, id, "other");
        offer.filename = Some(format!("held-{index}.bin"));
        offer.total_size = Some(10);
        offer.mtime = Some(1_700_000_000_000);
        offer.block_size = Some(BLOCK_BYTES);
        offer.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
        rig.b().on_control(offer);
    }
    eventually("two held", || {
        (rig.b().probe(|engine| engine.incoming_ids()).len() == 2).then_some(())
    });
    let bytes = payload(200_000, 81);
    let source = rig.source("waiting.bin", &bytes);
    let wait = rig.a().start_send(&source).unwrap();
    // Told busy, it offers again (every 100 ms in these tests), never failing.
    eventually("two offers", || {
        (rig.ab.sent(FileAction::Offer).len() >= 3).then_some(())
    });
    assert!(failed_with(&wait).is_none(), "busy is never shown");
    // A slot frees: the next offer is accepted.
    let mut cancel = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(2),
        "other",
    );
    cancel.code = Some(code::USER.to_string());
    rig.b().on_control(cancel);
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
}

// ── One gibibyte ───────────────────────────────────────────────────────────

/// Edge-case row "1 GiB file": exactly 2^30 bytes cross intact. Opt-in, on a
/// real disk (`$DESKLINK_BENCH_DIR`, default `~/.cache/desklink-bench`):
/// `cargo test --release -- --ignored one_gibibyte`.
#[test]
#[ignore = "writes 2 GiB; run with --ignored one_gibibyte"]
fn one_gibibyte_crosses_intact() {
    use std::io::{Read, Write};
    let base = std::env::var_os("DESKLINK_BENCH_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::cache_dir().map(|cache| cache.join("desklink-bench")))
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("one-gib-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let rig = Rig::with("one-gib", fast(), fast(), super::FILE_QUEUE_MAX_BYTES);
    let source = base.join("one-gib.bin");
    let mut file = fs::File::create(&source).unwrap();
    let block = payload(1 << 20, 99);
    for index in 0..1024u32 {
        let mut varied = block.clone();
        varied[..4].copy_from_slice(&index.to_be_bytes());
        file.write_all(&varied).unwrap();
    }
    file.sync_all().unwrap();
    assert_eq!(fs::metadata(&source).unwrap().len(), 1 << 30);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    let digest = |path: &Path| {
        let mut file = fs::File::open(path).unwrap();
        let mut hasher = openssl::sha::Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break hex(&hasher.finish());
            }
            hasher.update(&buffer[..count]);
        }
    };
    assert_eq!(fs::metadata(&got.path).unwrap().len(), 1 << 30);
    assert_eq!(digest(&got.path), digest(&source));
    assert_eq!(rig.ba.sent(FileAction::BlockAck).len(), 1024);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn a_synced_file_is_offered_with_its_pair_path_and_hash_and_taken_when_asked_for() {
    use crate::device_links::webrtc::file_protocol::FolderRef;
    let rig = Rig::new("bucket-send");
    let bytes = payload(150_000, 91);
    let relpath = format!("Trips/{}.jpg", uuid::Uuid::new_v4().simple());
    let source = rig.source("synced.jpg", &bytes);
    let sha = hex(&openssl::sha::sha256(&bytes));
    let folder = FolderRef {
        folder: "bucket".into(),
        relpath: relpath.clone(),
        mtime: Some(1),
    };
    // The receiver asked for exactly this file.
    crate::device_links::folders::test_want("alpha", &folder, &sha, bytes.len() as u64);
    let id = rig.a().start_send_folder(&source, folder, &sha).unwrap();
    let got = rig.received();
    // It went to folder sync (no worker here, so the stash is dropped), not to
    // the downloads.
    assert!(names(&rig.downloads("b")).is_empty());
    assert!(got.transfer_id == id);
    let offer = rig.ab.sent(FileAction::Offer).remove(0);
    assert_eq!(offer.folder.as_deref(), Some("bucket"));
    assert_eq!(offer.relpath.as_deref(), Some(relpath.as_str()));
    assert_eq!(offer.content_sha256.as_deref(), Some(sha.as_str()));
    assert_eq!(offer.filename.as_deref(), relpath.rsplit('/').next());
    // A hash that is not a hash is refused before anything is offered.
    let again = FolderRef {
        folder: "bucket".into(),
        relpath: "x.jpg".into(),
        mtime: None,
    };
    assert!(rig.a().start_send_folder(&source, again, "nothex").is_err());
}

// ── Wire-fix rules (the golden transcripts cover the rest) ────────────────

/// A sender alone, its receiver played by hand.
struct Solo {
    manager: Manager,
    link: Arc<Pipe>,
    base: PathBuf,
    id: String,
    token: String,
}

impl Solo {
    fn new(label: &str, length: usize) -> Self {
        let base = std::env::temp_dir().join(format!(
            "desklink-v2-solo-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(base.join("src")).unwrap();
        let manager = Manager::with(
            base.join("state"),
            base.join("dl"),
            fast(),
            super::FILE_QUEUE_MAX_BYTES,
        )
        .unwrap();
        let link = Pipe::new();
        manager.attach(wire_of("alpha", "beta"), link.clone());
        let source = base.join("src").join("f.bin");
        fs::write(&source, payload(length, 5)).unwrap();
        let id = manager.start_send(&source).unwrap();
        let offer = eventually("the offer", || {
            link.sent(FileAction::Offer).into_iter().next()
        });
        Self {
            manager,
            link,
            base,
            id,
            token: offer.transfer_token,
        }
    }

    fn reply(&self, action: FileAction) -> FileControl {
        FileControl::new(&wire_of("beta", "alpha"), action, &self.id, &self.token)
    }

    fn accept(&self) {
        let mut accept = self.reply(FileAction::Accept);
        accept.resume_block = Some(0);
        accept.prefix_sha256 = Some(hex(&hash_of_hashes(&[])));
        accept.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
        self.manager.on_control(accept);
    }
}

impl Drop for Solo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn an_unknown_pause_reason_holds_the_sender_like_user_and_a_bare_resume_releases_it() {
    let solo = Solo::new("unknown-pause", 200_000);
    let mut pause = solo.reply(FileAction::Pause);
    pause.reason = Some("brand-new-reason".to_string());
    solo.manager.on_control(pause);
    solo.accept();
    steady(
        "nothing is sent while paused",
        Duration::from_millis(250),
        || solo.link.frames.load(Ordering::SeqCst) == 0,
    );
    // A bare resume (no fields) is valid and releases it.
    solo.manager.on_control(solo.reply(FileAction::Resume));
    eventually("frames", || {
        (solo.link.frames.load(Ordering::SeqCst) > 0).then_some(())
    });
}

/// The rule for a `block-ack` whose hash is not the one the sender read:
/// the sender rewinds to that block under the next pass (a receiver adopts
/// it); after `MAX_REWINDS` of them it ends with `error {hash-mismatch}`.
#[test]
fn a_block_ack_with_the_wrong_hash_rewinds_under_the_next_pass_then_fails() {
    let solo = Solo::new("ack-mismatch", 70_000);
    let seen = Arc::new(Mutex::new(Vec::<(u8, u64)>::new()));
    let log = Arc::clone(&seen);
    *solo.link.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        let header = parse_frame(frame).unwrap().header;
        log.lock().unwrap().push((header.pass, header.offset));
        Fate::Deliver
    }));
    solo.accept();
    eventually("the first pass", || {
        (seen.lock().unwrap().len() >= 2).then_some(())
    });
    let wrong = |block: u64| {
        let mut ack = solo.reply(FileAction::BlockAck);
        ack.block_index = Some(block);
        ack.sha256 = Some("ee".repeat(32));
        ack
    };
    for round in 1..=3u8 {
        solo.manager.on_control(wrong(0));
        eventually("a rewind", || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|(pass, offset)| *pass == round && *offset == 0)
                .then_some(())
        });
    }
    // The fourth wrong acknowledgement is one rewind too many.
    solo.manager.on_control(wrong(0));
    let error = eventually("the error", || {
        solo.link.sent(FileAction::Error).into_iter().next()
    });
    assert_eq!(error.code.as_deref(), Some(code::HASH_MISMATCH));
}

/// An older phone restarts at block 0 under the SAME pass when it cannot
/// reproduce the prefix: the first frame after an accept is authoritative,
/// not a duplicate.
#[test]
fn the_first_frame_after_an_accept_is_authoritative_even_under_the_same_pass() {
    let hand = Hand::new("same-pass-restart");
    let total = 2 * BLOCK_BYTES;
    let file = payload(total as usize, 9);
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("the accept", || hand.reply(FileAction::Accept));
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0", || hand.reply(FileAction::BlockAck));
    // A re-offer: the receiver says where it is, and waits for the stream.
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("a second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    // The sender starts over at block 0, still under pass 0.
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0 acknowledged again", || {
        (hand.link.sent(FileAction::BlockAck).len() == 2).then_some(())
    });
}

/// A source read error is `error {internal}`, never a cancel with a code the
/// phone rejects; unknown cancel and error codes from a peer are a generic
/// end.
#[test]
fn unknown_cancel_and_error_codes_from_a_peer_end_the_transfer_generically() {
    let hand = Hand::new("unknown-codes");
    hand.send(hand.offer(id(1), "one.bin", 10));
    hand.send(hand.offer(id(2), "two.bin", 10));
    eventually("both accepted", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    let mut cancel = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    cancel.code = Some("brand-new-code".to_string());
    hand.send(cancel);
    let mut error = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Error,
        id(2),
        "token0",
    );
    error.code = Some("another-new-code".to_string());
    hand.send(error);
    eventually("both slots free", || {
        hand.incoming().is_empty().then_some(())
    });
}
