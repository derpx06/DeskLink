//! The engine over a real Lane A: two paired devices with real certificates,
//! the daemon's listener behaviour, a pacing/cutting proxy in front of it, and
//! the usual scriptable pipes as the WebRTC data channel.

use super::super::super::file_lane::{self, LaneTarget};
use super::*;
use crate::device_links::config::Config;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::NetworkPacket;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::MutexGuard;

/// The listen port and the corruption flag are process-wide: lane tests
/// run one at a time.
static LANE_LOCK: Mutex<()> = Mutex::new(());

const MIB: usize = 1 << 20;

struct Side {
    config: Arc<Mutex<Config>>,
    id: String,
    dir: PathBuf,
}

fn side() -> Side {
    let dir = std::env::temp_dir().join(format!("desklink-lane-eng-{}", uuid::Uuid::new_v4()));
    let config = Config::load_from_dir(dir.clone()).unwrap();
    let id = config.local_device_info().id;
    Side {
        config: Arc::new(Mutex::new(config)),
        id,
        dir,
    }
}

fn pem(side: &Side) -> String {
    String::from_utf8(side.config.lock().unwrap().certificate().to_pem().unwrap()).unwrap()
}

fn pair(a: &Side, b: &Side) {
    let (pem_a, pem_b) = (pem(a), pem(b));
    a.config
        .lock()
        .unwrap()
        .trust_device(&DeviceInfo::local(b.id.clone(), "b".into()), pem_b)
        .unwrap();
    b.config
        .lock()
        .unwrap()
        .trust_device(&DeviceInfo::local(a.id.clone(), "a".into()), pem_a)
        .unwrap();
}

/// What the proxy does to the sender-to-receiver direction of one lane
/// connection (connections take these in order; then `Pass`).
#[derive(Clone, Copy)]
enum Plan {
    Pass,
    /// Closes both directions after this many bytes.
    Cut(u64),
    /// Stops forwarding after this many bytes and keeps the sockets open.
    Blackhole(u64),
}

struct Proxy {
    port: u16,
    plans: Arc<Mutex<VecDeque<Plan>>>,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
}

impl Proxy {
    fn start(target: SocketAddr, pace: Duration) -> Self {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        socket.set_nonblocking(true).unwrap();
        let port = socket.local_addr().unwrap().port();
        let plans = Arc::new(Mutex::new(VecDeque::new()));
        let connections = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let (plans2, count, stop2) = (plans.clone(), connections.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop2.load(Ordering::SeqCst) {
                let Ok((client, _)) = socket.accept() else {
                    std::thread::sleep(Duration::from_millis(3));
                    continue;
                };
                client.set_nonblocking(false).unwrap();
                count.fetch_add(1, Ordering::SeqCst);
                let plan = plans2.lock().unwrap().pop_front().unwrap_or(Plan::Pass);
                let Ok(server) = TcpStream::connect(target) else {
                    continue;
                };
                for stream in [&client, &server] {
                    let _ = stream.set_nodelay(true);
                }
                let (c_read, s_write) = (client.try_clone().unwrap(), server.try_clone().unwrap());
                let (s_read, c_write) = (server.try_clone().unwrap(), client.try_clone().unwrap());
                std::thread::spawn(move || {
                    let (mut from, mut to) = (c_read, s_write);
                    let mut moved = 0u64;
                    let mut buffer = vec![0u8; 32 * 1024];
                    while let Ok(read) = from.read(&mut buffer) {
                        if read == 0 {
                            break;
                        }
                        match plan {
                            Plan::Cut(limit) if moved + read as u64 > limit => {
                                let keep = (limit - moved) as usize;
                                let _ = to.write_all(&buffer[..keep]);
                                break;
                            }
                            Plan::Blackhole(limit) if moved >= limit => {
                                moved += read as u64;
                                continue;
                            }
                            _ => {}
                        }
                        if to.write_all(&buffer[..read]).is_err() {
                            break;
                        }
                        moved += read as u64;
                        if !pace.is_zero() {
                            std::thread::sleep(pace);
                        }
                    }
                    if !matches!(plan, Plan::Blackhole(_)) {
                        let _ = to.shutdown(Shutdown::Both);
                        let _ = from.shutdown(Shutdown::Both);
                    }
                });
                std::thread::spawn(move || {
                    let (mut from, mut to) = (s_read, c_write);
                    let _ = std::io::copy(&mut from, &mut to);
                    let _ = to.shutdown(Shutdown::Write);
                });
            }
        });
        Self {
            port,
            plans,
            connections,
            stop,
        }
    }

    fn plan(&self, plans: &[Plan]) {
        self.plans.lock().unwrap().extend(plans.iter().copied());
    }
}

