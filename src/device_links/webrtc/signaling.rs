use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private, Public};
use openssl::sign::{Signer, Verifier};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_WEBRTC_SIGNAL_V1};

pub const SIGNALING_VERSION: i64 = 1;
const MAX_AGE_MILLIS: i64 = 5 * 60 * 1000;
const MAX_SDP_BYTES: usize = 512 * 1024;
const MAX_CANDIDATE_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalingMessageType {
    Offer,
    Answer,
    IceCandidate,
    EndOfCandidates,
    IceRestart,
    Close,
}

impl SignalingMessageType {
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Offer => "offer",
            Self::Answer => "answer",
            Self::IceCandidate => "ice_candidate",
            Self::EndOfCandidates => "end_of_candidates",
            Self::IceRestart => "ice_restart",
            Self::Close => "close",
        }
    }

    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "offer" => Ok(Self::Offer),
            "answer" => Ok(Self::Answer),
            "ice_candidate" => Ok(Self::IceCandidate),
            "end_of_candidates" => Ok(Self::EndOfCandidates),
            "ice_restart" => Ok(Self::IceRestart),
            "close" => Ok(Self::Close),
            _ => Err("Unknown DeskLink WebRTC signaling message type".to_string()),
        }
    }
}

/// Signed application signaling exchanged only over the authenticated DeskLink
/// LAN bootstrap link. Its canonical representation intentionally does not
/// depend on JSON key order so it remains compatible with Android.
#[derive(Debug, Clone, PartialEq)]
pub struct WebRtcSignalingMessage {
    pub signaling_version: i64,
    pub request_id: String,
    pub session_attempt_id: String,
    pub from_device_id: String,
    pub to_device_id: String,
    pub timestamp: i64,
    pub message_type: SignalingMessageType,
    pub payload: Map<String, Value>,
    pub signature: String,
}

impl WebRtcSignalingMessage {
    pub fn unsigned(
        session_attempt_id: impl Into<String>,
        from_device_id: impl Into<String>,
        to_device_id: impl Into<String>,
        timestamp: i64,
        message_type: SignalingMessageType,
        payload: Map<String, Value>,
    ) -> Self {
        Self {
            signaling_version: SIGNALING_VERSION,
            request_id: Uuid::new_v4().to_string(),
            session_attempt_id: session_attempt_id.into(),
            from_device_id: from_device_id.into(),
            to_device_id: to_device_id.into(),
            timestamp,
            message_type,
            payload,
            signature: String::new(),
        }
    }

    pub fn sign(mut self, key: &PKey<Private>) -> Result<Self, String> {
        let mut signer =
            Signer::new(MessageDigest::sha256(), key).map_err(|error| error.to_string())?;
        signer
            .update(&self.canonical_bytes()?)
            .map_err(|error| error.to_string())?;
        self.signature = BASE64.encode(signer.sign_to_vec().map_err(|error| error.to_string())?);
        Ok(self)
    }

    pub fn verify(&self, key: &PKey<Public>) -> bool {
        let Ok(signature) = BASE64.decode(&self.signature) else {
            return false;
        };
        let Ok(canonical) = self.canonical_bytes() else {
            return false;
        };
        let Ok(mut verifier) = Verifier::new(MessageDigest::sha256(), key) else {
            return false;
        };
        verifier
            .update(&canonical)
            .and_then(|_| verifier.verify(&signature))
            .unwrap_or(false)
    }

    pub fn validate_for(&self, local_device_id: &str, now: i64) -> Result<(), String> {
        if self.signaling_version != SIGNALING_VERSION {
            return Err("Unsupported DeskLink WebRTC signaling version".to_string());
        }
        if self.to_device_id != local_device_id {
            return Err("WebRTC signaling destination mismatch".to_string());
        }
        if self.from_device_id.is_empty() || self.from_device_id == local_device_id {
            return Err("Malformed DeskLink WebRTC signaling sender".to_string());
        }
        if self.request_id.is_empty()
            || self.session_attempt_id.is_empty()
            || self.signature.is_empty()
        {
            return Err("Malformed DeskLink WebRTC signaling message".to_string());
        }
        if now.abs_diff(self.timestamp) > MAX_AGE_MILLIS as u64 {
            return Err("Expired DeskLink WebRTC signaling message".to_string());
        }
        self.canonical_payload()?;
        Ok(())
    }

