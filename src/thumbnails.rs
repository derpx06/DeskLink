//! Previews of transferred files for the Transfers page: what kind of file
//! each is, and a small picture of it — an image scaled down, or a video's
//! frame at about one second with its length.
//!
//! Everything that touches the files — checking they are still there,
//! decoding, GStreamer — runs on one dedicated worker thread ([`request`],
//! [`check_presence`]); replies come back on the main thread. Results are kept
//! as PNGs under
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

/// How long one decode may take before the worker gives up on it and moves
/// on. A file on a stalled network mount must not hold up every other card.
const DECODE_TIMEOUT: Duration = Duration::from_secs(15);

/// What the worker hands back to the main thread.
pub(crate) enum Reply {
    /// A preview for the file with cache key `key`, and a video's length.
    Thumbnail {
        key: String,
        texture: gdk::Texture,
        duration: Option<Duration>,
    },
    /// No preview can be made (unreadable, unsupported, too slow): show the
    /// typed icon. Remembered on disk, so it is not tried again.
    Failed { key: String },
    /// Which of a list's files are still there, and their cache keys.
    Presence {
        generation: u64,
        files: Vec<Presence>,
    },
}

/// One file of a presence check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Presence {
    pub(crate) exists: bool,
    /// The cache key, for a file that is there.
    pub(crate) key: Option<String>,
}

enum Job {
    Thumbnail {
        key: String,
        path: PathBuf,
        kind: FileKind,
    },
    Presence {
        generation: u64,
        paths: Vec<PathBuf>,
    },
}

/// Where replies go: a function run on the main thread.
static DELIVER: std::sync::OnceLock<fn(Reply)> = std::sync::OnceLock::new();

/// Sets the main-thread function replies are given to. Called once.
pub(crate) fn set_delivery(deliver: fn(Reply)) {
    let _ = DELIVER.set(deliver);
}

fn deliver(reply: Reply) {
    if let Some(deliver) = DELIVER.get().copied() {
        glib::MainContext::default().invoke(move || deliver(reply));
    }
}

/// The one worker thread every file check and decode runs on, so none of
/// them occupies GLib's shared pool (where the viewer's decodes run).
fn worker() -> &'static std::sync::Mutex<std::sync::mpsc::Sender<Job>> {
    static WORKER: std::sync::OnceLock<std::sync::Mutex<std::sync::mpsc::Sender<Job>>> =
        std::sync::OnceLock::new();
    WORKER.get_or_init(|| {
        let (sender, jobs) = std::sync::mpsc::channel::<Job>();
        let _ = std::thread::Builder::new()
            .name("desklink-thumbnails".into())
            .spawn(move || {
                for job in jobs {
                    match job {
                        Job::Thumbnail { key, path, kind } => deliver(make(key, &path, kind)),
                        Job::Presence { generation, paths } => deliver(Reply::Presence {
                            generation,
                            files: paths.iter().map(|path| presence(path)).collect(),
                        }),
                    }
                }
            });
        std::sync::Mutex::new(sender)
    })
}

fn send(job: Job) {
    if let Ok(sender) = worker().lock() {
        let _ = sender.send(job);
    }
}

/// Asks for `path`'s preview; the reply comes to the delivery function.
pub(crate) fn request(key: &str, path: &Path, kind: FileKind) {
    send(Job::Thumbnail {
        key: key.to_string(),
        path: path.to_path_buf(),
        kind,
    });
}

/// Asks which of `paths` are still there (off the main thread: a stat on a
/// network mount can hang).
pub(crate) fn check_presence(generation: u64, paths: Vec<PathBuf>) {
    send(Job::Presence { generation, paths });
}

/// Whether `path` is on a FUSE mount (gvfs, a phone over MTP, sshfs):
/// reading those can stall for as long as the far end likes.
fn on_fuse(path: &Path) -> bool {
    const FUSE_SUPER_MAGIC: i64 = 0x6573_5546;
    let Ok(name) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return false;
    };
    // SAFETY: `name` is a valid NUL-terminated path and `stats` a zeroed
    // struct statfs that the call fills in.
    unsafe {
        let mut stats: libc::statfs = std::mem::zeroed();
        libc::statfs(name.as_ptr(), &mut stats) == 0
            && i64::from(stats.f_type as u32 as i32) == FUSE_SUPER_MAGIC
    }
}