/// The daemon's listener, as far as lanes go: read the first line, branch
/// on its type, hand a lane to `file_lane::accept`.
fn listener(config: Arc<Mutex<Config>>, stop: Arc<AtomicBool>) -> SocketAddr {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    socket.set_nonblocking(true).unwrap();
    let addr = socket.local_addr().unwrap();
    std::thread::spawn(move || {
        while !stop.load(Ordering::SeqCst) {
            let Ok((stream, _)) = socket.accept() else {
                std::thread::sleep(Duration::from_millis(3));
                continue;
            };
            stream.set_nonblocking(false).unwrap();
            let config = Arc::clone(&config);
            std::thread::spawn(move || {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let Ok(line) = file_lane::read_line(&mut stream) else {
                    return;
                };
                if let Ok(packet) = NetworkPacket::deserialize(&line) {
                    if packet.packet_type == file_lane::FIRST_LINE_TYPE {
                        let _ = file_lane::accept(stream, &config, &packet);
                    }
                }
            });
        }
    });
    addr
}

struct Env {
    _guard: MutexGuard<'static, ()>,
    sender: Side,
    receiver: Side,
    proxy: Proxy,
}

impl Env {
    /// `pace`: the proxy sleeps this long per 32 KiB it forwards.
    fn new(pace: Duration) -> Self {
        let guard = LANE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        CORRUPT_NEXT_LANE_FRAME.store(false, Ordering::SeqCst);
        CORRUPT_EVERY_LANE_FRAME.store(false, Ordering::SeqCst);
        let (sender, receiver) = (side(), side());
        pair(&sender, &receiver);
        let stop = Arc::new(AtomicBool::new(false));
        let real = listener(Arc::clone(&receiver.config), stop);
        let proxy = Proxy::start(real, pace);
        file_lane::set_listen_port(proxy.port);
        Self {
            _guard: guard,
            sender,
            receiver,
            proxy,
        }
    }

    fn rig(&self, label: &str, tun: Tunables) -> Rig {
        let target = |me: &Side, peer: &Side| LaneTarget {
            addr: std::net::IpAddr::from([127, 0, 0, 1]),
            config: Arc::clone(&me.config),
            local_id: me.id.clone(),
            peer_id: peer.id.clone(),
        };
        Rig::build(
            label,
            tun.clone(),
            tun,
            super::super::FILE_QUEUE_MAX_BYTES,
            (self.sender.id.clone(), self.receiver.id.clone()),
            Some((
                target(&self.sender, &self.receiver),
                target(&self.receiver, &self.sender),
            )),
        )
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.proxy.stop.store(true, Ordering::SeqCst);
        file_lane::set_listen_port(0);
        for side in [&self.sender, &self.receiver] {
            let _ = fs::remove_dir_all(&side.dir);
        }
    }
}

fn lane_actions(rig: &Rig) -> Vec<(String, u64, u8)> {
    rig.ab
        .sent(FileAction::Lane)
        .into_iter()
        .map(|control| {
            let lane = control.lane.unwrap();
            (
                lane.kind.unwrap(),
                lane.start_block.unwrap(),
                lane.pass.unwrap(),
            )
        })
        .collect()
}

fn verified(rig: &Rig) -> u64 {
    rig.b().probe(|engine| {
        engine
            .incoming
            .values()
            .map(|incoming| incoming.verified_blocks())
            .max()
            .unwrap_or(0)
    })
}

/// The sender with a lane for as long as `until` says so: a hook that
/// slows the data channel so a failover is visibly a failover.
fn slow_pipe(rig: &Rig, per_frame: Duration) {
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |_| {
        std::thread::sleep(per_frame);
        Fate::Deliver
    }));
}

/// A manager replaced by a newer one (a rebuilt WebRTC peer) shuts down
/// after the new one attached: the new one still takes lanes.
#[test]
fn a_replaced_manager_shutting_down_leaves_its_successor_registered() {
    let _guard = LANE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let base = std::env::temp_dir().join(format!("desklink-v2-replace-{}", uuid::Uuid::new_v4()));
    let make = |name: &str| {
        Manager::with(
            base.join(format!("{name}-state")),
            base.join(format!("{name}-dl")),
            fast(),
            super::super::FILE_QUEUE_MAX_BYTES,
        )
        .unwrap()
    };
    let peer = format!("replaced-{}", uuid::Uuid::new_v4());
    let (old, new) = (make("old"), make("new"));
    let (pipe_old, pipe_new) = (Pipe::new(), Pipe::new());
    old.attach(wire_of("desk", &peer), pipe_old);
    new.attach(wire_of("desk", &peer), pipe_new);
    // The old worker exits only now.
    old.detach();
    old.shut_down();
    old.stop();
    // The successor is still the registered engine.
    assert!(file_lane::receiver_for_test(&peer).is_some());
    new.detach();
    assert!(file_lane::receiver_for_test(&peer).is_none());
    new.stop();
    let _ = fs::remove_dir_all(base);
}

