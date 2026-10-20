//! The desktop's connection to the DeskLink relay.
//!
//! One thread owns one WebSocket. It authenticates with the device key,
//! allows every paired device, runs the end-to-end link
//! ([`super::handshake`]) with each one that comes online, and turns an
//! established link into the device's bootstrap link when the LAN cannot
//! serve: no live LAN link and no working WebRTC, or the peer is already
//! signaling through the relay. It also fetches TURN credentials for the
//! WebRTC coordinator. See docs/RELAY.md.
//!
//! Reconnects walk a short ladder and then stop in an explicit
//! "unavailable" state; a network, settings or pairing change starts over.
//! Nothing here ever loops on its own.

use std::collections::HashMap;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use desklink_relay::protocol::{
    auth_sign_input, ClientMessage, ErrorCode, IceServer, ServerMessage, CLOSE_REPLACED,
    CLOSE_UPGRADE_REQUIRED, PROTOCOL_VERSION,
};
use openssl::hash::MessageDigest;
use openssl::sign::Signer;
use openssl::x509::X509;
use serde_json::Value;
use tungstenite::protocol::frame::coding::CloseCode;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

use super::crypto;
use super::handshake::{Event, Identities, PeerLink};
use super::transport::{allowed_on_relay, Outbound, RelayLinkTransport};
use crate::device_links::config::{app_settings, Config};
use crate::device_links::core::{DeviceManager, SessionLink};
use crate::device_links::daemon::DaemonEvent;
use crate::device_links::device::DeviceView;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_HEARTBEAT};

const LADDER: [Duration; 6] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(30),
];
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);
const PING_EVERY: Duration = Duration::from_secs(20);
const SILENCE_LIMIT: Duration = Duration::from_secs(60);
const HANDSHAKE_SPACING: Duration = Duration::from_secs(2);
const TURN_TIMEOUT: Duration = Duration::from_secs(2);
/// A handshake not finished by then (a lost frame) is started again.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// What the Preferences window shows.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RelayStatus {
    #[default]
    Off,
    Connecting,
    Online,
    Unavailable(String),
    UpdateRequired,
}

pub enum Command {
    /// The network, the settings or the trust store changed: start over.
    Reset,
    Turn {
        device_id: String,
        reply: Sender<Vec<IceServer>>,
    },
}

fn status_slot() -> &'static Mutex<RelayStatus> {
    static STATUS: OnceLock<Mutex<RelayStatus>> = OnceLock::new();
    STATUS.get_or_init(Default::default)
}

pub fn status() -> RelayStatus {
    status_slot()
        .lock()
        .map(|status| status.clone())
        .unwrap_or_default()
}

fn set_status(status: RelayStatus) {
    if let Ok(mut slot) = status_slot().lock() {
        *slot = status;
    }
}

fn commands() -> &'static Mutex<Option<Sender<Command>>> {
    static COMMANDS: OnceLock<Mutex<Option<Sender<Command>>>> = OnceLock::new();
    COMMANDS.get_or_init(Default::default)
}

/// Asks the client to reconnect now (network/settings/pairing changed).
pub fn reset() {
    if let Some(sender) = commands().lock().ok().and_then(|tx| tx.clone()) {
        let _ = sender.send(Command::Reset);
    }
}

/// TURN (and STUN) servers for a WebRTC peer with `device_id`, or empty when
/// the relay cannot provide them within [`TURN_TIMEOUT`].
pub fn ice_servers_for(device_id: &str) -> Vec<IceServer> {
    let Some(sender) = commands().lock().ok().and_then(|tx| tx.clone()) else {
        return Vec::new();
    };
    let (reply, answer) = channel();
    if sender
        .send(Command::Turn {
            device_id: device_id.to_string(),
            reply,
        })
        .is_err()
    {
        return Vec::new();
    }
    answer.recv_timeout(TURN_TIMEOUT).unwrap_or_default()
}

/// Everything the client needs from the daemon.
pub struct Context {
    pub config: Arc<Mutex<Config>>,
    pub devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    pub sessions: Arc<DeviceManager>,
    pub events: Arc<Mutex<Vec<DaemonEvent>>>,
    pub shutdown: Arc<std::sync::atomic::AtomicBool>,
    /// Tests point the client at a local relay; production reads the setting.
    pub relay_url: Option<String>,
}

