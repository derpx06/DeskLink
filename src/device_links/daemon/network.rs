use openssl::ssl::{SslAcceptor, SslConnector, SslMethod, SslStream, SslVerifyMode};
use socket2::{Domain, Protocol, SockRef, Socket, TcpKeepalive, Type};
use std::io::{Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::device_links::config::Config;
use crate::device_links::core::{LinkTransport, ReadOutcome};
use crate::device_links::packet::NetworkPacket;

const UDP_PORT: u16 = 1716;
pub(super) const MIN_TCP_PORT: u16 = 1716;
pub(super) const MAX_TCP_PORT: u16 = 1764;

pub(super) fn bind_udp_listener() -> Result<UdpSocket, String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|err| err.to_string())?;
    socket
        .set_reuse_address(true)
        .map_err(|err| err.to_string())?;
    socket.set_broadcast(true).map_err(|err| err.to_string())?;
    socket
        .bind(&SocketAddr::from(([0, 0, 0, 0], UDP_PORT)).into())
        .map_err(|err| format!("Could not bind Device Links UDP port {UDP_PORT}: {err}"))?;
    Ok(socket.into())
}

pub(super) fn bind_tcp_listener() -> Result<TcpListener, String> {
    for port in MIN_TCP_PORT..=MAX_TCP_PORT {
        if let Ok(listener) = TcpListener::bind(("0.0.0.0", port)) {
            return Ok(listener);
        }
    }
    Err(format!(
        "Could not bind any Device Links TCP port in {MIN_TCP_PORT}-{MAX_TCP_PORT}"
    ))
}

/// Enables aggressive TCP keepalive on a device link.
///
/// A Wi-Fi drop, a suspended phone, or a killed peer process leaves the socket
/// open with no FIN. Without keepalive the reader simply times out forever, the
/// session is never marked disconnected, and the device keeps reading as
/// connected — the system's default keepalive would take roughly two hours to
/// notice. These settings surface a dead peer in about half a minute.
pub(super) fn enable_keepalive(stream: &TcpStream) {
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(15))
        .with_interval(Duration::from_secs(5));
    #[cfg(not(any(target_os = "windows", target_os = "openbsd")))]
    let keepalive = keepalive.with_retries(3);

    if let Err(error) = SockRef::from(stream).set_tcp_keepalive(&keepalive) {
        eprintln!("[Daemon] Could not enable TCP keepalive on a device link: {error}");
    }
    // Feature packets are small and latency-sensitive; Nagle only adds delay.
    let _ = stream.set_nodelay(true);
}

pub(crate) fn send_packet(
    stream: &Arc<Mutex<SslStream<TcpStream>>>,
    packet: &NetworkPacket,
) -> Result<(), String> {
    let mut stream = stream
        .lock()
        .map_err(|_| "Stream lock poisoned".to_string())?;
    stream
        .write_all(&packet.serialize_line().map_err(|err| err.to_string())?)
        .and_then(|_| stream.flush())
        .map_err(|err| err.to_string())
}

/// Whether a read error just means "nothing arrived yet".
///
/// This has to separate the read timeout from a socket the kernel has given up
/// on, because TCP keepalive reports the latter as a timeout too. On Unix the
/// two are distinct: `SO_RCVTIMEO` expiry surfaces as `EAGAIN`
/// (`WouldBlock`), while `ETIMEDOUT` (`TimedOut`) means the connection is dead.
///
/// Treating `TimedOut` as idle swallowed exactly the signal keepalive exists to
/// produce: the read then failed instantly on every call, so the loop spun
/// instead of blocking, `handle_disconnect` never ran, and the device kept
/// reporting as connected — the stale-"Connected" failure this whole path is
/// meant to prevent.
fn is_idle_read(error: &std::io::Error) -> bool {
    if error.kind() == std::io::ErrorKind::WouldBlock {
        return true;
    }
    // Platforms that report read-timeout expiry as `TimedOut` cannot make the
    // distinction, so they keep the previous, more forgiving behaviour.
    cfg!(not(unix)) && error.kind() == std::io::ErrorKind::TimedOut
}

/// The LAN carrier: newline-framed packets over pinned TLS on TCP.
pub(crate) struct TlsLinkTransport {
    stream: Arc<Mutex<SslStream<TcpStream>>>,
}

impl TlsLinkTransport {
    pub(crate) fn new(stream: Arc<Mutex<SslStream<TcpStream>>>) -> Self {
        Self { stream }
    }
}

impl LinkTransport for TlsLinkTransport {
    fn send_packet(&self, packet: &NetworkPacket) -> Result<(), String> {
        send_packet(&self.stream, packet)
    }

    fn read_byte(&self) -> Result<ReadOutcome, String> {
        let mut byte = [0u8; 1];
        let mut stream = self
            .stream
            .lock()
            .map_err(|_| "Session stream lock failed".to_string())?;
        match stream.read(&mut byte) {
            Ok(0) => Ok(ReadOutcome::Closed),
            Ok(_) => Ok(ReadOutcome::Byte(byte[0])),
            Err(error) if is_idle_read(&error) => Ok(ReadOutcome::Idle),
            Err(error) => Err(format!("Read error: {error}")),
        }
    }

    /// The real `Shutdown::Both`, not a flag.
    ///
    /// `DeviceManager::register_link` closes the link it replaces to unblock
    /// the superseded generation's reader immediately, from another thread. A
    /// flag would leave that reader parked in `read` for a whole idle window
    /// while the replacement is already live.
    fn shutdown(&self) {
        if let Ok(stream) = self.stream.lock() {
            let _ = stream.get_ref().shutdown(Shutdown::Both);
        }
    }

    fn local_ip(&self) -> Option<IpAddr> {
        let stream = self.stream.lock().ok()?;
        stream
            .get_ref()
            .local_addr()
            .ok()
            .map(|address| address.ip())
    }
}

pub(super) fn read_ssl_packet(stream: &mut SslStream<TcpStream>) -> Result<NetworkPacket, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = stream.read(&mut byte).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("Connection closed".to_string());
        }
        line.push(byte[0]);
        if byte[0] == b'\n' {
            if line.len() == 1 {
                line.clear();
                continue;
            }
            break;
        }
        if line.len() > 32 * 1024 * 1024 {
            return Err("Packet is too large".to_string());
        }
    }
    NetworkPacket::deserialize(&line).map_err(|err| err.to_string())
}

pub(super) fn ssl_acceptor(config: &Arc<Mutex<Config>>) -> Result<SslAcceptor, String> {
    let config = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?;
    let mut builder =
        SslAcceptor::mozilla_intermediate(SslMethod::tls()).map_err(|err| err.to_string())?;
    builder
        .set_certificate(config.certificate())
        .map_err(|err| err.to_string())?;
    builder
        .set_private_key(config.key())
        .map_err(|err| err.to_string())?;
    builder.set_verify_callback(SslVerifyMode::PEER, |_preverify_ok, _ctx| true);
    Ok(builder.build())
}

pub(super) fn ssl_connector(config: &Arc<Mutex<Config>>) -> Result<SslConnector, String> {
    let config = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?;
    let mut builder = SslConnector::builder(SslMethod::tls()).expect("TLS method is available");
    builder
        .set_certificate(config.certificate())
        .map_err(|err| err.to_string())?;
    builder
        .set_private_key(config.key())
        .map_err(|err| err.to_string())?;
    builder.set_verify_callback(SslVerifyMode::PEER, |_preverify_ok, _ctx| true);
    Ok(builder.build())
}