/// A frame of another transfer than the one the token was for ends the
/// lane; the frames before it were delivered.
#[test]
fn a_lane_frame_for_another_transfer_ends_the_lane() {
    use super::super::super::file_protocol::FrameHeader;
    let mine = id_bytes("0f8fad5b-d9cb-469f-a165-70867728950e").unwrap();
    let other = id_bytes("1f8fad5b-d9cb-469f-a165-70867728950e").unwrap();
    let frame = |transfer| {
        let mut out = Vec::new();
        FrameHeader {
            last_of_block: false,
            pass: 0,
            transfer,
            block_index: 0,
            offset: 0,
            length: 4,
        }
        .write(&mut out);
        out.extend_from_slice(b"data");
        out
    };
    let mut wire = Vec::new();
    wire.extend(frame(mine));
    wire.extend(frame(mine));
    wire.extend(frame(other));
    wire.extend(frame(mine));
    let (commands, inbox) = channel();
    let shared = super::super::Shared {
        queued: AtomicUsize::new(0),
        overflow: AtomicBool::new(false),
        queue_max: super::super::FILE_QUEUE_MAX_BYTES,
        attached: AtomicBool::new(true),
        purged: Arc::new(AtomicBool::new(false)),
        hook: Mutex::new(None),
    };
    super::super::read_lane_frames(&mut std::io::Cursor::new(wire), &commands, &shared, mine);
    drop(commands);
    let delivered = inbox.iter().count();
    assert_eq!(delivered, 2, "the two frames before the stranger");
}

/// A receiver that paused is not a dead lane: five quiet seconds under a
/// pause leave the lane up; the same quiet while sending end it.
#[test]
fn a_paused_receiver_does_not_make_a_lane_look_dead() {
    let env = Env::new(Duration::from_millis(2));
    let rig = env.rig("lane-pause", fast());
    let bytes = payload(48 * MIB, 31);
    let source = rig.source("big.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    eventually("some blocks over the lane", || {
        (board::lane_of(&id) == Some("fast, over Wi-Fi") && verified(&rig) >= 2).then_some(())
    });
    let step = |held: bool| {
        rig.a().probe(move |engine| {
            let net = engine.net.clone().unwrap();
            engine.outgoing[0].silent_lane_check(held, &net)
        })
    };
    let (still_up, _) = step(true);
    assert!(still_up, "a pause is not silence");
    let (up_after, sending) = step(false);
    assert!(!up_after, "but silence while sending is");
    assert!(sending);
}

/// A receiver that pauses for want of space holds the transfer for a long
/// while: the sender lets the lane go (no failure counted, no back-off for
/// the peer) instead of leaving an idle connection for the receiver to
/// find silent, and a lane is asked for again once the transfer moves.
#[test]
fn a_transfer_paused_for_space_lets_its_lane_go_without_a_failure_and_gets_one_again() {
    let env = Env::new(Duration::from_millis(2));
    let mut tun = fast();
    tun.lane_retry = Duration::from_millis(100);
    let rig = env.rig("lane-release", tun);
    slow_pipe(&rig, Duration::from_millis(3));
    let bytes = payload(48 * MIB, 33);
    let source = rig.source("big.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    eventually("some blocks over the lane", || {
        (board::lane_of(&id) == Some("fast, over Wi-Fi") && verified(&rig) >= 2).then_some(())
    });
    rig.b().probe(|engine| engine.faults.disk_full = true);
    eventually("the lane is let go", || {
        (board::lane_of(&id) == Some("over the secure link")).then_some(())
    });
    assert_eq!(
        file_lane::peer_wait_for_test(&env.receiver.id, Instant::now()),
        Duration::ZERO,
        "a pause is not a failed lane"
    );
    steady(
        "no new lane while it is held",
        Duration::from_millis(300),
        || board::lane_of(&id) == Some("over the secure link"),
    );
    rig.b().probe(|engine| {
        engine.faults.disk_full = false;
        engine.faults.free_bytes = Some(1 << 40);
    });
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    let kinds: Vec<String> = lane_actions(&rig).into_iter().map(|a| a.0).collect();
    assert_eq!(&kinds[..3], ["tcp", "webrtc", "tcp"], "{kinds:?}");
    assert_eq!(
        file_lane::peer_wait_for_test(&env.receiver.id, Instant::now()),
        Duration::ZERO
    );
}

