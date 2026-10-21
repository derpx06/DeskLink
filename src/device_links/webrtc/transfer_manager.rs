//! Checkpointed receiver for the DeskLink WebRTC file protocol.
//!
//! The manager owns only local filesystem state.  Peer and data-channel code
//! supplies validated wire records and sends the returned control messages.
//! This separation keeps untrusted network input away from filesystem paths
//! and makes restart/resume behaviour testable without GStreamer.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::file_protocol::{
    validate_filename, FileAction, FileChunk, FileControl, FILE_PROTOCOL_VERSION,
    LEGACY_FILE_CHUNK_BYTES, MAX_FILE_CHUNK_BYTES, MAX_TRANSFER_BYTES,
};
use super::transfer_board::{self as board, Finished, Outcome, TransferRow};
use super::WebRtcWireBinding;

/// Offers a single peer session may hold open at once. Every accepted offer
/// owns a partial file and a checkpoint on disk, so without a bound one peer
/// could exhaust inodes and disk with offers it never intends to finish.
const MAX_INCOMING_TRANSFERS: usize = 16;

/// A transfer's files untouched this long belong to a transfer nobody is going
/// to finish. Live ones are rewritten with every chunk, so age is a safe test.
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 60 * 60);

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
    /// The file the user picked, for the Transfers page's history. Local
    /// state only (never sent); absent in checkpoints from older builds.
    #[serde(default)]
    source_path: Option<PathBuf>,
    /// Highest byte sent; `offset` is the highest acknowledged.
    awaiting_offset: Option<u64>,
    /// Negotiated chunk size (legacy unless the receiver advertised more).
    #[serde(default = "legacy_chunk_size")]
    chunk_size: usize,
}

fn legacy_chunk_size() -> usize {
    LEGACY_FILE_CHUNK_BYTES
}

/// Unacknowledged bytes a sender keeps in flight. Stop-and-wait (one chunk
/// per round trip, plus the receiver's fsyncs) capped transfers at ~1 MB/s.
/// Older receivers already handle pipelined chunks: they process the ordered
/// channel one chunk at a time and acknowledge each.
const SEND_WINDOW_BYTES: u64 = 1024 * 1024;

/// Durability is batched: data is synced and checkpointed at most this often
/// (and at the end). A crash loses at most this much, which resume re-sends —
/// it truncates the partial file to the last checkpoint.
const DURABLE_EVERY_BYTES: u64 = 8 * 1024 * 1024;

/// Whether `[from, to)` crosses a durability boundary or ends the file.
fn crosses_durability_boundary(from: u64, to: u64, total: u64, every: u64) -> bool {
    to == total || from / every != to / every
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

    /// A checkpoint read back from disk is untrusted state: every field that
    /// names a file or bounds a write must still make sense for `transfer_id`.
    fn is_consistent(&self, transfer_id: &str) -> bool {
        self.transfer_id == transfer_id
            && self.part_name == part_name(transfer_id)
            && self.offset <= self.total_size
            && self.total_size <= MAX_TRANSFER_BYTES
            && self.sha256.len() == 64
            && self.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            && validate_filename(&self.filename).is_ok()
            && validate_filename(&self.destination_name).is_ok()
    }
}

fn part_name(transfer_id: &str) -> String {
    format!(".desklink-{transfer_id}.part")
}

/// A restart-safe, bounded receiver for Android-compatible file offers.
pub struct WebRtcTransferManager {
    state_root: PathBuf,
    destination_root: PathBuf,
    active: Mutex<HashMap<String, ReceiveCheckpoint>>,
    outgoing: Mutex<HashMap<String, SendCheckpoint>>,
    /// Set once interrupted sends have been offered again on this connection.
    resumed: AtomicBool,
    /// `DURABLE_EVERY_BYTES`, except in tests that exercise crash recovery.
    durable_every: u64,
    /// Running SHA-256 of each incoming file, fed in order as chunks land, and
    /// how far it has hashed. Verifying by re-reading the whole file stalled
    /// the peer's event worker for seconds on large files — long enough for
    /// the 15 s liveness check to kill a healthy connection. A transfer resumed
    /// after a restart has none and falls back to one full hash at the end.
    hashers: Mutex<HashMap<String, (u64, openssl::sha::Sha256)>>,
}

impl WebRtcTransferManager {
    /// Crash-recovery tests need every chunk durable to observe a precise
    /// resume point.
    #[cfg(test)]
    fn durable_every_chunk(mut self) -> Self {
        self.durable_every = 1;
        self
    }

    pub fn new(state_root: PathBuf, destination_root: PathBuf) -> Result<Self, String> {
        fs::create_dir_all(&state_root).map_err(|error| {
            format!("Could not create DeskLink transfer state directory: {error}")
        })?;
        fs::create_dir_all(&destination_root)
            .map_err(|error| format!("Could not create DeskLink download directory: {error}"))?;
        ensure_directory_not_symlink(&state_root)?;
        ensure_directory_not_symlink(&destination_root)?;
        let manager = Self {
            state_root,
            destination_root,
            active: Mutex::new(HashMap::new()),
            outgoing: Mutex::new(HashMap::new()),
            resumed: AtomicBool::new(false),
            durable_every: DURABLE_EVERY_BYTES,
            hashers: Mutex::new(HashMap::new()),
        };
        manager.sweep_stale(STALE_AFTER);
        Ok(manager)
    }

    /// Removes the files of transfers nobody has touched for `max_age`: a
    /// snapshot of up to 4 GiB or a partial download would otherwise sit on the
    /// disk forever. Only files this manager names (`.desklink-…`) are looked
    /// at, and only regular ones.
    pub fn sweep_stale(&self, max_age: Duration) {
        let Some(cutoff) = SystemTime::now().checked_sub(max_age) else {
            return;
        };
        let Ok(entries) = fs::read_dir(&self.state_root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            let ours = name.starts_with(".desklink-")
                && [".send", ".send.json", ".part", ".json", ".tmp"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix));
            if !ours {
                continue;
            }
            let stale = fs::symlink_metadata(entry.path())
                .ok()
                .filter(|metadata| metadata.is_file())
                .and_then(|metadata| metadata.modified().ok())
                .is_some_and(|modified| modified < cutoff);
            if stale {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    /// Offers again every send that a lost connection interrupted, so the peer
    /// carries on from what it already has instead of the user starting over.
    /// The offer keeps the transfer's id and token — that is how the peer finds
    /// its own checkpoint — and answers with where it stopped. Once per
    /// connection; a send whose snapshot no longer matches what was offered is
    /// dropped rather than retried forever.
    pub fn resume_sends(&self, wire: &WebRtcWireBinding) -> Vec<TransferEvent> {
        if self.resumed.swap(true, Ordering::AcqRel) {
            return Vec::new();
        }
        let mut events = Vec::new();
        for stored in self.stored_sends() {
            if stored.device_id != wire.peer_device_id {
                continue;
            }
            if self
                .outgoing
                .lock()
                .is_ok_and(|outgoing| outgoing.contains_key(&stored.transfer_id))
            {
                continue;
            }
            if !self.snapshot_is_intact(&stored) {
                if let Ok(path) = self.checked_state_path(&stored.source_name) {
                    let _ = remove_if_present(&path);
                }
                let _ = self.remove_send_checkpoint(&stored.transfer_id);
                continue;
            }
            let rebound = SendCheckpoint {
                session_id: wire.session_id,
                generation: wire.generation,
                awaiting_offset: None,
                ..stored
            };
            if self.save_send_checkpoint(&rebound).is_err() {
                continue;
            }
            match self.outgoing.lock() {
                Ok(mut outgoing) => outgoing.insert(rebound.transfer_id.clone(), rebound.clone()),
                Err(_) => continue,
            };
            board::begin(TransferRow {
                id: rebound.transfer_id.clone(),
                device_id: wire.peer_device_id.clone(),
                name: rebound.filename.clone(),
                incoming: false,
                done: rebound.offset,
                total: rebound.total_size,
                preparing: false,
            });
            events.push(TransferEvent::SendControl(self.send_control(
                wire,
                FileAction::Offer,
                &rebound,
                0,
            )));
        }
        events
    }

    /// The sends left on disk, each one checked to be a well-formed record of
    /// its own snapshot (a checkpoint is untrusted state once read back).
    fn stored_sends(&self) -> Vec<SendCheckpoint> {
        let Ok(entries) = fs::read_dir(&self.state_root) else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name
                .to_str()
                .and_then(|name| name.strip_prefix(".desklink-"))
                .and_then(|name| name.strip_suffix(".send.json"))
            else {
                continue;
            };
            let Ok(Some(checkpoint)) = self.load_send_checkpoint(id) else {
                continue;
            };
            let sane = checkpoint.transfer_id == id
                && checkpoint.source_name == format!(".desklink-{id}.send")
                && checkpoint.offset <= checkpoint.total_size
                && checkpoint.total_size <= MAX_TRANSFER_BYTES
                && validate_filename(&checkpoint.filename).is_ok();
            if sane {
                found.push(checkpoint);
            }
        }
        found
    }

    fn snapshot_is_intact(&self, checkpoint: &SendCheckpoint) -> bool {
        self.checked_state_path(&checkpoint.source_name)
            .and_then(|path| {
                ensure_regular_file(&path)?;
                fs::metadata(&path).map_err(|error| error.to_string())
            })
            .is_ok_and(|metadata| metadata.len() == checkpoint.total_size)
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
        // The peer refuses an unsafe name only after the whole file has been
        // offered, so refuse it here before copying anything.
        validate_filename(&filename)?;
        let mut input = File::open(source)
            .map_err(|error| format!("Could not open DeskLink shared file: {error}"))?;
        // A directory, FIFO or device would never yield a finite snapshot.
        if !input.metadata().is_ok_and(|metadata| metadata.is_file()) {
            return Err("DeskLink shared path is not a regular file".to_string());
        }
        let transfer_id = uuid::Uuid::new_v4().to_string();
        let token = uuid::Uuid::new_v4().to_string();
        let source_name = format!(".desklink-{transfer_id}.send");
        let snapshot = self.checked_state_path(&source_name)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&snapshot)
            .map_err(|error| format!("Could not create DeskLink transfer snapshot: {error}"))?;
        let (bytes, sha256) = match copy_snapshot(&mut input, &mut output) {
            Ok(copied) => copied,
            Err(error) => {
                // Do not strand a partial copy (disk full, read error, limit).
                let _ = fs::remove_file(&snapshot);
                return Err(error);
            }
        };
        let checkpoint = SendCheckpoint {
            transfer_id: transfer_id.clone(),
            device_id: wire.peer_device_id.clone(),
            session_id: wire.session_id,
            generation: wire.generation,
            token,
            filename,
            total_size: bytes,
            sha256,
            offset: 0,
            source_name,
            source_path: Some(source.to_path_buf()),
            awaiting_offset: None,
            chunk_size: LEGACY_FILE_CHUNK_BYTES,
        };
        if let Err(error) = self.save_send_checkpoint(&checkpoint) {
            let _ = fs::remove_file(&snapshot);
            return Err(error);
        }
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(transfer_id, checkpoint.clone());
        board::begin(TransferRow {
            id: checkpoint.transfer_id.clone(),
            device_id: wire.peer_device_id.clone(),
            name: checkpoint.filename.clone(),
            incoming: false,
            done: 0,
            total: checkpoint.total_size,
            preparing: false,
        });
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
        let (id, token) = (message.transfer_id.clone(), message.transfer_token.clone());
        let offer = message.action == FileAction::Offer;
        // Only an offer carries the name of a file that has no state yet.
        let offer_name = offer.then(|| message.filename.clone().unwrap_or_default());
        let outcome = match message.action {
            FileAction::Offer => self.receive_offer(wire, message),
            FileAction::Accept => self.accept_send(wire, &message),
            FileAction::Acknowledge => self.acknowledge_send(wire, &message),
            FileAction::Complete => self.complete_send(wire, &message),
            // A peer error ends the transfer exactly like a cancellation: it
            // must not leave up to 4 GiB of partial data behind, and a later
            // chunk for it has to be refused. A retry starts with a new offer.
            // Nothing is sent back: answering an `Error` with an `Error`
            // would never stop.
            FileAction::Cancel | FileAction::Error => {
                self.cancel(wire, &message.transfer_id, &message.transfer_token)?;
                board::finish(
                    &message.transfer_id,
                    if message.action == FileAction::Cancel {
                        Outcome::Cancelled
                    } else {
                        Outcome::Failed(peer_reason(message.error.as_deref()))
                    },
                );
                return Ok(Vec::new());
            }
        };
        self.report_failure(wire, &id, &token, offer_name.as_deref(), outcome)
    }

    /// Writes exactly one verified file-data chunk and returns an ACK.  A
    /// final chunk is hashed before the temporary file is atomically renamed.
    pub fn handle_chunk(
        &self,
        wire: &WebRtcWireBinding,
        encoded: &[u8],
    ) -> Result<Vec<TransferEvent>, String> {
        let chunk = FileChunk::decode(encoded)?;
        let outcome = self.write_chunk(wire, &chunk);
        self.report_failure(
            wire,
            &chunk.transfer_id,
            &chunk.transfer_token,
            None,
            outcome,
        )
    }

