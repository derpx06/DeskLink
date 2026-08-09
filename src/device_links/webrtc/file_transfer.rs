//! Restart-safe file transfer over the authenticated DeskLink WebRTC channels.

use std::collections::{HashMap, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use openssl::hash::{Hasher, MessageDigest};
use uuid::Uuid;

use super::file_protocol::{
    FileChunk, FileTransferAction, FileTransferControl, FILE_PROTOCOL_VERSION, MAX_FILE_CHUNK_BYTES,
};
use super::wire_binding::WebRtcWireBinding;
use crate::device_links::core::events::{CoreEvent, EventBus};
use crate::device_links::core::transfer_manager::{
    TransferCheckpoint, TransferCheckpointStore, TransferManager, TransferState, MAX_TRANSFER_SIZE,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundFileMessage {
    Control(FileTransferControl),
    Chunk(Vec<u8>),
}

#[derive(Clone)]
struct TransferPersistence {
    manager: TransferManager,
    store: TransferCheckpointStore,
    cancellations: Arc<Mutex<HashSet<String>>>,
    download_root: PathBuf,
}

#[derive(Clone)]
enum ActiveTransfer {
    Sending {
        wire: WebRtcWireBinding,
        awaiting_offset: Option<u64>,
    },
    Receiving {
        wire: WebRtcWireBinding,
    },
}

/// Owns active WebRTC transfer state. Protocol callbacks return outbound
/// messages so the coordinator remains the sole data-channel writer.
#[derive(Clone, Default)]
pub struct WebRtcFileTransferManager {
    persistence: Arc<Mutex<Option<TransferPersistence>>>,
    active: Arc<Mutex<HashMap<String, ActiveTransfer>>>,
}

impl WebRtcFileTransferManager {
    pub fn configure(
        &self,
        manager: TransferManager,
        store: TransferCheckpointStore,
        cancellations: Arc<Mutex<HashSet<String>>>,
        download_root: PathBuf,
    ) -> Result<(), String> {
        fs::create_dir_all(&download_root).map_err(|error| {
            format!("Could not create DeskLink download directory {download_root:?}: {error}")
        })?;
        reject_symlink(&download_root)?;
        *self
            .persistence
            .lock()
            .map_err(|_| "WebRTC transfer persistence lock poisoned".to_string())? =
            Some(TransferPersistence {
                manager,
                store,
                cancellations,
                download_root,
            });
        Ok(())
    }

    pub fn start_send(
        &self,
        wire: &WebRtcWireBinding,
        source_path: PathBuf,
        transfer_id: String,
        events: &EventBus,
    ) -> Result<OutboundFileMessage, String> {
        let persistence = self.persistence()?;
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| format!("Could not inspect shared file: {error}"))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err("DeskLink shares only regular, non-symlink files".to_string());
        }
        if metadata.len() > MAX_TRANSFER_SIZE {
            return Err(format!(
                "File exceeds the {MAX_TRANSFER_SIZE} byte transfer limit"
            ));
        }
        let filename = source_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "Shared filename is not valid UTF-8".to_string())?
            .to_string();
        validate_filename(&filename)?;
        let transfer_token = Uuid::new_v4().to_string();
        let digest = sha256_file(&source_path)?;
        let mut checkpoint = TransferCheckpoint::new(
            transfer_id.clone(),
            wire.peer_device_id.clone(),
            filename.clone(),
            source_path.clone(),
            PathBuf::new(),
            metadata.len(),
            transfer_token.clone(),
        )?;
        checkpoint.source_path = Some(source_path);
        checkpoint.sha256 = Some(digest.clone());
        checkpoint.chunk_size = MAX_FILE_CHUNK_BYTES as u64;
        checkpoint.state = TransferState::Pending;
        persist_checkpoint(&persistence, checkpoint.clone(), events, None)?;
        self.active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?
            .insert(
                transfer_id.clone(),
                ActiveTransfer::Sending {
                    wire: wire.clone(),
                    awaiting_offset: None,
                },
            );
        Ok(OutboundFileMessage::Control(control(
            wire,
            FileTransferAction::Offer,
            &checkpoint,
            Some(filename),
            Some(metadata.len()),
            Some(digest),
            0,
            None,
        )))
    }

    pub fn resume_sends(
        &self,
        wire: &WebRtcWireBinding,
        events: &EventBus,
    ) -> Result<Vec<OutboundFileMessage>, String> {
        let persistence = self.persistence()?;
        let mut outgoing = Vec::new();
        for mut checkpoint in persistence.store.list()? {
            if checkpoint.device_id != wire.peer_device_id
                || checkpoint.source_path.is_none()
                || matches!(
                    checkpoint.state,
                    TransferState::Completed | TransferState::Cancelled
                )
            {
                continue;
            }
            if self
                .active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?
                .contains_key(&checkpoint.transfer_id)
            {
                continue;
            }
            let source = checkpoint
                .source_path
                .as_deref()
                .ok_or_else(|| "Resumable transfer has no source".to_string())?;
            reject_symlink(source)?;
            let metadata = fs::metadata(source)
                .map_err(|error| format!("Could not reopen resumable transfer: {error}"))?;
            if !metadata.is_file() || metadata.len() != checkpoint.total_size {
                checkpoint.state = TransferState::Failed;
                persist_checkpoint(
                    &persistence,
                    checkpoint,
                    events,
                    Some("Shared file changed before transfer resume".to_string()),
                )?;
                continue;
            }
            let digest = sha256_file(source)?;
            if checkpoint.sha256.as_deref() != Some(digest.as_str()) {
                checkpoint.state = TransferState::Failed;
                persist_checkpoint(
                    &persistence,
                    checkpoint,
                    events,
                    Some("Shared file checksum changed before transfer resume".to_string()),
                )?;
                continue;
            }
            checkpoint.state = TransferState::Pending;
            persist_checkpoint(&persistence, checkpoint.clone(), events, None)?;
            self.active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?
                .insert(
                    checkpoint.transfer_id.clone(),
                    ActiveTransfer::Sending {
                        wire: wire.clone(),
                        awaiting_offset: None,
                    },
                );
            outgoing.push(OutboundFileMessage::Control(control(
                wire,
                FileTransferAction::Offer,
                &checkpoint,
                Some(checkpoint.filename.clone()),
                Some(checkpoint.total_size),
                checkpoint.sha256.clone(),
                0,
                None,
            )));
        }
        Ok(outgoing)
    }

    pub fn handle_control(
        &self,
        wire: &WebRtcWireBinding,
        message: FileTransferControl,
        events: &EventBus,
    ) -> Result<Vec<OutboundFileMessage>, String> {
        message.validate(wire).map_err(|error| error.to_string())?;
        match message.action {
            FileTransferAction::Offer => self.receive_offer(wire, message, events),
            FileTransferAction::Accept => self.accept_send(wire, message, events),
            FileTransferAction::Acknowledge => self.acknowledge_send(wire, message, events),
            FileTransferAction::Complete => {
                self.finish_sender(wire, &message, events)?;
                Ok(Vec::new())
            }
            FileTransferAction::Cancel => {
                self.finish_remote_cancel(wire, &message, events)?;
                Ok(Vec::new())
            }
            FileTransferAction::Error => {
                self.finish_remote_error(wire, &message, events)?;
                Ok(Vec::new())
            }
        }
    }

    pub fn handle_chunk(
        &self,
        wire: &WebRtcWireBinding,
        encoded: &[u8],
        events: &EventBus,
    ) -> Result<Vec<OutboundFileMessage>, String> {
        let chunk = FileChunk::decode(encoded).map_err(|error| error.to_string())?;
        let persistence = self.persistence()?;
        let active = self
            .active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?
            .get(&chunk.transfer_id)
            .cloned()
            .ok_or_else(|| "WebRTC file chunk has no active offer".to_string())?;
        match active {
            ActiveTransfer::Receiving { wire: expected } if expected == *wire => {}
            _ => return Err("WebRTC file chunk belongs to a stale transfer binding".to_string()),
        }
        let mut checkpoint = checkpoint(&persistence, &chunk.transfer_id)?;
        validate_checkpoint_binding(&checkpoint, wire, &chunk.transfer_token)?;
        if is_cancelled(&persistence, &chunk.transfer_id) {
            checkpoint.state = TransferState::Cancelled;
            persist_checkpoint(&persistence, checkpoint.clone(), events, None)?;
            self.active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?
                .remove(&chunk.transfer_id);
            return Ok(vec![OutboundFileMessage::Control(control(
                wire,
                FileTransferAction::Cancel,
                &checkpoint,
                None,
                None,
                None,
                checkpoint.offset,
                None,
            ))]);
        }
        if chunk.offset != checkpoint.offset {
            return Err(format!(
                "WebRTC file chunk offset {} does not match checkpoint {}",
                chunk.offset, checkpoint.offset
            ));
        }
        let next_offset = checkpoint
            .offset
            .checked_add(chunk.data.len() as u64)
            .ok_or_else(|| "WebRTC file chunk offset overflow".to_string())?;
        if next_offset > checkpoint.total_size {
            return Err("WebRTC file chunk exceeds the announced transfer size".to_string());
        }
        reject_symlink(&checkpoint.temporary_path)?;
        let current_size = fs::metadata(&checkpoint.temporary_path)
            .map_err(|error| format!("Could not inspect partial transfer: {error}"))?
            .len();
        if current_size != checkpoint.offset {
            return Err("Partial file size does not match its durable checkpoint".to_string());
        }
        let mut file = OpenOptions::new()
            .write(true)
            .open(&checkpoint.temporary_path)
            .map_err(|error| format!("Could not open partial transfer: {error}"))?;
        file.seek(SeekFrom::Start(checkpoint.offset))
            .map_err(|error| format!("Could not seek partial transfer: {error}"))?;
        file.write_all(&chunk.data)
            .map_err(|error| format!("Could not write partial transfer: {error}"))?;
        file.sync_data()
            .map_err(|error| format!("Could not synchronize partial transfer: {error}"))?;
        checkpoint.advance(next_offset)?;
        checkpoint.state = TransferState::Transferring;
        persist_checkpoint(&persistence, checkpoint.clone(), events, None)?;

        let mut outgoing = vec![OutboundFileMessage::Control(control(
            wire,
            FileTransferAction::Acknowledge,
            &checkpoint,
            None,
            None,
            None,
            next_offset,
            None,
        ))];
        if next_offset == checkpoint.total_size {
            finalize_receive(&persistence, &mut checkpoint, events)?;
            self.active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?
                .remove(&chunk.transfer_id);
            outgoing.push(OutboundFileMessage::Control(control(
                wire,
                FileTransferAction::Complete,
                &checkpoint,
                None,
                None,
                checkpoint.sha256.clone(),
                checkpoint.offset,
                None,
            )));
        }
        Ok(outgoing)
    }

    pub fn cancel(
        &self,
        transfer_id: &str,
        events: &EventBus,
    ) -> Result<Option<(WebRtcWireBinding, OutboundFileMessage)>, String> {
        let persistence = self.persistence()?;
        if let Ok(mut cancellations) = persistence.cancellations.lock() {
            cancellations.insert(transfer_id.to_string());
        }
        let active = self
            .active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?
            .remove(transfer_id);
        let Some(active) = active else {
            if let Some(mut stored) = persistence.store.load(transfer_id)? {
                stored.state = TransferState::Cancelled;
                persist_checkpoint(&persistence, stored, events, None)?;
            }
            return Ok(None);
        };
        let wire = match active {
            ActiveTransfer::Sending { wire, .. } | ActiveTransfer::Receiving { wire } => wire,
        };
        let mut checkpoint = checkpoint(&persistence, transfer_id)?;
        checkpoint.state = TransferState::Cancelled;
        persist_checkpoint(&persistence, checkpoint.clone(), events, None)?;
        let message = OutboundFileMessage::Control(control(
            &wire,
            FileTransferAction::Cancel,
            &checkpoint,
            None,
            None,
            None,
            checkpoint.offset,
            None,
        ));
        Ok(Some((wire, message)))
    }

    /// Drops volatile transfer writers for a failed peer while retaining the
    /// durable checkpoint and partial file. A newly authenticated generation
    /// can safely re-offer sender checkpoints.
    pub fn pause_for_wire(
        &self,
        wire: &WebRtcWireBinding,
        events: &EventBus,
    ) -> Result<(), String> {
        let persistence = self.persistence()?;
        let transfer_ids = {
            let active = self
                .active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?;
            active
                .iter()
                .filter_map(|(transfer_id, transfer)| {
                    let active_wire = match transfer {
                        ActiveTransfer::Sending { wire, .. }
                        | ActiveTransfer::Receiving { wire } => wire,
                    };
                    (active_wire == wire).then(|| transfer_id.clone())
                })
                .collect::<Vec<_>>()
        };
        for transfer_id in transfer_ids {
            self.active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?
                .remove(&transfer_id);
            if let Some(mut checkpoint) = persistence.store.load(&transfer_id)? {
                if !checkpoint.state.is_terminal() {
                    checkpoint.state = TransferState::Paused;
                    persist_checkpoint(&persistence, checkpoint, events, None)?;
                }
            }
        }
        Ok(())
    }

    fn receive_offer(
        &self,
        wire: &WebRtcWireBinding,
        message: FileTransferControl,
        events: &EventBus,
    ) -> Result<Vec<OutboundFileMessage>, String> {
        let persistence = self.persistence()?;
        let filename = message
            .filename
            .clone()
            .ok_or_else(|| "WebRTC file offer has no filename".to_string())?;
        validate_filename(&filename)?;
        let total_size = message
            .total_size
            .ok_or_else(|| "WebRTC file offer has no size".to_string())?;
        let digest = message
            .sha256
            .clone()
            .ok_or_else(|| "WebRTC file offer has no checksum".to_string())?;
        let existing = persistence.store.load(&message.transfer_id)?;
        let mut checkpoint = if let Some(checkpoint) = existing {
            if checkpoint.device_id != wire.peer_device_id
                || checkpoint.filename != filename
                || checkpoint.total_size != total_size
                || checkpoint.resume_token != message.transfer_token
                || checkpoint.sha256.as_deref() != Some(digest.as_str())
            {
                return Err("Incoming file offer does not match its checkpoint".to_string());
            }
            reject_symlink(&checkpoint.temporary_path)?;
            let partial_size = fs::metadata(&checkpoint.temporary_path)
                .map_err(|error| format!("Could not inspect resumable file: {error}"))?
                .len();
            if partial_size != checkpoint.offset {
                return Err("Resumable file size does not match its checkpoint".to_string());
            }
            checkpoint
        } else {
            let target = collision_safe_path(&persistence.download_root, &filename)?;
            let temporary = persistence
                .download_root
                .join(format!(".desklink-{}.part", message.transfer_id));
            reject_symlink(&temporary)?;
            OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .map_err(|error| format!("Could not create partial transfer: {error}"))?
                .sync_all()
                .map_err(|error| format!("Could not synchronize partial transfer: {error}"))?;
            let mut checkpoint = TransferCheckpoint::new(
                message.transfer_id.clone(),
                wire.peer_device_id.clone(),
                filename,
                temporary,
                target,
                total_size,
                message.transfer_token.clone(),
            )?;
            checkpoint.sha256 = Some(digest);
            checkpoint.chunk_size = u64::from(message.chunk_size.min(MAX_FILE_CHUNK_BYTES as u32));
            checkpoint
        };
        checkpoint.state = TransferState::Transferring;
        persist_checkpoint(&persistence, checkpoint.clone(), events, None)?;
        self.active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?
            .insert(
                checkpoint.transfer_id.clone(),
                ActiveTransfer::Receiving { wire: wire.clone() },
            );
        if checkpoint.total_size == 0 {
            finalize_receive(&persistence, &mut checkpoint, events)?;
            self.active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?
                .remove(&checkpoint.transfer_id);
            return Ok(vec![OutboundFileMessage::Control(control(
                wire,
                FileTransferAction::Complete,
                &checkpoint,
                None,
                None,
                checkpoint.sha256.clone(),
                0,
                None,
            ))]);
        }
        Ok(vec![OutboundFileMessage::Control(control(
            wire,
            FileTransferAction::Accept,
            &checkpoint,
            None,
            None,
            None,
            checkpoint.offset,
            None,
        ))])
    }

    fn accept_send(
        &self,
        wire: &WebRtcWireBinding,
        message: FileTransferControl,
        events: &EventBus,
    ) -> Result<Vec<OutboundFileMessage>, String> {
        let persistence = self.persistence()?;
        self.validate_sender(wire, &message)?;
        let mut checkpoint = checkpoint(&persistence, &message.transfer_id)?;
        validate_checkpoint_binding(&checkpoint, wire, &message.transfer_token)?;
        if message.offset > checkpoint.total_size {
            return Err("Receiver requested an invalid resume offset".to_string());
        }
        checkpoint.offset = message.offset;
        checkpoint.state = TransferState::Transferring;
        persist_checkpoint(&persistence, checkpoint, events, None)?;
        self.send_next_chunk(wire, &message.transfer_id)
            .map(|message| message.into_iter().collect())
    }

    fn acknowledge_send(
        &self,
        wire: &WebRtcWireBinding,
        message: FileTransferControl,
        events: &EventBus,
    ) -> Result<Vec<OutboundFileMessage>, String> {
        let persistence = self.persistence()?;
        let expected = self.validate_sender(wire, &message)?;
        if expected != Some(message.offset) {
            return Err("Receiver acknowledgement does not match the sent chunk".to_string());
        }
        let mut checkpoint = checkpoint(&persistence, &message.transfer_id)?;
        validate_checkpoint_binding(&checkpoint, wire, &message.transfer_token)?;
        checkpoint.advance(message.offset)?;
        checkpoint.state = TransferState::Transferring;
        persist_checkpoint(&persistence, checkpoint, events, None)?;
        self.send_next_chunk(wire, &message.transfer_id)
            .map(|message| message.into_iter().collect())
    }

    fn validate_sender(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileTransferControl,
    ) -> Result<Option<u64>, String> {
        let active = self
            .active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?;
        match active.get(&message.transfer_id) {
            Some(ActiveTransfer::Sending {
                wire: expected,
                awaiting_offset,
            }) if expected == wire => Ok(*awaiting_offset),
            _ => Err("WebRTC transfer response belongs to a stale sender".to_string()),
        }
    }

    fn send_next_chunk(
        &self,
        wire: &WebRtcWireBinding,
        transfer_id: &str,
    ) -> Result<Option<OutboundFileMessage>, String> {
        let persistence = self.persistence()?;
        let checkpoint = checkpoint(&persistence, transfer_id)?;
        if checkpoint.offset == checkpoint.total_size {
            if let Some(ActiveTransfer::Sending {
                awaiting_offset, ..
            }) = self
                .active
                .lock()
                .map_err(|_| "WebRTC transfer map poisoned".to_string())?
                .get_mut(transfer_id)
            {
                *awaiting_offset = None;
            }
            return Ok(None);
        }
        let source = checkpoint
            .source_path
            .as_deref()
            .ok_or_else(|| "Transfer source path is unavailable".to_string())?;
        reject_symlink(source)?;
        let metadata = fs::metadata(source).map_err(|error| error.to_string())?;
        if metadata.len() != checkpoint.total_size {
            return Err("Shared file changed size during transfer".to_string());
        }
        let remaining = checkpoint.total_size - checkpoint.offset;
        let length = remaining.min(checkpoint.chunk_size) as usize;
        let mut bytes = vec![0; length];
        let mut file = fs::File::open(source).map_err(|error| error.to_string())?;
        file.seek(SeekFrom::Start(checkpoint.offset))
            .map_err(|error| error.to_string())?;
        file.read_exact(&mut bytes)
            .map_err(|error| error.to_string())?;
        let next_offset = checkpoint.offset + bytes.len() as u64;
        let encoded = FileChunk {
            transfer_id: checkpoint.transfer_id.clone(),
            transfer_token: checkpoint.resume_token.clone(),
            offset: checkpoint.offset,
            data: bytes,
        }
        .encode()
        .map_err(|error| error.to_string())?;
        let mut active = self
            .active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?;
        match active.get_mut(transfer_id) {
            Some(ActiveTransfer::Sending {
                wire: expected,
                awaiting_offset,
            }) if expected == wire => *awaiting_offset = Some(next_offset),
            _ => return Err("WebRTC transfer sender became stale".to_string()),
        }
        Ok(Some(OutboundFileMessage::Chunk(encoded)))
    }

    fn finish_sender(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileTransferControl,
        events: &EventBus,
    ) -> Result<(), String> {
        let persistence = self.persistence()?;
        self.validate_sender(wire, message)?;
        let mut checkpoint = checkpoint(&persistence, &message.transfer_id)?;
        validate_checkpoint_binding(&checkpoint, wire, &message.transfer_token)?;
        if message.offset != checkpoint.total_size {
            return Err("Receiver completed a transfer at the wrong offset".to_string());
        }
        if message.sha256.as_deref() != checkpoint.sha256.as_deref() {
            return Err("Receiver completed a transfer with the wrong checksum".to_string());
        }
        checkpoint.offset = checkpoint.total_size;
        checkpoint.state = TransferState::Completed;
        persist_checkpoint(&persistence, checkpoint, events, None)?;
        self.active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?
            .remove(&message.transfer_id);
        Ok(())
    }

    fn finish_remote_cancel(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileTransferControl,
        events: &EventBus,
    ) -> Result<(), String> {
        self.finish_remote_state(wire, message, TransferState::Cancelled, None, events)
    }

    fn finish_remote_error(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileTransferControl,
        events: &EventBus,
    ) -> Result<(), String> {
        self.finish_remote_state(
            wire,
            message,
            TransferState::Failed,
            message.error.clone(),
            events,
        )
    }

    fn finish_remote_state(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileTransferControl,
        state: TransferState,
        error: Option<String>,
        events: &EventBus,
    ) -> Result<(), String> {
        let persistence = self.persistence()?;
        let mut checkpoint = checkpoint(&persistence, &message.transfer_id)?;
        validate_checkpoint_binding(&checkpoint, wire, &message.transfer_token)?;
        checkpoint.state = state;
        persist_checkpoint(&persistence, checkpoint, events, error)?;
        self.active
            .lock()
            .map_err(|_| "WebRTC transfer map poisoned".to_string())?
            .remove(&message.transfer_id);
        Ok(())
    }

    fn persistence(&self) -> Result<TransferPersistence, String> {
        self.persistence
            .lock()
            .map_err(|_| "WebRTC transfer persistence lock poisoned".to_string())?
            .clone()
            .ok_or_else(|| "WebRTC file-transfer persistence is not configured".to_string())
    }
}