/// A pause that arrives while the lane is still being agreed ends that
/// lane too (no dial goes on to a transfer that is held); one that
/// arrives when every block is acknowledged does not make the sender say
/// `done` again; a promotion that waits for acknowledgements is dropped
/// when its reason goes, instead of freezing the transfer.
#[test]
fn a_pause_ends_a_lane_being_agreed_and_a_finished_send_is_left_alone() {
    let env = Env::new(Duration::from_millis(2));
    let rig = env.rig("lane-pending", fast());
    let bytes = payload(48 * MIB, 34);
    let source = rig.source("big.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    eventually("some blocks over the lane", || {
        (board::lane_of(&id) == Some("fast, over Wi-Fi") && verified(&rig) >= 2).then_some(())
    });
    // The lane under agreement (a token waits): the pause ends it.
    let (off, releases, wants_again) = rig.a().probe(|engine| {
        let net = engine.net.clone().unwrap();
        let outgoing = &mut engine.outgoing[0];
        outgoing.pretend_lane_pending();
        outgoing.on_peer_pause("storage-full", Some(&net));
        let (off, releases, wants_again, _, _) = outgoing.lane_facts();
        (off, releases, wants_again)
    });
    assert!(off && releases == 1 && wants_again);
    // Every block acknowledged and `complete` awaited: a pause with a lane
    // up closes it and changes nothing else (no restart, no second `done`).
    let state_kept = rig.a().probe(|engine| {
        let net = engine.net.clone().unwrap();
        let outgoing = &mut engine.outgoing[0];
        outgoing.pretend(false, true);
        outgoing.on_peer_pause("user", Some(&net));
        outgoing.lane_facts().4
    });
    assert!(state_kept);
    // A promotion waiting for acknowledgements, its reason gone: dropped.
    let still = rig.a().probe(|engine| {
        engine.outgoing[0].pretend(true, false);
        engine.pump_sends();
        engine.outgoing[0].lane_facts().3
    });
    assert!(!still, "the wait ends with its reason");
    rig.a().cancel_transfer(&id, code::USER).unwrap();
}

