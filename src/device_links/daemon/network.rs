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

/// Kernel TCP keepalive on a device link: idle 30 s, then a probe every
/// 10 s, gone after 3 unanswered (as KDE Connect's `lanlinkprovider.cpp`).
///
/// A Wi-Fi drop, a suspended phone, or a killed peer process leaves the socket
/// open with no FIN. Without keepalive the reader simply times out forever, the
/// session is never marked disconnected, and the device keeps reading as
/// connected — the system's default keepalive would take roughly two hours to
/// notice. The kernel answers these probes even while the phone's app is
/// frozen (screen off), so a dozing phone is kept and a vanished one is found
/// within about a minute; the 30 s app heartbeat usually finds it first.
pub(super) const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
pub(super) const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
pub(super) const KEEPALIVE_COUNT: u32 = 3;

pub(super) fn enable_keepalive(stream: &TcpStream) {
    let keepalive = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    #[cfg(not(any(target_os = "windows", target_os = "openbsd")))]
    let keepalive = keepalive.with_retries(KEEPALIVE_COUNT);

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
    /// The socket's descriptor, to wait for data *without* the stream mutex.
    /// Valid as long as `stream` is, and the transport owns an `Arc` of it.
    fd: std::os::fd::RawFd,
}

/// How long the reader waits for data before it looks at the link again
/// (shutdown and adoption wake it at once: the socket is shut down, which
/// makes it readable). Not a poll rate: an idle link wakes once a second.
pub(crate) const IDLE_POLL: Duration = Duration::from_secs(1);

impl TlsLinkTransport {
    pub(crate) fn new(stream: Arc<Mutex<SslStream<TcpStream>>>) -> Self {
        use std::os::fd::AsRawFd;
        let fd = stream
            .lock()
            .map(|stream| stream.get_ref().as_raw_fd())
            .unwrap_or(-1);
        Self { stream, fd }
    }

    /// Reads one byte, waiting up to `wait` for it (outside the stream mutex).
    fn read_byte_waiting(&self, wait: Duration) -> Result<ReadOutcome, String> {
        let mut byte = [0u8; 1];
        // Decrypted bytes TLS already holds are not on the socket: read them
        // without waiting. Otherwise wait for the socket *outside* the stream
        // mutex, so an idle link costs one wake-up a second and a send never
        // waits behind the reader.
        let buffered = self
            .stream
            .lock()
            .map_err(|_| "Session stream lock failed".to_string())?
            .ssl()
            .pending()
            > 0;
        if !buffered && !self.readable_within(wait) {
            return Ok(ReadOutcome::Idle);
        }
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

    /// Whether the socket is readable (or has hung up) within `wait`.
    fn readable_within(&self, wait: Duration) -> bool {
        wait_readable(self.fd, wait)
    }
}

/// `poll(2)` on one descriptor for input. Errors and hang-ups count as
/// readable, so the read that follows reports them.
pub(crate) fn wait_readable(fd: std::os::fd::RawFd, wait: Duration) -> bool {
    if fd < 0 {
        return true;
    }
    let mut poll = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = wait.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: one valid pollfd, count 1.
    let ready = unsafe { libc::poll(&mut poll, 1, millis) };
    ready != 0
}

impl LinkTransport for TlsLinkTransport {
    fn send_packet(&self, packet: &NetworkPacket) -> Result<(), String> {
        send_packet(&self.stream, packet)
    }

    fn read_byte(&self) -> Result<ReadOutcome, String> {
        self.read_byte_waiting(IDLE_POLL)
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

/// Bound on unauthenticated connections being handshaken at once, in either
/// direction. Each holds a thread and a socket; without a cap any LAN host
/// could open thousands and exhaust both before proving anything.
const MAX_PREAUTH_HANDSHAKES: usize = 16;
static PREAUTH_HANDSHAKES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// At most this many of those slots per remote address. TLS gives no absolute
/// deadline (its reads time out individually), so without a per-host bound
/// one host dribbling bytes could hold every slot and lock out the phone.
const MAX_PREAUTH_HANDSHAKES_PER_HOST: usize = 3;
static PREAUTH_BY_HOST: std::sync::Mutex<Vec<(IpAddr, usize)>> = std::sync::Mutex::new(Vec::new());

/// A claimed handshake slot, released on drop however the handshake ends.
pub(super) struct HandshakeSlot(IpAddr);

impl HandshakeSlot {
    pub(super) fn claim(host: IpAddr) -> Option<Self> {
        use std::sync::atomic::Ordering;
        let mut by_host = PREAUTH_BY_HOST.lock().ok()?;
        let held = by_host
            .iter()
            .find(|(ip, _)| *ip == host)
            .map_or(0, |(_, count)| *count);
        if held >= MAX_PREAUTH_HANDSHAKES_PER_HOST {
            return None;
        }
        PREAUTH_HANDSHAKES
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < MAX_PREAUTH_HANDSHAKES).then_some(count + 1)
            })
            .ok()?;
        match by_host.iter_mut().find(|(ip, _)| *ip == host) {
            Some((_, count)) => *count += 1,
            None => by_host.push((host, 1)),
        }
        Some(Self(host))
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        PREAUTH_HANDSHAKES.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        if let Ok(mut by_host) = PREAUTH_BY_HOST.lock() {
            if let Some(index) = by_host.iter().position(|(ip, _)| *ip == self.0) {
                by_host[index].1 -= 1;
                if by_host[index].1 == 0 {
                    by_host.swap_remove(index);
                }
            }
        }
    }
}

