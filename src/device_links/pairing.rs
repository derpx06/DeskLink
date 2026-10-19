use openssl::hash::{hash, MessageDigest};

use super::packet::{NetworkPacket, PACKET_TYPE_PAIR};

const ALLOWED_TIMESTAMP_DIFFERENCE_SECONDS: i64 = 30 * 60;

/// How long a pairing negotiation waits, in either direction.
///
/// **One value for both roles**, matching the 30s upstream KDE Connect and the
/// protocol documentation both specify. Until recently the desktop had no
/// pairing timeout at all, and the first fix used two constants — 30s for the
/// asking side, 25s for the asked — with a compile-time assert that the asker
/// must outlive the askee. That asymmetry existed only because expiry was
/// silent: if both sides gave up in the same second, each would report that the
/// other had failed.
///
/// `expire_pairings` now sends `pair:false` when it abandons a negotiation, so
/// the side that gives up first tells the other rather than both guessing. The
/// ordering trick is no longer load-bearing, and one constant is one fewer
/// thing to keep in step with Android.
pub const PAIR_TIMEOUT_SECS: i64 = 30;

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
    /// When the current negotiation stops waiting, in unix seconds.
    ///
    /// `None` in every settled state. Data rather than a timer thread: the
    /// reconciler already ticks once a second, and a deadline that is only ever
    /// compared against a caller-supplied `now` is trivially testable.
    deadline: Option<i64>,
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
            deadline: None,
        }
    }

    /// Builds the outgoing pair request.
    ///
    /// Requesting a pair from an already-paired session is refused. The Android
    /// application treats an incoming request from a trusted device as a hard
    /// unpair, so sending one made the phone drop its trusted-device record and
    /// certificate while this side still trusted the phone — leaving the two
    /// ends permanently disagreeing about whether they were paired.
    pub fn request_packet(&mut self) -> Result<NetworkPacket, String> {
        if self.state == PairState::Paired {
            return Err("Device is already paired".to_string());
        }
        if self.state == PairState::RequestedByPeer {
            return self.accept_packet();
        }
        // Re-sending a request must not restamp the negotiation. A peer already
        // sitting in `RequestedByPeer` ignores the duplicate and keeps the
        // original timestamp, so a fresh one here leaves the two users
        // comparing **different** 8-character keys for the same pairing — the
        // security model quietly failing for two honest people.
        //
        // The deadline *does* refresh: the user actively asked again.
        if self.state != PairState::Requested {
            self.timestamp = unix_secs();
        }
        self.state = PairState::Requested;
        self.deadline = Some(unix_secs() + PAIR_TIMEOUT_SECS);
        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", true);
        packet.set("timestamp", self.timestamp);
        Ok(packet)
    }

    /// Accepts a pair request that is actually outstanding.
    ///
    /// Refusing every other state is a security property, not tidiness. The
    /// verification key belongs to the link the prompt arrived on, and
    /// `register_link` abandons a negotiation when that link is replaced — so
    /// without this guard an acceptance the user granted to one peer could be
    /// applied to whichever connection happens to be current, writing *its*
    /// certificate into the trust store.
    pub fn accept_packet(&mut self) -> Result<NetworkPacket, String> {
        if self.state != PairState::RequestedByPeer {
            return Err("There is no pairing request to accept".to_string());
        }
        self.state = PairState::Paired;
        self.deadline = None;
        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", true);
        Ok(packet)
    }

    pub fn reject_packet(&mut self) -> NetworkPacket {
        self.state = PairState::NotPaired;
        self.deadline = None;
        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", false);
        packet
    }

    /// Applies an inbound `desklink.pair` packet.
    ///
    /// `local_goes_first` is the deterministic role tie-break from
    /// [`crate::device_links::local_goes_first`]. It is only consulted when both
    /// sides asked at once, and it is what stops a crossing request pairing two
    /// devices with no one having compared a key.
    pub fn receive(&mut self, packet: &NetworkPacket, local_goes_first: bool) {
        if packet.packet_type != PACKET_TYPE_PAIR {
            return;
        }
        if packet.get_bool("pair") == Some(true) {
            match self.state {
                // The discriminator is the timestamp, exactly as it is
                // everywhere else: a *request* carries one, an *acceptance*
                // does not.
                PairState::Requested => match packet.get_i64("timestamp") {
                    // The peer answered us. This is the ordinary happy path.
                    None => {
                        self.state = PairState::Paired;
                        self.deadline = None;
                    }
                    // Both sides asked at once. Treating this as an acceptance
                    // — which is what the code did — completed the pairing with
                    // **neither user ever shown a verification key**, discarding
                    // the only defence against a man in the middle during the
                    // window where TLS still trusts anything. It also left each
                    // side keyed off its own timestamp, so the two screens would
                    // have disagreed had anyone looked.
                    //
                    // Resolve it by role instead. The leader's request stands;
                    // the follower abandons its own and adopts the leader's
                    // negotiation, timestamp included. Exactly one prompt
                    // exists, and both sides derive the same key from it.
                    Some(timestamp) => {
                        if !local_goes_first && self.timestamp_is_plausible(timestamp) {
                            self.timestamp = timestamp;
                            self.state = PairState::RequestedByPeer;
                            self.deadline = Some(unix_secs() + PAIR_TIMEOUT_SECS);
                        }
                    }
                },
                // A pair request from a device this side already trusts is a
                // re-confirmation, not a new negotiation. Dropping to
                // `RequestedByPeer` silently disabled every paired feature —
                // the dispatcher and the WebRTC handover both require `Paired` —
                // until the user happened to notice a second pair prompt.
                //
                // Staying paired is also what keeps the two applications in
                // agreement: the request is answered with a plain acceptance
                // below, which the peer's `Requested` state resolves to
                // `Paired`.
                PairState::Paired => {}
                PairState::NotPaired => {
                    let Some(timestamp) = packet.get_i64("timestamp") else {
                        self.state = PairState::NotPaired;
                        return;
                    };
                    if !self.timestamp_is_plausible(timestamp) {
                        self.state = PairState::NotPaired;
                        return;
                    }
                    self.timestamp = timestamp;
                    self.state = PairState::RequestedByPeer;
                    self.deadline = Some(unix_secs() + PAIR_TIMEOUT_SECS);
                }
                PairState::RequestedByPeer => {}
            }
        } else {
            self.state = PairState::NotPaired;
            self.deadline = None;
        }
    }

    /// Whether a peer's request timestamp is close enough to this clock to be
    /// part of a live negotiation rather than a replay.
    fn timestamp_is_plausible(&self, timestamp: i64) -> bool {
        (timestamp - unix_secs()).abs() <= ALLOWED_TIMESTAMP_DIFFERENCE_SECONDS
    }

    /// Abandons a negotiation the peer never answered.
    ///
    /// Returns whether the state changed, so the caller can update the device
    /// view and clear the prompt. `now_secs` is a parameter rather than a read
    /// of the clock: that is the entire testing seam.
    pub fn expire_if_due(&mut self, now_secs: i64) -> bool {
        let Some(deadline) = self.deadline else {
            return false;
        };
        if now_secs < deadline {
            return false;
        }
        if !matches!(
            self.state,
            PairState::Requested | PairState::RequestedByPeer
        ) {
            // Settled while the deadline was still set; nothing to abandon.
            self.deadline = None;
            return false;
        }
        self.state = PairState::NotPaired;
        self.deadline = None;
        true
    }

    #[cfg(test)]
    pub fn deadline(&self) -> Option<i64> {
        self.deadline
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
        six_digits(&digest)
    }

    pub fn timestamp(&self) -> i64 {
        self.timestamp
    }
}

