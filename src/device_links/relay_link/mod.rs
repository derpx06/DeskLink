//! Reaching a paired device through the DeskLink relay when no LAN link
//! exists: an end-to-end encrypted bootstrap link (signaling only).

pub mod client;
pub mod crypto;
pub mod handshake;
pub mod route;
pub mod transport;

/// The one switch for everything off the LAN: the relay client, TURN, and
/// every row, label and line that mentions them. 1.3 ships LAN-only, so it is
/// `false`; flip it to bring the relay back (Android: `RELAY_ENABLED` in
/// build.gradle.kts). Off, the persisted `relay_enabled`/`relay_url`/`use_turn`
/// settings are kept untouched and ignored. The relay code still compiles and
/// its tests still run (they start the client directly, against a loopback
/// relay).
pub const RELAY_ENABLED: bool = false;

/// Whether a WebRTC peer may use TURN: only with the relay compiled in and the
/// user's "Use TURN" on. TURN comes from the relay; the LAN never needs it.
pub fn turn_allowed(use_turn: bool) -> bool {
    RELAY_ENABLED && use_turn
}
