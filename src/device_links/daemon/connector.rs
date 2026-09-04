use std::collections::HashMap;
use std::io::{BufReader, Read};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::handle::DaemonEvent;
use super::handshake::finish_secure_link;
use super::network::ssl_connector;
use super::state::{push_error, upsert_device};
use crate::device_links::config::Config;
use crate::device_links::core::DeviceManager;
use crate::device_links::device::DeviceView;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::NetworkPacket;

pub(super) fn incoming_tcp_loop(
    listener: TcpListener,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
    errors: Arc<Mutex<Vec<String>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
    shutdown: Arc<AtomicBool>,
) {
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                let config = Arc::clone(&config);
                let devices = Arc::clone(&devices);
                let sessions = Arc::clone(&sessions);
                let errors = Arc::clone(&errors);
                let events = Arc::clone(&events);
                thread::spawn(move || {
                    if let Err(error) =
                        accept_incoming_device(stream, config, devices, sessions, events)
                    {
                        eprintln!("[DL-WRTC-BOOT] incoming bootstrap connection rejected: {error}");
                        push_error(&errors, error);
                    }
                });
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => push_error(&errors, format!("TCP listener failed: {error}")),
        }
    }
}

/// Largest identity packet accepted before TLS, matching
/// `MAX_IDENTITY_PACKET_SIZE` in the Android `LanLinkProvider`.
const MAX_IDENTITY_PACKET_BYTES: usize = 512 * 1024;

/// Reads one newline-delimited identity packet from an unauthenticated peer.
///
/// This runs before the TLS handshake, so the peer is anonymous and the read
/// must be bounded. An unbounded `read_until` let a peer that connects and
/// never sends a newline grow this buffer until the process was killed. Android
/// has always bounded the same read; the desktop did not.
fn read_identity_line(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = reader.read(&mut byte).map_err(|err| err.to_string())?;
        if read == 0 {
            return Err("Connection closed".to_string());
        }
        if byte[0] == b'\n' {
            if line.is_empty() {
                continue; // Keep-alive
            }
            line.push(byte[0]);
            return Ok(line);
        }
        line.push(byte[0]);
        if line.len() >= MAX_IDENTITY_PACKET_BYTES {
            return Err("Identity packet is too large".to_string());
        }
    }
}

fn accept_incoming_device(
    stream: TcpStream,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    // The listener is non-blocking so the accept loop can observe shutdown.
    // Accepted sockets must be made blocking before OpenSSL starts its
    // handshake; otherwise a transient incomplete TLS read is surfaced as
    // `nonblocking read call would have blocked` and the valid peer is
    // incorrectly disconnected.
    stream
        .set_nonblocking(false)
        .map_err(|err| format!("Could not configure incoming socket: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    super::network::enable_keepalive(&stream);
    let mut reader = BufReader::new(stream.try_clone().map_err(|err| err.to_string())?);
    let line = read_identity_line(&mut reader)?;
    let packet = NetworkPacket::deserialize(&line).map_err(|err| err.to_string())?;
    let info = DeviceInfo::from_identity_packet(&packet)
        .ok_or_else(|| "Invalid identity packet".to_string())?;
    let local_id = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .id;
    if packet
        .get_str("targetDeviceId")
        .is_some_and(|target| target != local_id)
    {
        return Err("Connection targeted another device".to_string());
    }
    upsert_device(
        &devices,
        &info,
        stream
            .peer_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        config
            .lock()
            .map(|config| config.is_trusted(&info.id))
            .unwrap_or(false),
    );
    let connector = ssl_connector(&config)?;
    let ssl_stream = connector
        .connect(&info.id, stream)
        .map_err(|err| err.to_string())?;
    finish_secure_link(info, ssl_stream, config, devices, sessions, events)
}