/// The acknowledgements of the first five blocks never arrive, then the
/// lane drops: the receiver verified five, the sender saw none. The
/// data channel takes over from the receiver's five, not the sender's none.
#[test]
fn a_failover_restarts_at_what_the_receiver_verified_not_what_the_sender_saw() {
    let env = Env::new(Duration::from_millis(1));
    env.proxy.plan(&[Plan::Cut(5 * MIB as u64 + 600_000)]);
    let mut tun = fast();
    tun.lane_retry = Duration::from_secs(600);
    let rig = env.rig("lane-verified", tun);
    let lost = Arc::new(AtomicUsize::new(0));
    let counter = lost.clone();
    *rig.ba.control_hook.lock().unwrap() = Some(Box::new(move |control| {
        if control.action == FileAction::BlockAck && counter.fetch_add(1, Ordering::SeqCst) < 5 {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    }));
    let bytes = payload(24 * MIB + 99, 61);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    let said: Vec<u64> = rig
        .ba
        .sent(FileAction::BlockNack)
        .into_iter()
        .filter(|nack| nack.reason.as_deref() == Some("lane"))
        .filter_map(|nack| nack.block_index)
        .collect();
    let verified = said
        .iter()
        .copied()
        .max()
        .expect("the receiver said where it was");
    // The sender saw no acknowledgement at all (the first five are dropped); the receiver had
    // verified some blocks, and said so.
    assert!(verified >= 1, "the receiver had verified {verified}");
    // Which end notices the dead lane first is a matter of thread timing. When the receiver's
    // word arrives first the sender carries on from it, and nothing verified goes again over
    // the data channel. When the sender's own switch (from the block it saw acknowledged: 0)
    // comes first, frames from there are already on their way when the receiver's position
    // follows (the receiver then holds only what it still needs); the byte count says
    // nothing about the receiver's number in that order, so it is checked in the first only.
    let switch = lane_actions(&rig)
        .into_iter()
        .find(|(kind, _, _)| kind == "webrtc")
        .expect("the sender went over the data channel");
    if switch.1 >= verified {
        let carried = rig.ab.frame_bytes.load(Ordering::SeqCst);
        let overhead = rig.ab.frames.load(Ordering::SeqCst) as u64 * 40 + 25 * 32;
        assert!(
            carried
                <= bytes.len() as u64 - (verified * BLOCK_BYTES).min(bytes.len() as u64) + overhead,
            "{carried} bytes over WebRTC with {verified} blocks verified"
        );
    }
    assert!(rig.ab.frames.load(Ordering::SeqCst) > 0);
}

/// Three lane failures and the transfer stays on the data channel.
#[test]
fn a_transfer_gives_the_lane_three_failures_and_then_stays_on_webrtc() {
    let env = Env::new(Duration::from_millis(1));
    env.proxy.plan(&[
        Plan::Cut(2 * MIB as u64),
        Plan::Cut(2 * MIB as u64),
        Plan::Cut(2 * MIB as u64),
        Plan::Cut(2 * MIB as u64),
    ]);
    let mut tun = fast();
    tun.lane_retry = Duration::from_millis(20);
    let rig = env.rig("lane-flap", tun);
    slow_pipe(&rig, Duration::from_millis(1));
    let bytes = payload(96 * MIB + 5, 62);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    let kinds: Vec<String> = lane_actions(&rig).into_iter().map(|a| a.0).collect();
    assert_eq!(
        kinds.iter().filter(|kind| *kind == "tcp").count(),
        3,
        "{kinds:?}"
    );
    assert_eq!(kinds.last().map(String::as_str), Some("webrtc"));
    assert_eq!(env.proxy.connections.load(Ordering::SeqCst), 3);
}

/// A peer that failed lanes is held back (no lane offered) until its
/// back-off is over; a clean lane transfer clears it.
#[test]
fn a_peer_in_back_off_is_not_offered_a_lane_and_a_clean_transfer_clears_it() {
    let env = Env::new(Duration::ZERO);
    file_lane::penalize_peer(&env.receiver.id, Duration::from_secs(600), Instant::now());
    let rig = env.rig("lane-backoff", fast());
    let bytes = payload(10 * MIB, 63);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    assert_eq!(fs::read(rig.received().path).unwrap(), bytes);
    assert!(rig.ab.sent(FileAction::Offer)[0].lane.is_none());
    assert!(rig.ab.frames.load(Ordering::SeqCst) > 0);
    file_lane::forgive_peer(&env.receiver.id);
    // Clear: the next transfer gets its lane, and finishing clean leaves
    // the peer unpunished.
    let rig = env.rig("lane-cleared", fast());
    let source = rig.source("big2.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    assert_eq!(fs::read(rig.received().path).unwrap(), bytes);
    rig.sent_ok();
    assert!(rig.ab.sent(FileAction::Offer)[0].lane.is_some());
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 0);
    assert_eq!(
        file_lane::peer_wait_for_test(&env.receiver.id, Instant::now()),
        Duration::ZERO
    );
}

/// A token minted before a long prefix check is not dialled with when it
/// is about to lapse: a fresh one is asked for first.
#[test]
fn an_old_token_is_replaced_before_it_is_dialled() {
    let env = Env::new(Duration::ZERO);
    let mut tun = fast();
    tun.lane_token_fresh = Duration::ZERO;
    let rig = env.rig("lane-slow-prefix", tun);
    let bytes = payload(10 * MIB, 64);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    assert_eq!(fs::read(rig.received().path).unwrap(), bytes);
    let tokens: Vec<String> = rig
        .ba
        .sent(FileAction::Accept)
        .into_iter()
        .filter_map(|accept| accept.lane.and_then(|lane| lane.token))
        .collect();
    assert_eq!(tokens.len(), 3, "the accept, and two asked-for refreshes");
    let mut distinct = tokens.clone();
    distinct.sort();
    distinct.dedup();
    assert_eq!(distinct.len(), 3, "each a fresh token");
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 3);
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 0, "and then the lane");
    assert_eq!(env.proxy.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn the_back_off_doubles_to_five_minutes_and_a_default_token_is_fresh_for_fifty_seconds() {
    let peer = format!("backoff-{}", uuid::Uuid::new_v4());
    let t0 = Instant::now();
    let waits: Vec<u64> = (0..8)
        .map(|_| {
            file_lane::penalize_peer(&peer, Duration::from_secs(10), t0);
            file_lane::peer_wait_for_test(&peer, t0).as_secs()
        })
        .collect();
    assert_eq!(waits, [10, 20, 40, 80, 160, 300, 300, 300]);
    assert!(file_lane::peer_blocked(
        &peer,
        t0 + Duration::from_secs(299)
    ));
    assert!(!file_lane::peer_blocked(
        &peer,
        t0 + Duration::from_secs(300)
    ));
    file_lane::forgive_peer(&peer);
    assert!(!file_lane::peer_blocked(&peer, t0));
    assert_eq!(
        Tunables::default().lane_token_fresh,
        file_lane::TOKEN_TTL - Duration::from_secs(10)
    );
}

#[test]
fn a_big_file_between_paired_devices_goes_over_the_lane() {
    let env = Env::new(Duration::from_millis(1));
    let rig = env.rig("lane-basic", fast());
    let bytes = payload(24 * MIB + 4321, 11);
    let source = rig.source("big.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    eventually("the lane label", || {
        (board::lane_of(&id) == Some("fast, over Wi-Fi")).then_some(())
    });
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    rig.sent_ok();
    // Not one frame went over the data channel; the control plane did
    // all of its usual work on it.
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 0);
    let offer = rig.ab.sent(FileAction::Offer).remove(0);
    assert_eq!(offer.lane.as_ref().and_then(|lane| lane.v), Some(1));
    let accept = rig.ba.sent(FileAction::Accept).remove(0).lane.unwrap();
    assert_eq!(accept.port, Some(env.proxy.port));
    assert_eq!(accept.ttl_ms, Some(60_000));
    assert_eq!(accept.token.as_deref().map(str::len), Some(32));
    assert_eq!(
        lane_actions(&rig)
            .iter()
            .map(|a| a.0.as_str())
            .collect::<Vec<_>>(),
        ["tcp"]
    );
    assert_eq!(rig.ba.sent(FileAction::BlockAck).len(), 25);
    assert_eq!(env.proxy.connections.load(Ordering::SeqCst), 1);
}

