//! Album artwork for the phone: a local player's `mpris:artUrl`, read off
//! every UI thread, scaled to at most [`MAX_SIDE`] px and sent as a small JPEG
//! (`albumArtData`, base64) with a stable id (`albumArtId`) the phone caches.
//!
//! Only `file://` and `http(s)://` are read: the URL comes from whatever
//! player is running. HTTP goes through GStreamer's `souphttpsrc` (already
//! required for the screen share; nothing extra to install), bounded by
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
/// The JPEG sent is at most this big; quality drops until it fits. Base64
/// in the packet and again in the envelope, it must stay one data-channel
/// message (64 KiB).
pub(crate) const MAX_JPEG_BYTES: usize = 24 * 1024;
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

/// Reads `url`: at most [`MAX_SOURCE_BYTES`], an http(s) one within
/// [`FETCH_TIMEOUT`].
fn read_source(url: &str) -> Result<Vec<u8>, String> {
    if url.to_ascii_lowercase().starts_with("file://") {
        let path = gio::File::for_uri(url).path().ok_or("not a local file")?;
        let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
        return read_capped(file);
    }
    fetch_http(url)
}

/// Everything `source` holds, or an error past [`MAX_SOURCE_BYTES`].
fn read_capped(source: impl std::io::Read) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    std::io::Read::read_to_end(
        &mut std::io::Read::take(source, MAX_SOURCE_BYTES as u64 + 1),
        &mut bytes,
    )
    .map_err(|error| error.to_string())?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err("album art is larger than 2 MB".to_string());
    }
    Ok(bytes)
}

/// Fetches an http(s) URL with `souphttpsrc ! fakesink`, collecting the
/// buffers; gives up past the size cap or the deadline.
fn fetch_http(url: &str) -> Result<Vec<u8>, String> {
    use gst::prelude::*;
    gst::init().map_err(|error| error.to_string())?;
    let source = gst::ElementFactory::make("souphttpsrc")
        .property("location", url)
        .property("timeout", FETCH_TIMEOUT.as_secs() as u32)
        .build()
        .map_err(|error| format!("souphttpsrc unavailable: {error}"))?;
    let sink = gst::ElementFactory::make("fakesink")
        .property("signal-handoffs", true)
        .property("sync", false)
        .build()
        .map_err(|error| error.to_string())?;
    let pipeline = gst::Pipeline::new();
    pipeline
        .add_many([&source, &sink])
        .map_err(|error| error.to_string())?;
    source.link(&sink).map_err(|error| error.to_string())?;
    let bytes = Arc::new(Mutex::new(Vec::<u8>::new()));
    let too_big = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let (bytes, too_big) = (Arc::clone(&bytes), Arc::clone(&too_big));
        sink.connect("handoff", false, move |values| {
            let buffer = values
                .get(1)
                .and_then(|value| value.get::<gst::Buffer>().ok());
            if let (Some(buffer), Ok(mut bytes)) = (buffer, bytes.lock()) {
                if let Ok(map) = buffer.map_readable() {
                    if bytes.len() + map.len() > MAX_SOURCE_BYTES {
                        too_big.store(true, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        bytes.extend_from_slice(&map);
                    }
                }
            }
            None
        });
    }
    let finish = |result: Result<Vec<u8>, String>| {
        let _ = pipeline.set_state(gst::State::Null);
        result
    };
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| error.to_string())?;
    let bus = pipeline.bus().ok_or("no bus")?;
    let deadline = std::time::Instant::now() + FETCH_TIMEOUT;
    loop {
        if too_big.load(std::sync::atomic::Ordering::Relaxed) {
            return finish(Err("album art is larger than 2 MB".to_string()));
        }
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return finish(Err("album art took longer than 3 s".to_string()));
        }
        let waited = gst::ClockTime::from_mseconds(left.as_millis().min(100) as u64);
        match bus.timed_pop_filtered(waited, &[gst::MessageType::Eos, gst::MessageType::Error]) {
            Some(message) => match message.view() {
                gst::MessageView::Eos(_) if too_big.load(std::sync::atomic::Ordering::Relaxed) => {
                    return finish(Err("album art is larger than 2 MB".to_string()));
                }
                gst::MessageView::Eos(_) => {
                    let collected = std::mem::take(&mut *bytes.lock().map_err(|_| "poisoned")?);
                    return finish(Ok(collected));
                }
                gst::MessageView::Error(error) => {
                    return finish(Err(error.error().to_string()));
                }
                _ => {}
            },
            None => continue,
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

/// The phone's cover (`albumArtData`), decoded on the daemon thread: the UI
/// wraps `pixels` in a texture without decoding, and the exported MPRIS
/// player points at `file`.
#[derive(Clone)]
pub struct PhoneArt {
    pub(crate) id: String,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) has_alpha: bool,
    pub(crate) stride: usize,
    pub(crate) pixels: gtk::glib::Bytes,
    pub(crate) file: Option<std::path::PathBuf>,
}

