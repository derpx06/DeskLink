//! Versioned, bounded WebRTC terminal records shared with DeskLink Android.
//!
//! Like [`super::file_protocol`], this module has no side effects: it validates
//! untrusted wire data before anything spawns a process or writes to a PTY.
//!
//! Two message types on the `desklink-terminal-v1` channel, mirroring the file
//! protocol's split — JSON for the rare control messages, a binary frame for
//! the hot path:
//!
//! * `desklink.terminal.control.v1` — list/create/attach/resize/close.
//! * `desklink.terminal.chunk.v1` — terminal bytes in either direction.
//!
//! Deliberately **no per-chunk checksum**, unlike the file protocol. A file
//! transfer is resumable and a corrupt byte is silent data loss; terminal
//! output rides an ordered, reliable SCTP channel inside an envelope already
//! bound to the device, session and generation, and hashing every frame at
//! interactive rates would buy nothing for real cost.

use serde::{Deserialize, Serialize};

use super::WebRtcWireBinding;

pub const TERMINAL_PROTOCOL_VERSION: u32 = 1;
pub const TERMINAL_CONTROL_MESSAGE_TYPE: &str = "desklink.terminal.control.v1";
pub const TERMINAL_CHUNK_MESSAGE_TYPE: &str = "desklink.terminal.chunk.v1";

/// The `error` a host answers with when the user has not allowed this phone to
/// open terminals on this computer (or has just turned that off).
///
/// It arrives as an ordinary `error` control message, and — when it is the
/// consequence of a revocation — as `closed` for each terminal the phone was
/// attached to. A phone should show "ask for terminal access on the computer"
/// rather than retry.
pub const TERMINAL_NOT_ALLOWED: &str = "terminal-not-allowed";
/// The desk ended this phone's terminal sessions ("End phone sessions").
/// Access is unchanged; the phone may attach again.
pub const TERMINAL_ENDED: &str = "terminal-ended";

/// Terminal **data** bytes per frame — what a [`TerminalChunk`] may carry.
///
/// This is the limit on `data`, not on what crosses the wire. A frame there is
/// the 19-byte header, the terminal id and the data, base64-encoded (x4/3) into
/// a JSON envelope: at most about 22.5 KB for a full chunk. That is *over* RFC
/// 8831 §6.6's 16 KB interoperability figure, which only binds browsers talking
/// to unknown peers; here both ends are DeskLink and both negotiate a far
/// larger message size. What matters is that a frame stays under the 64 KiB
/// GStreamer assumes when the SDP omits `max-message-size`, under libwebrtc's
/// 256 KiB, and under [`MAX_ENVELOPE_BYTES`](super::MAX_ENVELOPE_BYTES) —
/// `every_frame_fits_the_envelope_limit` pins all three.
pub const MAX_TERMINAL_CHUNK_BYTES: usize = 16 * 1024;

/// Ceiling on a client-supplied `argv`. Long enough for any real command,
/// short enough that a peer cannot make us allocate on a whim.
const MAX_COMMAND_ARGS: usize = 64;
const MAX_COMMAND_ARG_BYTES: usize = 4 * 1024;

/// Terminal geometry bounds. Mirrors the clamp in [`super::super::pty::session`]
/// so a hostile size is refused at the wire rather than quietly corrected.
const MAX_DIMENSION: u16 = 1000;

const CHUNK_HEADER_BYTES: usize = 19;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TerminalAction {
    /// Client asks for the session list.
    List,
    /// Host's reply to `List`, and an unsolicited push when the set changes.
    Sessions,
    /// Client asks for a new session.
    Create,
    /// Host's reply to `Create`.
    Created,
    /// Client asks to start receiving a session's output.
    Attach,
    /// Host's reply to `Attach`, naming where the byte stream resumes.
    Attached,
    /// Client stops receiving output. The session keeps running.
    Detach,
    /// Client ends a session deliberately.
    Close,
    /// Host reports a session ended, whether closed or exited on its own.
    Closed,
    /// Client's terminal viewport changed.
    Resize,
    /// Host could not honour a request.
    Error,
}

/// One session as shown in a client's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSessionInfo {
    pub terminal_id: String,
    pub title: String,
    pub rows: u16,
    pub cols: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<u32>,
}

