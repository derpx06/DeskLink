//! One peer's end-to-end relay link: the handshake, then sealed frames.
//!
//! Pure state: messages in, messages and events out, so both roles are
//! tested against each other without a relay or a socket. See [`super::crypto`]
//! for the wire format.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use openssl::pkey::{PKey, Private, Public};
use serde_json::{json, Value};

use super::crypto::{self, Ephemeral, LinkKeys, Role, Transcript, LINK_VERSION};

/// What the owner should do after feeding a message in.
#[derive(Debug, PartialEq, Eq)]
pub enum Event {
    /// Send this JSON to the peer through the relay.
    Send(Value),
    /// The link is up; `identity` is the peer's identity packet JSON, already
    /// checked to name the expected device.
    Established { identity: String },
    /// A decrypted DeskLink packet from the peer.
    Packet(Vec<u8>),
}

/// Who we are and who the peer must prove to be.
pub struct Identities {
    pub local_fp: String,
    pub peer_fp: String,
    pub local_key: PKey<Private>,
    /// Our identity packet, serialized.
    pub local_identity: String,
    /// The peer's key from its *stored pairing certificate*.
    pub peer_key: PKey<Public>,
    /// The device id the stored pairing names; the peer's identity must match.
    pub peer_device_id: String,
}

enum State {
    Idle,
    /// Initiator: sent `h1`, waiting for `h2`.
    AwaitingH2 {
        mine: Ephemeral,
    },
    /// Responder: sent `h2`, waiting for `fin`.
    AwaitingFin {
        keys: LinkKeys,
        transcript: Vec<u8>,
        identity: String,
    },
    Established {
        keys: LinkKeys,
    },
}

pub struct PeerLink {
    ids: Identities,
    role: Role,
    state: State,
    send_seq: u64,
    receive_seq: u64,
}

impl PeerLink {
    pub fn new(ids: Identities) -> Self {
        let role = if crypto::is_initiator(&ids.local_fp, &ids.peer_fp) {
            Role::Initiator
        } else {
            Role::Responder
        };
        Self {
            ids,
            role,
            state: State::Idle,
            send_seq: 0,
            receive_seq: 0,
        }
    }

    /// Whether this side opens handshakes (its fingerprint sorts lower).
    pub fn is_initiator(&self) -> bool {
        self.role == Role::Initiator
    }

    pub fn is_established(&self) -> bool {
        matches!(self.state, State::Established { .. })
    }

    /// Forgets the session, e.g. when the peer went offline.
    pub fn reset(&mut self) {
        self.state = State::Idle;
        self.send_seq = 0;
        self.receive_seq = 0;
    }

    /// Begins (or restarts) a handshake: the initiator sends `h1`, the
    /// responder asks the initiator to with `kick`.
    pub fn start(&mut self) -> Result<Value, String> {
        self.reset();
        if self.role == Role::Responder {
            return Ok(json!({"k": "kick"}));
        }
        let mine = Ephemeral::generate()?;
        let h1 = json!({
            "k": "h1",
            "lv": LINK_VERSION,
            "eph": BASE64.encode(&mine.public),
            "nonce": BASE64.encode(mine.nonce),
            "identity": self.ids.local_identity,
        });
        self.state = State::AwaitingH2 { mine };
        Ok(h1)
    }

    /// Seals one packet for the peer.
    pub fn seal(&mut self, plaintext: &[u8]) -> Result<Value, String> {
        let State::Established { keys } = &self.state else {
            return Err("Relay link is not established".to_string());
        };
        self.send_seq += 1;
        let ct = crypto::seal(&keys.send, self.send_seq, plaintext)?;
        Ok(json!({"k": "d", "seq": self.send_seq, "ct": ct}))
    }