#[allow(clippy::too_many_arguments)]
fn control(
    wire: &WebRtcWireBinding,
    action: FileTransferAction,
    checkpoint: &TransferCheckpoint,
    filename: Option<String>,
    total_size: Option<u64>,
    digest: Option<String>,
    offset: u64,
    error: Option<String>,
) -> FileTransferControl {
    FileTransferControl {
        protocol_version: FILE_PROTOCOL_VERSION,
        action,
        transfer_id: checkpoint.transfer_id.clone(),
        device_id: wire.sender_device_id.clone(),
        session_id: wire.session_id,
        connection_generation: wire.generation,
        transfer_token: checkpoint.resume_token.clone(),
        filename,
        total_size,
        sha256: digest,
        offset,
        chunk_size: checkpoint.chunk_size.min(MAX_FILE_CHUNK_BYTES as u64) as u32,
        error,
    }
}

fn checkpoint(
    persistence: &TransferPersistence,
    transfer_id: &str,
) -> Result<TransferCheckpoint, String> {
    persistence
        .manager
        .get(transfer_id)
        .or_else(|| persistence.store.load(transfer_id).ok().flatten())
        .ok_or_else(|| "Transfer checkpoint is unavailable".to_string())
}

fn persist_checkpoint(
    persistence: &TransferPersistence,
    checkpoint: TransferCheckpoint,
    events: &EventBus,
    error: Option<String>,
) -> Result<(), String> {
    persistence.manager.register(checkpoint.clone())?;
    persistence.store.save(&checkpoint)?;
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
    Ok(())
}

