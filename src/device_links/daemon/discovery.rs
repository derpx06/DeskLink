use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::network::{bind_udp_listener, MAX_TCP_PORT, MIN_TCP_PORT};
use super::state::{mark_error, push_error, upsert_device};
use super::validation::{connect_to_device, should_throttle_connection};
use super::DaemonWorker;
use crate::device_links::config::Config;
use crate::device_links::device::DeviceView;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_IDENTITY};

const UDP_PORT: u16 = 1716;

impl DaemonWorker {
    pub(super) fn start_udp_listener(&self) -> Result<(), String> {
        let socket = bind_udp_listener()?;
        socket
            .set_read_timeout(Some(Duration::from_millis(500)))
            .map_err(|err| err.to_string())?;
        socket.set_broadcast(true).map_err(|err| err.to_string())?;
        let config = Arc::clone(&self.config);
        let devices = Arc::clone(&self.devices);
        let links = Arc::clone(&self.links);
        let errors = Arc::clone(&self.errors);
        let tcp_port = self.tcp_port;
        let last_connections = Arc::new(Mutex::new(HashMap::new()));

        thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            loop {
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
                                    &links,
                                    &errors,
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
        let tcp_port = self.tcp_port;
        thread::spawn(move || loop {
            if let Err(error) = broadcast_identity(&config, tcp_port) {
                push_error(&errors, error);
            }
            thread::sleep(Duration::from_secs(15));
        });
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
    links: &Arc<Mutex<HashMap<String, super::Link>>>,
    errors: &Arc<Mutex<Vec<String>>>,
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
    let links = Arc::clone(links);

    thread::spawn(move || {
        if let Err(error) = connect_to_device(
            info.clone(),
            address,
            remote_port,
            config,
            devices.clone(),
            links,
        ) {
            mark_error(&devices, &info.id, error.clone());
            eprintln!(
                "[Daemon] Outgoing TCP connection to {} failed: {}",
                info.id, error
            );
        }
    });
}
