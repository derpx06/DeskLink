//! Symmetric control-channel state for enabling paired feature traffic.

use serde::{Deserialize, Serialize};

pub const HANDOVER_VERSION: u8 = 1;
pub const HANDOVER_MESSAGE_TYPE: &str = "desklink.handover.v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverState {
    Negotiating,
    Authenticated,
    CapabilitiesConfirmed,
    FeatureReady,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandoverMessage {
    Authenticated,
    Capabilities,
    FeatureReady,
    Degraded,
    Close,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HandoverControlKind {
    Hello,
    Challenge,
    Response,
    Authenticated,
    Capabilities,
    FeatureReady,
    Degraded,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoverControlMessage {
    pub handover_version: u8,
    pub kind: HandoverControlKind,
    pub session_attempt_id: String,
    pub device_id: String,
    pub session_id: u64,
    pub connection_generation: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peer_nonce: Option<String>,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature_base64: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub incoming_capabilities: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub outgoing_capabilities: Vec<String>,
}

impl HandoverControlMessage {
    pub fn validate(
        &self,
        expected_attempt_id: &str,
        expected_device_id: &str,
        expected_session_id: u64,
        expected_generation: u64,
        now: i64,
    ) -> Result<(), &'static str> {
        if self.handover_version != HANDOVER_VERSION {
            return Err("unsupported DeskLink WebRTC handover version");
        }
        if self.session_attempt_id != expected_attempt_id
            || self.device_id != expected_device_id
            || self.session_id != expected_session_id
            || self.connection_generation != expected_generation
        {
            return Err("DeskLink WebRTC handover binding mismatch");
        }
        if (now - self.timestamp).abs() > 5 * 60 * 1000 {
            return Err("expired DeskLink WebRTC handover message");
        }
        Ok(())
    }
}

impl HandoverState {
    pub fn receive(self, message: HandoverMessage) -> Result<Self, &'static str> {
        match (self, message) {
            (Self::Negotiating, HandoverMessage::Authenticated) => Ok(Self::Authenticated),
            (Self::Authenticated, HandoverMessage::Capabilities) => Ok(Self::CapabilitiesConfirmed),
            (Self::CapabilitiesConfirmed, HandoverMessage::FeatureReady) => Ok(Self::FeatureReady),
            (Self::FeatureReady, HandoverMessage::Degraded) => Ok(Self::Authenticated),
            (_, HandoverMessage::Close) => Ok(Self::Failed),
            (_, HandoverMessage::Degraded) => Ok(Self::Failed),
            _ => Err("invalid DeskLink WebRTC handover transition"),
        }
    }

    pub const fn features_allowed(self) -> bool {
        matches!(self, Self::FeatureReady)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn features_require_both_authentication_and_capabilities() {
        let state = HandoverState::Negotiating
            .receive(HandoverMessage::Authenticated)
            .unwrap();
        assert!(!state.features_allowed());
        let state = state.receive(HandoverMessage::Capabilities).unwrap();
        let state = state.receive(HandoverMessage::FeatureReady).unwrap();
        assert!(state.features_allowed());
    }

    #[test]
    fn control_message_rejects_stale_or_wrong_session_bindings() {
        let message = HandoverControlMessage {
            handover_version: HANDOVER_VERSION,
            kind: HandoverControlKind::Hello,
            session_attempt_id: "attempt".to_string(),
            device_id: "phone".to_string(),
            session_id: 7,
            connection_generation: 2,
            nonce: Some("nonce".to_string()),
            peer_nonce: None,
            timestamp: 1_000,
            signature_base64: None,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
        };
        message.validate("attempt", "phone", 7, 2, 1_000).unwrap();
        assert!(message.validate("attempt", "phone", 7, 3, 1_000).is_err());
        assert!(message
            .validate("attempt", "phone", 7, 2, 1_000 + 5 * 60 * 1000 + 1)
            .is_err());
    }
}
