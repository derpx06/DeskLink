//! File operations under a pair's root: safe target paths, hashing, mtimes,
//! the case folding of names, putting a received file in place, the trash,
//! Bucket folders and GTK bookmarks.
//!
//! Sibling edges: `model`. Outside: `file_protocol::validate_relpath`, gio.

use super::*;

pub(super) fn sha256_of(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = openssl::sha::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer).ok()?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Some(
        hasher
            .finish()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    )
}

/// `root`/`relpath`, creating the directories on the way; refuses any that
/// is not a real directory (a symlink could lead outside the root).
pub(super) fn target_path(root: &Path, relpath: &str) -> Result<PathBuf, String> {
    validate_relpath(relpath)?;
    let mut path = root.to_path_buf();
    let segments: Vec<&str> = relpath.split('/').collect();
    for segment in &segments[..segments.len() - 1] {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err("A folder on the way is not a folder".into()),
            Err(_) => fs::create_dir(&path).map_err(|_| "Could not create a folder".to_string())?,
        }
    }
    path.push(segments[segments.len() - 1]);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err("Something that is not a file is in the way".into());
    }
    Ok(path)
}

/// [`target_path`] for reading, deleting and moving away: the same checks,
/// but nothing is created (a path whose folders are missing is just absent).
pub(super) fn read_path(root: &Path, relpath: &str) -> Result<PathBuf, String> {
    validate_relpath(relpath)?;
    let mut path = root.to_path_buf();
    let segments: Vec<&str> = relpath.split('/').collect();
    for segment in &segments[..segments.len() - 1] {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err("A folder on the way is not a folder".into()),
            Err(_) => {}
        }
    }
    path.push(segments[segments.len() - 1]);
    if fs::symlink_metadata(&path).is_ok_and(|metadata| !metadata.is_file()) {
        return Err("Something that is not a file is in the way".into());
    }
    Ok(path)
}

/// `root`/`relpath` as a folder, made with every folder on the way.
pub(super) fn make_dir(root: &Path, relpath: &str) -> Result<PathBuf, String> {
    if relpath.is_empty() {
        return Ok(root.to_path_buf());
    }
    validate_relpath(relpath)?;
    let mut path = root.to_path_buf();
    for segment in relpath.split('/') {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err("Something that is not a folder is in the way".into()),
            Err(_) => fs::create_dir(&path).map_err(|_| "Could not create a folder".to_string())?,
        }
    }
    Ok(path)
}

/// Whether the local file still matches what the manifest says (absent
/// when the manifest has no live entry).
pub(super) fn unchanged(path: &Path, entry: Option<&Entry>) -> bool {
    match (
        fs::symlink_metadata(path),
        entry.filter(|entry| !entry.deleted),
    ) {
        (Err(_), None) => true,
        (Ok(metadata), Some(entry)) if entry.dir => metadata.is_dir(),
        (Ok(metadata), Some(entry)) => {
            metadata.is_file() && metadata.len() == entry.size && mtime_ms(&metadata) == entry.mtime
        }
        _ => false,
    }
}

/// F8b: where a phone's Bucket goes: `~/DeskLink Bucket` for the first,
/// `~/DeskLink Bucket (<phone>)` for each further one — siblings in `home`,
/// never inside another folder that syncs.
pub(super) fn bucket_root(home: &Path, phone: &str, taken: &[PathBuf]) -> PathBuf {
    let free = |path: &Path| {
        !taken
            .iter()
            .any(|root| root.starts_with(path) || path.starts_with(root))
    };
    let first = home.join("DeskLink Bucket");
    if free(&first) {
        return first;
    }
    let phone: String = phone
        .chars()
        .map(|c| {
            if matches!(c, '/' | '\\' | '\0') || c.is_control() {
                ' '
            } else {
                c
            }
        })
        .take(40)
        .collect();
    let phone = phone.trim();
    let phone = if phone.is_empty() { "phone" } else { phone };
    (1..100)
        .map(|number| match number {
            1 => home.join(format!("DeskLink Bucket ({phone})")),
            n => home.join(format!("DeskLink Bucket ({phone} {n})")),
        })
        .find(|path| free(path))
        .unwrap_or_else(|| home.join(format!("DeskLink Bucket {}", uuid::Uuid::new_v4().simple())))
}

/// `name` in `dir`, or `name (2)`, `name (3)` … before the extension when
/// it is taken.
pub(super) fn unique_in(dir: &Path, name: &str) -> PathBuf {
    unique_among(dir, name, &HashSet::new())
}

/// [`unique_in`], also avoiding names in `taken` (picked, not yet written).
pub(super) fn unique_among(dir: &Path, name: &str, taken: &HashSet<PathBuf>) -> PathBuf {
    // The phone's file system may not tell "Photo.jpg" from "photo.jpg"
    // (and neither does a case-folding one here): the same rule as the case
    // clashes of a sync, so a name that differs only by case is taken.
    let lower = |path: &Path| fold_name(&path.to_string_lossy());
    let existing: HashSet<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| lower(&entry.path()))
                .collect()
        })
        .unwrap_or_default();
    let taken_lower: HashSet<String> = taken.iter().map(|path| lower(path)).collect();
    let free = |path: &Path| {
        fs::symlink_metadata(path).is_err()
            && !taken.contains(path)
            && !existing.contains(&lower(path))
            && !taken_lower.contains(&lower(path))
    };
    let first = dir.join(name);
    if free(&first) {
        return first;
    }
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 16 => {
            (stem.to_string(), format!(".{extension}"))
        }
        _ => (name.to_string(), String::new()),
    };
    (2..10_000)
        .map(|number| dir.join(format!("{stem} ({number}){extension}")))
        .find(|path| free(path))
        .unwrap_or_else(|| {
            dir.join(format!(
                "{stem} {}{extension}",
                uuid::Uuid::new_v4().simple()
            ))
        })
}