fn presence(path: &Path) -> Presence {
    // ponytail: a gvfs path counts as present without a stat; a probe with
    // its own watchdog if those turn out common.
    if path.starts_with("/run/user") && path.components().any(|part| part.as_os_str() == "gvfs") {
        return Presence {
            exists: true,
            key: None,
        };
    }
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Presence {
            exists: true,
            key: meta
                .modified()
                .ok()
                .map(|modified| cache_key(path, modified, meta.len())),
        },
        _ => Presence {
            exists: false,
            key: None,
        },
    }
}

/// The preview for `key`, from the cache or made now with a watchdog.
fn make(key: String, path: &Path, kind: FileKind) -> Reply {
    let Some(dir) = cache_dir() else {
        return Reply::Failed { key };
    };
    let (png, secs, none) = (
        dir.join(format!("{key}.png")),
        dir.join(format!("{key}.secs")),
        dir.join(format!("{key}.none")),
    );
    // Checked when the job's turn comes: an earlier job may have made it.
    if let Ok(pixbuf) = Pixbuf::from_file(&png) {
        let duration = std::fs::read_to_string(&secs)
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .map(Duration::from_secs);
        return Reply::Thumbnail {
            key,
            texture: texture(&pixbuf),
            duration,
        };
    }
    if none.exists()
        || on_fuse(path)
        || !matches!(kind, FileKind::Image | FileKind::Video | FileKind::Pdf)
    {
        return Reply::Failed { key };
    }
    let (done, made) = std::sync::mpsc::channel();
    let source = path.to_path_buf();
    // ponytail: a decode that never returns leaks its thread; it is given up
    // on after DECODE_TIMEOUT so later cards still get theirs.
    let spawned = std::thread::Builder::new()
        .name("desklink-decode".into())
        .spawn(move || {
            let result = match kind {
                FileKind::Image => image(&source).map(|pixbuf| (pixbuf, None)),
                FileKind::Pdf => pdf_page(&source).map(|pixbuf| (page_top(&pixbuf), None)),
                _ => video_frame(&source),
            }
            .map(|(pixbuf, length)| {
                (
                    pixbuf.read_pixel_bytes(),
                    pixbuf.width(),
                    pixbuf.height(),
                    pixbuf.rowstride(),
                    pixbuf.has_alpha(),
                    length,
                )
            });
            let _ = done.send(result);
        });
    let result = match spawned {
        Ok(_) => made
            .recv_timeout(DECODE_TIMEOUT)
            .unwrap_or_else(|_| Err("The preview took too long".into())),
        Err(error) => Err(error.to_string()),
    };
    let _ = crate::device_links::private_fs::create_dir_all(&dir);
    match result {
        Ok((bytes, width, height, stride, alpha, length)) => {
            let pixbuf = Pixbuf::from_bytes(
                &bytes,
                gtk::gdk_pixbuf::Colorspace::Rgb,
                alpha,
                8,
                width,
                height,
                stride,
            );
            if let Err(error) = pixbuf.savev(&png, "png", &[]) {
                eprintln!("[UI] Could not cache a thumbnail: {error}");
            }
            if let Some(length) = length {
                let _ = std::fs::write(&secs, length.as_secs().to_string());
            }
            Reply::Thumbnail {
                key,
                texture: texture(&pixbuf),
                duration: length,
            }
        }
        Err(error) => {
            debug_log!("[UI] No preview for {}: {error}", path.display());
            let _ = std::fs::write(&none, error);
            Reply::Failed { key }
        }
    }
}

/// Drops cached previews unused for the retention period. Run once at
/// startup, beside the transfer log's own prune.
pub(crate) fn prune() {
    let Some(dir) = cache_dir() else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("desklink-thumbnail-prune".into())
        .spawn(move || prune_dir(&dir, std::time::SystemTime::now()));
}

/// The thumbnail cache's disk quota: past it, the oldest go first.
const MAX_CACHE_BYTES: u64 = 64 * 1024 * 1024;

fn prune_dir(dir: &Path, now: std::time::SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let retention = crate::device_links::connection_log::RETENTION;
    let mut kept = Vec::new();
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        let old = now
            .duration_since(modified)
            .is_ok_and(|age| age > retention);
        if old {
            let _ = std::fs::remove_file(entry.path());
        } else {
            kept.push((modified, meta.len(), entry.path()));
        }
    }
    // Then the quota, oldest first.
    kept.sort();
    let mut total: u64 = kept.iter().map(|(_, size, _)| size).sum();
    for (_, size, path) in kept {
        if total <= MAX_CACHE_BYTES {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
        }
    }
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

