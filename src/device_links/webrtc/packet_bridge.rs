//! Conversion between an authenticated WebRTC envelope and a DeskLink packet.
//!
//! This is the single transport seam used by the future feature handover. It
//! deliberately accepts only ordinary, paired feature packets: identity,
//! pairing, signaling, and payload-bearing packets stay on their dedicated
//! handshake/signaling/payload paths.

use super::channel::DataChannelSpec;
use super::envelope::{EnvelopeError, MessageEnvelope};
use super::wire_binding::WebRtcWireBinding;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_PAIR, PACKET_TYPE_PRESENTER,
};
use crate::protocol::desklink_v9::{PACKET_TYPE_IDENTITY, PACKET_TYPE_WEBRTC_SIGNAL_V1};

pub const NETWORK_PACKET_MESSAGE_TYPE: &str = "desklink.packet.v1";

#[derive(Debug)]
pub enum PacketBridgeError {
    Envelope(EnvelopeError),
    Serialization(String),
    UnsupportedMessageType,
    ForbiddenPacket,
    PayloadPacket,
}

impl std::fmt::Display for PacketBridgeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "invalid DeskLink WebRTC packet bridge message: {self:?}"
        )
    }
}

impl std::error::Error for PacketBridgeError {}

/// Select the fixed channel contract without letting feature modules expose
/// WebRTC details. Pointer traffic gets a later realtime specialization; all
/// current input packets remain reliable so key/button state cannot be lost.
pub fn channel_for_packet(packet: &NetworkPacket) -> DataChannelSpec {
    if is_replaceable_pointer_motion(packet) {
        DataChannelSpec::INPUT_REALTIME
    } else {
        match packet.packet_type.as_str() {
            PACKET_TYPE_MOUSEPAD_REQUEST | PACKET_TYPE_PRESENTER => DataChannelSpec::INPUT_RELIABLE,
            _ => DataChannelSpec::EVENTS,
        }
    }
}

fn is_replaceable_pointer_motion(packet: &NetworkPacket) -> bool {
    let has_motion = packet.body.contains_key("dx")
        || packet.body.contains_key("dy")
        || packet.body.contains_key("x")
        || packet.body.contains_key("y");
    if !has_motion {
        return false;
    }
    if packet.packet_type == PACKET_TYPE_PRESENTER {
        return !packet.get_bool("stop").unwrap_or(false);
    }
    packet.packet_type == PACKET_TYPE_MOUSEPAD_REQUEST
        && ![
            "singleclick",
            "doubleclick",
            "middleclick",
            "rightclick",
            "singlehold",
            "singlerelease",
            "scroll",
        ]
        .iter()
        .any(|key| packet.get_bool(key).unwrap_or(false))
        && packet.get_str("key").is_none()
        && !packet.body.contains_key("specialKey")
}

pub fn encode_packet(
    wire: &WebRtcWireBinding,
    packet: &NetworkPacket,
    timestamp: i64,
) -> Result<MessageEnvelope, PacketBridgeError> {
    ensure_feature_packet(packet)?;
    let payload = packet
        .serialize_line()
        .map_err(|error| PacketBridgeError::Serialization(error.to_string()))?;
    MessageEnvelope::new(
        &wire.sender_device_id,
        wire.session_id,
        wire.generation,
        &channel_for_packet(packet),
        NETWORK_PACKET_MESSAGE_TYPE,
        &payload,
        timestamp,
    )
    .map_err(PacketBridgeError::Envelope)
}

pub fn decode_packet(
    wire: &WebRtcWireBinding,
    envelope: &MessageEnvelope,
) -> Result<NetworkPacket, PacketBridgeError> {
    envelope
        .validate(&wire.peer_device_id, wire.session_id, wire.generation)
        .map_err(PacketBridgeError::Envelope)?;
    if envelope.message_type != NETWORK_PACKET_MESSAGE_TYPE {
        return Err(PacketBridgeError::UnsupportedMessageType);
    }
    let packet = NetworkPacket::deserialize(
        &envelope
            .decode_payload()
            .map_err(PacketBridgeError::Envelope)?,
    )
    .map_err(|error| PacketBridgeError::Serialization(error.to_string()))?;
    ensure_feature_packet(&packet)?;
    if envelope.channel != channel_for_packet(&packet).label {
        return Err(PacketBridgeError::ForbiddenPacket);
    }
    Ok(packet)
}

fn ensure_feature_packet(packet: &NetworkPacket) -> Result<(), PacketBridgeError> {
    if packet.payload_size.is_some() || packet.payload_transfer_info.is_some() {
        return Err(PacketBridgeError::PayloadPacket);
    }
    if matches!(
        packet.packet_type.as_str(),
        PACKET_TYPE_IDENTITY | PACKET_TYPE_PAIR | PACKET_TYPE_WEBRTC_SIGNAL_V1
    ) {
        return Err(PacketBridgeError::ForbiddenPacket);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire() -> WebRtcWireBinding {
        WebRtcWireBinding::from_attempt(
            "desktop-device",
            "phone-device",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap()
    }

    #[test]
    fn paired_feature_packet_round_trips_through_the_bridge() {
        let mut packet = NetworkPacket::new("desklink.clipboard");
        packet.set("content", "hello from DeskLink");
        let envelope = encode_packet(&wire(), &packet, 1).unwrap();

        let inbound = WebRtcWireBinding::from_attempt(
            "phone-device",
            "desktop-device",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let decoded = decode_packet(&inbound, &envelope).unwrap();
        assert_eq!(decoded.packet_type, packet.packet_type);
        assert_eq!(decoded.get_str("content"), Some("hello from DeskLink"));
    }

    #[test]
    fn payload_packets_cannot_be_misrouted_as_events() {
        let mut packet = NetworkPacket::new("desklink.share.request");
        packet.payload_size = Some(4);
        assert!(matches!(
            encode_packet(&wire(), &packet, 1),
            Err(PacketBridgeError::PayloadPacket)
        ));
    }

    #[test]
    fn pointer_motion_is_realtime_but_buttons_and_keys_are_reliable() {
        let mut motion = NetworkPacket::new(PACKET_TYPE_MOUSEPAD_REQUEST);
        motion.set("dx", 4.0);
        motion.set("dy", -2.0);
        assert_eq!(channel_for_packet(&motion), DataChannelSpec::INPUT_REALTIME);

        let mut click = NetworkPacket::new(PACKET_TYPE_MOUSEPAD_REQUEST);
        click.set("singleclick", true);
        assert_eq!(channel_for_packet(&click), DataChannelSpec::INPUT_RELIABLE);

        let mut presenter_stop = NetworkPacket::new(PACKET_TYPE_PRESENTER);
        presenter_stop.set("stop", true);
        assert_eq!(
            channel_for_packet(&presenter_stop),
            DataChannelSpec::INPUT_RELIABLE
        );
    }
}