pub fn start(context: Context) {
    let (tx, rx) = channel();
    if let Ok(mut slot) = commands().lock() {
        *slot = Some(tx);
    }
    let _ = thread::Builder::new()
        .name("desklink-relay".into())
        .spawn(move || Client::new(context, rx).run());
}

/// One paired device as the relay sees it.
struct Peer {
    device_id: String,
    certificate_pem: String,
    remote_public_der: Vec<u8>,
    link: PeerLink,
    online: bool,
    last_start: Option<Instant>,
    /// The device's identity from the handshake, once established.
    identity: Option<DeviceInfo>,
    /// The transport while this link is the device's bootstrap link.
    attached: Option<Arc<RelayLinkTransport>>,
    /// TURN requests waiting for an answer.
    turn_waiters: Vec<Sender<Vec<IceServer>>>,
}

type Socket = WebSocket<MaybeTlsStream<TcpStream>>;

struct Client {
    context: Context,
    commands: Receiver<Command>,
    outbound_tx: Sender<Outbound>,
    outbound_rx: Receiver<Outbound>,
    rung: usize,
    retry_at: Option<Instant>,
    /// Stopped after the ladder ran out, or the relay requires an update.
    stopped: bool,
}

impl Client {
    fn new(context: Context, commands: Receiver<Command>) -> Self {
        let (outbound_tx, outbound_rx) = channel();
        Self {
            context,
            commands,
            outbound_tx,
            outbound_rx,
            rung: 0,
            retry_at: Some(Instant::now()),
            stopped: false,
        }
    }

    fn shutting_down(&self) -> bool {
        self.context
            .shutdown
            .load(std::sync::atomic::Ordering::Acquire)
    }

    fn wanted(&self) -> bool {
        app_settings().relay_enabled
            && self
                .context
                .config
                .lock()
                .is_ok_and(|config| config.has_trusted_devices())
    }

    fn run(mut self) {
        while !self.shutting_down() {
            if !self.wanted() {
                set_status(RelayStatus::Off);
                self.idle(Duration::from_secs(2));
                continue;
            }
            let due = self.retry_at.is_some_and(|at| Instant::now() >= at);
            if self.stopped || !due {
                self.idle(Duration::from_millis(500));
                continue;
            }
            set_status(RelayStatus::Connecting);
            match self.session() {
                Ok(()) => {}
                // Another DeskLink with this identity connected: reconnecting
                // would knock it off in turn, forever. Wait for a reset.
                Err(Stop::Replaced) => {
                    self.stopped = true;
                    set_status(RelayStatus::Unavailable(
                        "another DeskLink on this computer is connected".into(),
                    ));
                    continue;
                }
                Err(Stop::UpdateRequired) => {
                    self.stopped = true;
                    set_status(RelayStatus::UpdateRequired);
                    continue;
                }
                Err(Stop::Reset) => {
                    self.rung = 0;
                    self.retry_at = Some(Instant::now());
                    continue;
                }
                Err(Stop::Failed(reason)) => {
                    if let Some(delay) = LADDER.get(self.rung) {
                        self.rung += 1;
                        self.retry_at = Some(Instant::now() + *delay);
                        set_status(RelayStatus::Connecting);
                    } else {
                        eprintln!("[DeskLink] Relay unavailable: {reason}");
                        self.stopped = true;
                        set_status(RelayStatus::Unavailable(reason));
                    }
                    continue;
                }
            }
            self.retry_at = Some(Instant::now() + LADDER[0]);
        }
    }

