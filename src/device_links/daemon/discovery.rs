use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::handle::DaemonEvent;
use super::network::{bind_udp_listener, MAX_TCP_PORT, MIN_TCP_PORT};
use super::state::{mark_error, push_error, upsert_device};
use super::validation::{connect_to_device, should_throttle_connection};
use super::DaemonWorker;
use crate::device_links::config::Config;
use crate::device_links::core::DeviceManager;
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_IDENTITY};

const UDP_PORT: u16 = 1716;

/// A connection worker may lose the normal inbound/outbound race after it has
/// already entered a blocking TLS handshake. Its eventual timeout is stale:
/// publishing it would overwrite the healthy authoritative session in the UI.
fn should_publish_outgoing_failure(sessions: &DeviceManager, device_id: &str) -> bool {
    sessions.wants_bootstrap_connection(device_id)
}

/// Set when the network changes: the broadcaster starts its burst again.
static BEACON_AGAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Announces this computer again at once, as at startup (broadcast, then
/// unicast to each paired phone's last address).
pub(super) fn beacon_again() {
    // Every address of every phone gets a fresh chance too.
    if let Ok(mut book) = addresses().lock() {
        book.wake(None);
    }
    BEACON_AGAIN.store(true, Ordering::Release);
}

/// A phone was unpaired: its addresses go.
pub(super) fn forget_addresses(device_id: &str) {
    if let Ok(mut book) = addresses().lock() {
        book.forget(device_id);
    }
}

/// What a paired phone last said about itself and the TCP port it listens
/// on, from its identity (a beacon, or an incoming connection's first line).
/// The address comes from its `DeviceView`.
fn known_endpoints() -> &'static Mutex<HashMap<String, (DeviceInfo, u16)>> {
    static KNOWN: std::sync::OnceLock<Mutex<HashMap<String, (DeviceInfo, u16)>>> =
        std::sync::OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// Records a paired phone's listening port from an identity packet (a
/// beacon, or an incoming connection's first line). Identities are
/// unauthenticated, so strangers' are not kept: the map would grow with
/// every spoofed id. The address is remembered across restarts only once a
/// paired link authenticates (`persist_paired_endpoint`).
pub(super) fn remember_endpoint(info: &DeviceInfo, packet: &NetworkPacket, trusted: bool) {
    let Some(port) = packet
        .get_i64("tcpPort")
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| (MIN_TCP_PORT..=MAX_TCP_PORT).contains(port))
    else {
        return;
    };
    if !trusted {
        return;
    }
    if let Ok(mut known) = known_endpoints().lock() {
        known.insert(info.id.clone(), (info.clone(), port));
    }
}

/// Most phones `endpoints.json` remembers.
const MAX_PERSISTED_ENDPOINTS: usize = 32;

/// After a paired link authenticated from `ip`: remembers where the phone
/// listens (that address and the port it announced), for the first dial
/// after a restart. Only paired phones are kept (`trusted`), at most
/// [`MAX_PERSISTED_ENDPOINTS`], written off the caller's thread.
pub(super) fn persist_paired_endpoint(
    device_id: &str,
    ip: std::net::IpAddr,
    trusted: std::collections::HashSet<String>,
) {
    if ip.is_loopback() || ip.is_unspecified() {
        return;
    }
    let Some(port) = known_endpoints()
        .lock()
        .ok()
        .and_then(|known| known.get(device_id).map(|(_, port)| *port))
    else {
        return;
    };
    note_address(device_id, ip);
    let device_id = device_id.to_string();
    let _ = thread::Builder::new()
        .name("desklink-endpoints".into())
        .spawn(move || {
            // One writer at a time: two links authenticating together must
            // not interleave read-modify-write.
            static WRITING: Mutex<()> = Mutex::new(());
            let _writing = WRITING.lock();
            persist_endpoint(&device_id, SocketAddr::new(ip, port), &trusted);
        });
}

/// Where each paired phone was last reachable, kept across restarts so the
/// first dial after a restart needs no discovery (`endpoints.json` beside
/// the connection history).
fn endpoints_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!(
            "desklink-test-endpoints-{}.json",
            std::process::id()
        )));
    }
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("endpoints.json"),
    )
}

