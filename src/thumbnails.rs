//! Previews of transferred files for the Transfers page: what kind of file
//! each is, and a small picture of it — an image scaled down, or a video's
//! frame at about one second with its length.
//!
//! [`thumbnail`] blocks (decoding, GStreamer) and is only ever run on a
//! worker thread. Results are kept as PNGs under
//! `$XDG_CACHE_HOME/desklink/thumbnails`, named by [`cache_key`], so a file is
//! decoded once until it changes. [`kind_of`] and [`cache_key`] have no
//! GTK in them and are unit-tested.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gtk::gdk_pixbuf::{InterpType, Pixbuf};
use gtk::prelude::*;
use gtk::{gdk, glib};

/// The longest side of a thumbnail, in pixels.
const SIZE: i32 = 320;
/// How long a video gets to show its first frame before it is given up on.
const VIDEO_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileKind {
    Image,
    Video,
    Pdf,
    Other,
}

/// What kind of file `path` is, from its extension.
pub(crate) fn kind_of(path: &Path) -> FileKind {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "jpg" | "jpeg" | "png" | "webp" | "gif" | "heic" | "heif" | "avif" | "bmp" | "tif"
        | "tiff" => FileKind::Image,
        "mp4" | "m4v" | "webm" | "mkv" | "mov" | "avi" | "3gp" => FileKind::Video,
        "pdf" => FileKind::Pdf,
        _ => FileKind::Other,
    }
}

/// The cache file name for `path` as it is now: its path, modification time
/// and size, hashed. A file changed in place gets a new thumbnail.
pub(crate) fn cache_key(path: &Path, modified: std::time::SystemTime, size: u64) -> String {
    let nanos = modified
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_nanos());
    let mut hasher = openssl::sha::Sha256::new();
    hasher.update(path.as_os_str().as_encoded_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&size.to_le_bytes());
    hasher
        .finish()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn cache_dir() -> Option<PathBuf> {
    // Tests must never write the person's cache.
    if cfg!(test) {
        return Some(
            std::env::temp_dir().join(format!("desklink-test-thumbnails-{}", std::process::id())),
        );
    }
    Some(dirs::cache_dir()?.join("desklink").join("thumbnails"))
}

/// A file's preview, ready to show.
pub(crate) struct Thumbnail {
    pub(crate) texture: gdk::Texture,
    /// A video's length.
    pub(crate) duration: Option<Duration>,
}

/// The preview of the image or video at `path`, from the cache or made now.
/// Blocks: call it from a worker thread.
pub(crate) fn thumbnail(path: &Path) -> Result<Thumbnail, String> {
    let meta = std::fs::metadata(path).map_err(|error| error.to_string())?;
    let key = cache_key(
        path,
        meta.modified().map_err(|error| error.to_string())?,
        meta.len(),
    );
    let dir = cache_dir().ok_or("No cache directory")?;
    let (png, secs) = (
        dir.join(format!("{key}.png")),
        dir.join(format!("{key}.secs")),
    );
    let duration = || {
        std::fs::read_to_string(&secs)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .map(Duration::from_secs)
    };
    if let Ok(pixbuf) = Pixbuf::from_file(&png) {
        return Ok(Thumbnail {
            texture: texture(&pixbuf),
            duration: duration(),
        });
    }
    // ponytail: one decode at a time; a pool if a big grid feels slow.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _turn = ONE_AT_A_TIME.lock();
    let (pixbuf, length) = match kind_of(path) {
        FileKind::Image => (image(path)?, None),
        FileKind::Video => video_frame(path)?,
        _ => return Err("No preview for this kind of file".into()),
    };
    let _ = std::fs::create_dir_all(&dir);
    if let Err(error) = pixbuf.savev(&png, "png", &[]) {
        eprintln!("[UI] Could not cache a thumbnail: {error}");
    }
    if let Some(length) = length {
        let _ = std::fs::write(&secs, length.as_secs().to_string());
    }
    Ok(Thumbnail {
        texture: texture(&pixbuf),
        duration: length,
    })
}

fn texture(pixbuf: &Pixbuf) -> gdk::Texture {
    let format = if pixbuf.has_alpha() {
        gdk::MemoryFormat::R8g8b8a8
    } else {
        gdk::MemoryFormat::R8g8b8
    };
    gdk::MemoryTexture::new(
        pixbuf.width(),
        pixbuf.height(),
        format,
        &pixbuf.read_pixel_bytes(),
        pixbuf.rowstride() as usize,
    )
    .upcast()
}

/// An image scaled to fit [`SIZE`], the right way up.
fn image(path: &Path) -> Result<Pixbuf, String> {
    let pixbuf =
        Pixbuf::from_file_at_scale(path, SIZE, SIZE, true).map_err(|error| error.to_string())?;
    Ok(pixbuf.apply_embedded_orientation().unwrap_or(pixbuf))
}

