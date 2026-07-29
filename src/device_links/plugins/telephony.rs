use crate::device_links::device::TelephonyStatus;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_TELEPHONY, PACKET_TYPE_TELEPHONY_REQUEST_MUTE,
};

pub fn request_mute() -> NetworkPacket {
    NetworkPacket::new(PACKET_TYPE_TELEPHONY_REQUEST_MUTE)
}

pub fn parse_status(packet: &NetworkPacket) -> Result<TelephonyStatus, String> {
    if packet.packet_type != PACKET_TYPE_TELEPHONY {
        return Err("Unexpected telephony packet type".to_string());
    }
    let event = packet
        .get_str("event")
        .ok_or_else(|| "Telephony packet has no event".to_string())?;
    if !matches!(event, "ringing" | "talking" | "missedCall" | "sms") {
        return Err("Telephony packet has an unknown event".to_string());
    }
    Ok(TelephonyStatus {
        event: event.to_string(),
        phone_number: packet.get_str("phoneNumber").map(ToString::to_string),
        contact_name: packet.get_str("contactName").map(ToString::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_call_events() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_TELEPHONY);
        packet.set("event", "unknown");
        assert!(parse_status(&packet).is_err());
    }
}