/// A six-digit code both screens show during pairing: the first four digest
/// bytes, big-endian, mod 10^6, zero-padded. Digits read aloud and compare more
/// reliably than hex, and the code is never truncated on either side.
fn six_digits(digest: &[u8]) -> String {
    let value = u32::from_be_bytes([digest[0], digest[1], digest[2], digest[3]]);
    format!("{:06}", value % 1_000_000)
}

/// Whether an incoming pair packet needs an acknowledgement sent back.
///
/// A trusted peer that re-sends its request stays paired on this side, so it
/// never reaches the normal accept path and would otherwise wait out its own
/// timeout and report a failure against a perfectly healthy session.
///
/// Only a *request* is answered. A request carries a `timestamp`; a plain
/// acceptance does not. Testing for it is what makes this terminate — answering
/// every `pair: true` would answer the peer's own acknowledgement and ping-pong
/// forever, which is precisely the loop that made the pre-v8 protocol unpair
/// here instead of replying.
pub fn needs_pair_reconfirmation(
    previous_state: PairState,
    current_state: PairState,
    packet: &NetworkPacket,
) -> bool {
    previous_state == PairState::Paired
        && current_state == PairState::Paired
        && packet.get_bool("pair") == Some(true)
        && packet.get_i64("timestamp").is_some()
}