    /// Waits for a command while not connected. A reset starts over.
    fn idle(&mut self, wait: Duration) {
        match self.commands.recv_timeout(wait) {
            Ok(Command::Reset) => {
                self.rung = 0;
                self.stopped = false;
                self.retry_at = Some(Instant::now());
            }
            Ok(Command::Turn { reply, .. }) => {
                let _ = reply.send(Vec::new());
            }
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
    }

    /// One connection, until it ends.
    fn session(&mut self) -> Result<(), Stop> {
        let url = self
            .context
            .relay_url
            .clone()
            .unwrap_or_else(|| app_settings().relay_url().to_string());
        let host = host_of(&url).ok_or_else(|| Stop::Failed("Invalid relay address".into()))?;
        let mut socket = connect(&url, &host).map_err(Stop::Failed)?;
        let (local_fp, mut peers) = self.load_peers()?;

        // Challenge → auth → welcome.
        let nonce = match read_message(&mut socket, Duration::from_secs(10))? {
            ServerMessage::Challenge { nonce, min_v, .. } => {
                if min_v > PROTOCOL_VERSION {
                    return Err(Stop::UpdateRequired);
                }
                nonce
            }
            _ => return Err(Stop::Failed("The relay did not challenge".into())),
        };
        let auth = self.auth(&nonce, &host)?;
        send(&mut socket, &auth)?;
        match read_message(&mut socket, Duration::from_secs(10))? {
            ServerMessage::Welcome { fp } if fp == local_fp => {}
            _ => return Err(Stop::Failed("The relay refused this computer".into())),
        }
        send(
            &mut socket,
            &ClientMessage::Allow {
                fps: peers.keys().cloned().collect(),
            },
        )?;
        set_status(RelayStatus::Online);
        self.rung = 0;

        let mut last_inbound = Instant::now();
        let mut last_ping = Instant::now();
        let mut last_tick = Instant::now();
        let result = loop {
            if self.shutting_down() {
                break Ok(());
            }
            if let Err(stop) = self.drain_commands(&mut socket, &mut peers) {
                break Err(stop);
            }
            if let Err(stop) = self.drain_outbound(&mut socket, &mut peers) {
                break Err(stop);
            }
            match socket.read() {
                Ok(Message::Text(text)) => {
                    last_inbound = Instant::now();
                    match serde_json::from_str::<ServerMessage>(&text) {
                        Ok(message) => {
                            if let Err(stop) = self.on_message(&mut socket, &mut peers, message) {
                                break Err(stop);
                            }
                        }
                        Err(_) => break Err(Stop::Failed("Malformed relay frame".into())),
                    }
                }
                Ok(Message::Close(frame)) => {
                    let code = frame.map(|frame| u16::from(frame.code));
                    break Err(match code {
                        Some(CLOSE_REPLACED) => Stop::Replaced,
                        Some(CLOSE_UPGRADE_REQUIRED) => Stop::UpdateRequired,
                        _ => Stop::Failed(format!("The relay closed the connection ({code:?})")),
                    });
                }
                Ok(_) => last_inbound = Instant::now(),
                Err(tungstenite::Error::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => break Err(Stop::Failed(error.to_string())),
            }
            if last_inbound.elapsed() > SILENCE_LIMIT {
                break Err(Stop::Failed("The relay stopped answering".into()));
            }
            if last_ping.elapsed() > PING_EVERY {
                last_ping = Instant::now();
                if socket.send(Message::Ping(Default::default())).is_err() {
                    break Err(Stop::Failed("Lost the relay connection".into()));
                }
            }
            if last_tick.elapsed() >= Duration::from_secs(1) {
                last_tick = Instant::now();
                if let Err(stop) = self.restart_stalled(&mut socket, &mut peers) {
                    break Err(stop);
                }
                self.attach_where_needed(&mut peers);
            }
        };
        // Every relay link dies with the connection.
        for peer in peers.values_mut() {
            detach(peer);
            for waiter in peer.turn_waiters.drain(..) {
                let _ = waiter.send(Vec::new());
            }
        }
        let _ = socket.close(Some(tungstenite::protocol::CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        }));
        result
    }

    fn auth(&self, nonce: &str, host: &str) -> Result<ClientMessage, Stop> {
        let config = self
            .context
            .config
            .lock()
            .map_err(|_| Stop::Failed("Config lock poisoned".into()))?;
        let cert = config
            .certificate()
            .to_der()
            .map_err(|error| Stop::Failed(error.to_string()))?;
        let mut signer = Signer::new(MessageDigest::sha256(), config.key())
            .map_err(|error| Stop::Failed(error.to_string()))?;
        signer
            .update(&auth_sign_input(nonce, host))
            .map_err(|error| Stop::Failed(error.to_string()))?;
        let sig = signer
            .sign_to_vec()
            .map_err(|error| Stop::Failed(error.to_string()))?;
        Ok(ClientMessage::Auth {
            v: PROTOCOL_VERSION,
            cert: BASE64.encode(cert),
            sig: BASE64.encode(sig),
        })
    }

    /// This device's fingerprint and one [`Peer`] per paired device, keyed by
    /// the fingerprint of its stored certificate.
    fn load_peers(&self) -> Result<(String, HashMap<String, Peer>), Stop> {
        let config = self
            .context
            .config
            .lock()
            .map_err(|_| Stop::Failed("Config lock poisoned".into()))?;
        let local_der = config
            .certificate()
            .to_der()
            .map_err(|error| Stop::Failed(error.to_string()))?;
        let local_fp = crypto::fingerprint(&local_der);
        let mut identity = config
            .local_device_info()
            .to_identity_packet(0)
            .serialize_line()
            .map_err(|error| Stop::Failed(error.to_string()))?;
        while identity.last() == Some(&b'\n') {
            identity.pop();
        }
        let local_identity =
            String::from_utf8(identity).map_err(|error| Stop::Failed(error.to_string()))?;
        let mut peers = HashMap::new();
        for device in config.trusted_devices() {
            let Ok(certificate) = X509::from_pem(device.certificate_pem.as_bytes()) else {
                continue;
            };
            let (Ok(der), Ok(public)) = (certificate.to_der(), certificate.public_key()) else {
                continue;
            };
            let Ok(remote_public_der) = public.public_key_to_der() else {
                continue;
            };
            let fp = crypto::fingerprint(&der);
            let link = PeerLink::new(Identities {
                local_fp: local_fp.clone(),
                peer_fp: fp.clone(),
                local_key: config.key().clone(),
                local_identity: local_identity.clone(),
                peer_key: public,
                peer_device_id: device.id.clone(),
            });
            peers.insert(
                fp,
                Peer {
                    device_id: device.id.clone(),
                    certificate_pem: device.certificate_pem.clone(),
                    remote_public_der,
                    link,
                    online: false,
                    last_start: None,
                    identity: None,
                    attached: None,
                    turn_waiters: Vec::new(),
                },
            );
        }
        Ok((local_fp, peers))
    }

    fn drain_commands(
        &mut self,
        socket: &mut Socket,
        peers: &mut HashMap<String, Peer>,
    ) -> Result<(), Stop> {
        while let Ok(command) = self.commands.try_recv() {
            match command {
                Command::Reset => return Err(Stop::Reset),
                Command::Turn { device_id, reply } => {
                    let Some((fp, peer)) = peers
                        .iter_mut()
                        .find(|(_, peer)| peer.device_id == device_id && peer.online)
                    else {
                        let _ = reply.send(Vec::new());
                        continue;
                    };
                    peer.turn_waiters.push(reply);
                    send(socket, &ClientMessage::Turn { peer: fp.clone() })?;
                }
            }
        }
        Ok(())
    }

    fn drain_outbound(
        &mut self,
        socket: &mut Socket,
        peers: &mut HashMap<String, Peer>,
    ) -> Result<(), Stop> {
        while let Ok(outbound) = self.outbound_rx.try_recv() {
            let Some(peer) = peers.get_mut(&outbound.fp) else {
                continue;
            };
            // A frame for a link that is already gone is dropped: its sender
            // saw the transport close.
            if let Ok(frame) = peer.link.seal(&outbound.plaintext) {
                send_data(socket, &outbound.fp, frame)?;
            }
        }
        Ok(())
    }

    fn on_message(
        &mut self,
        socket: &mut Socket,
        peers: &mut HashMap<String, Peer>,
        message: ServerMessage,
    ) -> Result<(), Stop> {
        match message {
            ServerMessage::Presence { fp, online } => {
                let Some(peer) = peers.get_mut(&fp) else {
                    return Ok(());
                };
                peer.online = online;
                if online {
                    // Only the initiator opens; the responder waits for `h1`
                    // and kicks only after a failure (docs/RELAY.md §2).
                    if peer.link.is_initiator() {
                        self.start_link(socket, &fp, peer)?;
                    }
                } else {
                    peer.link.reset();
                    peer.identity = None;
                    detach(peer);
                    for waiter in peer.turn_waiters.drain(..) {
                        let _ = waiter.send(Vec::new());
                    }
                }
            }
            ServerMessage::Msg { from, data } => {
                let Some(peer) = peers.get_mut(&from) else {
                    return Ok(());
                };
                match peer.link.receive(&data) {
                    Ok(events) => {
                        for event in events {
                            self.on_link_event(socket, &from, peer, event)?;
                        }
                    }
                    Err(error) => {
                        eprintln!(
                            "[DeskLink] Relay link with {} reset: {error}",
                            peer.device_id
                        );
                        peer.identity = None;
                        detach(peer);
                        self.start_link(socket, &from, peer)?;
                    }
                }
            }
            ServerMessage::TurnCreds {
                peer, ice_servers, ..
            } => {
                if let Some(peer) = peers.get_mut(&peer) {
                    for waiter in peer.turn_waiters.drain(..) {
                        let _ = waiter.send(ice_servers.clone());
                    }
                }
            }
            ServerMessage::Error { code, to } => {
                if let Some(peer) = peers.get_mut(&to) {
                    if matches!(code, ErrorCode::TurnDenied | ErrorCode::TurnQuota) {
                        for waiter in peer.turn_waiters.drain(..) {
                            let _ = waiter.send(Vec::new());
                        }
                    }
                }
            }
            ServerMessage::Challenge { .. } | ServerMessage::Welcome { .. } => {}
        }
        Ok(())
    }

    /// (Re)starts the handshake, at most once per [`HANDSHAKE_SPACING`].
    fn start_link(&self, socket: &mut Socket, fp: &str, peer: &mut Peer) -> Result<(), Stop> {
        if peer
            .last_start
            .is_some_and(|last| last.elapsed() < HANDSHAKE_SPACING)
        {
            return Ok(());
        }
        peer.last_start = Some(Instant::now());
        match peer.link.start() {
            Ok(first) => send_data(socket, fp, first),
            Err(error) => {
                eprintln!("[DeskLink] Relay link could not start: {error}");
                Ok(())
            }
        }
    }

    fn on_link_event(
        &mut self,
        socket: &mut Socket,
        fp: &str,
        peer: &mut Peer,
        event: Event,
    ) -> Result<(), Stop> {
        match event {
            Event::Send(value) => send_data(socket, fp, value)?,
            Event::Established { identity } => {
                let info = NetworkPacket::deserialize(identity.as_bytes())
                    .ok()
                    .and_then(|packet| DeviceInfo::from_identity_packet(&packet));
                peer.identity = info;
                // A new session replaces whatever the old one carried.
                detach(peer);
                debug_log!("[DeskLink] Relay link with {} established", peer.device_id);
                self.attach_if_needed(peer, false);
            }
            Event::Packet(bytes) => {
                if let Some(transport) = peer.attached.as_ref().filter(|t| !t.is_closed()) {
                    transport.deliver(&bytes);
                    return Ok(());
                }
                // The peer is signaling through the relay, so its LAN path is
                // gone even if this side has not noticed yet: take over now.
                let Ok(packet) = NetworkPacket::deserialize(&bytes) else {
                    return Ok(());
                };
                if allowed_on_relay(&packet) && packet.packet_type != PACKET_TYPE_HEARTBEAT {
                    if let Some(transport) = self.attach_if_needed(peer, true) {
                        transport.deliver(&bytes);
                    }
                }
            }
        }
        Ok(())
    }

    fn restart_stalled(
        &self,
        socket: &mut Socket,
        peers: &mut HashMap<String, Peer>,
    ) -> Result<(), Stop> {
        for (fp, peer) in peers.iter_mut() {
            let stalled = peer.online
                && peer.link.is_initiator()
                && !peer.link.is_established()
                && peer
                    .last_start
                    .is_none_or(|last| last.elapsed() > HANDSHAKE_TIMEOUT);
            if stalled {
                self.start_link(socket, fp, peer)?;
            }
        }
        Ok(())
    }

    fn attach_where_needed(&mut self, peers: &mut HashMap<String, Peer>) {
        for peer in peers.values_mut() {
            if peer.attached.as_ref().is_some_and(|t| t.is_closed()) {
                peer.attached = None;
            }
            self.attach_if_needed(peer, false);
        }
    }

    /// Makes this link `peer`'s bootstrap link when the LAN cannot serve, or
    /// unconditionally with `force`. Returns the transport when attached.
    fn attach_if_needed(&self, peer: &mut Peer, force: bool) -> Option<Arc<RelayLinkTransport>> {
        if peer.attached.is_some() || !peer.link.is_established() {
            return peer.attached.clone();
        }
        let info = peer.identity.clone()?;
        let sessions = &self.context.sessions;
        if !force && !sessions.wants_bootstrap_connection(&peer.device_id) {
            return None;
        }
        let local_public_der = self
            .context
            .config
            .lock()
            .ok()?
            .key()
            .public_key_to_der()
            .ok()?;
        let fp = crypto::fingerprint(
            &X509::from_pem(peer.certificate_pem.as_bytes())
                .ok()?
                .to_der()
                .ok()?,
        );
        let transport = Arc::new(RelayLinkTransport::new(fp, self.outbound_tx.clone()));
        let link = SessionLink::new(
            Arc::clone(&transport) as Arc<dyn crate::device_links::core::LinkTransport>,
            peer.certificate_pem.clone(),
            local_public_der,
            peer.remote_public_der.clone(),
            info,
        );
        let attached = crate::device_links::daemon::handshake::attach_bootstrap_link(
            &peer.device_id,
            link,
            Arc::clone(&transport) as Arc<dyn crate::device_links::core::LinkTransport>,
            true,
            true,
            Arc::clone(&self.context.config),
            Arc::clone(&self.context.devices),
            Arc::clone(sessions),
            Arc::clone(&self.context.events),
        );
        match attached {
            Ok(()) => {
                debug_log!(
                    "[DL-WRTC-BOOT] relay link is now the bootstrap link for {}",
                    peer.device_id
                );
                peer.attached = Some(Arc::clone(&transport));
                Some(transport)
            }
            Err(error) => {
                eprintln!("[DeskLink] Could not use the relay link: {error}");
                None
            }
        }
    }
}

fn detach(peer: &mut Peer) {
    if let Some(transport) = peer.attached.take() {
        // The reader sees `Closed` and runs the normal disconnect path.
        crate::device_links::core::LinkTransport::shutdown(transport.as_ref());
    }
}

enum Stop {
    Failed(String),
    Replaced,
    UpdateRequired,
    Reset,
}

/// The part after the scheme. Plain `ws://` is accepted only in debug
/// builds, for a relay running on this machine.
fn after_scheme(url: &str) -> Option<&str> {
    url.strip_prefix("wss://").or_else(|| {
        cfg!(debug_assertions)
            .then(|| url.strip_prefix("ws://"))
            .flatten()
    })
}

fn host_of(url: &str) -> Option<String> {
    let rest = after_scheme(url)?;
    let authority = rest.split('/').next()?;
    let host = authority
        .rsplit_once(':')
        .map_or(authority, |(host, _)| host);
    (!host.is_empty()).then(|| host.trim_matches(['[', ']']).to_string())
}

fn port_of(url: &str) -> u16 {
    after_scheme(url)
        .and_then(|rest| rest.split('/').next())
        .and_then(|authority| authority.rsplit_once(':'))
        .and_then(|(_, port)| port.parse().ok())
        .unwrap_or(if url.starts_with("ws://") { 80 } else { 443 })
}

fn connect(url: &str, host: &str) -> Result<Socket, String> {
    let addresses = (host, port_of(url))
        .to_socket_addrs()
        .map_err(|error| format!("Could not find the relay: {error}"))?;
    let mut last_error = "The relay has no address".to_string();
    for address in addresses {
        match TcpStream::connect_timeout(&address, CONNECT_TIMEOUT) {
            Ok(stream) => {
                let _ = stream.set_nodelay(true);
                stream
                    .set_read_timeout(Some(CONNECT_TIMEOUT))
                    .map_err(|error| error.to_string())?;
                let (mut socket, _) = tungstenite::client_tls(url, stream)
                    .map_err(|error| format!("Could not reach the relay: {error}"))?;
                set_read_timeout(&mut socket, POLL);
                return Ok(socket);
            }
            Err(error) => last_error = format!("Could not reach the relay: {error}"),
        }
    }
    Err(last_error)
}

fn set_read_timeout(socket: &mut Socket, timeout: Duration) {
    let stream = match socket.get_mut() {
        MaybeTlsStream::Plain(stream) => stream,
        MaybeTlsStream::NativeTls(stream) => stream.get_mut(),
        _ => return,
    };
    let _ = stream.set_read_timeout(Some(timeout));
}

/// Reads frames until a protocol message arrives or `within` passes.
fn read_message(socket: &mut Socket, within: Duration) -> Result<ServerMessage, Stop> {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        match socket.read() {
            Ok(Message::Text(text)) => {
                return serde_json::from_str(&text)
                    .map_err(|_| Stop::Failed("Malformed relay frame".into()))
            }
            Ok(Message::Close(frame)) => {
                return Err(match frame.map(|frame| u16::from(frame.code)) {
                    Some(CLOSE_UPGRADE_REQUIRED) => Stop::UpdateRequired,
                    code => Stop::Failed(format!("The relay closed the connection ({code:?})")),
                })
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(error) => return Err(Stop::Failed(error.to_string())),
        }
    }
    Err(Stop::Failed("The relay did not answer".into()))
}

fn send(socket: &mut Socket, message: &ClientMessage) -> Result<(), Stop> {
    let text = serde_json::to_string(message).map_err(|error| Stop::Failed(error.to_string()))?;
    socket
        .send(Message::Text(text.into()))
        .map_err(|error| Stop::Failed(error.to_string()))
}

fn send_data(socket: &mut Socket, to: &str, data: Value) -> Result<(), Stop> {
    send(
        socket,
        &ClientMessage::Send {
            to: to.to_string(),
            data,
        },
    )
}

/// `turn:`/`stun:` URLs from the relay as webrtcbin URIs: STUN first one
/// only (webrtcbin takes one), TURN with the credentials percent-encoded into
/// the userinfo (`username` contains `:`, the credential is base64).
pub fn webrtcbin_uris(servers: &[IceServer]) -> (Vec<String>, Vec<String>) {
    let mut stun = Vec::new();
    let mut turn = Vec::new();
    for server in servers {
        for url in &server.urls {
            if let Some(rest) = url.strip_prefix("stun:") {
                stun.push(format!("stun://{}", rest.trim_start_matches("//")));
            } else if let Some((scheme, rest)) = url
                .strip_prefix("turns:")
                .map(|rest| ("turns", rest))
                .or_else(|| url.strip_prefix("turn:").map(|rest| ("turn", rest)))
            {
                let (Some(user), Some(credential)) = (&server.username, &server.credential) else {
                    continue;
                };
                turn.push(format!(
                    "{scheme}://{}:{}@{}",
                    percent_encode(user),
                    percent_encode(credential),
                    rest.trim_start_matches("//")
                ));
            }
        }
    }
    (stun, turn)
}

fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Debug builds only: ignore LAN links so the relay path can be tested on one
/// network (`DESKLINK_FORCE_RELAY=1`).
pub fn force_relay() -> bool {
    cfg!(debug_assertions) && std::env::var("DESKLINK_FORCE_RELAY").as_deref() == Ok("1")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_urls_parse_host_and_port() {
        assert_eq!(
            host_of("wss://relay.example/v1").as_deref(),
            Some("relay.example")
        );
        assert_eq!(port_of("wss://relay.example/v1"), 443);
        assert_eq!(
            host_of("wss://relay.example:8443/v1").as_deref(),
            Some("relay.example")
        );
        assert_eq!(port_of("wss://relay.example:8443/v1"), 8443);
        assert_eq!(host_of("http://relay.example/v1"), None);
    }

    /// Two desktops that trust each other, a real relay on loopback, no LAN
    /// link: each must authenticate, run the end-to-end handshake, and make
    /// the relay link its bootstrap link for the other.
    #[test]
    fn two_paired_devices_meet_through_a_local_relay() {
        use crate::device_links::config::Config;
        use desklink_relay::config::Config as RelayConfig;
        use desklink_relay::limits::TurnQuota;
        use desklink_relay::{Server, Shared};

        let relay = Server::bind(Shared::new(
            RelayConfig {
                listen: "127.0.0.1:0".into(),
                public_host: "127.0.0.1".into(),
                ..RelayConfig::default()
            },
            TurnQuota::default(),
        ))
        .unwrap();
        let url = format!("ws://127.0.0.1:{}/v1", relay.local_addr().unwrap().port());
        thread::spawn(move || relay.run());

        let dir = |name: &str| {
            std::env::temp_dir().join(format!("desklink-relay-{name}-{}", uuid::Uuid::new_v4()))
        };
        let (dir_a, dir_b) = (dir("a"), dir("b"));
        let mut a = Config::load_from_dir(dir_a.clone()).unwrap();
        let mut b = Config::load_from_dir(dir_b.clone()).unwrap();
        let pem =
            |config: &Config| String::from_utf8(config.certificate().to_pem().unwrap()).unwrap();
        let (info_a, info_b) = (a.local_device_info(), b.local_device_info());
        let (pem_a, pem_b) = (pem(&a), pem(&b));
        a.trust_device(&info_b, pem_b).unwrap();
        b.trust_device(&info_a, pem_a).unwrap();

        let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let spawn = |config: Config| {
            let sessions = Arc::new(DeviceManager::new());
            let context = Context {
                config: Arc::new(Mutex::new(config)),
                devices: Arc::default(),
                sessions: Arc::clone(&sessions),
                events: Arc::default(),
                shutdown: Arc::clone(&shutdown),
                relay_url: Some(url.clone()),
            };
            let (_tx, rx) = channel();
            thread::spawn(move || {
                let _keep = _tx;
                Client::new(context, rx).run()
            });
            sessions
        };
        let sessions_a = spawn(a);
        let sessions_b = spawn(b);

        let deadline = Instant::now() + Duration::from_secs(20);
        while Instant::now() < deadline
            && !(sessions_a.bootstrap_is_relay(&info_b.id)
                && sessions_b.bootstrap_is_relay(&info_a.id))
        {
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            sessions_a.bootstrap_is_relay(&info_b.id),
            "A never attached"
        );
        assert!(
            sessions_b.bootstrap_is_relay(&info_a.id),
            "B never attached"
        );

        // And the WebRTC handover, negotiated entirely through the relay,
        // reaches feature-ready on both sides.
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline
            && !(sessions_a.has_ready_webrtc(&info_b.id) && sessions_b.has_ready_webrtc(&info_a.id))
        {
            thread::sleep(Duration::from_millis(100));
        }
        shutdown.store(true, std::sync::atomic::Ordering::Release);
        assert!(
            sessions_a.has_ready_webrtc(&info_b.id),
            "A never became feature-ready"
        );
        assert!(
            sessions_b.has_ready_webrtc(&info_a.id),
            "B never became feature-ready"
        );
        let _ = std::fs::remove_dir_all(dir_a);
        let _ = std::fs::remove_dir_all(dir_b);
    }

    #[test]
    fn relay_ice_servers_become_webrtcbin_uris() {
        let servers = vec![
            IceServer {
                urls: vec!["stun:stun.cloudflare.com:3478".into()],
                username: None,
                credential: None,
            },
            IceServer {
                urls: vec![
                    "turn:turn.cloudflare.com:3478?transport=udp".into(),
                    "turns:turn.cloudflare.com:443?transport=tcp".into(),
                ],
                username: Some("1700000000:abc".into()),
                credential: Some("a+b/c=".into()),
            },
        ];
        let (stun, turn) = webrtcbin_uris(&servers);
        assert_eq!(stun, ["stun://stun.cloudflare.com:3478"]);
        assert_eq!(
            turn,
            [
                "turn://1700000000%3Aabc:a%2Bb%2Fc%3D@turn.cloudflare.com:3478?transport=udp",
                "turns://1700000000%3Aabc:a%2Bb%2Fc%3D@turn.cloudflare.com:443?transport=tcp"
            ]
        );
    }
}