    fn write_chunk(
        &self,
        wire: &WebRtcWireBinding,
        chunk: &FileChunk,
    ) -> Result<Vec<TransferEvent>, String> {
        let checkpoint = self
            .active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(&chunk.transfer_id)
            .cloned()
            // Only an offer accepted by this manager can be fed: a checkpoint
            // on disk was bound to an earlier session and needs a fresh offer.
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
        self.hash_chunk(&chunk.transfer_id, chunk.offset, &chunk.data);

        let updated = ReceiveCheckpoint {
            offset: next_offset,
            ..checkpoint
        };
        // The live offset is in memory; disk catches up every few MiB. Syncing
        // and rewriting the checkpoint for every chunk was 3 fsyncs per 16 KiB.
        if crosses_durability_boundary(
            chunk.offset,
            next_offset,
            updated.total_size,
            self.durable_every,
        ) {
            file.sync_data()
                .map_err(|error| format!("Could not persist DeskLink file chunk: {error}"))?;
            self.save_checkpoint(&updated)?;
        }
        self.active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(updated.transfer_id.clone(), updated.clone());
        board::progress(&updated.transfer_id, next_offset);

        if next_offset == updated.total_size {
            return self.finish_receive(wire, &updated, true);
        }
        Ok(vec![TransferEvent::SendControl(self.control(
            wire,
            FileAction::Acknowledge,
            &updated,
            next_offset,
            None,
        ))])
    }

    /// A failure on a transfer this side owns ends it and tells the peer.
    ///
    /// Returning the `Err` alone reaches nothing but a log line, and the peer
    /// would wait out its whole deadline on a transfer that is already dead
    /// (a full disk is the common case). Only a transfer the peer can prove it
    /// holds — the device and the secret token both match — is answered, so a
    /// stray or forged message changes nothing and gets no reply. An offer has
    /// no state yet, so a refused new offer is answered too.
    fn report_failure(
        &self,
        wire: &WebRtcWireBinding,
        transfer_id: &str,
        token: &str,
        offer: Option<&str>,
        outcome: Result<Vec<TransferEvent>, String>,
    ) -> Result<Vec<TransferEvent>, String> {
        let Err(reason) = outcome else {
            return outcome;
        };
        if offer.is_some() {
            // An offer that disagrees with a transfer already on disk is a stale
            // or forged message, not a failure of ours to report.
            if self.load_checkpoint(transfer_id).ok().flatten().is_some() {
                return Err(reason);
            }
        } else {
            if !self.owns(wire, transfer_id, token) {
                return Err(reason);
            }
            // Best effort: the peer is told either way.
            let _ = self.cancel(wire, transfer_id, token);
        }
        debug_log!("[DeskLink] Ending file transfer {transfer_id}: {reason}");
        let reason: String = reason.chars().take(256).collect();
        match offer {
            // Nothing was listed for a refused offer, but the user should still
            // learn that a file meant for them did not arrive.
            Some(name) => board::record(Finished {
                id: transfer_id.to_string(),
                device_id: wire.peer_device_id.clone(),
                name: name.to_string(),
                incoming: true,
                outcome: Outcome::Failed(reason.clone()),
            }),
            None => board::finish(transfer_id, Outcome::Failed(reason.clone())),
        }
        Ok(vec![TransferEvent::SendControl(FileControl {
            protocol_version: FILE_PROTOCOL_VERSION,
            action: FileAction::Error,
            transfer_id: transfer_id.to_string(),
            device_id: wire.sender_device_id.clone(),
            session_id: wire.session_id,
            connection_generation: wire.generation,
            transfer_token: token.to_string(),
            filename: None,
            total_size: None,
            sha256: None,
            offset: 0,
            chunk_size: LEGACY_FILE_CHUNK_BYTES,
            max_chunk_size: None,
            error: Some(reason),
        })])
    }

    /// Ends a transfer at the user's request, in either direction, and returns
    /// the `Cancel` the peer must hear. Nothing is left on disk: a cancelled
    /// transfer is never resumed.
    pub fn cancel_transfer(
        &self,
        wire: &WebRtcWireBinding,
        transfer_id: &str,
    ) -> Result<Vec<TransferEvent>, String> {
        let incoming = self
            .active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(transfer_id)
            .filter(|checkpoint| checkpoint.wire_matches(wire))
            .cloned();
        let outgoing = self
            .outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(transfer_id)
            .filter(|checkpoint| checkpoint.wire_matches(wire))
            .cloned();
        let (token, control) = match (incoming, outgoing) {
            (Some(checkpoint), _) => (
                checkpoint.token.clone(),
                self.control(
                    wire,
                    FileAction::Cancel,
                    &checkpoint,
                    checkpoint.offset,
                    None,
                ),
            ),
            (None, Some(checkpoint)) => (
                checkpoint.token.clone(),
                self.send_control(wire, FileAction::Cancel, &checkpoint, checkpoint.offset),
            ),
            (None, None) => return Err("That file transfer is not running".to_string()),
        };
        self.cancel(wire, transfer_id, &token)?;
        board::finish(transfer_id, Outcome::Cancelled);
        Ok(vec![TransferEvent::SendControl(control)])
    }