fn unix_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requested_pair_becomes_paired_when_peer_accepts() {
        let mut handler = PairingHandler::new(false);
        handler.request_packet().unwrap();

        let mut accepted = NetworkPacket::new(PACKET_TYPE_PAIR);
        accepted.set("pair", true);
        handler.receive(&accepted, false);

        assert_eq!(handler.state, PairState::Paired);
    }

    #[test]
    fn peer_pair_request_waits_for_local_acceptance() {
        let mut handler = PairingHandler::new(false);
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);
        request.set("timestamp", unix_secs());

        handler.receive(&request, false);

        assert_eq!(handler.state, PairState::RequestedByPeer);
        assert!(handler.timestamp() > 0);
    }

    #[test]
    fn requesting_pair_while_peer_request_is_pending_accepts_it() {
        let mut handler = PairingHandler::new(false);
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);
        request.set("timestamp", unix_secs());
        handler.receive(&request, false);

        let accept = handler.request_packet().unwrap();

        assert_eq!(handler.state, PairState::Paired);
        assert_eq!(accept.get_bool("pair"), Some(true));
        assert_eq!(accept.get_i64("timestamp"), None);
    }

    /// Android treats an incoming pair request from a trusted device as a hard
    /// unpair, so this side must never send one while it still trusts the peer.
    #[test]
    fn requesting_a_pair_on_an_already_paired_session_is_refused() {
        let mut handler = PairingHandler::new(true);

        assert_eq!(
            handler.request_packet().unwrap_err(),
            "Device is already paired"
        );
        assert_eq!(handler.state, PairState::Paired);
    }

    /// A trusted peer re-sending its request must not knock this side out of
    /// `Paired`: every feature gate and the WebRTC handover depend on it.
    #[test]
    fn a_repeated_pair_request_from_a_trusted_peer_keeps_the_pairing() {
        let mut handler = PairingHandler::new(true);
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);
        request.set("timestamp", unix_secs());

        handler.receive(&request, false);

        assert_eq!(handler.state, PairState::Paired);
    }

    /// The acknowledgement exchange must terminate. A request carries a
    /// timestamp and is answered; the answer carries none and is not.
    #[test]
    fn only_a_pair_request_is_acknowledged_so_the_exchange_terminates() {
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);
        request.set("timestamp", unix_secs());
        assert!(needs_pair_reconfirmation(
            PairState::Paired,
            PairState::Paired,
            &request
        ));

        // The reply this produces must not itself provoke another reply.
        let mut acknowledgement = NetworkPacket::new(PACKET_TYPE_PAIR);
        acknowledgement.set("pair", true);
        assert!(!needs_pair_reconfirmation(
            PairState::Paired,
            PairState::Paired,
            &acknowledgement
        ));
    }

    #[test]
    fn a_normal_pairing_handshake_is_never_acknowledged_twice() {
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);
        request.set("timestamp", unix_secs());

        // Fresh pairing: the accept path already answers, so this must not.
        assert!(!needs_pair_reconfirmation(
            PairState::NotPaired,
            PairState::RequestedByPeer,
            &request
        ));
        assert!(!needs_pair_reconfirmation(
            PairState::Requested,
            PairState::Paired,
            &request
        ));

        // An unpair is never acknowledged with a pair.
        let mut unpair = NetworkPacket::new(PACKET_TYPE_PAIR);
        unpair.set("pair", false);
        unpair.set("timestamp", unix_secs());
        assert!(!needs_pair_reconfirmation(
            PairState::Paired,
            PairState::NotPaired,
            &unpair
        ));
    }

    #[test]
    fn an_explicit_unpair_from_a_trusted_peer_is_still_honoured() {
        let mut handler = PairingHandler::new(true);
        let mut unpair = NetworkPacket::new(PACKET_TYPE_PAIR);
        unpair.set("pair", false);

        handler.receive(&unpair, false);

        assert_eq!(handler.state, PairState::NotPaired);
    }

    #[test]
    fn peer_pair_request_without_timestamp_is_rejected() {
        let mut handler = PairingHandler::new(false);
        let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
        request.set("pair", true);

        handler.receive(&request, false);

        assert_eq!(handler.state, PairState::NotPaired);
    }

    #[test]
    fn an_unanswered_request_expires_instead_of_waiting_forever() {
        let mut handler = PairingHandler::new(false);
        let packet = handler.request_packet().unwrap();
        let sent_at = packet.get_i64("timestamp").unwrap();
        assert_eq!(handler.state, PairState::Requested);

        assert!(!handler.expire_if_due(sent_at + super::PAIR_TIMEOUT_SECS - 1));
        assert_eq!(handler.state, PairState::Requested);

        assert!(handler.expire_if_due(sent_at + super::PAIR_TIMEOUT_SECS));
        assert_eq!(handler.state, PairState::NotPaired);
        // Idempotent: a second tick has nothing left to abandon.
        assert!(!handler.expire_if_due(sent_at + 10_000));
    }

    #[test]
    /// Both roles now wait the same 30s, and the prompt's clock starts when the
    /// request *arrives*, so the asked side always outlives the asking side
    /// without needing two constants ordered against each other.
    ///
    /// The asymmetry this replaces existed only because expiry was silent.
    /// `expire_pairings` now sends `pair:false`, so whoever gives up first
    /// tells the other; see the two-peer coverage in `test_peer`.
    fn a_prompt_and_the_request_that_caused_it_wait_the_same_time() {
        let mut asker = PairingHandler::new(false);
        let request = asker.request_packet().unwrap();
        let mut asked = PairingHandler::new(false);
        asked.receive(&request, false);
        assert_eq!(asked.state, PairState::RequestedByPeer);

        let asker_deadline = asker.deadline().expect("a request has a deadline");
        let asked_deadline = asked.deadline().expect("a prompt has a deadline");
        assert!(
            asked_deadline >= asker_deadline,
            "the side that was asked cannot give up before the side that asked"
        );

        assert!(asked.expire_if_due(asked_deadline));
        assert_eq!(asked.state, PairState::NotPaired);
    }

    #[test]
    fn a_completed_pairing_has_no_deadline_left_to_fire() {
        let mut asker = PairingHandler::new(false);
        let request = asker.request_packet().unwrap();
        let mut asked = PairingHandler::new(false);
        asked.receive(&request, false);
        let acceptance = asked.accept_packet().expect("a prompt is outstanding");
        asker.receive(&acceptance, false);

        assert_eq!(asker.state, PairState::Paired);
        assert_eq!(asked.state, PairState::Paired);
        assert_eq!(asker.deadline(), None);
        assert_eq!(asked.deadline(), None);
        // A tick long after the fact must not unpair a healthy pairing.
        assert!(!asker.expire_if_due(i64::MAX / 2));
        assert!(!asked.expire_if_due(i64::MAX / 2));
        assert_eq!(asker.state, PairState::Paired);
    }

    #[test]
    fn verification_key_is_order_independent() {
        let first = PairingHandler::verification_key(b"aaa", b"bbb", 42);
        let second = PairingHandler::verification_key(b"bbb", b"aaa", 42);
        assert_eq!(first, second);
        // Pinned on Android too (PairingHandlerTest): the first four digest
        // bytes, big-endian, mod 10^6.
        assert_eq!(first, "087037");
    }

    #[test]
    fn paired_state_does_not_revert_on_packet_without_timestamp() {
        let mut handler = PairingHandler::new(true);
        assert_eq!(handler.state, PairState::Paired);

        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        packet.set("pair", true);
        handler.receive(&packet, false);

        assert_eq!(handler.state, PairState::Paired);
    }
}
