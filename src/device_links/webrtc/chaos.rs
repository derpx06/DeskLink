//! A userspace network for fault-injection tests: no root, no `tc`.
//!
//! Every UDP host candidate a test peer gathers is replaced by the address of
//! a relay socket owned by [`Chaos`], so every ICE, DTLS and SCTP packet
//! between the two peers crosses it and can be lost, delayed (with jitter,
//! which reorders), duplicated or black-holed. A relay has one fixed side,
//! the peer whose candidate it stands for; whoever else sends to it is the
//! other side, and its answers go back there. TCP candidates are dropped so
//! nothing goes around the relays.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Faults {
    /// Chance of each packet being lost, 0..=1.
    pub(crate) loss: f64,
    /// Every packet waits this long, plus up to `jitter` more (so packets
    /// overtake each other).
    pub(crate) delay: Duration,
    pub(crate) jitter: Duration,
    /// Chance of a packet arriving twice.
    pub(crate) duplicate: f64,
    /// Nothing gets through, either way.
    pub(crate) blackhole: bool,
}

struct Delayed {
    due: Instant,
    seq: u64,
    socket: Arc<UdpSocket>,
    to: SocketAddr,
    bytes: Vec<u8>,
}

impl PartialEq for Delayed {
    fn eq(&self, other: &Self) -> bool {
        (self.due, self.seq) == (other.due, other.seq)
    }
}
impl Eq for Delayed {}
impl PartialOrd for Delayed {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Delayed {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (self.due, self.seq).cmp(&(other.due, other.seq))
    }
}

pub(crate) struct Chaos {
    faults: Arc<Mutex<Faults>>,
    stop: Arc<AtomicBool>,
    later: Mutex<Option<mpsc::Sender<Delayed>>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    rng: Arc<Mutex<u64>>,
    seq: Arc<AtomicU64>,
    pub(crate) passed: Arc<AtomicU64>,
    pub(crate) dropped: Arc<AtomicU64>,
}