/// The remembered addresses, by device id.
pub(super) fn persisted_endpoints() -> HashMap<String, SocketAddr> {
    endpoints_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn persist_endpoint(
    device_id: &str,
    address: SocketAddr,
    trusted: &std::collections::HashSet<String>,
) {
    let Some(path) = endpoints_path() else {
        return;
    };
    let before = persisted_endpoints();
    let all = updated_endpoints(before.clone(), device_id, address, trusted);
    if all == before {
        return;
    }
    let written = serde_json::to_string(&all)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            path.parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::write(&path, text))
                .map_err(|error| error.to_string())
        });
    if let Err(error) = written {
        eprintln!("[Daemon] Could not remember {device_id}'s address: {error}");
    }
}

/// `all` with `device_id` at `address`, pruned to paired phones and capped.
fn updated_endpoints(
    mut all: HashMap<String, SocketAddr>,
    device_id: &str,
    address: SocketAddr,
    trusted: &std::collections::HashSet<String>,
) -> HashMap<String, SocketAddr> {
    all.retain(|id, _| trusted.contains(id));
    if trusted.contains(device_id) {
        all.insert(device_id.to_string(), address);
    }
    while all.len() > MAX_PERSISTED_ENDPOINTS {
        let Some(extra) = all.keys().find(|id| *id != device_id).cloned() else {
            break;
        };
        all.remove(&extra);
    }
    all
}

/// How often paired phones without a bootstrap link are considered for a
/// redial (each address has its own backoff).
const REDIAL_EVERY: Duration = Duration::from_secs(1);
/// Addresses kept per phone (the newest IPv4s; one IPv6 on top).
const MAX_ADDRESSES: usize = 3;
/// A dead address is retried after this, doubling per failure up to
/// [`MAX_ADDRESS_BACKOFF`]: not every few seconds for ever.
const FIRST_ADDRESS_BACKOFF: Duration = Duration::from_secs(3);
const MAX_ADDRESS_BACKOFF: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
struct Address {
    ip: std::net::IpAddr,
    failures: u32,
    retry_at: Option<Instant>,
}

/// Where each paired phone was last seen: its last [`MAX_ADDRESSES`] IPv4s
/// (from beacons, authenticated links and mDNS) and its newest IPv6. Phones
/// move between networks and DHCP leases, so dialling only the newest one
/// left a phone on its old address unreachable for minutes.
#[derive(Debug, Default)]
pub(super) struct AddressBook {
    phones: HashMap<String, Vec<Address>>,
}

impl AddressBook {
    /// `ip` is where `device_id` was just seen: newest first, a repeat moves
    /// up and is tried again at once.
    pub(super) fn seen(&mut self, device_id: &str, ip: std::net::IpAddr) {
        if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
            return;
        }
        let list = self.phones.entry(device_id.to_string()).or_default();
        list.retain(|known| known.ip != ip);
        list.insert(
            0,
            Address {
                ip,
                failures: 0,
                retry_at: None,
            },
        );
        let (mut v4, mut v6) = (0, 0);
        list.retain(|known| {
            let counter = if known.ip.is_ipv4() { &mut v4 } else { &mut v6 };
            *counter += 1;
            *counter <= if known.ip.is_ipv4() { MAX_ADDRESSES } else { 1 }
        });
    }

    /// The addresses to dial now: those whose backoff is over, newest first.
    pub(super) fn due(&self, device_id: &str, now: Instant) -> Vec<std::net::IpAddr> {
        self.phones
            .get(device_id)
            .map(|list| {
                list.iter()
                    .filter(|known| known.retry_at.is_none_or(|at| at <= now))
                    .map(|known| known.ip)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// A dial to `ip` failed: it waits longer next time.
    pub(super) fn failed(&mut self, device_id: &str, ip: std::net::IpAddr, now: Instant) {
        if let Some(known) = self
            .phones
            .get_mut(device_id)
            .and_then(|list| list.iter_mut().find(|known| known.ip == ip))
        {
            known.failures = known.failures.saturating_add(1);
            known.retry_at = Some(now + address_backoff(known.failures));
        }
    }

    /// A dial to `ip` worked: the others are only fallbacks now, and `ip`
    /// stays first.
    pub(super) fn worked(&mut self, device_id: &str, ip: std::net::IpAddr) {
        self.seen(device_id, ip);
    }

    /// Any sign of the phone (a network change, a beacon): every address is
    /// tried again at once.
    pub(super) fn wake(&mut self, device_id: Option<&str>) {
        for (id, list) in self.phones.iter_mut() {
            if device_id.is_none_or(|only| only == id) {
                for known in list {
                    known.failures = 0;
                    known.retry_at = None;
                }
            }
        }
    }

    /// Forgets `device_id` (unpaired).
    pub(super) fn forget(&mut self, device_id: &str) {
        self.phones.remove(device_id);
    }

    /// Whether `ip` is a known address of the phone that is waiting out its
    /// backoff.
    fn is_backing_off(&self, device_id: &str, ip: std::net::IpAddr, now: Instant) -> bool {
        self.phones.get(device_id).is_some_and(|list| {
            list.iter()
                .any(|known| known.ip == ip && known.retry_at.is_some_and(|at| at > now))
        })
    }

    /// Every address of every phone, for unicast beacons.
    fn all(&self) -> Vec<(String, std::net::IpAddr)> {
        self.phones
            .iter()
            .flat_map(|(id, list)| list.iter().map(|known| (id.clone(), known.ip)))
            .collect()
    }
}

/// How long a dead address waits after its `failures`-th failure.
pub(super) fn address_backoff(failures: u32) -> Duration {
    FIRST_ADDRESS_BACKOFF
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1).min(10)))
        .min(MAX_ADDRESS_BACKOFF)
}

