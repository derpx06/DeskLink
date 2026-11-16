//! The sealed files on disk: the pair list and one manifest book per pair.
//!
//! Sibling edges: `state`, `scan` (`now_ms` names a quarantined file).
//! Outside: `vault`.

use super::*;

// ── Storage ────────────────────────────────────────────────────────────────

pub(super) const SCHEMA: u32 = 1;
pub(super) const PAIRS_FILE: &str = "pairs.sealed";
pub(super) const PAIRS_AAD: &[u8] = b"desklink-folder-pairs";
pub(super) const BOOK_AAD: &[u8] = b"desklink-folder-manifest";

pub(super) fn load<T: for<'de> Deserialize<'de> + Default>(
    dir: &Path,
    file: &str,
    aad: &[u8],
) -> T {
    let path = dir.join(file);
    match crate::device_links::vault::open(dir, &path, aad) {
        Some(plain) => serde_json::from_slice(&plain).unwrap_or_else(|_| {
            quarantine(&path);
            T::default()
        }),
        None => {
            if path.exists() {
                quarantine(&path);
            }
            T::default()
        }
    }
}

fn quarantine(path: &Path) {
    let _ = fs::rename(path, path.with_extension(format!("corrupt-{}", now_ms())));
    eprintln!("[Folders] A sync file could not be read; set aside");
}

pub(super) fn save<T: Serialize>(dir: &Path, file: &str, aad: &[u8], value: &T) {
    let saved = serde_json::to_vec(value)
        .map_err(|error| error.to_string())
        .and_then(|plain| crate::device_links::vault::seal(dir, &dir.join(file), aad, &plain));
    if saved.is_err() {
        log_limited("save", "Could not save folder sync state");
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub(super) struct Pairs {
    #[serde(default)]
    pub(super) version: u32,
    pub(super) pairs: Vec<Pair>,
    /// Folder pairs of the a4a4fdf build that stopped syncing when the
    /// Bucket replaced them: told once on the Bucket page.
    #[serde(default)]
    pub(super) dropped: u32,
}

pub(super) fn book_file(device: &str, id: &str) -> String {
    let device: String = device
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    format!("{device}-{id}.sealed")
}
