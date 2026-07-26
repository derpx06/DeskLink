#![allow(dead_code)]

//! Retained as legacy KDE Connect-compatible wire identifiers.
//! This module is not user-visible DeskLink product branding and is not used
//! by the active DeskLink v9 transport.

pub const PROTOCOL_VERSION: i64 = 8;
pub const IDENTITY: &str = "kdeconnect.identity";
pub const PAIR: &str = "kdeconnect.pair";
pub const PING: &str = "kdeconnect.ping";
pub const CLIPBOARD: &str = "kdeconnect.clipboard";
pub const SHARE_REQUEST: &str = "kdeconnect.share.request";
pub const MOUSEPAD_REQUEST: &str = "kdeconnect.mousepad.request";
pub const NOTIFICATION: &str = "kdeconnect.notification";
pub const MPRIS: &str = "kdeconnect.mpris";
pub const DISCOVERY_SERVICE: &str = "_kdeconnect._udp";