pub(super) fn addresses() -> &'static Mutex<AddressBook> {
    static BOOK: std::sync::OnceLock<Mutex<AddressBook>> = std::sync::OnceLock::new();
    BOOK.get_or_init(Default::default)
}

/// Records where `device_id` was seen (a paired phone's beacon, link or mDNS
/// answer).
pub(super) fn note_address(device_id: &str, ip: std::net::IpAddr) {
    if let Ok(mut book) = addresses().lock() {
        book.seen(device_id, ip);
    }
}

/// The paired phones to dial now: no live bootstrap link (whether or not
/// their WebRTC peer survives), a last-known address and listening port.
///
/// Heard from this run (`known`): its address and port. Otherwise, after a
/// restart before any beacon, the address remembered from last time
/// (`persisted`), with what the trust store says about the phone.
fn redial_targets(
    devices: &HashMap<String, DeviceView>,
    known: &HashMap<String, (DeviceInfo, u16)>,
    persisted: &HashMap<String, SocketAddr>,
    book: &AddressBook,
    now: Instant,
    wants: impl Fn(&str) -> bool,
) -> Vec<(DeviceInfo, std::net::IpAddr, u16)> {
    let mut targets = Vec::new();
    for device in devices
        .values()
        .filter(|device| device.trusted && wants(&device.id))
    {
        // Where it listens, and who it is: what it announced this run, else
        // what the trust store and the remembered address say.
        let (info, port) = match (known.get(&device.id), persisted.get(&device.id)) {
            (Some((info, port)), _) => (info.clone(), *port),
            (None, Some(address)) => (
                DeviceInfo {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    device_type: device.device_type.clone(),
                    protocol_version: device.protocol_version,
                    incoming_capabilities: Vec::new(),
                    outgoing_capabilities: Vec::new(),
                    instance_id: None,
                },
                address.port(),
            ),
            (None, None) => continue,
        };
        // Every address it was seen on, newest first; the address on its
        // `DeviceView` and the remembered one are the seeds.
        let mut ips = book.due(&device.id, now);
        let seeds = [
            device.address.parse().ok(),
            persisted.get(&device.id).map(SocketAddr::ip),
        ];
        for seed in seeds.into_iter().flatten() {
            if !ips.contains(&seed) && !book.is_backing_off(&device.id, seed, now) {
                ips.push(seed);
            }
        }
        for ip in ips {
            targets.push((info.clone(), ip, port));
        }
    }
    targets
}

