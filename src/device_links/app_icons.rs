//! The phone's app icons for its notifications (`appIconHash`, `appIcon`).
//!
//! The phone sends each app's launcher icon once per connection: every
//! notification names it by `appIconHash`, and the first one of an app also
//! carries the PNG (`appIcon`, base64). Icons are kept by hash, in memory and
//! in a small on-disk cache, so a desk restart does not lose them. A peer's
//! picture is untrusted: only a small PNG whose hash is its own, decoded
//! within a size bound, is kept.
//!
//! Depends on `image_limits` and `private_fs` only.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use base64::Engine;

/// The PNG may be this big at most (the phone aims for 6 KB).
pub(crate) const MAX_PNG_BYTES: usize = 8 * 1024;
/// Hex characters of a hash: the first 128 bits of SHA-256.
pub(crate) const HASH_HEX_CHARS: usize = 32;
/// The longest side an icon may have (the phone sends 64 px).
pub(crate) const MAX_SIDE: i32 = 64;
/// Icons kept in memory, and files kept on disk.
const IN_MEMORY: usize = 64;
const ON_DISK: usize = 200;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

static MEMORY: Mutex<VecDeque<(String, Arc<[u8]>)>> = Mutex::new(VecDeque::new());

/// What a notification's `appIconHash` may be: 32 lowercase hex characters.
pub(crate) fn valid_hash(hash: &str) -> bool {
    hash.len() == HASH_HEX_CHARS
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The hash of `png`, as the phone computes it.
pub(crate) fn hash_of(png: &[u8]) -> String {
    openssl::sha::sha256(png)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>()[..HASH_HEX_CHARS]
        .to_string()
}

/// Whether `png` is an icon to keep under `hash`: a PNG, small, of that
/// hash, that decodes within the bound to at most [`MAX_SIDE`] px.
fn acceptable(hash: &str, png: &[u8]) -> bool {
    png.len() <= MAX_PNG_BYTES
        && png.starts_with(PNG_SIGNATURE)
        && valid_hash(hash)
        && hash_of(png) == hash
        && crate::image_limits::decode(png, Some("png"), u64::from(MAX_SIDE.unsigned_abs()).pow(2))
            .is_some_and(|pixbuf| pixbuf.width() <= MAX_SIDE && pixbuf.height() <= MAX_SIDE)
}

/// Keeps the phone's `appIcon` (base64 PNG) under `hash`. Returns whether it
/// was kept; anything else is dropped.
pub(crate) fn store(hash: &str, data: &str) -> bool {
    // `known` reads and decodes the file on a memory miss: once.
    let known = known(hash);
    if data.len() > MAX_PNG_BYTES.div_ceil(3) * 4 || known {
        return known;
    }
    let Ok(png) = base64::engine::general_purpose::STANDARD.decode(data) else {
        return false;
    };
    if !acceptable(hash, &png) {
        return false;
    }
    remember(hash, Arc::from(png.as_slice()));
    write_to_disk(hash, &png);
    true
}

/// Whether the icon `hash` is here (memory or disk).
pub(crate) fn known(hash: &str) -> bool {
    png(hash).is_some()
}

/// The PNG of icon `hash`, if it is here.
pub(crate) fn png(hash: &str) -> Option<Arc<[u8]>> {
    if !valid_hash(hash) {
        return None;
    }
    if let Some(found) = MEMORY
        .lock()
        .ok()?
        .iter()
        .find(|(known, _)| known == hash)
        .map(|(_, png)| Arc::clone(png))
    {
        return Some(found);
    }
    // The disk copy is checked again: it is a file anyone with this
    // account could have changed.
    let png = std::fs::read(dir()?.join(format!("{hash}.png"))).ok()?;
    if !acceptable(hash, &png) {
        return None;
    }
    let png: Arc<[u8]> = Arc::from(png.as_slice());
    remember(hash, Arc::clone(&png));
    Some(png)
}

fn remember(hash: &str, png: Arc<[u8]>) {
    if let Ok(mut memory) = MEMORY.lock() {
        memory.retain(|(known, _)| known != hash);
        if memory.len() >= IN_MEMORY {
            memory.pop_front();
        }
        memory.push_back((hash.to_string(), png));
    }
}

/// `~/.cache/desklink/app-icons`.
#[cfg(not(test))]
fn dir() -> Option<PathBuf> {
    Some(
        gtk::glib::user_cache_dir()
            .join("desklink")
            .join("app-icons"),
    )
}

/// Tests: the folder each test set, if any.
#[cfg(test)]
fn dir() -> Option<PathBuf> {
    tests::DIR.with(|dir| dir.borrow().clone())
}

/// Writes the icon, then keeps only the newest [`ON_DISK`] files.
fn write_to_disk(hash: &str, png: &[u8]) {
    let Some(dir) = dir() else {
        return;
    };
    if super::private_fs::create_dir_all(&dir).is_err()
        || super::private_fs::write(&dir.join(format!("{hash}.png")), png).is_err()
    {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| Some((entry.metadata().ok()?.modified().ok()?, entry.path())))
        .collect();
    if files.len() > ON_DISK {
        files.sort();
        for (_, path) in &files[..files.len() - ON_DISK] {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::RefCell;

    thread_local! {
        pub(super) static DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    }

    /// A `side` x `side` PNG.
    pub(crate) fn png_of(side: i32) -> Vec<u8> {
        let pixbuf =
            gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, side, side)
                .unwrap();
        pixbuf.fill(0x3366ccff);
        pixbuf.save_to_bufferv("png", &[]).unwrap()
    }

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn with_dir(run: impl FnOnce(&std::path::Path)) {
        let dir = std::env::temp_dir().join(format!("desklink-icons-{}", uuid::Uuid::new_v4()));
        DIR.with(|cell| cell.replace(Some(dir.clone())));
        run(&dir);
        DIR.with(|cell| cell.replace(None));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_small_png_of_its_own_hash_is_kept_in_memory_and_on_disk() {
        with_dir(|dir| {
            let bytes = png_of(48);
            let hash = hash_of(&bytes);
            assert!(valid_hash(&hash));
            assert!(store(&hash, &b64(&bytes)));
            assert_eq!(png(&hash).as_deref(), Some(bytes.as_slice()));
            assert_eq!(
                std::fs::read(dir.join(format!("{hash}.png"))).unwrap(),
                bytes
            );
            // Gone from memory, read back from disk.
            MEMORY.lock().unwrap().retain(|(known, _)| known != &hash);
            assert!(known(&hash));
        });
    }

    #[test]
    fn anything_else_is_refused() {
        with_dir(|_| {
            let bytes = png_of(48);
            // Another hash than its own.
            assert!(!store(&"0".repeat(32), &b64(&bytes)));
            // Not a PNG.
            let jpeg = b"\xff\xd8\xff\xe0not a png".to_vec();
            assert!(!store(&hash_of(&jpeg), &b64(&jpeg)));
            // Too big a picture, too many bytes, a header that lies.
            let big = png_of(128);
            assert!(!store(&hash_of(&big), &b64(&big)));
            let bomb = crate::image_limits::test_bomb_png();
            assert!(!store(&hash_of(&bomb), &b64(&bomb)));
            let long = "A".repeat(MAX_PNG_BYTES * 2);
            assert!(!store(&"a".repeat(32), &long));
            // Not base64; a hash that is not one.
            assert!(!store(&"b".repeat(32), "%%%"));
            assert!(!valid_hash("../../etc/passwd"));
            assert!(!valid_hash(&"A".repeat(32)));
            assert!(png("../x").is_none());
        });
    }

    /// The pinned vector (`wire_values.json`, `notificationAppIcon`): the
    /// phone's hash of this PNG is the desk's.
    #[test]
    fn the_shared_icon_vector_is_kept_under_its_hash() {
        let f: serde_json::Value =
            serde_json::from_str(include_str!("../../testdata/wire_values.json")).unwrap();
        let icon = &f["notificationAppIcon"];
        assert_eq!(icon["fields"]["hash"], "appIconHash");
        assert_eq!(icon["fields"]["png"], "appIcon");
        assert_eq!(icon["hashHexChars"], HASH_HEX_CHARS);
        assert_eq!(icon["maxPngBytes"], MAX_PNG_BYTES);
        assert_eq!(icon["maxSide"], MAX_SIDE);
        let data = icon["vector"]["png"].as_str().unwrap();
        let hash = icon["vector"]["hash"].as_str().unwrap();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data)
            .unwrap();
        assert_eq!(hash_of(&bytes), hash);
        with_dir(|_| assert!(store(hash, data)));
    }
}
