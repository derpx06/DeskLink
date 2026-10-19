//! Versioned, bounded WebRTC file-transfer records shared with DeskLink
//! Android. This module contains no filesystem effects: it validates the
//! untrusted wire data before the transfer manager touches a checkpoint or
//! temporary file.

use openssl::hash::{hash, MessageDigest};
use serde::{Deserialize, Serialize};

use super::WebRtcWireBinding;

pub const FILE_PROTOCOL_VERSION: u32 = 1;
pub const FILE_CONTROL_MESSAGE_TYPE: &str = "desklink.file.control.v1";
pub const FILE_CHUNK_MESSAGE_TYPE: &str = "desklink.file.chunk.v1";
/// Largest chunk either side accepts. Peers negotiate up to this through
/// `max_chunk_size`; a peer that never advertises it is sent
/// `LEGACY_FILE_CHUNK_BYTES`, the only size builds before it accept.
///
/// Sized so the whole message fits in 64 KiB, the data-channel maximum
/// webrtcbin assumes when an SDP carries no `a=max-message-size`: 45 KiB of
/// data plus the DLF1 header, base64 (×4/3) and the JSON envelope is ~62 KB.
pub const MAX_FILE_CHUNK_BYTES: usize = 45 * 1024;
pub const LEGACY_FILE_CHUNK_BYTES: usize = 16 * 1024;
pub const MAX_TRANSFER_BYTES: u64 = 4 * 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileAction {
    Offer,
    Accept,
    Acknowledge,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub offset: u64,
    /// Always `LEGACY_FILE_CHUNK_BYTES` on the wire, so older peers validate.
    pub chunk_size: usize,
    /// Chunk-size upgrade, ignored by older peers: the sender's Offer carries
    /// the largest chunk it can send, and a receiver that understands it
    /// answers with the size it accepts in its Accept. Absent ⇒ legacy size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chunk_size: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FileControl {
    pub fn from_json(input: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(input)
            .map_err(|error| format!("Malformed DeskLink file control: {error}"))
    }

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self)
            .map_err(|error| format!("Could not serialize DeskLink file control: {error}"))
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
        if self.chunk_size == 0 || self.chunk_size > MAX_FILE_CHUNK_BYTES {
            return Err("Invalid DeskLink WebRTC file chunk size".to_string());
        }
        if self
            .max_chunk_size
            .is_some_and(|size| size == 0 || size > MAX_FILE_CHUNK_BYTES)
        {
            return Err("Invalid DeskLink WebRTC file chunk size".to_string());
        }
        if let Some(total_size) = self.total_size {
            if total_size > MAX_TRANSFER_BYTES || self.offset > total_size {
                return Err("Invalid DeskLink WebRTC transfer size or offset".to_string());
            }
        }
        if let Some(checksum) = &self.sha256 {
            if checksum.len() != 64 || !checksum.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err("Invalid DeskLink WebRTC file checksum".to_string());
            }
        }
        if self.action == FileAction::Offer {
            validate_filename(
                self.filename
                    .as_deref()
                    .ok_or_else(|| "Malformed DeskLink WebRTC file offer".to_string())?,
            )?;
            if self.total_size.is_none() || self.sha256.is_none() || self.offset != 0 {
                return Err("Malformed DeskLink WebRTC file offer".to_string());
            }
        }
        if self.error.as_ref().is_some_and(|error| error.len() > 4096) {
            return Err("Oversized DeskLink WebRTC file error".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChunk {
    pub transfer_id: String,
    pub transfer_token: String,
    pub offset: u64,
    pub data: Vec<u8>,
}

impl FileChunk {
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        validate_id(&self.transfer_id)?;
        validate_token(&self.transfer_token)?;
        validate_chunk_data(&self.data)?;
        let id = self.transfer_id.as_bytes();
        let token = self.transfer_token.as_bytes();
        let id_length =
            u16::try_from(id.len()).map_err(|_| "DeskLink transfer ID is too long".to_string())?;
        let token_length = u16::try_from(token.len())
            .map_err(|_| "DeskLink transfer token is too long".to_string())?;
        let data_length = u32::try_from(self.data.len())
            .map_err(|_| "DeskLink file-data chunk is too large".to_string())?;
        let digest = hash(MessageDigest::sha256(), &self.data)
            .map_err(|error| format!("Could not checksum DeskLink file chunk: {error}"))?;

        let mut result = Vec::with_capacity(52 + id.len() + token.len() + self.data.len());
        result.extend_from_slice(b"DLF1");
        result.extend_from_slice(&id_length.to_be_bytes());
        result.extend_from_slice(&token_length.to_be_bytes());
        result.extend_from_slice(&self.offset.to_be_bytes());
        result.extend_from_slice(&data_length.to_be_bytes());
        result.extend_from_slice(&digest);
        result.extend_from_slice(id);
        result.extend_from_slice(token);
        result.extend_from_slice(&self.data);
        Ok(result)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, String> {
        if encoded.len() < 52 || &encoded[..4] != b"DLF1" {
            return Err("Malformed DeskLink WebRTC file chunk".to_string());
        }
        let id_length = usize::from(u16::from_be_bytes([encoded[4], encoded[5]]));
        let token_length = usize::from(u16::from_be_bytes([encoded[6], encoded[7]]));
        let offset = u64::from_be_bytes(
            encoded[8..16]
                .try_into()
                .map_err(|_| "Malformed DeskLink WebRTC file chunk".to_string())?,
        );
        let data_length = usize::try_from(u32::from_be_bytes(
            encoded[16..20]
                .try_into()
                .map_err(|_| "Malformed DeskLink WebRTC file chunk".to_string())?,
        ))
        .map_err(|_| "Invalid DeskLink WebRTC file chunk size".to_string())?;
        let expected_length = 52usize
            .checked_add(id_length)
            .and_then(|value| value.checked_add(token_length))
            .and_then(|value| value.checked_add(data_length))
            .ok_or_else(|| "Malformed DeskLink WebRTC file chunk".to_string())?;
        if expected_length != encoded.len() {
            return Err("Malformed DeskLink WebRTC file chunk".to_string());
        }
        let digest = &encoded[20..52];
        let id_end = 52 + id_length;
        let token_end = id_end + token_length;
        let transfer_id = std::str::from_utf8(&encoded[52..id_end])
            .map_err(|_| "Invalid DeskLink transfer ID".to_string())?
            .to_string();
        let transfer_token = std::str::from_utf8(&encoded[id_end..token_end])
            .map_err(|_| "Invalid DeskLink transfer token".to_string())?
            .to_string();
        let data = encoded[token_end..].to_vec();
        validate_id(&transfer_id)?;
        validate_token(&transfer_token)?;
        validate_chunk_data(&data)?;
        let actual_digest = hash(MessageDigest::sha256(), &data)
            .map_err(|error| format!("Could not checksum DeskLink file chunk: {error}"))?;
        if actual_digest.as_ref() != digest {
            return Err("DeskLink WebRTC file chunk checksum mismatch".to_string());
        }
        Ok(Self {
            transfer_id,
            transfer_token,
            offset,
            data,
        })
    }
}

fn validate_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
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

fn validate_chunk_data(data: &[u8]) -> Result<(), String> {
    if data.is_empty() || data.len() > MAX_FILE_CHUNK_BYTES {
        return Err("Invalid DeskLink WebRTC file chunk size".to_string());
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

    fn offer() -> FileControl {
        FileControl {
            protocol_version: FILE_PROTOCOL_VERSION,
            action: FileAction::Offer,
            transfer_id: "transfer_1".to_string(),
            device_id: "phone-device".to_string(),
            session_id: 9,
            connection_generation: 2,
            transfer_token: "token".to_string(),
            filename: Some("report.pdf".to_string()),
            total_size: Some(3),
            sha256: Some("a".repeat(64)),
            offset: 0,
            chunk_size: 16 * 1024,
            max_chunk_size: None,
            error: None,
        }
    }

    #[test]
    fn file_control_matches_the_android_json_contract() {
        let control = offer();
        control.validate(&wire()).unwrap();
        let decoded = FileControl::from_json(&control.to_json().unwrap()).unwrap();
        assert_eq!(decoded, control);
    }

    #[test]
    fn file_control_rejects_stale_wire_and_unsafe_filename() {
        let mut control = offer();
        control.device_id = "other-device".to_string();
        assert!(control.validate(&wire()).is_err());
        control.device_id = "phone-device".to_string();
        control.filename = Some("../unsafe".to_string());
        assert!(control.validate(&wire()).is_err());
    }

    #[test]
    fn file_chunk_round_trips_and_rejects_tampering() {
        let chunk = FileChunk {
            transfer_id: "transfer_1".to_string(),
            transfer_token: "token".to_string(),
            offset: 0,
            data: b"abc".to_vec(),
        };
        let encoded = chunk.encode().unwrap();
        assert_eq!(FileChunk::decode(&encoded).unwrap(), chunk);
        let mut tampered = encoded;
        let last = tampered.len() - 1;
        tampered[last] ^= 1;
        assert!(FileChunk::decode(&tampered).is_err());
    }

    /// An encoded chunk that skips `encode`'s validation, so a test can put on
    /// the wire what a hostile peer could.
    fn raw_chunk(id: &[u8], token: &[u8], offset: u64, data: &[u8]) -> Vec<u8> {
        let digest = hash(MessageDigest::sha256(), data).unwrap();
        let mut out = b"DLF1".to_vec();
        out.extend((id.len() as u16).to_be_bytes());
        out.extend((token.len() as u16).to_be_bytes());
        out.extend(offset.to_be_bytes());
        out.extend((data.len() as u32).to_be_bytes());
        out.extend_from_slice(&digest);
        out.extend_from_slice(id);
        out.extend_from_slice(token);
        out.extend_from_slice(data);
        out
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
    fn file_control_rejects_every_field_that_breaks_the_contract() {
        type Mutation = Box<dyn Fn(&mut FileControl)>;
        let bad: Vec<(&str, Mutation)> = vec![
            ("version", Box::new(|c| c.protocol_version = 2)),
            ("device", Box::new(|c| c.device_id = "other".to_string())),
            ("session", Box::new(|c| c.session_id += 1)),
            ("generation", Box::new(|c| c.connection_generation += 1)),
            ("empty id", Box::new(|c| c.transfer_id = String::new())),
            ("path id", Box::new(|c| c.transfer_id = "../x".to_string())),
            ("long id", Box::new(|c| c.transfer_id = "a".repeat(129))),
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
            ("zero chunk", Box::new(|c| c.chunk_size = 0)),
            (
                "big chunk",
                Box::new(|c| c.chunk_size = MAX_FILE_CHUNK_BYTES + 1),
            ),
            (
                "huge size",
                Box::new(|c| c.total_size = Some(MAX_TRANSFER_BYTES + 1)),
            ),
            ("offset past size", Box::new(|c| c.offset = 4)),
            (
                "short checksum",
                Box::new(|c| c.sha256 = Some("ab".to_string())),
            ),
            (
                "non-hex checksum",
                Box::new(|c| c.sha256 = Some("g".repeat(64))),
            ),
            ("no name", Box::new(|c| c.filename = None)),
            ("no size", Box::new(|c| c.total_size = None)),
            ("no checksum", Box::new(|c| c.sha256 = None)),
            ("offer offset", Box::new(|c| c.offset = 1)),
            ("long error", Box::new(|c| c.error = Some("e".repeat(4097)))),
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
    fn file_control_parses_the_json_android_sends() {
        let json = br#"{"protocolVersion":1,"action":"acknowledge","transferId":"t-1",
            "deviceId":"phone-device","sessionId":9,"connectionGeneration":2,
            "transferToken":"token","offset":16384,"chunkSize":16384}"#;
        let control = FileControl::from_json(json).unwrap();
        assert_eq!(control.action, FileAction::Acknowledge);
        assert_eq!(control.filename, None);
        assert_eq!(control.total_size, None);
        control.validate(&wire()).unwrap();

        for action in [
            "offer",
            "accept",
            "acknowledge",
            "complete",
            "cancel",
            "error",
        ] {
            let json = format!(
                r#"{{"protocolVersion":1,"action":"{action}","transferId":"t","deviceId":"d",
                "sessionId":1,"connectionGeneration":1,"transferToken":"k","offset":0,
                "chunkSize":1}}"#
            );
            assert!(FileControl::from_json(json.as_bytes()).is_ok(), "{action}");
        }
        for junk in [
            &b""[..],
            b"{",
            b"[]",
            b"null",
            br#"{"action":"delete"}"#,
            br#"{"protocolVersion":1,"action":"OFFER","transferId":"t","deviceId":"d","sessionId":1,"connectionGeneration":1,"transferToken":"k","offset":0,"chunkSize":1}"#,
            br#"{"protocolVersion":1,"action":"offer","transferId":"t","deviceId":"d","sessionId":-1,"connectionGeneration":1,"transferToken":"k","offset":0,"chunkSize":1}"#,
        ] {
            assert!(FileControl::from_json(junk).is_err(), "{junk:?}");
        }
    }

    #[test]
    fn hostile_chunk_frames_are_rejected_without_panicking() {
        let good = raw_chunk(b"transfer_1", b"token", 0, b"abc");
        assert!(FileChunk::decode(&good).is_ok());

        // Every truncation, and every extension, of a valid frame.
        for length in 0..good.len() {
            assert!(
                FileChunk::decode(&good[..length]).is_err(),
                "truncated to {length}"
            );
        }
        let mut longer = good.clone();
        longer.push(0);
        assert!(FileChunk::decode(&longer).is_err());

        let mut wrong_magic = good.clone();
        wrong_magic[3] = b'2';
        assert!(FileChunk::decode(&wrong_magic).is_err());

        // A flipped digest byte, and lengths that lie about the frame.
        let mut wrong_digest = good.clone();
        wrong_digest[20] ^= 0xff;
        assert!(FileChunk::decode(&wrong_digest).is_err());
        for (at, value) in [(4, 0xffu8), (6, 0xff), (16, 0xff), (19, 0xff)] {
            let mut lying = good.clone();
            lying[at] = value;
            assert!(FileChunk::decode(&lying).is_err(), "byte {at}");
        }

        // Payload rules, with a digest that is otherwise correct.
        let oversized = raw_chunk(
            b"transfer_1",
            b"token",
            0,
            &vec![1; MAX_FILE_CHUNK_BYTES + 1],
        );
        assert!(FileChunk::decode(&oversized).is_err());
        assert!(FileChunk::decode(&raw_chunk(b"transfer_1", b"token", 0, b"")).is_err());
        let largest = raw_chunk(b"transfer_1", b"token", 0, &vec![1; MAX_FILE_CHUNK_BYTES]);
        assert_eq!(
            FileChunk::decode(&largest).unwrap().data.len(),
            MAX_FILE_CHUNK_BYTES
        );

        // Identifiers: invalid UTF-8, path characters, empty, oversized.
        for (id, token) in [
            (&b"\xff\xfe"[..], &b"token"[..]),
            (b"../evil", b"token"),
            (b"", b"token"),
            (b"transfer_1", b""),
            (b"transfer_1", b"\xff"),
            (&[b'a'; 129][..], b"token"),
            (b"transfer_1", &[b't'; 257][..]),
        ] {
            assert!(
                FileChunk::decode(&raw_chunk(id, token, 0, b"abc")).is_err(),
                "{id:?} {token:?}"
            );
        }
    }

    #[test]
    fn file_chunk_encoding_refuses_what_decoding_would() {
        let chunk = |id: &str, token: &str, data: Vec<u8>| FileChunk {
            transfer_id: id.to_string(),
            transfer_token: token.to_string(),
            offset: 7,
            data,
        };
        assert!(chunk("t", "k", Vec::new()).encode().is_err());
        assert!(chunk("t", "k", vec![0; MAX_FILE_CHUNK_BYTES + 1])
            .encode()
            .is_err());
        assert!(chunk("../t", "k", vec![0]).encode().is_err());
        assert!(chunk("t", "", vec![0]).encode().is_err());
        let ok = chunk("t", "k", vec![0; MAX_FILE_CHUNK_BYTES]);
        assert_eq!(FileChunk::decode(&ok.encode().unwrap()).unwrap(), ok);
    }
}
