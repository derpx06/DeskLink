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
