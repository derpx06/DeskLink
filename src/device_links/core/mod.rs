//! Shared connection-core contracts.
//!
//! The daemon still owns the existing GTK-facing state during the migration,
//! but all new connection and feature code uses these product-neutral types.

#![allow(dead_code)]

pub mod capability_registry;
pub mod device_manager;
pub mod device_session;
pub mod errors;
pub mod events;
pub mod packet_router;
pub mod service;
pub mod session_link;
pub mod transfer_manager;
