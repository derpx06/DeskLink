use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_NOTIFICATION_ACTION, PACKET_TYPE_NOTIFICATION_CANCEL,
    PACKET_TYPE_NOTIFICATION_REPLY, PACKET_TYPE_NOTIFICATION_REQUEST,
};

pub const MAX_NOTIFICATION_TEXT_BYTES: usize = 64 * 1024;

pub fn request_packet() -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_REQUEST);
    packet.set("request", true);
    packet
}

pub fn cancel_packet(notification_id: &str) -> Result<NetworkPacket, String> {
    let notification_id = valid_id(notification_id, "notification id")?;
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_CANCEL);
    packet.set("id", notification_id);
    Ok(packet)
}

pub fn reply_packet(reply_id: &str, message: &str) -> Result<NetworkPacket, String> {
    let reply_id = valid_id(reply_id, "notification reply id")?;
    let message = message.trim();
    if message.is_empty() {
        return Err("Notification reply message is empty".to_string());
    }
    if message.len() > MAX_NOTIFICATION_TEXT_BYTES {
        return Err("Notification reply is too large".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_REPLY);
    packet.set("requestReplyId", reply_id);
    packet.set("message", message);
    Ok(packet)
}

pub fn action_packet(key: &str, action: &str) -> Result<NetworkPacket, String> {
    let key = valid_id(key, "notification key")?;
    let action = valid_id(action, "notification action")?;
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_ACTION);
    packet.set("key", key);
    packet.set("action", action);
    Ok(packet)
}

fn valid_id<'a>(value: &'a str, label: &str) -> Result<&'a str, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err(format!("{label} is required"));
    }
    if value.len() > 512 || value.bytes().any(|byte| byte == b'\n' || byte == b'\r') {
        return Err(format!("{label} is invalid"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_uses_a_dedicated_packet_type() {
        let packet = cancel_packet("notification-1").unwrap();
        assert_eq!(packet.packet_type, PACKET_TYPE_NOTIFICATION_CANCEL);
        assert_eq!(packet.get_str("id"), Some("notification-1"));
    }

    #[test]
    fn replies_reject_empty_text() {
        assert!(reply_packet("reply-1", "  ").is_err());
    }
}
