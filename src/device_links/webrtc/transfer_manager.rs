//! Checkpointed receiver for the DeskLink WebRTC file protocol.
//!
//! The manager owns only local filesystem state.  Peer and data-channel code
//! supplies validated wire records and sends the returned control messages.
//! This separation keeps untrusted network input away from filesystem paths
//! and makes restart/resume behaviour testable without GStreamer.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use openssl::hash::{hash, MessageDigest};
use serde::{Deserialize, Serialize};

use super::file_protocol::{
    FileAction, FileChunk, FileControl, FILE_PROTOCOL_VERSION, MAX_FILE_CHUNK_BYTES,
    MAX_TRANSFER_BYTES,
};
use super::WebRtcWireBinding;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransferEvent {
    SendControl(FileControl),
    SendChunk(Vec<u8>),
    Completed { transfer_id: String, path: PathBuf },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReceiveCheckpoint {
    transfer_id: String,
    device_id: String,
    session_id: u64,
    generation: u64,
    token: String,
    filename: String,
    total_size: u64,
    sha256: String,
    offset: u64,
    part_name: String,
    destination_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SendCheckpoint {
    transfer_id: String,
    device_id: String,
    session_id: u64,
    generation: u64,
    token: String,
    filename: String,
    total_size: u64,
    sha256: String,
    offset: u64,
    source_name: String,
    awaiting_offset: Option<u64>,
}

impl SendCheckpoint {
    fn wire_matches(&self, wire: &WebRtcWireBinding) -> bool {
        self.device_id == wire.peer_device_id
            && self.session_id == wire.session_id
            && self.generation == wire.generation
    }
}

impl ReceiveCheckpoint {
    fn wire_matches(&self, wire: &WebRtcWireBinding) -> bool {
        self.device_id == wire.peer_device_id
            && self.session_id == wire.session_id
            && self.generation == wire.generation
    }
}

/// A restart-safe, bounded receiver for Android-compatible file offers.
pub struct WebRtcTransferManager {
    state_root: PathBuf,
    destination_root: PathBuf,
    active: Mutex<HashMap<String, ReceiveCheckpoint>>,
    outgoing: Mutex<HashMap<String, SendCheckpoint>>,
}

impl WebRtcTransferManager {
    pub fn new(state_root: PathBuf, destination_root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&state_root).map_err(|error| {
            format!("Could not create DeskLink transfer state directory: {error}")
        })?;
        fs::create_dir_all(&destination_root)
            .map_err(|error| format!("Could not create DeskLink download directory: {error}"))?;
        ensure_directory_not_symlink(&state_root)?;
        ensure_directory_not_symlink(&destination_root)?;
        Ok(Self {
            state_root,
            destination_root,
            active: Mutex::new(HashMap::new()),
            outgoing: Mutex::new(HashMap::new()),
        })
    }

    /// Copies a source file into DeskLink-owned resumable state, hashes it,
    /// and produces the first offer. The source remains untouched and an
    /// interrupted send can safely resume only from the copied bytes.
    pub fn start_send(
        &self,
        wire: &WebRtcWireBinding,
        source: &Path,
    ) -> Result<Vec<TransferEvent>, String> {
        let filename = source
            .file_name()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "DeskLink shared file has no safe filename".to_string())?
            .to_string();
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let token = uuid::Uuid::new_v4().to_string();
        let source_name = format!(".desklink-{transfer_id}.send");
        let snapshot = self.checked_state_path(&source_name)?;
        let mut input = File::open(source)
            .map_err(|error| format!("Could not open DeskLink shared file: {error}"))?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&snapshot)
            .map_err(|error| format!("Could not create DeskLink transfer snapshot: {error}"))?;
        let mut bytes = 0u64;
        let mut hasher = openssl::sha::Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let count = std::io::Read::read(&mut input, &mut buffer)
                .map_err(|error| format!("Could not read DeskLink shared file: {error}"))?;
            if count == 0 {
                break;
            }
            bytes = bytes
                .checked_add(u64::try_from(count).map_err(|_| "Invalid file size".to_string())?)
                .ok_or_else(|| "DeskLink shared file is too large".to_string())?;
            if bytes > MAX_TRANSFER_BYTES {
                let _ = fs::remove_file(&snapshot);
                return Err("DeskLink shared file exceeds the 4 GiB transfer limit".to_string());
            }
            hasher.update(&buffer[..count]);
            output
                .write_all(&buffer[..count])
                .map_err(|error| format!("Could not snapshot DeskLink shared file: {error}"))?;
        }
        output
            .sync_all()
            .map_err(|error| format!("Could not persist DeskLink shared file snapshot: {error}"))?;
        let checkpoint = SendCheckpoint {
            transfer_id: transfer_id.clone(),
            device_id: wire.peer_device_id.clone(),
            session_id: wire.session_id,
            generation: wire.generation,
            token,
            filename,
            total_size: bytes,
            sha256: hasher
                .finish()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            offset: 0,
            source_name,
            awaiting_offset: None,
        };
        self.save_send_checkpoint(&checkpoint)?;
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(transfer_id, checkpoint.clone());
        Ok(vec![TransferEvent::SendControl(self.send_control(
            wire,
            FileAction::Offer,
            &checkpoint,
            0,
        ))])
    }

    /// Processes an authenticated file-control envelope.  Returned events
    /// must be sent only through the current session generation.
    pub fn handle_control(
        &self,
        wire: &WebRtcWireBinding,
        message: FileControl,
    ) -> Result<Vec<TransferEvent>, String> {
        message.validate(wire)?;
        match message.action {
            FileAction::Offer => self.receive_offer(wire, message),
            FileAction::Accept => self.accept_send(wire, &message),
            FileAction::Acknowledge => self.acknowledge_send(wire, &message),
            FileAction::Complete => self.complete_send(wire, &message),
            FileAction::Cancel => {
                self.cancel(&message.transfer_id, &message.transfer_token)?;
                Ok(Vec::new())
            }
            FileAction::Error => {
                // Preserve a verified partial file and checkpoint for an
                // explicit retry/resume, but remove it from this live peer.
                self.active
                    .lock()
                    .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
                    .remove(&message.transfer_id);
                Ok(Vec::new())
            }
        }
    }

    /// Writes exactly one verified file-data chunk and returns an ACK.  A
    /// final chunk is hashed before the temporary file is atomically renamed.
    pub fn handle_chunk(
        &self,
        wire: &WebRtcWireBinding,
        encoded: &[u8],
    ) -> Result<Vec<TransferEvent>, String> {
        let chunk = FileChunk::decode(encoded)?;
        let checkpoint = self
            .active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(&chunk.transfer_id)
            .cloned()
            .or_else(|| self.load_checkpoint(&chunk.transfer_id).ok().flatten())
            .ok_or_else(|| "DeskLink WebRTC file chunk has no accepted offer".to_string())?;
        if !checkpoint.wire_matches(wire) {
            return Err("DeskLink WebRTC file chunk has a stale session binding".to_string());
        }
        if checkpoint.token != chunk.transfer_token {
            return Err("DeskLink WebRTC file chunk has the wrong transfer token".to_string());
        }
        if checkpoint.offset != chunk.offset {
            return Err("DeskLink WebRTC file chunk has the wrong offset".to_string());
        }
        let next_offset = chunk
            .offset
            .checked_add(
                u64::try_from(chunk.data.len()).map_err(|_| "Invalid chunk size".to_string())?,
            )
            .ok_or_else(|| "DeskLink WebRTC transfer offset overflow".to_string())?;
        if next_offset > checkpoint.total_size || chunk.data.len() > MAX_FILE_CHUNK_BYTES {
            return Err(
                "DeskLink WebRTC file chunk exceeds the announced transfer size".to_string(),
            );
        }

        let part = self.part_path(&checkpoint)?;
        ensure_regular_file(&part)?;
        let actual_size = fs::metadata(&part)
            .map_err(|error| format!("Could not inspect DeskLink partial file: {error}"))?
            .len();
        if actual_size != checkpoint.offset {
            return Err("DeskLink partial file does not match its checkpoint".to_string());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .open(&part)
            .map_err(|error| format!("Could not open DeskLink partial file: {error}"))?;
        file.seek(SeekFrom::Start(chunk.offset))
            .map_err(|error| format!("Could not seek DeskLink partial file: {error}"))?;
        file.write_all(&chunk.data)
            .map_err(|error| format!("Could not write DeskLink partial file: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not persist DeskLink file chunk: {error}"))?;

        let updated = ReceiveCheckpoint {
            offset: next_offset,
            ..checkpoint
        };
        self.save_checkpoint(&updated)?;
        self.active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(updated.transfer_id.clone(), updated.clone());

        let mut events = vec![TransferEvent::SendControl(self.control(
            wire,
            FileAction::Acknowledge,
            &updated,
            next_offset,
            None,
        ))];
        if next_offset == updated.total_size {
            let completed = self.complete_receiver(wire, &updated)?;
            events.push(TransferEvent::SendControl(self.control(
                wire,
                FileAction::Complete,
                &updated,
                next_offset,
                None,
            )));
            events.push(TransferEvent::Completed {
                transfer_id: updated.transfer_id.clone(),
                path: completed,
            });
        }
        Ok(events)
    }

    fn receive_offer(
        &self,
        wire: &WebRtcWireBinding,
        message: FileControl,
    ) -> Result<Vec<TransferEvent>, String> {
        let filename = message
            .filename
            .clone()
            .ok_or_else(|| "DeskLink WebRTC file offer has no filename".to_string())?;
        let total_size = message
            .total_size
            .ok_or_else(|| "DeskLink WebRTC file offer has no size".to_string())?;
        let sha256 = message
            .sha256
            .clone()
            .ok_or_else(|| "DeskLink WebRTC file offer has no checksum".to_string())?;
        let checkpoint = match self.load_checkpoint(&message.transfer_id)? {
            Some(existing) => {
                if !existing.wire_matches(wire)
                    || existing.token != message.transfer_token
                    || existing.filename != filename
                    || existing.total_size != total_size
                    || !existing.sha256.eq_ignore_ascii_case(&sha256)
                {
                    return Err(
                        "DeskLink incoming file offer does not match its checkpoint".to_string()
                    );
                }
                let part = self.part_path(&existing)?;
                ensure_regular_file(&part)?;
                if fs::metadata(&part)
                    .map_err(|error| error.to_string())?
                    .len()
                    != existing.offset
                {
                    return Err(
                        "DeskLink incoming partial file does not match its checkpoint".to_string(),
                    );
                }
                existing
            }
            None => {
                let part_name = format!(".desklink-{}.part", message.transfer_id);
                let part = self.checked_state_path(&part_name)?;
                let file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&part)
                    .map_err(|error| format!("Could not create DeskLink partial file: {error}"))?;
                file.sync_all()
                    .map_err(|error| format!("Could not persist DeskLink partial file: {error}"))?;
                ReceiveCheckpoint {
                    transfer_id: message.transfer_id.clone(),
                    device_id: wire.peer_device_id.clone(),
                    session_id: wire.session_id,
                    generation: wire.generation,
                    token: message.transfer_token.clone(),
                    filename,
                    total_size,
                    sha256,
                    offset: 0,
                    part_name,
                    destination_name: self.available_destination_name(
                        message.filename.as_deref().unwrap_or_default(),
                    )?,
                }
            }
        };
        self.save_checkpoint(&checkpoint)?;
        self.active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint.clone());
        Ok(vec![TransferEvent::SendControl(self.control(
            wire,
            FileAction::Accept,
            &checkpoint,
            checkpoint.offset,
            None,
        ))])
    }

    fn complete_receiver(
        &self,
        _wire: &WebRtcWireBinding,
        checkpoint: &ReceiveCheckpoint,
    ) -> Result<PathBuf, String> {
        let part = self.part_path(checkpoint)?;
        ensure_regular_file(&part)?;
        if fs::metadata(&part)
            .map_err(|error| error.to_string())?
            .len()
            != checkpoint.total_size
        {
            return Err("DeskLink completed file has the wrong size".to_string());
        }
        let digest = sha256_file(&part)?;
        if !digest.eq_ignore_ascii_case(&checkpoint.sha256) {
            return Err("DeskLink completed file checksum does not match the sender".to_string());
        }
        let target = self.checked_destination_path(&checkpoint.destination_name)?;
        if target.exists() {
            return Err("DeskLink final download target already exists".to_string());
        }
        fs::rename(&part, &target)
            .map_err(|error| format!("Could not atomically publish DeskLink download: {error}"))?;
        sync_directory(&self.destination_root)?;
        self.remove_checkpoint(&checkpoint.transfer_id)?;
        self.active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .remove(&checkpoint.transfer_id);
        Ok(target)
    }

    fn cancel(&self, transfer_id: &str, token: &str) -> Result<(), String> {
        let checkpoint = self
            .active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .remove(transfer_id)
            .or_else(|| self.load_checkpoint(transfer_id).ok().flatten());
        if let Some(checkpoint) = checkpoint {
            if checkpoint.token != token {
                return Err("DeskLink file cancellation has the wrong transfer token".to_string());
            }
            let part = self.part_path(&checkpoint)?;
            if part.exists() {
                ensure_regular_file(&part)?;
                fs::remove_file(&part).map_err(|error| {
                    format!("Could not remove cancelled DeskLink partial file: {error}")
                })?;
            }
            self.remove_checkpoint(transfer_id)?;
        }
        if let Some(checkpoint) = self
            .outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .remove(transfer_id)
        {
            if checkpoint.token != token {
                return Err("DeskLink file cancellation has the wrong transfer token".to_string());
            }
            let source = self.checked_state_path(&checkpoint.source_name)?;
            if source.exists() {
                ensure_regular_file(&source)?;
                fs::remove_file(source).map_err(|error| error.to_string())?;
            }
            self.remove_send_checkpoint(transfer_id)?;
        }
        Ok(())
    }

    fn accept_send(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileControl,
    ) -> Result<Vec<TransferEvent>, String> {
        let mut checkpoint = self.take_sender(wire, message)?;
        if message.offset > checkpoint.total_size {
            return Err("DeskLink file resume offset exceeds the shared file".to_string());
        }
        checkpoint.offset = message.offset;
        checkpoint.awaiting_offset = None;
        self.save_send_checkpoint(&checkpoint)?;
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint.clone());
        self.next_send_chunk(&checkpoint)
    }

    fn acknowledge_send(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileControl,
    ) -> Result<Vec<TransferEvent>, String> {
        let mut checkpoint = self.take_sender(wire, message)?;
        if checkpoint.awaiting_offset != Some(message.offset) {
            return Err("DeskLink file acknowledgement does not match the sent chunk".to_string());
        }
        checkpoint.offset = message.offset;
        checkpoint.awaiting_offset = None;
        self.save_send_checkpoint(&checkpoint)?;
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint.clone());
        self.next_send_chunk(&checkpoint)
    }

    fn complete_send(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileControl,
    ) -> Result<Vec<TransferEvent>, String> {
        let checkpoint = self.take_sender(wire, message)?;
        if message.offset != checkpoint.total_size
            || message.sha256.as_deref() != Some(checkpoint.sha256.as_str())
        {
            return Err("DeskLink file completion does not match the shared file".to_string());
        }
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .remove(&checkpoint.transfer_id);
        let source = self.checked_state_path(&checkpoint.source_name)?;
        if source.exists() {
            ensure_regular_file(&source)?;
            fs::remove_file(source).map_err(|error| error.to_string())?;
        }
        self.remove_send_checkpoint(&checkpoint.transfer_id)?;
        Ok(vec![TransferEvent::Completed {
            transfer_id: checkpoint.transfer_id,
            path: PathBuf::from(checkpoint.filename),
        }])
    }

    fn take_sender(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileControl,
    ) -> Result<SendCheckpoint, String> {
        let checkpoint = self
            .outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(&message.transfer_id)
            .cloned()
            .or_else(|| {
                self.load_send_checkpoint(&message.transfer_id)
                    .ok()
                    .flatten()
            })
            .ok_or_else(|| "DeskLink file response has no active sender".to_string())?;
        if !checkpoint.wire_matches(wire) || checkpoint.token != message.transfer_token {
            return Err("DeskLink file response has a stale binding or wrong token".to_string());
        }
        Ok(checkpoint)
    }

    fn next_send_chunk(&self, checkpoint: &SendCheckpoint) -> Result<Vec<TransferEvent>, String> {
        if checkpoint.offset == checkpoint.total_size {
            return Ok(Vec::new());
        }
        let source = self.checked_state_path(&checkpoint.source_name)?;
        ensure_regular_file(&source)?;
        if fs::metadata(&source)
            .map_err(|error| error.to_string())?
            .len()
            != checkpoint.total_size
        {
            return Err("DeskLink shared file snapshot changed during transfer".to_string());
        }
        let count = usize::try_from(
            (checkpoint.total_size - checkpoint.offset).min(MAX_FILE_CHUNK_BYTES as u64),
        )
        .map_err(|_| "Invalid DeskLink file chunk size".to_string())?;
        let mut data = vec![0; count];
        let mut source_file = File::open(&source).map_err(|error| error.to_string())?;
        source_file
            .seek(SeekFrom::Start(checkpoint.offset))
            .map_err(|error| error.to_string())?;
        std::io::Read::read_exact(&mut source_file, &mut data)
            .map_err(|error| error.to_string())?;
        let encoded = FileChunk {
            transfer_id: checkpoint.transfer_id.clone(),
            transfer_token: checkpoint.token.clone(),
            offset: checkpoint.offset,
            data,
        }
        .encode()?;
        let mut updated = checkpoint.clone();
        updated.awaiting_offset = Some(
            checkpoint.offset
                + u64::try_from(count).map_err(|_| "Invalid chunk size".to_string())?,
        );
        self.save_send_checkpoint(&updated)?;
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(updated.transfer_id.clone(), updated);
        Ok(vec![TransferEvent::SendChunk(encoded)])
    }

    fn send_control(
        &self,
        wire: &WebRtcWireBinding,
        action: FileAction,
        checkpoint: &SendCheckpoint,
        offset: u64,
    ) -> FileControl {
        FileControl {
            protocol_version: FILE_PROTOCOL_VERSION,
            action,
            transfer_id: checkpoint.transfer_id.clone(),
            device_id: wire.sender_device_id.clone(),
            session_id: wire.session_id,
            connection_generation: wire.generation,
            transfer_token: checkpoint.token.clone(),
            filename: Some(checkpoint.filename.clone()),
            total_size: Some(checkpoint.total_size),
            sha256: Some(checkpoint.sha256.clone()),
            offset,
            chunk_size: MAX_FILE_CHUNK_BYTES,
            error: None,
        }
    }

    fn control(
        &self,
        wire: &WebRtcWireBinding,
        action: FileAction,
        checkpoint: &ReceiveCheckpoint,
        offset: u64,
        error: Option<String>,
    ) -> FileControl {
        FileControl {
            protocol_version: FILE_PROTOCOL_VERSION,
            action,
            transfer_id: checkpoint.transfer_id.clone(),
            device_id: wire.sender_device_id.clone(),
            session_id: wire.session_id,
            connection_generation: wire.generation,
            transfer_token: checkpoint.token.clone(),
            filename: None,
            total_size: Some(checkpoint.total_size),
            sha256: Some(checkpoint.sha256.clone()),
            offset,
            chunk_size: MAX_FILE_CHUNK_BYTES,
            error,
        }
    }

    fn checked_state_path(&self, name: &str) -> Result<PathBuf, String> {
        checked_child(&self.state_root, name)
    }

    fn checked_destination_path(&self, name: &str) -> Result<PathBuf, String> {
        checked_child(&self.destination_root, name)
    }

    fn part_path(&self, checkpoint: &ReceiveCheckpoint) -> Result<PathBuf, String> {
        self.checked_state_path(&checkpoint.part_name)
    }

    fn checkpoint_path(&self, transfer_id: &str) -> Result<PathBuf, String> {
        self.checked_state_path(&format!(".desklink-{transfer_id}.json"))
    }

    fn load_checkpoint(&self, transfer_id: &str) -> Result<Option<ReceiveCheckpoint>, String> {
        let path = self.checkpoint_path(transfer_id)?;
        if !path.exists() {
            return Ok(None);
        }
        ensure_regular_file(&path)?;
        let value = fs::read(&path)
            .map_err(|error| format!("Could not read DeskLink transfer checkpoint: {error}"))?;
        serde_json::from_slice(&value)
            .map(Some)
            .map_err(|error| format!("Malformed DeskLink transfer checkpoint: {error}"))
    }

    fn save_checkpoint(&self, checkpoint: &ReceiveCheckpoint) -> Result<(), String> {
        let path = self.checkpoint_path(&checkpoint.transfer_id)?;
        let temporary =
            self.checked_state_path(&format!(".desklink-{}.json.tmp", checkpoint.transfer_id))?;
        let bytes = serde_json::to_vec(checkpoint).map_err(|error| {
            format!("Could not serialize DeskLink transfer checkpoint: {error}")
        })?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|error| format!("Could not write DeskLink transfer checkpoint: {error}"))?;
        file.write_all(&bytes)
            .map_err(|error| format!("Could not write DeskLink transfer checkpoint: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("Could not persist DeskLink transfer checkpoint: {error}"))?;
        fs::rename(temporary, path).map_err(|error| {
            format!("Could not atomically update DeskLink transfer checkpoint: {error}")
        })?;
        sync_directory(&self.state_root)
    }

    fn remove_checkpoint(&self, transfer_id: &str) -> Result<(), String> {
        let path = self.checkpoint_path(transfer_id)?;
        if path.exists() {
            ensure_regular_file(&path)?;
            fs::remove_file(path).map_err(|error| {
                format!("Could not remove DeskLink transfer checkpoint: {error}")
            })?;
            sync_directory(&self.state_root)?;
        }
        Ok(())
    }

    fn send_checkpoint_path(&self, transfer_id: &str) -> Result<PathBuf, String> {
        self.checked_state_path(&format!(".desklink-{transfer_id}.send.json"))
    }

    fn save_send_checkpoint(&self, checkpoint: &SendCheckpoint) -> Result<(), String> {
        let path = self.send_checkpoint_path(&checkpoint.transfer_id)?;
        let temporary = self.checked_state_path(&format!(
            ".desklink-{}.send.json.tmp",
            checkpoint.transfer_id
        ))?;
        let bytes = serde_json::to_vec(checkpoint).map_err(|error| error.to_string())?;
        let mut file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&temporary)
            .map_err(|error| error.to_string())?;
        file.write_all(&bytes).map_err(|error| error.to_string())?;
        file.sync_all().map_err(|error| error.to_string())?;
        fs::rename(temporary, path).map_err(|error| error.to_string())?;
        sync_directory(&self.state_root)
    }

    fn load_send_checkpoint(&self, transfer_id: &str) -> Result<Option<SendCheckpoint>, String> {
        let path = self.send_checkpoint_path(transfer_id)?;
        if !path.exists() {
            return Ok(None);
        }
        ensure_regular_file(&path)?;
        serde_json::from_slice(&fs::read(path).map_err(|error| error.to_string())?)
            .map(Some)
            .map_err(|error| error.to_string())
    }

    fn remove_send_checkpoint(&self, transfer_id: &str) -> Result<(), String> {
        let path = self.send_checkpoint_path(transfer_id)?;
        if path.exists() {
            ensure_regular_file(&path)?;
            fs::remove_file(path).map_err(|error| error.to_string())?;
            sync_directory(&self.state_root)?;
        }
        Ok(())
    }

    fn available_destination_name(&self, filename: &str) -> Result<String, String> {
        let candidate = self.checked_destination_path(filename)?;
        if !candidate.exists() {
            return Ok(filename.to_string());
        }
        let source = Path::new(filename);
        let stem = source
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("file");
        let extension = source.extension().and_then(|value| value.to_str());
        for number in 1..10_000u32 {
            let name = match extension {
                Some(extension) if !extension.is_empty() => format!("{stem}_{number}.{extension}"),
                _ => format!("{stem}_{number}"),
            };
            if !self.checked_destination_path(&name)?.exists() {
                return Ok(name);
            }
        }
        Err("Could not allocate a collision-safe DeskLink download name".to_string())
    }
}