fn validate_checkpoint_binding(
    checkpoint: &TransferCheckpoint,
    wire: &WebRtcWireBinding,
    token: &str,
) -> Result<(), String> {
    if checkpoint.device_id != wire.peer_device_id || checkpoint.resume_token != token {
        return Err("WebRTC transfer device or token does not match".to_string());
    }
    Ok(())
}

fn finalize_receive(
    persistence: &TransferPersistence,
    checkpoint: &mut TransferCheckpoint,
    events: &EventBus,
) -> Result<(), String> {
    let metadata = fs::metadata(&checkpoint.temporary_path)
        .map_err(|error| format!("Could not inspect completed partial file: {error}"))?;
    if metadata.len() != checkpoint.total_size {
        return Err("Completed partial file has the wrong size".to_string());
    }
    let actual = sha256_file(&checkpoint.temporary_path)?;
    if checkpoint.sha256.as_deref() != Some(actual.as_str()) {
        checkpoint.state = TransferState::Failed;
        persist_checkpoint(
            persistence,
            checkpoint.clone(),
            events,
            Some("Downloaded file checksum does not match the offer".to_string()),
        )?;
        return Err("Downloaded file checksum does not match the offer".to_string());
    }
    let target = collision_safe_path(&persistence.download_root, &checkpoint.filename)?;
    reject_symlink(&target)?;
    fs::hard_link(&checkpoint.temporary_path, &target)
        .map_err(|error| format!("Could not atomically publish downloaded file: {error}"))?;
    if let Err(error) = fs::remove_file(&checkpoint.temporary_path) {
        let _ = fs::remove_file(&target);
        return Err(format!("Could not remove completed partial file: {error}"));
    }
    sync_directory(&persistence.download_root)?;
    checkpoint.destination_path = target;
    checkpoint.offset = checkpoint.total_size;
    checkpoint.state = TransferState::Completed;
    persist_checkpoint(persistence, checkpoint.clone(), events, None)
}

