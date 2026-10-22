//! Versioned, bounded WebRTC file-transfer records shared with DeskLink
//! Android (transfers v2). This module contains no filesystem effects: it
//! validates the untrusted wire data before the transfer manager touches a
//! checkpoint or temporary file.
//!
//! Two kinds of record exist:
//!
//! * **Control** messages (JSON in the usual envelope, `desklink.file.control.v2`
//!   on `desklink-file-control-v1`): offer, accept, block-ack, block-nack,
//!   pause, resume, done, complete, cancel, error.
//! * **Frames** (raw binary data-channel messages on `desklink-file-data-v2`,
//!   no envelope, no base64): a 40-byte header, up to `maxFrameBytes - 72`
//!   payload bytes, and on the last frame of each 1 MiB block the SHA-256 of
//!   the whole block.
//!
//! Integrity is per block; the file hash is the SHA-256 of the block hashes,
//! so neither side ever re-reads a file to verify or resume it.

use openssl::sha::sha256;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::WebRtcWireBinding;

pub const FILE_PROTOCOL_VERSION: u32 = 2;
pub const FILE_CONTROL_MESSAGE_TYPE: &str = "desklink.file.control.v2";

/// A file is cut into blocks of this size; each is hashed and acknowledged.
pub const BLOCK_BYTES: u64 = 1_048_576;
/// The total size of one data-channel message (header, payload and trailer)
/// both sides support: what a peer that never raised its maximum uses.
pub const FRAME_BYTES_DEFAULT: usize = 61_440;
/// The larger size both sides may raise their maximum to once 256 KiB
/// messages are proven to cross between the two stacks. **Unused:** on this
/// side the proof is impossible. GStreamer 1.28's `webrtcbin` clamps the SCTP
/// transport's `max-message-size` to 65 536 however large the SDP advertises
/// (measured with two peers over loopback; a 196 608-byte send fails with
/// "Requested to send data that is too large"), so the desk can neither send
/// nor prove receiving more than 64 KiB.
pub const FRAME_BYTES_MAX: usize = 196_608;
/// The largest frame this build offers and accepts.
pub const FRAME_BYTES_PROVEN: usize = FRAME_BYTES_DEFAULT;
pub const FRAME_HEADER_BYTES: usize = 40;
pub const BLOCK_TRAILER_BYTES: usize = 32;
pub const MAX_TRANSFER_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// A sender queues frames only while the channel holds less than this.
pub const IN_FLIGHT_TARGET: u64 = 4 * 1024 * 1024;
/// The channel's buffered-amount-low threshold: the sender is woken here.
pub const IN_FLIGHT_LOW: u64 = 1024 * 1024;
/// Sent but not yet block-acknowledged bytes a sender allows.
pub const UNACKED_MAX: u64 = 8 * 1024 * 1024;
/// A receiver checkpoints after this many verified blocks...
pub const CHECKPOINT_BLOCKS: u64 = 8;
/// ...or this long, whichever comes first.
pub const CHECKPOINT_MS: u64 = 2_000;
/// No byte moved for this long: informational `pause`, never a deadline.
pub const NO_PROGRESS_MS: u64 = 60_000;
/// Files moving at once in each direction; the rest wait.
pub const ACTIVE_PER_DIRECTION: usize = 2;
pub const MAX_REWINDS: u32 = 3;
pub const MAX_RESTARTS: u32 = 2;
/// Frames received and not yet handled by the file thread.
pub const FILE_QUEUE_MAX_BYTES: usize = 32 * 1024 * 1024;

/// Whether a data-channel message of `length` bytes is within the largest
/// frame this build accepts (61 440 including header and trailer).
pub const fn message_fits(length: usize) -> bool {
    length <= FRAME_BYTES_PROVEN
}

/// Payload bytes one frame of `max_frame` total bytes carries. The trailer
/// space is reserved in every frame.
pub const fn payload_capacity(max_frame: usize) -> usize {
    max_frame - FRAME_HEADER_BYTES - BLOCK_TRAILER_BYTES
}

/// Blocks in a file of `total` bytes (0 bytes = 0 blocks).
pub const fn blocks_for(total: u64) -> u64 {
    total.div_ceil(BLOCK_BYTES)
}

/// Why a transfer is paused (`pause.reason`).
pub mod reason {
    pub const STORAGE_FULL: &str = "storage-full";
    pub const BACKLOG: &str = "backlog";
    pub const NO_PROGRESS: &str = "no-progress";
    pub const USER: &str = "user";
    pub const ALL: [&str; 4] = [STORAGE_FULL, BACKLOG, NO_PROGRESS, USER];
}