/// This computer's addresses, each with its interface, from `getifaddrs`
/// (interfaces that are up only).
pub(crate) fn local_interfaces() -> Vec<(String, IpAddr)> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: `getifaddrs` fills `list` with a linked list we only read and
    // then free once; every pointer is checked before it is dereferenced.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut cursor = list;
        while !cursor.is_null() {
            let entry = &*cursor;
            cursor = entry.ifa_next;
            if entry.ifa_addr.is_null() || entry.ifa_flags & libc::IFF_UP as u32 == 0 {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(entry.ifa_name)
                .to_string_lossy()
                .into_owned();
            let ip = match i32::from((*entry.ifa_addr).sa_family) {
                libc::AF_INET => {
                    let address = &*(entry.ifa_addr as *const libc::sockaddr_in);
                    IpAddr::from(u32::from_be(address.sin_addr.s_addr).to_be_bytes())
                }
                libc::AF_INET6 => {
                    let address = &*(entry.ifa_addr as *const libc::sockaddr_in6);
                    IpAddr::from(address.sin6_addr.s6_addr)
                }
                _ => continue,
            };
            found.push((name, ip));
        }
        libc::freeifaddrs(list);
    }
    found
}

/// Interfaces that never reach a phone: loopback, containers, VMs, VPNs.
pub(crate) fn is_virtual_interface(name: &str) -> bool {
    const VIRTUAL: [&str; 11] = [
        "lo", "docker", "virbr", "veth", "tun", "tap", "wg", "br-", "vmnet", "vboxnet", "podman",
    ];
    VIRTUAL.iter().any(|prefix| name.starts_with(prefix))
}

/// The IPv4 addresses a phone on the same network can dial: not loopback,
/// not link-local, not on a virtual interface. For the QR code.
pub(crate) fn lan_ipv4s(interfaces: &[(String, IpAddr)]) -> Vec<std::net::Ipv4Addr> {
    let mut found: Vec<std::net::Ipv4Addr> = interfaces
        .iter()
        .filter(|(name, _)| !is_virtual_interface(name))
        .filter_map(|(_, ip)| match ip {
            IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_link_local() && !ip.is_unspecified() => {
                Some(*ip)
            }
            _ => None,
        })
        .collect();
    found.dedup();
    found
}

/// The device ids with a connection being set up (dialled, or accepted and
/// still in its handshake).
static IN_FLIGHT: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// One connection being set up for a device id; a second (a duplicate dial
/// from discovery, the redialer and an inbound socket racing at startup)
/// is not started. Each one that completed used to register again, bump
/// the generation and kill the WebRTC peer the first had just begun.
pub(super) struct DeviceFlight(String);

impl DeviceFlight {
    pub(super) fn claim(device_id: &str) -> Option<Self> {
        let mut in_flight = IN_FLIGHT.lock().ok()?;
        if in_flight.iter().any(|id| id == device_id) {
            return None;
        }
        in_flight.push(device_id.to_string());
        Some(Self(device_id.to_string()))
    }
}

impl Drop for DeviceFlight {
    fn drop(&mut self) {
        if let Ok(mut in_flight) = IN_FLIGHT.lock() {
            in_flight.retain(|id| *id != self.0);
        }
    }
}

/// The TLS handshake and the secure identity, together.
pub(super) const HANDSHAKE_LIMIT: Duration = Duration::from_secs(5);

/// Shuts a socket down unless dropped within `limit`: one deadline for the
/// TLS handshake and secure identity, which OpenSSL reads with only the
/// socket's per-read timeout.
pub(super) struct HandshakeDeadline {
    _done: std::sync::mpsc::Sender<()>,
}

