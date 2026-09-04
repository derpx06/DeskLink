//! A two-ended in-memory link for driving two peers against each other.
//!
//! Everything here runs on a **virtual clock**: nothing sleeps, so a test can
//! step a 30-second pairing timeout in a microsecond and assert on exactly what
//! crossed the wire.
//!
//! Reordering and duplication are **line-granular, not byte-granular**, and
//! deliberately so. Bytes on a stream cannot reorder; shuffling them would only
//! corrupt JSON and test the deserialiser's error path. What can genuinely
//! arrive out of order or twice is a whole framed packet.
//!
//! **Known ceiling.** [`TestLink::shutdown`] sets a flag, where
//! [`TlsLinkTransport`](crate::device_links::daemon::network) performs a real
//! `Shutdown::Both` that unblocks a parked reader on another thread. A
//! regression that turned the real shutdown into a flag would widen the
//! generation-replacement race and **this harness would not catch it**.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use super::link_transport::{LinkTransport, ReadOutcome};
use crate::device_links::packet::NetworkPacket;

struct InFlight {
    deliver_at_ms: u64,
    to: usize,
    line: Vec<u8>,
}

#[derive(Default)]
struct Inner {
    clock_ms: u64,
    queues: [VecDeque<u8>; 2],
    in_flight: Vec<InFlight>,
    dropped: [bool; 2],
    half_open: [bool; 2],
    delay_ms: u64,
    duplicate: bool,
    reorder: bool,
    sent: Vec<(usize, Vec<u8>)>,
}

pub(crate) struct TestNetwork {
    inner: Mutex<Inner>,
    /// Wall-clock anchor for the virtual clock.
    ///
    /// The pairing deadline is stamped from the **real** clock inside
    /// [`PairingHandler::request_packet`](crate::device_links::pairing::PairingHandler::request_packet),
    /// so a virtual clock starting at zero would sit half a century behind
    /// every deadline and expire nothing, ever. Anchoring to wall time at
    /// construction is what makes `advance(31_000)` actually cross a 30s
    /// pairing deadline — and it is why the harness needs no production seam.
    base_secs: i64,
}

impl TestNetwork {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner::default()),
            base_secs: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_secs() as i64)
                .unwrap_or_default(),
        })
    }

    /// The virtual clock as unix seconds, for anything that takes a `now`.
    ///
    /// Pairing timeouts are driven from this, so a test steps a 30s expiry with
    /// `advance(31_000)` in a microsecond and never sleeps.
    pub(crate) fn now_secs(&self) -> i64 {
        self.base_secs + (self.clock_ms() / 1000) as i64
    }

    /// One end of the link. `0` and `1` are the two peers.
    pub(crate) fn end(self: &Arc<Self>, index: usize) -> TestLink {
        assert!(index < 2, "a link has two ends");
        TestLink {
            network: Arc::clone(self),
            end: index,
        }
    }

    /// Moves virtual time forward and delivers everything now due.
    /// Returns the number of framed lines delivered.
    pub(crate) fn advance(&self, millis: u64) -> usize {
        let mut inner = self.inner.lock().expect("test network lock");
        inner.clock_ms += millis;
        let now = inner.clock_ms;

        let mut due: Vec<InFlight> = Vec::new();
        let mut still_waiting = Vec::new();
        for entry in inner.in_flight.drain(..) {
            if entry.deliver_at_ms <= now {
                due.push(entry);
            } else {
                still_waiting.push(entry);
            }
        }
        inner.in_flight = still_waiting;
        due.sort_by_key(|entry| entry.deliver_at_ms);

        let delivered = due.len();
        for entry in due {
            inner.queues[entry.to].extend(entry.line);
        }
        delivered
    }

    /// Closes one end. Its reader sees `Closed`; its sender fails with the text
    /// a real closed link produces.
    pub(crate) fn drop_end(&self, end: usize) {
        self.inner.lock().expect("test network lock").dropped[end] = true;
    }

    /// Accepts writes from `end` and never delivers them — a socket with no
    /// FIN, which is what an application-level heartbeat exists to detect.
    pub(crate) fn half_open(&self, end: usize) {
        self.inner.lock().expect("test network lock").half_open[end] = true;
    }

    /// Brings a dropped or half-open end back, as a **fresh** socket.
    ///
    /// Bytes queued for that end before the drop are discarded and in-flight
    /// lines addressed to it are abandoned, because a new TCP connection does
    /// not inherit the old one's buffers. Without this the harness could only
    /// ever tear a link down once, which is why reconnect — the single most
    /// common real-world event — was previously inexpressible.
    pub(crate) fn reconnect(&self, end: usize) {
        let mut inner = self.inner.lock().expect("test network lock");
        inner.dropped[end] = false;
        inner.half_open[end] = false;
        inner.queues[end].clear();
        inner.in_flight.retain(|entry| entry.to != end);
    }

    pub(crate) fn set_delay(&self, millis: u64) {
        self.inner.lock().expect("test network lock").delay_ms = millis;
    }

    pub(crate) fn set_duplicate(&self, on: bool) {
        self.inner.lock().expect("test network lock").duplicate = on;
    }

    pub(crate) fn set_reorder(&self, on: bool) {
        self.inner.lock().expect("test network lock").reorder = on;
    }

    /// Every line each end handed to the wire, delivered or not.
    pub(crate) fn sent_by(&self, end: usize) -> Vec<NetworkPacket> {
        self.inner
            .lock()
            .expect("test network lock")
            .sent
            .iter()
            .filter(|(from, _)| *from == end)
            .filter_map(|(_, line)| NetworkPacket::deserialize(line).ok())
            .collect()
    }

    pub(crate) fn clock_ms(&self) -> u64 {
        self.inner.lock().expect("test network lock").clock_ms
    }
}

