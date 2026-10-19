//! DeskLink relay: forwards end-to-end encrypted signaling between devices
//! that have allowed each other, reports their presence to each other, and
//! mints TURN credentials for such a pair. It never reads payloads.

pub mod auth;
pub mod config;
pub mod hub;
pub mod limits;
pub mod protocol;
pub mod server;
pub mod turn;

pub use server::{Server, Shared};
