use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::network::{bind_udp_listener, MAX_TCP_PORT, MIN_TCP_PORT, UDP_PORT};
use super::state::{publish_device_changed, push_error, upsert_device};
use super::validation::should_throttle_connection;
use super::{DaemonWorker, ReconnectTarget};
use crate::device_links::config::Config;
use crate::device_links::core::device_manager::DeviceManager;
use crate::device_links::core::device_session::DeviceConnectionState;
use crate::device_links::core::events::{CoreEvent, EventBus};
use crate::device_links::device::DeviceView;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_IDENTITY};
use crate::protocol::desklink_v9::MDNS_SERVICE_TYPE;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

impl DaemonWorker {
    pub(super) fn start_udp_listener(&self) -> Result<(), String> {
        let socket = bind_udp_listener()?;
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|err| err.to_string())?;
        socket.set_broadcast(true).map_err(|err| err.to_string())?;
        let config = Arc::clone(&self.config);
        let devices = Arc::clone(&self.devices);
        let reconnect_targets = Arc::clone(&self.reconnect_targets);
        let sessions = self.sessions.clone();
        let shutdown = Arc::clone(&self.shutdown);
        let errors = Arc::clone(&self.errors);
        let events = self.events.clone();
        let tcp_port = self.tcp_port;
        let last_connections = Arc::new(Mutex::new(HashMap::new()));

        thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            loop {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
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
                                    &reconnect_targets,
                                    &sessions,
                                    &errors,
                                    &last_connections,
                                    &events,
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

    /// Publish and browse the native DeskLink DNS-SD service. Broadcast remains
    /// enabled above because some mobile networks filter multicast, but mDNS
    /// gives us discovery after an interface change without waiting for a
    /// broadcast interval.
    pub(super) fn start_mdns(&self) -> Result<(), String> {
        let daemon = ServiceDaemon::new().map_err(|error| error.to_string())?;
        let config = Arc::clone(&self.config);
        let devices = Arc::clone(&self.devices);
        let reconnect_targets = Arc::clone(&self.reconnect_targets);
        let sessions = self.sessions.clone();
        let shutdown = Arc::clone(&self.shutdown);
        let tcp_port = self.tcp_port;
        let events_bus = self.events.clone();

        let local = config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())?
            .local_device_info();
        let local_ip = local_ipv4().ok_or_else(|| {
            "Could not determine a local IPv4 address for DeskLink mDNS".to_string()
        })?;
        let host = format!("desklink-{}.local.", &local.id[..8]);
        let mut properties = HashMap::new();
        properties.insert("id".to_string(), local.id.clone());
        properties.insert("name".to_string(), local.name.clone());
        properties.insert("type".to_string(), local.device_type.clone());
        properties.insert("protocol".to_string(), local.protocol_version.to_string());
        let service_type = format!("{MDNS_SERVICE_TYPE}.local.");
        let service = ServiceInfo::new(
            &service_type,
            &local.id,
            &host,
            std::net::IpAddr::V4(local_ip),
            tcp_port,
            Some(properties),
        )
        .map_err(|error| error.to_string())?;
        daemon
            .register(service)
            .map_err(|error| error.to_string())?;
        let events = daemon
            .browse(&service_type)
            .map_err(|error| error.to_string())?;

        thread::spawn(move || {
            // Keep the daemon alive for the lifetime of the event loop. Its
            // background socket threads handle address refreshes for us.
            let _daemon = daemon;
            while let Ok(event) = events.recv_timeout(Duration::from_secs(30)) {
                if shutdown.load(Ordering::SeqCst) {
                    break;
                }
                if let ServiceEvent::ServiceResolved(info) = event {
                    let Some(device_id) = info.get_property_val_str("id") else {
                        continue;
                    };
                    if device_id == local.id {
                        continue;
                    }
                    let Some(address) = info
                        .get_addresses_v4()
                        .into_iter()
                        .next()
                        .map(|ip| SocketAddr::new(ip.into(), info.get_port()))
                    else {
                        continue;
                    };
                    let name = info
                        .get_property_val_str("name")
                        .unwrap_or(device_id)
                        .to_string();
                    let device_type = info
                        .get_property_val_str("type")
                        .unwrap_or("unknown")
                        .to_string();
                    let protocol_version = info
                        .get_property_val_str("protocol")
                        .and_then(|value| value.parse().ok())
                        .unwrap_or(0);
                    let info = DeviceInfo {
                        id: device_id.to_string(),
                        name,
                        device_type,
                        protocol_version,
                        incoming_capabilities: Vec::new(),
                        outgoing_capabilities: Vec::new(),
                    };
                    if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&address.port()) {
                        continue;
                    }
                    let paired = config
                        .lock()
                        .ok()
                        .map(|config| config.is_trusted(&info.id))
                        .unwrap_or(false);
                    upsert_device(&devices, &info, address.ip().to_string(), paired);
                    publish_device_changed(&devices, &events_bus, &info.id);
                    events_bus.publish(CoreEvent::ConnectionChanged {
                        device_id: info.id.clone(),
                        state: DeviceConnectionState::Discovered,
                        message: None,
                    });
                    remember_target(&reconnect_targets, &sessions, info, address, paired);
                }
            }
        });
        Ok(())
    }

    pub(super) fn start_broadcaster(&self) {
        let config = Arc::clone(&self.config);
        let errors = Arc::clone(&self.errors);
        let shutdown = Arc::clone(&self.shutdown);
        let tcp_port = self.tcp_port;
        thread::spawn(move || loop {
            if shutdown.load(Ordering::SeqCst) {
                break;
            }
            if let Err(error) = broadcast_identity(&config, tcp_port) {
                push_error(&errors, error);
            }
            thread::sleep(Duration::from_secs(15));
        });
    }
}

fn local_ipv4() -> Option<std::net::Ipv4Addr> {
    let socket = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    socket.connect(("192.0.2.1", 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_loopback() => Some(ip),
        _ => None,
    }
}

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

#[allow(clippy::too_many_arguments)]
fn handle_identity_packet(
    packet: NetworkPacket,
    address: SocketAddr,
    local_tcp_port: u16,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    reconnect_targets: &Arc<Mutex<HashMap<String, ReconnectTarget>>>,
    sessions: &DeviceManager,
    errors: &Arc<Mutex<Vec<String>>>,
    last_connections: &Arc<Mutex<HashMap<String, Instant>>>,
    events: &EventBus,
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
    publish_device_changed(devices, events, &info.id);
    events.publish(CoreEvent::ConnectionChanged {
        device_id: info.id.clone(),
        state: DeviceConnectionState::Discovered,
        message: None,
    });

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

    remember_target(
        reconnect_targets,
        sessions,
        info,
        SocketAddr::new(address.ip(), remote_port),
        paired,
    );
}

fn remember_target(
    targets: &Arc<Mutex<HashMap<String, ReconnectTarget>>>,
    sessions: &DeviceManager,
    info: DeviceInfo,
    address: SocketAddr,
    paired: bool,
) {
    let device_id = info.id.clone();
    if let Ok(mut targets) = targets.lock() {
        targets
            .entry(info.id.clone())
            .and_modify(|target| {
                target.info = info.clone();
                target.address = address;
            })
            .or_insert(ReconnectTarget { info, address });
    }
    sessions.observe_device(&device_id, paired);
}