    /// Handles one message from the peer. An `Err` means the session is
    /// broken; the owner restarts it (rate-limited) with [`Self::start`].
    pub fn receive(&mut self, message: &Value) -> Result<Vec<Event>, String> {
        let kind = message.get("k").and_then(Value::as_str).unwrap_or("");
        match (kind, self.role) {
            // A handshake already in flight answers the kick; restarting it
            // would cross the responder's reply to the first `h1`.
            ("kick", Role::Initiator) if matches!(self.state, State::AwaitingH2 { .. }) => {
                Ok(Vec::new())
            }
            ("kick", Role::Initiator) => Ok(vec![Event::Send(self.start()?)]),
            ("h1", Role::Responder) => self.on_h1(message),
            ("h2", Role::Initiator) => self.on_h2(message),
            ("fin", Role::Responder) => self.on_fin(message),
            ("d", _) => self.on_data(message),
            _ => Err(format!("Unexpected relay link message {kind:?}")),
        }
    }

    fn on_h1(&mut self, message: &Value) -> Result<Vec<Event>, String> {
        self.reset();
        check_version(message)?;
        let peer_eph = decode(message, "eph")?;
        let peer_nonce = decode(message, "nonce")?;
        let identity = self.checked_identity(message)?;
        let mine = Ephemeral::generate()?;
        let keys = crypto::derive_keys(
            &mine,
            &peer_eph,
            Role::Responder,
            &self.ids.peer_fp,
            &self.ids.local_fp,
            &peer_nonce,
            &mine.nonce,
        )?;
        let transcript = crypto::transcript(&Transcript {
            fp_initiator: &self.ids.peer_fp,
            fp_responder: &self.ids.local_fp,
            eph_initiator: &peer_eph,
            eph_responder: &mine.public,
            nonce_initiator: &peer_nonce,
            nonce_responder: &mine.nonce,
            identity_initiator: &identity,
            identity_responder: &self.ids.local_identity,
        });
        let sig = crypto::sign(&self.ids.local_key, Role::Responder, &transcript)?;
        let h2 = json!({
            "k": "h2",
            "lv": LINK_VERSION,
            "eph": BASE64.encode(&mine.public),
            "nonce": BASE64.encode(mine.nonce),
            "identity": self.ids.local_identity,
            "sig": sig,
        });
        self.state = State::AwaitingFin {
            keys,
            transcript,
            identity,
        };
        Ok(vec![Event::Send(h2)])
    }

    fn on_h2(&mut self, message: &Value) -> Result<Vec<Event>, String> {
        let State::AwaitingH2 { mine } = std::mem::replace(&mut self.state, State::Idle) else {
            return Err("Unexpected relay link h2".to_string());
        };
        check_version(message)?;
        let peer_eph = decode(message, "eph")?;
        let peer_nonce = decode(message, "nonce")?;
        let identity = self.checked_identity(message)?;
        let transcript = crypto::transcript(&Transcript {
            fp_initiator: &self.ids.local_fp,
            fp_responder: &self.ids.peer_fp,
            eph_initiator: &mine.public,
            eph_responder: &peer_eph,
            nonce_initiator: &mine.nonce,
            nonce_responder: &peer_nonce,
            identity_initiator: &self.ids.local_identity,
            identity_responder: &identity,
        });
        let sig = text(message, "sig")?;
        if !crypto::verify(&self.ids.peer_key, Role::Responder, &transcript, sig) {
            return Err("Relay link peer failed to prove its pairing key".to_string());
        }
        let keys = crypto::derive_keys(
            &mine,
            &peer_eph,
            Role::Initiator,
            &self.ids.local_fp,
            &self.ids.peer_fp,
            &mine.nonce,
            &peer_nonce,
        )?;
        let fin = json!({
            "k": "fin",
            "sig": crypto::sign(&self.ids.local_key, Role::Initiator, &transcript)?,
        });
        self.state = State::Established { keys };
        Ok(vec![Event::Send(fin), Event::Established { identity }])
    }

