//! Photos and screenshots from a phone (W12), kept for a while under the state
//! folder (never Downloads): a phone that the person let sends each new image
//! as a v2 transfer whose offer says `gallery: true`; the Files page shows the
//! latest as "From your phone". Contract: `docs/protocol/stageD-protocol.md`.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use super::config::AppSettings;

/// One image at most.
pub(crate) const MAX_FILE_BYTES: u64 = 50 << 20;
/// What a phone may send in a rolling hour: files and bytes.
pub(crate) const HOURLY_FILES: usize = 20;
pub(crate) const HOURLY_BYTES: u64 = 200 << 20;
/// What is kept: this many bytes in all, this many days.
pub(crate) const KEEP_BYTES: u64 = 200 << 20;
pub(crate) const KEEP_SECONDS: u64 = 30 * 24 * 3600;
/// The strip on the Files page.
pub(crate) const STRIP_ITEMS: usize = 12;

const TYPES: [&str; 6] = [
    "image/jpeg",
    "image/png",
    "image/webp",
    "image/heic",
    "image/heif",
    "image/gif",
];

/// Why an offer is refused; the text is the wire `message`.
pub(crate) const OFF: &str = "gallery-off";
pub(crate) const LIMIT: &str = "gallery-limit";
pub(crate) const TYPE: &str = "gallery-type";

/// What each phone sent in the last hour: (seconds, bytes).
#[derive(Default)]
pub(crate) struct Ledger {
    sent: HashMap<String, VecDeque<(u64, u64)>>,
}

impl Ledger {
    /// Whether `device`'s offer of `bytes` of `mime` would be accepted at
    /// `now` (seconds). Nothing is counted: a busy refusal or a re-offer of a
    /// transfer already open costs the phone nothing ([`Self::commit`] does).
    pub(crate) fn check(
        &mut self,
        accepts: bool,
        device: &str,
        mime: Option<&str>,
        bytes: u64,
        now: u64,
    ) -> Result<(), &'static str> {
        if !accepts {
            return Err(OFF);
        }
        if !mime.is_some_and(|mime| TYPES.contains(&mime)) || bytes == 0 || bytes > MAX_FILE_BYTES {
            return Err(TYPE);
        }
        let hour = self.sent.entry(device.to_string()).or_default();
        while hour
            .front()
            .is_some_and(|(at, _)| now.saturating_sub(*at) >= 3600)
        {
            hour.pop_front();
        }
        let used: u64 = hour.iter().map(|(_, bytes)| bytes).sum();
        if hour.len() >= HOURLY_FILES || used + bytes > HOURLY_BYTES {
            return Err(LIMIT);
        }
        Ok(())
    }

    /// A new transfer was accepted: it counts against the hour.
    pub(crate) fn commit(&mut self, device: &str, bytes: u64, now: u64) {
        self.sent
            .entry(device.to_string())
            .or_default()
            .push_back((now, bytes));
    }

    #[cfg(test)]
    fn charged(&self, device: &str) -> usize {
        self.sent.get(device).map_or(0, VecDeque::len)
    }
}

static LEDGER: Mutex<Option<Ledger>> = Mutex::new(None);

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// The transfer manager asks before it accepts an offer flagged `gallery`.
/// Nothing is counted until [`commit_offer`].
pub(crate) fn check_offer(
    settings: &AppSettings,
    device: &str,
    mime: Option<&str>,
    bytes: u64,
) -> Result<(), &'static str> {
    let Ok(mut ledger) = LEDGER.lock() else {
        return Err(LIMIT);
    };
    ledger.get_or_insert_with(Ledger::default).check(
        settings.accepts_gallery_from(device),
        device,
        mime,
        bytes,
        unix_now(),
    )
}

/// A gallery transfer of `bytes` was accepted (new, not a re-offer).
pub(crate) fn commit_offer(device: &str, bytes: u64) {
    if let Ok(mut ledger) = LEDGER.lock() {
        ledger
            .get_or_insert_with(Ledger::default)
            .commit(device, bytes, unix_now());
    }
}

/// Tests: how many accepted transfers `device` has been charged for.
#[cfg(test)]
pub(crate) fn charged(device: &str) -> usize {
    LEDGER
        .lock()
        .ok()
        .and_then(|ledger| ledger.as_ref().map(|l| l.charged(device)))
        .unwrap_or(0)
}

/// Bumped on every change: the Files page redraws its strip when it moves.
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// `$XDG_STATE_HOME/desklink/gallery`; tests get a folder of their own.
pub(crate) fn root() -> Option<PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir().join(format!("desklink-test-gallery-{}", std::process::id())),
        );
    }
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("gallery"),
    )
}

