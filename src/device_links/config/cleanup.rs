//! Leftovers handled at load: removed-feature data (idempotent, never pairing,
//! ids, certificates or preferences) and the one-time move of transfer state,
//! copied and verified before the old copy goes.
//!
//! Sibling edges: none.

use super::*;

/// What features removed in 0.3 left in `base_dir`: the automations' sealed
/// rules and their key.
const REMOVED_FEATURE_DIRS: [&str; 2] = ["automations", "transfers"];

/// Files that older builds kept and nothing reads now: the screen-sharing
/// restore tokens (grants are held in memory only, and cleared on unpair).
const REMOVED_FEATURE_FILES: [&str; 1] = ["remote_desktop_tokens.json"];

/// Deletes what removed features stored in `base_dir`. Whether any was there.
pub(super) fn remove_stored_removed_features(base_dir: &Path) -> bool {
    let dirs = REMOVED_FEATURE_DIRS
        .iter()
        .filter(|name| fs::remove_dir_all(base_dir.join(name)).is_ok())
        .count();
    let files = REMOVED_FEATURE_FILES
        .iter()
        .filter(|name| fs::remove_file(base_dir.join(name)).is_ok())
        .count();
    dirs + files > 0
}

/// Moves the resumable-transfer partials (up to a gigabyte each) out of the
/// config directory, which backup and dotfile tools copy, into the state
/// directory. Quiet. A move that failed half way (another filesystem, a full
/// disk) is finished at the next start: the old folder is only removed once
/// everything in it is in the new one, and files already there are kept.
/// Where transfer state was and where it lives now.
pub(crate) struct TransferStatePaths {
    pub(super) old: PathBuf,
    pub(super) new: PathBuf,
}

impl TransferStatePaths {
    /// Moves state from the old place to the new if need be (file IO), and
    /// gives the directory to use.
    pub fn prepare(self) -> PathBuf {
        if self.old != self.new {
            migrate_transfer_state(&self.old, &self.new);
        }
        self.new
    }
}

/// Starts the error of a settings save that failed on the disk.
pub(crate) const SAVE_FAILED: &str = "Could not save the settings: ";

pub(super) fn migrate_transfer_state(old: &Path, new: &Path) {
    if !old.is_dir() {
        return;
    }
    if let Some(parent) = new.parent() {
        if crate::device_links::private_fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if !new.exists() && fs::rename(old, new).is_ok() {
        return;
    }
    // Another filesystem: copy what is there, then remove the old. Each file
    // is copied under a temporary name, synced, checked against the original
    // (size and SHA-256) and only then renamed into place, so a copy that
    // failed half way (a full disk) leaves no file that looks complete; the
    // original is removed only when everything in it is verifiably in the
    // new place.
    if crate::device_links::private_fs::create_dir_all(new).is_err() {
        return;
    }
    let mut all_copied = true;
    if let Ok(entries) = fs::read_dir(old) {
        for entry in entries.flatten() {
            let source = entry.path();
            let target = new.join(entry.file_name());
            if !source.is_file() {
                // Nothing here has subfolders; one that does is left alone,
                // and so is the folder that holds it.
                all_copied = false;
                continue;
            }
            // Already there and at least as long (identical, or progressed
            // since by the new place's own resumed transfer): kept. A
            // shorter one is what a failed copy leaves, and is replaced.
            if fs::metadata(&target)
                .ok()
                .zip(fs::metadata(&source).ok())
                .is_some_and(|(there, here)| there.is_file() && there.len() >= here.len())
            {
                continue;
            }
            if copy_verified(&source, &target).is_err() {
                all_copied = false;
            }
        }
    }
    if all_copied {
        let _ = fs::remove_dir_all(old);
    }
}

fn sha256_of(path: &Path) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut hasher = openssl::hash::Hasher::new(openssl::hash::MessageDigest::sha256()).ok()?;
    let mut file = fs::File::open(path).ok()?;
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).ok()?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]).ok()?;
    }
    hasher.finish().ok().map(|digest| digest.to_vec())
}

/// Whether `target` already holds exactly `source`'s bytes.
fn same_file_contents(source: &Path, target: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::metadata(source), fs::metadata(target)) else {
        return false;
    };
    a.len() == b.len() && sha256_of(source).is_some() && sha256_of(source) == sha256_of(target)
}

/// Copies `source` to `target` so that `target` is either absent (or what it
/// was) or complete: temp name, fsync, verify, rename.
fn copy_verified(source: &Path, target: &Path) -> Result<(), String> {
    let staged = target.with_extension("migrating");
    let copied = (|| {
        let mut from = fs::File::open(source)?;
        let mut to = crate::device_links::private_fs::create(&staged)?;
        std::io::copy(&mut from, &mut to)?;
        to.sync_all()
    })();
    let verified = copied.is_ok() && same_file_contents(source, &staged);
    if !verified {
        let _ = fs::remove_file(&staged);
        return Err("the copy is not complete".to_string());
    }
    fs::rename(&staged, target).map_err(|error| {
        let _ = fs::remove_file(&staged);
        error.to_string()
    })
}