/// The shared freedesktop thumbnail cache (`$XDG_CACHE_HOME/thumbnails`),
/// which Files and every other previewer read and write.
fn freedesktop_dir() -> Option<PathBuf> {
    // Tests must never write the person's cache.
    if cfg!(test) {
        return Some(std::env::temp_dir().join(format!(
            "desklink-test-xdg-thumbnails-{}",
            std::process::id()
        )));
    }
    Some(dirs::cache_dir()?.join("thumbnails"))
}

/// A file's modification time in whole seconds, as `Thumb::MTime` holds it.
fn mtime_secs(path: &Path) -> Option<String> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    let secs = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(secs.to_string())
}

/// A PDF's first page: the freedesktop thumbnail another app already made
/// for this version of the file, else drawn now with poppler (and written
/// there for the others too). `Err` with neither: the card keeps its icon.
fn pdf_page(path: &Path) -> Result<Pixbuf, String> {
    let mtime = mtime_secs(path).ok_or("The file has no modification time")?;
    let cache = freedesktop_dir().ok_or("No cache directory")?;
    for size in ["large", "normal"] {
        let Some(file) = crate::pdf::freedesktop_thumbnail(&cache, size, path) else {
            continue;
        };
        if let Ok(pixbuf) = Pixbuf::from_file(&file) {
            if pixbuf.option("tEXt::Thumb::MTime").as_deref() == Some(mtime.as_str()) {
                return Ok(pixbuf);
            }
        }
    }
    let document = crate::pdf::Document::open(path)?;
    let &(width, height) = document.sizes().first().ok_or("The PDF has no pages")?;
    // The freedesktop "large" size: 256 px on the long side.
    let pixels = if width >= height {
        256
    } else {
        (256.0 * width / height).round() as i32
    };
    let page = document.render(0, pixels)?;
    let pixbuf = Pixbuf::from_bytes(
        &glib::Bytes::from_owned(page.rgba()),
        gtk::gdk_pixbuf::Colorspace::Rgb,
        true,
        8,
        page.width,
        page.height,
        page.width * 4,
    );
    write_freedesktop(&cache, path, &mtime, &pixbuf);
    Ok(pixbuf)
}

/// The top of a page, in a card's wide shape: a document is recognised by
/// its heading, not by a middle slice of it.
fn page_top(page: &Pixbuf) -> Pixbuf {
    let height = ((f64::from(page.width()) / 1.5).round() as i32).clamp(1, page.height());
    page.new_subpixbuf(0, 0, page.width(), height)
        .copy()
        .unwrap_or_else(|| page.clone())
}

/// Saves `pixbuf` as `path`'s "large" freedesktop thumbnail: written aside
/// and renamed into place (a reader never sees half a file), 0600 in a
/// 0700 directory, with the URI and modification time it was made from.
fn write_freedesktop(cache: &Path, path: &Path, mtime: &str, pixbuf: &Pixbuf) {
    use std::os::unix::fs::PermissionsExt;
    let (Some(target), Ok(uri)) = (
        crate::pdf::freedesktop_thumbnail(cache, "large", path),
        glib::filename_to_uri(path, None),
    ) else {
        return;
    };
    let Some(dir) = target.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let _ = std::fs::set_permissions(cache, std::fs::Permissions::from_mode(0o700));
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    let aside = target.with_extension(format!("desklink-{}", std::process::id()));
    let saved = pixbuf.savev(
        &aside,
        "png",
        &[
            ("tEXt::Thumb::URI", uri.as_str()),
            ("tEXt::Thumb::MTime", mtime),
            ("tEXt::Software", "DeskLink"),
        ],
    );
    if saved.is_ok() {
        let _ = std::fs::set_permissions(&aside, std::fs::Permissions::from_mode(0o600));
        if std::fs::rename(&aside, &target).is_ok() {
            return;
        }
    }
    let _ = std::fs::remove_file(&aside);
}