    fn on_fin(&mut self, message: &Value) -> Result<Vec<Event>, String> {
        let State::AwaitingFin {
            keys,
            transcript,
            identity,
        } = std::mem::replace(&mut self.state, State::Idle)
        else {
            return Err("Unexpected relay link fin".to_string());
        };
        if !crypto::verify(
            &self.ids.peer_key,
            Role::Initiator,
            &transcript,
            text(message, "sig")?,
        ) {
            return Err("Relay link peer failed to prove its pairing key".to_string());
        }
        self.state = State::Established { keys };
        Ok(vec![Event::Established { identity }])
    }

    fn on_data(&mut self, message: &Value) -> Result<Vec<Event>, String> {
        let State::Established { keys } = &self.state else {
            return Err("Relay link data before the handshake".to_string());
        };
        let seq = message
            .get("seq")
            .and_then(Value::as_u64)
            .ok_or("Relay frame has no sequence")?;
        // Strictly increasing: a replayed or reordered frame is refused, and
        // the relay delivers in order, so nothing legitimate is lost.
        if seq <= self.receive_seq {
            return Err("Replayed relay frame".to_string());
        }
        let plaintext = crypto::open(&keys.receive, seq, text(message, "ct")?)?;
        self.receive_seq = seq;
        Ok(vec![Event::Packet(plaintext)])
    }

    /// The peer's identity JSON, after checking it names the paired device.
    fn checked_identity(&self, message: &Value) -> Result<String, String> {
        let identity = text(message, "identity")?.to_string();
        let packet = crate::device_links::packet::NetworkPacket::deserialize(identity.as_bytes())
            .map_err(|_| "Relay link identity is not a packet".to_string())?;
        let info = crate::device_links::device_info::DeviceInfo::from_identity_packet(&packet)
            .ok_or("Relay link identity is invalid")?;
        if info.id != self.ids.peer_device_id {
            return Err("Relay link identity names another device".to_string());
        }
        Ok(identity)
    }
}

fn check_version(message: &Value) -> Result<(), String> {
    match message.get("lv").and_then(Value::as_u64) {
        Some(version) if version == u64::from(LINK_VERSION) => Ok(()),
        _ => Err("Unsupported relay link version".to_string()),
    }
}

fn text<'a>(message: &'a Value, key: &str) -> Result<&'a str, String> {
    message
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Relay link message has no {key}"))
}

