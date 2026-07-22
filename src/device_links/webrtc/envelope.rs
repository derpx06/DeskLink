use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{channel::DataChannelSpec, MAX_ENVELOPE_BYTES, MAX_PAYLOAD_BYTES};

pub const ENVELOPE_VERSION: u8 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageEnvelope {
    pub protocol_version: u8,
    pub message_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub channel: String,
    pub message_type: String,
    pub timestamp: i64,
    pub flags: u32,
    pub payload_length: usize,
    pub payload_base64: String,
}

impl MessageEnvelope {
    pub fn new(
        device_id: impl Into<String>,
        session_id: u64,
        connection_generation: u64,
        channel: &DataChannelSpec,
        message_type: impl Into<String>,
        payload: &[u8],
        timestamp: i64,
    ) -> Result<Self, EnvelopeError> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(EnvelopeError::PayloadTooLarge(payload.len()));
        }
        Ok(Self {
            protocol_version: ENVELOPE_VERSION,
            message_id: Uuid::new_v4().to_string(),
            device_id: device_id.into(),
            session_id,
            connection_generation,
            channel: channel.label.to_string(),
            message_type: message_type.into(),
            timestamp,
            flags: 0,
            payload_length: payload.len(),
            payload_base64: base64::engine::general_purpose::STANDARD.encode(payload),
        })
    }

    pub fn decode_payload(&self) -> Result<Vec<u8>, EnvelopeError> {
        let payload = base64::engine::general_purpose::STANDARD
            .decode(&self.payload_base64)
            .map_err(|_| EnvelopeError::InvalidBase64)?;
        if payload.len() != self.payload_length {
            return Err(EnvelopeError::LengthMismatch {
                expected: self.payload_length,
                actual: payload.len(),
            });
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(EnvelopeError::PayloadTooLarge(payload.len()));
        }
        Ok(payload)
    }

    pub fn validate(
        &self,
        expected_device_id: &str,
        expected_session_id: u64,
        expected_generation: u64,
    ) -> Result<(), EnvelopeError> {
        if self.protocol_version != ENVELOPE_VERSION {
            return Err(EnvelopeError::UnsupportedVersion(self.protocol_version));
        }
        if self.device_id != expected_device_id {
            return Err(EnvelopeError::WrongDevice);
        }
        if self.session_id != expected_session_id
            || self.connection_generation != expected_generation
        {
            return Err(EnvelopeError::StaleGeneration);
        }
        if DataChannelSpec::for_label(&self.channel).is_none() {
            return Err(EnvelopeError::UnknownChannel(self.channel.clone()));
        }
        if self.message_id.is_empty() || self.message_type.is_empty() {
            return Err(EnvelopeError::Malformed);
        }
        if serde_json::to_vec(self)
            .map_err(|_| EnvelopeError::Malformed)?
            .len()
            > MAX_ENVELOPE_BYTES
        {
            return Err(EnvelopeError::EnvelopeTooLarge);
        }
        self.decode_payload().map(|_| ())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvelopeError {
    PayloadTooLarge(usize),
    EnvelopeTooLarge,
    InvalidBase64,
    LengthMismatch { expected: usize, actual: usize },
    UnsupportedVersion(u8),
    WrongDevice,
    StaleGeneration,
    UnknownChannel(String),
    Malformed,
}

impl std::fmt::Display for EnvelopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid WebRTC message envelope: {self:?}")
    }
}

impl std::error::Error for EnvelopeError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::webrtc::channel::DataChannelSpec;

    #[test]
    fn round_trips_and_validates_binding() {
        let envelope = MessageEnvelope::new(
            "phone-1",
            12,
            3,
            &DataChannelSpec::EVENTS,
            "ping",
            b"hello",
            1_780_000_000_000,
        )
        .unwrap();
        let bytes = serde_json::to_vec(&envelope).unwrap();
        let parsed: MessageEnvelope = serde_json::from_slice(&bytes).unwrap();
        parsed.validate("phone-1", 12, 3).unwrap();
        assert_eq!(parsed.decode_payload().unwrap(), b"hello");
        assert!(matches!(
            parsed.validate("other", 12, 3),
            Err(EnvelopeError::WrongDevice)
        ));
    }

    #[test]
    fn rejects_payload_length_and_stale_generation() {
        let mut envelope =
            MessageEnvelope::new("phone-1", 12, 3, &DataChannelSpec::CONTROL, "ping", b"x", 1)
                .unwrap();
        envelope.payload_length = 2;
        assert!(matches!(
            envelope.decode_payload(),
            Err(EnvelopeError::LengthMismatch { .. })
        ));
        envelope.payload_length = 1;
        assert!(matches!(
            envelope.validate("phone-1", 12, 4),
            Err(EnvelopeError::StaleGeneration)
        ));
    }
}