/// A `desklink.terminal.control.v1` message.
///
/// Optional fields are **omitted, never emitted as `null` or `""`** — the
/// Android peer reads them with `optString(...).takeIf { it.isNotEmpty() }`, so
/// an explicit empty value is indistinguishable from absent and a `null` would
/// arrive as the string `"null"`. Same rule as the remote-session control plane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalControl {
    pub protocol_version: u32,
    pub action: TerminalAction,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rows: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cols: Option<u16>,
    /// Where the client's byte stream resumes, or where the host says it does.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// `argv` for `Create`. **Never a shell string**: it is passed as an
    /// argument vector, so nothing here is word-split, glob-expanded or
    /// interpreted by a shell. `None` means the user's login shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sessions: Option<Vec<TerminalSessionInfo>>,
    /// Agent CLIs installed at the desk (`"claude"`, `"codex"`), sent with
    /// `Sessions` so the phone can offer them as one-tap sessions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub launchers: Option<Vec<String>>,
    /// The session password, from a phone, when the desktop has one set.
    /// See `session_lock`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_status: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl TerminalControl {
    pub fn new(action: TerminalAction, wire: &WebRtcWireBinding) -> Self {
        Self {
            protocol_version: TERMINAL_PROTOCOL_VERSION,
            action,
            device_id: wire.sender_device_id.clone(),
            session_id: wire.session_id,
            connection_generation: wire.generation,
            terminal_id: None,
            rows: None,
            cols: None,
            offset: None,
            command: None,
            sessions: None,
            launchers: None,
            password: None,
            exit_status: None,
            error: None,
        }
    }

    pub fn from_json(input: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(input)
            .map_err(|error| format!("Malformed DeskLink terminal control: {error}"))
    }

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self)
            .map_err(|error| format!("Could not serialize DeskLink terminal control: {error}"))
    }

    /// Applies every rule before a handler acts on this message.
    ///
    /// The binding is re-stated inside the payload and re-checked against the
    /// envelope, belt-and-braces, exactly as the file protocol does: an
    /// envelope that passed validation still must not carry a body describing
    /// somebody else's session.
    pub fn validate(&self, wire: &WebRtcWireBinding) -> Result<(), String> {
        if self.protocol_version != TERMINAL_PROTOCOL_VERSION {
            return Err("Unsupported DeskLink WebRTC terminal version".to_string());
        }
        if self.device_id != wire.peer_device_id
            || self.session_id != wire.session_id
            || self.connection_generation != wire.generation
        {
            return Err("DeskLink WebRTC terminal binding mismatch".to_string());
        }
        if let Some(id) = &self.terminal_id {
            validate_terminal_id(id)?;
        }
        if matches!(
            self.action,
            TerminalAction::Attach | TerminalAction::Detach | TerminalAction::Close
        ) && self.terminal_id.is_none()
        {
            return Err("DeskLink terminal request needs a terminal id".to_string());
        }
        if self.action == TerminalAction::Resize {
            if self.terminal_id.is_none() {
                return Err("DeskLink terminal request needs a terminal id".to_string());
            }
            if self.rows.is_none() || self.cols.is_none() {
                return Err("DeskLink terminal resize needs both dimensions".to_string());
            }
        }
        validate_dimensions(self.rows, self.cols)?;
        if let Some(command) = &self.command {
            validate_command(command)?;
        }
        if self.password.as_ref().is_some_and(|password| {
            password.len() > crate::device_links::session_lock::MAX_PASSWORD_BYTES
        }) {
            return Err("DeskLink terminal password is too long".to_string());
        }
        if self.error.as_ref().is_some_and(|error| error.len() > 1024) {
            return Err("DeskLink terminal error is too long".to_string());
        }
        Ok(())
    }
}

/// What a [`TerminalChunk`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkKind {
    /// Host → client: bytes the program wrote.
    Output,
    /// Client → host: keystrokes, written to the PTY untouched.
    Input,
    /// Host → client: escape codes reproducing the current screen.
    Snapshot,
}

impl ChunkKind {
    const fn wire_value(self) -> u8 {
        match self {
            Self::Output => 0,
            Self::Input => 1,
            Self::Snapshot => 2,
        }
    }

    fn parse(value: u8) -> Result<Self, String> {
        Ok(match value {
            0 => Self::Output,
            1 => Self::Input,
            2 => Self::Snapshot,
            _ => return Err("Unknown DeskLink terminal chunk kind".to_string()),
        })
    }
}

