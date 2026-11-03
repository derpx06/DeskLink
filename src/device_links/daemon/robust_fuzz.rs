//! Hostile-input tests for the real parse and dispatch entry points.
//!
//! Everything a paired or anonymous peer can put on the wire is run through
//! the production parser or dispatcher with random bytes, mutated valid
//! messages and random JSON of the wrong shape. A test passes when nothing
//! panics and nothing allocates without bound. The generator is a fixed-seed
//! xorshift, so a failure reproduces; set `DESKLINK_FUZZ_SEED` to explore.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};

use serde_json::{json, Map, Value};

use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, SessionLink};
use crate::device_links::packet::*;
use crate::device_links::webrtc::file_lane;
use crate::device_links::webrtc::file_protocol::{parse_frame, FileControl};
use crate::device_links::webrtc::remote_session::{ControlMessage, RemoteSession};
use crate::device_links::webrtc::terminal_protocol::{TerminalChunk, TerminalControl};
use crate::device_links::webrtc::{
    HandoverControlMessage, HandoverRuntime, WebRtcEnvelope, WebRtcPacketBridge,
    WebRtcSignalingMessage, WebRtcWireBinding, MAX_ENVELOPE_BYTES,
};

const ROUNDS: usize = 3000;

struct Rng(u64);

impl Rng {
    fn new(salt: u64) -> Self {
        let seed = std::env::var("DESKLINK_FUZZ_SEED")
            .ok()
            .and_then(|seed| seed.parse::<u64>().ok())
            .unwrap_or(0x5eed_1234_abcd_ef01);
        Self(seed ^ salt.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn bytes(&mut self, max: usize) -> Vec<u8> {
        let len = self.below(max + 1);
        (0..len).map(|_| self.next() as u8).collect()
    }

    /// A JSON value of any type, the nasty ones weighted in.
    fn value(&mut self, depth: usize) -> Value {
        match self.below(if depth > 3 { 9 } else { 12 }) {
            0 => Value::Null,
            1 => Value::Bool(self.next() & 1 == 0),
            2 => json!(i64::MIN),
            3 => json!(i64::MAX),
            4 => json!(u64::MAX),
            5 => json!(self.next() as i64),
            6 => json!(-1),
            7 => json!(f64::from_bits(self.next()).to_string()),
            8 => Value::String(
                [
                    "",
                    "..",
                    "../x",
                    "a\0b",
                    "\u{202e}x",
                    "é",
                    "0",
                    "true",
                    "null",
                ][self.below(9)]
                .to_string(),
            ),
            9 => Value::Array((0..self.below(4)).map(|_| self.value(depth + 1)).collect()),
            10 => Value::String("x".repeat(self.below(70_000))),
            _ => Value::Object(self.object(depth + 1)),
        }
    }

    fn object(&mut self, depth: usize) -> Map<String, Value> {
        const KEYS: &[&str] = &[
            "text",
            "id",
            "ticker",
            "title",
            "appName",
            "isClearable",
            "requestReplyId",
            "message",
            "events",
            "contacts",
            "uids",
            "timestamp",
            "isCancel",
            "silent",
            "payload",
            "op",
            "items",
            "rev",
            "have",
            "peerInstance",
            "state",
            "name",
            "number",
            "event",
            "ok",
            "reason",
            "token",
            "transferId",
            "pass",
            "startBlock",
            "deviceId",
            "sessionId",
            "connectionGeneration",
            "sessionAttemptId",
            "messageType",
            "signature",
            "kind",
            "action",
            "filename",
            "totalSize",
            "blockSize",
            "fileHash",
            "relPath",
            "password",
            "screenWidth",
            "screenHeight",
            "fps",
            "maxDimension",
            "rows",
            "cols",
            "player",
            "nowPlaying",
            "length",
            "pos",
            "isPlaying",
            "volume",
            "currentCharge",
            "isCharging",
            "thresholdEvent",
            "dozing",
            "setLocked",
            "nonce",
            "key",
            "value",
            "url",
        ];
        (0..self.below(12))
            .map(|_| {
                let key = if self.below(5) == 0 {
                    String::from_utf8_lossy(&self.bytes(12)).into_owned()
                } else {
                    KEYS[self.below(KEYS.len())].to_string()
                };
                (key, self.value(depth))
            })
            .collect()
    }

    /// `base` with a few bytes flipped, dropped, repeated or cut.
    fn mutate(&mut self, base: &[u8]) -> Vec<u8> {
        let mut out = base.to_vec();
        for _ in 0..1 + self.below(4) {
            if out.is_empty() {
                break;
            }
            let at = self.below(out.len());
            match self.below(5) {
                0 => out[at] ^= 1 << self.below(8),
                1 => out[at] = self.next() as u8,
                2 => {
                    out.remove(at);
                }
                3 => out.truncate(at),
                _ => {
                    let byte = out[at];
                    out.insert(at, byte);
                }
            }
        }
        out
    }
}

fn wire() -> WebRtcWireBinding {
    WebRtcWireBinding::from_attempt("desk", "phone", "01234567-89ab-cdef-0123-456789abcdef")
        .unwrap()
}

fn phone_wire() -> WebRtcWireBinding {
    WebRtcWireBinding {
        sender_device_id: "phone".to_string(),
        peer_device_id: "desk".to_string(),
        ..wire()
    }
}

#[test]
fn random_bytes_never_panic_the_packet_and_envelope_parsers() {
    let mut rng = Rng::new(1);
    let valid = WebRtcPacketBridge::encode(
        &phone_wire(),
        &NetworkPacket::new(PACKET_TYPE_PING),
        1_700_000_000_000,
    )
    .unwrap()
    .to_json()
    .unwrap();
    for round in 0..ROUNDS {
        let bytes = if round % 2 == 0 {
            rng.bytes(2048)
        } else {
            rng.mutate(&valid)
        };
        let _ = NetworkPacket::deserialize(&bytes);
        if let Ok(envelope) = WebRtcEnvelope::from_json(&bytes) {
            let _ = envelope.validate(&wire());
            let _ = WebRtcPacketBridge::decode(&wire(), &envelope);
        }
    }
    // Nesting far past serde_json's recursion limit is an error, not a stack
    // overflow; so is a size over the envelope bound.
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    assert!(NetworkPacket::deserialize(deep.as_bytes()).is_err());
    assert!(WebRtcEnvelope::from_json(&vec![b' '; MAX_ENVELOPE_BYTES + 1]).is_err());
}

#[test]
fn random_json_never_panics_the_typed_message_parsers() {
    let mut rng = Rng::new(2);
    let mut session = RemoteSession::default();
    for _ in 0..ROUNDS {
        let bytes = serde_json::to_vec(&Value::Object(rng.object(0))).unwrap();
        let bytes = if rng.below(3) == 0 {
            rng.mutate(&bytes)
        } else {
            bytes
        };
        if let Ok(message) = ControlMessage::from_json(&bytes) {
            let _ = message.validate(&phone_wire(), "attempt", 1_700_000_000_000);
            let _ = session.accept(&message, 1_700_000_000_000);
        }
        if let Ok(message) = FileControl::from_json(&bytes) {
            let _ = message.validate(&phone_wire());
        }
        if let Ok(message) = TerminalControl::from_json(&bytes) {
            let _ = message.validate(&phone_wire());
        }
        if let Ok(message) = HandoverControlMessage::from_json(&bytes) {
            let _ = message.validate(&phone_wire(), "attempt", 1_700_000_000_000);
        }
    }
}

#[test]
fn a_remote_session_survives_a_hostile_message_sequence() {
    // Well-formed control messages in a random order and with random fields:
    // the state machine may refuse, never panic.
    let mut rng = Rng::new(3);
    let kinds = [
        "view-request",
        "view-granted",
        "view-denied",
        "screen-ready",
        "screen-error",
        "control-request",
        "control-granted",
        "control-denied",
        "control-revoked",
        "session-stop",
        "heartbeat",
        "input-lease",
        "audio-start",
        "audio-stop",
    ];
    let mut session = RemoteSession::default();
    for _ in 0..ROUNDS {
        let mut body = rng.object(0);
        body.insert("remoteSessionVersion".into(), json!(1));
        body.insert("kind".into(), json!(kinds[rng.below(kinds.len())]));
        body.insert("sessionAttemptId".into(), json!("attempt"));
        body.insert("deviceId".into(), json!("phone"));
        body.insert(
            "remoteSessionId".into(),
            json!(format!("s{}", rng.below(3))),
        );
        if let Ok(message) = ControlMessage::from_json(&serde_json::to_vec(&body).unwrap()) {
            let _ = message.validate(&phone_wire(), "attempt", 0);
            let _ = session.accept(&message, rng.next() as i64);
        }
    }
}

#[test]
fn hostile_file_frames_and_terminal_chunks_never_panic() {
    let mut rng = Rng::new(4);
    for round in 0..ROUNDS * 3 {
        let mut frame = if round % 2 == 0 {
            rng.bytes(300)
        } else {
            // A header that looks right, with random numbers behind it.
            let mut frame = b"DLF2".to_vec();
            frame.extend_from_slice(&[(rng.below(2)) as u8, rng.next() as u8, 0, 0]);
            frame.extend(rng.bytes(60));
            frame
        };
        if round % 7 == 0 {
            frame = rng.mutate(&frame);
        }
        if let Ok(parsed) = parse_frame(&frame) {
            assert!(parsed.payload.len() <= frame.len());
            let _ = parsed.header.check(rng.next());
            let _ = parsed.header.check(u64::MAX);
            let _ = parsed.header.check(0);
        }
        let mut chunk = b"DLT1".to_vec();
        chunk.extend(rng.bytes(200));
        let _ = TerminalChunk::decode(&chunk);
        let _ = TerminalChunk::decode(&frame);
    }
}

#[test]
fn hostile_lane_lines_never_panic_and_are_bounded() {
    let mut rng = Rng::new(5);
    for _ in 0..ROUNDS {
        let line = serde_json::to_vec(&Value::Object(rng.object(0))).unwrap();
        let _ = file_lane::parse_token_line(&line);
        let _ = file_lane::parse_ready(&line);
        let _ = file_lane::parse_token_line(&rng.bytes(400));
        let mut packet = NetworkPacket::new(file_lane::FIRST_LINE_TYPE);
        packet.body = rng.object(0);
        let _ = file_lane::parse_first_line(&packet);
        let _ = file_lane::token_is_well_formed(&String::from_utf8_lossy(&rng.bytes(64)));
    }
    // A peer that never sends a newline cannot make the reader grow.
    struct Endless(usize);
    impl Read for Endless {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.0 += buffer.len();
            assert!(self.0 < 1 << 20, "read_line kept reading an endless line");
            buffer.fill(b'a');
            Ok(buffer.len())
        }
    }
    assert!(file_lane::read_line(&mut Endless(0)).is_err());
}

#[test]
fn hostile_signaling_packets_never_panic() {
    let mut rng = Rng::new(6);
    for _ in 0..ROUNDS {
        let mut packet = NetworkPacket::new(PACKET_TYPE_WEBRTC_SIGNAL_V1);
        packet.body = rng.object(0);
        let _ = WebRtcSignalingMessage::from_network_packet(&packet);
        // The keys the parser looks for, with the wrong types.
        for key in [
            "signalingVersion",
            "requestId",
            "sessionAttemptId",
            "fromDeviceId",
            "toDeviceId",
            "timestamp",
            "messageType",
            "payload",
            "signature",
        ] {
            packet.body.insert(key.to_string(), rng.value(0));
        }
        let _ = WebRtcSignalingMessage::from_network_packet(&packet);
    }
}

#[test]
fn hostile_contact_and_identity_bodies_never_panic() {
    let mut rng = Rng::new(7);
    for _ in 0..ROUNDS {
        let mut packet = NetworkPacket::new(PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS);
        packet.body = rng.object(0);
        let _ = crate::device_links::contacts::parse_snapshot(&packet);
        let text = String::from_utf8_lossy(&rng.bytes(4000)).into_owned();
        let _ = crate::device_links::contacts::parse_vcard("uid", &text);
        let _ = crate::device_links::contacts::number_key(&text);
        let mut identity = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        identity.body = rng.object(0);
        for key in ["deviceId", "deviceName", "deviceType", "protocolVersion"] {
            identity.body.insert(key.to_string(), rng.value(1));
        }
        let _ = crate::device_links::device_info::DeviceInfo::from_identity_packet(&identity);
    }
    let vcard = format!(
        "BEGIN:VCARD\nFN:{}\nTEL:{}\nPHOTO;ENCODING=b:{}\nEND:VCARD",
        "N".repeat(1 << 20),
        "1".repeat(1 << 20),
        "A".repeat(1 << 20)
    );
    let _ = crate::device_links::contacts::parse_vcard("uid", &vcard);
}

/// The peer's feature types whose handlers keep their state in memory (the
/// notes, contacts and folder stores are process-wide and installed by other
/// tests, and clipboard, lock and sound handlers act on the machine).
const SAFE_TYPES: &[&str] = &[
    PACKET_TYPE_ACTIVITY,
    PACKET_TYPE_BATTERY,
    PACKET_TYPE_MPRIS,
    PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_TELEPHONY,
    PACKET_TYPE_SYNC_HAVE,
    PACKET_TYPE_PING,
];

#[test]
fn hostile_feature_packets_never_panic_the_dispatcher() {
    let dir = std::env::temp_dir().join(format!("desklink-fuzz-{}", uuid::Uuid::new_v4().simple()));
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let sessions = Arc::new(DeviceManager::new());
    let mut link = SessionLink::test_placeholder("phone");
    link.info.outgoing_capabilities = SAFE_TYPES.iter().map(|name| name.to_string()).collect();
    let binding = sessions
        .register_link("phone".to_string(), link, true)
        .unwrap()
        .binding;
    sessions
        .with_session(&binding, |session| {
            let mut runtime = HandoverRuntime::new("attempt".to_string(), wire());
            runtime.record_offer("offer".to_string()).unwrap();
            runtime.record_answer("answer".to_string()).unwrap();
            runtime.mark_local_authenticated().unwrap();
            runtime.mark_remote_authenticated().unwrap();
            runtime.mark_local_capabilities_confirmed().unwrap();
            runtime.mark_remote_capabilities_confirmed().unwrap();
            runtime.mark_local_feature_ready().unwrap();
            runtime.mark_remote_feature_ready().unwrap();
            session.webrtc_handover = Some(runtime);
        })
        .unwrap();
    let devices = Arc::new(Mutex::new(HashMap::new()));
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut rng = Rng::new(8);
    let mut accepted = 0;
    for _ in 0..ROUNDS {
        let mut packet = NetworkPacket::new(SAFE_TYPES[rng.below(SAFE_TYPES.len())]);
        packet.body = rng.object(0);
        // Real keys with wrong types, on top of the random ones.
        for _ in 0..3 {
            let key = [
                "events", "payload", "text", "rev", "have", "id", "title", "state",
            ][rng.below(8)];
            packet.body.insert(key.to_string(), rng.value(0));
        }
        if super::packet_handler::dispatch_webrtc_feature_packet(
            &binding, packet, &devices, &sessions, &config, &events,
        )
        .is_ok()
        {
            accepted += 1;
        }
        // Whatever the handlers queue for the UI stays bounded.
        if let Ok(mut events) = events.lock() {
            assert!(events.len() < 100_000);
            events.clear();
        }
    }
    assert!(accepted > 0, "the harness reached no handler at all");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_screen_size_past_i32_is_dropped_not_wrapped_into_range() {
    let body = json!({"remoteSessionVersion": 1, "kind": "screen-ready",
        "sessionAttemptId": "a", "deviceId": "phone", "remoteSessionId": "s",
        "screenWidth": 4_294_967_297_i64, "screenHeight": 1080, "screenRotation": -(1_i64 << 40)});
    let message = ControlMessage::from_json(&serde_json::to_vec(&body).unwrap()).unwrap();
    assert_eq!(message.screen_width, None, "2^32 + 1 must not read as 1");
    assert_eq!(message.screen_rotation, None);
    assert!(message.validate(&phone_wire(), "a", 0).is_err());
}
