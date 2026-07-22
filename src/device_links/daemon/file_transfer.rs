use openssl::hash::{Hasher, MessageDigest};
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use std::fs::OpenOptions;
use std::io::{Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use uuid::Uuid;

use super::super::core::events::{CoreEvent, EventBus};
use super::super::core::transfer_manager::{
    TransferCheckpoint, TransferCheckpointStore, TransferManager, TransferState, MAX_TRANSFER_SIZE,
};
use super::network::ssl_acceptor;
use super::DaemonWorker;
use crate::device_links::config::Config;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_SHARE_REQUEST};

impl DaemonWorker {
    pub(super) fn send_file(&self, device_id: &str, file_path: std::path::PathBuf) {
        self.send_file_with_id(device_id, file_path, Uuid::new_v4().to_string());
    }

    pub(super) fn send_file_with_id(
        &self,
        device_id: &str,
        file_path: std::path::PathBuf,
        transfer_id: String,
    ) {
        let filename = match file_path.file_name().and_then(|s| s.to_str()) {
            Some(name) => name.to_string(),
            None => {
                publish_transfer_failure(
                    &self.events,
                    &transfer_id,
                    "Invalid file path".to_string(),
                );
                return;
            }
        };

        let file_size = match std::fs::metadata(&file_path) {
            Ok(meta) if meta.len() <= MAX_TRANSFER_SIZE => meta.len() as i64,
            Ok(meta) => {
                publish_transfer_failure(
                    &self.events,
                    &transfer_id,
                    format!(
                        "File exceeds the {} byte transfer limit ({})",
                        MAX_TRANSFER_SIZE,
                        meta.len()
                    ),
                );
                return;
            }
            Err(e) => {
                publish_transfer_failure(&self.events, &transfer_id, e.to_string());
                return;
            }
        };

        let transfer_token = Uuid::new_v4().to_string();
        let mut checkpoint = match TransferCheckpoint::new(
            transfer_id.clone(),
            device_id.to_string(),
            filename.clone(),
            file_path.clone(),
            std::path::PathBuf::new(),
            file_size as u64,
            transfer_token.clone(),
        ) {
            Ok(checkpoint) => checkpoint,
            Err(error) => {
                publish_transfer_failure(&self.events, &transfer_id, error);
                return;
            }
        };
        checkpoint.source_path = Some(file_path.clone());
        checkpoint.sha256 = match sha256_file(&file_path) {
            Ok(sha256) => Some(sha256),
            Err(error) => {
                publish_transfer_failure(&self.events, &transfer_id, error);
                return;
            }
        };
        if let Err(error) = self.transfer_manager.register(checkpoint.clone()) {
            publish_transfer_failure(&self.events, &transfer_id, error);
            return;
        }
        if let Err(error) = self.transfer_store.save(&checkpoint) {
            publish_transfer_failure(&self.events, &transfer_id, error);
            return;
        }
        publish_transfer_state(&self.events, &checkpoint, None);

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
        packet.set("transferId", transfer_id.clone());
        packet.set("transferToken", transfer_token.clone());
        if let Some(sha256) = &checkpoint.sha256 {
            packet.set("sha256", sha256.clone());
        }
        packet.payload_size = Some(file_size);

        let mut info = serde_json::Map::new();
        info.insert("port".to_string(), serde_json::Value::from(port));
        packet.payload_transfer_info = Some(info);

        let mut sent = false;
        if let Some(binding) = self.sessions.current_binding(device_id) {
            if self.sessions.is_current(&binding) {
                if let Ok(mut stream) = binding.link.stream.lock() {
                    if let Ok(bytes) = packet.serialize_line() {
                        if stream.write_all(&bytes).is_ok() {
                            sent = true;
                        }
                    }
                }
            }
        }

        if !sent {
            publish_transfer_failure(
                &self.events,
                &transfer_id,
                format!("Failed to send share request packet to device {device_id}"),
            );
            return;
        }

        persist_transfer_state(
            &self.transfer_manager,
            &self.transfer_store,
            &self.events,
            &transfer_id,
            TransferState::Transferring,
            None,
        );

        eprintln!(
            "[Daemon] Share request sent. Listening for receiver on port {}...",
            port
        );

        let config_clone = Arc::clone(&self.config);
        let cancellations = Arc::clone(&self.transfer_cancellations);
        let transfer_manager = self.transfer_manager.clone();
        let transfer_store = self.transfer_store.clone();
        let events = self.events.clone();
        let transfer_device_id = device_id.to_string();
        thread::spawn(move || {
            listener.set_nonblocking(true).ok();
            let start = Instant::now();
            let socket = loop {
                if start.elapsed().as_secs() > 15 {
                    publish_transfer_failure(
                        &events,
                        &transfer_id,
                        format!("Timed out waiting for receiver on port {port}"),
                    );
                    return;
                }
                match listener.accept() {
                    Ok((s, _)) => break s,
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(e) => {
                        publish_transfer_failure(
                            &events,
                            &transfer_id,
                            format!("Failed to accept upload connection: {e}"),
                        );
                        return;
                    }
                }
            };

            socket.set_nonblocking(false).ok();
            let _ = socket.set_read_timeout(Some(Duration::from_secs(30)));
            let _ = socket.set_write_timeout(Some(Duration::from_secs(30)));

            eprintln!("[Daemon] Receiver connected. Establishing SSL server handshake...");

            let acceptor = match ssl_acceptor(&config_clone) {
                Ok(a) => a,
                Err(e) => {
                    publish_transfer_failure(
                        &events,
                        &transfer_id,
                        format!("Failed to create SSL acceptor: {e}"),
                    );
                    return;
                }
            };

            let mut ssl_stream = match acceptor.accept(socket) {
                Ok(s) => s,
                Err(e) => {
                    publish_transfer_failure(
                        &events,
                        &transfer_id,
                        format!("SSL handshake failed for upload: {e}"),
                    );
                    return;
                }
            };

            eprintln!("[Daemon] SSL handshake complete. Uploading file bytes...");

            let Some(peer_certificate) = ssl_stream.ssl().peer_certificate() else {
                publish_transfer_failure(
                    &events,
                    &transfer_id,
                    "Upload peer did not provide a certificate".to_string(),
                );
                return;
            };
            if let Err(error) = super::validation::validate_pinned_certificate(
                &config_clone,
                &transfer_device_id,
                &peer_certificate,
            ) {
                publish_transfer_failure(&events, &transfer_id, error);
                return;
            }
            if let Err(error) = super::validation::validate_certificate_device_id(
                &peer_certificate,
                &transfer_device_id,
            ) {
                publish_transfer_failure(&events, &transfer_id, error);
                return;
            }
            if let Err(error) = consume_transfer_token(&mut ssl_stream, &transfer_token) {
                publish_transfer_failure(&events, &transfer_id, error);
                return;
            }

            let mut file = match std::fs::File::open(&file_path) {
                Ok(f) => f,
                Err(e) => {
                    publish_transfer_failure(&events, &transfer_id, e.to_string());
                    return;
                }
            };

            let mut buffer = [0u8; 16384];
            let mut uploaded = 0;
            loop {
                if cancellations
                    .lock()
                    .map(|cancelled| cancelled.contains(&transfer_id))
                    .unwrap_or(false)
                {
                    eprintln!("[Daemon] File transfer {transfer_id} cancelled");
                    if let Ok(mut cancelled) = cancellations.lock() {
                        cancelled.remove(&transfer_id);
                    }
                    persist_transfer_state(
                        &transfer_manager,
                        &transfer_store,
                        &events,
                        &transfer_id,
                        TransferState::Cancelled,
                        None,
                    );
                    return;
                }
                match file.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(read) => {
                        if ssl_stream.write_all(&buffer[..read]).is_err() {
                            publish_transfer_failure(
                                &events,
                                &transfer_id,
                                "Error writing file bytes to stream".to_string(),
                            );
                            return;
                        }
                        uploaded += read;
                        if let Err(error) =
                            transfer_manager.update_offset(&transfer_id, uploaded as u64)
                        {
                            publish_transfer_failure(&events, &transfer_id, error);
                            return;
                        }
                        if let Some(checkpoint) = transfer_manager.get(&transfer_id) {
                            if let Err(error) = transfer_store.save(&checkpoint) {
                                publish_transfer_failure(&events, &transfer_id, error);
                                return;
                            }
                            publish_transfer_state(&events, &checkpoint, None);
                        }
                    }
                    Err(e) => {
                        publish_transfer_failure(&events, &transfer_id, e.to_string());
                        return;
                    }
                }
            }

            eprintln!(
                "[Daemon] Transfer {transfer_id} completed successfully! {} bytes sent.",
                uploaded
            );
            persist_transfer_state(
                &transfer_manager,
                &transfer_store,
                &events,
                &transfer_id,
                TransferState::Completed,
                None,
            );
            if let Ok(mut cancelled) = cancellations.lock() {
                cancelled.remove(&transfer_id);
            }
        });
    }
}

