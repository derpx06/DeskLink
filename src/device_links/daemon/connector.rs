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
use super::state::{push_error, upsert_from_beacon};
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
            Ok((stream, source)) => {
                // Before a slot, a byte read or a thread: a routable host (a port forward, a routed VPN, a hotel
                // network) must not reach the pairing prompt, the "nearby device" cards or the unpaired slots.
                if !super::network::source_allowed(source) {
                    continue; // dropping the stream closes it
                }
                let Some(slot) = stream
                    .peer_addr()
                    .ok()
                    .and_then(|addr| super::network::HandshakeSlot::claim(addr.ip()))
                else {
                    // Dropping the stream closes it: at capacity, refuse rather
                    // than queue an unauthenticated connection. One line per 30 s
                    // (a port scan is one line): a silent refusal is what a
                    // "stuck, nothing in the log" report could not tell from none.
                    crate::device_links::limits::log_limited_every(
                        "slot-refused",
                        &format!(
                            "{} Refused a connection from {} (a local address): too many handshakes in progress",
                            crate::device_links::webrtc::coordinator::stamp(),
                            source.ip()
                        ),
                        Duration::from_secs(30),
                    );
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
    // A file lane (W18) names itself in the first line: not an identity, not
    // a device to list. It carries file frames and nothing else.
    if packet.packet_type == crate::device_links::webrtc::file_lane::FIRST_LINE_TYPE {
        // The identity reader allows far more than a lane line may be.
        crate::device_links::webrtc::file_lane::first_line_within_bound(&line)?;
        return crate::device_links::webrtc::file_lane::accept(stream, &config, &packet);
    }
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
    // Pre-TLS and unauthenticated: it may create a card, never rewrite a
    // known one (the pinned handshake does that, in `finish_secure_link`).
    upsert_from_beacon(
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
    // The phone dialed this computer's listener.
    finish_secure_link(
        info, ssl_stream, deadline, config, devices, sessions, events, false,
    )
    .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::net::{IpAddr, SocketAddr};

    /// Leak detection (F-QA3-4): however a pre-auth connection from one
    /// address ends (closed at once, garbage, a valid identity then silence,
    /// a half-sent line), its handshake slot comes back, so the address can
    /// still be served afterwards. A leaked slot refuses every later
    /// connection from that address silently (no log line, no card).
    #[test]
    fn aborted_handshakes_from_one_address_leak_no_slot() {
        let dir = std::env::temp_dir().join(format!("desklink-slots-{}", uuid::Uuid::new_v4()));
        let config = Arc::new(Mutex::new(Config::load_from_dir(dir).expect("config")));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let (devices, errors, events) = (
            Arc::new(Mutex::new(HashMap::new())),
            Arc::new(Mutex::new(Vec::new())),
            Arc::new(Mutex::new(Vec::new())),
        );
        let sessions = Arc::new(DeviceManager::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let accept = {
            let (config, devices, sessions, errors, events, shutdown) = (
                Arc::clone(&config),
                Arc::clone(&devices),
                Arc::clone(&sessions),
                Arc::clone(&errors),
                Arc::clone(&events),
                Arc::clone(&shutdown),
            );
            thread::spawn(move || {
                incoming_tcp_loop(
                    listener, config, devices, sessions, errors, events, shutdown,
                )
            })
        };
        // 127.0.0.2: no other test claims slots for it.
        let host: IpAddr = "127.0.0.2".parse().unwrap();
        let dial = || {
            let socket = std::net::TcpStream::connect_timeout(
                &SocketAddr::new("127.0.0.1".parse().unwrap(), port),
                Duration::from_secs(2),
            );
            socket.ok()
        };
        let identity = br#"{"id":1,"type":"kdeconnect.identity","body":{"deviceId":"zzz","deviceName":"x","deviceType":"phone","protocolVersion":8,"tcpPort":1716}}"#;
        let mut keep = Vec::new();
        for round in 0..40 {
            let Some(mut socket) = dial() else { continue };
            match round % 4 {
                0 => drop(socket),
                1 => {
                    let _ = socket.write_all(b"not json\n");
                    drop(socket);
                }
                2 => {
                    let _ = socket.write_all(identity);
                    let _ = socket.write_all(b"\n");
                    drop(socket);
                }
                _ => {
                    let _ = socket.write_all(b"{\"half\":");
                    keep.push(socket); // silent, closed at the end
                }
            }
        }
        // Everything (the silent ones too) ends within the read timeout.
        drop(keep);
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        // The loopback source of every dial is 127.0.0.1.
        let source: IpAddr = "127.0.0.1".parse().unwrap();
        while super::super::network::slots_held(source) > 0 && std::time::Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(100));
        }
        let held = super::super::network::slots_held(source);
        shutdown.store(true, Ordering::Release);
        let _ = accept.join();
        assert_eq!(held, 0, "slots leaked for {host}: {held}");
    }
}
