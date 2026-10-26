//! Files only this user's DeskLink can read: AES-256-GCM with a random key
//! kept beside them (`<dir>/key`, 0600, in a 0700 directory). For the
//! contacts cache (F5) and the snippets and notes (F6).
//!
//! File layout: nonce (12) ‖ ciphertext ‖ tag (16), with caller-chosen
//! associated data binding a file to what it is for.

use std::path::Path;

const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

/// `dir`'s key, made on first use.
fn key(dir: &Path) -> Result<[u8; 32], String> {
    let path = dir.join("key");
    match std::fs::read(&path) {
        Ok(bytes) => match <[u8; 32]>::try_from(bytes) {
            Ok(key) => return Ok(key),
            // Present but not a key (cut short, emptied): nothing can be
            // opened with it, and it will not mend by itself. It is kept as
            // `key.old-<ms>` and a new one is made below; whatever it sealed
            // is then found damaged and kept aside like any damaged file.
            Err(_) => {
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |since| since.as_millis());
                let _ = std::fs::rename(&path, dir.join(format!("key.old-{stamp}")));
                prune_set_aside(dir, "key.old-");
            }
        },
        // Only a key that is not there is made: any other failure (too many
        // open files, an I/O error) must not orphan every sealed file.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("The encryption key could not be read: {error}")),
    }
    create_private_dir(dir)?;
    let mut key = [0u8; 32];
    openssl::rand::rand_bytes(&mut key).map_err(|error| error.to_string())?;
    super::config::write_private(&path, &key)?;
    Ok(key)
}

fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| error.to_string())
}

/// Files whose last read failed for a reason that says nothing about the
/// file (the key or the disk could not be read): the caller got an empty
/// value, and saving it would overwrite a perfectly good file. Saving to them
/// is refused until the next start reads them (or [`start_fresh`]).
static UNREADABLE: std::sync::Mutex<Vec<std::path::PathBuf>> = std::sync::Mutex::new(Vec::new());
static NOTICES: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn is_unreadable(path: &Path) -> bool {
    UNREADABLE
        .lock()
        .is_ok_and(|paths| paths.iter().any(|known| known == path))
}

/// The person's choice to give up on `path`'s old contents: saving to it
/// works again (its copy `*.corrupt-*` stays beside it).
pub(crate) fn start_fresh(path: &Path) {
    if let Ok(mut paths) = UNREADABLE.lock() {
        paths.retain(|known| known != path);
    }
}

/// The sealed files under a folder named `folder` ("notes", "contacts")
/// whose last read failed: what the page's banner is about.
pub(crate) fn blocked_under(folder: &str) -> Vec<std::path::PathBuf> {
    UNREADABLE.lock().map_or_else(
        |_| Vec::new(),
        |paths| {
            paths
                .iter()
                .filter(|path| {
                    path.parent()
                        .and_then(Path::file_name)
                        .is_some_and(|name| name == folder)
                })
                .cloned()
                .collect()
        },
    )
}

/// "Try Again": forgets that those files failed, so the owner reads them
/// again (and refuses again if they still cannot be read).
pub(crate) fn clear_blocked_under(folder: &str) -> Vec<std::path::PathBuf> {
    let cleared = blocked_under(folder);
    for path in &cleared {
        start_fresh(path);
    }
    cleared
}

/// The file is gone on purpose (an unpair deleted it): nothing is waiting
/// for it to become readable.
pub(crate) fn file_deleted(path: &Path) {
    start_fresh(path);
}

/// What the person should be told, once, about files that could not be read.
pub(crate) fn take_notice() -> Option<String> {
    NOTICES.lock().ok().and_then(|mut notices| {
        (!notices.is_empty()).then(|| notices.drain(..).collect::<Vec<_>>().join(" "))
    })
}

/// Writes `plain` to `path` (atomically, 0600), sealed with `dir`'s key.
pub(crate) fn seal(dir: &Path, path: &Path, aad: &[u8], plain: &[u8]) -> Result<(), String> {
    use openssl::symm::{encrypt_aead, Cipher};
    if is_unreadable(path) {
        return Err(format!(
            "{} could not be read at start, so changes to it are not saved (a copy is kept); restart DeskLink to try again",
            path.display()
        ));
    }
    let key = key(dir)?;
    let mut nonce = [0u8; NONCE_BYTES];
    openssl::rand::rand_bytes(&mut nonce).map_err(|error| error.to_string())?;
    let mut tag = [0u8; TAG_BYTES];
    let sealed = encrypt_aead(
        Cipher::aes_256_gcm(),
        &key,
        Some(&nonce),
        aad,
        plain,
        &mut tag,
    )
    .map_err(|error| error.to_string())?;
    let mut bytes = nonce.to_vec();
    bytes.extend_from_slice(&sealed);
    bytes.extend_from_slice(&tag);
    super::config::write_private(path, &bytes)
}