/// One kept image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    pub(crate) device_id: String,
    pub(crate) path: PathBuf,
    /// The name as the phone sent it.
    pub(crate) name: String,
    pub(crate) size: u64,
    /// When it arrived, seconds.
    pub(crate) arrived: u64,
}

/// `text` with control characters and path separators replaced, cut to at
/// most `max_bytes` bytes on a character boundary (a file name is bytes).
fn safe(text: &str, max_bytes: usize) -> String {
    let mut out = String::new();
    for c in text.chars() {
        let c = if c.is_control() || matches!(c, '/' | '\\' | ':') {
            '_'
        } else {
            c
        };
        if out.len() + c.len_utf8() > max_bytes {
            break;
        }
        out.push(c);
    }
    out
}

/// The extension of the image type `path` really is, from its first bytes:
/// whatever name or type the sender gave, a file that is not one of these is
/// not kept.
pub(crate) fn sniff(path: &Path) -> Option<&'static str> {
    use std::io::Read;
    let mut head = [0u8; 32];
    let n = std::fs::File::open(path).ok()?.read(&mut head).ok()?;
    let head = &head[..n];
    if head.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("jpg")
    } else if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some("gif")
    } else if head.len() >= 12 && &head[..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some("webp")
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" {
        match &head[8..12] {
            b"heic" | b"heix" | b"hevc" | b"hevx" => Some("heic"),
            b"mif1" | b"msf1" | b"heif" => Some("heif"),
            _ => None,
        }
    } else {
        None
    }
}

fn device_dir(root: &Path, device: &str) -> PathBuf {
    root.join(
        device
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .take(80)
            .collect::<String>(),
    )
}

/// `<seconds>-<name>`: sorted by arrival by its name alone.
fn parse(name: &str) -> Option<(u64, &str)> {
    let (seconds, rest) = name.split_once('-')?;
    Some((seconds.parse().ok()?, rest))
}

/// What is kept for `device` under `root`, newest first.
pub(crate) fn list_in(root: &Path, device: &str) -> Vec<Item> {
    let dir = device_dir(root, device);
    let mut items: Vec<Item> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let (arrived, name) = parse(&file_name)?;
            let meta = entry.metadata().ok().filter(|meta| meta.is_file())?;
            Some(Item {
                device_id: device.to_string(),
                path: entry.path(),
                name: name.to_string(),
                size: meta.len(),
                arrived,
            })
        })
        .collect();
    items.sort_by(|a, b| b.arrived.cmp(&a.arrived).then_with(|| b.name.cmp(&a.name)));
    items
}

pub(crate) fn list(device: &str) -> Vec<Item> {
    root()
        .map(|root| list_in(&root, device))
        .unwrap_or_default()
}

/// Moves the verified file at `part` in as `device`'s newest image, and trims.
pub(crate) fn store_in(
    root: &Path,
    device: &str,
    filename: &str,
    part: &Path,
    now: u64,
) -> Result<PathBuf, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    let dir = device_dir(root, device);
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)
        .map_err(|error| format!("Could not make the photos folder: {error}"))?;
    // Kept under the type it really is, never the extension the sender chose
    // (Open and Save would otherwise carry a peer's ".html" or ".desktop").
    let ext = sniff(part).ok_or_else(|| "That file is not a picture".to_string())?;
    let base = filename.rsplit('/').next().unwrap_or(filename);
    let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
    let mut stem = safe(stem, 100);
    if stem.is_empty() {
        stem = "image".to_string();
    }
    let mut target = dir.join(format!("{now}-{stem}.{ext}"));
    let mut n = 1;
    while target.exists() {
        n += 1;
        target = dir.join(format!("{now}-{n}-{stem}.{ext}"));
    }
    if std::fs::rename(part, &target).is_err() {
        std::fs::copy(part, &target)
            .and_then(|_| std::fs::remove_file(part))
            .map_err(|error| format!("Could not keep the photo: {error}"))?;
    }
    let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600));
    trim_in(root, now, KEEP_BYTES, KEEP_SECONDS);
    GENERATION.fetch_add(1, Ordering::Relaxed);
    Ok(target)
}

/// Drops what is older than `keep_seconds`, then the oldest until the rest
/// fit `keep_bytes` (all phones together).
pub(crate) fn trim_in(root: &Path, now: u64, keep_bytes: u64, keep_seconds: u64) {
    let mut all: Vec<Item> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|dir| {
            let id = dir.file_name().to_string_lossy().into_owned();
            list_in(root, &id)
        })
        .collect();
    all.retain(|item| {
        let old = now.saturating_sub(item.arrived) > keep_seconds;
        if old {
            let _ = std::fs::remove_file(&item.path);
        }
        !old
    });
    all.sort_by_key(|item| std::cmp::Reverse(item.arrived));
    let mut used = 0;
    for item in all {
        used += item.size;
        if used > keep_bytes {
            let _ = std::fs::remove_file(&item.path);
        }
    }
}