pub(super) struct ReceiveFileRequest {
    pub(super) device_id: String,
    pub(super) peer_ip: String,
    pub(super) port: u16,
    pub(super) size: i64,
    pub(super) filename: String,
    pub(super) transfer_token: String,
    pub(super) transfer_id: String,
    pub(super) expected_sha256: Option<String>,
}

pub(super) struct ReceiveFilePersistence {
    pub(super) config: Arc<Mutex<Config>>,
    pub(super) transfer_manager: TransferManager,
    pub(super) transfer_store: TransferCheckpointStore,
    pub(super) events: EventBus,
    pub(super) cancellations: Arc<Mutex<std::collections::HashSet<String>>>,
}

pub(super) fn receive_file_payload(
    request: ReceiveFileRequest,
    persistence: ReceiveFilePersistence,
) -> Result<(), String> {
    let ReceiveFileRequest {
        device_id,
        peer_ip,
        port,
        size,
        filename,
        transfer_token,
        transfer_id,
        expected_sha256,
    } = request;
    let ReceiveFilePersistence {
        config,
        transfer_manager,
        transfer_store,
        events,
        cancellations,
    } = persistence;
    let device_id = device_id.as_str();
    let peer_ip = peer_ip.as_str();
    let filename = filename.as_str();
    let transfer_token = transfer_token.as_str();
    let transfer_id = transfer_id.as_str();
    if size < 0 {
        return Err("Transfer size cannot be negative".to_string());
    }
    let size = u64::try_from(size).map_err(|_| "Transfer size is invalid".to_string())?;
    if size > MAX_TRANSFER_SIZE {
        return Err("Transfer exceeds the maximum supported size".to_string());
    }
    let filename = safe_filename(filename)?;
    eprintln!(
        "[Daemon] Connecting to payload transfer stream at {}:{}...",
        peer_ip, port
    );
    let endpoint: SocketAddr = format!("{}:{}", peer_ip, port)
        .parse()
        .map_err(|e| format!("Invalid transfer endpoint: {e}"))?;
    let socket = TcpStream::connect_timeout(&endpoint, Duration::from_secs(10))
        .map_err(|e| format!("Failed to connect to transfer socket: {}", e))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("Failed to configure transfer read timeout: {e}"))?;
    socket
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| format!("Failed to configure transfer write timeout: {e}"))?;

    let mut connector = SslConnector::builder(SslMethod::tls())
        .map_err(|e| format!("Failed to create SSL connector builder: {}", e))?;
    {
        let config = config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())?;
        connector
            .set_certificate(config.certificate())
            .map_err(|e| format!("Failed to configure transfer certificate: {e}"))?;
        connector
            .set_private_key(config.key())
            .map_err(|e| format!("Failed to configure transfer key: {e}"))?;
    }
    connector.set_verify_callback(SslVerifyMode::PEER, |preverify_ok, context| {
        preverify_ok || (context.error_depth() == 0 && context.error().as_raw() == 18)
    });

    let ssl = connector
        .build()
        .configure()
        .map_err(|e| format!("Failed to configure SSL: {}", e))?
        .into_ssl(device_id)
        .map_err(|e| format!("Failed to build SSL object: {}", e))?;

    let mut ssl_stream = ssl
        .connect(socket)
        .map_err(|e| format!("SSL handshake failed: {}", e))?;
    let _ = ssl_stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(30)));
    let _ = ssl_stream
        .get_ref()
        .set_write_timeout(Some(Duration::from_secs(30)));

    eprintln!("[Daemon] SSL handshake completed for file transfer.");

    let peer_certificate = ssl_stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| "Transfer peer did not provide a certificate".to_string())?;
    super::validation::validate_certificate_device_id(&peer_certificate, device_id)?;
    super::validation::validate_pinned_certificate(&config, device_id, &peer_certificate)?;
    consume_transfer_token(&mut ssl_stream, transfer_token)?;

    let download_dir = dirs::download_dir().unwrap_or_else(|| std::path::PathBuf::from("."));
    let mut target_path = download_dir.join(&filename);
    let existing_checkpoint = transfer_store.load(transfer_id)?;
    if let Some(checkpoint) = &existing_checkpoint {
        if checkpoint.device_id != device_id
            || checkpoint.filename != filename
            || checkpoint.total_size != size
        {
            return Err("Transfer checkpoint does not match the incoming request".to_string());
        }
        target_path = checkpoint.destination_path.clone();
    } else {
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
    }

    let temporary_path = existing_checkpoint
        .as_ref()
        .map(|checkpoint| checkpoint.temporary_path.clone())
        .unwrap_or_else(|| download_dir.join(format!(".desklink-{transfer_id}.part")));
    reject_symlink(&temporary_path)?;
    reject_symlink(&target_path)?;
    let mut checkpoint = existing_checkpoint.unwrap_or(TransferCheckpoint::new(
        transfer_id,
        device_id,
        filename.clone(),
        temporary_path.clone(),
        target_path.clone(),
        size,
        transfer_token,
    )?);
    if let (Some(expected), Some(stored)) =
        (expected_sha256.as_deref(), checkpoint.sha256.as_deref())
    {
        if expected != stored {
            return Err("Transfer checksum does not match the checkpoint".to_string());
        }
    }
    if expected_sha256.is_some() {
        checkpoint.sha256 = expected_sha256;
    }
    checkpoint.resume_token = transfer_token.to_string();
    checkpoint.state = TransferState::Transferring;
    checkpoint.validate()?;
    transfer_manager.register(checkpoint.clone())?;
    transfer_store.save(&checkpoint)?;
    publish_transfer_state(&events, &checkpoint, None);

    let mut file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&temporary_path)
        .map_err(|e| format!("Failed to open temporary file {:?}: {}", temporary_path, e))?;
    file.seek(SeekFrom::Start(checkpoint.offset))
        .map_err(|e| format!("Failed to seek transfer checkpoint: {e}"))?;

    let mut buffer = [0u8; 16384];
    let mut bytes_to_discard = checkpoint.offset;
    while bytes_to_discard > 0 {
        let to_read = std::cmp::min(bytes_to_discard, buffer.len() as u64) as usize;
        let read = ssl_stream
            .read(&mut buffer[..to_read])
            .map_err(|e| format!("Error skipping resumed transfer bytes: {e}"))?;
        if read == 0 {
            return Err("Unexpected EOF while resuming transfer".to_string());
        }
        bytes_to_discard -= read as u64;
    }

    let mut bytes_left = size.saturating_sub(checkpoint.offset);
    while bytes_left > 0 {
        if cancellations
            .lock()
            .map(|cancelled| cancelled.contains(transfer_id))
            .unwrap_or(false)
        {
            if let Ok(mut cancelled) = cancellations.lock() {
                cancelled.remove(transfer_id);
            }
            checkpoint.state = TransferState::Cancelled;
            transfer_manager.register(checkpoint.clone())?;
            transfer_store.save(&checkpoint)?;
            publish_transfer_state(&events, &checkpoint, None);
            return Ok(());
        }
        let to_read = std::cmp::min(bytes_left, buffer.len() as u64) as usize;
        let read = ssl_stream
            .read(&mut buffer[..to_read])
            .map_err(|e| format!("Error reading from transfer stream: {}", e))?;
        if read == 0 {
            return Err("Unexpected EOF from transfer stream".to_string());
        }
        file.write_all(&buffer[..read])
            .map_err(|e| format!("Failed to write to file: {}", e))?;
        bytes_left -= read as u64;
        let offset = size - bytes_left;
        transfer_manager.update_offset(transfer_id, offset)?;
        checkpoint = transfer_manager
            .get(transfer_id)
            .ok_or_else(|| "Transfer checkpoint disappeared".to_string())?;
        transfer_store.save(&checkpoint)?;
        publish_transfer_state(&events, &checkpoint, None);
    }

    file.sync_all()
        .map_err(|e| format!("Failed to flush downloaded file: {e}"))?;
    drop(file);
    if let Some(expected) = checkpoint.sha256.as_deref() {
        let actual = sha256_file(&temporary_path)?;
        if actual != expected {
            return Err("Downloaded file checksum does not match the sender".to_string());
        }
    }
    if let Err(error) = std::fs::rename(&temporary_path, &target_path) {
        let _ = std::fs::remove_file(&temporary_path);
        return Err(format!(
            "Failed to finalize downloaded file {:?}: {error}",
            target_path
        ));
    }
    transfer_manager.set_state(transfer_id, TransferState::Completed)?;
    checkpoint = transfer_manager
        .get(transfer_id)
        .ok_or_else(|| "Transfer checkpoint disappeared after completion".to_string())?;
    transfer_store.save(&checkpoint)?;
    publish_transfer_state(&events, &checkpoint, None);
    eprintln!("[Daemon] Saved downloaded file to: {:?}", target_path);
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    let mut hasher = Hasher::new(MessageDigest::sha256()).map_err(|error| error.to_string())?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| error.to_string())?;
        if read == 0 {
            break;
        }
        hasher
            .update(&buffer[..read])
            .map_err(|error| error.to_string())?;
    }
    let digest = hasher.finish().map_err(|error| error.to_string())?;
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn safe_filename(filename: &str) -> Result<String, String> {
    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.contains('/')
        || filename.contains('\\')
        || Path::new(filename).components().count() != 1
    {
        return Err("Transfer filename must be a single safe path component".to_string());
    }
    let component = Path::new(filename)
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| "Transfer filename is not valid UTF-8".to_string())?;
    if component != filename {
        return Err("Transfer filename is not a normal path component".to_string());
    }
    Ok(filename.to_string())
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "Refusing to use symlink in transfer path: {:?}",
            path
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!(
            "Could not inspect transfer path {:?}: {error}",
            path
        )),
    }
}