impl HandshakeDeadline {
    pub(super) fn arm(stream: &TcpStream, limit: Duration) -> Self {
        let (done, wait) = std::sync::mpsc::channel::<()>();
        if let Ok(socket) = stream.try_clone() {
            let _ = std::thread::Builder::new()
                .name("desklink-handshake-deadline".into())
                .spawn(move || {
                    // Dropping the guard ends the wait with `Disconnected`.
                    if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) =
                        wait.recv_timeout(limit)
                    {
                        let _ = socket.shutdown(std::net::Shutdown::Both);
                    }
                });
        }
        Self { _done: done }
    }
}

/// Packets from a peer that is not paired yet are identity, pairing and
/// signaling only; the largest legitimate one (signed SDP) is well under this.
pub(super) const MAX_UNPAIRED_PACKET_BYTES: usize = 256 * 1024;

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
        // Only the secure identity is read here, before anything is trusted.
        if line.len() > MAX_UNPAIRED_PACKET_BYTES {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_qr_lists_only_addresses_a_phone_can_dial() {
        let interfaces: Vec<(String, IpAddr)> = [
            ("lo", "127.0.0.1"),
            ("wlp2s0", "192.168.1.20"),
            ("wlp2s0", "fe80::1"),
            ("enp3s0", "10.0.0.5"),
            ("docker0", "172.17.0.1"),
            ("virbr0", "192.168.122.1"),
            ("veth12ab", "169.254.3.3"),
            ("tun0", "10.8.0.2"),
            ("wg0", "10.9.0.1"),
            ("eth9", "169.254.9.9"),
        ]
        .iter()
        .map(|(name, ip)| (name.to_string(), ip.parse().unwrap()))
        .collect();
        let found: Vec<String> = lan_ipv4s(&interfaces)
            .iter()
            .map(|ip| ip.to_string())
            .collect();
        assert_eq!(found, ["192.168.1.20", "10.0.0.5"]);
        assert!(
            !local_interfaces().is_empty(),
            "this machine has at least loopback"
        );
    }

    #[test]
    fn a_handshake_past_its_deadline_is_cut_off_and_one_on_time_is_not() {
        use std::io::Read;
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let slow = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        slow.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let _deadline = HandshakeDeadline::arm(&slow, Duration::from_millis(50));
        let started = std::time::Instant::now();
        let mut byte = [0u8; 1];
        // The peer never sends: only the deadline ends this read.
        assert_eq!((&slow).read(&mut byte).unwrap_or(0), 0);
        assert!(started.elapsed() < Duration::from_secs(5));

        let on_time = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        drop(HandshakeDeadline::arm(&on_time, Duration::from_millis(50)));
        std::thread::sleep(Duration::from_millis(150));
        std::io::Write::write_all(&mut peer, b"x").unwrap();
        assert_eq!((&on_time).read(&mut byte).unwrap(), 1, "still open");
    }

    #[test]
    fn one_connection_at_a_time_per_device() {
        let first = DeviceFlight::claim("flight-phone").expect("free");
        assert!(
            DeviceFlight::claim("flight-phone").is_none(),
            "a duplicate waits its turn"
        );
        let other = DeviceFlight::claim("flight-tablet");
        assert!(other.is_some(), "other devices are not held up");
        drop(first);
        assert!(
            DeviceFlight::claim("flight-phone").is_some(),
            "free again once done"
        );
    }

    #[test]
    fn device_links_get_kernel_keepalive() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let stream = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        enable_keepalive(&stream);
        let socket = SockRef::from(&stream);
        assert!(socket.keepalive().unwrap());
        assert_eq!(socket.tcp_keepalive_time().unwrap(), KEEPALIVE_IDLE);
        assert_eq!(socket.tcp_keepalive_interval().unwrap(), KEEPALIVE_INTERVAL);
        assert_eq!(socket.tcp_keepalive_retries().unwrap(), KEEPALIVE_COUNT);
        assert!(stream.nodelay().unwrap());
    }

    #[test]
    fn preauth_handshakes_are_capped_and_slots_are_released() {
        let host: IpAddr = "192.0.2.7".parse().unwrap();
        let slots: Vec<_> = std::iter::from_fn(|| HandshakeSlot::claim(host))
            .take(64)
            .collect();
        // One host can never take more than its share.
        assert_eq!(slots.len(), MAX_PREAUTH_HANDSHAKES_PER_HOST);
        assert!(HandshakeSlot::claim(host).is_none());
        // Another host is not locked out by it.
        let other = HandshakeSlot::claim("192.0.2.8".parse().unwrap());
        assert!(other.is_some());
        drop(slots);
        assert!(HandshakeSlot::claim(host).is_some());
    }

    /// A connected TLS pair over loopback: the server side's transport, and
    /// the client's raw stream.
    fn tls_pair() -> (TlsLinkTransport, SslStream<TcpStream>) {
        use openssl::asn1::Asn1Time;
        use openssl::ec::{EcGroup, EcKey};
        use openssl::hash::MessageDigest;
        use openssl::nid::Nid;
        use openssl::pkey::PKey;
        use openssl::x509::{X509Builder, X509NameBuilder};
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, "test").unwrap();
        let name = name.build();
        let mut cert = X509Builder::new().unwrap();
        cert.set_version(2).unwrap();
        cert.set_subject_name(&name).unwrap();
        cert.set_issuer_name(&name).unwrap();
        cert.set_pubkey(&key).unwrap();
        cert.set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        cert.set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        cert.sign(&key, MessageDigest::sha256()).unwrap();
        let cert = cert.build();
        let mut acceptor = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
        acceptor.set_certificate(&cert).unwrap();
        acceptor.set_private_key(&key).unwrap();
        let acceptor = acceptor.build();
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (tcp, _) = listener.accept().unwrap();
            acceptor.accept(tcp).unwrap()
        });
        let mut connector = SslConnector::builder(SslMethod::tls()).unwrap();
        connector.set_verify(SslVerifyMode::NONE);
        let tcp = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let client = connector
            .build()
            .configure()
            .unwrap()
            .verify_hostname(false)
            .connect("test", tcp)
            .unwrap();
        let server = server.join().unwrap();
        server
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        (TlsLinkTransport::new(Arc::new(Mutex::new(server))), client)
    }

    /// An idle link is read by waiting, not polling: the wait does not hold
    /// the stream (a send goes through at once), one pass lasts the whole
    /// wait, data and a shutdown end it at once, and bytes TLS already holds
    /// are read without waiting.
    #[test]
    fn an_idle_link_waits_outside_the_stream_and_wakes_for_data() {
        let (transport, mut client) = tls_pair();
        let transport = Arc::new(transport);
        let waiting = {
            let transport = Arc::clone(&transport);
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let outcome = transport.read_byte_waiting(Duration::from_millis(400));
                (outcome, started.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        // A send while the reader waits is not held up behind it.
        let sent = std::time::Instant::now();
        transport
            .send_packet(&NetworkPacket::new("desklink.heartbeat"))
            .unwrap();
        assert!(
            sent.elapsed() < Duration::from_millis(50),
            "the send waited {:?} for the reader",
            sent.elapsed()
        );
        let (outcome, took) = waiting.join().unwrap();
        assert!(matches!(outcome, Ok(ReadOutcome::Idle)));
        assert!(
            took >= Duration::from_millis(350),
            "one idle pass took {took:?}: it polled"
        );
        // Data ends the wait at once; the rest of the line is already in TLS.
        client.write_all(b"ab").unwrap();
        let started = std::time::Instant::now();
        assert!(matches!(
            transport.read_byte_waiting(Duration::from_secs(5)),
            Ok(ReadOutcome::Byte(b'a'))
        ));
        // Data ends the wait; polling would have waited out the 5 s.
        assert!(started.elapsed() < Duration::from_secs(3));
        assert!(matches!(
            transport.read_byte_waiting(Duration::from_secs(5)),
            Ok(ReadOutcome::Byte(b'b'))
        ));
        // A shutdown wakes a long wait.
        let waiting = {
            let transport = Arc::clone(&transport);
            std::thread::spawn(move || {
                let started = std::time::Instant::now();
                let outcome = transport.read_byte_waiting(Duration::from_secs(30));
                (outcome, started.elapsed())
            })
        };
        std::thread::sleep(Duration::from_millis(100));
        transport.shutdown();
        let (outcome, took) = waiting.join().unwrap();
        assert!(took < Duration::from_secs(2), "shutdown took {took:?}");
        assert!(!matches!(outcome, Ok(ReadOutcome::Byte(_))));
    }

    /// Measured: the reader of an idle link used to wake 40-67 times a second
    /// (a 20 ms timeout and a 5 ms nap); it now returns once a second.
    #[test]
    fn an_idle_link_is_read_about_once_a_second() {
        let (transport, _client) = tls_pair();
        let started = std::time::Instant::now();
        let mut turns = 0;
        while started.elapsed() < Duration::from_millis(3_200) {
            assert!(matches!(transport.read_byte(), Ok(ReadOutcome::Idle)));
            turns += 1;
        }
        eprintln!("[test] idle link: {turns} reader turns in 3.2 s (was ~170)");
        assert!(turns <= 4, "{turns} reader turns in 3.2 s");
    }
}
