//! Lane A of the bulk engine (W18): `DLF2` frames over a TCP connection on the
//! LAN, for big files between two paired devices on the same network.
//!
//! The connection is the LAN bootstrap's own kind of connection - the same
//! listener, pinned mutual TLS with the stored pairing certificates, the same
//! TLS roles - plus a one-time token the receiver handed out over the WebRTC
//! control channel. It carries nothing but file frames: never a JSON feature
//! packet. Contract: `docs/protocol/stageD-protocol.md`, "W18".
//!
//! This module is the connection: tokens, the lines, the two handshakes. What
//! the frames mean is the transfer engine's.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use openssl::ssl::SslStream;
use serde_json::{json, Value};

use crate::device_links::config::Config;

pub(crate) const FIRST_LINE_TYPE: &str = "desklink.filelane";
const TOKEN_TYPE: &str = "desklink.filelane.token";
const READY_TYPE: &str = "desklink.filelane.ready";

/// Smaller files stay on WebRTC.
pub(crate) const MIN_FILE_BYTES: u64 = 8 << 20;
/// Connect and handshake each have this long; past it the transfer carries on
/// over WebRTC.
pub(crate) const HANDSHAKE_LIMIT: Duration = Duration::from_secs(3);
/// A token is good this long from the accept.
pub(crate) const TOKEN_TTL: Duration = Duration::from_secs(60);
/// No block acknowledged this long with the lane open: it is given up.
pub(crate) const STALL_LIMIT: Duration = Duration::from_secs(5);
/// The plaintext first line (and every line after it) is cut at this.
pub(crate) const LINE_MAX: usize = 4096;
/// Lane connections per peer.
pub(crate) const LANES_PER_PEER: usize = 2;

/// Where and as whom a lane would connect.
#[derive(Clone)]
pub(crate) struct LaneTarget {
    /// The peer's address on the path ICE selected (the LAN address).
    pub(crate) addr: std::net::IpAddr,
    pub(crate) config: Arc<Mutex<Config>>,
    pub(crate) local_id: String,
    pub(crate) peer_id: String,
}

static LISTEN_PORT: std::sync::atomic::AtomicU16 = std::sync::atomic::AtomicU16::new(0);

/// The LAN listener's port (set by the daemon at start; 0 until then).
pub(crate) fn set_listen_port(port: u16) {
    LISTEN_PORT.store(port, std::sync::atomic::Ordering::Relaxed);
}

pub(crate) fn listen_port() -> u16 {
    LISTEN_PORT.load(std::sync::atomic::Ordering::Relaxed)
}

// ── Tokens ─────────────────────────────────────────────────────────────────

/// 32 lowercase hex characters (128 random bits).
pub(crate) fn token_is_well_formed(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn new_token() -> String {
    let mut bytes = [0u8; 16];
    openssl::rand::rand_bytes(&mut bytes).expect("the system has random bytes");
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Why a lane is refused (the `reason` of the `ready` line).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Unknown, expired, used, malformed or another transfer's or device's.
    Token,
    /// The transfer is not open here, is cancelled, or goes the other way.
    Transfer,
    /// The start block or the pass is not where the transfer is.
    Range,
    /// Too many lane connections for this peer.
    Busy,
    /// This side does not see the LAN path.
    Path,
}

impl Refusal {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Refusal::Token => "token",
            Refusal::Transfer => "transfer",
            Refusal::Range => "range",
            Refusal::Busy => "busy",
            Refusal::Path => "path",
        }
    }
}

struct Grant {
    transfer_id: String,
    dialler_id: String,
    expires: Instant,
}

/// The tokens handed out and not yet used.
#[derive(Default)]
pub(crate) struct Grants {
    by_token: HashMap<String, Grant>,
}

/// More than this at once is a bug or an attack: the oldest go.
const MAX_GRANTS: usize = 64;

impl Grants {
    /// A token for `dialler_id` to send `transfer_id` over a lane.
    pub(crate) fn issue(&mut self, transfer_id: &str, dialler_id: &str, now: Instant) -> String {
        self.by_token.retain(|_, grant| grant.expires > now);
        if self.by_token.len() >= MAX_GRANTS {
            let oldest = self
                .by_token
                .iter()
                .min_by_key(|(_, grant)| grant.expires)
                .map(|(token, _)| token.clone());
            if let Some(oldest) = oldest {
                self.by_token.remove(&oldest);
            }
        }
        let token = new_token();
        self.by_token.insert(
            token.clone(),
            Grant {
                transfer_id: transfer_id.to_string(),
                dialler_id: dialler_id.to_string(),
                expires: now + TOKEN_TTL,
            },
        );
        token
    }

