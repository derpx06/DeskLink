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
pub const MAX_FILE_CHUNK_BYTES: usize = 16 * 1024;
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
    pub chunk_size: usize,
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

fn validate_filename(value: &str) -> Result<(), String> {
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
}