impl Chaos {
    pub(crate) fn new(seed: u64) -> Arc<Self> {
        let (later, queue) = mpsc::channel::<Delayed>();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = Arc::clone(&stop);
        let scheduler = std::thread::spawn(move || {
            let mut heap = BinaryHeap::new();
            while !stopping.load(Ordering::Acquire) {
                let wait = heap.peek().map_or(
                    Duration::from_millis(50),
                    |Reverse(d): &Reverse<Delayed>| d.due.saturating_duration_since(Instant::now()),
                );
                match queue.recv_timeout(wait.min(Duration::from_millis(50))) {
                    Ok(delayed) => heap.push(Reverse(delayed)),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                let now = Instant::now();
                while heap.peek().is_some_and(|Reverse(d)| d.due <= now) {
                    let Reverse(d) = heap.pop().unwrap();
                    let _ = d.socket.send_to(&d.bytes, d.to);
                }
            }
        });
        Arc::new(Self {
            faults: Arc::new(Mutex::new(Faults::default())),
            stop,
            later: Mutex::new(Some(later)),
            threads: Mutex::new(vec![scheduler]),
            rng: Arc::new(Mutex::new(seed | 1)),
            seq: Arc::new(AtomicU64::new(0)),
            passed: Arc::new(AtomicU64::new(0)),
            dropped: Arc::new(AtomicU64::new(0)),
        })
    }

    pub(crate) fn set(&self, faults: Faults) {
        *self.faults.lock().unwrap() = faults;
    }

    pub(crate) fn blackhole(&self, on: bool) {
        self.faults.lock().unwrap().blackhole = on;
    }

    /// The candidate a peer should be told instead of `candidate`: the same
    /// one at a relay's address, or `None` for one that is not UDP.
    pub(crate) fn relay_candidate(&self, candidate: &str) -> Option<String> {
        let mut fields: Vec<String> = candidate.split(' ').map(str::to_string).collect();
        if fields.len() < 8 || !fields[2].eq_ignore_ascii_case("udp") {
            return None;
        }
        let upstream: SocketAddr = format!("{}:{}", fields[4], fields[5]).parse().ok()?;
        let socket = Arc::new(UdpSocket::bind("127.0.0.1:0").ok()?);
        socket
            .set_read_timeout(Some(Duration::from_millis(50)))
            .ok()?;
        let local = socket.local_addr().ok()?;
        fields[4] = local.ip().to_string();
        fields[5] = local.port().to_string();
        let later = self.later.lock().unwrap().clone()?;
        let (faults, stop, rng, seq, passed, dropped) = (
            Arc::clone(&self.faults),
            Arc::clone(&self.stop),
            Arc::clone(&self.rng),
            Arc::clone(&self.seq),
            Arc::clone(&self.passed),
            Arc::clone(&self.dropped),
        );
        let roll = move || {
            let mut x = rng.lock().unwrap();
            *x ^= *x << 13;
            *x ^= *x >> 7;
            *x ^= *x << 17;
            (*x >> 11) as f64 / (1u64 << 53) as f64
        };
        let thread = std::thread::spawn(move || {
            let mut other: Option<SocketAddr> = None;
            let mut buffer = vec![0u8; 65_536];
            while !stop.load(Ordering::Acquire) {
                let Ok((count, from)) = socket.recv_from(&mut buffer) else {
                    continue;
                };
                let to = if from == upstream {
                    match other {
                        Some(to) => to,
                        None => continue,
                    }
                } else {
                    other = Some(from);
                    upstream
                };
                let now = *faults.lock().unwrap();
                if now.blackhole || roll() < now.loss {
                    dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                passed.fetch_add(1, Ordering::Relaxed);
                let copies = if roll() < now.duplicate { 2 } else { 1 };
                for _ in 0..copies {
                    let wait = now.delay + now.jitter.mul_f64(roll());
                    if wait.is_zero() {
                        let _ = socket.send_to(&buffer[..count], to);
                    } else {
                        let _ = later.send(Delayed {
                            due: Instant::now() + wait,
                            seq: seq.fetch_add(1, Ordering::Relaxed),
                            socket: Arc::clone(&socket),
                            to,
                            bytes: buffer[..count].to_vec(),
                        });
                    }
                }
            }
        });
        self.threads.lock().unwrap().push(thread);
        Some(fields.join(" "))
    }
}

impl Drop for Chaos {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.later.lock().unwrap().take();
        for thread in self.threads.lock().unwrap().drain(..) {
            let _ = thread.join();
        }
    }
}

/// Removes the candidates written into a description, so every candidate
/// crosses [`Chaos::relay_candidate`] (they are trickled as well).
pub(crate) fn without_candidates(sdp: &str) -> String {
    sdp.split("\r\n")
        .filter(|line| !line.starts_with("a=candidate:") && *line != "a=end-of-candidates")
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// Two real peers (ICE, DTLS, SCTP) across a [`Chaos`] network.
#[cfg(test)]
mod scenarios {
    use super::*;
    use crate::device_links::webrtc::coordinator::{
        classify_peer_state, PeerStateAction, PEER_DEAD_AFTER, PEER_DEAD_AFTER_DOZING,
        PEER_PROBE_AFTER,
    };
    use crate::device_links::webrtc::peer_connection::tests::{
        loopback_through, process_counts, Loopback, HEAVY,
    };
    use crate::device_links::webrtc::peer_connection::{DesktopWebRtcPeer, PeerEvent};
    use crate::device_links::webrtc::WebRtcChannel;

    const REQUIRED: [WebRtcChannel; 3] = [
        WebRtcChannel::Control,
        WebRtcChannel::Events,
        WebRtcChannel::Liveness,
    ];

    /// What one side heard: each numbered events-channel message, and every
    /// connection state, with when.
    #[derive(Default)]
    struct Heard {
        numbers: Vec<(u32, Instant)>,
        states: Vec<(String, Instant)>,
    }

    struct Linked {
        offerer: Arc<DesktopWebRtcPeer>,
        answerer: Arc<DesktopWebRtcPeer>,
        heard: Arc<Mutex<Heard>>,
        offerer_states: Arc<Mutex<Heard>>,
        listeners: Vec<JoinHandle<()>>,
    }

    fn listen(events: mpsc::Receiver<PeerEvent>) -> (Arc<Mutex<Heard>>, JoinHandle<()>) {
        let heard = Arc::new(Mutex::new(Heard::default()));
        let writing = Arc::clone(&heard);
        let thread = std::thread::spawn(move || {
            while let Ok(event) = events.recv() {
                let now = Instant::now();
                let mut heard = writing.lock().unwrap();
                match event {
                    PeerEvent::Envelope {
                        channel: WebRtcChannel::Events,
                        bytes,
                        ..
                    } => {
                        if let Some(n) = std::str::from_utf8(&bytes)
                            .ok()
                            .and_then(|t| t.strip_prefix("n:"))
                            .and_then(|t| t.parse().ok())
                        {
                            heard.numbers.push((n, now));
                        }
                    }
                    PeerEvent::ConnectionChanged(state) => heard.states.push((state, now)),
                    PeerEvent::Closed => break,
                    _ => {}
                }
            }
        });
        (heard, thread)
    }

    /// Connects through `chaos`, or `None` when the channels did not open in
    /// `within` (the peers are closed).
    fn try_link(chaos: &Arc<Chaos>, within: Duration) -> Option<Linked> {
        let Loopback {
            offerer,
            answerer,
            offerer_events,
            answerer_events,
            ..
        } = loopback_through(chaos);
        let opened = |events: &mpsc::Receiver<PeerEvent>| {
            let mut pending = REQUIRED.to_vec();
            let deadline = Instant::now() + within;
            while !pending.is_empty() {
                match events.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(PeerEvent::ChannelOpened(channel)) => pending.retain(|c| *c != channel),
                    Ok(_) => {}
                    Err(_) => return false,
                }
            }
            true
        };
        if !(opened(&offerer_events) && opened(&answerer_events)) {
            offerer.close();
            answerer.close();
            return None;
        }
        let (offerer_states, a) = listen(offerer_events);
        let (heard, b) = listen(answerer_events);
        Some(Linked {
            offerer,
            answerer,
            heard,
            offerer_states,
            listeners: vec![a, b],
        })
    }

    fn link(chaos: &Arc<Chaos>) -> Linked {
        let linked = try_link(chaos, Duration::from_secs(30));
        linked.expect("the required channels never opened")
    }

    impl Linked {
        /// Sends `count` numbered messages from the offerer, one every `every`.
        fn send_numbers(&self, from: u32, count: u32, every: Duration) {
            for n in from..from + count {
                self.offerer
                    .send_text(WebRtcChannel::Events, &format!("n:{n}"))
                    .unwrap();
                std::thread::sleep(every);
            }
        }

        /// Waits until message `last` arrived; then every number must have
        /// arrived exactly once and in order. The longest gap between
        /// arrivals.
        fn all_arrived(&self, last: u32, within: Duration) -> Duration {
            let deadline = Instant::now() + within;
            loop {
                let heard = self.heard.lock().unwrap();
                if heard.numbers.last().is_some_and(|(n, _)| *n >= last) {
                    let numbers: Vec<u32> = heard.numbers.iter().map(|(n, _)| *n).collect();
                    assert_eq!(numbers, (0..=last).collect::<Vec<_>>(), "lost or reordered");
                    return heard
                        .numbers
                        .windows(2)
                        .map(|w| w[1].1 - w[0].1)
                        .max()
                        .unwrap_or_default();
                }
                let got = heard.numbers.len();
                drop(heard);
                assert!(
                    Instant::now() < deadline,
                    "only {got} of {} arrived",
                    last + 1
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        /// The first state on either side that the coordinator rebuilds on
        /// at once, and how long after `since`.
        fn first_rebuild_state(&self, since: Instant) -> Option<(String, Duration)> {
            [&self.heard, &self.offerer_states]
                .iter()
                .flat_map(|heard| heard.lock().unwrap().states.clone())
                .filter(|(state, at)| {
                    *at >= since && classify_peer_state(state) == PeerStateAction::Rebuild
                })
                .map(|(state, at)| (state, at - since))
                .min_by_key(|(_, after)| *after)
        }

        fn states(&self) -> Vec<String> {
            self.heard
                .lock()
                .unwrap()
                .states
                .iter()
                .map(|(s, _)| s.clone())
                .collect()
        }

        fn close(self) {
            self.offerer.close();
            self.answerer.close();
            for listener in self.listeners {
                let _ = listener.join();
            }
        }
    }

    /// 1 %, 5 % and 20 % loss with up to 200 ms of jitter (so packets also
    /// overtake each other): the reliable channels deliver every message once
    /// and in order, the link never reaches a state the coordinator rebuilds
    /// on, and the longest silence stays under the dead time (so a lossy but
    /// working link is never torn down by the liveness rule).
    #[test]
    fn under_loss_and_jitter_every_message_arrives_once_in_order() {
        for loss in [0.01, 0.05, 0.20] {
            let chaos = Chaos::new(7);
            let linked = link(&chaos);
            let started = Instant::now();
            chaos.set(Faults {
                loss,
                jitter: Duration::from_millis(200),
                duplicate: 0.01,
                ..Faults::default()
            });
            linked.send_numbers(0, 150, Duration::from_millis(40));
            let gap = linked.all_arrived(149, Duration::from_secs(60));
            eprintln!(
                "[chaos] loss {loss}: longest silence {gap:?}, {} dropped of {}",
                chaos.dropped.load(Ordering::Relaxed),
                chaos.dropped.load(Ordering::Relaxed) + chaos.passed.load(Ordering::Relaxed)
            );
            assert_eq!(linked.first_rebuild_state(started), None, "loss {loss}");
            assert!(gap < PEER_DEAD_AFTER, "loss {loss}: {gap:?} of silence");
            if loss <= 0.05 {
                assert!(gap < PEER_PROBE_AFTER, "loss {loss}: {gap:?} of silence");
            }
            linked.close();
        }
    }

    /// The path dies while the connection is being set up (offer sent,
    /// nothing back): the attempt ends inside the negotiation timeout, the
    /// way the coordinator ends it, and leaves nothing that stops the next
    /// one from connecting at once.
    #[test]
    fn a_cut_during_negotiation_gives_up_in_time_and_the_next_attempt_connects() {
        let chaos = Chaos::new(19);
        chaos.blackhole(true);
        let started = Instant::now();
        assert!(try_link(
            &chaos,
            crate::device_links::webrtc::coordinator::NEGOTIATION_TIMEOUT
        )
        .is_none());
        assert!(
            started.elapsed()
                < crate::device_links::webrtc::coordinator::NEGOTIATION_TIMEOUT
                    + Duration::from_secs(2)
        );
        chaos.blackhole(false);
        let linked = link(&chaos);
        linked.send_numbers(0, 3, Duration::from_millis(10));
        linked.all_arrived(2, Duration::from_secs(10));
        linked.close();
    }

    /// A hiccup shorter than the dead time (a Wi-Fi roam, a busy router):
    /// what was sent into it arrives in order once it ends, on the same
    /// connection, and nothing on the way asks for a rebuild.
    #[test]
    fn a_five_second_blackhole_heals_on_the_same_connection() {
        let chaos = Chaos::new(11);
        let linked = link(&chaos);
        linked.send_numbers(0, 5, Duration::from_millis(20));
        let hole = Instant::now();
        chaos.blackhole(true);
        linked.send_numbers(5, 10, Duration::from_millis(200));
        std::thread::sleep(Duration::from_secs(5).saturating_sub(hole.elapsed()));
        chaos.blackhole(false);
        let lifted = Instant::now();
        linked.send_numbers(15, 5, Duration::from_millis(20));
        linked.all_arrived(19, Duration::from_secs(20));
        eprintln!(
            "[chaos] 5 s hole: delivered {:?} after it lifted; states {:?}",
            lifted.elapsed(),
            linked.states()
        );
        assert_eq!(linked.first_rebuild_state(hole), None);
        linked.close();
    }

    /// A peer that answers nothing (a frozen phone, a dead Wi-Fi): the
    /// connection's own state never asks for a rebuild before the liveness
    /// rule's [`PEER_DEAD_AFTER`], so liveness is always the first judge of
    /// an awake phone. Measured: ICE consent freshness fails the connection
    /// at 30 s of silence, *before* [`PEER_DEAD_AFTER_DOZING`] (35 s), so a
    /// dozing phone's real allowance is 30 s (reported in
    /// docs/testing/CONNECTION-HARDENING.md; a failed connection cannot be
    /// revived, so waiting longer would buy nothing). Past it, a fresh
    /// connection through the same network comes up at once.
    #[test]
    #[ignore = "40 s; run with --ignored chaos"]
    fn chaos_a_forty_second_blackhole_is_judged_by_liveness_not_ice() {
        let chaos = Chaos::new(13);
        let linked = link(&chaos);
        let hole = Instant::now();
        chaos.blackhole(true);
        std::thread::sleep(Duration::from_secs(40));
        let rebuild = linked.first_rebuild_state(hole);
        eprintln!(
            "[chaos] 40 s hole: states {:?}, first rebuild state {rebuild:?}",
            linked.states()
        );
        assert!(
            rebuild
                .as_ref()
                .is_none_or(|(_, after)| *after >= PEER_DEAD_AFTER),
            "{rebuild:?}"
        );
        if rebuild
            .as_ref()
            .is_some_and(|(_, after)| *after < PEER_DEAD_AFTER_DOZING)
        {
            eprintln!(
                "[chaos] ICE gave up before the dozing allowance ({PEER_DEAD_AFTER_DOZING:?})"
            );
        }
        linked.close();
        chaos.blackhole(false);
        let started = Instant::now();
        let fresh = link(&chaos);
        eprintln!(
            "[chaos] fresh connection after the hole in {:?}",
            started.elapsed()
        );
        fresh.close();
    }

    /// Twenty cuts and restores in a minute: one connection carries on
    /// through all of them, nothing lost or reordered.
    #[test]
    #[ignore = "a minute; run with --ignored chaos"]
    fn chaos_twenty_flaps_in_a_minute_lose_nothing() {
        let chaos = Chaos::new(17);
        let linked = link(&chaos);
        let started = Instant::now();
        let flapping = Arc::clone(&chaos);
        let flapper = std::thread::spawn(move || {
            for _ in 0..20 {
                flapping.blackhole(true);
                std::thread::sleep(Duration::from_millis(1_500));
                flapping.blackhole(false);
                std::thread::sleep(Duration::from_millis(1_500));
            }
        });
        linked.send_numbers(0, 600, Duration::from_millis(100));
        flapper.join().unwrap();
        let gap = linked.all_arrived(599, Duration::from_secs(60));
        eprintln!(
            "[chaos] 20 flaps: longest silence {gap:?}, states {:?}",
            linked.states()
        );
        assert_eq!(linked.first_rebuild_state(started), None);
        assert!(gap < PEER_DEAD_AFTER, "{gap:?}");
        linked.close();
    }

    /// The soak: connect, carry traffic, suffer a random fault, tear down,
    /// over and over (`DESKLINK_SOAK_SECS`, default 30 minutes). Every round
    /// ends connected or cleanly given up, and the process's threads and
    /// descriptors come back to where they were.
    #[test]
    #[ignore = "soak; run with --ignored chaos_soak --nocapture"]
    fn chaos_soak_rebuilds_leak_nothing() {
        let _alone = HEAVY.lock().unwrap_or_else(|e| e.into_inner());
        let seconds: u64 = std::env::var("DESKLINK_SOAK_SECS")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(1_800);
        // One warm-up round: GStreamer's first use starts threads for good.
        link(&Chaos::new(1)).close();
        std::thread::sleep(Duration::from_secs(2));
        let (threads, fds) = process_counts();
        let end = Instant::now() + Duration::from_secs(seconds);
        let (mut rounds, mut ready, mut gave_up, mut peak) = (0u32, 0u32, 0u32, (0, 0));
        let mut seed = 99u64;
        while Instant::now() < end {
            rounds += 1;
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let chaos = Chaos::new(seed);
            let pick = (seed >> 33) % 4;
            chaos.set(Faults {
                loss: [0.0, 0.01, 0.05, 0.1][pick as usize],
                jitter: Duration::from_millis(50 * pick),
                ..Faults::default()
            });
            let Some(linked) = try_link(
                &chaos,
                crate::device_links::webrtc::coordinator::NEGOTIATION_TIMEOUT,
            ) else {
                gave_up += 1;
                continue;
            };
            ready += 1;
            linked.send_numbers(0, 20, Duration::from_millis(25));
            if pick == 3 {
                chaos.blackhole(true);
                std::thread::sleep(Duration::from_secs(2));
                chaos.blackhole(false);
            }
            linked.all_arrived(19, Duration::from_secs(30));
            linked.close();
            drop(chaos);
            let now = process_counts();
            peak = (peak.0.max(now.0), peak.1.max(now.1));
            if rounds % 25 == 0 {
                eprintln!(
                    "[soak] round {rounds}: {ready} ready, {gave_up} gave up; threads {} (start {threads}), fds {} (start {fds})",
                    now.0, now.1
                );
            }
        }
        std::thread::sleep(Duration::from_secs(3));
        let (after_threads, after_fds) = process_counts();
        eprintln!(
            "[soak] {rounds} rounds in {seconds} s: {ready} ready, {gave_up} gave up; threads {threads} -> {after_threads} (peak {}), fds {fds} -> {after_fds} (peak {})",
            peak.0, peak.1
        );
        assert_eq!(ready + gave_up, rounds);
        assert!(
            after_threads <= threads + 4,
            "threads {threads} -> {after_threads}"
        );
        assert!(after_fds <= fds + 8, "fds {fds} -> {after_fds}");
    }
}