impl DaemonWorker {
    /// Keeps a TCP link to every paired phone it can reach. The phone often
    /// cannot dial in: on its own hotspot it hears only its own beacons, and
    /// a phone whose screen is off is frozen by its OS until *inbound* TCP
    /// arrives — this link (and its 10 s heartbeat) is how it is thawed. So
    /// the desk dials the last-known address itself, including while a
    /// WebRTC peer survives without its bootstrap socket (the new socket is
    /// adopted into the session; see `attach_bootstrap_link`). Failures are
    /// expected and not shown.
    pub(super) fn start_redialer(&self) {
        let devices = Arc::clone(&self.devices);
        let sessions = Arc::clone(&self.sessions);
        let config = Arc::clone(&self.config);
        let events = Arc::clone(&self.events);
        let shutdown = Arc::clone(&self.shutdown);
        thread::spawn(move || {
            let persisted = persisted_endpoints();
            while !shutdown.load(Ordering::Acquire) {
                thread::sleep(REDIAL_EVERY);
                let targets = {
                    let (Ok(devices), Ok(known)) = (devices.lock(), known_endpoints().lock())
                    else {
                        continue;
                    };
                    let Ok(book) = addresses().lock() else {
                        continue;
                    };
                    redial_targets(&devices, &known, &persisted, &book, Instant::now(), |id| {
                        sessions.wants_bootstrap_connection(id)
                    })
                };
                for (info, ip, port) in targets {
                    let Some(slot) = super::network::HandshakeSlot::claim(ip) else {
                        continue;
                    };
                    let (config, devices, sessions, events) = (
                        Arc::clone(&config),
                        Arc::clone(&devices),
                        Arc::clone(&sessions),
                        Arc::clone(&events),
                    );
                    let _ = thread::Builder::new().spawn(move || {
                        let _slot = slot;
                        let id = info.id.clone();
                        match connect_to_device(
                            info,
                            SocketAddr::new(ip, port),
                            port,
                            config,
                            devices,
                            sessions,
                            events,
                        ) {
                            Ok(()) => {
                                if let Ok(mut book) = addresses().lock() {
                                    book.worked(&id, ip);
                                }
                            }
                            Err(error) => {
                                // Backoff for this address only: the phone's
                                // others are still tried.
                                if let Ok(mut book) = addresses().lock() {
                                    book.failed(&id, ip, Instant::now());
                                }
                                debug_log!(
                                    "[DL-WRTC-BOOT] redial of {id} at {ip}:{port} failed: {error}"
                                );
                            }
                        }
                    });
                }
            }
        });
    }

    pub(super) fn start_udp_listener(&self) -> Result<(), String> {
        let socket = bind_udp_listener()?;
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|err| err.to_string())?;
        socket.set_broadcast(true).map_err(|err| err.to_string())?;
        let config = Arc::clone(&self.config);
        let devices = Arc::clone(&self.devices);
        let sessions = Arc::clone(&self.sessions);
        let errors = Arc::clone(&self.errors);
        let events = Arc::clone(&self.events);
        let shutdown = Arc::clone(&self.shutdown);
        let tcp_port = self.tcp_port;
        let last_connections = Arc::new(Mutex::new(HashMap::new()));

        thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            while !shutdown.load(Ordering::Acquire) {
                match socket.recv_from(&mut buffer) {
                    Ok((len, address)) => {
                        if let Ok(packet) = NetworkPacket::deserialize(&buffer[..len]) {
                            if packet.packet_type == PACKET_TYPE_IDENTITY {
                                handle_identity_packet(
                                    packet,
                                    address,
                                    tcp_port,
                                    &config,
                                    &devices,
                                    &sessions,
                                    &errors,
                                    &events,
                                    &last_connections,
                                );
                            }
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut
                            // A signal (suspend/resume) interrupted the wait: just read again.
                            || err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => push_error(&errors, format!("UDP discovery failed: {err}")),
                }
            }
        });

        Ok(())
    }

    pub(super) fn start_broadcaster(&self) {
        let config = Arc::clone(&self.config);
        let errors = Arc::clone(&self.errors);
        let shutdown = Arc::clone(&self.shutdown);
        let devices = Arc::clone(&self.devices);
        let tcp_port = self.tcp_port;
        thread::spawn(move || {
            // Burst at 0, 1, 3 and 7 s, then every 15 s. A single startup
            // beacon is easily lost or lands before the phone's listener is
            // up, and the next one 15 s later was most of a cold start.
            let mut sent: usize = 0;
            while !shutdown.load(Ordering::Acquire) {
                if let Err(error) = broadcast_identity(&config, tcp_port) {
                    push_error(&errors, error);
                }
                beacon_unreachable_paired(&config, &devices, tcp_port);
                let wait_ms = BEACON_BURST_GAPS_MS.get(sent).copied().unwrap_or(15_000);
                sent = sent.saturating_add(1);
                for _ in 0..wait_ms / 250 {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    // The network changed: start the burst over, now.
                    if BEACON_AGAIN.swap(false, Ordering::AcqRel) {
                        sent = 0;
                        break;
                    }
                    thread::sleep(Duration::from_millis(250));
                }
            }
        });
    }
}