/// `path`'s contents; `None` when missing, or not sealed with this key and
/// `aad` (logged).
pub(crate) fn open(dir: &Path, path: &Path, aad: &[u8]) -> Option<Vec<u8>> {
    try_open(dir, path, aad).ok()
}

/// Why a sealed file could not be opened: the file is damaged or sealed with
/// another key (set it aside), or the key or the file could not be *read* right
/// now (too many open files, an I/O error: the file may be perfectly good).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum OpenError {
    Damaged,
    Unreadable,
}

fn try_open(dir: &Path, path: &Path, aad: &[u8]) -> Result<Vec<u8>, OpenError> {
    use openssl::symm::{decrypt_aead, Cipher};
    let bytes = std::fs::read(path).map_err(|_| OpenError::Unreadable)?;
    if bytes.len() < NONCE_BYTES + TAG_BYTES {
        return Err(OpenError::Damaged);
    }
    // A key that is missing, unreadable or not a key is not the file's fault.
    let key = key(dir).map_err(|_| OpenError::Unreadable)?;
    let (nonce, rest) = bytes.split_at(NONCE_BYTES);
    let (sealed, tag) = rest.split_at(rest.len() - TAG_BYTES);
    decrypt_aead(Cipher::aes_256_gcm(), &key, Some(nonce), aad, sealed, tag).map_err(|_| {
        eprintln!("[DeskLink] {} could not be read", path.display());
        OpenError::Damaged
    })
}

/// How many set-aside copies of a file are kept (`<name>.corrupt-<ms>`,
/// `key.old-<ms>`): the newest.
const KEEP_ASIDE: usize = 3;

/// Removes all but the newest [`KEEP_ASIDE`] files in `dir` whose names start
/// with `prefix` (the rest of the name is the time in ms).
fn prune_set_aside(dir: &Path, prefix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut asides: Vec<(u128, std::path::PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let stamp = name.to_str()?.strip_prefix(prefix)?.parse::<u128>().ok()?;
            Some((stamp, entry.path()))
        })
        .collect();
    asides.sort();
    let excess = asides.len().saturating_sub(KEEP_ASIDE);
    for (_, path) in asides.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

/// Whether a `<name>.corrupt-*` copy of `length` bytes is already beside `path`.
fn has_aside_of_length(path: &Path, length: Option<u64>) -> bool {
    let (Some(dir), Some(prefix)) = (
        path.parent(),
        path.with_extension("corrupt-")
            .file_name()
            .map(|name| name.to_string_lossy().into_owned()),
    ) else {
        return false;
    };
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .any(|entry| {
            entry.file_name().to_string_lossy().starts_with(&prefix)
                && entry.metadata().ok().map(|meta| meta.len()) == length
        })
}

/// Sealed files waiting for the writer thread, at most this many (each
/// holds only its latest contents).
const MAX_PENDING_WRITES: usize = 64;

/// `path`'s contents as a `T`. A file that exists but cannot be opened or
/// read as a `T` (another key, damage, a newer format) is set aside as
/// `<name>.corrupt-<ms>` — never overwritten by the next save — and the
/// default is returned. A temporary file left by an interrupted save goes.
pub(crate) fn open_or_quarantine<T>(dir: &Path, path: &Path, aad: &[u8]) -> T
where
    T: serde::de::DeserializeOwned + Default,
{
    let _ = std::fs::remove_file(path.with_extension("tmp"));
    if !path.exists() {
        return T::default();
    }
    // A failure to read is tried again a moment later before it is believed.
    let mut opened = try_open(dir, path, aad);
    for _ in 0..3 {
        if opened != Err(OpenError::Unreadable) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
        opened = try_open(dir, path, aad);
    }
    let unreadable = opened == Err(OpenError::Unreadable);
    match opened
        .ok()
        .and_then(|plain| serde_json::from_slice(&plain).ok())
    {
        Some(value) => value,
        None => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or(0);
            let aside = path.with_extension(format!("corrupt-{now}"));
            if unreadable {
                // The file may be fine: copied, never moved, so a later good
                // start still finds it where it was. Nothing is saved over it
                // meanwhile, and the person is told once.
                // Not again for each start: a copy of this length is kept.
                let length = std::fs::metadata(path).map(|meta| meta.len()).ok();
                if !has_aside_of_length(path, length) {
                    let _ = std::fs::copy(path, &aside);
                }
                if let Ok(mut paths) = UNREADABLE.lock() {
                    if !paths.iter().any(|known| known == path) {
                        paths.push(path.to_path_buf());
                    }
                }
                if let Ok(mut notices) = NOTICES.lock() {
                    notices.push(format!(
                        "A saved file ({}) could not be read just now, so its contents are not shown and changes to it are not saved. Nothing was lost: use Try Again on the page, or restart DeskLink.",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                }
            } else {
                let _ = std::fs::rename(path, &aside);
            }
            if let (Some(dir), Some(prefix)) = (
                path.parent(),
                path.with_extension("corrupt-")
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned()),
            ) {
                prune_set_aside(dir, &prefix);
            }
            crate::device_links::limits::log_limited(
                "vault-corrupt",
                "A saved file could not be read; it was set aside, not overwritten",
            );
            T::default()
        }
    }
}