/// `cancel.code` and `error.code`.
pub mod code {
    pub const USER: &str = "user";
    pub const SOURCE_GONE: &str = "source-gone";
    pub const SOURCE_CHANGED: &str = "source-changed";
    pub const REMOVED: &str = "removed";
    pub const REFUSED: &str = "refused";
    pub const BUSY: &str = "busy";
    pub const TOO_LARGE: &str = "too-large";
    pub const BAD_FRAME: &str = "bad-frame";
    pub const HASH_MISMATCH: &str = "hash-mismatch";
    pub const INTERNAL: &str = "internal";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileAction {
    Offer,
    Accept,
    BlockAck,
    BlockNack,
    Pause,
    Resume,
    Done,
    Complete,
    Cancel,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileControl {
    pub protocol_version: u32,
    pub action: FileAction,
    pub transfer_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub transfer_token: String,
    // ── offer ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size: Option<u64>,
    /// Source modification time, ms since the epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_size: Option<u64>,
    /// The largest message the sender of this record will emit (offer) or
    /// accepts (accept).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_frame_bytes: Option<usize>,
    /// A clipboard copy (C3): placed on the receiver's clipboard, not saved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clipboard: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    /// The photo or scan a desk asked for (F3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture_id: Option<String>,
    /// A synced file (F8): its pair, its path and its manifest content hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relpath: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    // ── accept / resume ──
    /// Blocks `0..n` are verified and durable at the receiver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume_block: Option<u64>,
    /// SHA-256 of the concatenated 32-byte hashes of those blocks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix_sha256: Option<String>,
    // ── block-ack / block-nack ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_index: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// block-nack and a receiver's resume: the new pass.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<u8>,
    /// pause (see [`reason`]) or block-nack (`hash-mismatch`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    // ── done / complete ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocks: Option<u64>,
    // ── cancel / error ──
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// F8 P4: which pair a file belongs to, where in it, and its mtime (ms).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FolderRef {
    pub folder: String,
    pub relpath: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime: Option<i64>,
}

/// Segments of a synced path, and its length (F8 P2).
const MAX_RELPATH_SEGMENTS: usize = 32;
const MAX_RELPATH_BYTES: usize = 1024;
/// Text fields of a control message that a person may read are bounded.
const MAX_MESSAGE_CHARS: usize = 4096;

/// F8 P2: a path relative to a pair's root: `/`-separated valid filenames,
/// at most 32 of them and 1024 bytes. Never absolute, never `.` or `..`.
pub(crate) fn validate_relpath(value: &str) -> Result<(), String> {
    let segments: Vec<&str> = value.split('/').collect();
    if value.is_empty() || value.len() > MAX_RELPATH_BYTES || segments.len() > MAX_RELPATH_SEGMENTS
    {
        return Err("Unsafe DeskLink synced path".to_string());
    }
    segments
        .iter()
        .try_for_each(|segment| validate_filename(segment))
        .map_err(|_| "Unsafe DeskLink synced path".to_string())
}

/// Lowercase hex of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The 32 bytes a 64-digit hex string names.
pub fn unhex32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(out)
}

/// SHA-256 of the concatenated block hashes: the hash of a file (of an empty
/// one when there are none), and the `prefixSha256` of its first blocks.
pub fn hash_of_hashes(hashes: &[[u8; 32]]) -> [u8; 32] {
    sha256(&hashes.concat())
}

/// The 16 bytes of a transfer id (a canonical lowercase UUID string).
pub fn id_bytes(id: &str) -> Option<[u8; 16]> {
    let parsed = Uuid::parse_str(id).ok()?;
    (parsed.hyphenated().to_string() == id).then(|| *parsed.as_bytes())
}

/// The transfer id string of a frame's 16 bytes.
pub fn id_string(bytes: &[u8; 16]) -> String {
    Uuid::from_bytes(*bytes).hyphenated().to_string()
}

impl FileControl {
    /// A message of `action` for transfer `id`, from this side of `wire`, with
    /// every optional field empty.
    pub fn new(wire: &WebRtcWireBinding, action: FileAction, id: &str, token: &str) -> Self {
        Self {
            protocol_version: FILE_PROTOCOL_VERSION,
            action,
            transfer_id: id.to_string(),
            device_id: wire.sender_device_id.clone(),
            session_id: wire.session_id,
            connection_generation: wire.generation,
            transfer_token: token.to_string(),
            filename: None,
            total_size: None,
            mtime: None,
            block_size: None,
            max_frame_bytes: None,
            clipboard: None,
            mime_type: None,
            capture_id: None,
            folder: None,
            relpath: None,
            content_sha256: None,
            resume_block: None,
            prefix_sha256: None,
            block_index: None,
            sha256: None,
            pass: None,
            reason: None,
            file_hash: None,
            blocks: None,
            code: None,
            message: None,
        }
    }

    pub fn from_json(input: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(input)
            .map_err(|error| format!("Malformed DeskLink file control: {error}"))
    }

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self)
            .map_err(|error| format!("Could not serialize DeskLink file control: {error}"))
    }

    /// The Bucket reference of an offer, if it carries one.
    pub fn folder_ref(&self) -> Option<FolderRef> {
        Some(FolderRef {
            folder: self.folder.clone()?,
            relpath: self.relpath.clone()?,
            mtime: self.mtime,
        })
    }

    /// The text the peer sent for a person to read, bounded and stripped of
    /// control characters.
    pub fn peer_message(&self) -> Option<String> {
        let text: String = self
            .message
            .as_deref()?
            .chars()
            .filter(|character| !character.is_control())
            .take(256)
            .collect();
        (!text.trim().is_empty()).then_some(text)
    }

    pub fn validate(&self, wire: &WebRtcWireBinding) -> Result<(), String> {
        if self.protocol_version != FILE_PROTOCOL_VERSION {
            return Err("Unsupported DeskLink WebRTC file version".to_string());
        }
        if self.device_id != wire.peer_device_id
            || self.session_id != wire.session_id
            || self.connection_generation != wire.generation
        {
            return Err("DeskLink WebRTC file binding mismatch".to_string());
        }
        validate_id(&self.transfer_id)?;
        validate_token(&self.transfer_token)?;
        for text in [&self.message, &self.reason, &self.code, &self.mime_type]
            .into_iter()
            .flatten()
        {
            if text.chars().count() > MAX_MESSAGE_CHARS {
                return Err("Oversized DeskLink WebRTC file text".to_string());
            }
        }
        for hash in [
            &self.prefix_sha256,
            &self.sha256,
            &self.file_hash,
            &self.content_sha256,
        ]
        .into_iter()
        .flatten()
        {
            if unhex32(hash).is_none() {
                return Err("Invalid DeskLink WebRTC file checksum".to_string());
            }
        }
        if self
            .total_size
            .is_some_and(|size| size > MAX_TRANSFER_BYTES)
        {
            return Err("Invalid DeskLink WebRTC transfer size".to_string());
        }
        if self.max_frame_bytes.is_some_and(|size| {
            !(FRAME_HEADER_BYTES + BLOCK_TRAILER_BYTES + 1..=262_144).contains(&size)
        }) {
            return Err("Invalid DeskLink WebRTC file frame size".to_string());
        }
        match self.action {
            FileAction::Offer => self.validate_offer(),
            FileAction::Accept => {
                if self.resume_block.is_none() || self.prefix_sha256.is_none() {
                    return Err("Malformed DeskLink WebRTC file accept".to_string());
                }
                Ok(())
            }
            FileAction::BlockAck => {
                if self.block_index.is_none() || self.sha256.is_none() {
                    return Err("Malformed DeskLink WebRTC block acknowledgement".to_string());
                }
                Ok(())
            }
            FileAction::BlockNack => {
                if self.block_index.is_none() || self.pass.is_none() {
                    return Err("Malformed DeskLink WebRTC block refusal".to_string());
                }
                Ok(())
            }
            FileAction::Pause => {
                // An unknown reason still pauses, as the person's own.
                Ok(())
            }
            FileAction::Resume => {
                // Fields are all or nothing: the receiver's restart point.
                let given = [
                    self.resume_block.is_some(),
                    self.prefix_sha256.is_some(),
                    self.pass.is_some(),
                ];
                if given.iter().any(|g| *g) && !given.iter().all(|g| *g) {
                    return Err("Malformed DeskLink WebRTC file resume".to_string());
                }
                Ok(())
            }
            FileAction::Done => {
                if self.file_hash.is_none() || self.blocks.is_none() || self.total_size.is_none() {
                    return Err("Malformed DeskLink WebRTC file completion".to_string());
                }
                Ok(())
            }
            FileAction::Complete => {
                if self.file_hash.is_none() {
                    return Err("Malformed DeskLink WebRTC file receipt".to_string());
                }
                Ok(())
            }
            FileAction::Cancel | FileAction::Error => Ok(()),
        }
    }

    fn validate_offer(&self) -> Result<(), String> {
        let filename = self
            .filename
            .as_deref()
            .ok_or_else(|| "Malformed DeskLink WebRTC file offer".to_string())?;
        validate_filename(filename)?;
        if self.total_size.is_none() || self.mtime.is_none() {
            return Err("Malformed DeskLink WebRTC file offer".to_string());
        }
        if self.block_size != Some(BLOCK_BYTES) {
            return Err("Unsupported DeskLink WebRTC block size".to_string());
        }
        // The Bucket's three fields come together.
        let synced = [
            self.folder.is_some(),
            self.relpath.is_some(),
            self.content_sha256.is_some(),
        ];
        if synced.iter().any(|g| *g) {
            if !synced.iter().all(|g| *g) {
                return Err("Malformed DeskLink synced file offer".to_string());
            }
            let folder = self.folder.as_deref().unwrap_or_default();
            let relpath = self.relpath.as_deref().unwrap_or_default();
            let named = relpath.rsplit('/').next() == Some(filename);
            let id = !folder.is_empty()
                && folder.len() <= 64
                && folder
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-');
            validate_relpath(relpath)?;
            if !named || !id {
                return Err("Malformed DeskLink synced file offer".to_string());
            }
        }
        Ok(())
    }
}

