//! DeskLink relay: forwards end-to-end encrypted signaling between devices
//! that have allowed each other, reports their presence to each other, and
//! mints TURN credentials for such a pair. It never reads payloads.

pub mod protocol;

#[cfg(feature = "server")]
pub mod auth;
#[cfg(feature = "server")]
pub mod config;
#[cfg(feature = "server")]
pub mod hub;
#[cfg(feature = "server")]
pub mod limits;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod turn;

#[cfg(feature = "server")]
pub use server::{Server, Shared};
