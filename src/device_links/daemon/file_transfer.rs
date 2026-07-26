use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::network::ssl_acceptor;
use super::DaemonWorker;
use crate::device_links::config::Config;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_SHARE_REQUEST};

impl DaemonWorker {
    pub(super) fn send_file(&self, device_id: &str, file_path: std::path::PathBuf) {
        let filename = match file_path.file_name().and_then(|s| s.to_str()) {
            Some(name) => name.to_string(),
            None => {
                eprintln!("[Daemon] Invalid file path: {:?}", file_path);
                return;
            }
        };

        let file_size = match std::fs::metadata(&file_path) {
            Ok(meta) => meta.len() as i64,
            Err(e) => {
                eprintln!(
                    "[Daemon] Failed to get file metadata for {:?}: {}",
                    file_path, e
                );
                return;
            }
        };

        eprintln!(
            "[Daemon] Preparing to upload file: {} ({} bytes)",
            filename, file_size
        );

        let listener = match TcpListener::bind("0.0.0.0:0") {
            Ok(l) => l,
            Err(e) => {
                eprintln!("[Daemon] Failed to bind TCP listener for upload: {}", e);
                return;
            }
        };

        let port = match listener.local_addr() {
            Ok(addr) => addr.port(),
            Err(e) => {
                eprintln!("[Daemon] Failed to get local address for listener: {}", e);
                return;
            }
        };

        let mut packet = NetworkPacket::new(PACKET_TYPE_SHARE_REQUEST);
        packet.set("filename", filename.clone());
        packet.payload_size = Some(file_size);

        let mut info = serde_json::Map::new();
        info.insert("port".to_string(), serde_json::Value::from(port));
        packet.payload_transfer_info = Some(info);

        let sent = self
            .sessions
            .current_binding(device_id)
            .filter(|binding| self.sessions.is_current(binding))
            .and_then(|binding| {
                let paired = self
                    .sessions
                    .with_session(&binding, |session| {
                        session.pairing.state == crate::device_links::pairing::PairState::Paired
                    })
                    .ok()?;
                if !paired {
                    return None;
                }
                let stream = binding.link.stream().ok()?;
                let bytes = packet.serialize_line().ok()?;
                stream.lock().ok()?.write_all(&bytes).ok()?;
                Some(())
            })
            .is_some();

        if !sent {
            eprintln!(
                "[Daemon] Failed to send share request packet to device {}",
                device_id
            );
            return;
        }

        eprintln!(
            "[Daemon] Share request sent. Listening for receiver on port {}...",
            port
        );

        let config_clone = Arc::clone(&self.config);
        thread::spawn(move || {
            listener.set_nonblocking(true).ok();
            let start = Instant::now();
            let socket = loop {
                if start.elapsed().as_secs() > 15 {
                    eprintln!(
                        "[Daemon] Timeout waiting for file receiver to connect on port {}",
                        port
                    );
                    return;
                }
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        eprintln!("[Daemon] Failed to accept upload connection: {}", e);
                        return;
                    }
                }
            };

            socket.set_nonblocking(false).ok();

            eprintln!("[Daemon] Receiver connected. Establishing SSL server handshake...");

            let acceptor = match ssl_acceptor(&config_clone) {
                Ok(a) => a,
                Err(e) => {
                    eprintln!("[Daemon] Failed to create SSL acceptor: {}", e);
                    return;
                }
            };

            let mut ssl_stream = match acceptor.accept(socket) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[Daemon] SSL handshake failed for upload: {}", e);
                    return;
                }
            };

            eprintln!("[Daemon] SSL handshake complete. Uploading file bytes...");

            let mut file = match std::fs::File::open(&file_path) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("[Daemon] Failed to open source file: {}", e);
                    return;
                }
            };

            let mut buffer = [0u8; 16384];
            let mut uploaded = 0;
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if ssl_stream.write_all(&buffer[..read]).is_err() {
                            eprintln!("[Daemon] Error writing file bytes to stream");
                            return;
                        }
                        uploaded += read;
                    }
                    Err(e) => {
                        eprintln!("[Daemon] Error reading source file: {}", e);
                        return;
                    }
                }
            }

            eprintln!(
                "[Daemon] Upload completed successfully! {} bytes sent.",
                uploaded
            );
        });
    }
}

pub(super) fn receive_file_payload(
    device_id: &str,
    peer_ip: &str,
    port: u16,
    size: i64,
    filename: &str,
    _config: Arc<Mutex<Config>>,
) -> Result<(), String> {
    eprintln!(
        "[Daemon] Connecting to payload transfer stream at {}:{}...",
        peer_ip, port
    );
    let socket = TcpStream::connect(format!("{}:{}", peer_ip, port))
        .map_err(|e| format!("Failed to connect to transfer socket: {}", e))?;

    let mut connector = SslConnector::builder(SslMethod::tls())
        .map_err(|e| format!("Failed to create SSL connector builder: {}", e))?;
    connector.set_verify(SslVerifyMode::NONE);

    let ssl = connector
        .build()
        .configure()
        .map_err(|e| format!("Failed to configure SSL: {}", e))?
        .into_ssl(device_id)
        .map_err(|e| format!("Failed to build SSL object: {}", e))?;

    let mut ssl_stream = ssl
        .connect(socket)
        .map_err(|e| format!("SSL handshake failed: {}", e))?;

    eprintln!("[Daemon] SSL handshake completed for file transfer.");

    let download_dir = dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let mut target_path = download_dir.join(filename);

    let mut counter = 1;
    let stem = target_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file")
        .to_string();
    let ext = target_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_string();
    while target_path.exists() {
        let new_name = if ext.is_empty() {
            format!("{}_{}", stem, counter)
        } else {
            format!("{}_{}.{}", stem, counter, ext)
        };
        target_path = download_dir.join(new_name);
        counter += 1;
    }

    let mut file = std::fs::File::create(&target_path)
        .map_err(|e| format!("Failed to create file {:?}: {}", target_path, e))?;

    let mut buffer = [0u8; 16384];
    let mut bytes_left = size;
    while bytes_left > 0 {
        let to_read = std::cmp::min(bytes_left, buffer.len() as i64) as usize;
        let read = ssl_stream
            .read(&mut buffer[..to_read])
            .map_err(|e| format!("Error reading from transfer stream: {}", e))?;
        if read == 0 {
            return Err("Unexpected EOF from transfer stream".to_string());
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("Failed to write to file: {}", e))?;
        bytes_left -= read as i64;
    }

    eprintln!("[Daemon] Saved downloaded file to: {:?}", target_path);
    Ok(())
}
