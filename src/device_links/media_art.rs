//! Album artwork for the phone: a local player's `mpris:artUrl`, read off
//! every UI thread, scaled to at most [`MAX_SIDE`] px and sent as a small JPEG
//! (`albumArtData`, base64) with a stable id (`albumArtId`) the phone caches.
//!
//! Only `file://` and `http(s)://` are read: the URL comes from whatever
//! player is running. HTTP goes through GIO (gvfs), bounded by
//! [`FETCH_TIMEOUT`]; every source is capped at [`MAX_SOURCE_BYTES`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use gtk::gdk_pixbuf::{InterpType, Pixbuf, PixbufLoader};
use gtk::gio;
use gtk::prelude::*;

/// Longest side of the picture sent.
pub(crate) const MAX_SIDE: i32 = 320;
/// The JPEG sent is at most this big; quality drops until it fits.
pub(crate) const MAX_JPEG_BYTES: usize = 48 * 1024;
/// A source image larger than this is not read.
pub(crate) const MAX_SOURCE_BYTES: usize = 2 * 1024 * 1024;
const FETCH_TIMEOUT: Duration = Duration::from_secs(3);
/// Encodings kept, by art URL.
const CACHED: usize = 4;

/// One encoded picture.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Art {
    pub(crate) id: String,
    /// Base64 JPEG.
    pub(crate) data: String,
}

enum Entry {
    Loading,
    Done(Option<Arc<Art>>),
}

static CACHE: Mutex<VecDeque<(String, Entry)>> = Mutex::new(VecDeque::new());

/// Whether a player's art URL may be read at all.
pub(crate) fn allowed(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    lower.starts_with("file://") || lower.starts_with("https://") || lower.starts_with("http://")
}

/// The art for `url` if it is ready. The first call starts reading it on its
/// own thread and returns `None`; `ready` runs once it is done (with art or
/// without), so the caller can push again.
pub(crate) fn art_for(url: &str, ready: impl FnOnce() + Send + 'static) -> Option<Arc<Art>> {
    if !allowed(url) {
        return None;
    }
    {
        let Ok(mut cache) = CACHE.lock() else {
            return None;
        };
        if let Some((_, entry)) = cache.iter().find(|(known, _)| known == url) {
            return match entry {
                Entry::Done(art) => art.clone(),
                Entry::Loading => None,
            };
        }
        if cache.len() >= CACHED {
            cache.pop_front();
        }
        cache.push_back((url.to_string(), Entry::Loading));
    }
    let url = url.to_string();
    let _ = std::thread::Builder::new()
        .name("desklink-album-art".into())
        .spawn(move || {
            let art = read_source(&url)
                .and_then(|bytes| encode(&bytes))
                .map_err(|error| debug_log!("[DeskLink] No album art from {url}: {error}"))
                .ok()
                .map(Arc::new);
            if let Ok(mut cache) = CACHE.lock() {
                if let Some((_, entry)) = cache.iter_mut().find(|(known, _)| *known == url) {
                    *entry = Entry::Done(art);
                }
            }
            ready();
        });
    None
}

/// Reads `url` through GIO: at most [`MAX_SOURCE_BYTES`], within
/// [`FETCH_TIMEOUT`].
fn read_source(url: &str) -> Result<Vec<u8>, String> {
    let cancel = gio::Cancellable::new();
    {
        let cancel = cancel.clone();
        let _ = std::thread::Builder::new()
            .name("desklink-album-art-timeout".into())
            .spawn(move || {
                std::thread::sleep(FETCH_TIMEOUT);
                cancel.cancel();
            });
    }
    let stream = gio::File::for_uri(url)
        .read(Some(&cancel))
        .map_err(|error| error.to_string())?;
    read_capped(stream.upcast_ref(), &cancel)
}

/// Everything in `stream`, or an error once it passes [`MAX_SOURCE_BYTES`].
fn read_capped(stream: &gio::InputStream, cancel: &gio::Cancellable) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = stream
            .read(&mut buffer[..], Some(cancel))
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Ok(bytes);
        }
        bytes.extend_from_slice(&buffer[..read]);
        if bytes.len() > MAX_SOURCE_BYTES {
            return Err("album art is larger than 2 MB".to_string());
        }
    }
}

/// Decodes `bytes`, scales to [`MAX_SIDE`], and encodes a JPEG of at most
/// [`MAX_JPEG_BYTES`].
fn encode(bytes: &[u8]) -> Result<Art, String> {
    let loader = PixbufLoader::new();
    loader.write(bytes).map_err(|error| error.to_string())?;
    loader.close().map_err(|error| error.to_string())?;
    let pixbuf = loader.pixbuf().ok_or("not an image")?;
    encode_pixbuf(&pixbuf)
}