impl std::fmt::Debug for PhoneArt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PhoneArt({}, {}x{})", self.id, self.width, self.height)
    }
}

/// Longest base64 `albumArtData` read: [`MAX_JPEG_BYTES`] encoded.
const MAX_PHONE_ART_CHARS: usize = MAX_JPEG_BYTES.div_ceil(3) * 4;
static PHONE_ART: Mutex<VecDeque<PhoneArt>> = Mutex::new(VecDeque::new());

/// The phone's `albumArtData`, or `None` when it is empty, too big, not
/// base64 or not a JPEG. The same picture again is not decoded again.
pub(crate) fn phone_art(data: &str) -> Option<PhoneArt> {
    if data.is_empty() || data.len() > MAX_PHONE_ART_CHARS {
        return None;
    }
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(data)
        .ok()
        .filter(|jpeg| jpeg.len() <= MAX_JPEG_BYTES && jpeg.starts_with(&[0xff, 0xd8, 0xff]))?;
    let id = art_id(&jpeg);
    if let Some(known) = PHONE_ART
        .lock()
        .ok()
        .and_then(|cache| cache.iter().find(|art| art.id == id).cloned())
    {
        return Some(known);
    }
    let loader = PixbufLoader::with_type("jpeg").ok()?;
    loader.write(&jpeg).ok()?;
    loader.close().ok()?;
    let mut pixbuf = loader.pixbuf()?;
    let (width, height) = (pixbuf.width(), pixbuf.height());
    if width.max(height) > MAX_SIDE {
        let scale = f64::from(MAX_SIDE) / f64::from(width.max(height));
        pixbuf = pixbuf.scale_simple(
            ((f64::from(width) * scale).round() as i32).max(1),
            ((f64::from(height) * scale).round() as i32).max(1),
            InterpType::Bilinear,
        )?;
    }
    let art = PhoneArt {
        file: write_phone_art(&id, &jpeg),
        id,
        width: pixbuf.width(),
        height: pixbuf.height(),
        has_alpha: pixbuf.has_alpha(),
        stride: usize::try_from(pixbuf.rowstride()).ok()?,
        pixels: pixbuf.read_pixel_bytes(),
    };
    if let Ok(mut cache) = PHONE_ART.lock() {
        if cache.len() >= CACHED {
            if let Some(old) = cache.pop_front() {
                if let Some(file) = old.file {
                    let _ = std::fs::remove_file(file);
                }
            }
        }
        cache.push_back(art.clone());
    }
    Some(art)
}

/// `$XDG_RUNTIME_DIR/desklink/art/<id>.jpg`, for the MPRIS `artUrl`.
fn write_phone_art(id: &str, jpeg: &[u8]) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let dir = gtk::glib::user_runtime_dir().join("desklink").join("art");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .ok()?;
    let file = dir.join(format!("{id}.jpg"));
    std::fs::write(&file, jpeg).ok()?;
    Some(file)
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

    /// One HTTP response from a local server, then the fetch result.
    fn fetch_from(body: Vec<u8>, hang: bool) -> Result<Vec<u8>, String> {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let url = format!(
            "http://127.0.0.1:{}/cover.jpg",
            listener.local_addr().unwrap().port()
        );
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 2048];
            let _ = stream.read(&mut request);
            if hang {
                std::thread::sleep(FETCH_TIMEOUT + Duration::from_secs(1));
                return;
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        });
        let fetched = fetch_http(&url);
        let _ = server.join();
        fetched
    }

    #[test]
    fn http_art_is_fetched_with_a_size_cap_and_a_deadline() {
        if gst::init().is_err() || gst::ElementFactory::find("souphttpsrc").is_none() {
            eprintln!("souphttpsrc not installed; skipping");
            return;
        }
        assert_eq!(fetch_from(vec![9u8; 1000], false).unwrap(), vec![9u8; 1000]);
        assert!(
            fetch_from(vec![0u8; MAX_SOURCE_BYTES + 1], false).is_err(),
            "over 2 MB"
        );
        let started = std::time::Instant::now();
        assert!(
            fetch_from(Vec::new(), true).is_err(),
            "a server that never answers"
        );
        assert!(started.elapsed() < FETCH_TIMEOUT + Duration::from_secs(2));
    }

    #[test]
    fn sources_past_two_megabytes_are_refused() {
        assert_eq!(read_capped(&[7u8; 1000][..]).unwrap().len(), 1000);
        let exact = vec![0u8; MAX_SOURCE_BYTES];
        assert_eq!(read_capped(&exact[..]).unwrap().len(), MAX_SOURCE_BYTES);
        let big = vec![0u8; MAX_SOURCE_BYTES + 1];
        assert!(read_capped(&big[..]).is_err());
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