    /// Uses `token`: it must be known, unexpired, unused, and bound to this
    /// transfer and this device. A known token is spent by any attempt, right
    /// or wrong: a guess cannot be tried twice.
    pub(crate) fn redeem(
        &mut self,
        token: &str,
        transfer_id: &str,
        dialler_id: &str,
        now: Instant,
    ) -> Result<(), Refusal> {
        if !token_is_well_formed(token) {
            return Err(Refusal::Token);
        }
        let grant = self.by_token.remove(token).ok_or(Refusal::Token)?;
        if grant.expires <= now
            || grant.transfer_id != transfer_id
            || grant.dialler_id != dialler_id
        {
            return Err(Refusal::Token);
        }
        Ok(())
    }

    /// Tokens for `transfer_id` are void (it ended or was cancelled).
    pub(crate) fn void(&mut self, transfer_id: &str) {
        self.by_token
            .retain(|_, grant| grant.transfer_id != transfer_id);
    }
}

fn grants() -> &'static Mutex<Grants> {
    static GRANTS: std::sync::OnceLock<Mutex<Grants>> = std::sync::OnceLock::new();
    GRANTS.get_or_init(Default::default)
}

/// Spends a token the way the listener does (tests of the engine's layers).
#[cfg(test)]
pub(crate) fn spend_token_for_test(token: &str, transfer_id: &str, dialler_id: &str) -> bool {
    grants().lock().is_ok_and(|mut grants| {
        grants
            .redeem(token, transfer_id, dialler_id, Instant::now())
            .is_ok()
    })
}

/// Issues a token (receiver side, at the accept).
pub(crate) fn issue_token(transfer_id: &str, dialler_id: &str) -> String {
    grants()
        .lock()
        .map(|mut grants| grants.issue(transfer_id, dialler_id, Instant::now()))
        .unwrap_or_default()
}

pub(crate) fn void_tokens(transfer_id: &str) {
    if let Ok(mut grants) = grants().lock() {
        grants.void(transfer_id);
    }
}

// ── Who receives a lane ────────────────────────────────────────────────────

/// The side of the transfer engine that takes a lane connection for a peer:
/// asked whether the transfer is there and the range right, then handed the
/// stream (to read frames from).
pub(crate) trait Receiver: Send + Sync {
    fn check(&self, body: &TokenBody) -> Result<(), Refusal>;
    fn open(&self, stream: SslStream<TcpStream>, body: TokenBody);
}

#[derive(Default)]
struct Registry {
    receivers: HashMap<String, Arc<dyn Receiver>>,
    /// Lane connections open per peer.
    open: HashMap<String, usize>,
}

fn registry() -> &'static Mutex<Registry> {
    static REGISTRY: std::sync::OnceLock<Mutex<Registry>> = std::sync::OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

/// `peer`'s engine takes lanes from now on.
pub(crate) fn register_receiver(peer: &str, receiver: Arc<dyn Receiver>) {
    if let Ok(mut registry) = registry().lock() {
        registry.receivers.insert(peer.to_string(), receiver);
    }
}

pub(crate) fn unregister_receiver(peer: &str) {
    if let Ok(mut registry) = registry().lock() {
        registry.receivers.remove(peer);
    }
}

/// A lane of `peer` has ended (the engine tells).
pub(crate) fn lane_closed(peer: &str) {
    if let Ok(mut registry) = registry().lock() {
        if let Some(open) = registry.open.get_mut(peer) {
            *open = open.saturating_sub(1);
        }
    }
}

