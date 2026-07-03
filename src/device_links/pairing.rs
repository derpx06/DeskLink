use openssl::hash::{hash, MessageDigest};

use super::packet::{NetworkPacket, PACKET_TYPE_PAIR};

const ALLOWED_TIMESTAMP_DIFFERENCE_SECONDS: i64 = 30 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairState {
    NotPaired,
    Requested,
    RequestedByPeer,
    Paired,
}

#[derive(Debug, Clone)]
pub struct PairingHandler {
    pub state: PairState,
    timestamp: i64,
}

impl PairingHandler {
    pub fn new(is_paired: bool) -> Self {
        Self {
            state: if is_paired {
                PairState::Paired
            } else {
                PairState::NotPaired
            },
            timestamp: 0,
        }
    }

    pub fn request_packet(&mut self) -> NetworkPacket {
        if self.state == PairState::RequestedByPeer {
            return self.accept_packet();
        }
        self.state = PairState::Requested;
        self.timestamp = unix_secs();
        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", true);
        packet.set("timestamp", self.timestamp);
        packet
    }

    pub fn accept_packet(&mut self) -> NetworkPacket {
        self.state = PairState::Paired;
        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", true);
        packet
    }

    pub fn reject_packet(&mut self) -> NetworkPacket {
        self.state = PairState::NotPaired;
        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", false);
        packet
    }

    pub fn receive(&mut self, packet: &NetworkPacket) {
        if packet.packet_type != PACKET_TYPE_PAIR {
            return;
        }
        if packet.get_bool("pair") == Some(true) {
            match self.state {
                PairState::Requested => self.state = PairState::Paired,
                PairState::Paired => {
                    if let Some(timestamp) = packet.get_i64("timestamp") {
                        if (timestamp - unix_secs()).abs() > ALLOWED_TIMESTAMP_DIFFERENCE_SECONDS {
                            self.state = PairState::NotPaired;
                        } else {
                            self.timestamp = timestamp;
                            self.state = PairState::RequestedByPeer;
                        }
                    }
                }
                PairState::NotPaired => {
                    let Some(timestamp) = packet.get_i64("timestamp") else {
                        self.state = PairState::NotPaired;
                        return;
                    };
                    if (timestamp - unix_secs()).abs() > ALLOWED_TIMESTAMP_DIFFERENCE_SECONDS {
                        self.state = PairState::NotPaired;
                        return;
                    }
                    self.timestamp = timestamp;
                    self.state = PairState::RequestedByPeer;
                }
                PairState::RequestedByPeer => {}
            }
        } else {
            self.state = PairState::NotPaired;
        }
    }

    pub fn verification_key(
        local_public_der: &[u8],
        remote_public_der: &[u8],
        timestamp: i64,
    ) -> String {
        let mut first = local_public_der.to_vec();
        let mut second = remote_public_der.to_vec();
        if first < second {
            std::mem::swap(&mut first, &mut second);
        }
        let mut bytes = Vec::new();
        bytes.extend(first);
        bytes.extend(second);
        bytes.extend(timestamp.to_string().as_bytes());
        let digest = hash(MessageDigest::sha256(), &bytes).expect("sha256 is available");
        hex_upper(&digest)[..8].to_string()
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

fn hex_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02X}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_pair_becomes_paired_when_peer_accepts() {
        let mut handler = PairingHandler::new(false);
        handler.request_packet();

        let mut accepted = NetworkPacket::new(PACKET_TYPE_PAIR);
        accepted.set("pair", true);
        handler.receive(&accepted);

        assert_eq!(handler.state, PairState::Paired);
    }

    #[test]
    fn peer_pair_request_waits_for_local_acceptance() {
        let mut handler = PairingHandler::new(false);
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);
        request.set("timestamp", unix_secs());

        handler.receive(&request);

        assert_eq!(handler.state, PairState::RequestedByPeer);
        assert!(handler.timestamp() > 0);
    }

    #[test]
    fn requesting_pair_while_peer_request_is_pending_accepts_it() {
        let mut handler = PairingHandler::new(false);
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);
        request.set("timestamp", unix_secs());
        handler.receive(&request);

        let accept = handler.request_packet();

        assert_eq!(handler.state, PairState::Paired);
        assert_eq!(accept.get_bool("pair"), Some(true));
        assert_eq!(accept.get_i64("timestamp"), None);
    }

    #[test]
    fn peer_pair_request_without_timestamp_is_rejected() {
        let mut handler = PairingHandler::new(false);
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);

        handler.receive(&request);

        assert_eq!(handler.state, PairState::NotPaired);
    }

    #[test]
    fn verification_key_is_order_independent() {
        let first = PairingHandler::verification_key(b"aaa", b"bbb", 42);
        let second = PairingHandler::verification_key(b"bbb", b"aaa", 42);
        assert_eq!(first, second);
        assert_eq!(first.len(), 8);
    }

    #[test]
    fn paired_state_does_not_revert_on_packet_without_timestamp() {
        let mut handler = PairingHandler::new(true);
        assert_eq!(handler.state, PairState::Paired);

        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", true);
        handler.receive(&packet);

        assert_eq!(handler.state, PairState::Paired);
    }
}
