#![allow(dead_code)] // Wired into the live GStreamer transport in the next slice.

pub(crate) mod audio;
pub(crate) mod audio_session;
pub(crate) mod capture;
pub(crate) mod coordinator;
pub(crate) mod file_protocol;
mod handover;
pub(crate) mod ice;
mod peer_connection;
pub(crate) mod remote_desktop_portal;
pub(crate) mod virtual_display;
// The one portal entry point the UI may call: asking for consent, not input.
#[cfg(debug_assertions)]
pub(crate) use coordinator::debug_inject_desktop_input;
pub(crate) use remote_desktop_portal::preauthorize_remote_control;
pub(crate) mod remote_session;
pub(crate) mod screen;
mod signaling;
pub(crate) mod terminal_bridge;
pub(crate) mod terminal_protocol;
mod text_focus;
pub(crate) mod transfer_board;
pub(crate) mod transfer_manager;
mod x11_desktop;

#[allow(unused_imports)]
pub use handover::{
    AuthenticationTranscript, HandoverControlKind, HandoverControlMessage, HandoverRuntime,
    HandoverState, HANDOVER_MESSAGE_TYPE,
};
#[allow(unused_imports)]
pub use peer_connection::{DesktopWebRtcPeer, PeerEvent};
#[allow(unused_imports)]
pub use signaling::{SignalingMessageType, WebRtcSignalingMessage};

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_HEARTBEAT, PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_PAIR,
    PACKET_TYPE_WEBRTC_SIGNAL_V1,
};

pub const ENVELOPE_VERSION: u32 = 1;
pub const NETWORK_PACKET_MESSAGE_TYPE: &str = "desklink.packet.v1";
pub const MAX_PAYLOAD_BYTES: usize = 128 * 1024;
pub const MAX_ENVELOPE_BYTES: usize = 256 * 1024;
/// The largest message one data channel may carry: 64 KiB.
///
/// Our SDP carries no `a=max-message-size`, so webrtcbin assumes RFC 8841's
/// 64 KiB default, and so does the phone's libwebrtc. A larger send does not
/// fail cleanly: GStreamer's `sctpenc` puts the whole SCTP association in an
/// error state ("Could not write to resource") and every channel goes silent,
/// so the peer is rebuilt every ~16 s for as long as the sender retries (seen
/// with 100 KiB contact replies). Envelopes over this are refused before they
/// reach a data channel, as an ordinary send error.
pub const MAX_WIRE_MESSAGE_BYTES: usize = 64 * 1024;
/// What a feature packet's JSON may take so that, enveloped (base64 ×4/3 and
/// about half a kilobyte of fields), it always fits one wire message. Every
/// sender that packs a list splits or trims to this.
pub const MAX_FEATURE_PACKET_BYTES: usize = 40 * 1024;

/// A message on the liveness channel is a heartbeat: a larger inbound one is
/// dropped and logged (the channel stays). Mirrored by Android.
pub const MAX_LIVENESS_ENVELOPE_BYTES: usize = 2048;
/// A heartbeat is not sent while this much is already queued on the liveness
/// channel: the next one supersedes it (every heartbeat carries `dozing`).
pub const LIVENESS_MAX_BUFFERED_BYTES: u64 = 8192;
/// Flow control on `events`: a send above the high-water mark waits in order
/// and resumes under the low-water mark. Nothing user-visible is dropped.
pub const EVENTS_HIGH_WATER: u64 = 256 * 1024;
pub const EVENTS_LOW_WATER: u64 = 64 * 1024;
/// What may wait for `events` to drain (messages, bytes); past it a send
/// fails with an error the caller reports.
pub const EVENTS_MAX_HELD_MESSAGES: usize = 1024;
pub const EVENTS_MAX_HELD_BYTES: usize = 8 * 1024 * 1024;

/// How long `packet`'s envelope is on the wire (with the longest ids a
/// binding has), for senders and tests to check against
/// [`MAX_WIRE_MESSAGE_BYTES`].
pub fn enveloped_len(packet: &NetworkPacket) -> Result<usize, String> {
    let binding = WebRtcWireBinding {
        sender_device_id: "d".repeat(64),
        peer_device_id: "p".repeat(64),
        session_id: u64::MAX,
        generation: u64::MAX,
    };
    WebRtcPacketBridge::encode(&binding, packet, i64::MAX)?
        .to_json()
        .map(|json| json.len())
}

