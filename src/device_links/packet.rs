#![allow(dead_code)]

use serde_json::{Map, Value};

/// Active DeskLink protocol v9 identifiers. Legacy KDE Connect-compatible
/// identifiers are retained only in `protocol::legacy_kdeconnect_v8`.
pub const PROTOCOL_VERSION: i64 = 9;
pub const PACKET_TYPE_IDENTITY: &str = "desklink.identity";
pub const PACKET_TYPE_PAIR: &str = "desklink.pair";
/// QR pairing proof (phone → desktop), LAN bootstrap only. See `pairing_qr`.
/// Must match `DeskLinkProtocol.PACKET_TYPE_PAIR_QR` on Android.
pub const PACKET_TYPE_PAIR_QR: &str = "desklink.pair.qr";
/// Liveness probe for the pinned-TLS bootstrap link.
///
/// Deliberately not a request/response pair. Both ends probe only when the
/// link has been *quiet*, so any real traffic already proves liveness and an
/// active link pays nothing — a heartbeat is simply the traffic of last resort.
/// That also means it needs no sequence number, no echo and no body.
///
/// It exists because `is_closed()` on one side and `visibleDevices` on the
/// other are flags that nothing independently verifies. Both sides have been
/// observed holding a link to a process that no longer existed: once with the
/// desktop discarding 170 of the phone's fresh sockets, and once with the phone
/// refusing to dial a desktop that was sitting idle waiting for it.
pub const PACKET_TYPE_HEARTBEAT: &str = "desklink.heartbeat";
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
pub const PACKET_TYPE_DIGITIZER_SESSION: &str = "desklink.digitizer.session";
pub const PACKET_TYPE_DIGITIZER: &str = "desklink.digitizer";
/// The desk's own settings, reported to a phone (desk → phone).
pub const PACKET_TYPE_SETTINGS: &str = "desklink.settings";
/// A phone changing one of the desk's settings (phone → desk), allow-listed.
pub const PACKET_TYPE_SETTINGS_REQUEST: &str = "desklink.settings.request";
/// Unlock with the phone: the desk's nonce, or the phone asking for one.
pub const PACKET_TYPE_UNLOCK_REQUEST: &str = "desklink.unlock.request";
/// Lock & Hand Off: the terminal to continue on the phone (desk → phone).
pub const PACKET_TYPE_HANDOFF: &str = "desklink.handoff";
pub const PACKET_TYPE_SCREEN_REQUEST: &str = "desklink.screen.request";
pub const PACKET_TYPE_SCREEN_READY: &str = "desklink.screen.ready";
pub const PACKET_TYPE_SCREEN_FRAME: &str = "desklink.screen.frame";
pub const PACKET_TYPE_SCREEN_STOP: &str = "desklink.screen.stop";
pub const PACKET_TYPE_SCREEN_ERROR: &str = "desklink.screen.error";
pub const PACKET_TYPE_PRESENTER: &str = "desklink.presenter";
pub const PACKET_TYPE_CONTACTS_REQUEST: &str = "desklink.contacts.request";
/// Contacts (F5), desk → phone: a fresh snapshot, then the changed vCards.
pub const PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS: &str =
    "desklink.contacts.request_all_uids_timestamps";
pub const PACKET_TYPE_CONTACTS_REQUEST_VCARDS_BY_UID: &str =
    "desklink.contacts.request_vcards_by_uid";
/// Contacts (F5), phone → desk: the snapshot (uids and timestamps), vCards.
pub const PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS: &str =
    "desklink.contacts.response_uids_timestamps";
pub const PACKET_TYPE_CONTACTS_RESPONSE_VCARDS: &str = "desklink.contacts.response_vcards";
/// Snippets and notes (F6), both directions: `op` index, fetch or items.
pub const PACKET_TYPE_SNIPPETS: &str = "desklink.snippets";
pub const PACKET_TYPE_NOTES: &str = "desklink.notes";
/// Glide (F4): the pointer crossing onto the phone (desk→phone), and back
/// (phone→desk). KDE Connect's shareinputdevices edge protocol.
pub const PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST: &str = "desklink.shareinputdevices.request";
pub const PACKET_TYPE_SHARE_INPUT_DEVICES: &str = "desklink.shareinputdevices";
pub const PACKET_TYPE_SMS_REQUEST: &str = "desklink.sms.request";
/// The phone's calls (a full snapshot), a missed call, or a refused request
/// (phone → desk).
pub const PACKET_TYPE_TELEPHONY: &str = "desklink.telephony";
/// Answer, decline, end, silence, mute, unmute or dial (desk → phone).
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