fn publish_transfer_state(
    events: &EventBus,
    checkpoint: &TransferCheckpoint,
    error: Option<String>,
) {
    events.publish(CoreEvent::TransferChanged {
        transfer_id: checkpoint.transfer_id.clone(),
        state: format!("{:?}", checkpoint.state).to_lowercase(),
        bytes_done: checkpoint.offset,
        bytes_total: checkpoint.total_size,
        can_resume: checkpoint.offset > 0
            && !matches!(
                checkpoint.state,
                TransferState::Completed | TransferState::Cancelled
            ),
        error,
    });
}

fn publish_transfer_failure(events: &EventBus, transfer_id: &str, error: String) {
    events.publish(CoreEvent::TransferChanged {
        transfer_id: transfer_id.to_string(),
        state: "failed".to_string(),
        bytes_done: 0,
        bytes_total: 0,
        can_resume: false,
        error: Some(error.clone()),
    });
    events.publish(CoreEvent::Error {
        scope: "transfer".to_string(),
        device_id: None,
        message: error,
        retryable: true,
    });
}

fn persist_transfer_state(
    manager: &TransferManager,
    store: &TransferCheckpointStore,
    events: &EventBus,
    transfer_id: &str,
    state: TransferState,
    error: Option<String>,
) {
    if manager.set_state(transfer_id, state).is_err() {
        publish_transfer_failure(
            events,
            transfer_id,
            "Transfer state is no longer available".to_string(),
        );
        return;
    }
    let Some(checkpoint) = manager.get(transfer_id) else {
        publish_transfer_failure(
            events,
            transfer_id,
            "Transfer checkpoint is no longer available".to_string(),
        );
        return;
    };
    if let Err(store_error) = store.save(&checkpoint) {
        publish_transfer_failure(events, transfer_id, store_error);
        return;
    }
    publish_transfer_state(events, &checkpoint, error);
}