/// What the listener does with a lane first line: [`serve`], then the token,
/// the engine's `check`, the answer and, if it is yes, `open`. Anything that
/// is not a paired device with a good token ends here, quietly.
pub(crate) fn accept(
    stream: TcpStream,
    config: &Arc<Mutex<Config>>,
    packet: &crate::device_links::packet::NetworkPacket,
) -> Result<(), String> {
    let (dialler, target) = parse_first_line(packet).ok_or("a lane line that is not one")?;
    let local = config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .local_device_info()
        .id;
    if target != local {
        return Err("a lane for another device".into());
    }
    let paired = config
        .lock()
        .map(|config| config.is_trusted(&dialler))
        .unwrap_or(false);
    if !paired {
        return Err("a lane from a device that is not paired".into());
    }
    let request = serve(stream, config, &dialler)?;
    let receiver = registry()
        .lock()
        .ok()
        .and_then(|registry| registry.receivers.get(&dialler).cloned());
    let busy = registry().lock().map_or(true, |registry| {
        registry.open.get(&dialler).copied().unwrap_or(0) >= LANES_PER_PEER
    });
    let answered = request.answer(|body| {
        let receiver = receiver.as_ref().ok_or(Refusal::Transfer)?;
        if busy {
            return Err(Refusal::Busy);
        }
        receiver.check(body)
    });
    if let (Some((ssl, body)), Some(receiver)) = (answered, receiver) {
        if let Ok(mut registry) = registry().lock() {
            *registry.open.entry(dialler).or_default() += 1;
        }
        receiver.open(ssl, body);
    }
    Ok(())
}

// ── The lines ──────────────────────────────────────────────────────────────

/// The plaintext first line, without its newline.
pub(crate) fn first_line(dialler_id: &str, target_id: &str, now_ms: u64) -> String {
    json!({"id": now_ms, "type": FIRST_LINE_TYPE,
        "body": {"v": 1, "deviceId": dialler_id, "targetDeviceId": target_id}})
    .to_string()
}

/// `(dialler id, target id)` of a first line already known to be of the lane
/// type; `None` if it is malformed or of another version.
pub(crate) fn parse_first_line(
    packet: &crate::device_links::packet::NetworkPacket,
) -> Option<(String, String)> {
    (packet.packet_type == FIRST_LINE_TYPE
        && packet.body.get("v").and_then(Value::as_u64) == Some(1))
    .then(|| {
        Some((
            packet.get_str("deviceId")?.to_string(),
            packet.get_str("targetDeviceId")?.to_string(),
        ))
    })
    .flatten()
}

/// What the dialler asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TokenBody {
    pub(crate) transfer_id: String,
    pub(crate) token: String,
    pub(crate) pass: u8,
    pub(crate) start_block: u64,
}

pub(crate) fn token_line(body: &TokenBody) -> String {
    json!({"type": TOKEN_TYPE, "body": {"transferId": body.transfer_id,
        "token": body.token, "pass": body.pass, "startBlock": body.start_block}})
    .to_string()
}

pub(crate) fn parse_token_line(line: &[u8]) -> Result<TokenBody, Refusal> {
    let value: Value = serde_json::from_slice(line).map_err(|_| Refusal::Token)?;
    if value.get("type").and_then(Value::as_str) != Some(TOKEN_TYPE) {
        return Err(Refusal::Token);
    }
    let body = value.get("body").ok_or(Refusal::Token)?;
    Ok(TokenBody {
        transfer_id: body
            .get("transferId")
            .and_then(Value::as_str)
            .ok_or(Refusal::Transfer)?
            .to_string(),
        token: body
            .get("token")
            .and_then(Value::as_str)
            .ok_or(Refusal::Token)?
            .to_string(),
        pass: body
            .get("pass")
            .and_then(Value::as_u64)
            .and_then(|pass| u8::try_from(pass).ok())
            .ok_or(Refusal::Range)?,
        start_block: body
            .get("startBlock")
            .and_then(Value::as_u64)
            .ok_or(Refusal::Range)?,
    })
}

pub(crate) fn ready_line(answer: Result<(), Refusal>) -> String {
    match answer {
        Ok(()) => json!({"type": READY_TYPE, "body": {"ok": true}}),
        Err(why) => json!({"type": READY_TYPE, "body": {"ok": false, "reason": why.as_str()}}),
    }
    .to_string()
}

/// `Ok` or the listener's reason.
pub(crate) fn parse_ready(line: &[u8]) -> Result<(), String> {
    let value: Value = serde_json::from_slice(line).map_err(|_| "unreadable".to_string())?;
    if value.get("type").and_then(Value::as_str) != Some(READY_TYPE) {
        return Err("unexpected".to_string());
    }
    let body = value.get("body").ok_or("unexpected")?;
    if body.get("ok").and_then(Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(body
            .get("reason")
            .and_then(Value::as_str)
            .map(|reason| reason.chars().take(16).collect())
            .unwrap_or_else(|| "refused".to_string()))
    }
}