impl DaemonWorker {
    /// Advertises `_desklink._udp` over mDNS, the path Android already browses
    /// (`MdnsDiscovery.kt`). Many access points filter broadcast but pass
    /// multicast, so this is a second, independent way to be found. Uses its
    /// own responder rather than Avahi, which is off on many desktops.
    /// Best effort: discovery still works by broadcast if this fails.
    pub(super) fn start_mdns_advertiser(&self) {
        let Ok(info) = self.config.lock().map(|config| config.local_device_info()) else {
            return;
        };
        let shutdown = Arc::clone(&self.shutdown);
        let trusted = Arc::clone(&self.config);
        let own_id = info.id.clone();
        let spawned = thread::Builder::new()
            .name("desklink-mdns".to_string())
            .spawn(move || {
                let daemon = match mdns_sd::ServiceDaemon::new() {
                    Ok(daemon) => daemon,
                    Err(error) => {
                        eprintln!("[Daemon] mDNS advertising unavailable: {error}");
                        return;
                    }
                };
                let protocol = info.protocol_version.to_string();
                let properties = [
                    ("id", info.id.as_str()),
                    ("name", info.name.as_str()),
                    ("type", info.device_type.as_str()),
                    ("protocol", protocol.as_str()),
                ];
                let service = mdns_sd::ServiceInfo::new(
                    MDNS_SERVICE_TYPE,
                    &info.id,
                    &format!("{}.local.", info.id),
                    "",
                    UDP_PORT,
                    &properties[..],
                )
                .map(mdns_sd::ServiceInfo::enable_addr_auto);
                match service
                    .map_err(|error| error.to_string())
                    .and_then(|service| daemon.register(service).map_err(|error| error.to_string()))
                {
                    Ok(()) => debug_log!("[Daemon] Advertising {MDNS_SERVICE_TYPE} over mDNS"),
                    Err(error) => eprintln!("[Daemon] mDNS advertising failed: {error}"),
                }
                // Also browse: a phone found by mDNS is another address to
                // dial (broadcast-filtering networks often pass multicast).
                let browser = daemon.browse(MDNS_SERVICE_TYPE).ok();
                while !shutdown.load(Ordering::Acquire) {
                    let Some(browser) = browser.as_ref() else {
                        thread::sleep(Duration::from_millis(500));
                        continue;
                    };
                    if let Ok(mdns_sd::ServiceEvent::ServiceResolved(found)) =
                        browser.recv_timeout(Duration::from_millis(500))
                    {
                        let Some(id) = found.get_property_val_str("id").map(str::to_string) else {
                            continue;
                        };
                        if id == own_id
                            || !trusted.lock().is_ok_and(|config| config.is_trusted(&id))
                        {
                            continue;
                        }
                        for ip in found.get_addresses() {
                            note_address(&id, ip.to_ip_addr());
                        }
                    }
                }
                let _ = daemon.shutdown();
            });
        if let Err(error) = spawned {
            eprintln!("[Daemon] could not start mDNS advertising: {error}");
        }
    }
}

/// Must match `DeskLinkProtocol.MDNS_SERVICE_TYPE` on Android.
const MDNS_SERVICE_TYPE: &str = "_desklink._udp.local.";

/// Gaps after each of the first beacons; afterwards every 15 s.
const BEACON_BURST_GAPS_MS: [u64; 3] = [1_000, 2_000, 4_000];