fn checked_child(root: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || Path::new(name).components().count() != 1
        || name.contains(['/', '\\', '\0'])
    {
        return Err("Unsafe DeskLink transfer path".to_string());
    }
    let path = root.join(name);
    if path.parent() != Some(root) {
        return Err("DeskLink transfer path escapes its root".to_string());
    }
    Ok(path)
}

fn ensure_directory_not_symlink(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect DeskLink transfer directory: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err("DeskLink transfer directory is not a safe directory".to_string());
    }
    Ok(())
}

fn ensure_regular_file(path: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("Could not inspect DeskLink transfer file: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err("DeskLink transfer path is not a regular file".to_string());
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not persist DeskLink transfer directory: {error}"))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let bytes = fs::read(path).map_err(|error| format!("Could not hash DeskLink file: {error}"))?;
    let digest = hash(MessageDigest::sha256(), &bytes)
        .map_err(|error| format!("Could not hash DeskLink file: {error}"))?;
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn root(label: &str) -> PathBuf {
        let value = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("desklink-{label}-{value}"))
    }

    fn wire() -> WebRtcWireBinding {
        WebRtcWireBinding {
            sender_device_id: "desktop".to_string(),
            peer_device_id: "phone".to_string(),
            session_id: 4,
            generation: 2,
        }
    }

    fn offer() -> FileControl {
        FileControl {
            protocol_version: FILE_PROTOCOL_VERSION,
            action: FileAction::Offer,
            transfer_id: "transfer_1".to_string(),
            device_id: "phone".to_string(),
            session_id: 4,
            connection_generation: 2,
            transfer_token: "token".to_string(),
            filename: Some("report.txt".to_string()),
            total_size: Some(3),
            sha256: Some(
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad".to_string(),
            ),
            offset: 0,
            chunk_size: MAX_FILE_CHUNK_BYTES,
            error: None,
        }
    }

    #[test]
    fn accepts_chunks_then_atomically_publishes_a_verified_download() {
        let state = root("state");
        let downloads = root("downloads");
        let manager = WebRtcTransferManager::new(state.clone(), downloads.clone()).unwrap();
        let events = manager.handle_control(&wire(), offer()).unwrap();
        assert!(
            matches!(events.as_slice(), [TransferEvent::SendControl(control)] if control.action == FileAction::Accept)
        );

        let chunk = FileChunk {
            transfer_id: "transfer_1".to_string(),
            transfer_token: "token".to_string(),
            offset: 0,
            data: b"abc".to_vec(),
        };
        let events = manager
            .handle_chunk(&wire(), &chunk.encode().unwrap())
            .unwrap();
        assert!(
            matches!(events.as_slice(), [TransferEvent::SendControl(ack), TransferEvent::SendControl(done), TransferEvent::Completed { path, .. }] if ack.action == FileAction::Acknowledge && done.action == FileAction::Complete && path.exists())
        );
        assert_eq!(fs::read(downloads.join("report.txt")).unwrap(), b"abc");
        assert!(!state.join(".desklink-transfer_1.part").exists());
        assert!(!state.join(".desklink-transfer_1.json").exists());
        let _ = fs::remove_dir_all(state);
        let _ = fs::remove_dir_all(downloads);
    }

    #[test]
    fn rejects_wrong_offset_without_publishing_a_partial_file() {
        let state = root("state");
        let downloads = root("downloads");
        let manager = WebRtcTransferManager::new(state.clone(), downloads.clone()).unwrap();
        manager.handle_control(&wire(), offer()).unwrap();
        let chunk = FileChunk {
            transfer_id: "transfer_1".to_string(),
            transfer_token: "token".to_string(),
            offset: 1,
            data: b"abc".to_vec(),
        };
        assert!(manager
            .handle_chunk(&wire(), &chunk.encode().unwrap())
            .is_err());
        assert!(!downloads.join("report.txt").exists());
        let _ = fs::remove_dir_all(state);
        let _ = fs::remove_dir_all(downloads);
    }

    #[test]
    fn sender_uses_an_offer_then_acknowledged_bounded_chunks() {
        let state = root("send-state");
        let downloads = root("send-downloads");
        fs::create_dir_all(&state).unwrap();
        let source = state.join("source.txt");
        fs::write(&source, b"abc").unwrap();
        let manager = WebRtcTransferManager::new(state.clone(), downloads.clone()).unwrap();
        let events = manager.start_send(&wire(), &source).unwrap();
        let offer = match events.as_slice() {
            [TransferEvent::SendControl(offer)] => offer.clone(),
            _ => panic!("expected one file offer"),
        };
        assert_eq!(offer.action, FileAction::Offer);

        let mut accept = offer.clone();
        accept.action = FileAction::Accept;
        accept.device_id = "phone".to_string();
        accept.filename = None;
        let events = manager.handle_control(&wire(), accept).unwrap();
        let chunk = match events.as_slice() {
            [TransferEvent::SendChunk(chunk)] => FileChunk::decode(chunk).unwrap(),
            _ => panic!("expected one file chunk"),
        };
        assert_eq!(chunk.data, b"abc");

        let mut acknowledgement = offer;
        acknowledgement.action = FileAction::Acknowledge;
        acknowledgement.device_id = "phone".to_string();
        acknowledgement.filename = None;
        acknowledgement.offset = 3;
        assert!(manager
            .handle_control(&wire(), acknowledgement)
            .unwrap()
            .is_empty());
        let _ = fs::remove_dir_all(state);
        let _ = fs::remove_dir_all(downloads);
    }
}