/// One line (without the newline), read a byte at a time and cut at
/// [`LINE_MAX`]: from a peer that has not yet proved anything.
pub(crate) fn read_line(reader: &mut impl Read) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if reader.read(&mut byte).map_err(|error| error.to_string())? == 0 {
            return Err("closed".to_string());
        }
        if byte[0] == b'\n' {
            return Ok(line);
        }
        line.push(byte[0]);
        if line.len() > LINE_MAX {
            return Err("line too long".to_string());
        }
    }
}

// ── The two handshakes ─────────────────────────────────────────────────────

/// The peer's certificate must be the one stored at pairing, for this device:
/// a device that is not paired, or whose certificate differs, is refused (no
/// QR admission here, unlike the bootstrap).
fn check_pinned(
    config: &Arc<Mutex<Config>>,
    stream: &SslStream<TcpStream>,
    peer_id: &str,
) -> Result<(), String> {
    let certificate = stream
        .ssl()
        .peer_certificate()
        .ok_or("the peer showed no certificate")?;
    let pem = String::from_utf8(certificate.to_pem().map_err(|error| error.to_string())?)
        .map_err(|error| error.to_string())?;
    let stored = config
        .lock()
        .map_err(|_| "config lock poisoned".to_string())?
        .trusted_certificate_pem(peer_id);
    if stored.as_deref() != Some(pem.as_str()) {
        return Err("not the paired certificate".to_string());
    }
    let common_name = certificate
        .subject_name()
        .entries_by_nid(openssl::nid::Nid::COMMONNAME)
        .next()
        .and_then(|entry| entry.data().to_string().ok());
    if common_name.as_deref() != Some(peer_id) {
        return Err("the certificate is for another device".to_string());
    }
    Ok(())
}

/// A lane the listener has accepted up to the token: not yet answered.
pub(crate) struct Request {
    pub(crate) stream: SslStream<TcpStream>,
    pub(crate) dialler_id: String,
    pub(crate) body: TokenBody,
}

impl Request {
    /// Redeems the token against this process's grants, then asks `accept`
    /// whether the transfer is there and the range right; answers the dialler
    /// and, on `Ok`, hands the stream on.
    pub(crate) fn answer(
        mut self,
        accept: impl FnOnce(&TokenBody) -> Result<(), Refusal>,
    ) -> Option<(SslStream<TcpStream>, TokenBody)> {
        let outcome = grants()
            .lock()
            .map_err(|_| Refusal::Token)
            .and_then(|mut grants| {
                grants.redeem(
                    &self.body.token,
                    &self.body.transfer_id,
                    &self.dialler_id,
                    Instant::now(),
                )
            })
            .and_then(|()| accept(&self.body));
        let _ = self
            .stream
            .write_all(format!("{}\n", ready_line(outcome)).as_bytes());
        let _ = self.stream.flush();
        outcome.ok().map(|()| (self.stream, self.body))
    }
}

/// The listener's side, after the first line named a lane: TLS (as the
/// bootstrap's listener does), the pinned certificate, then the token line.
pub(crate) fn serve(
    stream: TcpStream,
    config: &Arc<Mutex<Config>>,
    dialler_id: &str,
) -> Result<Request, String> {
    stream
        .set_read_timeout(Some(HANDSHAKE_LIMIT))
        .and_then(|()| stream.set_write_timeout(Some(HANDSHAKE_LIMIT)))
        .map_err(|error| error.to_string())?;
    let _deadline =
        crate::device_links::daemon::network::HandshakeDeadline::arm(&stream, HANDSHAKE_LIMIT);
    let connector = crate::device_links::daemon::network::ssl_connector(config)?;
    let mut ssl = connector
        .connect(dialler_id, stream)
        .map_err(|error| error.to_string())?;
    check_pinned(config, &ssl, dialler_id)?;
    let line = read_line(&mut ssl)?;
    let body = parse_token_line(&line).map_err(|why| why.as_str().to_string())?;
    Ok(Request {
        stream: ssl,
        dialler_id: dialler_id.to_string(),
        body,
    })
}

