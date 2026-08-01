use serde::{Deserialize, Serialize};

use super::device_info::DeviceInfo;
use super::pairing::PairState;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeviceStatus {
    Discovered,
    Connected,
    PairRequested,
    PairRequestedByPeer,
    Paired,
    Unreachable,
}

impl DeviceStatus {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Discovered => "Discovered",
            Self::Connected => "Connected",
            Self::PairRequested => "Pairing requested",
            Self::PairRequestedByPeer => "Pair request",
            Self::Paired => "Paired",
            Self::Unreachable => "Unreachable",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaStatus {
    pub player: String,
    pub player_list: Vec<String>,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub is_playing: bool,
    pub volume: Option<i64>,
    pub position: Option<i64>,
    pub length: Option<i64>,
    pub can_seek: bool,
    pub album_art_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BatteryStatus {
    pub current_charge: i64,
    pub is_charging: bool,
    pub battery_quantity: Option<i64>,
    pub threshold_event: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeviceNotification {
    pub id: String,
    pub app_name: String,
    pub title: String,
    pub text: String,
    pub ticker: String,
    pub is_clearable: bool,
    pub request_reply_id: Option<String>,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VolumeSink {
    pub name: String,
    pub description: String,
    pub volume: i64,
    pub max_volume: Option<i64>,
    pub muted: bool,
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SystemVolumeStatus {
    pub sinks: Vec<VolumeSink>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RemoteCommand {
    pub key: String,
    pub name: String,
    pub command: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SftpStatus {
    pub user: Option<String>,
    pub port: Option<i64>,
    pub path: Option<String>,
    pub password: Option<String>,
    pub directories: Vec<(String, String)>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceView {
    pub id: String,
    pub name: String,
    pub device_type: String,
    pub address: String,
    pub protocol_version: i64,
    pub status: DeviceStatus,
    pub trusted: bool,
    pub verification_key: Option<String>,
    pub last_error: Option<String>,
    pub media_status: Option<MediaStatus>,
    pub battery_status: Option<BatteryStatus>,
    pub notifications: Vec<DeviceNotification>,
    pub volume_status: Option<SystemVolumeStatus>,
    pub available_commands: Vec<RemoteCommand>,
    pub sftp_status: Option<SftpStatus>,
}

impl DeviceView {
    pub fn from_info(info: &DeviceInfo, address: String, paired: bool) -> Self {
        Self {
            id: info.id.clone(),
            name: info.name.clone(),
            device_type: info.device_type.clone(),
            address,
            protocol_version: info.protocol_version,
            trusted: paired,
            status: if paired {
                DeviceStatus::Paired
            } else {
                DeviceStatus::Discovered
            },
            verification_key: None,
            last_error: None,
            media_status: None,
            battery_status: None,
            notifications: Vec::new(),
            volume_status: None,
            available_commands: Vec::new(),
            sftp_status: None,
        }
    }

    pub fn apply_pair_state(&mut self, state: PairState) {
        self.trusted = state == PairState::Paired;
        self.status = match state {
            PairState::NotPaired => DeviceStatus::Connected,
            PairState::Requested => DeviceStatus::PairRequested,
            PairState::RequestedByPeer => DeviceStatus::PairRequestedByPeer,
            PairState::Paired => DeviceStatus::Paired,
        };
    }
}
