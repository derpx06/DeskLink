use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const PROTOCOL_VERSION: i64 = 8;
pub const PACKET_TYPE_IDENTITY: &str = "kdeconnect.identity";
pub const PACKET_TYPE_PAIR: &str = "kdeconnect.pair";
pub const PACKET_TYPE_PING: &str = "kdeconnect.ping";
pub const PACKET_TYPE_MOUSEPAD_REQUEST: &str = "kdeconnect.mousepad.request";
pub const PACKET_TYPE_SHARE_REQUEST: &str = "kdeconnect.share.request";
pub const PACKET_TYPE_CLIPBOARD: &str = "kdeconnect.clipboard";
pub const PACKET_TYPE_CLIPBOARD_CONNECT: &str = "kdeconnect.clipboard.connect";
pub const PACKET_TYPE_LOCK: &str = "kdeconnect.lock";
pub const PACKET_TYPE_LOCK_REQUEST: &str = "kdeconnect.lock.request";
pub const PACKET_TYPE_FINDMYPHONE_REQUEST: &str = "kdeconnect.findmyphone.request";
pub const PACKET_TYPE_MPRIS: &str = "kdeconnect.mpris";
pub const PACKET_TYPE_MPRIS_REQUEST: &str = "kdeconnect.mpris.request";
pub const PACKET_TYPE_SFTP: &str = "kdeconnect.sftp";
pub const PACKET_TYPE_SFTP_REQUEST: &str = "kdeconnect.sftp.request";
pub const PACKET_TYPE_BATTERY: &str = "kdeconnect.battery";
pub const PACKET_TYPE_NOTIFICATION: &str = "kdeconnect.notification";
pub const PACKET_TYPE_NOTIFICATION_REQUEST: &str = "kdeconnect.notification.request";
pub const PACKET_TYPE_NOTIFICATION_REPLY: &str = "kdeconnect.notification.reply";
pub const PACKET_TYPE_NOTIFICATION_ACTION: &str = "kdeconnect.notification.action";
pub const PACKET_TYPE_SYSTEMVOLUME: &str = "kdeconnect.systemvolume";
pub const PACKET_TYPE_SYSTEMVOLUME_REQUEST: &str = "kdeconnect.systemvolume.request";
pub const PACKET_TYPE_RUNCOMMAND: &str = "kdeconnect.runcommand";
pub const PACKET_TYPE_RUNCOMMAND_REQUEST: &str = "kdeconnect.runcommand.request";
pub const PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE: &str = "kdeconnect.mousepad.keyboardstate";
pub const PACKET_TYPE_SHARE_REQUEST_UPDATE: &str = "kdeconnect.share.request.update";
pub const PACKET_TYPE_SCREEN_REQUEST: &str = "desklink.screen.request";
pub const PACKET_TYPE_SCREEN_READY: &str = "desklink.screen.ready";
pub const PACKET_TYPE_SCREEN_FRAME: &str = "desklink.screen.frame";
pub const PACKET_TYPE_SCREEN_STOP: &str = "desklink.screen.stop";
pub const PACKET_TYPE_SCREEN_ERROR: &str = "desklink.screen.error";

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScreenFrameFormat {
    Jpeg,
    Webp,
    Png,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenFrameHeader {
    pub stream_id: String,
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub format: ScreenFrameFormat,
    pub timestamp_millis: i64,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedScreenFrame {
    pub header: ScreenFrameHeader,
    pub payload: Vec<u8>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScreenFrameCodecError {
    Truncated,
    InvalidHeader,
    InvalidLength,
}

impl std::fmt::Display for ScreenFrameCodecError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated => write!(formatter, "screen frame message is truncated"),
            Self::InvalidHeader => write!(formatter, "screen frame header is invalid"),
            Self::InvalidLength => write!(formatter, "screen frame length exceeds supported range"),
        }
    }
}

impl std::error::Error for ScreenFrameCodecError {}

