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
    if let Ok(bytes) = std::fs::read(&path) {
        return bytes
            .try_into()
            .map_err(|_| format!("{} is not a key", path.display()));
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

/// Writes `plain` to `path` (atomically, 0600), sealed with `dir`'s key.
pub(crate) fn seal(dir: &Path, path: &Path, aad: &[u8], plain: &[u8]) -> Result<(), String> {
    use openssl::symm::{encrypt_aead, Cipher};
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
    use openssl::symm::{decrypt_aead, Cipher};
    let bytes = std::fs::read(path).ok()?;
    if bytes.len() < NONCE_BYTES + TAG_BYTES {
        return None;
    }
    let key = key(dir).ok()?;
    let (nonce, rest) = bytes.split_at(NONCE_BYTES);
    let (sealed, tag) = rest.split_at(rest.len() - TAG_BYTES);
    decrypt_aead(Cipher::aes_256_gcm(), &key, Some(nonce), aad, sealed, tag)
        .map_err(|_| eprintln!("[DeskLink] {} could not be read", path.display()))
        .ok()
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
    match open(dir, path, aad).and_then(|plain| serde_json::from_slice(&plain).ok()) {
        Some(value) => value,
        None => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|elapsed| elapsed.as_millis())
                .unwrap_or(0);
            let aside = path.with_extension(format!("corrupt-{now}"));
            let _ = std::fs::rename(path, &aside);
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

/// Seals `plain` into `path` on the writer thread, so the caller (often the
/// GTK thread: a note being typed) never waits on encryption or fsync. Only
/// the latest contents per file are written.
// ponytail: a write still queued when the process exits is lost (it waits
// milliseconds at most); flush on shutdown if that is ever seen.
pub(crate) fn seal_later(dir: &Path, path: &Path, aad: &[u8], plain: Vec<u8>) {
    if cfg!(test) {
        if let Err(error) = seal(dir, path, aad, &plain) {
            eprintln!("[DeskLink] Could not save: {error}");
        }
        return;
    }
    static PENDING: std::sync::Mutex<Option<Pending>> = std::sync::Mutex::new(None);
    static WAKE: std::sync::OnceLock<std::sync::mpsc::SyncSender<()>> = std::sync::OnceLock::new();
    let wake = WAKE.get_or_init(|| {
        let (wake, woken) = std::sync::mpsc::sync_channel::<()>(1);
        let _ = std::thread::Builder::new()
            .name("desklink-vault".into())
            .spawn(move || {
                while woken.recv().is_ok() {
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
            let _ = seal(dir, path, aad, &plain);
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
        assert_eq!(open(&dir, &path, b"purpose").unwrap(), b"[4]");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