pub(super) fn consume_transfer_token(
    stream: &mut openssl::ssl::SslStream<TcpStream>,
    expected: &str,
) -> Result<(), String> {
    if expected.is_empty() || expected.len() > 128 {
        return Err("Transfer token is invalid".to_string());
    }
    let mut token = Vec::with_capacity(expected.len() + 1);
    let mut byte = [0u8; 1];
    loop {
        stream
            .read(&mut byte)
            .map_err(|error| format!("Failed to read transfer token: {error}"))?;
        if byte[0] == b'\n' {
            break;
        }
        token.push(byte[0]);
        if token.len() > 128 {
            return Err("Transfer token is too long".to_string());
        }
    }
    let token = String::from_utf8(token).map_err(|_| "Transfer token is not UTF-8".to_string())?;
    if token == expected {
        Ok(())
    } else {
        Err("Transfer token does not match the control packet".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::safe_filename;

    #[test]
    fn rejects_path_traversal_filenames() {
        for value in ["../secret", "/tmp/file", "nested/file", "..", ""] {
            assert!(safe_filename(value).is_err(), "{value} must be rejected");
        }
    }

    #[test]
    fn accepts_a_single_normal_filename() {
        assert_eq!(safe_filename("photo.jpg").unwrap(), "photo.jpg");
    }
}