#[allow(dead_code)]
pub fn encode_screen_frame(
    header: &ScreenFrameHeader,
    payload: &[u8],
) -> Result<Vec<u8>, ScreenFrameCodecError> {
    let header_bytes =
        serde_json::to_vec(header).map_err(|_| ScreenFrameCodecError::InvalidHeader)?;
    let header_len =
        u32::try_from(header_bytes.len()).map_err(|_| ScreenFrameCodecError::InvalidLength)?;
    let payload_len =
        u32::try_from(payload.len()).map_err(|_| ScreenFrameCodecError::InvalidLength)?;

    let mut encoded = Vec::with_capacity(8 + header_bytes.len() + payload.len());
    encoded.extend_from_slice(&header_len.to_be_bytes());
    encoded.extend_from_slice(&header_bytes);
    encoded.extend_from_slice(&payload_len.to_be_bytes());
    encoded.extend_from_slice(payload);
    Ok(encoded)
}

#[allow(dead_code)]
pub fn decode_screen_frame(input: &[u8]) -> Result<DecodedScreenFrame, ScreenFrameCodecError> {
    let mut offset = 0;
    let header_len = read_u32(input, &mut offset)? as usize;
    let header_end = offset
        .checked_add(header_len)
        .ok_or(ScreenFrameCodecError::InvalidLength)?;
    if input.len() < header_end {
        return Err(ScreenFrameCodecError::Truncated);
    }

    let header = serde_json::from_slice(&input[offset..header_end])
        .map_err(|_| ScreenFrameCodecError::InvalidHeader)?;
    offset = header_end;

    let payload_len = read_u32(input, &mut offset)? as usize;
    let payload_end = offset
        .checked_add(payload_len)
        .ok_or(ScreenFrameCodecError::InvalidLength)?;
    if input.len() < payload_end {
        return Err(ScreenFrameCodecError::Truncated);
    }

    Ok(DecodedScreenFrame {
        header,
        payload: input[offset..payload_end].to_vec(),
    })
}

#[allow(dead_code)]
fn read_u32(input: &[u8], offset: &mut usize) -> Result<u32, ScreenFrameCodecError> {
    let end = offset
        .checked_add(4)
        .ok_or(ScreenFrameCodecError::InvalidLength)?;
    if input.len() < end {
        return Err(ScreenFrameCodecError::Truncated);
    }
    let bytes = input[*offset..end]
        .try_into()
        .map_err(|_| ScreenFrameCodecError::Truncated)?;
    *offset = end;
    Ok(u32::from_be_bytes(bytes))
}

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

    #[test]
    fn desklink_screen_packet_constants_use_desklink_namespace() {
        assert_eq!(PACKET_TYPE_SCREEN_REQUEST, "desklink.screen.request");
        assert_eq!(PACKET_TYPE_SCREEN_READY, "desklink.screen.ready");
        assert_eq!(PACKET_TYPE_SCREEN_FRAME, "desklink.screen.frame");
        assert_eq!(PACKET_TYPE_SCREEN_STOP, "desklink.screen.stop");
        assert_eq!(PACKET_TYPE_SCREEN_ERROR, "desklink.screen.error");
    }

    #[test]
    fn screen_frame_codec_round_trips_header_and_payload() {
        let header = ScreenFrameHeader {
            stream_id: "desktop".to_string(),
            sequence: 7,
            width: 1280,
            height: 720,
            format: ScreenFrameFormat::Jpeg,
            timestamp_millis: 1_234_567,
        };
        let encoded = encode_screen_frame(&header, b"frame-bytes").unwrap();

        let decoded = decode_screen_frame(&encoded).unwrap();

        assert_eq!(decoded.header, header);
        assert_eq!(decoded.payload, b"frame-bytes");
    }

    #[test]
    fn screen_frame_codec_rejects_truncated_messages() {
        let error = decode_screen_frame(&[0, 0, 0, 20, b'{']).unwrap_err();

        assert!(matches!(error, ScreenFrameCodecError::Truncated));
    }
}