fn decode(message: &Value, key: &str) -> Result<Vec<u8>, String> {
    BASE64
        .decode(text(message, key)?)
        .map_err(|_| format!("Relay link {key} is not base64"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device_info::DeviceInfo;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::nid::Nid;

    const PHONE: &str = "phone0000000000000000000000000000";
    const DESKTOP: &str = "desktop00000000000000000000000000";
    const OTHER: &str = "other000000000000000000000000000000";

    fn key() -> PKey<Private> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap()
    }

    fn public(key: &PKey<Private>) -> PKey<Public> {
        PKey::public_key_from_der(&key.public_key_to_der().unwrap()).unwrap()
    }

    fn identity(id: &str) -> String {
        let line = DeviceInfo::local(id.to_string(), id.to_string())
            .to_identity_packet(0)
            .serialize_line()
            .unwrap();
        String::from_utf8(line).unwrap().trim_end().to_string()
    }

    /// Phone ("AAA…", initiator) and desktop ("ZZZ…", responder).
    fn pair() -> (PeerLink, PeerLink, PKey<Private>) {
        let (phone_key, desktop_key) = (key(), key());
        let phone = PeerLink::new(Identities {
            local_fp: "AAA".into(),
            peer_fp: "ZZZ".into(),
            local_key: phone_key.clone(),
            local_identity: identity(PHONE),
            peer_key: public(&desktop_key),
            peer_device_id: DESKTOP.into(),
        });
        let desktop = PeerLink::new(Identities {
            local_fp: "ZZZ".into(),
            peer_fp: "AAA".into(),
            local_key: desktop_key,
            local_identity: identity(DESKTOP),
            peer_key: public(&phone_key),
            peer_device_id: PHONE.into(),
        });
        (phone, desktop, phone_key)
    }

    fn sent(events: &[Event]) -> Value {
        events
            .iter()
            .find_map(|event| match event {
                Event::Send(value) => Some(value.clone()),
                _ => None,
            })
            .unwrap()
    }

    fn handshake(phone: &mut PeerLink, desktop: &mut PeerLink) {
        let h1 = phone.start().unwrap();
        let h2 = sent(&desktop.receive(&h1).unwrap());
        let events = phone.receive(&h2).unwrap();
        assert!(
            matches!(&events[1], Event::Established { identity } if identity.contains(DESKTOP))
        );
        let events = desktop.receive(&sent(&events)).unwrap();
        assert!(matches!(&events[0], Event::Established { identity } if identity.contains(PHONE)));
    }

    #[test]
    fn both_sides_establish_and_exchange_packets_in_both_directions() {
        let (mut phone, mut desktop, _) = pair();
        handshake(&mut phone, &mut desktop);
        let frame = phone.seal(b"to desktop").unwrap();
        assert_eq!(
            desktop.receive(&frame).unwrap(),
            vec![Event::Packet(b"to desktop".to_vec())]
        );
        let frame = desktop.seal(b"to phone").unwrap();
        assert_eq!(
            phone.receive(&frame).unwrap(),
            vec![Event::Packet(b"to phone".to_vec())]
        );
        // A replayed frame is refused.
        assert!(phone.receive(&frame).is_err());
    }

    #[test]
    fn the_responder_asks_the_initiator_to_start() {
        let (mut phone, mut desktop, _) = pair();
        let kick = desktop.start().unwrap();
        assert_eq!(kick["k"], "kick");
        let h1 = sent(&phone.receive(&kick).unwrap());
        assert_eq!(h1["k"], "h1");
        // A second kick while that h1 is in flight is ignored, so the
        // responder's h2 still matches.
        assert!(phone.receive(&kick).unwrap().is_empty());
        let h2 = sent(&desktop.receive(&h1).unwrap());
        assert_eq!(h2["k"], "h2");
        assert!(phone.receive(&h2).is_ok());
    }

    #[test]
    fn a_relay_cannot_impersonate_either_side() {
        let (mut phone, mut desktop, _) = pair();
        // The relay answers the phone's h1 itself, with its own key.
        let h1 = phone.start().unwrap();
        let mut impostor = PeerLink::new(Identities {
            local_fp: "ZZZ".into(),
            peer_fp: "AAA".into(),
            local_key: key(),
            local_identity: identity(DESKTOP),
            peer_key: public(&key()),
            peer_device_id: PHONE.into(),
        });
        let forged = sent(&impostor.receive(&h1).unwrap());
        assert!(phone.receive(&forged).is_err());

        // An h1 claiming to be a different device is refused by the desktop.
        let (mut other, _, _) = pair();
        other.ids.local_identity = identity(OTHER);
        assert!(desktop.receive(&other.start().unwrap()).is_err());
    }

    #[test]
    fn a_tampered_h2_or_fin_fails() {
        let (mut phone, mut desktop, _) = pair();
        let h1 = phone.start().unwrap();
        let mut h2 = sent(&desktop.receive(&h1).unwrap());
        h2["nonce"] = json!(BASE64.encode([9u8; 32]));
        assert!(phone.receive(&h2).is_err());

        let (mut phone, mut desktop, _) = pair();
        let h2 = sent(&desktop.receive(&phone.start().unwrap()).unwrap());
        let mut fin = sent(&phone.receive(&h2).unwrap());
        fin["sig"] = json!(BASE64.encode([1u8; 64]));
        assert!(desktop.receive(&fin).is_err());
        assert!(!desktop.is_established());
    }

    #[test]
    fn a_new_h1_restarts_an_established_responder() {
        let (mut phone, mut desktop, _) = pair();
        handshake(&mut phone, &mut desktop);
        handshake(&mut phone, &mut desktop);
        let frame = phone.seal(b"after restart").unwrap();
        assert_eq!(frame["seq"], 1);
        assert!(desktop.receive(&frame).is_ok());
    }
}
