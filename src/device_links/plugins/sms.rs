use crate::device_links::device::SmsMessage;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_SMS_MESSAGES, PACKET_TYPE_SMS_REQUEST_CONVERSATION,
    PACKET_TYPE_SMS_REQUEST_CONVERSATIONS,
};

pub fn request_conversations() -> NetworkPacket {
    NetworkPacket::new(PACKET_TYPE_SMS_REQUEST_CONVERSATIONS)
}

pub fn request_conversation(thread_id: &str) -> Result<NetworkPacket, String> {
    if thread_id.is_empty() || thread_id.len() > 128 {
        return Err("SMS thread id is invalid".to_string());
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_SMS_REQUEST_CONVERSATION);
    packet.set("threadID", thread_id);
    Ok(packet)
}

pub fn parse_messages(packet: &NetworkPacket) -> Result<Vec<SmsMessage>, String> {
    if packet.packet_type != PACKET_TYPE_SMS_MESSAGES {
        return Err("Unexpected SMS packet type".to_string());
    }
    let messages = packet
        .body
        .get("messages")
        .and_then(|value| value.as_array())
        .ok_or_else(|| "SMS packet has no messages array".to_string())?;
    if messages.len() > 4096 {
        return Err("SMS packet contains too many messages".to_string());
    }
    messages
        .iter()
        .map(|message| {
            let object = message
                .as_object()
                .ok_or_else(|| "SMS message is not an object".to_string())?;
            let addresses = object
                .get("addresses")
                .and_then(|value| value.as_array())
                .and_then(|values| values.first())
                .and_then(|value| value.as_object())
                .and_then(|value| value.get("address"))
                .and_then(|value| value.as_str())
                .unwrap_or("Unknown")
                .to_string();
            let body = object
                .get("body")
                .and_then(|value| value.as_str())
                .unwrap_or_default()
                .to_string();
            if body.len() > 1024 * 1024 {
                return Err("SMS body is too large".to_string());
            }
            Ok(SmsMessage {
                id: string_field(object, "_id").unwrap_or_default(),
                thread_id: string_field(object, "thread_id").unwrap_or_default(),
                address: addresses,
                body,
                timestamp: object
                    .get("date")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0),
                read: object
                    .get("read")
                    .and_then(|value| value.as_i64())
                    .unwrap_or(0)
                    != 0,
            })
        })
        .collect()
}

fn string_field(object: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
    object.get(key).and_then(|value| {
        value
            .as_str()
            .map(ToString::to_string)
            .or_else(|| value.as_i64().map(|value| value.to_string()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_android_sms_message_shape() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SMS_MESSAGES);
        packet.body = serde_json::json!({
            "messages": [{"_id": 7, "thread_id": 2, "date": 10, "read": 1,
                "body": "hello", "addresses": [{"address": "+123"}]}]
        })
        .as_object()
        .unwrap()
        .clone();
        let messages = parse_messages(&packet).unwrap();
        assert_eq!(messages[0].address, "+123");
        assert_eq!(messages[0].body, "hello");
    }
}