type Pending =
    std::collections::HashMap<std::path::PathBuf, (std::path::PathBuf, Vec<u8>, Vec<u8>)>;

/// Held by whoever is writing a batch (the writer thread, or `flush`).
static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Files waiting for the writer thread (each with only its latest contents).
static PENDING: std::sync::Mutex<Option<Pending>> = std::sync::Mutex::new(None);

/// Drops a queued write for `path` (its file is being deleted: it must not
/// come back).
pub(crate) fn cancel_pending(path: &Path) {
    if let Ok(mut pending) = PENDING.lock() {
        if let Some(pending) = pending.as_mut() {
            pending.remove(path);
        }
    }
}

/// Writes everything still queued, now, on this thread: for shutdown, when
/// the writer thread would be killed with its queue (typing in the last
/// moments before a logout was lost).
pub(crate) fn flush() {
    // Behind a batch the writer thread is writing right now: when this
    // returns, everything queued before the call is on disk.
    let _writing = WRITING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let batch = PENDING
        .lock()
        .ok()
        .and_then(|mut pending| pending.take())
        .unwrap_or_default();
    for (path, (dir, aad, plain)) in batch {
        if seal(&dir, &path, &aad, &plain).is_err() {
            eprintln!(
                "[DeskLink] Could not save {} on the way out",
                path.display()
            );
        }
    }
}