/// The user's home, where Buckets go (tests: a scratch folder).
pub(super) fn home() -> PathBuf {
    #[cfg(test)]
    {
        TEST_HOME
            .with(|home| home.borrow().clone())
            .unwrap_or_else(std::env::temp_dir)
    }
    #[cfg(not(test))]
    {
        gtk::glib::home_dir()
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// GTK's bookmarks file (Files' sidebar on any GTK file manager).
fn bookmarks_file() -> Option<PathBuf> {
    #[cfg(test)]
    {
        TEST_BOOKMARKS.with(|path| path.borrow().clone())
    }
    #[cfg(not(test))]
    {
        Some(
            gtk::glib::user_config_dir()
                .join("gtk-3.0")
                .join("bookmarks"),
        )
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_BOOKMARKS: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Adds (or removes) `folder` from the Files sidebar. Best effort; written
/// atomically, other lines untouched.
pub(super) fn bookmark(folder: &Path, add: bool) {
    let Some(file) = bookmarks_file() else {
        return;
    };
    let uri = gtk::prelude::FileExt::uri(&gtk::gio::File::for_path(folder)).to_string();
    let label = folder
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("DeskLink Bucket");
    let old = fs::read_to_string(&file).unwrap_or_default();
    let ours = |line: &str| line == uri || line.starts_with(&format!("{uri} "));
    let mut lines: Vec<String> = old
        .lines()
        .filter(|line| !ours(line))
        .map(str::to_string)
        .collect();
    if add {
        lines.push(format!("{uri} {label}"));
    }
    let mut new = lines.join("\n");
    if !new.is_empty() {
        new.push('\n');
    }
    if new == old {
        return;
    }
    if let Some(dir) = file.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let temporary = file.with_extension("desklink-tmp");
    if fs::write(&temporary, new).is_ok() && fs::rename(&temporary, &file).is_err() {
        let _ = fs::remove_file(&temporary);
    }
}

/// Moves `from` into place at `to` atomically: a rename, or a copy to a
/// temporary name beside it and a rename.
pub(super) fn put_in_place(from: &Path, to: &Path, mtime: i64) -> Result<(), String> {
    let temporary = to.with_file_name(format!(".desklink-{}.tmp", uuid::Uuid::new_v4().simple()));
    if fs::rename(from, &temporary).is_err() {
        fs::copy(from, &temporary).map_err(|_| "Could not write the file".to_string())?;
        let _ = fs::remove_file(from);
    }
    let file = fs::OpenOptions::new()
        .write(true)
        .open(&temporary)
        .map_err(|_| "Could not write the file".to_string())?;
    let time = UNIX_EPOCH + Duration::from_millis(mtime.max(0) as u64);
    let _ = file.set_modified(time);
    let _ = file.sync_all();
    fs::rename(&temporary, to).map_err(|_| {
        let _ = fs::remove_file(&temporary);
        "Could not put the file in place".to_string()
    })
}

/// Removes a local file: to the trash when the desktop has one.
/// Moves a local file to the trash; false when the desktop has none for it.
/// A file is never deleted for good because a peer said so.
pub(super) fn trash(path: &Path) -> bool {
    #[cfg(test)]
    {
        if TEST_TRASH_FAILS.with(std::cell::Cell::get) {
            return false;
        }
        TEST_TRASHED.with(|count| count.set(count.get() + 1));
        if path.is_dir() {
            fs::remove_dir_all(path).is_ok()
        } else {
            fs::remove_file(path).is_ok()
        }
    }
    #[cfg(not(test))]
    {
        let file = gtk::gio::File::for_path(path);
        gtk::prelude::FileExt::trash(&file, gtk::gio::Cancellable::NONE).is_ok()
    }
}

#[cfg(test)]
thread_local! {
    /// Tests: a desktop without a trash (this test's thread only).
    pub(super) static TEST_TRASH_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Tests: how many items were sent to the trash (this thread).
    pub(super) static TEST_TRASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn mtime_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// The case folding both apps use to decide that two names are "the same
/// name" ("W3 — case clashes (folding)" in stageD-protocol.md): per code
/// point, the SIMPLE lowercase mapping only, taken when it yields exactly one
/// character and otherwise left as it is; no locale, no final-sigma context
/// (which `str::to_lowercase` applies), no normalisation. So `Σ` is `σ`
/// everywhere (never `ς`), `İ` and `ß` stay, `𐐀` becomes `𐐨`, and `é` typed
/// as `e` + U+0301 is another name than the precomposed one.
pub(crate) fn fold_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            let mut lower = c.to_lowercase();
            match (lower.next(), lower.next()) {
                (Some(single), None) => single,
                _ => c,
            }
        })
        .collect()
}