    /// Whether `token` opens a live transfer of this session, in either
    /// direction.
    fn owns(&self, wire: &WebRtcWireBinding, transfer_id: &str, token: &str) -> bool {
        let receiving = self.active.lock().is_ok_and(|active| {
            active.get(transfer_id).is_some_and(|checkpoint| {
                checkpoint.token == token && checkpoint.wire_matches(wire)
            })
        });
        receiving
            || self.outgoing.lock().is_ok_and(|outgoing| {
                outgoing.get(transfer_id).is_some_and(|checkpoint| {
                    checkpoint.token == token && checkpoint.wire_matches(wire)
                })
            })
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
        {
            let active = self
                .active
                .lock()
                .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?;
            // A replay of an offer that is already open is not a new transfer.
            if active.len() >= MAX_INCOMING_TRANSFERS && !active.contains_key(&message.transfer_id)
            {
                return Ok(vec![TransferEvent::SendControl(FileControl {
                    protocol_version: FILE_PROTOCOL_VERSION,
                    action: FileAction::Error,
                    transfer_id: message.transfer_id.clone(),
                    device_id: wire.sender_device_id.clone(),
                    session_id: wire.session_id,
                    connection_generation: wire.generation,
                    transfer_token: message.transfer_token.clone(),
                    filename: None,
                    total_size: None,
                    sha256: None,
                    offset: 0,
                    chunk_size: LEGACY_FILE_CHUNK_BYTES,
                    max_chunk_size: None,
                    error: Some("Too many simultaneous incoming transfers".to_string()),
                })]);
            }
        }
        // Within this process the in-memory offset is authoritative: it runs
        // ahead of the batched on-disk checkpoint, and the partial file really
        // holds those bytes. After a restart only the disk checkpoint counts.
        let in_memory = self
            .active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(&message.transfer_id)
            .cloned();
        let on_disk = self.load_checkpoint(&message.transfer_id)?;
        let resumable = match (in_memory, on_disk) {
            (Some(live), Some(stored)) if live.token == stored.token => Some(live),
            (_, stored) => stored,
        };
        let checkpoint = match resumable {
            Some(existing) => {
                // Session and generation are per-connection, so a resume
                // normally arrives on new ones; the device and the secret
                // transfer token are what identify the transfer.
                if existing.device_id != wire.peer_device_id
                    || existing.token != message.transfer_token
                    || existing.filename != filename
                    || existing.total_size != total_size
                    || !existing.sha256.eq_ignore_ascii_case(&sha256)
                {
                    return Err(
                        "DeskLink incoming file offer does not match its checkpoint".to_string()
                    );
                }
                let offset = self.reconcile_part(&self.part_path(&existing)?, existing.offset)?;
                ReceiveCheckpoint {
                    session_id: wire.session_id,
                    generation: wire.generation,
                    offset,
                    ..existing
                }
            }
            None => {
                let destination_name = self.available_destination_name(&filename)?;
                let part_name = part_name(&message.transfer_id);
                // Without a checkpoint nothing vouches for a file already at
                // this path (a crash orphan or a planted link), so it is
                // replaced rather than reused or written through.
                create_fresh_file(&self.checked_state_path(&part_name)?)?
                    .sync_all()
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
                    destination_name,
                }
            }
        };
        self.save_checkpoint(&checkpoint)?;
        self.active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint.clone());
        board::begin(TransferRow {
            id: checkpoint.transfer_id.clone(),
            device_id: wire.peer_device_id.clone(),
            name: checkpoint.filename.clone(),
            incoming: true,
            done: checkpoint.offset,
            total: checkpoint.total_size,
            preparing: false,
        });
        if checkpoint.offset == checkpoint.total_size {
            // An empty file, or a partial that already holds every byte: there
            // is no chunk left to wait for.
            return self.finish_receive(wire, &checkpoint, false);
        }
        let mut accept = self.control(
            wire,
            FileAction::Accept,
            &checkpoint,
            checkpoint.offset,
            None,
        );
        // Only a sender that offered the upgrade can be told a larger size.
        accept.max_chunk_size = message
            .max_chunk_size
            .map(|offered| offered.min(MAX_FILE_CHUNK_BYTES));
        Ok(vec![TransferEvent::SendControl(accept)])
    }

    /// The checkpoint, not the partial file, is the source of truth for how
    /// much was received. Bytes past it were never acknowledged (a crash
    /// between the write and the checkpoint), and a file that is missing,
    /// short or not a regular file cannot back the checkpoint at all, so the
    /// transfer restarts from zero. Returns the offset the receiver resumes at.
    fn reconcile_part(&self, part: &Path, offset: u64) -> Result<u64, String> {
        let length = match fs::symlink_metadata(part) {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            Ok(_) => {
                create_fresh_file(part)?;
                0
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                create_fresh_file(part)?;
                0
            }
            Err(error) => {
                return Err(format!("Could not inspect DeskLink partial file: {error}"));
            }
        };
        let keep = if length >= offset { offset } else { 0 };
        if length != keep {
            let file = OpenOptions::new()
                .write(true)
                .open(part)
                .map_err(|error| format!("Could not open DeskLink partial file: {error}"))?;
            file.set_len(keep)
                .and_then(|()| file.sync_all())
                .map_err(|error| format!("Could not repair DeskLink partial file: {error}"))?;
        }
        Ok(keep)
    }

    /// Verifies and publishes a fully received file. A file that fails
    /// verification can never become valid by waiting, so it is discarded and
    /// the sender is told with an `Error` control instead of being left to time
    /// out on a transfer that is stuck at 100%.
    fn finish_receive(
        &self,
        wire: &WebRtcWireBinding,
        checkpoint: &ReceiveCheckpoint,
        acknowledge: bool,
    ) -> Result<Vec<TransferEvent>, String> {
        if let Err(reason) = self.verify_part(checkpoint) {
            debug_log!(
                "[DeskLink] Discarding incoming transfer {}: {reason}",
                checkpoint.transfer_id
            );
            self.discard_receive(checkpoint)?;
            board::finish(&checkpoint.transfer_id, Outcome::Failed(reason.clone()));
            return Ok(vec![TransferEvent::SendControl(self.control(
                wire,
                FileAction::Error,
                checkpoint,
                checkpoint.offset,
                Some(reason),
            ))]);
        }
        let path = self.publish(checkpoint)?;
        let mut events = Vec::new();
        if acknowledge {
            events.push(TransferEvent::SendControl(self.control(
                wire,
                FileAction::Acknowledge,
                checkpoint,
                checkpoint.total_size,
                None,
            )));
        }
        events.push(TransferEvent::SendControl(self.control(
            wire,
            FileAction::Complete,
            checkpoint,
            checkpoint.total_size,
            None,
        )));
        events.push(TransferEvent::Completed {
            transfer_id: checkpoint.transfer_id.clone(),
            path,
        });
        Ok(events)
    }

    /// Feeds the running hash when the chunk continues it exactly; a gap (a
    /// resume) drops it, and verification then hashes the file once instead.
    fn hash_chunk(&self, transfer_id: &str, offset: u64, data: &[u8]) {
        let Ok(mut hashers) = self.hashers.lock() else {
            return;
        };
        if offset == 0 {
            hashers.insert(transfer_id.to_string(), (0, openssl::sha::Sha256::new()));
        }
        match hashers.get_mut(transfer_id) {
            Some((hashed, hasher)) if *hashed == offset => {
                hasher.update(data);
                *hashed += data.len() as u64;
            }
            Some(_) => {
                hashers.remove(transfer_id);
            }
            None => {}
        }
    }

    fn verify_part(&self, checkpoint: &ReceiveCheckpoint) -> Result<(), String> {
        let part = self.part_path(checkpoint)?;
        ensure_regular_file(&part)?;
        if fs::metadata(&part)
            .map_err(|error| error.to_string())?
            .len()
            != checkpoint.total_size
        {
            return Err("DeskLink completed file has the wrong size".to_string());
        }
        let running = self
            .hashers
            .lock()
            .ok()
            .and_then(|mut hashers| hashers.remove(&checkpoint.transfer_id))
            .filter(|(hashed, _)| *hashed == checkpoint.total_size)
            .map(|(_, hasher)| {
                hasher
                    .finish()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            });
        let actual = match running {
            Some(digest) => digest,
            None => sha256_file(&part)?,
        };
        if !actual.eq_ignore_ascii_case(&checkpoint.sha256) {
            return Err("DeskLink completed file checksum does not match the sender".to_string());
        }
        Ok(())
    }

    /// Moves the verified partial file into the downloads directory without
    /// ever replacing anything. `hard_link` fails if the name is taken (even by
    /// a dangling symlink), which `exists()` followed by `rename` cannot
    /// promise. The name is settled here, not at offer time, so two transfers
    /// of the same filename that were in flight together both succeed.
    fn publish(&self, checkpoint: &ReceiveCheckpoint) -> Result<PathBuf, String> {
        let part = self.part_path(checkpoint)?;
        ensure_directory_not_symlink(&self.destination_root)?;
        let mut name = checkpoint.destination_name.clone();
        for _ in 0..16 {
            let target = self.checked_destination_path(&name)?;
            match fs::hard_link(&part, &target) {
                Ok(()) => {
                    // The download is already published; a leftover part file
                    // is only clutter.
                    let _ = fs::remove_file(&part);
                    sync_directory(&self.destination_root)?;
                    self.remove_checkpoint(&checkpoint.transfer_id)?;
                    self.active
                        .lock()
                        .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
                        .remove(&checkpoint.transfer_id);
                    board::finish(&checkpoint.transfer_id, Outcome::Received(target.clone()));
                    return Ok(target);
                }
                Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                    name = self.available_destination_name(&checkpoint.filename)?;
                }
                Err(error) => {
                    return Err(format!(
                        "Could not atomically publish DeskLink download: {error}"
                    ));
                }
            }
        }
        Err("Could not allocate a collision-safe DeskLink download name".to_string())
    }

    /// Forgets an incoming transfer: memory, partial file and checkpoint.
    fn discard_receive(&self, checkpoint: &ReceiveCheckpoint) -> Result<(), String> {
        if let Ok(mut hashers) = self.hashers.lock() {
            hashers.remove(&checkpoint.transfer_id);
        }
        self.active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .remove(&checkpoint.transfer_id);
        remove_if_present(&self.part_path(checkpoint)?)?;
        self.remove_checkpoint(&checkpoint.transfer_id)
    }

    /// Handles a peer's `Cancel` or `Error` for either direction. Both the
    /// device and the transfer token are checked before anything is removed,
    /// so a forged or stale message changes nothing.
    fn cancel(
        &self,
        wire: &WebRtcWireBinding,
        transfer_id: &str,
        token: &str,
    ) -> Result<(), String> {
        let live = self
            .active
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(transfer_id)
            .cloned();
        let incoming = match live {
            Some(checkpoint) => Some(checkpoint),
            None => self.load_checkpoint(transfer_id)?,
        };
        let outgoing = self
            .outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .get(transfer_id)
            .cloned();
        let denied = incoming
            .as_ref()
            .is_some_and(|c| c.token != token || c.device_id != wire.peer_device_id)
            || outgoing
                .as_ref()
                .is_some_and(|c| c.token != token || c.device_id != wire.peer_device_id);
        if denied {
            return Err("DeskLink file cancellation has the wrong transfer token".to_string());
        }
        if let Some(checkpoint) = incoming {
            self.discard_receive(&checkpoint)?;
        }
        if let Some(checkpoint) = outgoing {
            self.outgoing
                .lock()
                .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
                .remove(transfer_id);
            remove_if_present(&self.checked_state_path(&checkpoint.source_name)?)?;
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
        checkpoint.chunk_size = message
            .max_chunk_size
            .map_or(LEGACY_FILE_CHUNK_BYTES, |size| {
                size.clamp(LEGACY_FILE_CHUNK_BYTES, MAX_FILE_CHUNK_BYTES)
            });
        self.save_send_checkpoint(&checkpoint)?;
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint.clone());
        board::progress(&checkpoint.transfer_id, checkpoint.offset);
        self.fill_send_window(checkpoint)
    }

    fn acknowledge_send(
        &self,
        wire: &WebRtcWireBinding,
        message: &FileControl,
    ) -> Result<Vec<TransferEvent>, String> {
        let mut checkpoint = self.take_sender(wire, message)?;
        // `awaiting_offset` comes from disk on a reload, so bound it as well:
        // an offset past the file would underflow the remaining-byte count.
        // Cumulative: any point between what was acknowledged and what was
        // sent. A receiver acknowledging every chunk and one acknowledging in
        // batches are both handled.
        let sent = checkpoint.awaiting_offset.unwrap_or(checkpoint.offset);
        if message.offset > sent || message.offset > checkpoint.total_size {
            return Err("DeskLink file acknowledgement does not match the sent chunk".to_string());
        }
        if message.offset <= checkpoint.offset {
            // Already covered by a later cumulative ack: nothing to do, and
            // certainly no reason to fail a healthy transfer.
            return Ok(Vec::new());
        }
        let previous = checkpoint.offset;
        checkpoint.offset = message.offset;
        // On resume the receiver's own offset decides, so the sender's disk
        // checkpoint only needs to be roughly current.
        if crosses_durability_boundary(
            previous,
            message.offset,
            checkpoint.total_size,
            self.durable_every,
        ) {
            self.save_send_checkpoint(&checkpoint)?;
        }
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint.clone());
        board::progress(&checkpoint.transfer_id, checkpoint.offset);
        self.fill_send_window(checkpoint)
    }

    /// Sends chunks until `SEND_WINDOW_BYTES` are unacknowledged or the file
    /// is exhausted.
    fn fill_send_window(
        &self,
        mut checkpoint: SendCheckpoint,
    ) -> Result<Vec<TransferEvent>, String> {
        let mut events = Vec::new();
        loop {
            let sent = checkpoint.awaiting_offset.unwrap_or(checkpoint.offset);
            if sent >= checkpoint.total_size || sent - checkpoint.offset >= SEND_WINDOW_BYTES {
                break;
            }
            let (event, next) = self.read_send_chunk(&checkpoint, sent)?;
            events.push(event);
            checkpoint.awaiting_offset = Some(next);
        }
        self.outgoing
            .lock()
            .map_err(|_| "DeskLink transfer manager lock poisoned".to_string())?
            .insert(checkpoint.transfer_id.clone(), checkpoint);
        Ok(events)
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
        remove_if_present(&self.checked_state_path(&checkpoint.source_name)?)?;
        self.remove_send_checkpoint(&checkpoint.transfer_id)?;
        board::finish(&checkpoint.transfer_id, Outcome::Sent);
        if let Some(source) = &checkpoint.source_path {
            crate::device_links::transfer_log::record_sent(
                &checkpoint.device_id,
                &checkpoint.filename,
                source,
                checkpoint.total_size,
            );
        }
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

    /// Reads and frames the chunk starting at `at`; returns it and the offset
    /// after it.
    fn read_send_chunk(
        &self,
        checkpoint: &SendCheckpoint,
        at: u64,
    ) -> Result<(TransferEvent, u64), String> {
        let source = self.checked_state_path(&checkpoint.source_name)?;
        ensure_regular_file(&source)?;
        if fs::metadata(&source)
            .map_err(|error| error.to_string())?
            .len()
            != checkpoint.total_size
        {
            return Err("DeskLink shared file snapshot changed during transfer".to_string());
        }
        let chunk_size = checkpoint
            .chunk_size
            .clamp(LEGACY_FILE_CHUNK_BYTES, MAX_FILE_CHUNK_BYTES) as u64;
        let count = usize::try_from((checkpoint.total_size - at).min(chunk_size))
            .map_err(|_| "Invalid DeskLink file chunk size".to_string())?;
        let mut data = vec![0; count];
        let mut source_file = File::open(&source).map_err(|error| error.to_string())?;
        source_file
            .seek(SeekFrom::Start(at))
            .map_err(|error| error.to_string())?;
        std::io::Read::read_exact(&mut source_file, &mut data)
            .map_err(|error| error.to_string())?;
        let encoded = FileChunk {
            transfer_id: checkpoint.transfer_id.clone(),
            transfer_token: checkpoint.token.clone(),
            offset: at,
            data,
        }
        .encode()?;
        let next = at + u64::try_from(count).map_err(|_| "Invalid chunk size".to_string())?;
        Ok((TransferEvent::SendChunk(encoded), next))
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
            chunk_size: LEGACY_FILE_CHUNK_BYTES,
            max_chunk_size: (action == FileAction::Offer).then_some(MAX_FILE_CHUNK_BYTES),
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
            chunk_size: LEGACY_FILE_CHUNK_BYTES,
            max_chunk_size: None,
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

    /// Reads a persisted checkpoint. A checkpoint that cannot be parsed, or
    /// whose fields no longer make sense for `transfer_id`, is treated as if it
    /// did not exist: it is removed together with the partial file that nothing
    /// can vouch for any more, instead of wedging the transfer id forever.
    fn load_checkpoint(&self, transfer_id: &str) -> Result<Option<ReceiveCheckpoint>, String> {
        let path = self.checkpoint_path(transfer_id)?;
        let bytes = match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_file() => fs::read(&path)
                .map_err(|error| format!("Could not read DeskLink transfer checkpoint: {error}"))?,
            // A link or directory is not a checkpoint we wrote.
            Ok(_) => Vec::new(),
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(format!(
                    "Could not inspect DeskLink transfer checkpoint: {error}"
                ));
            }
        };
        match serde_json::from_slice::<ReceiveCheckpoint>(&bytes) {
            Ok(checkpoint) if checkpoint.is_consistent(transfer_id) => Ok(Some(checkpoint)),
            _ => {
                remove_if_present(&path)?;
                remove_if_present(&self.checked_state_path(&part_name(transfer_id))?)?;
                Ok(None)
            }
        }
    }

    fn save_checkpoint(&self, checkpoint: &ReceiveCheckpoint) -> Result<(), String> {
        let path = self.checkpoint_path(&checkpoint.transfer_id)?;
        let temporary =
            self.checked_state_path(&format!(".desklink-{}.json.tmp", checkpoint.transfer_id))?;
        let bytes = serde_json::to_vec(checkpoint).map_err(|error| {
            format!("Could not serialize DeskLink transfer checkpoint: {error}")
        })?;
        let mut file = create_fresh_file(&temporary)?;
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
        remove_if_present(&self.checkpoint_path(transfer_id)?)?;
        sync_directory(&self.state_root)
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
        let mut file = create_fresh_file(&temporary)?;
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
        remove_if_present(&self.send_checkpoint_path(transfer_id)?)?;
        sync_directory(&self.state_root)
    }

    /// A name in the downloads directory that nothing occupies. Occupied means
    /// "any directory entry", so a dangling symlink counts, and a suffix never
    /// pushes the name past the 255-byte filesystem limit.
    fn available_destination_name(&self, filename: &str) -> Result<String, String> {
        let taken = |name: &str| -> Result<bool, String> {
            Ok(fs::symlink_metadata(self.checked_destination_path(name)?).is_ok())
        };
        if !taken(filename)? {
            return Ok(filename.to_string());
        }
        let source = Path::new(filename);
        // A very long "extension" is just part of the name; keeping it would
        // leave no room for the suffix.
        let extension = source
            .extension()
            .and_then(|value| value.to_str())
            .filter(|value| !value.is_empty() && value.len() <= 32);
        let stem = match extension {
            Some(_) => source
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("file"),
            None => filename,
        };
        for number in 1..10_000u32 {
            let suffix = match extension {
                Some(extension) => format!("_{number}.{extension}"),
                None => format!("_{number}"),
            };
            let name = format!("{}{suffix}", truncate_to_boundary(stem, 255 - suffix.len()));
            if !taken(&name)? {
                return Ok(name);
            }
        }
        Err("Could not allocate a collision-safe DeskLink download name".to_string())
    }
}

/// The longest prefix of `value` that is at most `max` bytes and ends on a
/// character boundary.
fn truncate_to_boundary(value: &str, max: usize) -> &str {
    let mut end = max.min(value.len());
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    &value[..end]
}

/// Copies `input` into the snapshot, bounded by `MAX_TRANSFER_BYTES`, and
/// returns its length and SHA-256.
fn copy_snapshot(input: &mut impl Read, output: &mut File) -> Result<(u64, String), String> {
    let mut bytes = 0u64;
    let mut hasher = openssl::sha::Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = input
            .read(&mut buffer)
            .map_err(|error| format!("Could not read DeskLink shared file: {error}"))?;
        if count == 0 {
            break;
        }
        bytes = bytes
            .checked_add(u64::try_from(count).map_err(|_| "Invalid file size".to_string())?)
            .filter(|total| *total <= MAX_TRANSFER_BYTES)
            .ok_or_else(|| "DeskLink shared file exceeds the 4 GiB transfer limit".to_string())?;
        hasher.update(&buffer[..count]);
        output
            .write_all(&buffer[..count])
            .map_err(|error| format!("Could not snapshot DeskLink shared file: {error}"))?;
    }
    output
        .sync_all()
        .map_err(|error| format!("Could not persist DeskLink shared file snapshot: {error}"))?;
    let digest = hasher
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok((bytes, digest))
}

fn checked_child(root: &Path, name: &str) -> Result<PathBuf, String> {
    // `.` and `..` are single path components that still resolve outside a
    // plain child, so `components().count()` alone does not exclude them.
    if name.is_empty()
        || matches!(name, "." | "..")
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

/// Unlinks `path` if something is there. `remove_file` removes a symlink
/// itself and never follows it, so this cannot touch a link's target.
fn remove_if_present(path: &Path) -> Result<(), String> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("Could not remove DeskLink transfer file: {error}")),
    }
}

/// Creates an empty file at `path`, replacing whatever entry was there. The
/// old entry is unlinked and the new file created with `O_EXCL`, which refuses
/// to open through a symlink, so a planted link is never written through.
fn create_fresh_file(path: &Path) -> Result<File, String> {
    remove_if_present(path)?;
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| format!("Could not create DeskLink transfer file: {error}"))
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

/// What to show for a peer's `Error`. It is the other device's text, so it is
/// bounded and stripped of control characters before it reaches a window.
fn peer_reason(error: Option<&str>) -> String {
    let text: String = error
        .unwrap_or_default()
        .chars()
        .filter(|character| !character.is_control())
        .take(256)
        .collect();
    if text.trim().is_empty() {
        "The other device reported an error".to_string()
    } else {
        text
    }
}

fn sync_directory(path: &Path) -> Result<(), String> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| format!("Could not persist DeskLink transfer directory: {error}"))
}

