use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use openssl::hash::MessageDigest;
use openssl::pkey::{PKey, Private, Public};
use openssl::sign::{Signer, Verifier};
use serde::{Deserialize, Serialize};

use super::WebRtcWireBinding;

pub const HANDOVER_VERSION: i64 = 1;
pub const HANDOVER_MESSAGE_TYPE: &str = "desklink.handover.v1";
const MAX_AGE_MILLIS: i64 = 5 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverState {
    Negotiating,
    Authenticated,
    CapabilitiesConfirmed,
    FeatureReady,
    Failed,
}

impl HandoverState {
    pub const fn receive(self, message: HandoverMessage) -> Self {
        match (self, message) {
            (Self::Negotiating, HandoverMessage::Authenticated) => Self::Authenticated,
            (Self::Authenticated, HandoverMessage::Capabilities) => Self::CapabilitiesConfirmed,
            (Self::CapabilitiesConfirmed, HandoverMessage::FeatureReady) => Self::FeatureReady,
            (Self::FeatureReady, HandoverMessage::Degraded) => Self::Authenticated,
            _ => Self::Failed,
        }
    }

    pub const fn features_allowed(self) -> bool {
        matches!(self, Self::FeatureReady)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverMessage {
    Authenticated,
    Capabilities,
    FeatureReady,
    Degraded,
    Close,
}

/// Mutable, per-peer state for the post-DTLS handover.  This belongs to the
/// logical device session: a stale peer generation must never be able to make
/// a replacement transport feature-ready.
///
/// The control coordinator records the transcript material here as SDP and
/// control messages arrive.  Keeping it separate from the GStreamer peer
/// object makes the security decisions testable without a media runtime.
#[derive(Debug, Clone)]
pub struct HandoverRuntime {
    pub attempt_id: String,
    pub wire_binding: WebRtcWireBinding,
    pub state: HandoverState,
    pub offer_sdp: Option<String>,
    pub answer_sdp: Option<String>,
    pub local_nonce: Option<String>,
    pub remote_nonce: Option<String>,
    pub authentication_timestamp: Option<i64>,
    local_authenticated: bool,
    remote_authenticated: bool,
    local_capabilities_confirmed: bool,
    remote_capabilities_confirmed: bool,
    local_feature_ready: bool,
    remote_feature_ready: bool,
}

impl HandoverRuntime {
    pub fn new(attempt_id: String, wire_binding: WebRtcWireBinding) -> Self {
        Self {
            attempt_id,
            wire_binding,
            state: HandoverState::Negotiating,
            offer_sdp: None,
            answer_sdp: None,
            local_nonce: None,
            remote_nonce: None,
            authentication_timestamp: None,
            local_authenticated: false,
            remote_authenticated: false,
            local_capabilities_confirmed: false,
            remote_capabilities_confirmed: false,
            local_feature_ready: false,
            remote_feature_ready: false,
        }
    }

    pub fn record_offer(&mut self, sdp: String) -> Result<(), String> {
        Self::record_once(&mut self.offer_sdp, sdp, "offer")
    }

    pub fn record_answer(&mut self, sdp: String) -> Result<(), String> {
        Self::record_once(&mut self.answer_sdp, sdp, "answer")
    }

    pub fn record_local_nonce(&mut self, nonce: String) -> Result<(), String> {
        Self::record_once(&mut self.local_nonce, nonce, "local nonce")
    }

    pub fn record_remote_nonce(&mut self, nonce: String) -> Result<(), String> {
        Self::record_once(&mut self.remote_nonce, nonce, "remote nonce")
    }

    pub fn record_authentication_timestamp(&mut self, timestamp: i64) -> Result<(), String> {
        if timestamp <= 0 {
            return Err("DeskLink WebRTC authentication timestamp is invalid".to_string());
        }
        match self.authentication_timestamp {
            Some(existing) if existing != timestamp => Err(
                "DeskLink WebRTC authentication timestamp changed during one attempt".to_string(),
            ),
            Some(_) => Ok(()),
            None => {
                self.authentication_timestamp = Some(timestamp);
                Ok(())
            }
        }
    }

    pub fn mark_local_authenticated(&mut self) -> Result<(), String> {
        self.ensure_negotiating("authenticate")?;
        self.local_authenticated = true;
        self.refresh_state();
        Ok(())
    }

    pub fn mark_remote_authenticated(&mut self) -> Result<(), String> {
        self.ensure_negotiating("accept authentication")?;
        self.remote_authenticated = true;
        self.refresh_state();
        Ok(())
    }

    pub fn mark_local_capabilities_confirmed(&mut self) -> Result<(), String> {
        self.ensure_authenticated("confirm local capabilities")?;
        self.local_capabilities_confirmed = true;
        self.refresh_state();
        Ok(())
    }

    pub fn mark_remote_capabilities_confirmed(&mut self) -> Result<(), String> {
        self.ensure_authenticated("accept remote capabilities")?;
        self.remote_capabilities_confirmed = true;
        self.refresh_state();
        Ok(())
    }

    pub fn mark_local_feature_ready(&mut self) -> Result<(), String> {
        self.ensure_capabilities_confirmed("enable local feature transport")?;
        self.local_feature_ready = true;
        self.refresh_state();
        Ok(())
    }

    pub fn mark_remote_feature_ready(&mut self) -> Result<(), String> {
        self.ensure_capabilities_confirmed("accept remote feature transport")?;
        self.remote_feature_ready = true;
        self.refresh_state();
        Ok(())
    }

    pub const fn features_ready(&self) -> bool {
        self.local_feature_ready && self.remote_feature_ready && self.state.features_allowed()
    }

    pub const fn local_capabilities_confirmed(&self) -> bool {
        self.local_capabilities_confirmed
    }

    pub const fn local_feature_ready(&self) -> bool {
        self.local_feature_ready
    }

    pub const fn remote_authenticated(&self) -> bool {
        self.remote_authenticated
    }

    pub fn fail(&mut self) {
        self.state = HandoverState::Failed;
    }

    fn record_once(slot: &mut Option<String>, value: String, field: &str) -> Result<(), String> {
        if value.is_empty() {
            return Err(format!("DeskLink WebRTC {field} must not be empty"));
        }
        match slot {
            Some(existing) if existing != &value => Err(format!(
                "DeskLink WebRTC {field} changed during one session attempt"
            )),
            Some(_) => Ok(()),
            None => {
                *slot = Some(value);
                Ok(())
            }
        }
    }

    fn ensure_negotiating(&self, operation: &str) -> Result<(), String> {
        if self.state == HandoverState::Failed {
            return Err(format!(
                "Cannot {operation}: DeskLink WebRTC handover has failed"
            ));
        }
        if self.offer_sdp.is_none() || self.answer_sdp.is_none() {
            return Err(format!(
                "Cannot {operation}: DeskLink WebRTC SDP negotiation is incomplete"
            ));
        }
        Ok(())
    }

    fn ensure_authenticated(&self, operation: &str) -> Result<(), String> {
        self.ensure_negotiating(operation)?;
        if !(self.local_authenticated && self.remote_authenticated) {
            return Err(format!(
                "Cannot {operation}: DeskLink WebRTC peers are not mutually authenticated"
            ));
        }
        Ok(())
    }

    fn ensure_capabilities_confirmed(&self, operation: &str) -> Result<(), String> {
        self.ensure_authenticated(operation)?;
        if !(self.local_capabilities_confirmed && self.remote_capabilities_confirmed) {
            return Err(format!(
                "Cannot {operation}: DeskLink WebRTC capabilities are not mutually confirmed"
            ));
        }
        Ok(())
    }

    fn refresh_state(&mut self) {
        if self.state == HandoverState::Failed {
            return;
        }
        self.state = if self.local_feature_ready && self.remote_feature_ready {
            HandoverState::FeatureReady
        } else if self.local_capabilities_confirmed && self.remote_capabilities_confirmed {
            HandoverState::CapabilitiesConfirmed
        } else if self.local_authenticated && self.remote_authenticated {
            HandoverState::Authenticated
        } else {
            HandoverState::Negotiating
        };
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandoverControlKind {
    #[serde(rename = "hello")]
    Hello,
    #[serde(rename = "challenge")]
    Challenge,
    #[serde(rename = "response")]
    Response,
    #[serde(rename = "authenticated")]
    Authenticated,
    #[serde(rename = "capabilities")]
    Capabilities,
    #[serde(rename = "feature-ready")]
    FeatureReady,
    #[serde(rename = "degraded")]
    Degraded,
    #[serde(rename = "close")]
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoverControlMessage {
    pub handover_version: i64,
    pub kind: HandoverControlKind,
    pub session_attempt_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peer_nonce: Option<String>,
    pub timestamp: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature_base64: Option<String>,
    #[serde(default)]
    pub incoming_capabilities: Vec<String>,
    #[serde(default)]
    pub outgoing_capabilities: Vec<String>,
}

impl HandoverControlMessage {
    pub fn new(
        kind: HandoverControlKind,
        attempt_id: impl Into<String>,
        binding: &WebRtcWireBinding,
        timestamp: i64,
    ) -> Self {
        Self {
            handover_version: HANDOVER_VERSION,
            kind,
            session_attempt_id: attempt_id.into(),
            device_id: binding.sender_device_id.clone(),
            session_id: binding.session_id,
            connection_generation: binding.generation,
            nonce: None,
            peer_nonce: None,
            timestamp,
            signature_base64: None,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
        }
    }

    pub fn validate(
        &self,
        binding: &WebRtcWireBinding,
        attempt_id: &str,
        now: i64,
    ) -> Result<(), String> {
        if self.handover_version != HANDOVER_VERSION {
            return Err("Unsupported DeskLink WebRTC handover version".to_string());
        }
        if self.session_attempt_id != attempt_id
            || self.device_id != binding.peer_device_id
            || self.session_id != binding.session_id
            || self.connection_generation != binding.generation
        {
            return Err("DeskLink WebRTC handover binding mismatch".to_string());
        }
        if now.abs_diff(self.timestamp) > MAX_AGE_MILLIS as u64 {
            return Err("Expired DeskLink WebRTC handover message".to_string());
        }
        Ok(())
    }

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }
}

/// Cross-platform signed transcript for post-DTLS authentication. The JSON is
/// constructed field-by-field because Android's JSONStringer has a defined
/// insertion order and both peers must sign identical bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthenticationTranscript {
    pub session_attempt_id: String,
    pub initiator_device_id: String,
    pub responder_device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    pub initiator_nonce: String,
    pub responder_nonce: String,
    pub offer_sha256: String,
    pub answer_sha256: String,
    pub initiator_dtls_fingerprint: String,
    pub responder_dtls_fingerprint: String,
    pub protocol_version: i64,
    pub timestamp: i64,
}

impl AuthenticationTranscript {
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let string =
            |value: &str| serde_json::to_string(value).expect("String serialization is infallible");
        format!(
            concat!(
                "{{\"sessionAttemptId\":{},\"initiatorDeviceId\":{},\"responderDeviceId\":{},",
                "\"sessionId\":{},\"connectionGeneration\":{},\"initiatorNonce\":{},",
                "\"responderNonce\":{},\"offerSha256\":{},\"answerSha256\":{},",
                "\"initiatorDtlsFingerprint\":{},\"responderDtlsFingerprint\":{},",
                "\"protocolVersion\":{},\"timestamp\":{}}}"
            ),
            string(&self.session_attempt_id),
            string(&self.initiator_device_id),
            string(&self.responder_device_id),
            self.session_id,
            self.connection_generation,
            string(&self.initiator_nonce),
            string(&self.responder_nonce),
            string(&self.offer_sha256),
            string(&self.answer_sha256),
            string(&self.initiator_dtls_fingerprint),
            string(&self.responder_dtls_fingerprint),
            self.protocol_version,
            self.timestamp,
        )
        .into_bytes()
    }

    pub fn sign(&self, key: &PKey<Private>) -> Result<Vec<u8>, String> {
        let mut signer =
            Signer::new(MessageDigest::sha256(), key).map_err(|error| error.to_string())?;
        signer
            .update(&self.canonical_bytes())
            .map_err(|error| error.to_string())?;
        signer.sign_to_vec().map_err(|error| error.to_string())
    }

    pub fn verify(&self, key: &PKey<Public>, signature: &[u8]) -> bool {
        let Ok(mut verifier) = Verifier::new(MessageDigest::sha256(), key) else {
            return false;
        };
        verifier
            .update(&self.canonical_bytes())
            .and_then(|_| verifier.verify(signature))
            .unwrap_or(false)
    }

    pub fn signature_to_base64(signature: &[u8]) -> String {
        BASE64.encode(signature)
    }

    pub fn signature_from_base64(encoded: &str) -> Result<Vec<u8>, String> {
        if encoded.len() > 16 * 1024 {
            return Err("DeskLink WebRTC authentication signature is too large".to_string());
        }
        BASE64
            .decode(encoded)
            .map_err(|_| "Malformed DeskLink WebRTC authentication signature".to_string())
    }
}

#[cfg(test)]
mod tests {
    use openssl::ec::{EcGroup, EcKey};
    use openssl::nid::Nid;
    use openssl::pkey::PKey;

    use super::*;

    fn binding(sender: &str, peer: &str) -> WebRtcWireBinding {
        WebRtcWireBinding::from_attempt(sender, peer, "01234567-89ab-cdef-0123-456789abcdef")
            .unwrap()
    }

    fn key() -> PKey<Private> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap()
    }

    #[test]
    fn handover_state_requires_the_complete_sequence() {
        assert_eq!(
            HandoverState::Negotiating.receive(HandoverMessage::Authenticated),
            HandoverState::Authenticated
        );
        assert_eq!(
            HandoverState::Authenticated.receive(HandoverMessage::FeatureReady),
            HandoverState::Failed
        );
    }

    #[test]
    fn control_message_binds_to_the_opposite_wire_peer() {
        let local = binding("desktop", "phone");
        let remote = binding("phone", "desktop");
        let message = HandoverControlMessage::new(
            HandoverControlKind::Hello,
            "01234567-89ab-cdef-0123-456789abcdef",
            &local,
            100,
        );

        let parsed = HandoverControlMessage::from_json(&message.to_json().unwrap()).unwrap();
        parsed
            .validate(&remote, "01234567-89ab-cdef-0123-456789abcdef", 100)
            .unwrap();
    }

    #[test]
    fn transcript_signature_rejects_a_changed_answer_hash() {
        let private = key();
        let public = PKey::public_key_from_der(&private.public_key_to_der().unwrap()).unwrap();
        let mut transcript = AuthenticationTranscript {
            session_attempt_id: "attempt".to_string(),
            initiator_device_id: "desktop".to_string(),
            responder_device_id: "phone".to_string(),
            session_id: 1,
            connection_generation: 1,
            initiator_nonce: "a".to_string(),
            responder_nonce: "b".to_string(),
            offer_sha256: "offer".to_string(),
            answer_sha256: "answer".to_string(),
            initiator_dtls_fingerprint: "one".to_string(),
            responder_dtls_fingerprint: "two".to_string(),
            protocol_version: 1,
            timestamp: 100,
        };
        let signature = transcript.sign(&private).unwrap();
        assert!(transcript.verify(&public, &signature));
        transcript.answer_sha256 = "changed".to_string();
        assert!(!transcript.verify(&public, &signature));
    }

    #[test]
    fn runtime_requires_bidirectional_feature_ready_confirmation() {
        let wire = binding("desktop", "phone");
        let mut runtime = HandoverRuntime::new("attempt".to_string(), wire);
        runtime.record_offer("offer".to_string()).unwrap();
        runtime.record_answer("answer".to_string()).unwrap();
        runtime.mark_local_authenticated().unwrap();
        runtime.mark_remote_authenticated().unwrap();
        runtime.mark_local_capabilities_confirmed().unwrap();
        runtime.mark_remote_capabilities_confirmed().unwrap();
        runtime.mark_local_feature_ready().unwrap();

        assert_eq!(runtime.state, HandoverState::CapabilitiesConfirmed);
        assert!(!runtime.features_ready());

        runtime.mark_remote_feature_ready().unwrap();
        assert_eq!(runtime.state, HandoverState::FeatureReady);
        assert!(runtime.features_ready());
    }

    #[test]
    fn runtime_rejects_changed_sdp_for_one_attempt() {
        let wire = binding("desktop", "phone");
        let mut runtime = HandoverRuntime::new("attempt".to_string(), wire);
        runtime.record_offer("offer-one".to_string()).unwrap();

        assert!(runtime.record_offer("offer-two".to_string()).is_err());
    }
}
