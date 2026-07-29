//! KDE Connect-compatible protocol v8 wire identifiers.
//!
//! Retained as legacy KDE Connect-compatible wire identifiers.
//! This is not a user-visible DeskLink product name.

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
