//! DeskLink Protocol v9 wire identifiers.
//!
//! These identifiers are used only by the active DeskLink desktop and
//! desklink-mobile applications.  The old KDE Connect-compatible constants
//! remain in `legacy_kdeconnect_v8` as an inactive reference boundary.

pub const PROTOCOL_VERSION: i64 = 9;
pub const MDNS_SERVICE_TYPE: &str = "_desklink._udp";
pub const UDP_PORT: u16 = 1716;
pub const MIN_TCP_PORT: u16 = 1716;
pub const MAX_TCP_PORT: u16 = 1764;

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
pub const PACKET_TYPE_CONTACTS_REQUEST_ALL_UIDS_TIMESTAMPS: &str =
    "desklink.contacts.request_all_uids_timestamps";
pub const PACKET_TYPE_CONTACTS_REQUEST_VCARDS_BY_UID: &str =
    "desklink.contacts.request_vcards_by_uid";
pub const PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS: &str =
    "desklink.contacts.response_uids_timestamps";
pub const PACKET_TYPE_CONTACTS_RESPONSE_VCARDS: &str = "desklink.contacts.response_vcards";
pub const PACKET_TYPE_SMS_REQUEST: &str = "desklink.sms.request";
pub const PACKET_TYPE_SMS_MESSAGES: &str = "desklink.sms.messages";
pub const PACKET_TYPE_SMS_REQUEST_CONVERSATIONS: &str = "desklink.sms.request_conversations";
pub const PACKET_TYPE_SMS_REQUEST_CONVERSATION: &str = "desklink.sms.request_conversation";
pub const PACKET_TYPE_SMS_REQUEST_ATTACHMENT: &str = "desklink.sms.request_attachment";
pub const PACKET_TYPE_SMS_ATTACHMENT_FILE: &str = "desklink.sms.attachment_file";
pub const PACKET_TYPE_TELEPHONY_REQUEST: &str = "desklink.telephony.request";
pub const PACKET_TYPE_TELEPHONY: &str = "desklink.telephony";
pub const PACKET_TYPE_TELEPHONY_REQUEST_MUTE: &str = "desklink.telephony.request_mute";
pub const PACKET_TYPE_CONNECTIVITY_REPORT: &str = "desklink.connectivity_report";

/// Every packet that can be accepted before a device is trusted.
pub const PRE_PAIRING_PACKET_TYPES: &[&str] = &[PACKET_TYPE_PAIR];

/// Native packet identifiers advertised by this implementation.  Keeping the
/// list in one place prevents capabilities from drifting away from handlers.
pub const PACKET_TYPES: &[&str] = &[
    PACKET_TYPE_IDENTITY,
    PACKET_TYPE_PAIR,
    PACKET_TYPE_PING,
    PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_MOUSEPAD_ECHO,
    PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE,
    PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_SHARE_REQUEST_UPDATE,
    PACKET_TYPE_CLIPBOARD,
    PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST,
    PACKET_TYPE_SFTP,
    PACKET_TYPE_SFTP_REQUEST,
    PACKET_TYPE_BATTERY,
    PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_NOTIFICATION_CANCEL,
    PACKET_TYPE_NOTIFICATION_REPLY,
    PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_SYSTEMVOLUME,
    PACKET_TYPE_SYSTEMVOLUME_REQUEST,
    PACKET_TYPE_RUNCOMMAND,
    PACKET_TYPE_RUNCOMMAND_REQUEST,
    PACKET_TYPE_SCREEN_REQUEST,
    PACKET_TYPE_SCREEN_READY,
    PACKET_TYPE_SCREEN_FRAME,
    PACKET_TYPE_SCREEN_STOP,
    PACKET_TYPE_SCREEN_ERROR,
    PACKET_TYPE_PRESENTER,
    PACKET_TYPE_CONTACTS_REQUEST,
    PACKET_TYPE_SMS_REQUEST,
    PACKET_TYPE_TELEPHONY_REQUEST,
    PACKET_TYPE_CONNECTIVITY_REPORT,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_protocol_uses_only_desklink_packet_names() {
        assert_eq!(PROTOCOL_VERSION, 9);
        assert_eq!(MDNS_SERVICE_TYPE, "_desklink._udp");
        assert!(PACKET_TYPES
            .iter()
            .all(|packet| packet.starts_with("desklink.")));
        assert_eq!(PRE_PAIRING_PACKET_TYPES, &["desklink.pair"]);
    }
}
