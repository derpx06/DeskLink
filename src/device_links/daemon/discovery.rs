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

/// What a paired phone last said about itself and the TCP port it listens
/// on, from its identity (a beacon, or an incoming connection's first line).
/// The address comes from its `DeviceView`.
fn known_endpoints() -> &'static Mutex<HashMap<String, (DeviceInfo, u16)>> {
    static KNOWN: std::sync::OnceLock<Mutex<HashMap<String, (DeviceInfo, u16)>>> =
        std::sync::OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// Records `info`'s listening port from an identity packet, when it has one.
pub(super) fn remember_endpoint(info: &DeviceInfo, packet: &NetworkPacket) {
    let Some(port) = packet
        .get_i64("tcpPort")
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| (MIN_TCP_PORT..=MAX_TCP_PORT).contains(port))
    else {
        return;
    };
    if let Ok(mut known) = known_endpoints().lock() {
        known.insert(info.id.clone(), (info.clone(), port));
    }
}

/// How often paired phones without a bootstrap link are redialled.
const REDIAL_EVERY: Duration = Duration::from_secs(3);

/// The paired phones to dial now: no live bootstrap link (whether or not
/// their WebRTC peer survives), a last-known address and listening port.
fn redial_targets(
    devices: &HashMap<String, DeviceView>,
    known: &HashMap<String, (DeviceInfo, u16)>,
    wants: impl Fn(&str) -> bool,
) -> Vec<(DeviceInfo, std::net::IpAddr, u16)> {
    devices
        .values()
        .filter(|device| device.trusted && wants(&device.id))
        .filter_map(|device| {
            let ip = device.address.parse().ok()?;
            let (info, port) = known.get(&device.id)?;
            Some((info.clone(), ip, *port))
        })
        .collect()
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
            while !shutdown.load(Ordering::Acquire) {
                thread::sleep(REDIAL_EVERY);
                let targets = {
                    let (Ok(devices), Ok(known)) = (devices.lock(), known_endpoints().lock())
                    else {
                        continue;
                    };
                    redial_targets(&devices, &known, |id| {
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
                        if let Err(error) = connect_to_device(
                            info,
                            SocketAddr::new(ip, port),
                            port,
                            config,
                            devices,
                            sessions,
                            events,
                        ) {
                            debug_log!(
                                "[DL-WRTC-BOOT] redial of {id} at {ip}:{port} failed: {error}"
                            );
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
                            || err.kind() == std::io::ErrorKind::TimedOut => {}
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
                while !shutdown.load(Ordering::Acquire) {
                    thread::sleep(Duration::from_millis(500));
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
    let targets: Vec<std::net::IpAddr> = devices
        .lock()
        .map(|devices| {
            devices
                .values()
                .filter(|device| device.trusted && device.status == DeviceStatus::Unreachable)
                .filter_map(|device| device.address.parse().ok())
                .collect()
        })
        .unwrap_or_default();
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
    remember_endpoint(&info, &packet);

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
            remember_endpoint(&info(id), &beacon);
        }
        let mut odd = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        odd.set("tcpPort", 22);
        remember_endpoint(&info("no-port"), &odd);
        let known = known_endpoints().lock().unwrap().clone();
        // `wants` is the session's view: here only "linked" has a live socket.
        let targets = redial_targets(&devices, &known, |id| id != "linked");
        let dialled: Vec<_> = targets
            .iter()
            .map(|(info, ip, port)| (info.id.as_str(), ip.to_string(), *port))
            .collect();
        assert_eq!(dialled, [("unlinked", "10.0.0.3".to_string(), 1716)]);
    }
    use crate::device_links::core::SessionLink;

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