/// Seals `plain` into `path` on the writer thread, so the caller (often the
/// GTK thread: a note being typed) never waits on encryption or fsync. Only
/// the latest contents per file are written.
// A write still queued at shutdown is written by `flush()`.
pub(crate) fn seal_later(dir: &Path, path: &Path, aad: &[u8], plain: Vec<u8>) {
    static WAKE: std::sync::OnceLock<std::sync::mpsc::SyncSender<()>> = std::sync::OnceLock::new();
    let wake = WAKE.get_or_init(|| {
        let (wake, woken) = std::sync::mpsc::sync_channel::<()>(1);
        let _ = std::thread::Builder::new()
            .name("desklink-vault".into())
            .spawn(move || {
                while woken.recv().is_ok() {
                    let _writing = WRITING
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    let batch = PENDING
                        .lock()
                        .ok()
                        .and_then(|mut pending| pending.take())
                        .unwrap_or_default();
                    for (path, (dir, aad, plain)) in batch {
                        if seal(&dir, &path, &aad, &plain).is_err() {
                            crate::device_links::limits::log_limited(
                                "vault-save",
                                "Could not save an encrypted file",
                            );
                        }
                    }
                }
            });
        wake
    });
    if let Ok(mut pending) = PENDING.lock() {
        let pending = pending.get_or_insert_with(Default::default);
        if pending.len() >= MAX_PENDING_WRITES && !pending.contains_key(path) {
            // Never dropped: written here instead.
            if let Err(error) = seal(dir, path, aad, &plain) {
                crate::device_links::limits::log_limited(
                    "vault-save",
                    &format!("Could not save an encrypted file: {error}"),
                );
            }
            return;
        }
        pending.insert(path.to_path_buf(), (dir.to_path_buf(), aad.to_vec(), plain));
    }
    let _ = wake.try_send(());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sealed_files_open_only_with_their_key_and_purpose() {
        let dir = std::env::temp_dir().join(format!("desklink-vault-{}", uuid::Uuid::new_v4()));
        let path = dir.join("thing.sealed");
        seal(&dir, &path, b"purpose", b"[1,2,3]").unwrap();
        assert_eq!(open(&dir, &path, b"purpose").unwrap(), b"[1,2,3]");
        assert!(open(&dir, &path, b"other purpose").is_none());
        let value: Vec<u32> = open_or_quarantine(&dir, &path, b"purpose");
        assert_eq!(value, [1, 2, 3]);
        // Unreadable: set aside, never handed back as empty-and-saved-over.
        std::fs::write(&path, b"garbage that is long enough to look sealed").unwrap();
        let value: Vec<u32> = open_or_quarantine(&dir, &path, b"purpose");
        assert!(value.is_empty());
        assert!(!path.exists());
        let aside = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().contains("corrupt"));
        assert!(aside, "the damaged file is kept beside it");
        seal_later(&dir, &path, b"purpose", b"[4]".to_vec());
        flush();
        // A key that cannot be read (here: a directory in its place) is an
        // error, never a reason to make a new one.
        let unreadable = dir.join("other");
        std::fs::create_dir_all(unreadable.join("key")).unwrap();
        assert!(key(&unreadable).is_err());
        assert!(unreadable.join("key").is_dir(), "left alone");
        assert_eq!(open(&dir, &path, b"purpose").unwrap(), b"[4]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key that cannot be read *right now* is not a damaged file: the
    /// sealed file stays where it is (a copy is kept aside), where the old
    /// code renamed the person's notes away and started them empty.
    #[test]
    fn a_key_that_cannot_be_read_does_not_cost_the_file() {
        let dir = std::env::temp_dir().join(format!("desklink-vault-key-{}", uuid::Uuid::new_v4()));
        let path = dir.join("notes.sealed");
        seal(&dir, &path, b"purpose", b"[7]").unwrap();
        let key_file = dir.join("key");
        let good = std::fs::read(&key_file).unwrap();
        // The key is momentarily unreadable (a directory stands in).
        std::fs::remove_file(&key_file).unwrap();
        std::fs::create_dir(&key_file).unwrap();
        assert_eq!(
            try_open(&dir, &path, b"purpose"),
            Err(OpenError::Unreadable)
        );
        let value: Vec<u32> = open_or_quarantine(&dir, &path, b"purpose");
        assert!(value.is_empty(), "nothing can be read now");
        assert!(path.exists(), "the sealed file is where it was");
        // The key comes back: the same file opens.
        std::fs::remove_dir(&key_file).unwrap();
        std::fs::write(&key_file, good).unwrap();
        let value: Vec<u32> = open_or_quarantine(&dir, &path, b"purpose");
        assert_eq!(value, [7]);
        // A real damage is still set aside by rename.
        assert_eq!(
            try_open(&dir, &path, b"another purpose"),
            Err(OpenError::Damaged)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A sealed file cut short, or opened with another install's key, is
    /// damaged (set aside), never read as something else.
    #[test]
    fn a_truncated_file_and_a_foreign_key_are_damage() {
        let dir = std::env::temp_dir().join(format!("desklink-vault-cut-{}", uuid::Uuid::new_v4()));
        let path = dir.join("n.sealed");
        seal(&dir, &path, b"purpose", b"[1,2,3,4,5,6,7,8,9]").unwrap();
        let whole = std::fs::read(&path).unwrap();
        for cut in [0, 1, whole.len() / 2, whole.len() - 1] {
            std::fs::write(&path, &whole[..cut]).unwrap();
            assert!(open(&dir, &path, b"purpose").is_none(), "cut at {cut}");
        }
        // Another install's key.
        std::fs::write(&path, &whole).unwrap();
        let other =
            std::env::temp_dir().join(format!("desklink-vault-key2-{}", uuid::Uuid::new_v4()));
        seal(&other, &other.join("x.sealed"), b"purpose", b"[0]").unwrap();
        std::fs::copy(other.join("key"), dir.join("key")).unwrap();
        assert!(open(&dir, &path, b"purpose").is_none(), "wrong key");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// Many writers to one file: the last queued contents win, nothing is
    /// torn, and the file always opens.
    #[test]
    fn concurrent_writes_leave_a_whole_file_with_the_latest_contents() {
        let dir = std::env::temp_dir().join(format!("desklink-vault-par-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("p.sealed");
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let (dir, path) = (dir.clone(), path.clone());
                std::thread::spawn(move || {
                    for round in 0..20 {
                        seal_later(&dir, &path, b"p", format!("[{n},{round}]").into_bytes());
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        seal_later(&dir, &path, b"p", b"[99]".to_vec());
        flush();
        assert_eq!(open(&dir, &path, b"p").unwrap(), b"[99]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A key file that is not a key (cut short, emptied) is damage, not a
    /// reason to refuse for ever: it is set aside as `key.old-<ms>` (at most
    /// three kept) and a new one is made.
    #[test]
    fn a_cut_short_key_is_set_aside_and_replaced() {
        let dir =
            std::env::temp_dir().join(format!("desklink-vault-badkey-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for round in 0..5 {
            std::fs::write(dir.join("key"), vec![1u8; 5 + round]).unwrap();
            let made = key(&dir).expect("replaced, not refused");
            assert_eq!(std::fs::read(dir.join("key")).unwrap().len(), 32);
            assert_eq!(made.len(), 32);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let old = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("key.old-"))
            .count();
        assert_eq!(old, KEEP_ASIDE, "the newest three kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Restarts do not pile up copies: one per distinct file, three at most.
    #[test]
    fn set_aside_copies_are_capped_and_not_repeated() {
        let dir = std::env::temp_dir().join(format!("desklink-vault-cap-{}", uuid::Uuid::new_v4()));
        let path = dir.join("n.sealed");
        seal(&dir, &path, b"purpose", b"[1]").unwrap();
        let key_file = dir.join("key");
        let good = std::fs::read(&key_file).unwrap();
        std::fs::remove_file(&key_file).unwrap();
        std::fs::create_dir(&key_file).unwrap();
        for _ in 0..6 {
            // A "restart": the marker is gone, the read fails again.
            start_fresh(&path);
            let _: Vec<u32> = open_or_quarantine(&dir, &path, b"purpose");
        }
        let copies = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().contains(".corrupt-"))
            .count();
        assert_eq!(copies, 1, "one copy of the same file");
        std::fs::remove_dir(&key_file).unwrap();
        std::fs::write(&key_file, good).unwrap();
        assert_eq!(open(&dir, &path, b"purpose").unwrap(), b"[1]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A read that failed for a reason outside the file leaves the file alone:
    /// saving is refused, a copy is kept, the person is told once.
    #[test]
    fn an_unreadable_read_never_lets_the_next_save_overwrite_the_good_file() {
        let dir =
            std::env::temp_dir().join(format!("desklink-vault-unread-{}", uuid::Uuid::new_v4()));
        let path = dir.join("n.sealed");
        seal(&dir, &path, b"purpose", b"[7]").unwrap();
        let key_file = dir.join("key");
        let good = std::fs::read(&key_file).unwrap();
        std::fs::remove_file(&key_file).unwrap();
        std::fs::create_dir(&key_file).unwrap();
        let value: Vec<u32> = open_or_quarantine(&dir, &path, b"purpose");
        assert!(value.is_empty());
        assert!(take_notice().is_some_and(|n| n.contains("Nothing was lost")));
        assert!(seal(&dir, &path, b"purpose", b"[]").is_err(), "refused");
        // The key returns; the file was never touched.
        std::fs::remove_dir(&key_file).unwrap();
        std::fs::write(&key_file, good).unwrap();
        assert_eq!(open(&dir, &path, b"purpose").unwrap(), b"[7]");
        // The person's choice lets saving work again.
        start_fresh(&path);
        seal(&dir, &path, b"purpose", b"[8]").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whatever is queued when the app quits is written by `flush`, not lost
    /// with the writer thread.
    #[test]
    fn flush_writes_what_is_still_queued() {
        let dir =
            std::env::temp_dir().join(format!("desklink-vault-flush-{}", uuid::Uuid::new_v4()));
        let path = dir.join("later.sealed");
        // Queue it by hand the way the writer thread's input is (the test
        // build seals at once, so the queue is filled directly).
        {
            let mut pending = PENDING.lock().unwrap();
            pending.get_or_insert_with(Default::default).insert(
                path.clone(),
                (dir.clone(), b"purpose".to_vec(), b"[9]".to_vec()),
            );
        }
        assert!(!path.exists());
        flush();
        assert_eq!(open(&dir, &path, b"purpose").unwrap(), b"[9]");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A queued write for a file that is being deleted is dropped, so the
    /// file is not brought back after an unpair.
    #[test]
    fn a_cancelled_write_does_not_come_back() {
        let dir =
            std::env::temp_dir().join(format!("desklink-vault-cancel-{}", uuid::Uuid::new_v4()));
        let path = dir.join("gone.sealed");
        {
            let mut pending = PENDING.lock().unwrap();
            pending.get_or_insert_with(Default::default).insert(
                path.clone(),
                (dir.clone(), b"purpose".to_vec(), b"[1]".to_vec()),
            );
        }
        cancel_pending(&path);
        let queued = PENDING
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|pending| pending.contains_key(&path));
        assert!(!queued, "the write was dropped");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