/// The fixed part of a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    pub last_of_block: bool,
    pub pass: u8,
    pub transfer: [u8; 16],
    pub block_index: u32,
    pub offset: u64,
    pub length: u32,
}

const MAGIC: &[u8; 4] = b"DLF2";
const FLAG_LAST_OF_BLOCK: u8 = 1;

impl FrameHeader {
    /// Appends the 40 header bytes (big-endian) to `out`.
    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(MAGIC);
        out.push(if self.last_of_block {
            FLAG_LAST_OF_BLOCK
        } else {
            0
        });
        out.push(self.pass);
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&self.transfer);
        out.extend_from_slice(&self.block_index.to_be_bytes());
        out.extend_from_slice(&self.offset.to_be_bytes());
        out.extend_from_slice(&self.length.to_be_bytes());
    }

    /// Whether this frame fits a file of `total_size` bytes: its block index
    /// agrees with its offset, it stays inside its block and the file, and it
    /// is the last of its block exactly when it ends the block or the file.
    pub fn check(&self, total_size: u64) -> Result<(), String> {
        let end = self.offset.checked_add(u64::from(self.length));
        let Some(end) = end else {
            return Err("DeskLink file frame overflows".to_string());
        };
        if self.length == 0
            || self.offset >= total_size
            || u64::from(self.block_index) != self.offset / BLOCK_BYTES
        {
            return Err("DeskLink file frame is out of range".to_string());
        }
        let block_end = ((u64::from(self.block_index) + 1) * BLOCK_BYTES).min(total_size);
        if end > block_end {
            return Err("DeskLink file frame crosses a block".to_string());
        }
        if self.last_of_block != (end == block_end) {
            return Err("DeskLink file frame misplaces its block end".to_string());
        }
        Ok(())
    }
}

/// A parsed frame, borrowing the message.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub header: FrameHeader,
    pub payload: &'a [u8],
    /// SHA-256 of the whole block, on the last frame of a block.
    pub trailer: Option<[u8; 32]>,
}

/// Parses one binary data-channel message. Only structure is checked here
/// (magic, reserved bits, lengths); [`FrameHeader::check`] is the file-aware
/// half.
pub fn parse_frame(message: &[u8]) -> Result<Frame<'_>, String> {
    let malformed = || "Malformed DeskLink file frame".to_string();
    if message.len() < FRAME_HEADER_BYTES + 1 || &message[..4] != MAGIC {
        return Err(malformed());
    }
    let flags = message[4];
    if flags & !FLAG_LAST_OF_BLOCK != 0 || message[6] != 0 || message[7] != 0 {
        return Err(malformed());
    }
    let last_of_block = flags & FLAG_LAST_OF_BLOCK != 0;
    let mut transfer = [0u8; 16];
    transfer.copy_from_slice(&message[8..24]);
    let block_index = u32::from_be_bytes(message[24..28].try_into().map_err(|_| malformed())?);
    let offset = u64::from_be_bytes(message[28..36].try_into().map_err(|_| malformed())?);
    let length = u32::from_be_bytes(message[36..40].try_into().map_err(|_| malformed())?);
    let trailer_bytes = if last_of_block {
        BLOCK_TRAILER_BYTES
    } else {
        0
    };
    let expected = FRAME_HEADER_BYTES
        .checked_add(length as usize)
        .and_then(|value| value.checked_add(trailer_bytes))
        .ok_or_else(malformed)?;
    if length == 0 || expected != message.len() {
        return Err(malformed());
    }
    let payload_end = FRAME_HEADER_BYTES + length as usize;
    let trailer = last_of_block.then(|| {
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&message[payload_end..]);
        hash
    });
    Ok(Frame {
        header: FrameHeader {
            last_of_block,
            pass: message[5],
            transfer,
            block_index,
            offset,
            length,
        },
        payload: &message[FRAME_HEADER_BYTES..payload_end],
        trailer,
    })
}

/// `pass` after a rewind or a restart: it wraps.
pub const fn next_pass(pass: u8) -> u8 {
    pass.wrapping_add(1)
}

fn validate_id(value: &str) -> Result<(), String> {
    if id_bytes(value).is_none() {
        return Err("Invalid DeskLink transfer ID".to_string());
    }
    Ok(())
}

fn validate_token(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > 256 || !value.is_ascii() {
        return Err("Invalid DeskLink transfer token".to_string());
    }
    Ok(())
}

