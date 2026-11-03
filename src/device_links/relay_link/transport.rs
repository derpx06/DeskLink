//! A relay link as a [`LinkTransport`], so everything above the link —
//! pairing state, heartbeats, signed signaling, the packet reader — works on
//! it unchanged.
//!
//! Inbound packets are pushed in by the relay client as newline-framed bytes;
//! outbound ones are handed to it to seal and send. Only bootstrap packets may
//! pass: features never travel over a relay link (docs/RELAY.md §3).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Condvar, Mutex};
use std::time::Duration;

use crate::device_links::core::{LinkTransport, ReadOutcome};
use crate::device_links::packet::NetworkPacket;

const IDLE_WAIT: Duration = Duration::from_millis(50);

/// A packet for the relay client to seal and send to `fp`.
pub struct Outbound {
    pub fp: String,
    pub plaintext: Vec<u8>,
}

pub struct RelayLinkTransport {
    fp: String,
    outbound: Mutex<Sender<Outbound>>,
    inbound: Mutex<VecDeque<u8>>,
    arrived: Condvar,
    closed: AtomicBool,
}

/// Whether `packet` may cross a relay link at all.
///
/// The rule is `pairing_transport::packet_may_cross`: only an unpair notice
/// of the pairing packets, until off-LAN pairing is compiled in and switched on.
pub fn allowed_on_relay(packet: &NetworkPacket) -> bool {
    crate::device_links::pairing_transport::admits(
        crate::device_links::pairing_transport::PairingTransportKind::Relay,
        packet,
    )
}

impl RelayLinkTransport {
    pub fn new(fp: String, outbound: Sender<Outbound>) -> Self {
        Self {
            fp,
            outbound: Mutex::new(outbound),
            inbound: Mutex::new(VecDeque::new()),
            arrived: Condvar::new(),
            closed: AtomicBool::new(false),
        }
    }

    /// Queues one decrypted packet for the reader.
    pub fn deliver(&self, packet: &[u8]) {
        if let Ok(mut inbound) = self.inbound.lock() {
            inbound.extend(packet);
            inbound.push_back(b'\n');
        }
        self.arrived.notify_all();
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }
}

impl LinkTransport for RelayLinkTransport {
    fn send_packet(&self, packet: &NetworkPacket) -> Result<(), String> {
        if self.is_closed() {
            return Err("Relay link is closed".to_string());
        }
        if !allowed_on_relay(packet) {
            return Err(format!(
                "{} may not travel over a relay link",
                packet.packet_type
            ));
        }
        let mut plaintext = packet.serialize_line().map_err(|error| error.to_string())?;
        while plaintext.last() == Some(&b'\n') {
            plaintext.pop();
        }
        self.outbound
            .lock()
            .map_err(|_| "Relay link lock poisoned".to_string())?
            .send(Outbound {
                fp: self.fp.clone(),
                plaintext,
            })
            .map_err(|_| "The relay connection is gone".to_string())
    }

    fn read_byte(&self) -> Result<ReadOutcome, String> {
        let mut inbound = self
            .inbound
            .lock()
            .map_err(|_| "Relay link lock poisoned".to_string())?;
        if let Some(byte) = inbound.pop_front() {
            return Ok(ReadOutcome::Byte(byte));
        }
        if self.is_closed() {
            return Ok(ReadOutcome::Closed);
        }
        let (mut inbound, _) = self
            .arrived
            .wait_timeout(inbound, IDLE_WAIT)
            .map_err(|_| "Relay link lock poisoned".to_string())?;
        Ok(match inbound.pop_front() {
            Some(byte) => ReadOutcome::Byte(byte),
            None if self.is_closed() => ReadOutcome::Closed,
            None => ReadOutcome::Idle,
        })
    }

    fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        self.arrived.notify_all();
    }

    fn pairing_transport(&self) -> crate::device_links::pairing_transport::PairingTransportKind {
        crate::device_links::pairing_transport::PairingTransportKind::Relay
    }

    // `local_ip` stays `None`: the relay is not the path media will take.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::packet::{PACKET_TYPE_HEARTBEAT, PACKET_TYPE_PAIR};
    use std::sync::mpsc::channel;

    #[test]
    fn a_relay_link_names_itself_to_the_pairing_state_machine() {
        let (tx, _rx) = channel();
        let link = RelayLinkTransport::new("fp".into(), tx);
        assert_eq!(
            link.pairing_transport(),
            crate::device_links::pairing_transport::PairingTransportKind::Relay
        );
    }

    #[test]
    fn only_bootstrap_packets_pass_and_bytes_frame_by_line() {
        let (tx, rx) = channel();
        let link = RelayLinkTransport::new("fp".into(), tx);
        assert!(link
            .send_packet(&NetworkPacket::new(PACKET_TYPE_HEARTBEAT))
            .is_ok());
        let sent = rx.try_recv().unwrap();
        assert_eq!(sent.fp, "fp");
        assert!(!sent.plaintext.ends_with(b"\n"));

        assert!(link
            .send_packet(&NetworkPacket::new(
                crate::device_links::packet::PACKET_TYPE_CLIPBOARD
            ))
            .is_err());
        let mut pair = NetworkPacket::new(PACKET_TYPE_PAIR);
        pair.set("pair", true);
        assert!(link.send_packet(&pair).is_err());
        pair.set("pair", false);
        assert!(link.send_packet(&pair).is_ok());

        link.deliver(b"ab");
        assert_eq!(link.read_byte(), Ok(ReadOutcome::Byte(b'a')));
        assert_eq!(link.read_byte(), Ok(ReadOutcome::Byte(b'b')));
        assert_eq!(link.read_byte(), Ok(ReadOutcome::Byte(b'\n')));
        assert_eq!(link.read_byte(), Ok(ReadOutcome::Idle));
        link.shutdown();
        assert_eq!(link.read_byte(), Ok(ReadOutcome::Closed));
        assert!(link
            .send_packet(&NetworkPacket::new(PACKET_TYPE_HEARTBEAT))
            .is_err());
    }
}
