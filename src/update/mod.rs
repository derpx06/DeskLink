//! The update check: tells the person a newer DeskLink exists and how to get
//! it, and nothing more. It never downloads, installs or runs anything
//! (`docs/UPDATES.md`, owner-approved design).
//!
//! A leaf module. Nothing in pairing, transport, WebRTC, the relay or the
//! features calls into it; it can fail, hang or be deleted without any of them
//! noticing. It owns one thread, one state file (`update.json`, not
//! `config.json`) and two settings (`update_check`, `update_channel`). An empty
//! `DESKLINK_UPDATE_URL` at build time leaves it inert: no thread work, no
//! network.
//!
//! Layout. Pure: `model` (versions, the signed manifest and its checks,
//! channels, rollout, the decision), `open` (when the popup may show),
//! `schedule`, `skew` (the older-phone line), `install_source` (what to tell
//! the person, derived locally, never run), `url`. I/O: `fetch` (one HTTPS
//! GET), `store` (the state file). Glue: `engine` (one check, the state),
//! `service` (the thread and its handle), `keys`.
//!
//! May depend on: std, `openssl`, `serde`, `base64`, `device_links::private_fs`
//! and `desklink_config_root` (the state file's place) and `app_settings` (the
//! two settings). Used by: `application` (start), `shell_dbus` (read-only
//! properties) and `window::updates` (the popup and the Settings rows).
//! Nothing here knows a phone, a pairing or an identity.

mod engine;
mod fetch;
mod install_source;
mod keys;
mod model;
mod open;
mod schedule;
mod service;
mod skew;
mod store;
mod strict_json;
mod url;

// The crate-facing surface.
#[allow(unused_imports)]
pub(crate) use engine::Status;
#[allow(unused_imports)]
pub(crate) use install_source::{detect as detect_source, Env, Family, Guidance, Source};
#[allow(unused_imports)]
pub(crate) use model::{Channel, Component, Decision, Glyph, Note, Version};
#[allow(unused_imports)]
pub(crate) use open::{
    plan_open, settle, OpenGuard, OpenInput, OpenPlan, FORCED_WAIT_SECS, OPEN_BUDGET_MS,
};
#[allow(unused_imports)]
pub(crate) use service::{handle, start, top_bar, Command, Snapshot, UpdateHandle};
#[allow(unused_imports)]
pub(crate) use skew::{skew, Skew};

#[cfg(test)]
mod tests;