/// Whether `packet` fits one data-channel message once enveloped.
pub fn fits_on_wire(packet: &NetworkPacket) -> bool {
    enveloped_len(packet).is_ok_and(|length| length <= MAX_WIRE_MESSAGE_BYTES)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WebRtcChannel {
    Control,
    Events,
    InputReliable,
    InputRealtime,
    FileControl,
    FileData,
    /// Reserved for a future explicitly authorised terminal feature. No
    /// packet bridge operation may currently select this channel.
    Terminal,
    /// Heartbeats only (probe, reply, doze report): small and reliable, so a
    /// feature burst on `events` can never queue a probe behind it.
    Liveness,
}

impl WebRtcChannel {
    pub const ALL: [Self; 8] = [
        Self::Control,
        Self::Events,
        Self::InputReliable,
        Self::InputRealtime,
        Self::FileControl,
        Self::FileData,
        Self::Terminal,
        Self::Liveness,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Control => "desklink-control-v1",
            Self::Events => "desklink-events-v1",
            Self::InputReliable => "desklink-input-reliable-v1",
            Self::InputRealtime => "desklink-input-realtime-v1",
            Self::FileControl => "desklink-file-control-v1",
            Self::FileData => "desklink-file-data-v1",
            Self::Terminal => "desklink-terminal-v1",
            Self::Liveness => "desklink-liveness-v1",
        }
    }

    /// The longest text message accepted on this channel.
    pub const fn max_inbound_bytes(self) -> usize {
        match self {
            Self::Liveness => MAX_LIVENESS_ENVELOPE_BYTES,
            _ => MAX_ENVELOPE_BYTES,
        }
    }

    /// The channels a link needs open before it can be feature-ready.
    pub const REQUIRED: [Self; 3] = [Self::Control, Self::Events, Self::Liveness];

    pub const fn ordered(self) -> bool {
        !matches!(self, Self::InputRealtime)
    }

    pub const fn max_retransmits(self) -> Option<u16> {
        if matches!(self, Self::InputRealtime) {
            Some(0)
        } else {
            None
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "desklink-control-v1" => Ok(Self::Control),
            "desklink-events-v1" => Ok(Self::Events),
            "desklink-input-reliable-v1" => Ok(Self::InputReliable),
            "desklink-input-realtime-v1" => Ok(Self::InputRealtime),
            "desklink-file-control-v1" => Ok(Self::FileControl),
            "desklink-file-data-v1" => Ok(Self::FileData),
            "desklink-terminal-v1" => Ok(Self::Terminal),
            "desklink-liveness-v1" => Ok(Self::Liveness),
            _ => Err("Unknown DeskLink WebRTC data channel".to_string()),
        }
    }
}

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
        let session_id = Self::session_id_from_attempt(attempt_id)?;
        Ok(Self {
            sender_device_id: sender_device_id.into(),
            peer_device_id: peer_device_id.into(),
            session_id,
            generation: 1,
        })
    }

    pub fn session_id_from_attempt(attempt_id: &str) -> Result<u64, String> {
        let parsed = Uuid::parse_str(attempt_id)
            .map_err(|_| "WebRTC session attempt ID must be a UUID".to_string())?;
        let compact = parsed.simple().to_string();
        u64::from_str_radix(&compact[..15], 16)
            .map_err(|_| "WebRTC session attempt ID could not be converted".to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebRtcEnvelope {
    pub protocol_version: u32,
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

impl WebRtcEnvelope {
    pub fn create(
        binding: &WebRtcWireBinding,
        channel: WebRtcChannel,
        message_type: impl Into<String>,
        payload: &[u8],
        timestamp: i64,
    ) -> Result<Self, String> {
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err("WebRTC envelope payload is too large".to_string());
        }
        Ok(Self {
            protocol_version: ENVELOPE_VERSION,
            message_id: Uuid::new_v4().to_string(),
            device_id: binding.sender_device_id.clone(),
            session_id: binding.session_id,
            connection_generation: binding.generation,
            channel: channel.label().to_string(),
            message_type: message_type.into(),
            timestamp,
            flags: 0,
            payload_length: payload.len(),
            payload_base64: BASE64.encode(payload),
        })
    }

    pub fn validate(&self, binding: &WebRtcWireBinding) -> Result<Vec<u8>, String> {
        if self.protocol_version != ENVELOPE_VERSION {
            return Err("Unsupported DeskLink WebRTC envelope version".to_string());
        }
        if self.device_id != binding.peer_device_id {
            return Err("WebRTC envelope device binding mismatch".to_string());
        }
        if self.session_id != binding.session_id || self.connection_generation != binding.generation
        {
            return Err("WebRTC envelope generation is stale".to_string());
        }
        if self.message_id.is_empty()
            || Uuid::parse_str(&self.message_id).is_err()
            || self.message_type.is_empty()
        {
            return Err("Malformed WebRTC envelope".to_string());
        }
        WebRtcChannel::parse(&self.channel)?;
        let bytes = BASE64
            .decode(&self.payload_base64)
            .map_err(|_| "Malformed WebRTC envelope payload".to_string())?;
        if bytes.len() != self.payload_length {
            return Err("WebRTC envelope payload length mismatch".to_string());
        }
        if bytes.len() > MAX_PAYLOAD_BYTES {
            return Err("WebRTC envelope payload is too large".to_string());
        }
        Ok(bytes)
    }

    pub fn to_json(&self) -> Result<Vec<u8>, String> {
        serde_json::to_vec(self).map_err(|error| error.to_string())
    }

    pub fn from_json(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() > MAX_ENVELOPE_BYTES {
            return Err("WebRTC envelope is too large".to_string());
        }
        serde_json::from_slice(bytes).map_err(|error| error.to_string())
    }
}

pub struct WebRtcPacketBridge;

impl WebRtcPacketBridge {
    pub fn encode(
        binding: &WebRtcWireBinding,
        packet: &NetworkPacket,
        timestamp: i64,
    ) -> Result<WebRtcEnvelope, String> {
        Self::ensure_feature_packet(packet)?;
        let channel = Self::channel_for(packet);
        let payload = packet.serialize().map_err(|error| error.to_string())?;
        WebRtcEnvelope::create(
            binding,
            channel,
            NETWORK_PACKET_MESSAGE_TYPE,
            &payload,
            timestamp,
        )
    }

    pub fn decode(
        binding: &WebRtcWireBinding,
        envelope: &WebRtcEnvelope,
    ) -> Result<NetworkPacket, String> {
        if envelope.message_type != NETWORK_PACKET_MESSAGE_TYPE {
            return Err("Unsupported DeskLink WebRTC message type".to_string());
        }
        let payload = envelope.validate(binding)?;
        let packet = NetworkPacket::deserialize(&payload).map_err(|error| error.to_string())?;
        Self::ensure_feature_packet(&packet)?;
        let lenient_heartbeat = packet.packet_type == PACKET_TYPE_HEARTBEAT
            && envelope.channel == WebRtcChannel::Events.label();
        if envelope.channel != Self::channel_for(&packet).label() && !lenient_heartbeat {
            return Err("DeskLink packet arrived on the wrong WebRTC channel".to_string());
        }
        Ok(packet)
    }

    pub fn channel_for(packet: &NetworkPacket) -> WebRtcChannel {
        if packet.packet_type == PACKET_TYPE_HEARTBEAT {
            WebRtcChannel::Liveness
        } else if Self::is_replaceable_pointer_motion(packet) {
            WebRtcChannel::InputRealtime
        } else if packet.packet_type == PACKET_TYPE_MOUSEPAD_REQUEST {
            WebRtcChannel::InputReliable
        } else {
            WebRtcChannel::Events
        }
    }

    fn is_replaceable_pointer_motion(packet: &NetworkPacket) -> bool {
        let has_motion = ["dx", "dy", "x", "y"]
            .iter()
            .any(|key| packet.body.contains_key(*key));
        if !has_motion {
            return false;
        }
        if packet.packet_type != PACKET_TYPE_MOUSEPAD_REQUEST {
            return false;
        }
        let discrete = [
            "singleclick",
            "doubleclick",
            "middleclick",
            "rightclick",
            "singlehold",
            "singlerelease",
            "scroll",
        ]
        .iter()
        .any(|key| packet.get_bool(key).unwrap_or(false));
        !discrete && packet.get_str("key").is_none() && packet.get_i64("specialKey").is_none()
    }

    fn ensure_feature_packet(packet: &NetworkPacket) -> Result<(), String> {
        if packet.payload_size.is_some() || packet.payload_transfer_info.is_some() {
            return Err("Payload packet must not use the WebRTC event bridge".to_string());
        }
        if matches!(
            packet.packet_type.as_str(),
            crate::device_links::packet::PACKET_TYPE_IDENTITY
                | PACKET_TYPE_PAIR
                | PACKET_TYPE_WEBRTC_SIGNAL_V1
        ) {
            return Err(
                "Handshake or signaling packet must not use the WebRTC event bridge".to_string(),
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_feature_budget_always_fits_one_wire_message() {
        // A packet whose JSON is the whole budget, of characters that need
        // no escaping, fits; one well over it does not.
        let mut packet = NetworkPacket::new("desklink.notes");
        let pad = MAX_FEATURE_PACKET_BYTES - packet.serialize().unwrap().len() - 16;
        packet.set("pad", "x".repeat(pad));
        assert!(packet.serialize().unwrap().len() <= MAX_FEATURE_PACKET_BYTES);
        assert!(fits_on_wire(&packet), "{:?}", enveloped_len(&packet));
        packet.set("pad", "x".repeat(MAX_FEATURE_PACKET_BYTES + 10_000));
        assert!(!fits_on_wire(&packet));
    }
    use crate::device_links::packet::PACKET_TYPE_CLIPBOARD;

    #[test]
    fn wire_binding_is_stable_for_one_attempt() {
        let binding = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();

        assert_eq!(binding.session_id, 0x0123456789abcde);
        assert_eq!(WebRtcChannel::Events.label(), "desklink-events-v1");
        assert_eq!(WebRtcChannel::Liveness.label(), "desklink-liveness-v1");
        assert_eq!(MAX_LIVENESS_ENVELOPE_BYTES, 2048);
        assert_eq!(LIVENESS_MAX_BUFFERED_BYTES, 8192);
        assert_eq!(
            (EVENTS_HIGH_WATER, EVENTS_LOW_WATER),
            (256 * 1024, 64 * 1024)
        );
    }

    /// Heartbeats ride the liveness channel and nothing else does; a
    /// heartbeat on `events` (a mixed build) is still understood.
    #[test]
    fn only_heartbeats_use_the_liveness_channel() {
        let local = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let remote = WebRtcWireBinding::from_attempt(
            "phone",
            "desktop",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let mut beat = NetworkPacket::new(PACKET_TYPE_HEARTBEAT);
        beat.set("reply", true);
        beat.set("dozing", true);
        assert_eq!(
            WebRtcPacketBridge::channel_for(&beat),
            WebRtcChannel::Liveness
        );
        let envelope = WebRtcPacketBridge::encode(&local, &beat, 1).unwrap();
        assert_eq!(envelope.channel, "desklink-liveness-v1");
        assert!(envelope.to_json().unwrap().len() <= MAX_LIVENESS_ENVELOPE_BYTES);
        assert!(WebRtcChannel::Liveness.ordered());
        assert_eq!(
            WebRtcChannel::Liveness.max_retransmits(),
            None,
            "fully reliable"
        );
        assert!(WebRtcChannel::ALL.contains(&WebRtcChannel::Liveness));
        assert_eq!(
            WebRtcChannel::parse("desklink-liveness-v1"),
            Ok(WebRtcChannel::Liveness)
        );
        assert_eq!(WebRtcChannel::Liveness.max_inbound_bytes(), 2048);
        assert_eq!(
            WebRtcChannel::Events.max_inbound_bytes(),
            MAX_ENVELOPE_BYTES
        );
        assert!(WebRtcChannel::REQUIRED.contains(&WebRtcChannel::Liveness));
        let decoded = WebRtcPacketBridge::decode(&remote, &envelope).unwrap();
        assert!(decoded.get_bool("dozing").unwrap());
        // On `events` (an older build): accepted.
        let mut on_events = envelope.clone();
        on_events.channel = WebRtcChannel::Events.label().to_string();
        assert!(WebRtcPacketBridge::decode(&remote, &on_events).is_ok());
        // Anything else on the liveness channel: refused.
        for other in [
            PACKET_TYPE_CLIPBOARD,
            "desklink.notification",
            "desklink.mpris",
        ] {
            let packet = NetworkPacket::new(other);
            let mut sneaky = WebRtcPacketBridge::encode(&local, &packet, 1).unwrap();
            sneaky.channel = WebRtcChannel::Liveness.label().to_string();
            assert!(
                WebRtcPacketBridge::decode(&remote, &sneaky).is_err(),
                "{other}"
            );
        }
    }

    #[test]
    fn packet_bridge_round_trips_an_event_packet() {
        let local = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let remote = WebRtcWireBinding::from_attempt(
            "phone",
            "desktop",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
        packet.set("content", "DeskLink clipboard");

        let envelope = WebRtcPacketBridge::encode(&local, &packet, 1).unwrap();
        let decoded = WebRtcPacketBridge::decode(&remote, &envelope).unwrap();

        assert_eq!(decoded.packet_type, PACKET_TYPE_CLIPBOARD);
        assert_eq!(decoded.get_str("content"), Some("DeskLink clipboard"));
    }

    #[test]
    fn packet_bridge_rejects_bootstrap_packets() {
        let binding = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let packet = NetworkPacket::new(PACKET_TYPE_PAIR);

        assert!(WebRtcPacketBridge::encode(&binding, &packet, 1).is_err());
    }

    #[test]
    fn envelope_rejects_a_non_uuid_message_id() {
        let binding = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
        packet.set("content", "DeskLink clipboard");
        let mut envelope = WebRtcPacketBridge::encode(&binding, &packet, 1).unwrap();
        envelope.message_id = "not-a-uuid".to_string();

        assert!(WebRtcPacketBridge::decode(&binding, &envelope).is_err());
    }
}