fn encode_pixbuf(pixbuf: &Pixbuf) -> Result<Art, String> {
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let scale = (f64::from(MAX_SIDE) / f64::from(width.max(height))).min(1.0);
    let scaled = pixbuf
        .scale_simple(
            ((f64::from(width) * scale).round() as i32).max(1),
            ((f64::from(height) * scale).round() as i32).max(1),
            InterpType::Bilinear,
        )
        .ok_or("could not scale the album art")?;
    // JPEG has no alpha: flatten onto black in an RGB picture.
    let opaque = if scaled.has_alpha() {
        let (width, height) = (scaled.width(), scaled.height());
        let flat = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, width, height)
            .ok_or("could not flatten the album art")?;
        flat.fill(0x0000_00ff);
        scaled.composite(
            &flat,
            0,
            0,
            width,
            height,
            0.0,
            0.0,
            1.0,
            1.0,
            InterpType::Nearest,
            255,
        );
        flat
    } else {
        scaled
    };
    for quality in [80, 70, 60, 50, 40, 30] {
        let jpeg = opaque
            .save_to_bufferv("jpeg", &[("quality", &quality.to_string())])
            .map_err(|error| error.to_string())?;
        if jpeg.len() <= MAX_JPEG_BYTES {
            return Ok(Art {
                id: art_id(&jpeg),
                data: base64::engine::general_purpose::STANDARD.encode(&jpeg),
            });
        }
    }
    Err("album art does not fit in 48 KB".to_string())
}

/// A stable id for the encoded picture: the first 16 hex digits of its
/// SHA-256.
pub(crate) fn art_id(jpeg: &[u8]) -> String {
    openssl::sha::sha256(jpeg)[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_file_and_http_art_is_read() {
        assert!(allowed("file:///home/me/.cache/spotify/cover.jpg"));
        assert!(allowed("https://i.scdn.co/image/ab67"));
        assert!(allowed("HTTP://example.org/a.png"));
        for refused in [
            "data:image/png;base64,AAAA",
            "smb://server/share/a.jpg",
            "ftp://example.org/a.jpg",
            "/home/me/cover.jpg",
            "javascript:alert(1)",
            "",
        ] {
            assert!(!allowed(refused), "{refused}");
            assert!(art_for(refused, || {}).is_none());
        }
    }

    #[test]
    fn art_is_scaled_and_fits_the_size_cap() {
        // Noise compresses worst: the hardest case for the 48 KB cap.
        let pixbuf = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 1600, 1200).unwrap();
        let mut seed = 0x1234_5678u32;
        let stride = pixbuf.rowstride() as usize;
        unsafe {
            let pixels = pixbuf.pixels();
            for row in 0..1200usize {
                for byte in &mut pixels[row * stride..row * stride + 1600 * 3] {
                    seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *byte = (seed >> 24) as u8;
                }
            }
        }
        let art = encode_pixbuf(&pixbuf).unwrap();
        let jpeg = base64::engine::general_purpose::STANDARD
            .decode(&art.data)
            .unwrap();
        assert!(jpeg.len() <= MAX_JPEG_BYTES, "{} bytes", jpeg.len());
        let decoded = {
            let loader = PixbufLoader::new();
            loader.write(&jpeg).unwrap();
            loader.close().unwrap();
            loader.pixbuf().unwrap()
        };
        assert_eq!((decoded.width(), decoded.height()), (320, 240));
    }

    #[test]
    fn sources_past_two_megabytes_are_refused() {
        let cancel = gio::Cancellable::new();
        let small = gio::MemoryInputStream::from_bytes(&gtk::glib::Bytes::from(&[7u8; 1000][..]));
        assert_eq!(
            read_capped(small.upcast_ref(), &cancel).unwrap().len(),
            1000
        );
        let big = vec![0u8; MAX_SOURCE_BYTES + 1];
        let big = gio::MemoryInputStream::from_bytes(&gtk::glib::Bytes::from(&big[..]));
        assert!(read_capped(big.upcast_ref(), &cancel).is_err());
    }

    #[test]
    fn the_same_picture_keeps_its_id() {
        let pixbuf = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 64, 64).unwrap();
        pixbuf.fill(0x3366_99ff);
        let first = encode_pixbuf(&pixbuf).unwrap();
        let again = encode_pixbuf(&pixbuf).unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(first.id.len(), 16);
        pixbuf.fill(0x9933_66ff);
        assert_ne!(encode_pixbuf(&pixbuf).unwrap().id, first.id);
        assert_eq!(art_id(b"abc"), art_id(b"abc"));
    }
}
