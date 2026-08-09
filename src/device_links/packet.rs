#![allow(dead_code)]

use serde_json::{Map, Value};

/// Active DeskLink protocol v9 identifiers. Legacy KDE Connect-compatible
/// identifiers are retained only in `protocol::legacy_kdeconnect_v8`.
pub const PROTOCOL_VERSION: i64 = 9;
pub const PACKET_TYPE_IDENTITY: &str = "desklink.identity";
pub const PACKET_TYPE_PAIR: &str = "desklink.pair";
pub const PACKET_TYPE_PING: &str = "desklink.ping";
pub const PACKET_TYPE_MOUSEPAD_REQUEST: &str = "desklink.mousepad.request";
pub const PACKET_TYPE_MOUSEPAD_ECHO: &str = "desklink.mousepad.echo";
pub const PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE: &str = "desklink.mousepad.keyboardstate";
pub const PACKET_TYPE_SHARE_REQUEST: &str = "desklink.share.request";
pub const PACKET_TYPE_SHARE_REQUEST_UPDATE: &str = "desklink.share.request.update";
pub const PACKET_TYPE_CLIPBOARD: &str = "desklink.clipboard";
pub const PACKET_TYPE_CLIPBOARD_CONNECT: &str = "desklink.clipboard.connect";
pub const PACKET_TYPE_LOCK: &str = "desklink.lock";
pub const PACKET_TYPE_LOCK_REQUEST: &str = "desklink.lock.request";
pub const PACKET_TYPE_FINDMYPHONE_REQUEST: &str = "desklink.findmyphone.request";
pub const PACKET_TYPE_MPRIS: &str = "desklink.mpris";
pub const PACKET_TYPE_MPRIS_REQUEST: &str = "desklink.mpris.request";
pub const PACKET_TYPE_SFTP: &str = "desklink.sftp";
pub const PACKET_TYPE_SFTP_REQUEST: &str = "desklink.sftp.request";
pub const PACKET_TYPE_BATTERY: &str = "desklink.battery";
pub const PACKET_TYPE_NOTIFICATION: &str = "desklink.notification";
pub const PACKET_TYPE_NOTIFICATION_REQUEST: &str = "desklink.notification.request";
pub const PACKET_TYPE_NOTIFICATION_CANCEL: &str = "desklink.notification.cancel";
pub const PACKET_TYPE_NOTIFICATION_REPLY: &str = "desklink.notification.reply";
pub const PACKET_TYPE_NOTIFICATION_ACTION: &str = "desklink.notification.action";
pub const PACKET_TYPE_SYSTEMVOLUME: &str = "desklink.systemvolume";
pub const PACKET_TYPE_SYSTEMVOLUME_REQUEST: &str = "desklink.systemvolume.request";
pub const PACKET_TYPE_RUNCOMMAND: &str = "desklink.runcommand";
pub const PACKET_TYPE_RUNCOMMAND_REQUEST: &str = "desklink.runcommand.request";
pub const PACKET_TYPE_SCREEN_REQUEST: &str = "desklink.screen.request";
pub const PACKET_TYPE_SCREEN_READY: &str = "desklink.screen.ready";
pub const PACKET_TYPE_SCREEN_FRAME: &str = "desklink.screen.frame";
pub const PACKET_TYPE_SCREEN_STOP: &str = "desklink.screen.stop";
pub const PACKET_TYPE_SCREEN_ERROR: &str = "desklink.screen.error";
pub const PACKET_TYPE_PRESENTER: &str = "desklink.presenter";
pub const PACKET_TYPE_CONTACTS_REQUEST: &str = "desklink.contacts.request";
pub const PACKET_TYPE_SMS_REQUEST: &str = "desklink.sms.request";
pub const PACKET_TYPE_TELEPHONY_REQUEST: &str = "desklink.telephony.request";
pub const PACKET_TYPE_CONNECTIVITY_REPORT: &str = "desklink.connectivity_report";
pub const PACKET_TYPE_WEBRTC_SIGNAL_V1: &str = "desklink.webrtc.signal.v1";

#[derive(Debug, Clone, PartialEq)]
pub struct NetworkPacket {
    pub id: i64,
    pub packet_type: String,
    pub body: Map<String, Value>,
    pub payload_size: Option<i64>,
    pub payload_transfer_info: Option<Map<String, Value>>,
}

impl NetworkPacket {
    pub fn new(packet_type: impl Into<String>) -> Self {
        Self {
            id: now_millis(),
            packet_type: packet_type.into(),
            body: Map::new(),
            payload_size: None,
            payload_transfer_info: None,
        }
    }

    pub fn with_body(packet_type: impl Into<String>, body: Map<String, Value>) -> Self {
        Self {
            body,
            ..Self::new(packet_type)
        }
    }

    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.body.get(key).and_then(Value::as_str)
    }

    pub fn get_i64(&self, key: &str) -> Option<i64> {
        self.body.get(key).and_then(Value::as_i64)
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        self.body.get(key).and_then(Value::as_bool)
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) {
        self.body.insert(key.to_string(), value.into());
    }

    pub fn serialize_line(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut root = Map::new();
        root.insert("id".to_string(), Value::from(self.id));
        root.insert("type".to_string(), Value::from(self.packet_type.clone()));
        root.insert("body".to_string(), Value::Object(self.body.clone()));
        if let Some(size) = self.payload_size {
            root.insert("payloadSize".to_string(), Value::from(size));
        }
        if let Some(info) = &self.payload_transfer_info {
            root.insert(
                "payloadTransferInfo".to_string(),
                Value::Object(info.clone()),
            );
        }
        let mut bytes = serde_json::to_vec(&Value::Object(root))?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    pub fn serialize(&self) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = self.serialize_line()?;
        let _ = bytes.pop();
        Ok(bytes)
    }

    pub fn deserialize(input: &[u8]) -> Result<Self, serde_json::Error> {
        let value: Value = serde_json::from_slice(input)?;
        let object = value.as_object().cloned().unwrap_or_default();
        Ok(Self {
            id: object.get("id").and_then(Value::as_i64).unwrap_or_default(),
            packet_type: object
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            body: object
                .get("body")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default(),
            payload_size: object.get("payloadSize").and_then(Value::as_i64),
            payload_transfer_info: object
                .get("payloadTransferInfo")
                .and_then(Value::as_object)
                .cloned(),
        })
    }
}

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_and_unserializes_newline_delimited_packets() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_PING);
        packet.set("message", "hello");

        let line = packet.serialize_line().unwrap();
        assert_eq!(line.last(), Some(&b'\n'));

        let parsed = NetworkPacket::deserialize(&line).unwrap();
        assert_eq!(parsed.packet_type, PACKET_TYPE_PING);
        assert_eq!(parsed.get_str("message"), Some("hello"));
    }
}
