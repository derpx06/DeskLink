//! Bounded, resumable file-transfer messages carried by DeskLink WebRTC.

use openssl::sha::sha256;
use serde::{Deserialize, Serialize};

use super::wire_binding::WebRtcWireBinding;
use crate::device_links::core::transfer_manager::MAX_TRANSFER_SIZE;

pub const FILE_CONTROL_MESSAGE_TYPE: &str = "desklink.file.control.v1";
pub const FILE_CHUNK_MESSAGE_TYPE: &str = "desklink.file.chunk.v1";
pub const FILE_PROTOCOL_VERSION: u8 = 1;
pub const MAX_FILE_CHUNK_BYTES: usize = 16 * 1024;
const CHUNK_MAGIC: &[u8; 4] = b"DLF1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FileTransferAction {
    Offer,
    Accept,
    Acknowledge,
    Complete,
    Cancel,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTransferControl {
    pub protocol_version: u8,
    pub action: FileTransferAction,
    pub transfer_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub transfer_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_size: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    pub offset: u64,
    pub chunk_size: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl FileTransferControl {
    pub fn validate(&self, wire: &WebRtcWireBinding) -> Result<(), FileProtocolError> {
        if self.protocol_version != FILE_PROTOCOL_VERSION {
            return Err(FileProtocolError::UnsupportedVersion);
        }
        if self.device_id != wire.peer_device_id
            || self.session_id != wire.session_id
            || self.connection_generation != wire.generation
        {
            return Err(FileProtocolError::BindingMismatch);
        }
        validate_identifier(&self.transfer_id)?;
        validate_token(&self.transfer_token)?;
        if self.chunk_size == 0 || self.chunk_size as usize > MAX_FILE_CHUNK_BYTES {
            return Err(FileProtocolError::InvalidChunkSize);
        }
        if let Some(total_size) = self.total_size {
            if total_size > MAX_TRANSFER_SIZE || self.offset > total_size {
                return Err(FileProtocolError::InvalidSize);
            }
        }
        if let Some(digest) = &self.sha256 {
            if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(FileProtocolError::InvalidChecksum);
            }
        }
        if self.action == FileTransferAction::Offer {
            let filename = self
                .filename
                .as_deref()
                .ok_or(FileProtocolError::InvalidFilename)?;
            validate_filename(filename)?;
            if self.total_size.is_none() || self.sha256.is_none() || self.offset != 0 {
                return Err(FileProtocolError::Malformed);
            }
        }
        if self.error.as_ref().is_some_and(|error| error.len() > 4096) {
            return Err(FileProtocolError::Malformed);
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
    pub fn encode(&self) -> Result<Vec<u8>, FileProtocolError> {
        validate_identifier(&self.transfer_id)?;
        validate_token(&self.transfer_token)?;
        if self.data.is_empty() || self.data.len() > MAX_FILE_CHUNK_BYTES {
            return Err(FileProtocolError::InvalidChunkSize);
        }
        let id = self.transfer_id.as_bytes();
        let token = self.transfer_token.as_bytes();
        let id_length = u16::try_from(id.len()).map_err(|_| FileProtocolError::Malformed)?;
        let token_length = u16::try_from(token.len()).map_err(|_| FileProtocolError::Malformed)?;
        let data_length =
            u32::try_from(self.data.len()).map_err(|_| FileProtocolError::Malformed)?;
        let mut encoded = Vec::with_capacity(52 + id.len() + token.len() + self.data.len());
        encoded.extend_from_slice(CHUNK_MAGIC);
        encoded.extend_from_slice(&id_length.to_be_bytes());
        encoded.extend_from_slice(&token_length.to_be_bytes());
        encoded.extend_from_slice(&self.offset.to_be_bytes());
        encoded.extend_from_slice(&data_length.to_be_bytes());
        encoded.extend_from_slice(&sha256(&self.data));
        encoded.extend_from_slice(id);
        encoded.extend_from_slice(token);
        encoded.extend_from_slice(&self.data);
        Ok(encoded)
    }

    pub fn decode(encoded: &[u8]) -> Result<Self, FileProtocolError> {
        if encoded.len() < 52 || &encoded[..4] != CHUNK_MAGIC {
            return Err(FileProtocolError::Malformed);
        }
        let id_length = u16::from_be_bytes([encoded[4], encoded[5]]) as usize;
        let token_length = u16::from_be_bytes([encoded[6], encoded[7]]) as usize;
        let offset = u64::from_be_bytes(
            encoded[8..16]
                .try_into()
                .map_err(|_| FileProtocolError::Malformed)?,
        );
        let data_length = u32::from_be_bytes(
            encoded[16..20]
                .try_into()
                .map_err(|_| FileProtocolError::Malformed)?,
        ) as usize;
        if data_length == 0 || data_length > MAX_FILE_CHUNK_BYTES {
            return Err(FileProtocolError::InvalidChunkSize);
        }
        let expected_length = 52usize
            .checked_add(id_length)
            .and_then(|value| value.checked_add(token_length))
            .and_then(|value| value.checked_add(data_length))
            .ok_or(FileProtocolError::Malformed)?;
        if encoded.len() != expected_length {
            return Err(FileProtocolError::Malformed);
        }
        let id_start = 52;
        let token_start = id_start + id_length;
        let data_start = token_start + token_length;
        let transfer_id = std::str::from_utf8(&encoded[id_start..token_start])
            .map_err(|_| FileProtocolError::Malformed)?
            .to_string();
        let transfer_token = std::str::from_utf8(&encoded[token_start..data_start])
            .map_err(|_| FileProtocolError::Malformed)?
            .to_string();
        let data = encoded[data_start..].to_vec();
        validate_identifier(&transfer_id)?;
        validate_token(&transfer_token)?;
        if sha256(&data).as_slice() != &encoded[20..52] {
            return Err(FileProtocolError::InvalidChecksum);
        }
        Ok(Self {
            transfer_id,
            transfer_token,
            offset,
            data,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileProtocolError {
    UnsupportedVersion,
    BindingMismatch,
    InvalidIdentifier,
    InvalidToken,
    InvalidFilename,
    InvalidSize,
    InvalidChunkSize,
    InvalidChecksum,
    Malformed,
}

impl std::fmt::Display for FileProtocolError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid DeskLink WebRTC file message: {self:?}")
    }
}

impl std::error::Error for FileProtocolError {}

fn validate_identifier(value: &str) -> Result<(), FileProtocolError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(FileProtocolError::InvalidIdentifier);
    }
    Ok(())
}

fn validate_token(value: &str) -> Result<(), FileProtocolError> {
    if value.is_empty() || value.len() > 256 || !value.is_ascii() {
        return Err(FileProtocolError::InvalidToken);
    }
    Ok(())
}

fn validate_filename(value: &str) -> Result<(), FileProtocolError> {
    let path = std::path::Path::new(value);
    if value.is_empty()
        || value.len() > 255
        || value == "."
        || value == ".."
        || path.components().count() != 1
        || path.file_name().and_then(|name| name.to_str()) != Some(value)
    {
        return Err(FileProtocolError::InvalidFilename);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_round_trip_detects_corruption_and_bounds() {
        let chunk = FileChunk {
            transfer_id: "transfer-1".to_string(),
            transfer_token: "token-1".to_string(),
            offset: 16,
            data: b"DeskLink".to_vec(),
        };
        let encoded = chunk.encode().unwrap();
        assert_eq!(FileChunk::decode(&encoded).unwrap(), chunk);

        let mut corrupted = encoded;
        *corrupted.last_mut().unwrap() ^= 1;
        assert_eq!(
            FileChunk::decode(&corrupted),
            Err(FileProtocolError::InvalidChecksum)
        );
    }

    #[test]
    fn offer_rejects_wrong_binding_and_unsafe_filename() {
        let wire = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let offer = FileTransferControl {
            protocol_version: 1,
            action: FileTransferAction::Offer,
            transfer_id: "transfer-1".to_string(),
            device_id: "phone".to_string(),
            session_id: wire.session_id,
            connection_generation: wire.generation,
            transfer_token: "token".to_string(),
            filename: Some("../escape".to_string()),
            total_size: Some(4),
            sha256: Some("0".repeat(64)),
            offset: 0,
            chunk_size: MAX_FILE_CHUNK_BYTES as u32,
            error: None,
        };
        assert_eq!(
            offer.validate(&wire),
            Err(FileProtocolError::InvalidFilename)
        );
        let mut wrong_device = offer;
        wrong_device.filename = Some("safe.txt".to_string());
        wrong_device.device_id = "other".to_string();
        assert_eq!(
            wrong_device.validate(&wire),
            Err(FileProtocolError::BindingMismatch)
        );
    }
}