/// An image scaled to fit [`SIZE`], the right way up.
fn image(path: &Path) -> Result<Pixbuf, String> {
    // The header says the size: a decompression bomb is refused, not decoded.
    if !crate::image_limits::file_within_bounds(path, crate::image_limits::MAX_DECODE_PIXELS) {
        return Err("The picture is too large to preview".to_string());
    }
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
    // Phone videos are often stored sideways with a rotation tag.
    if let Ok(flip) = gst::ElementFactory::make("videoflip").build() {
        flip.set_property_from_str("video-direction", "auto");
        playbin.set_property("video-filter", &flip);
    }
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
    fn previews_unused_for_thirty_days_are_pruned() {
        let dir = std::env::temp_dir().join(format!("desklink-prune-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("abc.png");
        std::fs::write(&file, b"x").unwrap();
        let now = std::time::SystemTime::now();
        prune_dir(&dir, now);
        assert!(file.exists(), "a fresh preview stays");
        prune_dir(
            &dir,
            now + crate::device_links::connection_log::RETENTION + Duration::from_secs(60),
        );
        assert!(!file.exists(), "an old one goes");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_cache_keeps_to_its_quota_oldest_first() {
        let dir = std::env::temp_dir().join(format!("desklink-quota-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let now = std::time::SystemTime::now();
        for (index, name) in ["old.png", "mid.png", "new.png"].iter().enumerate() {
            // Sparse: the logical size counts, no disk is used.
            let file = std::fs::File::create(dir.join(name)).unwrap();
            file.set_len(MAX_CACHE_BYTES / 2 - 1).unwrap();
            file.set_modified(now - Duration::from_secs(300 - index as u64 * 100))
                .unwrap();
        }
        prune_dir(&dir, now);
        assert!(!dir.join("old.png").exists(), "the oldest went");
        assert!(dir.join("mid.png").exists() && dir.join("new.png").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_file_that_is_gone_is_reported_missing() {
        let gone = presence(Path::new("/nonexistent/desklink/photo.jpg"));
        assert_eq!(
            gone,
            Presence {
                exists: false,
                key: None
            }
        );
        let here = std::env::temp_dir().join(format!("desklink-here-{}", uuid::Uuid::new_v4()));
        std::fs::write(&here, b"x").unwrap();
        let found = presence(&here);
        assert!(found.exists && found.key.is_some());
        let _ = std::fs::remove_file(here);
    }

    /// A PDF's preview comes from the shared freedesktop cache when it was
    /// made for this version of the file (even without poppler), and not
    /// when the file changed since.
    #[test]
    fn a_pdf_uses_the_shared_thumbnail_made_for_its_version() {
        let dir = std::env::temp_dir().join(format!("desklink-pdf-thumb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("scan.pdf");
        std::fs::write(&path, b"not really a PDF").unwrap();
        let mtime = mtime_secs(&path).unwrap();
        let cache = freedesktop_dir().unwrap();
        let shared = crate::pdf::freedesktop_thumbnail(&cache, "large", &path).unwrap();
        assert!(
            shared.starts_with(std::env::temp_dir()),
            "never the real cache"
        );
        let picture = Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, true, 8, 20, 30).unwrap();
        write_freedesktop(&cache, &path, &mtime, &picture);
        assert!(shared.exists());
        let found = pdf_page(&path).expect("the shared thumbnail is used");
        assert_eq!((found.width(), found.height()), (20, 30));
        // Made for another version of the file: not used (and this one
        // cannot be drawn, it is not a PDF).
        write_freedesktop(&cache, &path, "1", &picture);
        assert!(pdf_page(&path).is_err());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&shared);
    }

    /// With poppler here, a PDF's first page is drawn and shared.
    #[test]
    fn a_pdfs_first_page_is_drawn_and_shared_when_poppler_is_here() {
        if !crate::pdf::available() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("desklink-pdf-draw-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wide.pdf");
        std::fs::write(&path, crate::pdf::MINIMAL_PDF).unwrap();
        let page = pdf_page(&path).unwrap();
        assert_eq!(
            (page.width(), page.height()),
            (256, 128),
            "large: 256 on the long side"
        );
        let shared =
            crate::pdf::freedesktop_thumbnail(&freedesktop_dir().unwrap(), "large", &path).unwrap();
        let saved = Pixbuf::from_file(&shared).unwrap();
        assert_eq!(
            saved.option("tEXt::Thumb::MTime").as_deref(),
            mtime_secs(&path).as_deref()
        );
        assert!(saved
            .option("tEXt::Thumb::URI")
            .is_some_and(|uri| uri.ends_with("wide.pdf")));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(&shared);
    }

    #[test]
    fn tests_keep_thumbnails_out_of_the_real_cache() {
        let dir = cache_dir().unwrap();
        assert!(dir.starts_with(std::env::temp_dir()), "{}", dir.display());
    }
}
