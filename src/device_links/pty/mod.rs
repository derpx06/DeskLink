//! DeskLink-owned pseudo-terminal sessions.
//!
//! A session's lifetime is deliberately independent of any phone connection:
//! the reader thread drains the PTY whether or not a client is attached, so a
//! build keeps building while the phone is in a pocket.

pub(crate) mod desk;
pub(crate) mod host;
pub(crate) mod launchers;
pub(crate) mod registry;
pub(crate) mod ring;
pub(crate) mod session;