/// Trims what is kept (at start): the age and size limits.
pub(crate) fn trim() {
    if let Some(root) = root() {
        trim_in(&root, unix_now(), KEEP_BYTES, KEEP_SECONDS);
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// "Remove": one kept image goes. Only a file under the gallery folder.
pub(crate) fn remove_in(root: &Path, path: &Path) -> bool {
    use std::path::Component;
    // Strictly `root/<phone>/<file>`, by components: no `..`, no `.`, nothing
    // that only looks like it from outside.
    let inside = path.parent().and_then(Path::parent) == Some(root)
        && path.starts_with(root)
        && path
            .components()
            .all(|c| !matches!(c, Component::ParentDir | Component::CurDir));
    if inside && std::fs::remove_file(path).is_ok() {
        GENERATION.fetch_add(1, Ordering::Relaxed);
        return true;
    }
    false
}

pub(crate) fn remove(path: &Path) -> bool {
    root().is_some_and(|root| remove_in(&root, path))
}

/// An unpaired phone's images and counts go with it.
pub(crate) fn forget(device: &str) {
    if let Ok(mut ledger) = LEDGER.lock() {
        if let Some(ledger) = ledger.as_mut() {
            ledger.sent.remove(device);
        }
    }
    if let Some(root) = root() {
        let _ = std::fs::remove_dir_all(device_dir(&root, device));
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
}

/// An image that has just arrived, for the toast.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Arrived {
    pub(crate) device_id: String,
    pub(crate) path: PathBuf,
}

static ARRIVED: Mutex<Vec<Arrived>> = Mutex::new(Vec::new());

pub(crate) fn announce(device_id: &str, path: &Path) {
    if let Ok(mut arrived) = ARRIVED.lock() {
        arrived.push(Arrived {
            device_id: device_id.to_string(),
            path: path.to_path_buf(),
        });
    }
}

/// The images that arrived since the last call; each is returned once.
pub(crate) fn take_arrived() -> Vec<Arrived> {
    ARRIVED
        .lock()
        .map(|mut arrived| std::mem::take(&mut *arrived))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("desklink-gallery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A file that starts like a PNG, `size` bytes long.
    fn file(dir: &Path, name: &str, size: usize) -> PathBuf {
        let path = dir.join(name);
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.resize(size.max(8), 7);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn an_offer_needs_the_switch_an_image_type_and_a_size() {
        let mut ledger = Ledger::default();
        let ok =
            |ledger: &mut Ledger, on, mime, bytes, now| ledger.check(on, "p", mime, bytes, now);
        assert_eq!(ok(&mut ledger, false, Some("image/png"), 10, 0), Err(OFF));
        assert_eq!(
            ok(&mut ledger, true, Some("application/pdf"), 10, 0),
            Err(TYPE)
        );
        assert_eq!(ok(&mut ledger, true, None, 10, 0), Err(TYPE));
        assert_eq!(ok(&mut ledger, true, Some("image/png"), 0, 0), Err(TYPE));
        assert_eq!(
            ok(&mut ledger, true, Some("image/jpeg"), MAX_FILE_BYTES + 1, 0),
            Err(TYPE)
        );
        assert_eq!(ok(&mut ledger, true, Some("image/heic"), 10, 0), Ok(()));
    }

    /// 20 files or 200 MB an hour per phone, on a rolling hour (fake clock);
    /// only accepted transfers are charged, so a busy refusal and its re-offers
    /// cost nothing.
    #[test]
    fn a_phone_may_send_twenty_files_or_two_hundred_megabytes_an_hour() {
        let mut ledger = Ledger::default();
        let png = Some("image/png");
        // A busy engine: the phone offers again and again; nothing is charged.
        for n in 0..60 {
            assert_eq!(ledger.check(true, "p", png, 1000, n), Ok(()));
        }
        assert_eq!(ledger.charged("p"), 0);
        for n in 0..HOURLY_FILES as u64 {
            assert_eq!(ledger.check(true, "p", png, 1000, n), Ok(()));
            ledger.commit("p", 1000, n);
        }
        assert_eq!(ledger.check(true, "p", png, 1000, 100), Err(LIMIT));
        // Another phone has its own count.
        assert_eq!(ledger.check(true, "q", png, 1000, 100), Ok(()));
        // The first leaves the hour: room for one.
        assert_eq!(ledger.check(true, "p", png, 1000, 3600), Ok(()));
        ledger.commit("p", 1000, 3600);
        assert_eq!(ledger.check(true, "p", png, 1000, 3600), Err(LIMIT));
        // Bytes: four of 50 MB fit, the fifth does not.
        let mut big = Ledger::default();
        for n in 0..4 {
            assert_eq!(big.check(true, "p", png, MAX_FILE_BYTES, n), Ok(()));
            big.commit("p", MAX_FILE_BYTES, n);
        }
        assert_eq!(big.check(true, "p", png, MAX_FILE_BYTES, 5), Err(LIMIT));
        assert_eq!(
            big.check(true, "p", png, 1, 4000),
            Ok(()),
            "the hour passed"
        );
    }

    #[test]
    fn a_stored_image_is_listed_newest_first_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let (root, src) = (scratch(), scratch());
        let a = store_in(&root, "ph/one", "../a b.png", &file(&src, "p1", 10), 1000).unwrap();
        let b = store_in(&root, "ph/one", "b.png", &file(&src, "p2", 20), 2000).unwrap();
        let same_second = store_in(&root, "ph/one", "b.png", &file(&src, "p3", 5), 2000).unwrap();
        assert_ne!(b, same_second, "no overwrite");
        let names: Vec<_> = list_in(&root, "ph/one")
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names.len(), 3);
        assert_eq!(names[2], "a b.png", "a path in the name is only a name");
        // The type is the file's own: a ".html" name on a PNG is kept as .png,
        // and a file that is not a picture is not kept at all.
        let named = store_in(&root, "ph/one", "evil.html", &file(&src, "p4", 9), 3000).unwrap();
        assert_eq!(named.extension().unwrap(), "png");
        let text = src.join("text");
        std::fs::write(&text, b"<script>alert(1)</script>").unwrap();
        assert!(store_in(&root, "ph/one", "x.png", &text, 3001).is_err());
        // A long name of wide characters is cut by bytes, so it can be written.
        let wide = "\u{1F4F7}".repeat(100);
        let kept = store_in(
            &root,
            "ph/one",
            &format!("{wide}.png"),
            &file(&src, "p5", 9),
            3002,
        )
        .unwrap();
        assert!(kept.file_name().unwrap().len() <= 255);
        assert!(a.starts_with(&root) && !a.to_string_lossy().contains(".."));
        assert_eq!(
            std::fs::metadata(&a).unwrap().permissions().mode() & 0o077,
            0
        );
        assert_eq!(
            std::fs::metadata(a.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o077,
            0
        );
        assert!(list_in(&root, "other").is_empty());
    }

    /// 200 MB / 30 days, oldest first, all phones together (fake clock).
    #[test]
    fn what_is_kept_is_trimmed_by_age_then_by_size() {
        let (root, src) = (scratch(), scratch());
        let day = 24 * 3600;
        let now = 100 * day;
        store_in(&root, "p", "old.png", &file(&src, "o", 10), now - 31 * day).unwrap();
        store_in(&root, "p", "mid.png", &file(&src, "m", 60), now - 10 * day).unwrap();
        store_in(&root, "q", "mid2.png", &file(&src, "m2", 60), now - 5 * day).unwrap();
        store_in(&root, "p", "new.png", &file(&src, "n", 60), now - 1).unwrap();
        // The store trimmed at 31 days when the newer ones arrived? Not yet
        // here (their `now` was in the past); trim explicitly at `now`.
        trim_in(&root, now, 130, 30 * day);
        let names = |id| {
            list_in(&root, id)
                .into_iter()
                .map(|i| i.name)
                .collect::<Vec<_>>()
        };
        assert_eq!(names("p"), ["new.png"], "old by age, mid by size");
        assert_eq!(names("q"), ["mid2.png"]);
        trim_in(&root, now, 1000, 30 * day);
        assert_eq!(names("p"), ["new.png"], "nothing more goes");
    }

    #[test]
    fn remove_only_takes_a_file_under_the_gallery() {
        let (root, src) = (scratch(), scratch());
        let kept = store_in(&root, "p", "a.png", &file(&src, "a", 3), 5).unwrap();
        let outside = file(&src, "important.txt", 3);
        assert!(!remove_in(&root, &outside), "not ours");
        assert!(outside.exists());
        assert!(
            !remove_in(&root, &root.join("p").join("..").join("..")),
            "no way out"
        );
        assert!(remove_in(&root, &kept));
        assert!(list_in(&root, "p").is_empty());
    }
}
