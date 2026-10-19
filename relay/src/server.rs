//! Accept loop, health/metrics probes, and one thread per WebSocket.
//!
//! Threads, not async: the relay forwards a few small signaling frames per
//! device, a blocked thread costs only its stack, and blocking code with a
//! read timeout is far easier to audit than a hand-rolled event loop.

use std::io::{self, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::StatusCode;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tungstenite::{Message, WebSocket};

use crate::auth;
use crate::config::Config;
use crate::hub::{outbox, Hub, Inbox, Outbound, Outbox};
use crate::limits::{ConnLimits, NetKey, NetLimits, TurnQuota};
use crate::protocol::{
    ClientMessage, ErrorCode, ServerMessage, CLOSE_BAD_REQUEST, CLOSE_RATE_LIMITED,
    CLOSE_UNAUTHORIZED, CLOSE_UPGRADE_REQUIRED, MAX_ALLOW, MIN_PROTOCOL_VERSION, PROTOCOL_VERSION,
};
use crate::turn::TurnBackend;

/// The only path that upgrades. Apps hard-code `wss://<host>/v1`.
pub const WEBSOCKET_PATH: &str = "/v1";

/// Bounds the HTTP request and the WebSocket handshake (slowloris).
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Unauthenticated sockets are closed with 4401 after this.
const AUTH_TIMEOUT: Duration = Duration::from_secs(10);
/// How often a connection thread wakes to drain its outbox. It is also the
/// worst-case added forwarding latency.
const POLL_INTERVAL: Duration = Duration::from_millis(200);
const PING_INTERVAL: Duration = Duration::from_secs(20);
/// Three missed pings. Mobile radios drop silently; without this a dead
/// phone would keep reading as online to its peer.
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
/// A receiver that cannot take a frame for this long is treated as gone.
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_GRACE: Duration = Duration::from_secs(1);
/// Largest frame accepted before `welcome`. An `auth` frame with an RSA-4096
/// certificate is about 3 KiB; strangers get no more than this.
const PRE_AUTH_MAX_FRAME: usize = 16 * 1024;
/// tungstenite's own outgoing buffer. A client that stops reading fills it
/// and is dropped instead of growing the relay's memory.
const MAX_WRITE_BUFFER: usize = 4 * 1024 * 1024;
/// Fingerprints are 43 characters; anything much longer is not one.
const MAX_FP_LEN: usize = 64;
/// Close code for a frame above `max_frame_bytes` (RFC 6455 "message too big").
const CLOSE_TOO_BIG: u16 = 1009;

/// A panic on one connection thread must not take the hub down with it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// Aggregate counters only. Nothing here may identify a device or network.
#[derive(Default)]
pub struct Metrics {
    pub auth_ok: AtomicU64,
    pub auth_fail: AtomicU64,
    pub messages_forwarded: AtomicU64,
    pub turn_grants: AtomicU64,
    pub turn_denials: AtomicU64,
    pub rate_limit_closes: AtomicU64,
}

fn bump(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

pub struct Shared {
    pub config: Config,
    hub: Mutex<Hub>,
    limits: Mutex<NetLimits>,
    quota: Mutex<TurnQuota>,
    pub metrics: Metrics,
    turn: TurnBackend,
    next_conn: AtomicU64,
}

impl Shared {
    pub fn new(config: Config, quota: TurnQuota) -> Arc<Self> {
        Arc::new(Self {
            hub: Mutex::new(Hub::default()),
            limits: Mutex::new(NetLimits::new(config.limits)),
            quota: Mutex::new(quota),
            metrics: Metrics::default(),
            turn: TurnBackend::from_config(&config),
            next_conn: AtomicU64::new(1),
            config,
        })
    }

    /// Writes the TURN quota to `state_file`, if one is configured.
    pub fn save_quota(&self) -> Result<(), String> {
        let Some(path) = &self.config.state_file else {
            return Ok(());
        };
        lock(&self.quota)
            .save(path)
            .map_err(|error| format!("could not save quota state: {error}"))
    }

    pub fn prune(&self) {
        lock(&self.limits).prune(Instant::now());
    }

    fn counters(&self) -> [(&'static str, &'static str, u64); 8] {
        let m = &self.metrics;
        let get = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
        [
            ("connections", "gauge", lock(&self.limits).total() as u64),
            ("devices_online", "gauge", lock(&self.hub).online() as u64),
            ("auth_ok_total", "counter", get(&m.auth_ok)),
            ("auth_fail_total", "counter", get(&m.auth_fail)),
            (
                "messages_forwarded_total",
                "counter",
                get(&m.messages_forwarded),
            ),
            ("turn_grants_total", "counter", get(&m.turn_grants)),
            ("turn_denials_total", "counter", get(&m.turn_denials)),
            (
                "rate_limit_closes_total",
                "counter",
                get(&m.rate_limit_closes),
            ),
        ]
    }

    /// Prometheus text exposition format.
    pub fn metrics_text(&self) -> String {
        self.counters()
            .iter()
            .map(|(name, kind, value)| {
                format!("# TYPE desklink_relay_{name} {kind}\ndesklink_relay_{name} {value}\n")
            })
            .collect()
    }

    /// One log line of aggregate counts.
    pub fn summary(&self) -> String {
        self.counters()
            .iter()
            .map(|(name, _, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub struct Server {
    listener: TcpListener,
    shared: Arc<Shared>,
}

impl Server {
    pub fn bind(shared: Arc<Shared>) -> io::Result<Self> {
        let listener = TcpListener::bind(&shared.config.listen)?;
        Ok(Self { listener, shared })
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    pub fn run(self) {
        for stream in self.listener.incoming() {
            let stream = match stream {
                Ok(stream) => stream,
                // Usually EMFILE. Back off instead of spinning on accept.
                Err(_) => {
                    thread::sleep(Duration::from_millis(50));
                    continue;
                }
            };
            if !lock(&self.shared.limits).open_global() {
                bump(&self.shared.metrics.rate_limit_closes);
                continue;
            }
            let slot = GlobalSlot {
                shared: self.shared.clone(),
                authenticated: false,
            };
            // If spawning fails the closure (and the slot) is dropped, which
            // releases the global count.
            let _ = thread::Builder::new()
                .name("relay-conn".into())
                .stack_size(512 * 1024)
                .spawn(move || handle(stream, slot));
        }
    }
}

struct GlobalSlot {
    shared: Arc<Shared>,
    /// Whether the slot moved to the authenticated share. A panic leaves it
    /// false, which at worst under-counts unauthenticated sockets.
    authenticated: bool,
}

impl Drop for GlobalSlot {
    fn drop(&mut self) {
        lock(&self.shared.limits).close_global(self.authenticated);
    }
}

/// A socket whose reads and writes give up at a deadline.
///
/// tungstenite's `read()` loops until a whole frame has arrived, so a socket
/// read timeout alone never fires for a client that drip-feeds one byte every
/// 150 ms; the connection loop would never get back to its auth timeout,
/// silence check, pings or outbox. Past the deadline this returns
/// `WouldBlock`, which tungstenite treats as "try again later" while keeping
/// any partial frame. The connection loop moves the deadline forward on every
/// iteration.
struct Deadline {
    stream: TcpStream,
    until: Instant,
}

impl Deadline {
    fn expired(&self) -> io::Result<()> {
        if Instant::now() >= self.until {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        Ok(())
    }
}

impl Read for Deadline {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.expired()?;
        self.stream.read(buf)
    }
}

impl Write for Deadline {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.expired()?;
        self.stream.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

struct NetworkSlot<'a>(&'a Shared, NetKey);

impl Drop for NetworkSlot<'_> {
    fn drop(&mut self) {
        lock(&self.0.limits).close_network(self.1);
    }
}

fn handle(stream: TcpStream, mut slot: GlobalSlot) {
    let shared = &*slot.shared.clone();
    let Ok(peer) = stream.peer_addr() else {
        return;
    };
    let _ = stream.set_nodelay(true);
    if stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).is_err()
        || stream.set_write_timeout(Some(WRITE_TIMEOUT)).is_err()
    {
        return;
    }
    let Some(line) = request_line(&stream) else {
        return;
    };
    if line.starts_with("GET /healthz ") {
        respond(stream, "200 OK", "text/plain", "ok");
    } else if line.starts_with("GET /metrics ") {
        let body = shared.metrics_text();
        respond(stream, "200 OK", "text/plain; version=0.0.4", &body);
    } else if line.starts_with("GET /v1 ") || line.starts_with("GET /v1?") {
        slot.authenticated = websocket(stream, peer.ip(), shared);
    } else {
        respond(stream, "404 Not Found", "text/plain", "not found");
    }
}

/// Peeks the HTTP request line without consuming it, so a WebSocket upgrade
/// can still be handed to tungstenite untouched.
fn request_line(stream: &TcpStream) -> Option<String> {
    let mut buffer = [0u8; 256];
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let read = stream.peek(&mut buffer).ok()?;
        if read == 0 {
            return None;
        }
        if let Some(end) = buffer[..read].windows(2).position(|pair| pair == b"\r\n") {
            return Some(String::from_utf8_lossy(&buffer[..end]).into_owned());
        }
        // A request line this long is not a probe; let the handshake judge it.
        if read == buffer.len() {
            return Some(String::from_utf8_lossy(&buffer).into_owned());
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn respond(mut stream: TcpStream, status: &str, content_type: &str, body: &str) {
    // Consume the request head first: closing with unread bytes makes the
    // kernel send RST, which can destroy the response in flight.
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    while !head.windows(4).any(|w| w == b"\r\n\r\n") && head.len() < 8192 {
        if Instant::now() >= deadline {
            return;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(read) => head.extend_from_slice(&chunk[..read]),
        }
    }
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.shutdown(Shutdown::Write);
}

fn not_found() -> ErrorResponse {
    let mut response = ErrorResponse::new(Some("not found".into()));
    *response.status_mut() = StatusCode::NOT_FOUND;
    response
}

/// The client's address: the socket peer, or the first `X-Forwarded-For`
/// entry when the peer is a trusted proxy. Caddy overwrites a client-supplied
/// header, so the first entry is the address Caddy saw.
fn forwarded_for(request: &Request) -> Option<IpAddr> {
    request
        .headers()
        .get("x-forwarded-for")?
        .to_str()
        .ok()?
        .split(',')
        .next()?
        .trim()
        .parse()
        .ok()
}

/// Returns whether the socket authenticated, for the global slot's books.
fn websocket(stream: TcpStream, peer_ip: IpAddr, shared: &Shared) -> bool {
    let trusted = shared.config.trusted_proxies.contains(&peer_ip);
    let mut forwarded = None;
    let callback = |request: &Request, response: Response| {
        if request.uri().path() != WEBSOCKET_PATH {
            return Err(not_found());
        }
        if trusted {
            forwarded = forwarded_for(request);
        }
        Ok(response)
    };
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(PRE_AUTH_MAX_FRAME))
        .max_frame_size(Some(PRE_AUTH_MAX_FRAME))
        .max_write_buffer_size(MAX_WRITE_BUFFER);
    let stream = Deadline {
        stream,
        until: Instant::now() + HANDSHAKE_TIMEOUT,
    };
    let Ok(mut ws) = tungstenite::accept_hdr_with_config(stream, callback, Some(ws_config)) else {
        return false;
    };
    // From here every blocking call is bounded by the poll interval; the
    // deadline bounds whole frames.
    let socket = &ws.get_ref().stream;
    if socket.set_read_timeout(Some(POLL_INTERVAL)).is_err()
        || socket.set_write_timeout(Some(POLL_INTERVAL)).is_err()
    {
        return false;
    }
    let net = NetKey::of(forwarded.unwrap_or(peer_ip));

    if !lock(&shared.limits).open_network(net) {
        bump(&shared.metrics.rate_limit_closes);
        close_with(&mut ws, CLOSE_RATE_LIMITED, "too many connections");
        return false;
    }
    let _slot = NetworkSlot(shared, net);
    let Ok(nonce) = auth::new_nonce() else {
        return false;
    };
    let (outbox, inbox) = outbox();
    let mut connection = Connection {
        shared,
        ws,
        net,
        conn: shared.next_conn.fetch_add(1, Ordering::Relaxed),
        nonce,
        fp: None,
        outbox,
        inbox,
        limits: ConnLimits::new(&shared.config.limits, Instant::now()),
    };
    let exit = connection.serve().err().unwrap_or(Exit::Gone);
    // Leave the hub before the close handshake, so peers see the device go
    // offline without waiting for the grace period.
    if let Some(fp) = &connection.fp {
        lock(&shared.hub).unregister(fp, connection.conn);
    }
    match exit {
        Exit::Close(code, reason) => {
            if code == CLOSE_RATE_LIMITED {
                bump(&shared.metrics.rate_limit_closes);
            }
            close_with(&mut connection.ws, code, reason);
        }
        // Flushes tungstenite's automatic reply to a client close frame.
        Exit::Gone => {
            let _ = connection.ws.flush();
        }
    }
    connection.fp.is_some()
}

fn would_block(error: &tungstenite::Error) -> bool {
    matches!(error, tungstenite::Error::Io(io) if matches!(io.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
}

/// Sends a close frame and waits briefly for the client's reply, so the code
/// arrives instead of being lost to a TCP reset.
fn close_with(ws: &mut WebSocket<Deadline>, code: u16, reason: &'static str) {
    let deadline = Instant::now() + CLOSE_GRACE;
    ws.get_mut().until = deadline;
    let frame = CloseFrame {
        code: CloseCode::from(code),
        reason: reason.into(),
    };
    match ws.close(Some(frame)) {
        Err(error) if !would_block(&error) => return,
        _ => {}
    }
    while Instant::now() < deadline {
        match ws.read() {
            Ok(_) => {}
            Err(error) if would_block(&error) => {}
            Err(_) => return,
        }
    }
}

enum Exit {
    /// The socket is closed or broken; nothing more can be sent.
    Gone,
    Close(u16, &'static str),
}

/// A write that hit the deadline stays queued inside tungstenite and is
/// retried on the next iteration; any other error ends the connection.
fn written(result: tungstenite::Result<()>) -> Result<(), Exit> {
    match result {
        Err(error) if !would_block(&error) => Err(Exit::Gone),
        _ => Ok(()),
    }
}

struct Connection<'a> {
    shared: &'a Shared,
    ws: WebSocket<Deadline>,
    net: NetKey,
    conn: u64,
    nonce: String,
    /// Set once authenticated.
    fp: Option<String>,
    outbox: Outbox,
    inbox: Inbox,
    limits: ConnLimits,
}

impl Connection<'_> {
    fn send(&mut self, message: &ServerMessage) -> Result<(), Exit> {
        let text = serde_json::to_string(message).map_err(|_| Exit::Gone)?;
        written(self.ws.write(Message::text(text)))
    }

    fn serve(&mut self) -> Result<(), Exit> {
        let challenge = ServerMessage::Challenge {
            v: PROTOCOL_VERSION,
            min_v: MIN_PROTOCOL_VERSION,
            nonce: self.nonce.clone(),
        };
        self.send(&challenge)?;
        let started = Instant::now();
        let mut last_heard = started;
        let mut last_ping = started;
        loop {
            self.ws.get_mut().until = Instant::now() + POLL_INTERVAL;
            while let Some(outbound) = self.inbox.try_recv() {
                match outbound {
                    Outbound::Frame(text) => written(self.ws.write(Message::text(text)))?,
                    Outbound::Close(code, reason) => return Err(Exit::Close(code, reason)),
                }
            }
            written(self.ws.flush())?;

            let now = Instant::now();
            if self.fp.is_none() && now.duration_since(started) >= AUTH_TIMEOUT {
                return Err(Exit::Close(CLOSE_UNAUTHORIZED, "authentication timed out"));
            }
            if now.duration_since(last_heard) >= SILENCE_LIMIT {
                return Err(Exit::Gone);
            }
            if now.duration_since(last_ping) >= PING_INTERVAL {
                written(self.ws.write(Message::Ping(Default::default())))?;
                last_ping = now;
            }

            match self.ws.read() {
                Ok(message) => {
                    let now = Instant::now();
                    last_heard = now;
                    if !self.limits.allow_frame(message.len(), now) {
                        return Err(Exit::Close(CLOSE_RATE_LIMITED, "rate limited"));
                    }
                    match message {
                        Message::Text(text) => self.on_text(text.as_str())?,
                        Message::Binary(_) | Message::Frame(_) => {
                            return Err(Exit::Close(CLOSE_BAD_REQUEST, "text frames only"))
                        }
                        Message::Ping(_) | Message::Pong(_) => {}
                        Message::Close(_) => return Err(Exit::Gone),
                    }
                }
                Err(error) if would_block(&error) => {}
                Err(tungstenite::Error::Capacity(_)) => {
                    return Err(Exit::Close(CLOSE_TOO_BIG, "frame too large"))
                }
                Err(_) => return Err(Exit::Gone),
            }
        }
    }

    fn on_text(&mut self, text: &str) -> Result<(), Exit> {
        let Ok(message) = serde_json::from_str::<ClientMessage>(text) else {
            return Err(Exit::Close(CLOSE_BAD_REQUEST, "malformed frame"));
        };
        let Some(fp) = self.fp.clone() else {
            return match message {
                ClientMessage::Auth { v, cert, sig } => self.authenticate(v, &cert, &sig),
                _ => Err(Exit::Close(CLOSE_UNAUTHORIZED, "authenticate first")),
            };
        };
        let shared = self.shared;
        match message {
            ClientMessage::Auth { .. } => {
                Err(Exit::Close(CLOSE_BAD_REQUEST, "already authenticated"))
            }
            ClientMessage::Allow { fps } => {
                if fps.len() > MAX_ALLOW || fps.iter().any(|fp| fp.len() > MAX_FP_LEN) {
                    return Err(Exit::Close(CLOSE_BAD_REQUEST, "allow list too large"));
                }
                lock(&shared.hub).set_allow(&fp, self.conn, fps);
                Ok(())
            }
            ClientMessage::Send { to, data } => {
                let result = lock(&shared.hub).forward(&fp, self.conn, &to, data);
                match result {
                    Ok(()) => {
                        bump(&shared.metrics.messages_forwarded);
                        Ok(())
                    }
                    Err(code) => self.send(&ServerMessage::Error { code, to }),
                }
            }
            ClientMessage::Turn { peer } => self.turn(&fp, peer),
        }
    }

    fn authenticate(&mut self, v: u32, cert: &str, sig: &str) -> Result<(), Exit> {
        let shared = self.shared;
        if !lock(&shared.limits).allow_auth(self.net, Instant::now()) {
            return Err(Exit::Close(
                CLOSE_RATE_LIMITED,
                "too many authentication attempts",
            ));
        }
        if v < MIN_PROTOCOL_VERSION {
            return Err(Exit::Close(
                CLOSE_UPGRADE_REQUIRED,
                "protocol upgrade required",
            ));
        }
        let Some(fp) = auth::verify(cert, sig, &self.nonce, &shared.config.public_host) else {
            bump(&shared.metrics.auth_fail);
            return Err(Exit::Close(CLOSE_UNAUTHORIZED, "authentication failed"));
        };
        bump(&shared.metrics.auth_ok);
        lock(&shared.limits).authenticated();
        self.fp = Some(fp.clone());
        let max = shared.config.limits.max_frame_bytes;
        self.ws.set_config(|config| {
            config.max_message_size = Some(max);
            config.max_frame_size = Some(max);
        });
        // Welcome is written before registering, so it precedes any presence
        // the hub queues for this connection.
        self.send(&ServerMessage::Welcome { fp: fp.clone() })?;
        lock(&shared.hub).register(&fp, self.conn, self.outbox.clone());
        Ok(())
    }

    fn turn(&mut self, fp: &str, peer: String) -> Result<(), Exit> {
        let shared = self.shared;
        let refuse = |this: &mut Self, code: ErrorCode, to: String| {
            bump(&shared.metrics.turn_denials);
            this.send(&ServerMessage::Error { code, to })
        };
        if !shared.turn.is_enabled() || !lock(&shared.hub).can_turn(fp, self.conn, &peer) {
            return refuse(self, ErrorCode::TurnDenied, peer);
        }
        let now = unix_now();
        let grant = lock(&shared.quota).try_grant(fp, &peer, self.net, now, &shared.config.limits);
        let Ok(grant) = grant else {
            return refuse(self, ErrorCode::TurnQuota, peer);
        };
        // The backend may take seconds (Cloudflare); no lock is held here.
        match shared.turn.credentials(fp, now) {
            Ok((ice_servers, ttl)) => {
                bump(&shared.metrics.turn_grants);
                self.send(&ServerMessage::TurnCreds {
                    peer,
                    ice_servers,
                    ttl,
                })
            }
            Err(_) => {
                lock(&shared.quota).refund(grant);
                refuse(self, ErrorCode::TurnDenied, peer)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::test_support::Identity;
    use crate::config::{CoturnConfig, TurnBackendKind};
    use crate::protocol::{CLOSE_REPLACED, CLOSE_UNAUTHORIZED};
    use serde_json::json;

    const HOST: &str = "relay.test";

    fn start() -> SocketAddr {
        let config = Config {
            listen: "127.0.0.1:0".into(),
            public_host: HOST.into(),
            turn_backend: TurnBackendKind::Coturn,
            coturn: CoturnConfig {
                static_secret: "secret".into(),
                urls: vec!["turn:turn.test:3478?transport=udp".into()],
                ttl: 600,
            },
            ..Config::default()
        };
        let server = Server::bind(Shared::new(config, TurnQuota::default())).unwrap();
        let addr = server.local_addr().unwrap();
        thread::spawn(move || server.run());
        addr
    }

    type Client = WebSocket<TcpStream>;

    fn connect(addr: SocketAddr) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        tungstenite::client(format!("ws://{addr}/v1"), stream)
            .unwrap()
            .0
    }

    fn recv(ws: &mut Client) -> ServerMessage {
        loop {
            if let Message::Text(text) = ws.read().unwrap() {
                return serde_json::from_str(text.as_str()).unwrap();
            }
        }
    }

    fn send(ws: &mut Client, message: ClientMessage) {
        ws.send(Message::text(serde_json::to_string(&message).unwrap()))
            .unwrap();
    }

    fn close_code(ws: &mut Client) -> u16 {
        loop {
            match ws.read() {
                Ok(Message::Close(Some(frame))) => return frame.code.into(),
                Ok(_) => {}
                Err(error) => panic!("no close frame: {error}"),
            }
        }
    }

    fn challenge(ws: &mut Client) -> String {
        match recv(ws) {
            ServerMessage::Challenge {
                v: 1,
                min_v: 1,
                nonce,
            } => nonce,
            other => panic!("expected challenge, got {other:?}"),
        }
    }

    fn login(addr: SocketAddr, identity: &Identity) -> Client {
        let mut ws = connect(addr);
        let nonce = challenge(&mut ws);
        send(
            &mut ws,
            ClientMessage::Auth {
                v: 1,
                cert: identity.cert_b64(),
                sig: identity.sign(&nonce, HOST),
            },
        );
        assert_eq!(recv(&mut ws), ServerMessage::Welcome { fp: identity.fp() });
        ws
    }

    fn allow(ws: &mut Client, fp: &str) {
        send(
            ws,
            ClientMessage::Allow {
                fps: vec![fp.to_string()],
            },
        );
    }

    fn presence(fp: &str, online: bool) -> ServerMessage {
        ServerMessage::Presence {
            fp: fp.into(),
            online,
        }
    }

    #[test]
    fn mutual_devices_see_each_other_and_exchange_messages_strangers_do_not() {
        let addr = start();
        let (a_id, b_id, c_id) = (Identity::ec(), Identity::rsa(), Identity::ec());
        let (a_fp, b_fp, c_fp) = (a_id.fp(), b_id.fp(), c_id.fp());
        let mut a = login(addr, &a_id);
        let mut b = login(addr, &b_id);

        allow(&mut a, &b_fp);
        allow(&mut b, &a_fp);
        assert_eq!(recv(&mut a), presence(&b_fp, true));
        assert_eq!(recv(&mut b), presence(&a_fp, true));

        send(
            &mut a,
            ClientMessage::Send {
                to: b_fp.clone(),
                data: json!("ciphertext"),
            },
        );
        assert_eq!(
            recv(&mut b),
            ServerMessage::Msg {
                from: a_fp.clone(),
                data: json!("ciphertext")
            }
        );

        // C allows A, but A never allowed C.
        let mut c = login(addr, &c_id);
        allow(&mut c, &a_fp);
        send(
            &mut c,
            ClientMessage::Send {
                to: a_fp.clone(),
                data: json!("hi"),
            },
        );
        assert_eq!(
            recv(&mut c),
            ServerMessage::Error {
                code: ErrorCode::Offline,
                to: a_fp.clone()
            }
        );
        send(
            &mut c,
            ClientMessage::Send {
                to: b_fp.clone(),
                data: json!("hi"),
            },
        );
        assert_eq!(
            recv(&mut c),
            ServerMessage::Error {
                code: ErrorCode::NotAllowed,
                to: b_fp.clone()
            }
        );
        send(&mut c, ClientMessage::Turn { peer: a_fp.clone() });
        assert_eq!(
            recv(&mut c),
            ServerMessage::Error {
                code: ErrorCode::TurnDenied,
                to: a_fp.clone()
            }
        );

        // A's next frame is B's message, so nothing about C was queued first.
        send(
            &mut b,
            ClientMessage::Send {
                to: a_fp.clone(),
                data: json!({"n": 2}),
            },
        );
        assert_eq!(
            recv(&mut a),
            ServerMessage::Msg {
                from: b_fp.clone(),
                data: json!({"n": 2})
            }
        );

        send(&mut a, ClientMessage::Turn { peer: b_fp.clone() });
        match recv(&mut a) {
            ServerMessage::TurnCreds {
                peer,
                ice_servers,
                ttl,
            } => {
                assert_eq!((peer, ttl), (b_fp.clone(), 600));
                let username = ice_servers[0].username.clone().unwrap();
                assert!(username.ends_with(&format!(":{}", &a_fp[..16])));
            }
            other => panic!("expected TURN credentials, got {other:?}"),
        }

        // A reconnects: the old socket gets 4409, B sees A leave and return.
        let mut a2 = login(addr, &a_id);
        assert_eq!(close_code(&mut a), CLOSE_REPLACED);
        assert_eq!(recv(&mut b), presence(&a_fp, false));
        allow(&mut a2, &b_fp);
        assert_eq!(recv(&mut a2), presence(&b_fp, true));
        assert_eq!(recv(&mut b), presence(&a_fp, true));

        // C still never heard of anyone.
        drop(a2);
        assert_eq!(recv(&mut b), presence(&a_fp, false));
        send(&mut c, ClientMessage::Allow { fps: vec![] });
        send(
            &mut c,
            ClientMessage::Send {
                to: c_fp.clone(),
                data: json!(0),
            },
        );
        assert_eq!(
            recv(&mut c),
            ServerMessage::Error {
                code: ErrorCode::NotAllowed,
                to: c_fp
            }
        );
    }

    #[test]
    fn bad_auth_is_closed_with_4401_and_old_versions_with_4426() {
        let addr = start();
        let identity = Identity::ec();

        let mut ws = connect(addr);
        let nonce = challenge(&mut ws);
        send(
            &mut ws,
            ClientMessage::Auth {
                v: 1,
                cert: identity.cert_b64(),
                sig: identity.sign(&nonce, "other.host"),
            },
        );
        assert_eq!(close_code(&mut ws), CLOSE_UNAUTHORIZED);

        let mut ws = connect(addr);
        challenge(&mut ws);
        send(&mut ws, ClientMessage::Allow { fps: vec![] });
        assert_eq!(close_code(&mut ws), CLOSE_UNAUTHORIZED);

        let mut ws = connect(addr);
        let nonce = challenge(&mut ws);
        send(
            &mut ws,
            ClientMessage::Auth {
                v: 0,
                cert: identity.cert_b64(),
                sig: identity.sign(&nonce, HOST),
            },
        );
        assert_eq!(close_code(&mut ws), CLOSE_UPGRADE_REQUIRED);

        let mut ws = login(addr, &identity);
        ws.send(Message::text("not json")).unwrap();
        assert_eq!(close_code(&mut ws), CLOSE_BAD_REQUEST);
    }

    #[test]
    fn a_client_drip_feeding_a_frame_before_auth_is_closed_on_time() {
        let addr = start();
        let mut ws = connect(addr);
        challenge(&mut ws);
        let started = Instant::now();
        let raw = ws.get_mut();
        raw.set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        // A masked text frame announcing 8000 bytes, fed one byte at a time.
        raw.write_all(&[0x81, 0xFE, 0x1F, 0x40, 1, 2, 3, 4])
            .unwrap();
        let mut reply = Vec::new();
        while reply.len() < 4 && started.elapsed() < Duration::from_secs(15) {
            let _ = raw.write_all(b"x");
            let mut chunk = [0u8; 64];
            match raw.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => reply.extend_from_slice(&chunk[..read]),
                Err(_) => {}
            }
        }
        let elapsed = started.elapsed();
        assert!(elapsed < Duration::from_secs(11), "took {elapsed:?}");
        assert!(elapsed >= Duration::from_secs(9), "took {elapsed:?}");
        // Close frame carrying 4401.
        assert_eq!(reply[0], 0x88, "{reply:?}");
        assert_eq!(u16::from_be_bytes([reply[2], reply[3]]), CLOSE_UNAUTHORIZED);
    }

    #[test]
    fn frames_over_16_kib_are_refused_before_auth_but_not_after() {
        let addr = start();
        let mut ws = connect(addr);
        challenge(&mut ws);
        ws.send(Message::text("x".repeat(PRE_AUTH_MAX_FRAME + 1)))
            .unwrap();
        assert_eq!(close_code(&mut ws), CLOSE_TOO_BIG);

        let (a_id, b_id) = (Identity::ec(), Identity::ec());
        let mut a = login(addr, &a_id);
        let mut b = login(addr, &b_id);
        allow(&mut a, &b_id.fp());
        allow(&mut b, &a_id.fp());
        recv(&mut a);
        recv(&mut b);
        let big = "y".repeat(100 * 1024);
        send(
            &mut a,
            ClientMessage::Send {
                to: b_id.fp(),
                data: json!(big),
            },
        );
        assert_eq!(
            recv(&mut b),
            ServerMessage::Msg {
                from: a_id.fp(),
                data: json!(big)
            }
        );
    }

    fn http_get(addr: SocketAddr, path: &str) -> String {
        let mut stream = TcpStream::connect(addr).unwrap();
        write!(stream, "GET {path} HTTP/1.1\r\nHost: relay.test\r\n\r\n").unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    }

    #[test]
    fn health_and_metrics_answer_plain_http_and_other_paths_do_not_upgrade() {
        let addr = start();
        let health = http_get(addr, "/healthz");
        assert!(health.starts_with("HTTP/1.1 200 OK\r\n"), "{health}");
        assert!(health.ends_with("\r\n\r\nok"), "{health}");

        let metrics = http_get(addr, "/metrics");
        assert!(
            metrics.contains("desklink_relay_auth_ok_total 0"),
            "{metrics}"
        );
        assert!(metrics.contains("# TYPE desklink_relay_connections gauge"));

        assert!(http_get(addr, "/").starts_with("HTTP/1.1 404 "));
        let stream = TcpStream::connect(addr).unwrap();
        assert!(tungstenite::client(format!("ws://{addr}/v2"), stream).is_err());
    }

    #[test]
    fn forwarded_for_takes_the_first_entry() {
        let request = Request::builder()
            .header("X-Forwarded-For", "2001:db8::7, 10.0.0.1")
            .body(())
            .unwrap();
        assert_eq!(
            forwarded_for(&request),
            Some("2001:db8::7".parse().unwrap())
        );
        let garbage = Request::builder()
            .header("X-Forwarded-For", "nonsense")
            .body(())
            .unwrap();
        assert_eq!(forwarded_for(&garbage), None);
    }
}
