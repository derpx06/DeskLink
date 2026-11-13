//! Reaching a paired device through the DeskLink relay when no LAN link
//! exists: an end-to-end encrypted bootstrap link (signaling only).

pub mod client;
pub mod crypto;
pub mod handshake;
pub mod route;
pub mod transport;

/// The only control for everything off the LAN: the relay client and its
/// TURN. There is no setting or row that turns either on or off. On, the
/// relay (`relay_url` in config.json, else the built-in `DEFAULT_RELAY_URL`)
/// and its TURN credentials are simply used; off, neither runs and no surface
/// mentions them. 1.3 ships LAN-only, so it is `false` (Android:
/// `RELAY_ENABLED` in build.gradle.kts). The persisted `relay_enabled` and
/// `use_turn` are kept in config.json untouched and ignored. The relay code
/// still compiles and its tests still run (they start the client directly,
/// against a loopback relay).
pub const RELAY_ENABLED: bool = false;