/// A lane throttled to about 10 MB/s (3 ms per 32 KiB through the proxy)
/// with the production tunables: the watchdogs (5 s without an
/// acknowledged block, the receiver's quiet rule) must not end it.
#[test]
fn a_throttled_lane_survives_a_slow_link() {
    let env = Env::new(Duration::from_millis(3));
    let rig = env.rig("lane-slow", Tunables::default());
    let bytes = payload(40 * MIB + 77, 21);
    let source = rig.source("big.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    eventually("the lane label", || {
        (board::lane_of(&id) == Some("fast, over Wi-Fi")).then_some(())
    });
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    rig.sent_ok();
    assert_eq!(
        rig.ab.frames.load(Ordering::SeqCst),
        0,
        "all of it on the lane"
    );
    assert_eq!(
        lane_actions(&rig).len(),
        1,
        "one switch, to the lane, no more"
    );
    assert!(rig
        .ba
        .sent(FileAction::BlockNack)
        .iter()
        .all(|nack| nack.reason.as_deref() != Some("lane")));
    assert_eq!(env.proxy.connections.load(Ordering::SeqCst), 1);
}

#[test]
fn a_lane_that_drops_mid_block_falls_back_and_resumes_at_the_last_verified_block() {
    let env = Env::new(Duration::from_millis(1));
    // Cut after about 10.5 blocks of ciphertext.
    env.proxy.plan(&[Plan::Cut(10 * MIB as u64 + 600_000)]);
    let mut tun = fast();
    tun.lane_retry = Duration::from_secs(600);
    let rig = env.rig("lane-drop", tun);
    let bytes = payload(24 * MIB + 17, 12);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(
        fs::read(&got.path).unwrap(),
        bytes,
        "the final bytes are the file"
    );
    rig.sent_ok();
    let actions = lane_actions(&rig);
    assert_eq!(actions.len(), 2, "{actions:?}");
    assert_eq!(actions[0].0, "tcp");
    assert_eq!(actions[1].0, "webrtc");
    assert_ne!(actions[1].2, actions[0].2, "a new pass");
    let from = actions[1].1;
    assert!(
        (1..=11).contains(&from),
        "resumed at block {from}, not from 0 and not past the cut"
    );
    // The data channel carried only what the lane had not delivered
    // and verified: no block twice.
    let carried = rig.ab.frame_bytes.load(Ordering::SeqCst);
    let rest = bytes.len() as u64 - from * BLOCK_BYTES;
    let overhead = rig.ab.frames.load(Ordering::SeqCst) as u64 * 40 + 25 * 32;
    assert!(
        carried <= rest + overhead,
        "{carried} bytes over WebRTC for {rest} left"
    );
    assert!(rig.ab.frames.load(Ordering::SeqCst) > 0);
    assert!(rig
        .ba
        .sent(FileAction::BlockNack)
        .iter()
        .all(|nack| nack.reason.as_deref() == Some("lane")));
}

#[test]
fn a_lane_that_goes_silent_is_left_after_five_seconds() {
    let env = Env::new(Duration::ZERO);
    env.proxy.plan(&[Plan::Blackhole(5 * MIB as u64)]);
    let mut tun = fast();
    tun.lane_retry = Duration::from_secs(600);
    let rig = env.rig("lane-silent", tun);
    let bytes = payload(16 * MIB, 13);
    let source = rig.source("big.bin", &bytes);
    let started = Instant::now();
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    assert!(
        started.elapsed() >= Duration::from_secs(4),
        "the sender waited for the lane"
    );
    let actions = lane_actions(&rig);
    assert_eq!(actions.last().map(|a| a.0.as_str()), Some("webrtc"));
}

#[test]
fn after_a_failure_the_lane_comes_back_at_a_block_boundary() {
    let env = Env::new(Duration::from_millis(1));
    env.proxy.plan(&[Plan::Cut(6 * MIB as u64), Plan::Pass]);
    let mut tun = fast();
    tun.lane_retry = Duration::from_millis(150);
    let rig = env.rig("lane-promote", tun);
    slow_pipe(&rig, Duration::from_millis(2));
    let bytes = payload(64 * MIB + 5, 14);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    let kinds: Vec<String> = lane_actions(&rig).into_iter().map(|a| a.0).collect();
    assert_eq!(&kinds[..3], ["tcp", "webrtc", "tcp"], "{kinds:?}");
    assert!(env.proxy.connections.load(Ordering::SeqCst) >= 2);
}

