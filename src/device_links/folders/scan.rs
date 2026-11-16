//! Reading a pair's folder: the scan, case clashes, files still being written,
//! and the clock the scan stamps with (`now_ms`).
//!
//! Sibling edges: `model`, `state`, `disk`.

use super::*;

pub(super) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

// ── Disk ───────────────────────────────────────────────────────────────────

/// Every file under `root` that syncs: relative path → size and mtime.
/// Symlinks are never followed; at most [`MAX_FILES`].
/// Why a Bucket item is listed as not synced when another differs only in case.
pub(super) const CASE_CLASH: &str = "another file differs only in capital letters";

/// "W3 — case clashes" (stageD-protocol.md): among the live items of one
/// directory, names that differ only in case (`A.txt`, `a.txt`; Linux holds
/// both, most other systems cannot) are one name. The one whose name sorts
/// first in UTF-8 byte order (so `A` beats `a`) is synced; the others, and
/// everything below them, are not. Files and folders count together, in a
/// scan and in a received manifest alike, so both sides pick the same one.
/// Returns the paths of the ones left out (not what is below them).
pub(super) fn drop_case_clashes<V>(
    items: &mut BTreeMap<String, V>,
    live: impl Fn(&V) -> bool,
) -> Vec<String> {
    let mut first: HashSet<(String, String)> = HashSet::new();
    let mut losers: Vec<String> = Vec::new();
    for (path, value) in items.iter() {
        if !live(value) {
            continue;
        }
        // Below one that is already left out: no say.
        let under_loser = path
            .match_indices('/')
            .any(|(at, _)| losers.iter().any(|loser| loser == &path[..at]));
        if under_loser {
            continue;
        }
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        if !first.insert((parent.to_string(), fold_name(name))) {
            losers.push(path.clone());
        }
    }
    items.retain(|path, value| {
        !live(value)
            || !losers.iter().any(|loser| {
                path == loser
                    || path
                        .strip_prefix(loser.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    });
    losers
}

/// [`drop_case_clashes`] over names that are all live, for the shared
/// fixture test.
#[cfg(test)]
pub(crate) fn drop_case_clashes_for_tests(items: &mut BTreeMap<String, ()>) -> Vec<String> {
    drop_case_clashes(items, |_| true)
}

/// The directory and the lowercased name of `path`: what names must differ in
/// to be different on a system that ignores case.
fn case_key(path: &str) -> (&str, String) {
    let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
    (parent, fold_name(name))
}

/// The other side's manifest, minus what clashes in case: within itself
/// ([`drop_case_clashes`]), and with a different live path of `local` (this
/// side's own, already synced or on disk). A remote path that clashes with a
/// local one is never downloaded, whichever of the two would sort first: the
/// local entry stays untouched and the two sides are not asked to agree on a
/// winner that would put both names on a Linux disk. Returns the paths left
/// out (not what is below them).
pub(super) fn left_out_of_remote(remote: &mut Manifest, local: &Manifest) -> Vec<String> {
    let mut left_out = drop_case_clashes(remote, |entry| !entry.deleted);
    let held: HashSet<(&str, String)> = local
        .iter()
        .filter(|(_, entry)| !entry.deleted)
        .map(|(path, _)| case_key(path))
        .collect();
    let mut clashing: Vec<String> = Vec::new();
    for (path, entry) in remote.iter() {
        let under = path
            .match_indices('/')
            .any(|(at, _)| clashing.iter().any(|loser| loser == &path[..at]));
        let (parent, key) = case_key(path);
        if !entry.deleted
            && !under
            && held.contains(&(parent, key))
            && local.get(path).is_none_or(|ours| ours.deleted)
        {
            clashing.push(path.clone());
        }
    }
    remote.retain(|path, entry| {
        entry.deleted
            || !clashing.iter().any(|loser| {
                path == loser
                    || path
                        .strip_prefix(loser.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
    });
    left_out.extend(clashing);
    left_out.sort();
    left_out
}

/// The problem text of a folder that is gone (C4: the banner offers a fix).
pub(crate) const FOLDER_MISSING: &str = "The folder is missing";

/// The fix for [`FOLDER_MISSING`]: makes the folder again. It comes back
/// empty, which the engine holds (nothing is deleted on the phone) until the
/// person confirms or the files return.
pub(crate) fn recreate_root(root: &Path) -> std::io::Result<()> {
    fs::create_dir_all(root)
}

/// A scan: the files, the directories to watch, whether there were more
/// files than allowed, and (a Bucket) the folders in it left unsynced.
type Scanned = (BTreeMap<String, Stat>, Vec<PathBuf>, bool, Vec<String>);

pub(super) fn scan(pair: &Pair) -> Result<Scanned, String> {
    let mut found = BTreeMap::new();
    let mut directories = Vec::new();
    let mut skipped = Vec::new();
    let mut too_many = false;
    let root_meta = fs::symlink_metadata(&pair.root).map_err(|_| FOLDER_MISSING.to_string())?;
    if !root_meta.is_dir() {
        return Err(FOLDER_MISSING.into());
    }
    let mut note = |path: &str, why: &str| {
        if skipped.len() < MAX_SKIPPED_SHOWN {
            skipped.push(format!("{path} ({why})"));
        }
    };
    let mut stack = vec![(pair.root.clone(), String::new(), 0usize)];
    while let Some((dir, prefix, depth)) = stack.pop() {
        if directories.len() < MAX_WATCHED_DIRS {
            directories.push(dir.clone());
        }
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                // A name that is not text cannot go on the wire: a Bucket says
                // so instead of leaving the file silently unsynced.
                if pair.bucket {
                    let shown = entry.file_name().to_string_lossy().into_owned();
                    let shown = if prefix.is_empty() {
                        shown
                    } else {
                        format!("{prefix}/{shown}")
                    };
                    note(&shown, "the name can't sync");
                }
                continue;
            };
            let path = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}/{name}")
            };
            if name.starts_with(".desklink-") && name.ends_with(".tmp") {
                // An interrupted put-in-place: gone once it is stale.
                let stale = entry
                    .metadata()
                    .ok()
                    .and_then(|metadata| metadata.modified().ok())
                    .and_then(|modified| modified.elapsed().ok())
                    .is_some_and(|age| age > STALE_TEMP);
                if stale {
                    let _ = fs::remove_file(entry.path());
                }
                continue;
            }
            if ignored(&path, pair.include_hidden, &pair.ignore) || validate_relpath(&path).is_err()
            {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            let linked = metadata.file_type().is_symlink();
            if pair.bucket && !name_ok(&name) {
                note(&path, "the name can't sync");
                continue;
            }
            // Links are never followed, special files never read.
            if metadata.is_dir() && !linked {
                if !pair.bucket {
                    if depth + 1 < MAX_DEPTH {
                        stack.push((entry.path(), path, depth + 1));
                    }
                } else if depth >= MAX_BUCKET_DEPTH {
                    note(&path, "more than 2 folders deep");
                } else if found.len() >= MAX_BUCKET_ITEMS {
                    too_many = true;
                } else {
                    found.insert(
                        path.clone(),
                        Stat {
                            size: 0,
                            mtime: 0,
                            dir: true,
                        },
                    );
                    stack.push((entry.path(), path, depth + 1));
                }
                continue;
            }
            if linked || !metadata.is_file() {
                if pair.bucket {
                    note(&path, "links and special files aren't synced");
                }
                continue;
            }
            if metadata.len() > MAX_FILE_BYTES {
                note(&path, "over 1 GiB");
                continue;
            }
            let cap = if pair.bucket {
                MAX_BUCKET_ITEMS
            } else {
                MAX_FILES
            };
            if found.len() >= cap {
                too_many = true;
                continue;
            }
            found.insert(
                path,
                Stat {
                    size: metadata.len(),
                    mtime: mtime_ms(&metadata),
                    dir: false,
                },
            );
        }
    }
    if pair.bucket {
        for loser in drop_case_clashes(&mut found, |_| true) {
            if skipped.len() < MAX_SKIPPED_SHOWN {
                skipped.push(format!("{loser} ({CASE_CLASH})"));
            }
        }
    }
    skipped.sort();
    Ok((found, directories, too_many, skipped))
}

/// A file that exists but is not in `found` — the scan stopped at its cap,
/// or skipped it (over 1 GiB, a name that can't sync) — is not gone: its entry
/// is kept as it is, so nothing is tombstoned (and deleted on the phone) for
/// it.
pub(super) fn keep_existing(found: &mut BTreeMap<String, Stat>, manifest: &Manifest, root: &Path) {
    for (path, entry) in manifest {
        if entry.deleted || found.contains_key(path) {
            continue;
        }
        if read_path(root, path).is_ok_and(|full| fs::symlink_metadata(full).is_ok())
            || inside_unreadable_folder(root, path)
        {
            found.insert(
                path.clone(),
                Stat {
                    size: entry.size,
                    mtime: entry.mtime,
                    dir: entry.dir,
                },
            );
        }
    }
}

/// Whether `path` lies in a folder that exists but cannot be listed (a
/// permission change, an I/O error): its files are not gone, the scan just
/// could not see them, and tombstoning them would delete them on the phone.
fn inside_unreadable_folder(root: &Path, path: &str) -> bool {
    let mut parent = Path::new(path).parent();
    while let Some(relative) = parent.filter(|relative| !relative.as_os_str().is_empty()) {
        let full = root.join(relative);
        match fs::symlink_metadata(&full) {
            Ok(metadata) if metadata.is_dir() => return fs::read_dir(&full).is_err(),
            Ok(_) => return false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // Cannot even look: not a folder that is known to be gone.
            Err(_) => return true,
        }
        parent = relative.parent();
    }
    false
}

#[cfg(test)]
thread_local! {
    /// Tests: how long a file must have held before it is scanned (ms).
    pub(super) static TEST_STABLE_MS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
}

/// [`STABLE_FOR`] in ms (tests write and scan at once: none unless asked).
pub(super) fn stable_ms() -> i64 {
    #[cfg(test)]
    {
        TEST_STABLE_MS.with(std::cell::Cell::get)
    }
    #[cfg(not(test))]
    {
        STABLE_FOR.as_millis() as i64
    }
}

/// F8c: a file whose mtime is under [`STABLE_FOR`] old may still be being
/// written. A new one is left out of `found` for now; a known one is held at
/// what the manifest says. Whether any was held (scan again soon).
pub(super) fn hold_unstable(
    found: &mut BTreeMap<String, Stat>,
    manifest: &Manifest,
    now_ms: i64,
    window: i64,
) -> bool {
    let mut held = false;
    let unstable: Vec<String> = found
        .iter()
        .filter(|(path, stat)| {
            !stat.dir
                && (0..window).contains(&(now_ms - stat.mtime))
                && manifest
                    .get(*path)
                    .is_none_or(|entry| entry.deleted || entry.mtime != stat.mtime)
        })
        .map(|(path, _)| path.clone())
        .collect();
    for path in unstable {
        held = true;
        match manifest
            .get(&path)
            .filter(|entry| !entry.deleted && !entry.dir)
        {
            Some(entry) => {
                found.insert(
                    path,
                    Stat {
                        size: entry.size,
                        mtime: entry.mtime,
                        dir: false,
                    },
                );
            }
            None => {
                found.remove(&path);
            }
        }
    }
    held
}