/// The dialler's (sender's) side: connect within 3 s, the first line, TLS (as
/// the bootstrap's dialler does), the pinned certificate, the token line, the
/// answer. The returned stream carries frames, with no timeout set.
pub(crate) fn dial(
    config: &Arc<Mutex<Config>>,
    local_id: &str,
    peer_id: &str,
    addr: SocketAddr,
    body: &TokenBody,
) -> Result<SslStream<TcpStream>, String> {
    let started = Instant::now();
    let mut stream =
        TcpStream::connect_timeout(&addr, HANDSHAKE_LIMIT).map_err(|error| error.to_string())?;
    let left = HANDSHAKE_LIMIT
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(200));
    stream
        .set_read_timeout(Some(left))
        .and_then(|()| stream.set_write_timeout(Some(left)))
        .and_then(|()| stream.set_nodelay(true))
        .map_err(|error| error.to_string())?;
    let _deadline = crate::device_links::daemon::network::HandshakeDeadline::arm(&stream, left);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);
    stream
        .write_all(format!("{}\n", first_line(local_id, peer_id, now_ms)).as_bytes())
        .map_err(|error| error.to_string())?;
    let acceptor = crate::device_links::daemon::network::ssl_acceptor(config)?;
    let mut ssl = acceptor.accept(stream).map_err(|error| error.to_string())?;
    check_pinned(config, &ssl, peer_id)?;
    ssl.write_all(format!("{}\n", token_line(body)).as_bytes())
        .and_then(|()| ssl.flush())
        .map_err(|error| error.to_string())?;
    let answer = read_line(&mut ssl)?;
    parse_ready(&answer)?;
    // The deadline guard drops here; frames flow without timeouts.
    let socket = ssl.get_ref();
    let _ = socket.set_read_timeout(None);
    let _ = socket.set_write_timeout(None);
    Ok(ssl)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device_info::DeviceInfo;
    use crate::device_links::packet::NetworkPacket;
    use std::net::TcpListener;

    /// The lines are JSON: compared as values, not as the order of their keys.
    fn same_json(ours: &str, theirs: &str) -> bool {
        serde_json::from_str::<Value>(ours).unwrap()
            == serde_json::from_str::<Value>(theirs).unwrap()
    }

    fn fixture() -> Value {
        serde_json::from_str(include_str!("../../../testdata/filelane_bodies.json")).unwrap()
    }

    #[test]
    fn the_lines_are_the_shared_vectors() {
        let f = fixture();
        // The dialler writes this first line.
        let line = first_line("phone-id", "desk-id", 1_730_000_000_000);
        assert!(
            same_json(&line, f["first_line"]["line"].as_str().unwrap()),
            "{line}"
        );
        let packet = NetworkPacket::deserialize(line.as_bytes()).unwrap();
        assert_eq!(
            parse_first_line(&packet),
            Some(("phone-id".into(), "desk-id".into()))
        );
        // And the token line, read both ways.
        let token = &f["token_line"];
        let body = parse_token_line(token["line"].as_str().unwrap().as_bytes()).unwrap();
        assert_eq!(body.transfer_id, token["transferId"].as_str().unwrap());
        assert_eq!(body.token, token["token"].as_str().unwrap());
        assert_eq!((body.pass, body.start_block), (0, 3));
        assert!(same_json(
            &token_line(&body),
            token["line"].as_str().unwrap()
        ));
        // The answers.
        assert!(same_json(
            &ready_line(Ok(())),
            f["ready_ok"].as_str().unwrap()
        ));
        assert_eq!(parse_ready(ready_line(Ok(())).as_bytes()), Ok(()));
        for case in f["ready_refusals"].as_array().unwrap() {
            let reason = case["reason"].as_str().unwrap();
            let refusal = match reason {
                "token" => Refusal::Token,
                "transfer" => Refusal::Transfer,
                "range" => Refusal::Range,
                "busy" => Refusal::Busy,
                _ => Refusal::Path,
            };
            assert!(same_json(
                &ready_line(Err(refusal)),
                case["line"].as_str().unwrap()
            ));
            assert_eq!(
                parse_ready(case["line"].as_str().unwrap().as_bytes()),
                Err(reason.to_string())
            );
        }
        // The limits the contract states.
        let limits = &f["limits"];
        assert_eq!(MIN_FILE_BYTES, limits["minFileBytes"].as_u64().unwrap());
        assert_eq!(
            HANDSHAKE_LIMIT.as_millis() as u64,
            limits["connectMs"].as_u64().unwrap()
        );
        assert_eq!(
            TOKEN_TTL.as_millis() as u64,
            limits["tokenTtlMs"].as_u64().unwrap()
        );
        assert_eq!(
            STALL_LIMIT.as_millis() as u64,
            limits["stallMs"].as_u64().unwrap()
        );
        assert_eq!(LINE_MAX as u64, limits["firstLineBytes"].as_u64().unwrap());
        assert_eq!(
            LANES_PER_PEER as u64,
            limits["lanesPerPeer"].as_u64().unwrap()
        );
        for bad in f["refused_tokens"].as_array().unwrap() {
            assert!(!token_is_well_formed(bad.as_str().unwrap()), "{bad}");
        }
        assert!(token_is_well_formed(token["token"].as_str().unwrap()));
        assert!(token_is_well_formed(&new_token()));
    }

    /// One use, 60 s, this transfer and this device; any attempt spends it.
    #[test]
    fn a_token_is_single_use_expires_and_is_bound() {
        let (mut grants, t0) = (Grants::default(), Instant::now());
        let token = grants.issue("t1", "phone", t0);
        assert_eq!(
            grants.redeem(&token, "t1", "phone", t0 + Duration::from_secs(1)),
            Ok(())
        );
        assert_eq!(
            grants.redeem(&token, "t1", "phone", t0 + Duration::from_secs(2)),
            Err(Refusal::Token),
            "a replay"
        );
        let late = grants.issue("t1", "phone", t0);
        assert_eq!(
            grants.redeem(&late, "t1", "phone", t0 + TOKEN_TTL),
            Err(Refusal::Token),
            "at 60 s"
        );
        let other = grants.issue("t1", "phone", t0);
        assert_eq!(
            grants.redeem(&other, "t2", "phone", t0),
            Err(Refusal::Token),
            "another transfer"
        );
        assert_eq!(
            grants.redeem(&other, "t1", "phone", t0),
            Err(Refusal::Token),
            "and spent by the try"
        );
        let third = grants.issue("t1", "phone", t0);
        assert_eq!(
            grants.redeem(&third, "t1", "tablet", t0),
            Err(Refusal::Token),
            "another device"
        );
        let voided = grants.issue("t9", "phone", t0);
        grants.void("t9");
        assert_eq!(
            grants.redeem(&voided, "t9", "phone", t0),
            Err(Refusal::Token),
            "voided"
        );
        assert_eq!(
            grants.redeem("nope", "t1", "phone", t0),
            Err(Refusal::Token)
        );
        // The table is bounded.
        for n in 0..200 {
            grants.issue(&format!("t{n}"), "phone", t0);
        }
        assert!(grants.by_token.len() <= MAX_GRANTS);
    }

    #[test]
    fn a_line_is_read_whole_and_cut_at_the_limit() {
        assert_eq!(read_line(&mut &b"abc\nrest"[..]).unwrap(), b"abc");
        assert!(
            read_line(&mut &b"abc"[..]).is_err(),
            "closed before the newline"
        );
        let long = vec![b'x'; LINE_MAX + 10];
        assert!(read_line(&mut &long[..]).unwrap_err().contains("too long"));
    }

    // ── Over a real TLS connection between two paired configs ──

    type Handshake = Option<(SslStream<TcpStream>, TokenBody)>;

    struct Side {
        config: Arc<Mutex<Config>>,
        id: String,
        dir: std::path::PathBuf,
    }

    fn side() -> Side {
        let dir = std::env::temp_dir().join(format!("desklink-lane-{}", uuid::Uuid::new_v4()));
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

    /// A listener thread that does what the daemon's listener does with a lane
    /// first line: `serve`, then `answer` with `accept`. Returns the address
    /// and a receiver of what the handshake produced.
    fn listener(
        side: &Side,
        accept: fn(&TokenBody) -> Result<(), Refusal>,
    ) -> (SocketAddr, std::sync::mpsc::Receiver<Handshake>) {
        let socket = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = socket.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let config = Arc::clone(&side.config);
        std::thread::spawn(move || {
            let (mut stream, _) = socket.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let line = read_line(&mut stream).unwrap();
            let packet = NetworkPacket::deserialize(&line).unwrap();
            let (dialler, _) = parse_first_line(&packet).unwrap();
            let outcome = serve(stream, &config, &dialler)
                .ok()
                .and_then(|request| request.answer(accept));
            let _ = tx.send(outcome);
        });
        (addr, rx)
    }

    fn body(transfer_id: &str, token: &str) -> TokenBody {
        TokenBody {
            transfer_id: transfer_id.into(),
            token: token.into(),
            pass: 0,
            start_block: 0,
        }
    }

    #[test]
    fn a_paired_device_with_a_good_token_gets_a_lane_and_it_carries_bytes_fast() {
        let (sender, receiver) = (side(), side());
        pair(&sender, &receiver);
        let token = issue_token("t-fast", &sender.id);
        let (addr, got) = listener(&receiver, |_| Ok(()));
        let mut lane = dial(
            &sender.config,
            &sender.id,
            &receiver.id,
            addr,
            &body("t-fast", &token),
        )
        .unwrap();
        let (mut far, accepted) = got
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .expect("answered");
        assert_eq!(accepted.transfer_id, "t-fast");
        // 256 MB of frames' worth of bytes, 60 KiB at a time, through TLS.
        let total: usize = 256 * 1_000_000;
        let reader = std::thread::spawn(move || {
            let mut buffer = vec![0u8; 1 << 17];
            let mut seen = 0usize;
            while seen < total {
                let n = far.read(&mut buffer).unwrap();
                assert!(n > 0, "closed early");
                seen += n;
            }
            seen
        });
        let chunk = vec![7u8; 61_440];
        let started = Instant::now();
        let mut sent = 0usize;
        while sent < total {
            let n = chunk.len().min(total - sent);
            lane.write_all(&chunk[..n]).unwrap();
            sent += n;
        }
        lane.flush().unwrap();
        assert_eq!(reader.join().unwrap(), total);
        let rate = total as f64 / started.elapsed().as_secs_f64() / 1e6;
        eprintln!("[bench] lane A over loopback TLS: {rate:.0} MB/s");
        let floor: f64 = std::env::var("DESKLINK_LANE_FLOOR")
            .ok()
            .and_then(|text| text.parse().ok())
            .unwrap_or(100.0);
        assert!(
            rate >= floor,
            "{rate:.0} MB/s is under the floor of {floor} MB/s"
        );
        let _ = (&sender.dir, &receiver.dir);
    }

    #[test]
    fn a_replayed_a_wrong_or_an_unpaired_dial_is_refused() {
        let (sender, receiver, stranger) = (side(), side(), side());
        pair(&sender, &receiver);
        // A token used once is dead.
        let token = issue_token("t-replay", &sender.id);
        let (addr, got) = listener(&receiver, |_| Ok(()));
        dial(
            &sender.config,
            &sender.id,
            &receiver.id,
            addr,
            &body("t-replay", &token),
        )
        .unwrap();
        assert!(got.recv_timeout(Duration::from_secs(5)).unwrap().is_some());
        let (addr, got) = listener(&receiver, |_| Ok(()));
        let error = dial(
            &sender.config,
            &sender.id,
            &receiver.id,
            addr,
            &body("t-replay", &token),
        )
        .unwrap_err();
        assert_eq!(error, "token");
        assert!(got.recv_timeout(Duration::from_secs(5)).unwrap().is_none());
        // A token for another transfer.
        let token = issue_token("t-a", &sender.id);
        let (addr, _got) = listener(&receiver, |_| Ok(()));
        assert_eq!(
            dial(
                &sender.config,
                &sender.id,
                &receiver.id,
                addr,
                &body("t-b", &token)
            )
            .unwrap_err(),
            "token"
        );
        // A token issued for another device.
        let token = issue_token("t-c", "someone-else");
        let (addr, _got) = listener(&receiver, |_| Ok(()));
        assert_eq!(
            dial(
                &sender.config,
                &sender.id,
                &receiver.id,
                addr,
                &body("t-c", &token)
            )
            .unwrap_err(),
            "token"
        );
        // The transfer is not there / the range is wrong: the engine's answer.
        let token = issue_token("t-d", &sender.id);
        let (addr, _got) = listener(&receiver, |_| Err(Refusal::Range));
        assert_eq!(
            dial(
                &sender.config,
                &sender.id,
                &receiver.id,
                addr,
                &body("t-d", &token)
            )
            .unwrap_err(),
            "range"
        );
        // A device that is not paired is refused before any token is read.
        let token = issue_token("t-e", &stranger.id);
        let (addr, got) = listener(&receiver, |_| Ok(()));
        assert!(dial(
            &stranger.config,
            &stranger.id,
            &receiver.id,
            addr,
            &body("t-e", &token)
        )
        .is_err());
        assert!(got.recv_timeout(Duration::from_secs(5)).unwrap().is_none());
        // A device that claims a paired device's id with its own certificate.
        let token = issue_token("t-f", &sender.id);
        let (addr, got) = listener(&receiver, |_| Ok(()));
        assert!(dial(
            &stranger.config,
            &sender.id,
            &receiver.id,
            addr,
            &body("t-f", &token)
        )
        .is_err());
        assert!(got.recv_timeout(Duration::from_secs(5)).unwrap().is_none());
    }

    struct Recording {
        opened: Mutex<Vec<TokenBody>>,
        refuse: Mutex<Option<Refusal>>,
    }

    impl Receiver for Recording {
        fn check(&self, _: &TokenBody) -> Result<(), Refusal> {
            self.refuse.lock().unwrap().map_or(Ok(()), Err)
        }

        fn open(&self, _stream: SslStream<TcpStream>, body: TokenBody) {
            self.opened.lock().unwrap().push(body);
        }
    }

    /// The listener's whole path, as the daemon runs it: refused with no
    /// engine registered, refused as the engine says, opened when it agrees,
    /// and at most two lanes open for one peer.
    #[test]
    fn the_listener_hands_a_good_lane_to_the_engine_and_refuses_the_rest() {
        let (sender, receiver) = (side(), side());
        pair(&sender, &receiver);
        let serve_one = |config: &Arc<Mutex<Config>>| {
            let socket = TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = socket.local_addr().unwrap();
            let config = Arc::clone(config);
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = socket.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let line = read_line(&mut stream).unwrap();
                let packet = NetworkPacket::deserialize(&line).unwrap();
                accept(stream, &config, &packet)
            });
            (addr, handle)
        };
        let dial_with = |addr, transfer: &str| {
            let token = issue_token(transfer, &sender.id);
            dial(
                &sender.config,
                &sender.id,
                &receiver.id,
                addr,
                &body(transfer, &token),
            )
        };
        // No engine for this peer: refused.
        let (addr, handle) = serve_one(&receiver.config);
        assert_eq!(dial_with(addr, "t-none").unwrap_err(), "transfer");
        handle.join().unwrap().unwrap();
        // An engine that says no.
        let engine = Arc::new(Recording {
            opened: Mutex::new(Vec::new()),
            refuse: Mutex::new(Some(Refusal::Range)),
        });
        register_receiver(&sender.id, engine.clone());
        let (addr, handle) = serve_one(&receiver.config);
        assert_eq!(dial_with(addr, "t-range").unwrap_err(), "range");
        handle.join().unwrap().unwrap();
        // An engine that says yes: the lane is opened, twice, not a third time.
        *engine.refuse.lock().unwrap() = None;
        let mut lanes = Vec::new();
        for transfer in ["t-1", "t-2"] {
            let (addr, handle) = serve_one(&receiver.config);
            lanes.push(dial_with(addr, transfer).unwrap());
            handle.join().unwrap().unwrap();
        }
        assert_eq!(engine.opened.lock().unwrap().len(), 2);
        let (addr, handle) = serve_one(&receiver.config);
        assert_eq!(dial_with(addr, "t-3").unwrap_err(), "busy");
        handle.join().unwrap().unwrap();
        // One ends: there is room again.
        lane_closed(&sender.id);
        let (addr, handle) = serve_one(&receiver.config);
        lanes.push(dial_with(addr, "t-4").unwrap());
        handle.join().unwrap().unwrap();
        assert_eq!(engine.opened.lock().unwrap().len(), 3);
        unregister_receiver(&sender.id);
    }

    /// Nothing listens: the dial fails inside the limit and says so.
    #[test]
    fn a_dial_to_nowhere_fails_within_the_limit() {
        let (sender, receiver) = (side(), side());
        pair(&sender, &receiver);
        let closed = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let started = Instant::now();
        assert!(dial(
            &sender.config,
            &sender.id,
            &receiver.id,
            closed,
            &body("t", &"0".repeat(32))
        )
        .is_err());
        assert!(started.elapsed() < HANDSHAKE_LIMIT + Duration::from_secs(1));
    }
}