/// A promotion re-offers only once the block in progress is finished and
/// every block sent is acknowledged: what the receiver restarts at is
/// exactly what was sent, so nothing in flight on the data channel is
/// thrown away.
#[test]
fn a_promotion_waits_for_the_blocks_in_flight_so_nothing_is_thrown_away() {
    let env = Env::new(Duration::from_millis(1));
    env.proxy.plan(&[Plan::Cut(6 * MIB as u64), Plan::Pass]);
    let mut tun = fast();
    tun.lane_retry = Duration::from_millis(150);
    let rig = env.rig("lane-drain", tun);
    slow_pipe(&rig, Duration::from_millis(2));
    let bytes = payload(64 * MIB + 5, 17);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    eventually("the promotion's re-offer", || {
        (rig.ab.sent(FileAction::Offer).len() == 2).then_some(())
    });
    let in_flight = rig.a().probe(|engine| {
        engine
            .outgoing
            .first()
            .and_then(|outgoing| outgoing.promoted_with_in_flight)
    });
    assert_eq!(in_flight, Some(0), "the re-offer went with data in flight");
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    let kinds: Vec<String> = lane_actions(&rig).into_iter().map(|a| a.0).collect();
    assert_eq!(&kinds[..3], ["tcp", "webrtc", "tcp"], "{kinds:?}");
}

#[test]
fn a_block_corrupted_on_the_lane_is_sent_once_more_on_the_lane() {
    let env = Env::new(Duration::ZERO);
    let rig = env.rig("lane-corrupt", fast());
    CORRUPT_NEXT_LANE_FRAME.store(true, Ordering::SeqCst);
    let bytes = payload(12 * MIB + 1, 15);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    let nacks = rig.ba.sent(FileAction::BlockNack);
    assert_eq!(nacks.len(), 1, "refused once");
    assert_eq!(
        nacks[0].reason.as_deref(),
        Some("hash-mismatch"),
        "a damaged block, not a lost lane"
    );
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 0, "still on the lane");
    assert_eq!(lane_actions(&rig).len(), 1);
}

/// A lane that corrupts every block is given up after the second refusal
/// of a block: the data channel finishes the file; no error, no rewinds.
#[test]
fn a_lane_whose_blocks_keep_failing_hands_over_to_webrtc() {
    let env = Env::new(Duration::ZERO);
    let rig = env.rig("lane-bad", fast());
    CORRUPT_EVERY_LANE_FRAME.store(true, Ordering::SeqCst);
    let bytes = payload(12 * MIB + 7, 32);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    CORRUPT_EVERY_LANE_FRAME.store(false, Ordering::SeqCst);
    assert_eq!(fs::read(&got.path).unwrap(), bytes);
    let kinds: Vec<String> = lane_actions(&rig).into_iter().map(|a| a.0).collect();
    assert_eq!(kinds.first().map(String::as_str), Some("tcp"));
    assert!(kinds.contains(&"webrtc".to_string()), "{kinds:?}");
    assert!(rig.ab.frames.load(Ordering::SeqCst) > 0);
    let reasons: Vec<_> = rig
        .ba
        .sent(FileAction::BlockNack)
        .into_iter()
        .map(|nack| nack.reason)
        .collect();
    // The lane is given up at the second refusal of a block. The receiver may also say where
    // it was when it sees the lane close (`lane`: not a refusal); whether that word is sent
    // before the sender's own switch is processed depends on thread timing, so it is allowed.
    let refusals = reasons
        .iter()
        .filter(|reason| reason.as_deref() == Some("hash-mismatch"))
        .count();
    assert_eq!(refusals, 2, "{reasons:?}");
    assert!(
        reasons
            .iter()
            .all(|reason| matches!(reason.as_deref(), Some("hash-mismatch" | "lane"))),
        "{reasons:?}"
    );
}

#[test]
fn cancel_ends_a_laned_transfer_on_both_sides_and_leaves_nothing() {
    let env = Env::new(Duration::from_millis(2));
    let rig = env.rig("lane-cancel", fast());
    let bytes = payload(48 * MIB, 16);
    let source = rig.source("big.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    eventually("some blocks over the lane", || {
        (board::lane_of(&id) == Some("fast, over Wi-Fi") && verified(&rig) >= 2).then_some(())
    });
    rig.a().cancel_transfer(&id, code::USER).unwrap();
    eventually("cancelled", || was_cancelled(&id).then_some(()));
    eventually("no partial file", || {
        names(&rig.downloads("b")).is_empty().then_some(())
    });
    eventually("no checkpoint", || {
        names(&rig.state("b")).is_empty().then_some(())
    });
    assert_eq!(board::lane_of(&id), None);
}

