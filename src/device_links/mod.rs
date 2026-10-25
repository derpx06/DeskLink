pub(crate) mod calls;
pub mod config;
pub mod connection_log;
pub(crate) mod contacts;
pub mod core;
pub mod daemon;
pub(crate) mod desk_features;
pub(crate) mod desk_notifications;
pub mod device;
pub mod device_info;
pub(crate) mod eis;
pub mod features;
pub(crate) mod folders;
pub(crate) mod glide;
pub(crate) mod lan_scope;
pub(crate) mod limits;
pub(crate) mod logind;
pub(crate) mod media_art;
pub(crate) mod mpris;
pub(crate) mod notes;
pub mod packet;
pub mod pairing;
pub mod pairing_qr;
pub(crate) mod private_fs;
pub mod pty;
pub mod relay_link;
pub mod session_lock;
pub(crate) mod sync;
pub mod transfer_log;
pub mod transport;
pub(crate) mod unlock;
pub(crate) mod vault;
pub mod webrtc;

/// Whether this device takes the leading role against `peer_device_id`.
///
/// **One comparison, used everywhere a role has to be elected** — WebRTC
/// initiator, pairing requester, and any future duplicate-connection tie-break.
/// BGP (RFC 4271 §6.8), ICE (RFC 8445 §7.3.1.1) and mDNS (RFC 6762) all settle
/// simultaneity the same way: compare a value both sides already know and let
/// one side deterministically win. Both peers compute this independently and
/// agree with zero round trips.
///
/// It lives here, extracted from two inline copies in `webrtc::coordinator`,
/// because two role elections that can disagree is not a race that resolves —
/// it is a permanent flap, with each side certain the other should have gone
/// first.
pub fn local_goes_first(local_device_id: &str, peer_device_id: &str) -> bool {
    local_device_id < peer_device_id
}