pub(crate) struct TestLink {
    network: Arc<TestNetwork>,
    end: usize,
}

impl LinkTransport for TestLink {
    fn send_packet(&self, packet: &NetworkPacket) -> Result<(), String> {
        let line = packet.serialize_line().map_err(|err| err.to_string())?;
        let mut inner = self.network.inner.lock().expect("test network lock");
        if inner.dropped[self.end] {
            return Err("Session link is closed".to_string());
        }
        inner.sent.push((self.end, line.clone()));
        if inner.half_open[self.end] {
            return Ok(());
        }

        let to = 1 - self.end;
        let deliver_at_ms = inner.clock_ms + inner.delay_ms;
        let copies = if inner.duplicate { 2 } else { 1 };
        for _ in 0..copies {
            inner.in_flight.push(InFlight {
                deliver_at_ms,
                to,
                line: line.clone(),
            });
        }

        if inner.reorder {
            // Swap this line's delivery time with the previous one addressed to
            // the same peer, so the pair arrives back to front.
            let indices: Vec<usize> = inner
                .in_flight
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.to == to)
                .map(|(index, _)| index)
                .collect();
            if indices.len() >= 2 {
                // Swap positions, not delivery times: two lines sent in the
                // same virtual millisecond share a deadline, so swapping the
                // times would be a no-op. `advance` sorts stably, which means
                // vec order decides the tie — and this reverses it.
                let (previous, current) = (indices[indices.len() - 2], indices[indices.len() - 1]);
                inner.in_flight.swap(previous, current);
            }
        }
        Ok(())
    }

    fn read_byte(&self) -> Result<ReadOutcome, String> {
        let mut inner = self.network.inner.lock().expect("test network lock");
        match inner.queues[self.end].pop_front() {
            Some(byte) => Ok(ReadOutcome::Byte(byte)),
            None if inner.dropped[self.end] => Ok(ReadOutcome::Closed),
            None => Ok(ReadOutcome::Idle),
        }
    }

    fn shutdown(&self) {
        self.network.drop_end(self.end);
    }

    // `local_ip` stays `None`: this link is not an L3 path, and returning an
    // address here would be exactly the mistake the trait doc warns about.
    // It also means `ice_config_for` is exercised against a transport that has
    // no address at all.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::packet::PACKET_TYPE_PING;

    fn ping(message: &str) -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_PING);
        packet.set("message", message);
        packet
    }

    fn drain(link: &TestLink) -> Vec<NetworkPacket> {
        let mut packets = Vec::new();
        let mut line = Vec::new();
        while let ReadOutcome::Byte(byte) = link.read_byte().expect("read") {
            line.push(byte);
            if byte == b'\n' {
                if let Ok(packet) = NetworkPacket::deserialize(&line) {
                    packets.push(packet);
                }
                line.clear();
            }
        }
        packets
    }

    #[test]
    fn a_packet_crosses_to_the_other_end_and_nowhere_else() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));

        a.send_packet(&ping("hello")).unwrap();
        assert_eq!(net.advance(0), 1);
        assert!(drain(&a).is_empty(), "a sender never reads its own packet");
        let received = drain(&b);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].get_str("message"), Some("hello"));
    }

    #[test]
    fn nothing_is_delivered_before_its_delay_elapses() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));
        net.set_delay(200);

        a.send_packet(&ping("late")).unwrap();
        assert_eq!(net.advance(199), 0);
        assert!(drain(&b).is_empty());
        assert_eq!(net.advance(1), 1);
        assert_eq!(drain(&b).len(), 1);
        assert_eq!(net.clock_ms(), 200);
    }

    #[test]
    fn a_half_open_end_accepts_writes_that_never_arrive() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));
        net.half_open(0);

        a.send_packet(&ping("into the void")).unwrap();
        assert_eq!(net.advance(60_000), 0);
        assert!(drain(&b).is_empty());
        // The sender saw success and has no way to know. That is the point.
        assert_eq!(net.sent_by(0).len(), 1);
        assert_eq!(b.read_byte().unwrap(), ReadOutcome::Idle);
    }

    #[test]
    fn a_dropped_end_reports_closed_to_its_reader_and_fails_its_sender() {
        let net = TestNetwork::new();
        let a = net.end(0);
        net.drop_end(0);

        assert_eq!(a.read_byte().unwrap(), ReadOutcome::Closed);
        let error = a.send_packet(&ping("gone")).unwrap_err();
        assert!(
            !crate::device_links::transport::should_surface_send_error(&error),
            "a closed link is a transport transition, not a feature failure"
        );
    }

    #[test]
    fn shutdown_closes_only_the_end_that_called_it() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));

        a.shutdown();
        assert_eq!(a.read_byte().unwrap(), ReadOutcome::Closed);
        assert_eq!(b.read_byte().unwrap(), ReadOutcome::Idle);
    }

    #[test]
    fn a_reconnected_end_carries_none_of_the_dead_socket_s_bytes() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));

        a.send_packet(&ping("before the drop")).unwrap();
        net.advance(0);
        net.drop_end(1);
        // A closed socket still hands over what it already received; `Closed`
        // means "nothing more is coming", not "discard the buffer". Pinned
        // because the read loop depends on draining before it sees EOF.
        assert!(matches!(b.read_byte().unwrap(), ReadOutcome::Byte(_)));

        net.reconnect(1);
        assert_eq!(
            b.read_byte().unwrap(),
            ReadOutcome::Idle,
            "a fresh socket inherits no buffered bytes from the dead one"
        );
        a.send_packet(&ping("after")).unwrap();
        net.advance(0);
        let received = drain(&b);
        assert_eq!(received.len(), 1);
        assert_eq!(received[0].get_str("message"), Some("after"));
    }

    #[test]
    fn reconnect_clears_half_open_as_well_as_dropped() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));
        net.half_open(0);
        a.send_packet(&ping("swallowed")).unwrap();
        assert_eq!(net.advance(1_000), 0);

        net.reconnect(0);
        a.send_packet(&ping("delivered")).unwrap();
        assert_eq!(net.advance(0), 1);
        assert_eq!(drain(&b).len(), 1);
    }

    #[test]
    fn the_virtual_clock_is_anchored_to_wall_time() {
        let net = TestNetwork::new();
        let before = net.now_secs();
        // A zero-based clock would sit half a century behind every pairing
        // deadline and expire nothing. Anchoring is the whole point.
        let wall = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!((before - wall).abs() <= 1, "starts at wall time");

        net.advance(31_000);
        assert_eq!(net.now_secs(), before + 31, "and advances with the clock");
    }

    #[test]
    fn a_duplicated_line_is_delivered_twice_intact() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));
        net.set_duplicate(true);

        a.send_packet(&ping("twice")).unwrap();
        assert_eq!(net.advance(0), 2);
        let received = drain(&b);
        assert_eq!(received.len(), 2);
        assert!(received
            .iter()
            .all(|packet| packet.get_str("message") == Some("twice")));
    }

    #[test]
    fn reordering_swaps_whole_lines_and_never_corrupts_one() {
        let net = TestNetwork::new();
        let (a, b) = (net.end(0), net.end(1));
        net.set_delay(10);
        net.set_reorder(true);

        a.send_packet(&ping("first")).unwrap();
        a.send_packet(&ping("second")).unwrap();
        assert_eq!(net.advance(100), 2);

        let received = drain(&b);
        assert_eq!(received.len(), 2, "both lines arrive, whole");
        assert_eq!(received[0].get_str("message"), Some("second"));
        assert_eq!(received[1].get_str("message"), Some("first"));
    }
}