/// Terminal bytes on the wire.
///
/// `data` is opaque. It is never decoded as text here and must not be: PTY
/// output is whatever the program wrote, including NUL bytes and partial UTF-8
/// sequences that only complete in the next frame. Both ends feed it to a
/// byte-oriented VT parser, which is stateful across frames, so a split
/// anywhere — mid-character, mid-escape-sequence — is safe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalChunk {
    pub kind: ChunkKind,
    pub terminal_id: String,
    /// Byte offset of `data[0]` in the session's output stream. Meaningless for
    /// [`ChunkKind::Input`], which has no stream of its own.
    pub offset: u64,
    pub data: Vec<u8>,
}

impl TerminalChunk {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        validate_terminal_id(&self.terminal_id)?;
        validate_chunk_data(&self.data)?;
        let id = self.terminal_id.as_bytes();
        let id_length =
            u16::try_from(id.len()).map_err(|_| "DeskLink terminal id is too long".to_string())?;
        let data_length = u32::try_from(self.data.len())
            .map_err(|_| "DeskLink terminal chunk is too large".to_string())?;

        let mut result = Vec::with_capacity(CHUNK_HEADER_BYTES + id.len() + self.data.len());
        result.extend_from_slice(b"DLT1");
        result.push(self.kind.wire_value());
        result.extend_from_slice(&id_length.to_be_bytes());
        result.extend_from_slice(&self.offset.to_be_bytes());
        result.extend_from_slice(&data_length.to_be_bytes());
        result.extend_from_slice(id);
        result.extend_from_slice(&self.data);
        Ok(result)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, String> {
        if encoded.len() < CHUNK_HEADER_BYTES || &encoded[..4] != b"DLT1" {
            return Err("Malformed DeskLink WebRTC terminal chunk".to_string());
        }
        let kind = ChunkKind::parse(encoded[4])?;
        let id_length = usize::from(u16::from_be_bytes([encoded[5], encoded[6]]));
        let offset = u64::from_be_bytes(
            encoded[7..15]
                .try_into()
                .map_err(|_| "Malformed DeskLink WebRTC terminal chunk".to_string())?,
        );
        let data_length = usize::try_from(u32::from_be_bytes(
            encoded[15..19]
                .try_into()
                .map_err(|_| "Malformed DeskLink WebRTC terminal chunk".to_string())?,
        ))
        .map_err(|_| "Invalid DeskLink terminal chunk size".to_string())?;
        // Compare against the declared lengths before slicing, so a header that
        // claims more than it carries is refused rather than panicking.
        let expected_length = CHUNK_HEADER_BYTES
            .checked_add(id_length)
            .and_then(|value| value.checked_add(data_length))
            .ok_or_else(|| "Malformed DeskLink WebRTC terminal chunk".to_string())?;
        if expected_length != encoded.len() {
            return Err("Malformed DeskLink WebRTC terminal chunk".to_string());
        }
        let id_end = CHUNK_HEADER_BYTES + id_length;
        let terminal_id = std::str::from_utf8(&encoded[CHUNK_HEADER_BYTES..id_end])
            .map_err(|_| "Invalid DeskLink terminal id".to_string())?
            .to_string();
        let data = encoded[id_end..].to_vec();
        validate_terminal_id(&terminal_id)?;
        validate_chunk_data(&data)?;
        Ok(Self {
            kind,
            terminal_id,
            offset,
            data,
        })
    }
}

fn validate_terminal_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err("Invalid DeskLink terminal id".to_string());
    }
    Ok(())
}

fn validate_chunk_data(data: &[u8]) -> Result<(), String> {
    if data.len() > MAX_TERMINAL_CHUNK_BYTES {
        return Err("DeskLink terminal chunk is too large".to_string());
    }
    Ok(())
}

fn validate_dimensions(rows: Option<u16>, cols: Option<u16>) -> Result<(), String> {
    for value in [rows, cols].into_iter().flatten() {
        if value == 0 || value > MAX_DIMENSION {
            return Err("DeskLink terminal size is out of range".to_string());
        }
    }
    Ok(())
}

