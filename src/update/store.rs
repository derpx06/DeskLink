//! The update state file, `~/.config/desklink/update.json` (0600): its own
//! file, not `config.json`, so per-record salvage, settings export and
//! unpairing never see it. Written atomically. A missing, damaged or odd file
//! means a fresh start, never a panic and never an error shown.
//!
//! The local install id lives here. It only picks the rollout bucket on this
//! computer and is never sent or logged.
//!
//! Sibling edges: none. Outside: `private_fs`, `serde_json`, `openssl::rand`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct Store {
    /// 32 lower-case hex digits (128 random bits).
    pub(super) install_id: String,
    pub(super) etag: Option<String>,
    pub(super) last_checked: u64,
    pub(super) last_issued_at: u64,
    pub(super) skipped_version: Option<String>,
    pub(super) snoozed_version: Option<String>,
    pub(super) snoozed_until: u64,
    /// When the current run of failed checks began, with its code.
    pub(super) failed_since: Option<u64>,
    pub(super) fail_reason: Option<String>,
    /// Failed checks in a row (the backoff step).
    pub(super) failures: u32,
    /// The last fully verified manifest as served, checked again on load.
    pub(super) cached: Option<String>,
}

fn fresh_id() -> String {
    let mut bytes = [0u8; 16];
    if openssl::rand::rand_bytes(&mut bytes).is_err() {
        // Not worth failing for: a time-derived id only picks a bucket.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |d| d.as_nanos());
        bytes = nanos.to_be_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Default for Store {
    fn default() -> Self {
        Store {
            install_id: fresh_id(),
            etag: None,
            last_checked: 0,
            last_issued_at: 0,
            skipped_version: None,
            snoozed_version: None,
            snoozed_until: 0,
            failed_since: None,
            fail_reason: None,
            failures: 0,
            cached: None,
        }
    }
}

/// An ETag we are willing to store and send back: printable ASCII, short, no
/// quotes-breaking or header-splitting characters.
pub(super) fn etag_ok(etag: &str) -> bool {
    !etag.is_empty()
        && etag.len() <= 128
        && etag.bytes().all(|b| (0x21..0x7f).contains(&b) || b == b' ')
}

/// Exactly 32 lower-case hex characters (16 bytes); anything else is replaced.
pub(super) fn install_id_ok(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A fresh state that keeps `last_issued_at` and `install_id` read out of
/// damaged JSON text (a scan for their keys, not a parse).
fn salvage(bytes: &[u8]) -> Store {
    let mut store = Store::default();
    let text = String::from_utf8_lossy(bytes);
    let after = |key: &str| {
        text.find(&format!("\"{key}\":"))
            .map(|at| text[at + key.len() + 3..].trim_start())
    };
    if let Some(rest) = after("last_issued_at") {
        let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
        store.last_issued_at = digits.parse().unwrap_or(0);
    }
    if let Some(rest) = after("install_id") {
        let id: String = rest.trim_start_matches('"').chars().take(32).collect();
        if install_id_ok(&id) {
            store.install_id = id;
        }
    }
    store
}

impl Store {
    /// The raw install id bytes (16), for the rollout bucket.
    pub(super) fn install_id_bytes(&self) -> Vec<u8> {
        (0..16)
            .map(|i| u8::from_str_radix(&self.install_id[i * 2..i * 2 + 2], 16).unwrap_or(0))
            .collect()
    }

    pub(super) fn default_path() -> PathBuf {
        crate::device_links::config::desklink_config_root().join("update.json")
    }

    /// The saved state, or a fresh one when the file is missing or damaged.
    /// From a damaged file the two values worth keeping are salvaged by
    /// reading their text: the replay ratchet (so a damaged file cannot be
    /// used to take back what was seen) and the install id.
    pub(super) fn load(path: &Path) -> Store {
        let bytes = std::fs::read(path)
            .ok()
            .filter(|bytes| bytes.len() <= 256 * 1024);
        let mut store: Store = match &bytes {
            Some(bytes) => serde_json::from_slice(bytes).unwrap_or_else(|_| salvage(bytes)),
            None => Store::default(),
        };
        if !install_id_ok(&store.install_id) {
            store.install_id = fresh_id();
        }
        if store.etag.as_deref().is_some_and(|e| !etag_ok(e)) {
            store.etag = None;
        }
        store
    }

    /// Whole or not at all. Failure is returned to be ignored by the caller:
    /// losing this file costs one extra check.
    pub(super) fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            crate::device_links::private_fs::create_dir_all(dir)?;
        }
        let temporary = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(self).map_err(std::io::Error::other)?;
        {
            use std::io::Write;
            let mut file = crate::device_links::private_fs::create(&temporary)?;
            file.write_all(&bytes)?;
            // On disk before the rename makes it the state.
            file.sync_all()?;
        }
        std::fs::rename(&temporary, path)
    }
}