fn collision_safe_path(root: &Path, filename: &str) -> Result<PathBuf, String> {
    validate_filename(filename)?;
    let original = root.join(filename);
    if !original.exists() {
        return Ok(original);
    }
    let path = Path::new(filename);
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let extension = path.extension().and_then(|value| value.to_str());
    for suffix in 1..=10_000u32 {
        let candidate = match extension {
            Some(extension) if !extension.is_empty() => {
                root.join(format!("{stem}_{suffix}.{extension}"))
            }
            _ => root.join(format!("{stem}_{suffix}")),
        };
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err("Could not allocate a collision-safe destination filename".to_string())
}

fn validate_filename(filename: &str) -> Result<(), String> {
    let path = Path::new(filename);
    if filename.is_empty()
        || filename == "."
        || filename == ".."
        || filename.contains('/')
        || filename.contains('\\')
        || path.components().count() != 1
        || path.file_name().and_then(|name| name.to_str()) != Some(filename)
    {
        return Err("Transfer filename must be one safe path component".to_string());
    }
    Ok(())
}

fn reject_symlink(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("Refusing transfer symlink: {path:?}"))
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not inspect transfer path {path:?}: {error}")),
    }
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| error.to_string())?;
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

fn is_cancelled(persistence: &TransferPersistence, transfer_id: &str) -> bool {
    persistence
        .cancellations
        .lock()
        .map(|cancelled| cancelled.contains(transfer_id))
        .unwrap_or(true)
}