fn validate_command(command: &[String]) -> Result<(), String> {
    if command.is_empty() {
        return Err("DeskLink terminal command is empty".to_string());
    }
    if command.len() > MAX_COMMAND_ARGS {
        return Err("DeskLink terminal command has too many arguments".to_string());
    }
    if command
        .iter()
        .any(|argument| argument.len() > MAX_COMMAND_ARG_BYTES)
    {
        return Err("DeskLink terminal command argument is too long".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire() -> WebRtcWireBinding {
        WebRtcWireBinding {
            sender_device_id: "desktop-1".to_string(),
            peer_device_id: "phone-1".to_string(),
            session_id: 7,
            generation: 3,
        }
    }

    fn inbound(action: TerminalAction) -> TerminalControl {
        let mut control = TerminalControl::new(action, &wire());
        // An inbound message carries the *peer's* device id.
        control.device_id = "phone-1".to_string();
        control
    }

    #[test]
    fn a_control_message_round_trips() {
        let mut control = inbound(TerminalAction::Create);
        control.rows = Some(24);
        control.cols = Some(80);
        control.command = Some(vec!["tmux".to_string(), "attach".to_string()]);
        let encoded = control.to_json().unwrap();
        assert_eq!(TerminalControl::from_json(&encoded).unwrap(), control);
        control.validate(&wire()).unwrap();
    }

    /// Absent fields must not be serialized. The Android peer cannot tell an
    /// explicit empty string from an absent one, and a `null` reaches it as the
    /// literal text `"null"`.
    #[test]
    fn optional_fields_are_omitted_rather_than_emitted_empty() {
        let control = inbound(TerminalAction::List);
        let json = String::from_utf8(control.to_json().unwrap()).unwrap();
        for absent in [
            "terminalId",
            "rows",
            "cols",
            "offset",
            "command",
            "sessions",
            "exitStatus",
            "error",
            "launchers",
            "password",
        ] {
            assert!(!json.contains(absent), "{absent} should have been omitted");
        }
        assert!(json.contains("\"action\":\"list\""));
    }

    /// The envelope is already bound to device, session and generation; the
    /// body re-states them so a valid envelope cannot smuggle a body describing
    /// a different session.
    #[test]
    fn a_stale_binding_is_rejected() {
        let mut control = inbound(TerminalAction::List);
        control.session_id = 8;
        assert!(control.validate(&wire()).is_err());

        let mut control = inbound(TerminalAction::List);
        control.connection_generation = 4;
        assert!(control.validate(&wire()).is_err());

        let mut control = inbound(TerminalAction::List);
        control.device_id = "someone-else".to_string();
        assert!(control.validate(&wire()).is_err());
    }

    #[test]
    fn an_unsupported_version_is_rejected() {
        let mut control = inbound(TerminalAction::List);
        control.protocol_version = 2;
        assert!(control.validate(&wire()).is_err());
    }

    /// Acting on an attach/close/resize with no terminal id would mean guessing
    /// which session the peer meant.
    #[test]
    fn requests_that_name_a_session_must_carry_its_id() {
        for action in [
            TerminalAction::Attach,
            TerminalAction::Detach,
            TerminalAction::Close,
        ] {
            assert!(inbound(action).validate(&wire()).is_err(), "{action:?}");
        }
        let mut resize = inbound(TerminalAction::Resize);
        resize.rows = Some(24);
        resize.cols = Some(80);
        assert!(resize.validate(&wire()).is_err());
    }

    #[test]
    fn a_resize_needs_both_dimensions() {
        let mut resize = inbound(TerminalAction::Resize);
        resize.terminal_id = Some("abc".to_string());
        resize.rows = Some(24);
        assert!(resize.validate(&wire()).is_err());
        resize.cols = Some(80);
        resize.validate(&wire()).unwrap();
    }

    /// Dimensions come from the network. Zero is rejected by the kernel and an
    /// absurd value would make the screen model allocate without bound.
    #[test]
    fn hostile_dimensions_are_refused() {
        let mut control = inbound(TerminalAction::Create);
        control.rows = Some(0);
        control.cols = Some(80);
        assert!(control.validate(&wire()).is_err());

        let mut control = inbound(TerminalAction::Create);
        control.rows = Some(24);
        control.cols = Some(u16::MAX);
        assert!(control.validate(&wire()).is_err());
    }

    #[test]
    fn an_unbounded_command_is_refused() {
        let mut control = inbound(TerminalAction::Create);
        control.command = Some(Vec::new());
        assert!(control.validate(&wire()).is_err());

        let mut control = inbound(TerminalAction::Create);
        control.command = Some(vec!["x".to_string(); MAX_COMMAND_ARGS + 1]);
        assert!(control.validate(&wire()).is_err());

        let mut control = inbound(TerminalAction::Create);
        control.command = Some(vec!["x".repeat(MAX_COMMAND_ARG_BYTES + 1)]);
        assert!(control.validate(&wire()).is_err());
    }

    #[test]
    fn a_chunk_round_trips_for_every_kind() {
        for kind in [ChunkKind::Output, ChunkKind::Input, ChunkKind::Snapshot] {
            let chunk = TerminalChunk {
                kind,
                terminal_id: "0123abcd-ef".to_string(),
                offset: 4_294_967_296,
                data: vec![0x00, 0x1b, 0x5b, 0xff, 0xc3],
            };
            let encoded = chunk.encode().unwrap();
            assert_eq!(TerminalChunk::decode(&encoded).unwrap(), chunk);
        }
    }

    /// Terminal bytes are not text. A NUL, a lone continuation byte, or a
    /// truncated multi-byte sequence must survive the wire untouched — the VT
    /// parser at the far end is byte-oriented and stateful across frames.
    #[test]
    fn arbitrary_bytes_survive_encoding() {
        let data: Vec<u8> = (0u8..=255).collect();
        let chunk = TerminalChunk {
            kind: ChunkKind::Output,
            terminal_id: "abc".to_string(),
            offset: 0,
            data: data.clone(),
        };
        let decoded = TerminalChunk::decode(&chunk.encode().unwrap()).unwrap();
        assert_eq!(decoded.data, data);
    }

    /// A header that claims more payload than it carries must be refused, not
    /// slice out of bounds.
    #[test]
    fn a_truncated_or_lying_chunk_is_rejected() {
        let chunk = TerminalChunk {
            kind: ChunkKind::Output,
            terminal_id: "abc".to_string(),
            offset: 0,
            data: b"hello".to_vec(),
        };
        let encoded = chunk.encode().unwrap();

        assert!(TerminalChunk::decode(&encoded[..encoded.len() - 1]).is_err());
        assert!(TerminalChunk::decode(&encoded[..3]).is_err());

        let mut wrong_magic = encoded.clone();
        wrong_magic[..4].copy_from_slice(b"DLF1");
        assert!(TerminalChunk::decode(&wrong_magic).is_err());

        let mut lying_length = encoded.clone();
        lying_length[15..19].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(TerminalChunk::decode(&lying_length).is_err());

        let mut unknown_kind = encoded;
        unknown_kind[4] = 9;
        assert!(TerminalChunk::decode(&unknown_kind).is_err());
    }

    #[test]
    fn an_oversized_chunk_is_refused() {
        let chunk = TerminalChunk {
            kind: ChunkKind::Output,
            terminal_id: "abc".to_string(),
            offset: 0,
            data: vec![0; MAX_TERMINAL_CHUNK_BYTES + 1],
        };
        assert!(chunk.encode().is_err());
    }

    #[test]
    fn a_malformed_terminal_id_is_refused() {
        for id in ["", "has space", "../etc/passwd", &"x".repeat(129)] {
            let chunk = TerminalChunk {
                kind: ChunkKind::Output,
                terminal_id: id.to_string(),
                offset: 0,
                data: b"x".to_vec(),
            };
            assert!(chunk.encode().is_err(), "{id:?} should be refused");
        }
    }

    /// The wire frame is larger than [`MAX_TERMINAL_CHUNK_BYTES`]: base64 and
    /// the envelope add roughly 37%. Nothing this module can encode may be
    /// refused by the transport for size, or the sender's pump would stop.
    #[test]
    fn every_frame_fits_the_envelope_limit() {
        use super::super::{WebRtcChannel, WebRtcEnvelope, MAX_ENVELOPE_BYTES, MAX_PAYLOAD_BYTES};

        let chunk = TerminalChunk {
            kind: ChunkKind::Output,
            terminal_id: "x".repeat(128),
            offset: u64::MAX,
            data: vec![0xff; MAX_TERMINAL_CHUNK_BYTES],
        };
        let payload = chunk.encode().unwrap();
        assert!(payload.len() <= MAX_PAYLOAD_BYTES);
        let envelope = WebRtcEnvelope::create(
            &wire(),
            WebRtcChannel::Terminal,
            TERMINAL_CHUNK_MESSAGE_TYPE,
            &payload,
            i64::MAX,
        )
        .unwrap();
        let text = envelope.to_json().unwrap().len();
        assert!(text < 24 * 1024, "a full frame is {text} bytes of text");
        assert!(text < 64 * 1024, "over GStreamer's default message size");
        assert!(text <= MAX_ENVELOPE_BYTES);
    }
}