#[test]
fn two_transfers_at_once_each_get_a_lane() {
    let env = Env::new(Duration::from_millis(1));
    let rig = env.rig("lane-two", fast());
    let (one, two) = (payload(16 * MIB + 3, 17), payload(12 * MIB + 9, 18));
    let (path_one, path_two) = (rig.source("one.bin", &one), rig.source("two.bin", &two));
    rig.a().start_send(&path_one).unwrap();
    rig.a().start_send(&path_two).unwrap();
    let first = rig.received();
    let second = rig.received();
    let mut got = vec![
        fs::read(&first.path).unwrap(),
        fs::read(&second.path).unwrap(),
    ];
    got.sort_by_key(Vec::len);
    assert_eq!(got, [two, one]);
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 0);
    assert_eq!(env.proxy.connections.load(Ordering::SeqCst), 2);
}

#[test]
fn small_files_and_peers_without_a_lane_stay_on_the_data_channel() {
    let env = Env::new(Duration::ZERO);
    // Just under 8 MiB: no lane field at all.
    let rig = env.rig("lane-small", fast());
    let small = payload(8 * MIB - 1, 19);
    let source = rig.source("small.bin", &small);
    rig.a().start_send(&source).unwrap();
    assert_eq!(fs::read(rig.received().path).unwrap(), small);
    assert!(rig.ab.sent(FileAction::Offer)[0].lane.is_none());
    assert!(rig.ba.sent(FileAction::Accept)[0].lane.is_none());
    assert_eq!(env.proxy.connections.load(Ordering::SeqCst), 0);
    // Exactly 8 MiB: a lane.
    let rig = env.rig("lane-edge", fast());
    let edge = payload(8 * MIB, 20);
    let source = rig.source("edge.bin", &edge);
    rig.a().start_send(&source).unwrap();
    assert_eq!(fs::read(rig.received().path).unwrap(), edge);
    assert!(rig.ab.sent(FileAction::Offer)[0].lane.is_some());
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 0);
    // The receiver cannot take a lane (no listener): the accept says
    // nothing and the data channel carries it.
    file_lane::set_listen_port(0);
    let rig = env.rig("lane-nolistener", fast());
    let source = rig.source("edge.bin", &edge);
    rig.a().start_send(&source).unwrap();
    assert_eq!(fs::read(rig.received().path).unwrap(), edge);
    assert!(rig.ba.sent(FileAction::Accept)[0].lane.is_none());
    assert!(rig.ab.frames.load(Ordering::SeqCst) > 0);
}

#[test]
fn without_a_local_path_no_lane_is_offered_or_dialled() {
    // A relay or TURN pair, or a peer without the capability: the link
    // reports no lane target, which is all the engine looks at.
    let rig = Rig::new("lane-relay");
    let bytes = payload(20 * MIB, 21);
    let source = rig.source("big.bin", &bytes);
    rig.a().start_send(&source).unwrap();
    assert_eq!(fs::read(rig.received().path).unwrap(), bytes);
    assert!(rig.ab.sent(FileAction::Offer)[0].lane.is_none());
    assert!(rig.ba.sent(FileAction::Accept)[0].lane.is_none());
    assert!(rig.ab.sent(FileAction::Lane).is_empty());
    assert!(rig.ab.frames.load(Ordering::SeqCst) > 0);
}

/// Measured: the engine over a real TLS lane on loopback. The floor
/// (`DESKLINK_LANE_ENGINE_FLOOR`, MB/s, default 60) is enforced only with
/// `DESKLINK_BENCH=1` and the test run alone; in the parallel suite it
/// checks that the file arrived and the rate is printed.
#[test]
fn the_engine_over_a_loopback_lane_is_fast() {
    let env = Env::new(Duration::ZERO);
    let rig = env.rig("lane-bench", fast());
    let size = 160 * MIB;
    let bytes = payload(size, 22);
    let source = rig.source("bench.bin", &bytes);
    let started = Instant::now();
    rig.a().start_send(&source).unwrap();
    rig.received();
    let seconds = started.elapsed().as_secs_f64();
    let rate = size as f64 / 1e6 / seconds;
    eprintln!("[bench] engine over a loopback lane: {rate:.0} MB/s ({seconds:.2} s)");
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 0);
    let floor = file_lane::bench_floor("DESKLINK_LANE_ENGINE_FLOOR", 60.0);
    assert!(
        rate >= floor,
        "{rate:.0} MB/s is under the floor of {floor}"
    );
}