    pub fn to_network_packet(&self) -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_WEBRTC_SIGNAL_V1);
        packet.set("signalingVersion", self.signaling_version);
        packet.set("requestId", self.request_id.clone());
        packet.set("sessionAttemptId", self.session_attempt_id.clone());
        packet.set("fromDeviceId", self.from_device_id.clone());
        packet.set("toDeviceId", self.to_device_id.clone());
        packet.set("timestamp", self.timestamp);
        packet.set("messageType", self.message_type.wire_name());
        packet.set("payload", Value::Object(self.payload.clone()));
        packet.set("signature", self.signature.clone());
        packet
    }

    pub fn from_network_packet(packet: &NetworkPacket) -> Result<Self, String> {
        if packet.packet_type != PACKET_TYPE_WEBRTC_SIGNAL_V1 {
            return Err("Wrong DeskLink WebRTC signaling packet type".to_string());
        }
        let value = |key: &str| {
            packet
                .body
                .get(key)
                .ok_or_else(|| format!("Missing DeskLink WebRTC signaling field: {key}"))
        };
        Ok(Self {
            signaling_version: value("signalingVersion")?
                .as_i64()
                .ok_or_else(|| "Invalid DeskLink WebRTC signaling version".to_string())?,
            request_id: value("requestId")?
                .as_str()
                .ok_or_else(|| "Invalid DeskLink WebRTC request ID".to_string())?
                .to_string(),
            session_attempt_id: value("sessionAttemptId")?
                .as_str()
                .ok_or_else(|| "Invalid DeskLink WebRTC attempt ID".to_string())?
                .to_string(),
            from_device_id: value("fromDeviceId")?
                .as_str()
                .ok_or_else(|| "Invalid DeskLink WebRTC sender".to_string())?
                .to_string(),
            to_device_id: value("toDeviceId")?
                .as_str()
                .ok_or_else(|| "Invalid DeskLink WebRTC destination".to_string())?
                .to_string(),
            timestamp: value("timestamp")?
                .as_i64()
                .ok_or_else(|| "Invalid DeskLink WebRTC timestamp".to_string())?,
            message_type: SignalingMessageType::parse(
                value("messageType")?
                    .as_str()
                    .ok_or_else(|| "Invalid DeskLink WebRTC message type".to_string())?,
            )?,
            payload: value("payload")?
                .as_object()
                .cloned()
                .ok_or_else(|| "Missing DeskLink WebRTC signaling payload".to_string())?,
            signature: value("signature")?
                .as_str()
                .ok_or_else(|| "Invalid DeskLink WebRTC signature".to_string())?
                .to_string(),
        })
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, String> {
        let values = [
            self.signaling_version.to_string(),
            self.request_id.clone(),
            self.session_attempt_id.clone(),
            self.from_device_id.clone(),
            self.to_device_id.clone(),
            self.timestamp.to_string(),
            self.message_type.wire_name().to_string(),
            self.canonical_payload()?,
        ];
        Ok(values
            .iter()
            .map(|value| format!("{}:{value}", value.len()))
            .collect::<String>()
            .into_bytes())
    }

    fn canonical_payload(&self) -> Result<String, String> {
        let required_string = |key: &str, maximum: usize| {
            let value = self
                .payload
                .get(key)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("Missing DeskLink WebRTC signaling payload field: {key}"))?;
            if value.len() > maximum {
                return Err(format!(
                    "DeskLink WebRTC signaling payload field is too large: {key}"
                ));
            }
            Ok(value)
        };
        match self.message_type {
            SignalingMessageType::Offer | SignalingMessageType::Answer => {
                Ok(format!("sdp={}", required_string("sdp", MAX_SDP_BYTES)?))
            }
            SignalingMessageType::IceCandidate => {
                let index = self
                    .payload
                    .get("sdpMLineIndex")
                    .and_then(Value::as_i64)
                    .ok_or_else(|| "Missing DeskLink WebRTC ICE m-line index".to_string())?;
                Ok(format!(
                    "sdpMLineIndex={index}\ncandidate={}",
                    required_string("candidate", MAX_CANDIDATE_BYTES)?
                ))
            }
            SignalingMessageType::EndOfCandidates
            | SignalingMessageType::IceRestart
            | SignalingMessageType::Close => Ok(String::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use openssl::ec::{EcGroup, EcKey};
    use openssl::nid::Nid;
    use openssl::pkey::PKey;
    use serde_json::{Map, Value};

    use super::*;

    fn test_key() -> PKey<Private> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap()
    }

    fn offer_payload() -> Map<String, Value> {
        [("sdp".to_string(), Value::String("v=0\r\n".to_string()))]
            .into_iter()
            .collect()
    }

    #[test]
    fn signed_offer_round_trips_through_a_network_packet() {
        let private = test_key();
        let public = PKey::public_key_from_der(&private.public_key_to_der().unwrap()).unwrap();
        let message = WebRtcSignalingMessage::unsigned(
            "01234567-89ab-cdef-0123-456789abcdef",
            "desktop",
            "phone",
            100,
            SignalingMessageType::Offer,
            offer_payload(),
        )
        .sign(&private)
        .unwrap();
        let packet = message.to_network_packet();
        let parsed = WebRtcSignalingMessage::from_network_packet(&packet).unwrap();

        parsed.validate_for("phone", 100).unwrap();
        assert!(parsed.verify(&public));
        assert_eq!(parsed.message_type, SignalingMessageType::Offer);
    }

    #[test]
    fn signature_covers_the_destination_and_sdp() {
        let private = test_key();
        let public = PKey::public_key_from_der(&private.public_key_to_der().unwrap()).unwrap();
        let mut message = WebRtcSignalingMessage::unsigned(
            "01234567-89ab-cdef-0123-456789abcdef",
            "desktop",
            "phone",
            100,
            SignalingMessageType::Offer,
            offer_payload(),
        )
        .sign(&private)
        .unwrap();

        message.to_device_id = "different-phone".to_string();
        assert!(!message.verify(&public));
    }
}