pub(super) fn broadcast_identity(config: &Arc<Mutex<Config>>, tcp_port: u16) -> Result<(), String> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|err| err.to_string())?;
    socket.set_broadcast(true).map_err(|err| err.to_string())?;
    let packet = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .to_identity_packet(tcp_port);
    socket
        .send_to(
            &packet.serialize_line().map_err(|err| err.to_string())?,
            ("255.255.255.255", UDP_PORT),
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// Sends the identity beacon straight to each paired device that is not
/// reachable, at the address it was last seen on. Access points that filter
/// broadcast (and multicast) still pass unicast, and the phone dials back on
/// any beacon it hears.
// ponytail: last addresses live in memory only; persist them if cold starts
// on broadcast-filtering networks turn out to matter.
fn beacon_unreachable_paired(
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    tcp_port: u16,
) {
    let unreachable: Vec<(String, Option<std::net::IpAddr>)> = devices
        .lock()
        .map(|devices| {
            devices
                .values()
                .filter(|device| device.trusted && device.status == DeviceStatus::Unreachable)
                .map(|device| (device.id.clone(), device.address.parse().ok()))
                .collect()
        })
        .unwrap_or_default();
    // Every address a phone was seen on, not only the newest.
    let book = addresses()
        .lock()
        .map(|book| book.all())
        .unwrap_or_default();
    let mut targets: Vec<std::net::IpAddr> = Vec::new();
    for (id, current) in &unreachable {
        targets.extend(current);
        targets.extend(
            book.iter()
                .filter(|(owner, _)| owner == id)
                .map(|(_, ip)| *ip),
        );
    }
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return;
    }
    let Ok(packet) = config.lock().map_err(|_| ()).and_then(|config| {
        config
            .local_device_info()
            .to_identity_packet(tcp_port)
            .serialize_line()
            .map_err(|_| ())
    }) else {
        return;
    };
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)) else {
        return;
    };
    for ip in targets {
        let _ = socket.send_to(&packet, (ip, UDP_PORT));
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_identity_packet(
    packet: NetworkPacket,
    address: SocketAddr,
    local_tcp_port: u16,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    errors: &Arc<Mutex<Vec<String>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
    last_connections: &Arc<Mutex<HashMap<String, Instant>>>,
) {
    let Some(info) = DeviceInfo::from_identity_packet(&packet) else {
        return;
    };
    let local_id = config
        .lock()
        .ok()
        .map(|config| config.local_device_info().id);
    if local_id.as_deref() == Some(info.id.as_str()) {
        return;
    }
    if should_throttle_connection(last_connections, &info.id) {
        return;
    }

    // A beacon from a paired phone is a sign of it: its address is kept, the
    // addresses get a fresh chance, and a recovery ladder that ran out
    // starts over (and reconnects at once).
    if config
        .lock()
        .is_ok_and(|config| config.is_trusted(&info.id))
    {
        if let Ok(mut book) = addresses().lock() {
            book.seen(&info.id, address.ip());
            book.wake(Some(&info.id));
        }
        crate::device_links::webrtc::coordinator::recovery_sign(
            &info.id, sessions, config, devices, events,
        );
    }

    // A closed link stays installed while a WebRTC transport outlives it, so
    // ask about the socket rather than the binding: otherwise a device whose
    // socket died is never offered a replacement connection.
    // ...unless this beacon comes from a restarted process: then the socket
    // and WebRTC peer this side holds are the dead previous instance's.
    if !sessions.wants_bootstrap_connection(&info.id)
        && !sessions.peer_restarted(&info.id, info.instance_id.as_deref())
    {
        return;
    }
    let paired = config
        .lock()
        .map(|config| config.is_trusted(&info.id))
        .unwrap_or(false);
    if paired
        && config
            .lock()
            .ok()
            .and_then(|config| config.trusted_protocol_version(&info.id))
            .is_some_and(|trusted_version| trusted_version > info.protocol_version)
    {
        push_error(
            errors,
            format!(
                "Refusing downgraded protocol from {}: {}",
                info.name, info.protocol_version
            ),
        );
        return;
    }
    upsert_device(devices, &info, address.ip().to_string(), paired);
    remember_endpoint(&info, &packet, paired);

    let Some(remote_port) = packet
        .get_i64("tcpPort")
        .and_then(|port| u16::try_from(port).ok())
    else {
        return;
    };
    if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&remote_port) {
        return;
    }
    if remote_port == local_tcp_port && address.ip().is_loopback() {
        return;
    }

    let config = Arc::clone(config);
    let devices = Arc::clone(devices);
    let sessions = Arc::clone(sessions);
    let events = Arc::clone(events);

    let Some(slot) = super::network::HandshakeSlot::claim(address.ip()) else {
        return;
    };
    let spawned = thread::Builder::new().spawn(move || {
        let _slot = slot;
        if let Err(error) = connect_to_device(
            info.clone(),
            address,
            remote_port,
            config,
            devices.clone(),
            Arc::clone(&sessions),
            events,
        ) {
            if should_publish_outgoing_failure(&sessions, &info.id) {
                mark_error(&devices, &info.id, error.clone());
                eprintln!(
                    "[Daemon] Outgoing TCP connection to {} failed: {}",
                    info.id, error
                );
            } else {
                eprintln!(
                    "[DL-WRTC-BOOT] ignored stale outgoing connection failure for {}: {}",
                    info.id, error
                );
            }
        }
    });
    if let Err(error) = spawned {
        eprintln!("[Daemon] could not start an outgoing connection thread: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_phones_without_a_live_link_are_redialled_at_their_last_address() {
        let info = |id: &str| DeviceInfo {
            id: id.into(),
            name: id.into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        let view = |id: &str, address: &str, trusted: bool| {
            DeviceView::from_info(&info(id), address.to_string(), trusted)
        };
        let devices: HashMap<String, DeviceView> = [
            view("linked", "10.0.0.2", true),
            view("unlinked", "10.0.0.3", true),
            view("stranger", "10.0.0.4", false),
            view("no-port", "10.0.0.5", true),
            view("relay-only", "unknown", true),
        ]
        .into_iter()
        .map(|device| (device.id.clone(), device))
        .collect();
        let mut beacon = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        beacon.set("tcpPort", 1716);
        for id in ["linked", "unlinked", "stranger", "relay-only"] {
            remember_endpoint(&info(id), &beacon, true);
        }
        let mut odd = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        odd.set("tcpPort", 22);
        remember_endpoint(&info("no-port"), &odd, true);
        let known = known_endpoints().lock().unwrap().clone();
        // `wants` is the session's view: here only "linked" has a live socket.
        let mut persisted = HashMap::new();
        persisted.insert("restarted".to_string(), "10.0.0.7:1716".parse().unwrap());
        let mut devices = devices;
        let restarted = view("restarted", "", true);
        devices.insert(restarted.id.clone(), restarted);
        let targets = redial_targets(
            &devices,
            &known,
            &persisted,
            &AddressBook::default(),
            Instant::now(),
            |id| id != "linked",
        );
        let dialled: Vec<_> = targets
            .iter()
            .map(|(info, ip, port)| (info.id.as_str(), ip.to_string(), *port))
            .collect();
        let mut dialled = dialled;
        dialled.sort();
        assert_eq!(
            dialled,
            [
                ("restarted", "10.0.0.7".to_string(), 1716),
                ("unlinked", "10.0.0.3".to_string(), 1716)
            ],
            "after a restart, the address remembered from last time"
        );
    }
    use crate::device_links::core::SessionLink;

    fn ip(text: &str) -> std::net::IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn a_phone_keeps_its_last_three_ipv4_addresses_and_one_ipv6() {
        let mut book = AddressBook::default();
        let now = Instant::now();
        for address in [
            "10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4", "fe80::1", "fe80::2",
        ] {
            book.seen("phone", ip(address));
        }
        assert_eq!(
            book.due("phone", now),
            [
                ip("fe80::2"),
                ip("10.0.0.4"),
                ip("10.0.0.3"),
                ip("10.0.0.2")
            ],
            "newest first; the oldest IPv4 and the older IPv6 are gone"
        );
        // Seen again: moves to the front, once.
        book.seen("phone", ip("10.0.0.2"));
        assert_eq!(book.due("phone", now)[0], ip("10.0.0.2"));
        assert_eq!(book.due("phone", now).len(), 4);
        // Never loopback, unspecified or multicast.
        book.seen("phone", ip("127.0.0.1"));
        book.seen("phone", ip("0.0.0.0"));
        book.seen("phone", ip("224.0.0.1"));
        assert_eq!(book.due("phone", now).len(), 4);
        book.forget("phone");
        assert!(book.due("phone", now).is_empty());
    }

    #[test]
    fn a_dead_address_backs_off_instead_of_being_redialled_every_few_seconds() {
        let backoffs: Vec<u64> = (1..=9).map(|n| address_backoff(n).as_secs()).collect();
        assert_eq!(backoffs, [3, 6, 12, 24, 48, 96, 192, 300, 300]);
        let mut book = AddressBook::default();
        let start = Instant::now();
        book.seen("phone", ip("10.0.0.2"));
        book.seen("phone", ip("10.0.0.3"));
        // The newest is dead; the other is still dialled at once.
        book.failed("phone", ip("10.0.0.3"), start);
        assert_eq!(book.due("phone", start), [ip("10.0.0.2")]);
        assert!(book.due("phone", start + Duration::from_secs(2)).len() == 1);
        assert_eq!(book.due("phone", start + Duration::from_secs(3)).len(), 2);
        // Each failure doubles the wait.
        book.failed("phone", ip("10.0.0.3"), start);
        assert_eq!(book.due("phone", start + Duration::from_secs(5)).len(), 1);
        assert_eq!(book.due("phone", start + Duration::from_secs(6)).len(), 2);
        // A sign of the phone (a beacon, a network change) clears every wait.
        book.failed("phone", ip("10.0.0.2"), start);
        book.failed("phone", ip("10.0.0.3"), start);
        assert!(book.due("phone", start).is_empty());
        book.wake(Some("phone"));
        assert_eq!(book.due("phone", start).len(), 2);
        // Others are untouched by one phone's wake.
        book.seen("other", ip("10.0.0.9"));
        book.failed("other", ip("10.0.0.9"), start);
        book.wake(Some("phone"));
        assert!(book.due("other", start).is_empty());
        book.wake(None);
        assert_eq!(book.due("other", start).len(), 1);
    }

    /// A network change starts the beacon burst at once and gives every dead
    /// address a fresh chance.
    #[test]
    fn network_up_restarts_the_beacons_and_wakes_every_backed_off_address() {
        let id = "network-up-phone";
        note_address(id, ip("10.9.0.2"));
        note_address(id, ip("10.9.0.3"));
        let now = Instant::now();
        {
            let mut book = addresses().lock().unwrap();
            book.failed(id, ip("10.9.0.2"), now);
            book.failed(id, ip("10.9.0.3"), now);
            assert!(book.due(id, now).is_empty());
        }
        BEACON_AGAIN.store(false, Ordering::Release);
        beacon_again();
        assert!(
            BEACON_AGAIN.swap(false, Ordering::AcqRel),
            "the burst starts over"
        );
        assert_eq!(addresses().lock().unwrap().due(id, now).len(), 2);
        // The unicast burst reaches every address of an unreachable phone.
        let book = addresses().lock().unwrap().all();
        assert!(book.iter().filter(|(owner, _)| owner == id).count() >= 2);
        forget_addresses(id);
    }

    #[test]
    fn the_redialer_dials_every_address_the_phone_was_seen_on() {
        let info = DeviceInfo {
            id: "multi".into(),
            name: "Multi".into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        let mut view = DeviceView::from_info(&info, "10.0.0.8".to_string(), true);
        view.trusted = true;
        let devices: HashMap<String, DeviceView> = [("multi".to_string(), view)].into();
        let known: HashMap<String, (DeviceInfo, u16)> =
            [("multi".to_string(), (info, 1716))].into();
        let mut book = AddressBook::default();
        let now = Instant::now();
        for address in ["10.0.0.5", "10.0.0.6", "10.0.0.7"] {
            book.seen("multi", ip(address));
        }
        book.failed("multi", ip("10.0.0.6"), now);
        let targets = redial_targets(&devices, &known, &HashMap::new(), &book, now, |_| true);
        let mut dialled: Vec<String> = targets.iter().map(|(_, ip, _)| ip.to_string()).collect();
        dialled.sort();
        assert_eq!(
            dialled,
            ["10.0.0.5", "10.0.0.7", "10.0.0.8"],
            "the three seen (one backing off) and the address on its card"
        );
        assert!(targets
            .iter()
            .all(|(info, _, port)| info.id == "multi" && *port == 1716));
    }

    /// Only paired phones are remembered, strangers' identities are not
    /// kept even in memory, and the file stays bounded.
    #[test]
    fn only_paired_phones_endpoints_are_kept_and_the_file_is_bounded() {
        let info = |id: &str| DeviceInfo {
            id: id.to_string(),
            name: id.to_string(),
            device_type: "phone".to_string(),
            protocol_version: crate::device_links::packet::PROTOCOL_VERSION,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        let mut beacon = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        beacon.set("tcpPort", 1716);
        remember_endpoint(&info("spoofed-stranger"), &beacon, false);
        assert!(!known_endpoints()
            .lock()
            .unwrap()
            .contains_key("spoofed-stranger"));

        let address: SocketAddr = "10.0.0.9:1716".parse().unwrap();
        let mut trusted: std::collections::HashSet<String> =
            ["phone".to_string(), "kept".to_string()].into();
        let mut all = HashMap::new();
        all.insert("unpaired-since".to_string(), address);
        all.insert("kept".to_string(), address);
        let updated = updated_endpoints(all, "phone", address, &trusted);
        let mut ids: Vec<_> = updated.keys().cloned().collect();
        ids.sort();
        assert_eq!(ids, ["kept", "phone"], "pruned to paired phones");
        assert!(
            updated_endpoints(HashMap::new(), "stranger", address, &trusted).is_empty(),
            "never a stranger"
        );
        let many: HashMap<String, SocketAddr> =
            (0..100).map(|n| (format!("p{n}"), address)).collect();
        trusted.extend(many.keys().cloned());
        let capped = updated_endpoints(many, "phone", address, &trusted);
        assert_eq!(capped.len(), MAX_PERSISTED_ENDPOINTS);
        assert!(capped.contains_key("phone"), "the one just seen stays");
    }

    #[test]
    fn stale_outgoing_failure_is_not_published_after_an_inbound_link_wins() {
        let sessions = DeviceManager::new();
        assert!(should_publish_outgoing_failure(&sessions, "phone-1"));

        sessions
            .register_link(
                "phone-1".to_string(),
                SessionLink::test_placeholder("phone-1"),
                true,
            )
            .unwrap();

        assert!(!should_publish_outgoing_failure(&sessions, "phone-1"));
    }
}