pub(crate) fn validate_filename(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 255
        || matches!(value, "." | "..")
        || value.contains(['/', '\\', '\0'])
    {
        return Err("Unsafe DeskLink transfer filename".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire() -> WebRtcWireBinding {
        WebRtcWireBinding {
            sender_device_id: "desktop-device".to_string(),
            peer_device_id: "phone-device".to_string(),
            session_id: 9,
            generation: 2,
        }
    }

    const ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

    /// What the phone sends: a control message from its side of `wire()`.
    fn from_phone(action: FileAction) -> FileControl {
        let phone = WebRtcWireBinding {
            sender_device_id: "phone-device".to_string(),
            peer_device_id: "desktop-device".to_string(),
            session_id: 9,
            generation: 2,
        };
        FileControl::new(&phone, action, ID, "token")
    }

    fn offer() -> FileControl {
        FileControl {
            filename: Some("report.pdf".to_string()),
            total_size: Some(3),
            mtime: Some(1_700_000_000_000),
            block_size: Some(BLOCK_BYTES),
            max_frame_bytes: Some(FRAME_BYTES_DEFAULT),
            ..from_phone(FileAction::Offer)
        }
    }

    #[test]
    fn the_constants_are_the_contract() {
        assert_eq!(BLOCK_BYTES, 1_048_576);
        assert_eq!(FRAME_BYTES_DEFAULT, 61_440);
        assert_eq!(FRAME_BYTES_MAX, 196_608);
        assert_eq!(FRAME_HEADER_BYTES, 40);
        assert_eq!(BLOCK_TRAILER_BYTES, 32);
        assert_eq!(MAX_TRANSFER_BYTES, 4 << 30);
        assert_eq!(IN_FLIGHT_TARGET, 4 << 20);
        assert_eq!(IN_FLIGHT_LOW, 1 << 20);
        assert_eq!(UNACKED_MAX, 8 << 20);
        assert_eq!((CHECKPOINT_BLOCKS, CHECKPOINT_MS), (8, 2000));
        assert_eq!(NO_PROGRESS_MS, 60_000);
        assert_eq!(ACTIVE_PER_DIRECTION, 2);
        assert_eq!((MAX_REWINDS, MAX_RESTARTS), (3, 2));
        assert_eq!(FILE_QUEUE_MAX_BYTES, 32 << 20);
        assert_eq!(FILE_PROTOCOL_VERSION, 2);
        assert_eq!(FILE_CONTROL_MESSAGE_TYPE, "desklink.file.control.v2");
        // A 60 KiB frame carries 61 368 payload bytes; a block is 18 frames.
        assert_eq!(payload_capacity(FRAME_BYTES_DEFAULT), 61_368);
        assert_eq!(BLOCK_BYTES.div_ceil(61_368), 18);
        assert_eq!(FRAME_BYTES_PROVEN, FRAME_BYTES_DEFAULT);
    }

    #[test]
    fn a_file_is_cut_into_whole_and_partial_blocks() {
        assert_eq!(blocks_for(0), 0);
        assert_eq!(blocks_for(1), 1);
        assert_eq!(blocks_for(BLOCK_BYTES), 1);
        assert_eq!(blocks_for(BLOCK_BYTES + 1), 2);
        assert_eq!(blocks_for(MAX_TRANSFER_BYTES), 4096);
    }

    #[test]
    fn the_file_hash_is_the_hash_of_the_block_hashes() {
        let empty = hash_of_hashes(&[]);
        assert_eq!(hex(&empty), hex(&sha256(b"")));
        let (a, b) = (sha256(b"a"), sha256(b"b"));
        let mut both = a.to_vec();
        both.extend_from_slice(&b);
        assert_eq!(hash_of_hashes(&[a, b]), sha256(&both));
        assert_eq!(unhex32(&hex(&a)), Some(a));
        assert_eq!(unhex32("zz"), None);
        assert_eq!(unhex32(&"g".repeat(64)), None);
    }

    #[test]
    fn control_messages_use_the_v2_json_names() {
        let mut ack = from_phone(FileAction::BlockAck);
        ack.block_index = Some(3);
        ack.sha256 = Some("ab".repeat(32));
        let json = String::from_utf8(ack.to_json().unwrap()).unwrap();
        assert!(json.contains("\"action\":\"block-ack\""), "{json}");
        assert!(json.contains("\"blockIndex\":3") && json.contains("\"protocolVersion\":2"));
        assert!(!json.contains("chunkSize") && !json.contains("offset"));
        assert_eq!(FileControl::from_json(json.as_bytes()).unwrap(), ack);
        for (action, name) in [
            (FileAction::Offer, "offer"),
            (FileAction::Accept, "accept"),
            (FileAction::BlockAck, "block-ack"),
            (FileAction::BlockNack, "block-nack"),
            (FileAction::Pause, "pause"),
            (FileAction::Resume, "resume"),
            (FileAction::Done, "done"),
            (FileAction::Complete, "complete"),
            (FileAction::Cancel, "cancel"),
            (FileAction::Error, "error"),
        ] {
            assert_eq!(
                serde_json::to_string(&action).unwrap(),
                format!("\"{name}\"")
            );
        }
    }

    #[test]
    fn an_offer_validates_and_round_trips() {
        let control = offer();
        control.validate(&wire()).unwrap();
        assert_eq!(
            FileControl::from_json(&control.to_json().unwrap()).unwrap(),
            control
        );
        // Unknown fields from a newer peer are ignored.
        let mut json = String::from_utf8(control.to_json().unwrap()).unwrap();
        json.insert_str(1, "\"somethingNew\":1,");
        assert!(FileControl::from_json(json.as_bytes()).is_ok());
    }

    #[test]
    fn a_synced_file_offer_names_its_path_safely() {
        let synced = |relpath: &str, filename: &str, folder: &str| FileControl {
            filename: Some(filename.into()),
            folder: Some(folder.into()),
            relpath: Some(relpath.into()),
            content_sha256: Some("cd".repeat(32)),
            ..offer()
        };
        assert!(synced("docs/a.txt", "a.txt", "pair-1")
            .validate(&wire())
            .is_ok());
        assert!(synced("docs/a.txt", "b.txt", "pair-1")
            .validate(&wire())
            .is_err());
        assert!(synced("../a.txt", "a.txt", "pair-1")
            .validate(&wire())
            .is_err());
        assert!(synced("docs/a.txt", "a.txt", "pair/1")
            .validate(&wire())
            .is_err());
        assert!(synced("docs/a.txt", "a.txt", "").validate(&wire()).is_err());
        // The three fields come together.
        let mut partial = synced("a.txt", "a.txt", "p");
        partial.content_sha256 = None;
        assert!(partial.validate(&wire()).is_err());
        let reference = synced("d/a.txt", "a.txt", "p").folder_ref().unwrap();
        assert_eq!(reference.relpath, "d/a.txt");
        assert_eq!(reference.folder, "p");
        assert!(offer().folder_ref().is_none());
        let plain = String::from_utf8(offer().to_json().unwrap()).unwrap();
        assert!(!plain.contains("relpath") && !plain.contains("contentSha256"));
    }

    #[test]
    fn control_rejects_every_field_that_breaks_the_contract() {
        type Mutation = Box<dyn Fn(&mut FileControl)>;
        let bad: Vec<(&str, Mutation)> = vec![
            ("version", Box::new(|c| c.protocol_version = 1)),
            ("device", Box::new(|c| c.device_id = "other".to_string())),
            ("session", Box::new(|c| c.session_id += 1)),
            ("generation", Box::new(|c| c.connection_generation += 1)),
            ("empty id", Box::new(|c| c.transfer_id = String::new())),
            ("path id", Box::new(|c| c.transfer_id = "../x".to_string())),
            (
                "upper-case id",
                Box::new(|c| c.transfer_id = ID.to_uppercase()),
            ),
            (
                "unhyphenated id",
                Box::new(|c| c.transfer_id = ID.replace('-', "")),
            ),
            (
                "empty token",
                Box::new(|c| c.transfer_token = String::new()),
            ),
            (
                "long token",
                Box::new(|c| c.transfer_token = "t".repeat(257)),
            ),
            (
                "non-ascii token",
                Box::new(|c| c.transfer_token = "tökén".to_string()),
            ),
            (
                "huge size",
                Box::new(|c| c.total_size = Some(MAX_TRANSFER_BYTES + 1)),
            ),
            ("block size", Box::new(|c| c.block_size = Some(1024))),
            ("no block size", Box::new(|c| c.block_size = None)),
            ("tiny frame", Box::new(|c| c.max_frame_bytes = Some(72))),
            (
                "huge frame",
                Box::new(|c| c.max_frame_bytes = Some(262_145)),
            ),
            ("no name", Box::new(|c| c.filename = None)),
            ("no size", Box::new(|c| c.total_size = None)),
            ("no mtime", Box::new(|c| c.mtime = None)),
            (
                "short hash",
                Box::new(|c| c.content_sha256 = Some("ab".to_string())),
            ),
            (
                "long message",
                Box::new(|c| c.message = Some("e".repeat(MAX_MESSAGE_CHARS + 1))),
            ),
        ];
        for (label, mutate) in bad {
            let mut control = offer();
            mutate(&mut control);
            assert!(control.validate(&wire()).is_err(), "{label}");
        }
        // The documented ceiling itself, and an empty file, are valid.
        let mut at_limit = offer();
        at_limit.total_size = Some(MAX_TRANSFER_BYTES);
        at_limit.validate(&wire()).unwrap();
        let mut empty = offer();
        empty.total_size = Some(0);
        empty.validate(&wire()).unwrap();
    }

    #[test]
    fn each_action_needs_its_own_fields() {
        let check = |control: FileControl| control.validate(&wire());
        assert!(check(from_phone(FileAction::Accept)).is_err());
        let mut accept = from_phone(FileAction::Accept);
        accept.resume_block = Some(0);
        accept.prefix_sha256 = Some(hex(&hash_of_hashes(&[])));
        assert!(check(accept).is_ok());
        assert!(check(from_phone(FileAction::BlockAck)).is_err());
        assert!(check(from_phone(FileAction::BlockNack)).is_err());
        assert!(check(from_phone(FileAction::Done)).is_err());
        assert!(check(from_phone(FileAction::Complete)).is_err());
        // Pause, cancel, error and a bare resume need nothing.
        for action in [
            FileAction::Pause,
            FileAction::Cancel,
            FileAction::Error,
            FileAction::Resume,
        ] {
            assert!(check(from_phone(action)).is_ok(), "{action:?}");
        }
        // A receiver's resume gives all three of its fields, or none.
        let mut partial = from_phone(FileAction::Resume);
        partial.resume_block = Some(2);
        assert!(check(partial).is_err());
    }

    #[test]
    fn filenames_that_could_leave_the_downloads_directory_are_refused() {
        let too_long = "a".repeat(256);
        // 128 two-byte characters: short in characters, over the byte limit.
        let too_long_in_bytes = "é".repeat(128);
        for name in [
            "",
            ".",
            "..",
            "../x",
            "..\\x",
            "/etc/passwd",
            "dir/file",
            "dir\\file",
            "C:\\Windows\\x",
            "a\0b",
            "\0",
            "x/",
            "/",
            "\\",
            too_long.as_str(),
            too_long_in_bytes.as_str(),
        ] {
            assert!(validate_filename(name).is_err(), "{name:?} must be refused");
            let mut control = offer();
            control.filename = Some(name.to_string());
            assert!(control.validate(&wire()).is_err(), "{name:?} in an offer");
        }
        let longest = "a".repeat(255);
        for name in [
            "...",
            "..a",
            ".hidden",
            "a b",
            "ünï.txt",
            "-rf",
            longest.as_str(),
        ] {
            assert!(validate_filename(name).is_ok(), "{name:?} is harmless");
        }
    }

    #[test]
    fn control_parses_the_json_android_sends() {
        let json = format!(
            r#"{{"protocolVersion":2,"action":"block-nack","transferId":"{ID}",
            "deviceId":"phone-device","sessionId":9,"connectionGeneration":2,
            "transferToken":"token","blockIndex":4,"pass":1,"reason":"hash-mismatch"}}"#
        );
        let control = FileControl::from_json(json.as_bytes()).unwrap();
        assert_eq!(control.action, FileAction::BlockNack);
        assert_eq!(control.pass, Some(1));
        control.validate(&wire()).unwrap();
        for junk in [
            &b""[..],
            b"{",
            b"[]",
            b"null",
            br#"{"action":"delete"}"#,
            br#"{"protocolVersion":2,"action":"OFFER","transferId":"t","deviceId":"d","sessionId":1,"connectionGeneration":1,"transferToken":"k"}"#,
            br#"{"protocolVersion":2,"action":"acknowledge","transferId":"t","deviceId":"d","sessionId":1,"connectionGeneration":1,"transferToken":"k"}"#,
            br#"{"protocolVersion":2,"action":"offer","transferId":"t","deviceId":"d","sessionId":-1,"connectionGeneration":1,"transferToken":"k"}"#,
        ] {
            assert!(FileControl::from_json(junk).is_err(), "{junk:?}");
        }
    }

    #[test]
    fn a_peer_messages_text_is_bounded_and_stripped() {
        let mut control = from_phone(FileAction::Error);
        assert_eq!(control.peer_message(), None);
        control.message = Some("Disk\u{7} full\n".to_string());
        assert_eq!(control.peer_message().as_deref(), Some("Disk full"));
        control.message = Some("x".repeat(1000));
        assert_eq!(control.peer_message().unwrap().chars().count(), 256);
        control.message = Some("   ".to_string());
        assert_eq!(control.peer_message(), None);
    }

    fn header(offset: u64, length: u32, last: bool) -> FrameHeader {
        FrameHeader {
            last_of_block: last,
            pass: 7,
            transfer: id_bytes(ID).unwrap(),
            block_index: (offset / BLOCK_BYTES) as u32,
            offset,
            length,
        }
    }

    fn encode(header: &FrameHeader, payload: &[u8], trailer: Option<[u8; 32]>) -> Vec<u8> {
        let mut out = Vec::new();
        header.write(&mut out);
        out.extend_from_slice(payload);
        if let Some(trailer) = trailer {
            out.extend_from_slice(&trailer);
        }
        out
    }

    #[test]
    fn a_frame_has_the_documented_layout() {
        let h = header(BLOCK_BYTES, 5, true);
        let bytes = encode(&h, b"hello", Some([9; 32]));
        assert_eq!(bytes.len(), 40 + 5 + 32);
        assert_eq!(&bytes[..4], b"DLF2");
        assert_eq!(bytes[4], 1, "LAST_OF_BLOCK");
        assert_eq!(bytes[5], 7, "pass");
        assert_eq!(&bytes[6..8], &[0, 0], "reserved");
        assert_eq!(&bytes[8..24], &id_bytes(ID).unwrap());
        assert_eq!(&bytes[24..28], &1u32.to_be_bytes(), "block index");
        assert_eq!(&bytes[28..36], &BLOCK_BYTES.to_be_bytes(), "offset");
        assert_eq!(&bytes[36..40], &5u32.to_be_bytes(), "length");
        assert_eq!(&bytes[40..45], b"hello");
        assert_eq!(&bytes[45..], &[9u8; 32]);
        let parsed = parse_frame(&bytes).unwrap();
        assert_eq!(parsed.header, h);
        assert_eq!(parsed.payload, b"hello");
        assert_eq!(parsed.trailer, Some([9; 32]));
        // Without the flag there is no trailer.
        let middle = encode(&header(0, 5, false), b"hello", None);
        assert_eq!(middle[4], 0);
        assert_eq!(parse_frame(&middle).unwrap().trailer, None);
        assert_eq!(id_string(&id_bytes(ID).unwrap()), ID);
    }

    #[test]
    fn hostile_frames_are_rejected_without_panicking() {
        let good = encode(&header(0, 5, false), b"hello", None);
        assert!(parse_frame(&good).is_ok());
        // Every truncation, and an extension, of a valid frame.
        for length in 0..good.len() {
            assert!(parse_frame(&good[..length]).is_err(), "cut to {length}");
        }
        let mut longer = good.clone();
        longer.push(0);
        assert!(parse_frame(&longer).is_err());
        // Magic, flag bits, reserved bytes, lengths that lie.
        for (at, value) in [
            (0usize, b'X'),
            (3, b'1'),
            (4, 2),
            (4, 0x80),
            (6, 1),
            (7, 1),
            (39, 99),
        ] {
            let mut bad = good.clone();
            bad[at] = value;
            assert!(parse_frame(&bad).is_err(), "byte {at}");
        }
        // The flag without its trailer, and a trailer without the flag.
        let mut flagged = good.clone();
        flagged[4] = 1;
        assert!(parse_frame(&flagged).is_err());
        let with_trailer = encode(&header(0, 5, false), b"hello", Some([0; 32]));
        assert!(parse_frame(&with_trailer).is_err());
        // A zero-length frame is never valid (a 0-byte file has none).
        assert!(parse_frame(&encode(&header(0, 0, false), b"", None)).is_err());
        // The V1 magic is gone.
        let mut v1 = good;
        v1[3] = b'1';
        assert!(parse_frame(&v1).is_err());
    }

    #[test]
    fn a_frame_must_fit_its_block_and_its_file() {
        let total = 2 * BLOCK_BYTES + 10;
        let last_in_block = |offset: u64, length: u32| {
            let end = offset + u64::from(length);
            end % BLOCK_BYTES == 0 || end == total
        };
        let ok = |offset: u64, length: u32| {
            header(offset, length, last_in_block(offset, length)).check(total)
        };
        assert!(ok(0, 100).is_ok());
        assert!(ok(BLOCK_BYTES - 100, 100).is_ok(), "ends block 0");
        assert!(ok(BLOCK_BYTES, 61_368).is_ok(), "first frame of block 1");
        assert!(ok(2 * BLOCK_BYTES, 10).is_ok(), "the short last block");
        // Past the end of the file, and starting at it.
        assert!(header(2 * BLOCK_BYTES, 11, true).check(total).is_err());
        assert!(header(total, 1, true).check(total).is_err());
        // Crossing a block boundary.
        assert!(header(BLOCK_BYTES - 10, 20, false).check(total).is_err());
        // The block index must agree with the offset.
        let mut wrong = header(BLOCK_BYTES, 100, false);
        wrong.block_index = 0;
        assert!(wrong.check(total).is_err());
        // LAST_OF_BLOCK exactly at a block or file end.
        assert!(header(0, 100, true).check(total).is_err());
        assert!(header(BLOCK_BYTES - 100, 100, false).check(total).is_err());
        assert!(header(2 * BLOCK_BYTES, 10, false).check(total).is_err());
        // An empty file has no valid frame.
        assert!(header(0, 1, true).check(0).is_err());
        // Overflowing arithmetic is an error, not a panic.
        let huge = FrameHeader {
            offset: u64::MAX,
            ..header(0, 5, false)
        };
        assert!(huge.check(u64::MAX).is_err());
    }

    #[test]
    fn pass_wraps_at_256() {
        assert_eq!(next_pass(0), 1);
        assert_eq!(next_pass(255), 0);
    }

    #[test]
    fn only_canonical_lowercase_uuids_are_transfer_ids() {
        assert!(id_bytes(ID).is_some());
        assert!(id_bytes(&ID.to_uppercase()).is_none());
        assert!(id_bytes("transfer_1").is_none());
        assert!(id_bytes(&ID.replace('-', "")).is_none());
        assert!(id_bytes("").is_none());
    }

    // ── Golden vectors, pinned identically on both sides ──────────────────
    // (the phone's tests carry the same bytes and values)

    const TOKEN: &str = "00112233445566778899aabbccddeeff";

    fn unhex(text: &str) -> Vec<u8> {
        let text: String = text.split_whitespace().collect();
        (0..text.len() / 2)
            .map(|at| u8::from_str_radix(&text[at * 2..at * 2 + 2], 16).unwrap())
            .collect()
    }

    /// F1: a whole 3-byte file "abc", pass 0, one LAST_OF_BLOCK frame.
    #[test]
    fn golden_f1_a_whole_three_byte_file_is_one_75_byte_frame() {
        let expected = unhex(
            "444c4632 01 00 0000 0f8fad5bd9cb469fa16570867728950e 00000000 0000000000000000 00000003 616263
             ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        );
        assert_eq!(expected.len(), 75);
        let header = FrameHeader {
            last_of_block: true,
            pass: 0,
            transfer: id_bytes(ID).unwrap(),
            block_index: 0,
            offset: 0,
            length: 3,
        };
        let mut made = Vec::new();
        header.write(&mut made);
        made.extend_from_slice(b"abc");
        made.extend_from_slice(&sha256(b"abc"));
        assert_eq!(made, expected);
        let parsed = parse_frame(&expected).unwrap();
        assert_eq!(parsed.header, header);
        assert_eq!(parsed.payload, b"abc");
        assert_eq!(parsed.trailer, Some(sha256(b"abc")));
        header.check(3).unwrap();
    }

    /// F2: a middle frame, pass 7, block 1 of a 2 MiB file, no trailer.
    #[test]
    fn golden_f2_a_middle_frame_of_block_one_has_no_trailer() {
        let expected = unhex(
            "444c4632 00 07 0000 0f8fad5bd9cb469fa16570867728950e 00000001 0000000000100000 00000005 68656c6c6f",
        );
        assert_eq!(expected.len(), 45);
        let header = FrameHeader {
            last_of_block: false,
            pass: 7,
            transfer: id_bytes(ID).unwrap(),
            block_index: 1,
            offset: BLOCK_BYTES,
            length: 5,
        };
        let mut made = Vec::new();
        header.write(&mut made);
        made.extend_from_slice(b"hello");
        assert_eq!(made, expected);
        let parsed = parse_frame(&expected).unwrap();
        assert_eq!(
            (parsed.header, parsed.payload, parsed.trailer),
            (header, &b"hello"[..], None)
        );
        header.check(2 * BLOCK_BYTES).unwrap();
    }

    /// F3: the size rules.
    #[test]
    fn golden_f3_the_largest_frame_is_61440_bytes_and_the_rules_hold() {
        assert_eq!(FRAME_BYTES_PROVEN, 61_440);
        assert_eq!(
            FRAME_HEADER_BYTES + payload_capacity(61_440) + BLOCK_TRAILER_BYTES,
            61_440
        );
        assert_eq!(payload_capacity(61_440), 61_368);
        assert!(message_fits(61_440));
        assert!(!message_fits(61_441), "over 61 440 is rejected");
        // A LAST frame with a missing trailer.
        let f1 = unhex(
            "444c4632 01 00 0000 0f8fad5bd9cb469fa16570867728950e 00000000 0000000000000000 00000003 616263",
        );
        assert!(parse_frame(&f1).is_err());
        // A non-LAST frame that ends exactly at a block end.
        let at_end = FrameHeader {
            last_of_block: false,
            pass: 0,
            transfer: id_bytes(ID).unwrap(),
            block_index: 0,
            offset: BLOCK_BYTES - 5,
            length: 5,
        };
        assert!(at_end.check(2 * BLOCK_BYTES).is_err());
        // Nonzero reserved bytes, unknown flag bits.
        let good = unhex(
            "444c4632 00 07 0000 0f8fad5bd9cb469fa16570867728950e 00000001 0000000000100000 00000005 68656c6c6f",
        );
        for at in [6usize, 7] {
            let mut bad = good.clone();
            bad[at] = 1;
            assert!(parse_frame(&bad).is_err(), "reserved byte {at}");
        }
        for flag in [2u8, 4, 0x80, 0x81] {
            let mut bad = good.clone();
            bad[4] = flag;
            assert!(parse_frame(&bad).is_err(), "flag {flag:#x}");
        }
    }

    /// H: the hashes.
    #[test]
    fn golden_h_hashes() {
        let empty = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(hex(&hash_of_hashes(&[])), empty, "n = 0 hashes");
        assert_eq!(hex(&sha256(b"")), empty, "the 0-byte file");
        let block = sha256(b"abc");
        assert_eq!(
            hex(&block),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&hash_of_hashes(&[block])),
            "4f8b42c22dd3729b519ba6f68d2da7cc5b2d606d05daed5ad5128cc03e6c6358",
            "fileHash of \"abc\" = SHA-256 of the 32 raw block-hash bytes"
        );
        assert_eq!(
            (blocks_for(BLOCK_BYTES), blocks_for(BLOCK_BYTES + 1)),
            (1, 2)
        );
        assert_eq!(blocks_for(0), 0);
    }

    fn vector(json: &str) -> FileControl {
        FileControl::from_json(json.as_bytes()).unwrap_or_else(|error| panic!("{json}: {error}"))
    }

    fn common(action: &str) -> String {
        format!(
            r#""protocolVersion":2,"action":"{action}","transferId":"{ID}","deviceId":"phone-device","sessionId":9,"connectionGeneration":2,"transferToken":"{TOKEN}""#
        )
    }

    /// C: the control JSON, compared as parsed values.
    #[test]
    fn golden_c_control_json_parses_validates_and_round_trips() {
        let prefix = "ab".repeat(32);
        let sha = "cd".repeat(32);
        type Check = Box<dyn Fn(&FileControl)>;
        let vectors: Vec<(String, Check)> = vec![
            (
                format!(
                    r#"{{{},"filename":"big.iso","totalSize":4294967296,"mtime":1700000000000,"blockSize":1048576,"maxFrameBytes":61440}}"#,
                    common("offer")
                ),
                Box::new(|c| {
                    assert_eq!(c.action, FileAction::Offer);
                    assert_eq!(c.total_size, Some(4_294_967_296));
                    assert_eq!(c.mtime, Some(1_700_000_000_000));
                    assert_eq!(
                        (c.block_size, c.max_frame_bytes),
                        (Some(1_048_576), Some(61_440))
                    );
                }),
            ),
            (
                format!(
                    r#"{{{},"filename":"a.jpg","totalSize":10,"mtime":1,"blockSize":1048576,"maxFrameBytes":61440,"folder":"bucket","relpath":"Trips/a.jpg","contentSha256":"{sha}"}}"#,
                    common("offer")
                ),
                Box::new(|c| {
                    assert_eq!(c.folder_ref().unwrap().relpath, "Trips/a.jpg");
                    assert_eq!(c.content_sha256.as_deref(), Some("cd".repeat(32).as_str()));
                }),
            ),
            (
                format!(
                    r#"{{{},"filename":"clip.png","totalSize":10,"mtime":1,"blockSize":1048576,"maxFrameBytes":61440,"clipboard":true,"mimeType":"image/png"}}"#,
                    common("offer")
                ),
                Box::new(|c| {
                    assert_eq!(c.clipboard, Some(true));
                    assert_eq!(c.mime_type.as_deref(), Some("image/png"));
                }),
            ),
            (
                format!(
                    r#"{{{},"resumeBlock":3,"prefixSha256":"{prefix}","maxFrameBytes":61440}}"#,
                    common("accept")
                ),
                Box::new(|c| {
                    assert_eq!((c.resume_block, c.max_frame_bytes), (Some(3), Some(61_440)))
                }),
            ),
            (
                format!(
                    r#"{{{},"blockIndex":2,"sha256":"{sha}"}}"#,
                    common("block-ack")
                ),
                Box::new(|c| assert_eq!(c.block_index, Some(2))),
            ),
            (
                format!(
                    r#"{{{},"blockIndex":4,"pass":1,"reason":"hash-mismatch"}}"#,
                    common("block-nack")
                ),
                Box::new(|c| {
                    assert_eq!((c.block_index, c.pass), (Some(4), Some(1)));
                    assert_eq!(c.reason.as_deref(), Some("hash-mismatch"));
                }),
            ),
            (
                format!(r#"{{{},"reason":"backlog"}}"#, common("pause")),
                Box::new(|c| assert_eq!(c.reason.as_deref(), Some("backlog"))),
            ),
            (
                format!(
                    r#"{{{},"resumeBlock":5,"prefixSha256":"{prefix}","pass":2}}"#,
                    common("resume")
                ),
                Box::new(|c| assert_eq!((c.resume_block, c.pass), (Some(5), Some(2)))),
            ),
            (
                format!(r#"{{{}}}"#, common("resume")),
                Box::new(|c| assert_eq!((c.resume_block, c.pass), (None, None))),
            ),
            (
                format!(
                    r#"{{{},"fileHash":"{sha}","blocks":4096,"totalSize":4294967296}}"#,
                    common("done")
                ),
                Box::new(|c| {
                    assert_eq!((c.blocks, c.total_size), (Some(4096), Some(4_294_967_296)))
                }),
            ),
            (
                format!(r#"{{{},"fileHash":"{sha}"}}"#, common("complete")),
                Box::new(|c| assert_eq!(c.file_hash.as_deref(), Some("cd".repeat(32).as_str()))),
            ),
        ];
        for (json, check) in &vectors {
            let control = vector(json);
            control
                .validate(&wire())
                .unwrap_or_else(|error| panic!("{json}: {error}"));
            check(&control);
            // Parsed values, not strings.
            let again = FileControl::from_json(&control.to_json().unwrap()).unwrap();
            assert_eq!(again, control, "{json}");
        }
        for code in ["user", "source-gone", "source-changed", "removed"] {
            let control = vector(&format!(
                r#"{{{},"code":"{code}","message":"x"}}"#,
                common("cancel")
            ));
            control.validate(&wire()).unwrap();
            assert_eq!(control.code.as_deref(), Some(code));
        }
        for code in [
            "refused",
            "busy",
            "too-large",
            "bad-frame",
            "hash-mismatch",
            "internal",
        ] {
            let control = vector(&format!(
                r#"{{{},"code":"{code}","message":"x"}}"#,
                common("error")
            ));
            control.validate(&wire()).unwrap();
            assert_eq!(control.code.as_deref(), Some(code));
        }
    }

    /// N: negative and tolerant cases.
    #[test]
    fn golden_n_partial_resumes_are_rejected_and_unknown_values_are_tolerated() {
        let prefix = "ab".repeat(32);
        for fields in [
            r#""resumeBlock":1"#.to_string(),
            r#""pass":1"#.to_string(),
            format!(r#""prefixSha256":"{prefix}""#),
            format!(r#""resumeBlock":1,"prefixSha256":"{prefix}""#),
            r#""resumeBlock":1,"pass":1"#.to_string(),
            format!(r#""prefixSha256":"{prefix}","pass":1"#),
        ] {
            let control = vector(&format!("{{{},{fields}}}", common("resume")));
            assert!(control.validate(&wire()).is_err(), "{fields}");
        }
        // Unknown codes and reasons parse, validate, and are a generic end
        // (cancel) or `user` (pause); they never throw.
        for action in ["cancel", "error"] {
            let control = vector(&format!(
                r#"{{{},"code":"brand-new-code"}}"#,
                common(action)
            ));
            control.validate(&wire()).unwrap();
        }
        let pause = vector(&format!(
            r#"{{{},"reason":"brand-new-reason"}}"#,
            common("pause")
        ));
        pause.validate(&wire()).unwrap();
    }
}