fn sync_directory(directory: &Path) -> Result<(), String> {
    #[cfg(unix)]
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| error.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use openssl::sha::sha256;

    fn setup(root: &Path) -> WebRtcFileTransferManager {
        let manager = WebRtcFileTransferManager::default();
        manager
            .configure(
                TransferManager::default(),
                TransferCheckpointStore::new(root.join("state")).unwrap(),
                Arc::new(Mutex::new(HashSet::new())),
                root.join("downloads"),
            )
            .unwrap();
        manager
    }

    fn wire() -> WebRtcWireBinding {
        WebRtcWireBinding {
            sender_device_id: "desktop".to_string(),
            peer_device_id: "phone".to_string(),
            session_id: 42,
            generation: 7,
        }
    }

    #[test]
    fn receive_chunks_are_checkpointed_and_atomically_published() {
        let root = std::env::temp_dir().join(format!("desklink-webrtc-file-{}", Uuid::new_v4()));
        let manager = setup(&root);
        let events = EventBus::default();
        let data = b"DeskLink WebRTC file";
        let offer = FileTransferControl {
            protocol_version: 1,
            action: FileTransferAction::Offer,
            transfer_id: "transfer-1".to_string(),
            device_id: "phone".to_string(),
            session_id: 42,
            connection_generation: 7,
            transfer_token: "token-1".to_string(),
            filename: Some("report.txt".to_string()),
            total_size: Some(data.len() as u64),
            sha256: Some(
                sha256(data)
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect(),
            ),
            offset: 0,
            chunk_size: MAX_FILE_CHUNK_BYTES as u32,
            error: None,
        };
        let reply = manager
            .handle_control(&wire(), offer, &events)
            .expect("offer accepted");
        assert!(matches!(
            &reply[0],
            OutboundFileMessage::Control(FileTransferControl {
                action: FileTransferAction::Accept,
                offset: 0,
                ..
            })
        ));
        let chunk = FileChunk {
            transfer_id: "transfer-1".to_string(),
            transfer_token: "token-1".to_string(),
            offset: 0,
            data: data.to_vec(),
        }
        .encode()
        .unwrap();
        let replies = manager.handle_chunk(&wire(), &chunk, &events).unwrap();
        assert_eq!(replies.len(), 2);
        assert_eq!(fs::read(root.join("downloads/report.txt")).unwrap(), data);
        assert!(!root.join("downloads/.desklink-transfer-1.part").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sender_advances_only_after_exact_acknowledgement() {
        let root = std::env::temp_dir().join(format!("desklink-webrtc-send-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.txt");
        fs::write(&source, b"hello").unwrap();
        let manager = setup(&root);
        let events = EventBus::default();
        let offer = manager
            .start_send(&wire(), source, "transfer-2".to_string(), &events)
            .unwrap();
        let OutboundFileMessage::Control(offer) = offer else {
            panic!("offer expected")
        };
        let accept = FileTransferControl {
            action: FileTransferAction::Accept,
            device_id: "phone".to_string(),
            filename: None,
            total_size: None,
            sha256: None,
            offset: 0,
            error: None,
            ..offer.clone()
        };
        let chunk = manager.handle_control(&wire(), accept, &events).unwrap();
        let OutboundFileMessage::Chunk(encoded) = &chunk[0] else {
            panic!("chunk expected")
        };
        let end = FileChunk::decode(encoded).unwrap().data.len() as u64;
        let bad_ack = FileTransferControl {
            action: FileTransferAction::Acknowledge,
            device_id: "phone".to_string(),
            offset: end - 1,
            filename: None,
            total_size: None,
            sha256: None,
            error: None,
            ..offer
        };
        assert!(manager.handle_control(&wire(), bad_ack, &events).is_err());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn sender_checkpoint_is_reoffered_after_manager_restart() {
        let root = std::env::temp_dir().join(format!("desklink-webrtc-resume-{}", Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let source = root.join("source.txt");
        fs::write(&source, b"resume me").unwrap();
        let events = EventBus::default();
        let first = setup(&root);
        first
            .start_send(&wire(), source, "transfer-3".to_string(), &events)
            .unwrap();
        drop(first);

        let restarted = setup(&root);
        let messages = restarted.resume_sends(&wire(), &events).unwrap();
        assert!(matches!(
            messages.as_slice(),
            [OutboundFileMessage::Control(FileTransferControl {
                action: FileTransferAction::Offer,
                transfer_id,
                ..
            })] if transfer_id == "transfer-3"
        ));
        let _ = fs::remove_dir_all(root);
    }
}
