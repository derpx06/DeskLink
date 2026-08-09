//! Cross-platform binding for one negotiated WebRTC attempt.
//!
//! `DeviceSession` identifiers are local implementation details: the desktop
//! and Android applications are free to allocate different values for the
//! same paired peer.  Data-channel envelopes therefore use this deterministic
//! value derived from the signed `sessionAttemptId`, not either local ID.

pub const WIRE_GENERATION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebRtcWireBinding {
    pub sender_device_id: String,
    pub peer_device_id: String,
    pub session_id: u64,
    pub generation: u64,
}

impl WebRtcWireBinding {
    pub fn from_attempt(
        sender_device_id: impl Into<String>,
        peer_device_id: impl Into<String>,
        attempt_id: &str,
    ) -> Result<Self, String> {
        Ok(Self {
            sender_device_id: sender_device_id.into(),
            peer_device_id: peer_device_id.into(),
            session_id: wire_session_id(attempt_id)?,
            generation: WIRE_GENERATION,
        })
    }
}

/// Returns a positive 60-bit identifier that is stable for a UUID attempt.
/// Both implementations consume the first fifteen hexadecimal digits.  It is
/// intentionally representable by JSON's common signed-number consumers.
pub fn wire_session_id(attempt_id: &str) -> Result<u64, String> {
    let hex: String = attempt_id
        .chars()
        .filter(|character| *character != '-')
        .collect();
    if hex.len() != 32 || !hex.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err("WebRTC session attempt ID must be a UUID".to_string());
    }
    u64::from_str_radix(&hex[..15], 16)
        .map_err(|_| "WebRTC session attempt ID is not valid hexadecimal".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_a_cross_platform_positive_wire_session_id() {
        let binding = WebRtcWireBinding::from_attempt(
            "desktop-device",
            "phone-device",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        assert_eq!(binding.session_id, 0x0123_4567_89ab_cde);
        assert_eq!(binding.generation, WIRE_GENERATION);
        assert_eq!(binding.sender_device_id, "desktop-device");
        assert_eq!(binding.peer_device_id, "phone-device");
    }

    #[test]
    fn rejects_non_uuid_attempt_ids() {
        assert!(wire_session_id("not-a-session").is_err());
    }
}