/// A frame from about one second into the video (or a third of the way in,
/// for a shorter one), scaled to fit [`SIZE`], and the video's length.
fn video_frame(path: &Path) -> Result<(Pixbuf, Option<Duration>), String> {
    use gst::prelude::*;
    gst::init().map_err(|error| error.to_string())?;
    let uri = glib::filename_to_uri(path, None).map_err(|error| error.to_string())?;
    let playbin = gst::ElementFactory::make("playbin")
        .property("uri", uri.as_str())
        .build()
        .map_err(|error| error.to_string())?;
    for sink in ["video-sink", "audio-sink"] {
        let fake = gst::ElementFactory::make("fakesink")
            .build()
            .map_err(|error| error.to_string())?;
        playbin.set_property(sink, &fake);
    }
    let result = (|| {
        let bus = playbin.bus().ok_or("No GStreamer bus")?;
        let settle = || -> Result<(), String> {
            let message = bus
                .timed_pop_filtered(
                    gst::ClockTime::from_nseconds(VIDEO_TIMEOUT.as_nanos() as u64),
                    &[gst::MessageType::AsyncDone, gst::MessageType::Error],
                )
                .ok_or("The video took too long to open")?;
            match message.view() {
                gst::MessageView::Error(error) => Err(error.error().to_string()),
                _ => Ok(()),
            }
        };
        playbin
            .set_state(gst::State::Paused)
            .map_err(|error| error.to_string())?;
        settle()?;
        let length = playbin
            .query_duration::<gst::ClockTime>()
            .map(|length| Duration::from_nanos(length.nseconds()));
        let at = length.map_or(Duration::from_secs(1), |length| {
            Duration::from_secs(1).min(length / 3)
        });
        if !at.is_zero()
            && playbin
                .seek_simple(
                    gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                    gst::ClockTime::from_nseconds(at.as_nanos() as u64),
                )
                .is_ok()
        {
            settle()?;
        }
        let caps = gst::Caps::builder("video/x-raw")
            .field("format", "RGBA")
            .field("pixel-aspect-ratio", gst::Fraction::new(1, 1))
            .build();
        let sample = playbin
            .emit_by_name::<Option<gst::Sample>>("convert-sample", &[&caps])
            .ok_or("The video gave no frame")?;
        let structure = sample
            .caps()
            .and_then(|caps| caps.structure(0).map(|s| s.to_owned()))
            .ok_or("The frame has no size")?;
        let width = structure.get::<i32>("width").map_err(|e| e.to_string())?;
        let height = structure.get::<i32>("height").map_err(|e| e.to_string())?;
        let buffer = sample.buffer().ok_or("The frame is empty")?;
        let map = buffer.map_readable().map_err(|error| error.to_string())?;
        let pixels = glib::Bytes::from(map.as_slice());
        let frame = Pixbuf::from_bytes(
            &pixels,
            gtk::gdk_pixbuf::Colorspace::Rgb,
            true,
            8,
            width,
            height,
            width * 4,
        );
        let scale = f64::from(SIZE) / f64::from(width.max(height));
        let scaled = frame
            .scale_simple(
                ((f64::from(width) * scale).round() as i32).max(1),
                ((f64::from(height) * scale).round() as i32).max(1),
                InterpType::Bilinear,
            )
            .ok_or("Could not scale the frame")?;
        Ok((scaled, length))
    })();
    let _ = playbin.set_state(gst::State::Null);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_told_apart_by_their_extension() {
        assert_eq!(kind_of(Path::new("/x/IMG_2041.JPG")), FileKind::Image);
        assert_eq!(kind_of(Path::new("photo.heic")), FileKind::Image);
        assert_eq!(kind_of(Path::new("clip.mp4")), FileKind::Video);
        assert_eq!(kind_of(Path::new("clip.MOV")), FileKind::Video);
        assert_eq!(kind_of(Path::new("pass.pdf")), FileKind::Pdf);
        assert_eq!(kind_of(Path::new("notes.txt")), FileKind::Other);
        assert_eq!(kind_of(Path::new("README")), FileKind::Other);
    }

    #[test]
    fn a_thumbnail_is_made_again_only_when_the_file_changes() {
        let path = Path::new("/home/me/Downloads/a.jpg");
        let then = std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let key = cache_key(path, then, 1234);
        assert_eq!(key, cache_key(path, then, 1234), "stable for the same file");
        assert_eq!(key.len(), 32);
        assert_ne!(key, cache_key(path, then + Duration::from_secs(1), 1234));
        assert_ne!(key, cache_key(path, then, 1235));
        assert_ne!(key, cache_key(Path::new("/home/me/b.jpg"), then, 1234));
    }

    #[test]
    fn tests_keep_thumbnails_out_of_the_real_cache() {
        let dir = cache_dir().unwrap();
        assert!(dir.starts_with(std::env::temp_dir()), "{}", dir.display());
    }
}
