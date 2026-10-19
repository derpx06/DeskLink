//! Reaching a paired device through the DeskLink relay when no LAN link
//! exists: an end-to-end encrypted bootstrap link (signaling only).

pub mod client;
pub mod crypto;
pub mod handshake;
pub mod route;
pub mod transport;
