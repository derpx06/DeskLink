use super::errors::FeatureError;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_CONNECTIVITY_REPORT, PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
    PACKET_TYPE_CONTACTS_RESPONSE_VCARDS, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST, PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION, PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_CANCEL, PACKET_TYPE_NOTIFICATION_REPLY,
    PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PING, PACKET_TYPE_PRESENTER,
    PACKET_TYPE_RUNCOMMAND, PACKET_TYPE_SCREEN_ERROR, PACKET_TYPE_SCREEN_FRAME,
    PACKET_TYPE_SCREEN_READY, PACKET_TYPE_SCREEN_REQUEST, PACKET_TYPE_SCREEN_STOP,
    PACKET_TYPE_SFTP, PACKET_TYPE_SFTP_REQUEST, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_SMS_MESSAGES, PACKET_TYPE_SYSTEMVOLUME, PACKET_TYPE_SYSTEMVOLUME_REQUEST,
    PACKET_TYPE_TELEPHONY,
};

pub struct FeatureContext;

pub enum FeatureResult {
    NoReply,
    Reply(NetworkPacket),
}

pub trait FeatureHandler: Send + Sync {
    fn id(&self) -> &'static str;
    fn incoming_capabilities(&self) -> &'static [&'static str];
    fn outgoing_capabilities(&self) -> &'static [&'static str];
    fn handle(
        &self,
        _context: &FeatureContext,
        _packet: NetworkPacket,
    ) -> Result<FeatureResult, FeatureError>;
}

/// The capability list is deliberately derived from the packet routes that
/// exist in `packet_handler.rs` and `commands.rs`. Unsupported protocol names
/// must not be advertised merely because Android knows about them.
pub fn desktop_capabilities() -> (Vec<String>, Vec<String>) {
    let incoming = [
        PACKET_TYPE_PING,
        PACKET_TYPE_MOUSEPAD_REQUEST,
        PACKET_TYPE_PRESENTER,
        PACKET_TYPE_SHARE_REQUEST,
        PACKET_TYPE_CLIPBOARD,
        PACKET_TYPE_CLIPBOARD_CONNECT,
        PACKET_TYPE_LOCK,
        PACKET_TYPE_LOCK_REQUEST,
        PACKET_TYPE_MPRIS,
        PACKET_TYPE_MPRIS_REQUEST,
        PACKET_TYPE_BATTERY,
        PACKET_TYPE_NOTIFICATION,
        PACKET_TYPE_NOTIFICATION_CANCEL,
        PACKET_TYPE_NOTIFICATION_REQUEST,
        PACKET_TYPE_SYSTEMVOLUME,
        PACKET_TYPE_SYSTEMVOLUME_REQUEST,
        PACKET_TYPE_RUNCOMMAND,
        PACKET_TYPE_SFTP,
        PACKET_TYPE_SCREEN_REQUEST,
        PACKET_TYPE_SCREEN_READY,
        PACKET_TYPE_SCREEN_FRAME,
        PACKET_TYPE_SCREEN_STOP,
        PACKET_TYPE_SCREEN_ERROR,
        PACKET_TYPE_FINDMYPHONE_REQUEST,
        PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
        PACKET_TYPE_CONTACTS_RESPONSE_VCARDS,
        PACKET_TYPE_SMS_MESSAGES,
        PACKET_TYPE_TELEPHONY,
        PACKET_TYPE_CONNECTIVITY_REPORT,
    ];
    let outgoing = [
        PACKET_TYPE_PING,
        PACKET_TYPE_SHARE_REQUEST,
        PACKET_TYPE_CLIPBOARD,
        PACKET_TYPE_CLIPBOARD_CONNECT,
        PACKET_TYPE_LOCK_REQUEST,
        PACKET_TYPE_FINDMYPHONE_REQUEST,
        PACKET_TYPE_MPRIS_REQUEST,
        PACKET_TYPE_SFTP_REQUEST,
        PACKET_TYPE_NOTIFICATION_REQUEST,
        PACKET_TYPE_NOTIFICATION_REPLY,
        PACKET_TYPE_NOTIFICATION_ACTION,
        PACKET_TYPE_SCREEN_REQUEST,
        PACKET_TYPE_SCREEN_STOP,
        PACKET_TYPE_SYSTEMVOLUME,
        crate::device_links::packet::PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS,
        crate::device_links::packet::PACKET_TYPE_CONTACTS_REQUEST_VCARDS_BY_UID,
        crate::device_links::packet::PACKET_TYPE_SMS_REQUEST_CONVERSATIONS,
        crate::device_links::packet::PACKET_TYPE_SMS_REQUEST_CONVERSATION,
        crate::device_links::packet::PACKET_TYPE_SMS_REQUEST_ATTACHMENT,
        crate::device_links::packet::PACKET_TYPE_TELEPHONY_REQUEST_MUTE,
    ];
    let mut outgoing = outgoing.into_iter().map(str::to_string).collect::<Vec<_>>();
    if crate::platform::upower::available() {
        outgoing.push(PACKET_TYPE_BATTERY.to_string());
    }
    (incoming.into_iter().map(str::to_string).collect(), outgoing)
}