/// Hashes a completed download in bounded chunks.
///
/// Transfers are allowed up to `MAX_TRANSFER_BYTES` (4 GiB), so reading the
/// whole file in to verify it undid the streaming the rest of this module is
/// built around and allocated the entire transfer at the last step.
fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file =
        File::open(path).map_err(|error| format!("Could not hash DeskLink file: {error}"))?;
    let mut hasher = openssl::sha::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let count = std::io::Read::read(&mut file, &mut buffer)
            .map_err(|error| format!("Could not hash DeskLink file: {error}"))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
            chunk_size: LEGACY_FILE_CHUNK_BYTES,
            max_chunk_size: None,
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
        error_control(
            manager
                .handle_chunk(&wire(), &chunk.encode().unwrap())
                .unwrap(),
        );
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

    // ---- hardening tests -------------------------------------------------
    //
    // The peer is authenticated but its bytes are still untrusted input that
    // ends up on the user's disk, so these pin the failure paths as tightly as
    // the happy path above.

    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    static SANDBOXES: AtomicU64 = AtomicU64::new(0);

    /// `base/state` and `base/downloads`, removed on drop. Keeping both under
    /// one `base` lets a test assert that nothing ever appears beside them.
    struct Sandbox {
        base: PathBuf,
    }

    impl Sandbox {
        fn new(label: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "desklink-hardening-{label}-{}-{}",
                std::process::id(),
                SANDBOXES.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&base).unwrap();
            Self { base }
        }

        fn state(&self) -> PathBuf {
            self.base.join("state")
        }

        fn downloads(&self) -> PathBuf {
            self.base.join("downloads")
        }

        fn manager(&self) -> WebRtcTransferManager {
            WebRtcTransferManager::new(self.state(), self.downloads())
                .unwrap()
                .durable_every_chunk()
        }

        fn base_entries(&self) -> Vec<String> {
            names(&self.base)
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.base);
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut found: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        found.sort();
        found
    }

    /// Name -> contents (or link target) for everything directly in `dir`.
    fn snapshot(dir: &Path) -> BTreeMap<String, Vec<u8>> {
        names(dir)
            .into_iter()
            .map(|name| {
                let path = dir.join(&name);
                let value = if fs::symlink_metadata(&path)
                    .unwrap()
                    .file_type()
                    .is_symlink()
                {
                    fs::read_link(&path)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned()
                        .into_bytes()
                } else {
                    fs::read(&path).unwrap()
                };
                (name, value)
            })
            .collect()
    }

    fn sha256_hex(data: &[u8]) -> String {
        openssl::hash::hash(openssl::hash::MessageDigest::sha256(), data)
            .unwrap()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn payload(length: usize) -> Vec<u8> {
        (0..length).map(|index| (index % 251) as u8).collect()
    }

    fn wire_of(session_id: u64, generation: u64) -> WebRtcWireBinding {
        WebRtcWireBinding {
            session_id,
            generation,
            ..wire()
        }
    }

    fn offer_for(id: &str, filename: &str, data: &[u8]) -> FileControl {
        FileControl {
            transfer_id: id.to_string(),
            filename: Some(filename.to_string()),
            total_size: Some(data.len() as u64),
            sha256: Some(sha256_hex(data)),
            ..offer()
        }
    }

    /// A control record the peer would send back for an existing transfer. It
    /// names the peer as its device, whichever side made the original offer.
    fn reply(offer: &FileControl, action: FileAction) -> FileControl {
        FileControl {
            action,
            device_id: wire().peer_device_id,
            filename: None,
            total_size: None,
            sha256: None,
            ..offer.clone()
        }
    }

    fn chunk_bytes(id: &str, token: &str, offset: u64, data: &[u8]) -> Vec<u8> {
        FileChunk {
            transfer_id: id.to_string(),
            transfer_token: token.to_string(),
            offset,
            data: data.to_vec(),
        }
        .encode()
        .unwrap()
    }

    /// An encoded chunk that skips the encoder's own validation, so a test can
    /// present what a hostile peer could put on the wire.
    fn raw_chunk(id: &str, token: &str, offset: u64, data: &[u8]) -> Vec<u8> {
        let digest = openssl::hash::hash(openssl::hash::MessageDigest::sha256(), data).unwrap();
        let mut out = b"DLF1".to_vec();
        out.extend((id.len() as u16).to_be_bytes());
        out.extend((token.len() as u16).to_be_bytes());
        out.extend(offset.to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        out.extend_from_slice(&digest);
        out.extend_from_slice(id.as_bytes());
        out.extend_from_slice(token.as_bytes());
        out.extend_from_slice(data);
        out
    }

    fn feed(
        manager: &WebRtcTransferManager,
        wire: &WebRtcWireBinding,
        id: &str,
        offset: u64,
        data: &[u8],
    ) -> Result<Vec<TransferEvent>, String> {
        manager.handle_chunk(wire, &chunk_bytes(id, "token", offset, data))
    }

    fn only_control(events: Vec<TransferEvent>) -> FileControl {
        match events.as_slice() {
            [TransferEvent::SendControl(control)] => control.clone(),
            other => panic!("expected exactly one control record, got {other:?}"),
        }
    }

    fn completed_path(events: &[TransferEvent]) -> Option<PathBuf> {
        events.iter().find_map(|event| match event {
            TransferEvent::Completed { path, .. } => Some(path.clone()),
            _ => None,
        })
    }

    /// Offers `data` and streams every chunk from the accepted offset,
    /// returning the published path.
    fn upload(
        manager: &WebRtcTransferManager,
        wire: &WebRtcWireBinding,
        offer: &FileControl,
        data: &[u8],
    ) -> PathBuf {
        let accept = only_control(manager.handle_control(wire, offer.clone()).unwrap());
        assert_eq!(accept.action, FileAction::Accept);
        let mut offset = accept.offset as usize;
        let mut published = None;
        while offset < data.len() {
            let end = (offset + MAX_FILE_CHUNK_BYTES).min(data.len());
            let events = feed(
                manager,
                wire,
                &offer.transfer_id,
                offset as u64,
                &data[offset..end],
            )
            .unwrap();
            published = published.or_else(|| completed_path(&events));
            offset = end;
        }
        published.expect("the transfer should have been published")
    }

    fn active_ids(manager: &WebRtcTransferManager) -> Vec<String> {
        let mut ids: Vec<String> = manager.active.lock().unwrap().keys().cloned().collect();
        ids.sort();
        ids
    }

    fn part_of(sandbox: &Sandbox, id: &str) -> PathBuf {
        sandbox.state().join(format!(".desklink-{id}.part"))
    }

    fn checkpoint_of(sandbox: &Sandbox, id: &str) -> PathBuf {
        sandbox.state().join(format!(".desklink-{id}.json"))
    }

    #[test]
    fn resumes_after_a_restart_at_the_verified_offset_on_a_new_session() {
        let sandbox = Sandbox::new("resume");
        let data = payload(40_000);
        let offer = offer_for("transfer_r", "big.bin", &data);

        let first = sandbox.manager();
        first.handle_control(&wire(), offer.clone()).unwrap();
        feed(&first, &wire(), "transfer_r", 0, &data[..16_384]).unwrap();
        feed(&first, &wire(), "transfer_r", 16_384, &data[16_384..32_768]).unwrap();
        drop(first);

        // A restarted daemon negotiates a brand new attempt, so both the
        // session id and the generation differ from the persisted checkpoint.
        let resumed = wire_of(77, 1);
        let mut again = offer.clone();
        again.session_id = 77;
        again.connection_generation = 1;
        let second = sandbox.manager();
        let accept = only_control(second.handle_control(&resumed, again).unwrap());
        assert_eq!(accept.action, FileAction::Accept);
        assert_eq!(accept.offset, 32_768);

        // The old session's binding must no longer be able to feed the transfer.
        assert!(feed(&second, &wire(), "transfer_r", 32_768, &data[32_768..]).is_err());

        let events = feed(&second, &resumed, "transfer_r", 32_768, &data[32_768..]).unwrap();
        let path = completed_path(&events).expect("resumed transfer publishes");
        assert_eq!(path, sandbox.downloads().join("big.bin"));
        assert_eq!(fs::read(path).unwrap(), data);
        assert!(names(&sandbox.state()).is_empty());
    }

    #[test]
    fn a_different_device_cannot_resume_someone_elses_checkpoint() {
        let sandbox = Sandbox::new("resume-device");
        let data = payload(20_000);
        let offer = offer_for("transfer_d", "big.bin", &data);
        let first = sandbox.manager();
        first.handle_control(&wire(), offer.clone()).unwrap();
        feed(&first, &wire(), "transfer_d", 0, &data[..16_384]).unwrap();
        drop(first);

        let stranger = WebRtcWireBinding {
            peer_device_id: "stranger".to_string(),
            ..wire_of(77, 1)
        };
        let mut hijack = offer;
        hijack.device_id = "stranger".to_string();
        hijack.session_id = 77;
        hijack.connection_generation = 1;
        let before = snapshot(&sandbox.state());
        assert!(sandbox.manager().handle_control(&stranger, hijack).is_err());
        assert_eq!(snapshot(&sandbox.state()), before);
    }

    #[test]
    fn chunks_with_a_wrong_binding_or_token_are_rejected_without_touching_state() {
        let sandbox = Sandbox::new("chunk-binding");
        let data = payload(40_000);
        let manager = sandbox.manager();
        let offer = offer_for("transfer_1", "big.bin", &data);
        manager.handle_control(&wire(), offer.clone()).unwrap();
        feed(&manager, &wire(), "transfer_1", 0, &data[..16_384]).unwrap();
        let before = snapshot(&sandbox.state());

        let variants = [
            WebRtcWireBinding {
                peer_device_id: "other-phone".to_string(),
                ..wire()
            },
            wire_of(5, 2),
            wire_of(4, 3),
        ];
        for stale in &variants {
            assert!(
                feed(&manager, stale, "transfer_1", 16_384, &data[16_384..32_768]).is_err(),
                "{stale:?} must not feed the transfer"
            );
        }
        let wrong_token = chunk_bytes("transfer_1", "not-the-token", 16_384, &data[16_384..32_768]);
        assert!(manager.handle_chunk(&wire(), &wrong_token).is_err());
        assert_eq!(snapshot(&sandbox.state()), before);

        // The genuine peer is unaffected by the rejected attempts.
        let mut offset = 16_384;
        let mut published = None;
        while offset < data.len() {
            let end = (offset + MAX_FILE_CHUNK_BYTES).min(data.len());
            let events = feed(
                &manager,
                &wire(),
                "transfer_1",
                offset as u64,
                &data[offset..end],
            )
            .unwrap();
            published = published.or_else(|| completed_path(&events));
            offset = end;
        }
        assert_eq!(fs::read(published.unwrap()).unwrap(), data);
    }

    #[test]
    fn control_records_with_a_wrong_binding_or_token_are_rejected_without_touching_state() {
        let sandbox = Sandbox::new("control-binding");
        let data = payload(40_000);
        let manager = sandbox.manager();
        let offer = offer_for("transfer_1", "big.bin", &data);
        manager.handle_control(&wire(), offer.clone()).unwrap();
        feed(&manager, &wire(), "transfer_1", 0, &data[..16_384]).unwrap();
        let before = snapshot(&sandbox.state());

        for action in [
            FileAction::Offer,
            FileAction::Cancel,
            FileAction::Error,
            FileAction::Accept,
            FileAction::Acknowledge,
            FileAction::Complete,
        ] {
            let base = if action == FileAction::Offer {
                offer.clone()
            } else {
                reply(&offer, action)
            };
            let mut tampered = Vec::new();
            for mutate in [
                (|c: &mut FileControl| c.device_id = "other-phone".to_string())
                    as fn(&mut FileControl),
                |c| c.session_id += 1,
                |c| c.connection_generation += 1,
                |c| c.transfer_token = "not-the-token".to_string(),
            ] {
                let mut control = base.clone();
                mutate(&mut control);
                tampered.push(control);
            }
            for control in tampered {
                assert!(
                    manager.handle_control(&wire(), control.clone()).is_err(),
                    "{action:?} {control:?} must be rejected"
                );
                assert_eq!(
                    snapshot(&sandbox.state()),
                    before,
                    "{action:?} mutated state"
                );
                assert_eq!(
                    active_ids(&manager),
                    vec!["transfer_1".to_string()],
                    "{action:?} dropped the live transfer"
                );
            }
        }
    }

    #[test]
    fn a_final_checksum_mismatch_never_publishes_and_cleans_up() {
        let sandbox = Sandbox::new("checksum");
        let manager = sandbox.manager();
        // The offer promises "abc"; the peer delivers "abd".
        manager.handle_control(&wire(), offer()).unwrap();
        let events = feed(&manager, &wire(), "transfer_1", 0, b"abd").unwrap();
        let error = only_control(events);
        assert_eq!(error.action, FileAction::Error);
        assert!(error.error.is_some());

        assert!(names(&sandbox.downloads()).is_empty());
        assert!(
            names(&sandbox.state()).is_empty(),
            "partial or checkpoint left behind"
        );
        assert!(active_ids(&manager).is_empty());
        assert!(feed(&manager, &wire(), "transfer_1", 0, b"abc").is_err());

        // The id is not poisoned: a fresh offer starts again from zero.
        let path = upload(&manager, &wire(), &offer(), b"abc");
        assert_eq!(fs::read(path).unwrap(), b"abc");
    }

    #[test]
    fn hostile_filenames_are_refused_and_nothing_escapes_the_sandbox() {
        let sandbox = Sandbox::new("hostile");
        let manager = sandbox.manager();
        let too_long = "a".repeat(256);
        let hostile = [
            "../x",
            "../../etc/passwd",
            "/etc/passwd",
            "/",
            "a\0b",
            ".",
            "..",
            "dir/file",
            "dir\\file",
            "..\\..\\x",
            "C:\\Windows\\x",
            "x/",
            "\\",
            "",
            too_long.as_str(),
        ];
        for name in hostile {
            assert!(
                manager
                    .handle_control(&wire(), offer_for("transfer_1", name, b"abc"))
                    .is_err(),
                "{name:?} must be refused"
            );
        }
        assert_eq!(sandbox.base_entries(), ["downloads", "state"]);
        assert!(names(&sandbox.downloads()).is_empty());
        assert!(names(&sandbox.state()).is_empty());
    }

    #[test]
    fn unusual_but_harmless_filenames_land_directly_inside_downloads() {
        let sandbox = Sandbox::new("odd-names");
        let manager = sandbox.manager();
        let longest = "b".repeat(255);
        for (index, name) in [
            "...",
            "..a",
            "a..",
            ".hidden",
            "-rf",
            "a b",
            "ünï-çødé.txt",
            ".desklink-transfer_1.part",
            longest.as_str(),
        ]
        .into_iter()
        .enumerate()
        {
            let id = format!("odd_{index}");
            let path = upload(&manager, &wire(), &offer_for(&id, name, b"abc"), b"abc");
            assert_eq!(path, sandbox.downloads().join(name), "{name:?}");
            assert_eq!(fs::read(&path).unwrap(), b"abc");
        }
        assert_eq!(sandbox.base_entries(), ["downloads", "state"]);
        assert!(names(&sandbox.state()).is_empty());
    }

    #[test]
    fn checked_child_only_ever_returns_a_direct_child() {
        let root = Path::new("/tmp/desklink-root");
        for name in ["", ".", "..", "a/b", "/a", "a\\b", "a\0b", "../a"] {
            assert!(checked_child(root, name).is_err(), "{name:?}");
        }
        assert_eq!(checked_child(root, "...").unwrap(), root.join("..."));
        assert_eq!(checked_child(root, "file").unwrap(), root.join("file"));
    }

    #[test]
    fn downloads_never_overwrite_and_collisions_get_a_suffix() {
        let sandbox = Sandbox::new("collide");
        let manager = sandbox.manager();
        fs::write(sandbox.downloads().join("report.txt"), b"precious").unwrap();
        fs::write(sandbox.downloads().join("plain"), b"precious").unwrap();

        let first = upload(
            &manager,
            &wire(),
            &offer_for("c_1", "report.txt", b"abc"),
            b"abc",
        );
        let second = upload(
            &manager,
            &wire(),
            &offer_for("c_2", "report.txt", b"xyz"),
            b"xyz",
        );
        let bare = upload(
            &manager,
            &wire(),
            &offer_for("c_3", "plain", b"abc"),
            b"abc",
        );
        assert_eq!(first, sandbox.downloads().join("report_1.txt"));
        assert_eq!(second, sandbox.downloads().join("report_2.txt"));
        assert_eq!(bare, sandbox.downloads().join("plain_1"));
        assert_eq!(fs::read(&first).unwrap(), b"abc");
        assert_eq!(fs::read(&second).unwrap(), b"xyz");
        assert_eq!(
            fs::read(sandbox.downloads().join("report.txt")).unwrap(),
            b"precious"
        );
        assert_eq!(
            fs::read(sandbox.downloads().join("plain")).unwrap(),
            b"precious"
        );
    }

    #[test]
    fn two_in_flight_transfers_with_the_same_name_both_survive() {
        let sandbox = Sandbox::new("same-name");
        let manager = sandbox.manager();
        // Both offers are accepted before either finishes, so the name cannot
        // be reserved at offer time without one of them failing at the end.
        let first = offer_for("same_1", "photo.jpg", b"abc");
        let second = offer_for("same_2", "photo.jpg", b"xyz");
        manager.handle_control(&wire(), first).unwrap();
        manager.handle_control(&wire(), second).unwrap();
        let one = completed_path(&feed(&manager, &wire(), "same_1", 0, b"abc").unwrap()).unwrap();
        let two = completed_path(&feed(&manager, &wire(), "same_2", 0, b"xyz").unwrap()).unwrap();
        assert_ne!(one, two);
        assert_eq!(fs::read(one).unwrap(), b"abc");
        assert_eq!(fs::read(two).unwrap(), b"xyz");
        assert!(names(&sandbox.state()).is_empty());
    }

    #[test]
    fn a_collision_suffix_never_pushes_the_name_past_the_filesystem_limit() {
        let sandbox = Sandbox::new("long-collision");
        let manager = sandbox.manager();
        let name = format!("{}.txt", "a".repeat(251));
        assert_eq!(name.len(), 255);
        fs::write(sandbox.downloads().join(&name), b"precious").unwrap();
        let path = upload(
            &manager,
            &wire(),
            &offer_for("long_1", &name, b"abc"),
            b"abc",
        );
        assert_ne!(path, sandbox.downloads().join(&name));
        assert!(path.file_name().unwrap().len() <= 255);
        assert_eq!(fs::read(path).unwrap(), b"abc");
        assert_eq!(
            fs::read(sandbox.downloads().join(&name)).unwrap(),
            b"precious"
        );
    }

    #[test]
    fn a_symlink_at_the_destination_is_never_followed_or_replaced() {
        let sandbox = Sandbox::new("dest-symlink");
        let victim = sandbox.base.join("victim.txt");
        fs::write(&victim, b"do not touch").unwrap();
        let manager = sandbox.manager();

        // A link to an existing file...
        symlink(&victim, sandbox.downloads().join("live.txt")).unwrap();
        // ...and a dangling one, which `Path::exists` reports as absent.
        let missing = sandbox.base.join("does-not-exist");
        symlink(&missing, sandbox.downloads().join("dangling.txt")).unwrap();

        let live = upload(
            &manager,
            &wire(),
            &offer_for("s_1", "live.txt", b"abc"),
            b"abc",
        );
        let dangling = upload(
            &manager,
            &wire(),
            &offer_for("s_2", "dangling.txt", b"abc"),
            b"abc",
        );
        assert_ne!(live, sandbox.downloads().join("live.txt"));
        assert_ne!(dangling, sandbox.downloads().join("dangling.txt"));
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
        assert!(!missing.exists(), "the dangling link target was created");
        for link in ["live.txt", "dangling.txt"] {
            assert!(fs::symlink_metadata(sandbox.downloads().join(link))
                .unwrap()
                .file_type()
                .is_symlink());
        }
        assert_eq!(fs::read(live).unwrap(), b"abc");
        assert_eq!(fs::read(dangling).unwrap(), b"abc");
    }

    #[test]
    fn a_symlink_planted_at_a_temporary_path_is_never_written_through() {
        let sandbox = Sandbox::new("temp-symlink");
        let victim = sandbox.base.join("victim.txt");
        fs::write(&victim, b"do not touch").unwrap();
        let manager = sandbox.manager();

        // Every path the manager writes for transfer_1, pre-planted.
        for suffix in ["part", "json", "json.tmp"] {
            symlink(
                &victim,
                sandbox
                    .state()
                    .join(format!(".desklink-transfer_1.{suffix}")),
            )
            .unwrap();
        }
        let _ = manager.handle_control(&wire(), offer());
        let _ = feed(&manager, &wire(), "transfer_1", 0, b"abc");
        assert_eq!(fs::read(&victim).unwrap(), b"do not touch");
    }

    #[test]
    fn a_symlink_swapped_in_for_the_partial_file_is_refused() {
        let sandbox = Sandbox::new("part-swap");
        let victim = sandbox.base.join("victim.txt");
        fs::write(&victim, b"").unwrap();
        let manager = sandbox.manager();
        manager.handle_control(&wire(), offer()).unwrap();
        let part = part_of(&sandbox, "transfer_1");
        fs::remove_file(&part).unwrap();
        symlink(&victim, &part).unwrap();
        error_control(feed(&manager, &wire(), "transfer_1", 0, b"abc").unwrap());
        assert_eq!(fs::read(&victim).unwrap(), b"");
        assert!(names(&sandbox.downloads()).is_empty());
    }

    #[test]
    fn symlinked_state_or_download_directories_are_refused() {
        let sandbox = Sandbox::new("dir-symlink");
        let real = sandbox.base.join("real");
        fs::create_dir_all(&real).unwrap();
        let link = sandbox.base.join("link");
        symlink(&real, &link).unwrap();
        assert!(WebRtcTransferManager::new(link.clone(), sandbox.downloads()).is_err());
        assert!(WebRtcTransferManager::new(sandbox.state(), link).is_err());
    }

    #[test]
    fn cancel_and_peer_error_remove_the_transfer_and_reject_later_chunks() {
        for action in [FileAction::Cancel, FileAction::Error] {
            let sandbox = Sandbox::new("cancel");
            let data = payload(40_000);
            let offer = offer_for("transfer_1", "big.bin", &data);
            let manager = sandbox.manager();
            manager.handle_control(&wire(), offer.clone()).unwrap();
            feed(&manager, &wire(), "transfer_1", 0, &data[..16_384]).unwrap();
            assert!(part_of(&sandbox, "transfer_1").exists());

            let events = manager
                .handle_control(&wire(), reply(&offer, action))
                .unwrap();
            assert!(events.is_empty(), "{action:?}");
            assert!(
                names(&sandbox.state()).is_empty(),
                "{action:?} left state behind"
            );
            assert!(active_ids(&manager).is_empty());
            assert!(names(&sandbox.downloads()).is_empty());
            assert!(
                feed(
                    &manager,
                    &wire(),
                    "transfer_1",
                    16_384,
                    &data[16_384..32_768]
                )
                .is_err(),
                "{action:?}: a later chunk was accepted"
            );

            // Nothing is resumable afterwards, even from a restarted manager.
            let restarted = sandbox.manager();
            let accept = only_control(restarted.handle_control(&wire(), offer).unwrap());
            assert_eq!(accept.offset, 0, "{action:?}");
        }
    }

    #[test]
    fn cancelling_an_unknown_transfer_is_harmless() {
        let sandbox = Sandbox::new("cancel-unknown");
        let manager = sandbox.manager();
        let events = manager
            .handle_control(&wire(), reply(&offer(), FileAction::Cancel))
            .unwrap();
        assert!(events.is_empty());
        assert!(names(&sandbox.state()).is_empty());
    }

    #[test]
    fn declared_sizes_are_bounded_and_enforced() {
        let sandbox = Sandbox::new("sizes");
        let manager = sandbox.manager();

        let mut too_big = offer();
        too_big.total_size = Some(MAX_TRANSFER_BYTES + 1);
        assert!(manager.handle_control(&wire(), too_big).is_err());
        assert!(names(&sandbox.state()).is_empty());

        // The limit itself is fine, and nothing is preallocated for it.
        let mut at_limit = offer();
        at_limit.total_size = Some(MAX_TRANSFER_BYTES);
        manager.handle_control(&wire(), at_limit.clone()).unwrap();
        assert_eq!(
            fs::metadata(part_of(&sandbox, "transfer_1")).unwrap().len(),
            0
        );
        manager
            .handle_control(&wire(), reply(&at_limit, FileAction::Cancel))
            .unwrap();

        for chunk_size in [0, MAX_FILE_CHUNK_BYTES + 1] {
            let mut control = offer();
            control.chunk_size = chunk_size;
            assert!(manager.handle_control(&wire(), control).is_err());
        }
        assert!(names(&sandbox.state()).is_empty());

        // "abc" is announced. Going over, in one chunk or across two, ends the
        // transfer and tells the sender; nothing is kept.
        manager.handle_control(&wire(), offer()).unwrap();
        error_control(feed(&manager, &wire(), "transfer_1", 0, b"abcd").unwrap());
        assert!(names(&sandbox.state()).is_empty());
        manager.handle_control(&wire(), offer()).unwrap();
        feed(&manager, &wire(), "transfer_1", 0, b"ab").unwrap();
        error_control(feed(&manager, &wire(), "transfer_1", 2, b"cd").unwrap());
        assert!(names(&sandbox.state()).is_empty());

        // A chunk above the wire limit does not even decode, so it names no
        // transfer: it is refused before it reaches the disk and changes nothing.
        manager.handle_control(&wire(), offer()).unwrap();
        feed(&manager, &wire(), "transfer_1", 0, b"ab").unwrap();
        let before = snapshot(&sandbox.state());
        let oversized = raw_chunk(
            "transfer_1",
            "token",
            2,
            &vec![7u8; MAX_FILE_CHUNK_BYTES + 1],
        );
        assert!(manager.handle_chunk(&wire(), &oversized).is_err());
        assert_eq!(snapshot(&sandbox.state()), before);

        // Finishing normally, then trying to keep writing.
        let events = feed(&manager, &wire(), "transfer_1", 2, b"c").unwrap();
        assert!(completed_path(&events).is_some());
        assert!(feed(&manager, &wire(), "transfer_1", 3, b"d").is_err());
        assert_eq!(
            fs::read(sandbox.downloads().join("report.txt")).unwrap(),
            b"abc"
        );
    }

    #[test]
    fn a_replayed_offer_keeps_progress_and_never_starts_a_second_transfer() {
        let sandbox = Sandbox::new("replay");
        let data = payload(40_000);
        let offer = offer_for("transfer_1", "big.bin", &data);
        let manager = sandbox.manager();
        manager.handle_control(&wire(), offer.clone()).unwrap();
        feed(&manager, &wire(), "transfer_1", 0, &data[..16_384]).unwrap();
        let before = snapshot(&sandbox.state());

        let accept = only_control(manager.handle_control(&wire(), offer.clone()).unwrap());
        assert_eq!(accept.action, FileAction::Accept);
        assert_eq!(accept.offset, 16_384);
        assert_eq!(snapshot(&sandbox.state()), before);
        assert_eq!(active_ids(&manager), ["transfer_1"]);

        // An offer that disagrees with the stored one is refused outright.
        let disagreements: [fn(&mut FileControl); 4] = [
            |c| c.filename = Some("other.bin".to_string()),
            |c| c.total_size = Some(40_001),
            |c| c.sha256 = Some("f".repeat(64)),
            |c| c.transfer_token = "another-token".to_string(),
        ];
        for mutate in disagreements {
            let mut changed = offer.clone();
            mutate(&mut changed);
            assert!(manager.handle_control(&wire(), changed).is_err());
            assert_eq!(snapshot(&sandbox.state()), before);
            assert_eq!(active_ids(&manager), ["transfer_1"]);
        }

        let mut published = None;
        let mut offset = 16_384;
        while offset < data.len() {
            let end = (offset + MAX_FILE_CHUNK_BYTES).min(data.len());
            let events = feed(
                &manager,
                &wire(),
                "transfer_1",
                offset as u64,
                &data[offset..end],
            )
            .unwrap();
            published = published.or_else(|| completed_path(&events));
            offset = end;
        }
        assert_eq!(fs::read(published.unwrap()).unwrap(), data);
        assert_eq!(names(&sandbox.downloads()), ["big.bin"]);
    }

    #[test]
    fn corrupt_or_truncated_checkpoints_are_treated_as_no_checkpoint() {
        let junk: [&[u8]; 6] = [
            b"",
            b"{",
            b"not json at all",
            b"{\"transfer_id\":\"transfer_1\"}",
            b"\xff\xfe\x00\x01",
            b"[]",
        ];
        for bytes in junk {
            let sandbox = Sandbox::new("junk");
            let manager = sandbox.manager();
            fs::write(checkpoint_of(&sandbox, "transfer_1"), bytes).unwrap();
            fs::write(part_of(&sandbox, "transfer_1"), b"stale bytes").unwrap();

            // Nothing can ride on a checkpoint that cannot be trusted.
            assert!(feed(&manager, &wire(), "transfer_1", 0, b"abc").is_err());

            // A fresh offer for the same id starts from zero and completes.
            let path = upload(&manager, &wire(), &offer(), b"abc");
            assert_eq!(fs::read(path).unwrap(), b"abc", "{bytes:?}");
            assert!(names(&sandbox.state()).is_empty(), "{bytes:?}");
        }
    }

    #[test]
    fn a_checkpoint_that_parses_but_makes_no_sense_is_not_trusted() {
        let corruptions: [(&str, serde_json::Value); 7] = [
            ("offset", serde_json::json!(4)),
            ("total_size", serde_json::json!(MAX_TRANSFER_BYTES + 1)),
            ("part_name", serde_json::json!("../evil")),
            (
                "part_name",
                serde_json::json!(".desklink-someone_else.part"),
            ),
            ("transfer_id", serde_json::json!("someone_else")),
            ("destination_name", serde_json::json!("..")),
            ("sha256", serde_json::json!("not-a-digest")),
        ];
        for (field, value) in corruptions {
            let sandbox = Sandbox::new("insane");
            let first = sandbox.manager();
            first.handle_control(&wire(), offer()).unwrap();
            drop(first);
            let path = checkpoint_of(&sandbox, "transfer_1");
            let mut stored: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            stored[field] = value;
            fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

            let manager = sandbox.manager();
            let published = upload(&manager, &wire(), &offer(), b"abc");
            assert_eq!(published, sandbox.downloads().join("report.txt"), "{field}");
            assert_eq!(fs::read(published).unwrap(), b"abc", "{field}");
            assert_eq!(sandbox.base_entries(), ["downloads", "state"], "{field}");
        }
    }

    #[test]
    fn resume_trusts_the_checkpoint_over_a_partial_file_that_disagrees() {
        let data = payload(40_000);
        let offer = offer_for("transfer_1", "big.bin", &data);
        // (what happens to the part file, offset the peer is told to resume at)
        type Damage = fn(&Path);
        let cases: [(&str, Damage, u64); 4] = [
            (
                "crashed after writing but before the checkpoint",
                |part| {
                    let mut file = OpenOptions::new().append(true).open(part).unwrap();
                    file.write_all(&[9u8; 100]).unwrap();
                },
                16_384,
            ),
            (
                "lost its tail",
                |part| {
                    OpenOptions::new()
                        .write(true)
                        .open(part)
                        .unwrap()
                        .set_len(100)
                        .unwrap();
                },
                0,
            ),
            ("was deleted", |part| fs::remove_file(part).unwrap(), 0),
            (
                "is a symlink",
                |part| {
                    let target = part.with_extension("elsewhere");
                    fs::write(&target, b"x").unwrap();
                    fs::remove_file(part).unwrap();
                    symlink(&target, part).unwrap();
                },
                0,
            ),
        ];
        for (label, damage, expected_offset) in cases {
            let sandbox = Sandbox::new("disagree");
            let first = sandbox.manager();
            first.handle_control(&wire(), offer.clone()).unwrap();
            feed(&first, &wire(), "transfer_1", 0, &data[..16_384]).unwrap();
            drop(first);
            damage(&part_of(&sandbox, "transfer_1"));

            let manager = sandbox.manager();
            let path = if label == "is a symlink" {
                // Refusing outright is as good as repairing; it must not write through.
                let result = manager.handle_control(&wire(), offer.clone());
                let elsewhere = part_of(&sandbox, "transfer_1").with_extension("elsewhere");
                assert_eq!(fs::read(elsewhere).unwrap(), b"x");
                drop(result);
                continue;
            } else {
                manager.handle_control(&wire(), offer.clone()).unwrap()
            };
            let accept = only_control(path);
            assert_eq!(accept.offset, expected_offset, "{label}");
            assert_eq!(
                fs::metadata(part_of(&sandbox, "transfer_1")).unwrap().len(),
                expected_offset,
                "{label}"
            );
            let mut offset = expected_offset as usize;
            let mut published = None;
            while offset < data.len() {
                let end = (offset + MAX_FILE_CHUNK_BYTES).min(data.len());
                let events = feed(
                    &manager,
                    &wire(),
                    "transfer_1",
                    offset as u64,
                    &data[offset..end],
                )
                .unwrap();
                published = published.or_else(|| completed_path(&events));
                offset = end;
            }
            assert_eq!(fs::read(published.unwrap()).unwrap(), data, "{label}");
        }
    }

    #[test]
    fn an_orphaned_partial_file_does_not_wedge_a_new_offer() {
        let sandbox = Sandbox::new("orphan");
        let manager = sandbox.manager();
        fs::write(part_of(&sandbox, "transfer_1"), b"left by a crash").unwrap();
        let path = upload(&manager, &wire(), &offer(), b"abc");
        assert_eq!(fs::read(path).unwrap(), b"abc");
        assert!(names(&sandbox.state()).is_empty());
    }

    #[test]
    fn an_empty_file_is_published_immediately() {
        let sandbox = Sandbox::new("empty");
        let manager = sandbox.manager();
        let events = manager
            .handle_control(&wire(), offer_for("empty_1", "empty.txt", b""))
            .unwrap();
        assert!(
            matches!(events.as_slice(), [TransferEvent::SendControl(done), TransferEvent::Completed { .. }] if done.action == FileAction::Complete),
            "{events:?}"
        );
        assert_eq!(
            fs::read(sandbox.downloads().join("empty.txt")).unwrap(),
            b""
        );
        assert!(names(&sandbox.state()).is_empty());
        assert!(active_ids(&manager).is_empty());
    }

    #[test]
    fn re_offering_a_fully_received_partial_finishes_it() {
        // Crash window: the last chunk and its checkpoint reached the disk but
        // the file was never published.
        let sandbox = Sandbox::new("full");
        let first = sandbox.manager();
        first.handle_control(&wire(), offer()).unwrap();
        drop(first);
        fs::write(part_of(&sandbox, "transfer_1"), b"abc").unwrap();
        let path = checkpoint_of(&sandbox, "transfer_1");
        let mut stored: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        stored["offset"] = serde_json::json!(3);
        fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

        let manager = sandbox.manager();
        let events = manager.handle_control(&wire_of(90, 1), {
            let mut again = offer();
            again.session_id = 90;
            again.connection_generation = 1;
            again
        });
        let events = events.unwrap();
        assert!(completed_path(&events).is_some(), "{events:?}");
        assert_eq!(
            fs::read(sandbox.downloads().join("report.txt")).unwrap(),
            b"abc"
        );
        assert!(names(&sandbox.state()).is_empty());
    }

    #[test]
    fn simultaneous_incoming_transfers_are_bounded() {
        let sandbox = Sandbox::new("bound");
        let manager = sandbox.manager();
        for index in 0..MAX_INCOMING_TRANSFERS {
            let events = manager
                .handle_control(&wire(), offer_for(&format!("t_{index}"), "f.bin", b"abc"))
                .unwrap();
            assert_eq!(only_control(events).action, FileAction::Accept);
        }
        let before = snapshot(&sandbox.state());

        let refused = only_control(
            manager
                .handle_control(&wire(), offer_for("t_extra", "f.bin", b"abc"))
                .unwrap(),
        );
        assert_eq!(refused.action, FileAction::Error);
        assert!(refused.error.is_some());
        assert_eq!(
            snapshot(&sandbox.state()),
            before,
            "a refused offer left files behind"
        );
        assert_eq!(active_ids(&manager).len(), MAX_INCOMING_TRANSFERS);

        // A replay of an offer that is already open is not a new transfer.
        let replay = manager
            .handle_control(&wire(), offer_for("t_0", "f.bin", b"abc"))
            .unwrap();
        assert_eq!(only_control(replay).action, FileAction::Accept);

        // Freeing a slot lets the refused offer in.
        manager
            .handle_control(
                &wire(),
                reply(&offer_for("t_1", "f.bin", b"abc"), FileAction::Cancel),
            )
            .unwrap();
        let accepted = manager
            .handle_control(&wire(), offer_for("t_extra", "f.bin", b"abc"))
            .unwrap();
        assert_eq!(only_control(accepted).action, FileAction::Accept);
    }

    // ---- sending ---------------------------------------------------------

    fn source_file(sandbox: &Sandbox, name: &str, data: &[u8]) -> PathBuf {
        let path = sandbox.base.join(name);
        fs::write(&path, data).unwrap();
        path
    }

    fn send_files(sandbox: &Sandbox) -> Vec<String> {
        names(&sandbox.state())
            .into_iter()
            .filter(|name| name.contains(".send"))
            .collect()
    }

    #[test]
    fn a_file_sent_by_one_manager_arrives_byte_identical_at_another() {
        let sender = Sandbox::new("loop-a");
        let receiver = Sandbox::new("loop-b");
        let data = payload(50_000);
        let source = source_file(&sender, "movie.bin", &data);
        let (a, b) = (sender.manager(), receiver.manager());
        let a_wire = wire_of(4, 2);
        let b_wire = WebRtcWireBinding {
            sender_device_id: a_wire.peer_device_id.clone(),
            peer_device_id: a_wire.sender_device_id.clone(),
            ..a_wire.clone()
        };

        let mut to_b = a.start_send(&a_wire, &source).unwrap();
        let (mut sent_done, mut received) = (false, None);
        for _ in 0..64 {
            let mut to_a = Vec::new();
            for event in to_b.drain(..) {
                let out = match event {
                    TransferEvent::SendControl(control) => b.handle_control(&b_wire, control),
                    TransferEvent::SendChunk(bytes) => b.handle_chunk(&b_wire, &bytes),
                    TransferEvent::Completed { .. } => unreachable!(),
                }
                .unwrap();
                for event in out {
                    match event {
                        TransferEvent::Completed { path, .. } => received = Some(path),
                        other => to_a.push(other),
                    }
                }
            }
            for event in to_a {
                let out = match event {
                    TransferEvent::SendControl(control) => a.handle_control(&a_wire, control),
                    other => panic!("receiver sent {other:?}"),
                }
                .unwrap();
                for event in out {
                    match event {
                        TransferEvent::Completed { .. } => sent_done = true,
                        other => to_b.push(other),
                    }
                }
            }
            if to_b.is_empty() {
                break;
            }
        }
        assert!(sent_done, "the sender never saw the completion");
        assert_eq!(
            fs::read(received.expect("receiver published")).unwrap(),
            data
        );
        assert!(send_files(&sender).is_empty(), "sender snapshot leaked");
        assert!(names(&receiver.state()).is_empty());
    }

    #[test]
    fn sending_an_empty_file_offers_zero_bytes_and_completes() {
        let sandbox = Sandbox::new("send-empty");
        let source = source_file(&sandbox, "empty.txt", b"");
        let manager = sandbox.manager();
        let offer = only_control(manager.start_send(&wire(), &source).unwrap());
        assert_eq!(offer.total_size, Some(0));
        assert!(manager
            .handle_control(&wire(), reply(&offer, FileAction::Accept))
            .unwrap()
            .is_empty());
        let mut complete = reply(&offer, FileAction::Complete);
        complete.sha256 = offer.sha256.clone();
        let events = manager.handle_control(&wire(), complete).unwrap();
        assert!(completed_path(&events).is_some());
        assert!(send_files(&sandbox).is_empty());
    }

    #[test]
    fn start_send_refuses_unsendable_sources_without_leaving_a_snapshot() {
        let sandbox = Sandbox::new("send-refuse");
        let manager = sandbox.manager();
        let directory = sandbox.base.join("a-directory");
        fs::create_dir_all(&directory).unwrap();
        // A name the peer would reject as unsafe: refuse before copying gigabytes.
        let backslash = source_file(&sandbox, "back\\slash.txt", b"abc");
        for source in [
            directory,
            backslash,
            sandbox.base.join("missing.txt"),
            PathBuf::from("/"),
        ] {
            assert!(manager.start_send(&wire(), &source).is_err(), "{source:?}");
            assert!(
                send_files(&sandbox).is_empty(),
                "{source:?} left a snapshot"
            );
        }
    }

    #[test]
    fn sender_responses_with_a_wrong_binding_or_token_change_nothing() {
        let sandbox = Sandbox::new("send-binding");
        let source = source_file(&sandbox, "f.bin", &payload(20_000));
        let manager = sandbox.manager();
        let offer = only_control(manager.start_send(&wire(), &source).unwrap());
        manager
            .handle_control(&wire(), reply(&offer, FileAction::Accept))
            .unwrap();
        let before = snapshot(&sandbox.state());

        for action in [
            FileAction::Accept,
            FileAction::Acknowledge,
            FileAction::Complete,
            FileAction::Cancel,
            FileAction::Error,
        ] {
            let mut wrong_token = reply(&offer, action);
            wrong_token.transfer_token = "not-the-token".to_string();
            let mut stale = reply(&offer, action);
            stale.session_id += 1;
            for control in [wrong_token, stale] {
                assert!(
                    manager.handle_control(&wire(), control).is_err(),
                    "{action:?}"
                );
                assert_eq!(snapshot(&sandbox.state()), before, "{action:?}");
            }
        }
    }

    /// Out-of-order or forged progress from the peer that holds the token means
    /// the two sides no longer agree on the transfer. It ends — snapshot
    /// released, peer told — instead of leaving both sides waiting.
    #[test]
    fn progress_that_disagrees_with_the_transfer_ends_it_and_tells_the_peer() {
        type Forge = fn(&mut FileControl);
        let cases: [(FileAction, Forge); 3] = [
            (FileAction::Acknowledge, |c| {
                c.offset = MAX_FILE_CHUNK_BYTES as u64 * 2
            }),
            (FileAction::Complete, |c| c.offset = 3),
            (FileAction::Accept, |c| c.offset = 20_001),
        ];
        for (action, mutate) in cases {
            let sandbox = Sandbox::new("send-disagree");
            let source = source_file(&sandbox, "f.bin", &payload(20_000));
            let manager = sandbox.manager();
            let offer = only_control(manager.start_send(&wire(), &source).unwrap());
            manager
                .handle_control(&wire(), reply(&offer, FileAction::Accept))
                .unwrap();

            let mut forged = reply(&offer, action);
            forged.sha256 = offer.sha256.clone();
            mutate(&mut forged);
            let told = error_control(manager.handle_control(&wire(), forged).unwrap());

            assert_eq!(told.transfer_id, offer.transfer_id, "{action:?}");
            assert!(
                send_files(&sandbox).is_empty(),
                "{action:?} kept the snapshot"
            );
            assert!(source.exists(), "the user's own file must never be removed");
        }
    }

    #[test]
    fn cancel_and_peer_error_release_the_sender_snapshot() {
        for action in [FileAction::Cancel, FileAction::Error] {
            let sandbox = Sandbox::new("send-cancel");
            let source = source_file(&sandbox, "f.bin", &payload(20_000));
            let manager = sandbox.manager();
            let offer = only_control(manager.start_send(&wire(), &source).unwrap());
            manager
                .handle_control(&wire(), reply(&offer, FileAction::Accept))
                .unwrap();
            assert!(!send_files(&sandbox).is_empty());

            manager
                .handle_control(&wire(), reply(&offer, action))
                .unwrap();
            assert!(
                send_files(&sandbox).is_empty(),
                "{action:?} left the snapshot behind"
            );
            let mut ack = reply(&offer, FileAction::Acknowledge);
            ack.offset = MAX_FILE_CHUNK_BYTES as u64;
            assert!(manager.handle_control(&wire(), ack).is_err(), "{action:?}");
            assert!(source.exists(), "the user's own file must never be removed");
        }
    }

    #[test]
    fn a_tampered_sender_checkpoint_errors_instead_of_panicking() {
        let sandbox = Sandbox::new("send-tamper");
        let source = source_file(&sandbox, "f.bin", &payload(20_000));
        let first = sandbox.manager();
        let offer = only_control(first.start_send(&wire(), &source).unwrap());
        drop(first);
        let path = sandbox
            .state()
            .join(format!(".desklink-{}.send.json", offer.transfer_id));
        let mut stored: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        stored["offset"] = serde_json::json!(20_010);
        stored["awaiting_offset"] = serde_json::json!(20_010);
        fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

        let mut ack = reply(&offer, FileAction::Acknowledge);
        ack.offset = 20_010;
        assert!(sandbox.manager().handle_control(&wire(), ack).is_err());
    }

    // ---- failures the peer must hear about -------------------------------
    //
    // A transfer this side accepted (or started) that cannot go on has to end
    // loudly. Returning `Err` only reaches a log line, and the peer would wait
    // out its whole deadline on a transfer that is already dead.

    fn error_control(events: Vec<TransferEvent>) -> FileControl {
        let control = only_control(events);
        assert_eq!(control.action, FileAction::Error, "{control:?}");
        control
    }

    #[test]
    fn a_write_failure_on_an_accepted_transfer_ends_it_and_tells_the_sender() {
        let sandbox = Sandbox::new("fail-write");
        let manager = sandbox.manager();
        let data = payload(40_000);
        let offer = offer_for("transfer_w", "big.bin", &data);
        manager.handle_control(&wire(), offer.clone()).unwrap();
        // The partial file becomes something the manager refuses to write.
        let part = part_of(&sandbox, "transfer_w");
        fs::remove_file(&part).unwrap();
        symlink("/nonexistent", &part).unwrap();

        let told = error_control(feed(&manager, &wire(), "transfer_w", 0, &data[..1000]).unwrap());

        assert_eq!(told.transfer_id, "transfer_w");
        assert_eq!(told.transfer_token, "token");
        assert!(told.error.as_deref().is_some_and(|text| !text.is_empty()));
        assert!(active_ids(&manager).is_empty(), "the transfer is over");
        assert!(!checkpoint_of(&sandbox, "transfer_w").exists());
        assert!(names(&sandbox.downloads()).is_empty());
        assert!(
            feed(&manager, &wire(), "transfer_w", 0, &data[..1000]).is_err(),
            "a later chunk for a dead transfer is refused, and gets no reply"
        );
    }

    #[test]
    fn an_offer_that_cannot_be_accepted_is_answered_not_dropped() {
        let sandbox = Sandbox::new("fail-offer");
        let manager = sandbox.manager();
        // Something that is not a file is in the way of the partial file.
        fs::create_dir(part_of(&sandbox, "transfer_o")).unwrap();
        let offer = offer_for("transfer_o", "a.bin", b"abc");

        let told = error_control(manager.handle_control(&wire(), offer).unwrap());

        assert_eq!(told.transfer_id, "transfer_o");
        assert_eq!(told.transfer_token, "token");
        assert!(active_ids(&manager).is_empty());
    }

    #[test]
    fn a_sender_that_loses_its_snapshot_tells_the_receiver() {
        let sandbox = Sandbox::new("fail-send");
        let manager = sandbox.manager();
        let source = source_file(&sandbox, "f.bin", &payload(40_000));
        let offer = only_control(manager.start_send(&wire(), &source).unwrap());
        manager
            .handle_control(&wire(), reply(&offer, FileAction::Accept))
            .unwrap();
        for name in send_files(&sandbox) {
            if !name.ends_with(".json") {
                fs::remove_file(sandbox.state().join(name)).unwrap();
            }
        }
        let mut ack = reply(&offer, FileAction::Acknowledge);
        ack.offset = MAX_FILE_CHUNK_BYTES as u64;

        let told = error_control(manager.handle_control(&wire(), ack).unwrap());

        assert_eq!(told.transfer_id, offer.transfer_id);
        assert!(
            send_files(&sandbox).is_empty(),
            "no snapshot or checkpoint left"
        );
        assert!(source.exists(), "the user's own file must never be removed");
    }

    /// The peer's claims about a transfer this side does not have change
    /// nothing and are not answered: that would make every stray message an
    /// oracle, and a chunk flood a reply flood.
    #[test]
    fn a_message_about_a_transfer_we_do_not_have_gets_no_reply() {
        let sandbox = Sandbox::new("no-reply");
        let manager = sandbox.manager();
        let stray = FileChunk {
            transfer_id: "nothing".to_string(),
            transfer_token: "token".to_string(),
            offset: 0,
            data: b"abc".to_vec(),
        };
        assert!(manager
            .handle_chunk(&wire(), &stray.encode().unwrap())
            .is_err());
        for action in [
            FileAction::Accept,
            FileAction::Acknowledge,
            FileAction::Complete,
        ] {
            let stray = reply(&offer_for("nothing", "a.bin", b"abc"), action);
            assert!(
                manager.handle_control(&wire(), stray).is_err(),
                "{action:?}"
            );
        }
    }

    /// A transfer that is running must not be killed by someone who does not
    /// hold its token.
    #[test]
    fn a_wrong_token_cannot_end_a_running_transfer() {
        let sandbox = Sandbox::new("wrong-token");
        let manager = sandbox.manager();
        let data = payload(40_000);
        manager
            .handle_control(&wire(), offer_for("transfer_t", "big.bin", &data))
            .unwrap();

        let forged = chunk_bytes("transfer_t", "not-the-token", 0, &data[..1000]);
        assert!(manager.handle_chunk(&wire(), &forged).is_err());

        assert_eq!(active_ids(&manager), ["transfer_t"]);
        assert!(feed(&manager, &wire(), "transfer_t", 0, &data[..1000]).is_ok());
    }

    // ---- what the user is shown ------------------------------------------

    use crate::device_links::webrtc::transfer_board::{self as board, Outcome};

    #[test]
    fn a_received_file_is_listed_advances_and_is_reported_when_published() {
        let sandbox = Sandbox::new("board-recv");
        let manager = sandbox.manager();
        let data = payload(40_000);
        manager
            .handle_control(&wire(), offer_for("board_recv", "big.bin", &data))
            .unwrap();

        let row = board::peek_row("board_recv").expect("an accepted offer is listed");
        assert!(row.incoming);
        assert_eq!((row.total, row.done), (40_000, 0));
        assert_eq!(
            (row.device_id.as_str(), row.name.as_str()),
            ("phone", "big.bin")
        );

        feed(&manager, &wire(), "board_recv", 0, &data[..16_384]).unwrap();
        assert_eq!(board::peek_row("board_recv").unwrap().done, 16_384);

        let mut offset = 16_384;
        while offset < data.len() {
            let end = (offset + MAX_FILE_CHUNK_BYTES).min(data.len());
            feed(
                &manager,
                &wire(),
                "board_recv",
                offset as u64,
                &data[offset..end],
            )
            .unwrap();
            offset = end;
        }
        assert!(board::peek_row("board_recv").is_none());
        let finished = board::peek_finished("board_recv");
        assert_eq!(finished.len(), 1);
        assert!(finished[0].incoming);
        assert_eq!(
            finished[0].outcome,
            Outcome::Received(sandbox.downloads().join("big.bin"))
        );
    }

    #[test]
    fn a_sent_file_is_listed_advances_on_acknowledgement_and_is_reported_when_complete() {
        let sandbox = Sandbox::new("board-send");
        let manager = sandbox.manager();
        let source = source_file(&sandbox, "f.bin", &payload(20_000));
        let offer = only_control(manager.start_send(&wire(), &source).unwrap());
        let id = offer.transfer_id.clone();

        let row = board::peek_row(&id).expect("a started send is listed");
        assert!(!row.incoming);
        assert_eq!(
            (row.total, row.done, row.name.as_str()),
            (20_000, 0, "f.bin")
        );

        manager
            .handle_control(&wire(), reply(&offer, FileAction::Accept))
            .unwrap();
        let mut ack = reply(&offer, FileAction::Acknowledge);
        ack.offset = LEGACY_FILE_CHUNK_BYTES as u64;
        manager.handle_control(&wire(), ack).unwrap();
        assert_eq!(
            board::peek_row(&id).unwrap().done,
            LEGACY_FILE_CHUNK_BYTES as u64
        );

        let mut last = reply(&offer, FileAction::Acknowledge);
        last.offset = 20_000;
        manager.handle_control(&wire(), last).unwrap();
        let mut done = reply(&offer, FileAction::Complete);
        done.offset = 20_000;
        done.sha256 = offer.sha256.clone();
        manager.handle_control(&wire(), done).unwrap();

        assert!(board::peek_row(&id).is_none());
        let finished = board::peek_finished(&id);
        assert_eq!(finished.len(), 1);
        assert!(!finished[0].incoming);
        assert_eq!(finished[0].outcome, Outcome::Sent);
    }

    #[test]
    fn every_way_a_transfer_can_end_is_reported_once() {
        // The peer cancels, the peer reports an error, we abort on a local
        // failure, and the file arrives corrupt.
        let sandbox = Sandbox::new("board-ends");
        let manager = sandbox.manager();
        let data = payload(40_000);

        let cancelled = offer_for("board_cancelled", "a.bin", &data);
        manager.handle_control(&wire(), cancelled.clone()).unwrap();
        manager
            .handle_control(&wire(), reply(&cancelled, FileAction::Cancel))
            .unwrap();
        let found = board::peek_finished("board_cancelled");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].outcome, Outcome::Cancelled);

        let errored = offer_for("board_errored", "b.bin", &data);
        manager.handle_control(&wire(), errored.clone()).unwrap();
        let mut error = reply(&errored, FileAction::Error);
        error.error = Some("disk full over there".to_string());
        manager.handle_control(&wire(), error).unwrap();
        let found = board::peek_finished("board_errored");
        assert_eq!(found.len(), 1);
        assert!(
            matches!(&found[0].outcome, Outcome::Failed(text) if text.contains("disk full over there")),
            "{found:?}"
        );

        let aborted = offer_for("board_aborted", "c.bin", &data);
        manager.handle_control(&wire(), aborted).unwrap();
        let part = part_of(&sandbox, "board_aborted");
        fs::remove_file(&part).unwrap();
        symlink("/nonexistent", &part).unwrap();
        error_control(feed(&manager, &wire(), "board_aborted", 0, &data[..1000]).unwrap());
        let found = board::peek_finished("board_aborted");
        assert_eq!(found.len(), 1);
        assert!(matches!(found[0].outcome, Outcome::Failed(_)), "{found:?}");

        let corrupt = offer_for("board_corrupt", "d.bin", b"abc");
        manager.handle_control(&wire(), corrupt).unwrap();
        error_control(feed(&manager, &wire(), "board_corrupt", 0, b"abd").unwrap());
        let found = board::peek_finished("board_corrupt");
        assert_eq!(found.len(), 1);
        assert!(matches!(found[0].outcome, Outcome::Failed(_)), "{found:?}");
        assert!(board::peek_row("board_corrupt").is_none());
    }

    #[test]
    fn the_user_can_cancel_a_transfer_in_either_direction() {
        let sandbox = Sandbox::new("board-user-cancel");
        let manager = sandbox.manager();
        let data = payload(40_000);

        manager
            .handle_control(&wire(), offer_for("board_in", "in.bin", &data))
            .unwrap();
        let told = only_control(manager.cancel_transfer(&wire(), "board_in").unwrap());
        assert_eq!(told.action, FileAction::Cancel);
        assert_eq!(told.transfer_id, "board_in");
        assert_eq!(told.transfer_token, "token");
        assert!(active_ids(&manager).is_empty());
        assert!(!part_of(&sandbox, "board_in").exists());
        assert_eq!(
            board::peek_finished("board_in")[0].outcome,
            Outcome::Cancelled
        );
        assert!(
            feed(&manager, &wire(), "board_in", 0, &data[..100]).is_err(),
            "a chunk that was already in flight is refused"
        );

        let source = source_file(&sandbox, "out.bin", &payload(20_000));
        let offer = only_control(manager.start_send(&wire(), &source).unwrap());
        let told = only_control(
            manager
                .cancel_transfer(&wire(), &offer.transfer_id)
                .unwrap(),
        );
        assert_eq!(told.action, FileAction::Cancel);
        assert_eq!(told.transfer_token, offer.transfer_token);
        assert!(send_files(&sandbox).is_empty());
        assert!(source.exists(), "the user's own file must never be removed");
        assert_eq!(
            board::peek_finished(&offer.transfer_id)[0].outcome,
            Outcome::Cancelled
        );

        assert!(
            manager
                .cancel_transfer(&wire(), "never-heard-of-it")
                .is_err(),
            "nothing to cancel"
        );
    }

    // ---- surviving a lost connection -------------------------------------

    fn stop_and_restart(
        sandbox: &Sandbox,
        manager: WebRtcTransferManager,
    ) -> WebRtcTransferManager {
        drop(manager);
        sandbox.manager()
    }

    #[test]
    fn an_interrupted_send_is_offered_again_and_resumes_where_the_peer_stopped() {
        let sandbox = Sandbox::new("resume-send");
        let data = payload(40_000);
        let source = source_file(&sandbox, "big.bin", &data);
        let first = sandbox.manager();
        let offer = only_control(first.start_send(&wire(), &source).unwrap());
        first
            .handle_control(&wire(), reply(&offer, FileAction::Accept))
            .unwrap();
        let mut ack = reply(&offer, FileAction::Acknowledge);
        ack.offset = LEGACY_FILE_CHUNK_BYTES as u64;
        first.handle_control(&wire(), ack).unwrap();

        // The connection is lost and comes back as a new session.
        let second = stop_and_restart(&sandbox, first);
        let again_wire = wire_of(9, 3);
        let events = second.resume_sends(&again_wire);
        let offered = only_control(events);
        assert_eq!(offered.action, FileAction::Offer);
        assert_eq!(offered.transfer_id, offer.transfer_id);
        assert_eq!(
            offered.transfer_token, offer.transfer_token,
            "the peer's checkpoint is matched by the token"
        );
        assert_eq!(offered.filename.as_deref(), Some("big.bin"));
        assert_eq!(offered.total_size, Some(40_000));
        assert_eq!(offered.sha256, offer.sha256);
        assert_eq!(offered.session_id, 9);
        assert_eq!(offered.connection_generation, 3);
        let row = board::peek_row(&offer.transfer_id).expect("a resumed send is listed again");
        assert_eq!(row.done, LEGACY_FILE_CHUNK_BYTES as u64);

        // The peer accepts from what it already has; the send carries on there.
        let mut accept = reply(&offered, FileAction::Accept);
        accept.offset = LEGACY_FILE_CHUNK_BYTES as u64;
        // A receiver that predates the chunk-size upgrade: legacy-sized chunks.
        accept.max_chunk_size = None;
        let events = second.handle_control(&again_wire, accept).unwrap();
        // Windowed: everything left fits in the window, so it all goes now,
        // starting exactly where the peer stopped.
        assert_eq!(events.len(), 2, "the rest of the file, in two chunks");
        let Some(TransferEvent::SendChunk(next)) = events.first() else {
            panic!("expected the next chunk, got {events:?}");
        };
        let next = FileChunk::decode(next).unwrap();
        assert_eq!(next.offset, LEGACY_FILE_CHUNK_BYTES as u64);
        assert_eq!(
            next.data,
            &data[LEGACY_FILE_CHUNK_BYTES..(2 * LEGACY_FILE_CHUNK_BYTES).min(data.len())]
        );
        assert!(source.exists(), "the user's own file must never be removed");
    }

    #[test]
    fn a_send_is_offered_again_only_once_per_connection() {
        let sandbox = Sandbox::new("resume-once");
        let source = source_file(&sandbox, "f.bin", &payload(20_000));
        let first = sandbox.manager();
        first.start_send(&wire(), &source).unwrap();
        let second = stop_and_restart(&sandbox, first);

        assert_eq!(second.resume_sends(&wire_of(9, 3)).len(), 1);
        assert!(second.resume_sends(&wire_of(9, 3)).is_empty());
    }

    #[test]
    fn only_the_reconnecting_devices_intact_sends_are_offered() {
        let sandbox = Sandbox::new("resume-filter");
        let mine = source_file(&sandbox, "mine.bin", &payload(20_000));
        let broken = source_file(&sandbox, "broken.bin", &payload(20_000));
        let theirs = source_file(&sandbox, "theirs.bin", &payload(20_000));
        let first = sandbox.manager();
        let mine_offer = only_control(first.start_send(&wire(), &mine).unwrap());
        let broken_offer = only_control(first.start_send(&wire(), &broken).unwrap());
        let other_device = WebRtcWireBinding {
            peer_device_id: "tablet".to_string(),
            ..wire()
        };
        first.start_send(&other_device, &theirs).unwrap();
        // A snapshot that no longer matches what was offered cannot be resumed.
        fs::write(
            sandbox
                .state()
                .join(format!(".desklink-{}.send", broken_offer.transfer_id)),
            b"short",
        )
        .unwrap();
        let second = stop_and_restart(&sandbox, first);

        let events = second.resume_sends(&wire_of(9, 3));

        let ids: Vec<_> = events
            .iter()
            .map(|event| match event {
                TransferEvent::SendControl(control) => control.transfer_id.clone(),
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(ids, std::slice::from_ref(&mine_offer.transfer_id));
        assert!(
            !sandbox
                .state()
                .join(format!(".desklink-{}.send.json", broken_offer.transfer_id))
                .exists(),
            "an unusable send is cleaned up, not retried forever"
        );
        assert!(mine.exists() && broken.exists() && theirs.exists());
    }

    #[test]
    fn a_transfer_nobody_touched_for_a_week_is_swept_but_recent_ones_stay() {
        let sandbox = Sandbox::new("sweep");
        let source = source_file(&sandbox, "f.bin", &payload(20_000));
        let manager = sandbox.manager();
        let stale = only_control(manager.start_send(&wire(), &source).unwrap());
        let recent = only_control(manager.start_send(&wire(), &source).unwrap());
        manager
            .handle_control(&wire(), offer_for("swept_in", "in.bin", b"abc"))
            .unwrap();
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
        for name in [
            format!(".desklink-{}.send", stale.transfer_id),
            format!(".desklink-{}.send.json", stale.transfer_id),
            ".desklink-swept_in.part".to_string(),
            ".desklink-swept_in.json".to_string(),
        ] {
            fs::OpenOptions::new()
                .write(true)
                .open(sandbox.state().join(name))
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        // Something that is not ours is never touched, however old.
        let foreign = sandbox.state().join("notes.txt");
        fs::write(&foreign, b"mine").unwrap();
        fs::File::options()
            .write(true)
            .open(&foreign)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        drop(manager);

        let swept = sandbox.manager();
        swept.sweep_stale(STALE_AFTER);

        let left = names(&sandbox.state());
        assert!(
            left.iter()
                .all(|name| !name.contains(&stale.transfer_id) && !name.contains("swept_in")),
            "{left:?}"
        );
        assert!(
            left.iter().any(|name| name.contains(&recent.transfer_id)),
            "{left:?}"
        );
        assert!(left.contains(&"notes.txt".to_string()));
    }

    #[test]
    fn a_sender_keeps_a_bounded_window_in_flight_and_takes_cumulative_acks() {
        let sandbox = Sandbox::new("window");
        let manager = sandbox.manager();
        let data = payload(3 * 1024 * 1024);
        let source = source_file(&sandbox, "big.bin", &data);
        let offer = only_control(manager.start_send(&wire(), &source).unwrap());
        assert_eq!(
            offer.chunk_size, LEGACY_FILE_CHUNK_BYTES,
            "valid for old peers"
        );
        assert_eq!(offer.max_chunk_size, Some(MAX_FILE_CHUNK_BYTES));

        let mut accept = reply(&offer, FileAction::Accept);
        accept.max_chunk_size = None; // an older receiver
        let events = manager.handle_control(&wire(), accept).unwrap();
        let sent: usize = events
            .iter()
            .map(|event| match event {
                TransferEvent::SendChunk(bytes) => FileChunk::decode(bytes).unwrap().data.len(),
                other => panic!("unexpected {other:?}"),
            })
            .sum();
        assert_eq!(
            sent as u64, SEND_WINDOW_BYTES,
            "a full window, not one chunk"
        );

        // One cumulative ack for half the window frees exactly that much.
        let mut ack = reply(&offer, FileAction::Acknowledge);
        ack.offset = SEND_WINDOW_BYTES / 2;
        let more = manager.handle_control(&wire(), ack.clone()).unwrap();
        assert_eq!(
            more.len() as u64,
            SEND_WINDOW_BYTES / 2 / LEGACY_FILE_CHUNK_BYTES as u64
        );
        // A repeated ack is harmless; one past what was sent ends the transfer.
        assert!(manager.handle_control(&wire(), ack).unwrap().is_empty());
        let mut beyond = reply(&offer, FileAction::Acknowledge);
        beyond.offset = 2 * SEND_WINDOW_BYTES;
        let refused = manager.handle_control(&wire(), beyond).unwrap();
        assert!(matches!(
            refused.as_slice(),
            [TransferEvent::SendControl(control)] if control.action == FileAction::Error
        ));
    }

    #[test]
    fn a_receiver_only_offers_bigger_chunks_to_a_sender_that_asked() {
        let sandbox = Sandbox::new("negotiate");
        let manager = sandbox.manager();
        let data = payload(100_000);
        let legacy = offer_for("transfer_l", "a.bin", &data);
        let accept = only_control(manager.handle_control(&wire(), legacy).unwrap());
        assert_eq!(accept.max_chunk_size, None);

        let mut upgraded = offer_for("transfer_u", "b.bin", &data);
        upgraded.max_chunk_size = Some(MAX_FILE_CHUNK_BYTES);
        let accept = only_control(manager.handle_control(&wire(), upgraded).unwrap());
        assert_eq!(accept.max_chunk_size, Some(MAX_FILE_CHUNK_BYTES));
        assert_eq!(accept.chunk_size, LEGACY_FILE_CHUNK_BYTES);
        let chunk = &data[..MAX_FILE_CHUNK_BYTES];
        assert!(feed(&manager, &wire(), "transfer_u", 0, chunk).is_ok());
    }

    #[test]
    fn durability_is_batched_but_a_live_resume_keeps_its_progress() {
        let sandbox = Sandbox::new("batched");
        // The production setting, not the per-chunk one the sandbox uses.
        let manager = WebRtcTransferManager::new(sandbox.state(), sandbox.downloads()).unwrap();
        let data = payload(40_000);
        let offer = offer_for("transfer_b", "c.bin", &data);
        manager.handle_control(&wire(), offer.clone()).unwrap();
        feed(&manager, &wire(), "transfer_b", 0, &data[..16_384]).unwrap();

        let stored: serde_json::Value =
            serde_json::from_slice(&fs::read(checkpoint_of(&sandbox, "transfer_b")).unwrap())
                .unwrap();
        assert_eq!(stored["offset"], 0, "no checkpoint rewrite per chunk");

        let accept = only_control(manager.handle_control(&wire(), offer).unwrap());
        assert_eq!(
            accept.offset, 16_384,
            "the live process resumes from memory"
        );
    }
}
