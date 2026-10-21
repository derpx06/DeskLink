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
                let Some(slot) = stream
                    .peer_addr()
                    .ok()
                    .and_then(|addr| super::network::HandshakeSlot::claim(addr.ip()))
                else {
                    // Dropping the stream closes it: at capacity, refuse rather
                    // than queue an unauthenticated connection.
                    continue;
                };
                let config = Arc::clone(&config);
                let devices = Arc::clone(&devices);
                let sessions = Arc::clone(&sessions);
                let errors = Arc::clone(&errors);
                let events = Arc::clone(&events);
                let spawned = thread::Builder::new().spawn(move || {
                    let _slot = slot;
                    let peer = stream
                        .peer_addr()
                        .map_or_else(|_| "unknown".to_string(), |addr| addr.to_string());
                    if let Err(error) =
                        accept_incoming_device(stream, config, devices, sessions, events)
                    {
                        // A KDE Connect client (GSConnect, kdeconnectd) on the LAN
                        // retries forever; say so once per address, not per try.
                        if !error.contains("kdeconnect.identity") || first_sighting(&peer) {
                            eprintln!(
                                "[DL-WRTC-BOOT] incoming bootstrap connection from {peer} rejected: {error}"
                            );
                        }
                        // Anyone on the LAN can open this port, so a rejected
                        // unauthenticated connection is noise, not news: it is
                        // logged, never shown. The one exception is a paired
                        // device presenting a different certificate, which may
                        // be someone impersonating it.
                        if error.contains("Trusted device certificate changed") {
                            push_error(&errors, error);
                        }
                    }
                });
                // `thread::spawn` panics at the thread limit, which killed this
                // accept loop — and with it every future connection.
                if let Err(error) = spawned {
                    eprintln!("[DL-WRTC-BOOT] could not start a handshake thread: {error}");
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => {
                push_error(&errors, format!("TCP listener failed: {error}"));
                // Out of file descriptors and the like: do not spin.
                thread::sleep(Duration::from_millis(200));
            }
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
/// True the first time an address is seen (bounded; for log damping only).
fn first_sighting(peer: &str) -> bool {
    static SEEN: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());
    let host = peer
        .rsplit_once(':')
        .map_or(peer, |(host, _)| host)
        .to_string();
    let Ok(mut seen) = SEEN.lock() else {
        return true;
    };
    if seen.contains(&host) {
        return false;
    }
    if seen.len() >= 64 {
        seen.remove(0);
    }
    seen.push(host);
    true
}

fn read_identity_line(reader: &mut BufReader<TcpStream>) -> Result<Vec<u8>, String> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    // The socket's 10 s timeout is per read, so a peer dripping one byte every
    // 9 s held this thread for hours. Bound the whole line too.
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if std::time::Instant::now() > deadline {
            return Err("Identity packet took too long".to_string());
        }
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
        .ok_or_else(|| format!("Invalid identity packet (type {})", packet.packet_type))?;
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
    let trusted = config
        .lock()
        .map(|config| config.is_trusted(&info.id))
        .unwrap_or(false);
    super::discovery::remember_endpoint(&info, &packet, trusted);
    upsert_device(
        &devices,
        &info,
        stream
            .peer_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        trusted,
    );
    // One deadline for TLS and the secure identity: per-read timeouts let a
    // peer dribbling bytes hold this connection for minutes.
    let deadline = super::network::HandshakeDeadline::arm(&stream, super::network::HANDSHAKE_LIMIT);
    let connector = ssl_connector(&config)?;
    let ssl_stream = connector
        .connect(&info.id, stream)
        .map_err(|err| err.to_string())?;
    finish_secure_link(
        info, ssl_stream, deadline, config, devices, sessions, events,
    )
}
