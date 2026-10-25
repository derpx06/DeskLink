//! Two-way folder sync (DeskLink 1.2, F8): a desk folder and a phone folder
//! kept the same while the phone is connected (contract "DeskLink 1.2 — F8
//! two-way folder sync").
//!
//! Each side keeps a manifest per pair — every path it has seen, with its
//! size, mtime, sha256 and a two-counter version vector — and sends it to
//! the other. Each side plans from the other's manifest ([`plan`], pure and
//! property-tested): take what dominates, keep what wins, a concurrent edit
//! is a conflict (the desk's version keeps the name; the phone renames its
//! own), a concurrent edit beats a deletion, and a rename is found by hash.
//! Files travel over the existing file channel, only when asked for, and land
//! only under the pair's root.
//!
//! One worker thread owns every pair: scans (never on the GTK thread),
//! GIO directory monitors, the plans and the file moves.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::limits::{log_limited, Buckets};
use super::packet::{NetworkPacket, PACKET_TYPE_FOLDERS};
use super::webrtc::file_protocol::{validate_relpath, FolderRef};

// Limits. Each bounds what a peer, or a huge folder, can make this hold or do.
/// Pairs per phone.
pub(crate) const MAX_PAIRS_PER_DEVICE: usize = 8;
/// Manifest entries per pair (live and tombstones).
const MAX_FILES: usize = 20_000;
/// F8c: a file larger than this can't go in the Bucket (1 GiB; "1 GiB" to
/// the user).
pub(crate) const MAX_FILE_BYTES: u64 = 1 << 30;
/// F8c: folder levels in a Bucket: folders `A` and `A/B`, with files in any.
pub(crate) const MAX_BUCKET_DEPTH: usize = 2;
/// F8c: transfers in flight per direction; the rest read "Waiting…". One:
/// the other slot of the two per direction is kept for what the person
/// chose (a share, insert-from-phone), and the phone mirrors it.
pub(crate) const MAX_IN_FLIGHT: usize = super::webrtc::file_protocol::BUCKET_SLOTS_PER_DIRECTION;
/// F8c: one add action (a drop, a paste, `--bucket`): at most this many
/// items and bytes; the rest are left out and the user is told.
pub(crate) const MAX_ADD_ITEMS: usize = 500;
pub(crate) const MAX_ADD_BYTES: u64 = 5 << 30;
/// F8c: a name (one path segment) is at most this many bytes.
pub(crate) const MAX_NAME_BYTES: usize = 255;
/// F8c: a file is sent once its size and mtime have held this long (a
/// download still being written is not).
pub(crate) const STABLE_FOR: Duration = Duration::from_secs(2);
const MAX_NAME_CHARS: usize = 80;
/// A `manifest` packet stays one data-channel message once enveloped.
const MAX_PACKET_BYTES: usize = crate::device_links::webrtc::MAX_FEATURE_PACKET_BYTES;
/// Parts of one manifest, and entries held while assembling one.
const MAX_PARTS: usize = 256;
/// Paths per `request`, and outstanding requested paths per pair.
const REQUEST_PATHS: usize = 64;
const MAX_OUTSTANDING: usize = 256;
const TOMBSTONE_DAYS: i64 = 30;
/// Directories watched per pair (beyond it, the periodic rescan covers).
const MAX_WATCHED_DIRS: usize = 2048;
/// A leftover `.desklink-*.tmp` in a synced folder older than this is
/// removed by the next scan.
const STALE_TEMP: Duration = Duration::from_secs(3600);
/// Directory depth scanned.
const MAX_DEPTH: usize = 32;
/// Folders inside a Bucket listed as not synced (F8b: files only).
const MAX_SKIPPED_SHOWN: usize = 50;
/// Local changes settle this long before a rescan.
const SETTLE: Duration = Duration::from_secs(2);
/// A full rescan this often while connected, monitors or not.
const RESCAN_EVERY: Duration = Duration::from_secs(60);
/// An unanswered pair offer lapses.
const OFFER_LAPSES: Duration = Duration::from_secs(600);
const MAX_PENDING_OFFERS: usize = 8;
/// Conflicts listed per pair.
const MAX_CONFLICTS: usize = 200;
/// Messages waiting for the worker.
const QUEUE: usize = 512;
/// F8b: a phone's Bucket is the pair with this id.
pub(crate) const BUCKET_ID: &str = "bucket";
/// Items in one Bucket (the scan lists no more).
pub(crate) const MAX_BUCKET_ITEMS: usize = 10_000;
/// A Bucket larger than this shows a warning (8 GiB)...
pub(crate) const BUCKET_WARN_BYTES: u64 = 8 << 30;
/// ...and takes no more additions past this (10 GiB).
pub(crate) const BUCKET_FULL_BYTES: u64 = 10 << 30;
/// A pasted text or link item.
pub(crate) const MAX_TEXT_ITEM_BYTES: usize = 1024 * 1024;
/// Packets accepted per phone per minute (a full manifest is ≤ 256).
const PACKETS_PER_MINUTE: u32 = 1200;

// ── The model ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Side {
    Desk,
    Phone,
}

/// One counter per side: each bumps its own on a local change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Vv {
    pub(crate) desk: u64,
    pub(crate) phone: u64,
}

impl Vv {
    fn dominates(self, other: Vv) -> bool {
        self.desk >= other.desk && self.phone >= other.phone && self != other
    }

    fn max(self, other: Vv) -> Vv {
        Vv {
            desk: self.desk.max(other.desk),
            phone: self.phone.max(other.phone),
        }
    }

    fn bumped(mut self, side: Side) -> Vv {
        match side {
            Side::Desk => self.desk = self.desk.saturating_add(1),
            Side::Phone => self.phone = self.phone.saturating_add(1),
        }
        self
    }
}

fn is_false(value: &bool) -> bool {
    !value
}

/// A name another system (Windows, SAF) can hold too: bounded, with no
/// `\` or `:`, no control characters, no trailing dot or space.
pub(crate) fn name_ok(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_NAME_BYTES
        && !matches!(segment, "." | "..")
        && !segment.ends_with(['.', ' '])
        && !segment
            .chars()
            .any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
}

/// A path the Bucket holds: valid names, at most [`MAX_BUCKET_DEPTH`] folder
/// levels (a folder `A/B`, a file `A/B/c`).
pub(crate) fn bucket_path_ok(path: &str, dir: bool) -> bool {
    let segments = path.split('/').count();
    validate_relpath(path).is_ok()
        && segments <= MAX_BUCKET_DEPTH + usize::from(!dir)
        && path.split('/').all(name_ok)
}

/// The folder `path` is in ("" for the top).
pub(crate) fn parent_of(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Items under folder `dir` (all levels, folders too) and their bytes: a
/// folder tile's "N items · size".
pub(crate) fn totals(items: &[Item], dir: &str) -> (usize, u64) {
    let below = format!("{dir}/");
    items
        .iter()
        .filter(|item| item.path.starts_with(&below))
        .fold((0, 0), |(count, bytes), item| {
            (count + 1, bytes + item.size)
        })
}

/// A folder's badge, from what is in it: a conflict first, then movement,
/// then waiting.
pub(crate) fn rollup(states: impl IntoIterator<Item = ItemState>) -> ItemState {
    let mut result = ItemState::Synced;
    for state in states {
        result = match (result, state) {
            (ItemState::Conflict, _) | (_, ItemState::Conflict) => ItemState::Conflict,
            (ItemState::Receiving, _) | (_, ItemState::Receiving) => ItemState::Receiving,
            (ItemState::Sending, _) | (_, ItemState::Sending) => ItemState::Sending,
            (ItemState::Waiting, _) | (_, ItemState::Waiting) => ItemState::Waiting,
            _ => ItemState::Synced,
        };
    }
    result
}

/// Folder levels a path has above it (`a` 0, `A/a` 1).
fn levels(path: &str) -> usize {
    path.matches('/').count()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    #[serde(default)]
    pub(crate) size: u64,
    #[serde(default)]
    pub(crate) mtime: i64,
    #[serde(default)]
    pub(crate) sha256: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
    /// F8c: a folder (`dir: true` on the wire): no size or hash; only that
    /// it exists.
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) dir: bool,
    pub(crate) vv: Vv,
}

pub(crate) type Manifest = BTreeMap<String, Entry>;

/// A local file's size and mtime (ms), from a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    /// A folder.
    pub(crate) dir: bool,
}

/// One step of a plan, for the side that made it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Record the other side's entry (a tombstone, or the same content).
    Take { path: String, entry: Entry },
    /// Remove the local file; record `entry` (a tombstone).
    Delete { path: String, entry: Entry },
    /// Fetch the other side's file at `path`.
    Download { path: String, entry: Entry },
    /// A local file that `Delete` would remove has the content to fetch.
    Move {
        from: String,
        to: String,
        entry: Entry,
        tombstone: Entry,
    },
    /// Another local file has the content to fetch.
    Copy {
        from: String,
        to: String,
        entry: Entry,
    },
    /// Ours wins a concurrent change: our entry gets `vv`, so the other
    /// side takes it.
    Bump { path: String, vv: Vv },
    /// The phone's own version moves aside under the conflict name.
    Rename { from: String, to: String },
    /// A conflict to list (the desk's version kept, the phone's copy named).
    Conflict { path: String, copy: String },
}

/// `<stem> (conflict from <phone> <YYYY-MM-DD>)<.ext>`, beside `path`; the
/// date is `mtime`'s, UTC.
pub(crate) fn conflict_name(path: &str, phone: &str, mtime: i64) -> String {
    let (dir, name) = match path.rsplit_once('/') {
        Some((dir, name)) => (format!("{dir}/"), name),
        None => (String::new(), path),
    };
    let (stem, extension) = match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && extension.len() <= 16 => {
            (stem, format!(".{extension}"))
        }
        _ => (name, String::new()),
    };
    let date = gtk::glib::DateTime::from_unix_utc(mtime.div_euclid(1000))
        .ok()
        .and_then(|date| date.format("%Y-%m-%d").ok())
        .map(|date| date.to_string())
        .unwrap_or_else(|| "1970-01-01".into());
    let phone: String = phone
        .chars()
        .filter(|c| !matches!(c, '/' | '\\' | '\0' | ':') && !c.is_control())
        .take(40)
        .collect();
    let suffix = format!(" (conflict from {phone} {date}){extension}");
    // The name stays a valid filename (≤ 255 bytes).
    let mut stem = stem.to_string();
    while stem.len() + suffix.len() > 255 && stem.pop().is_some() {}
    format!("{dir}{stem}{suffix}")
}

/// P3: what `local_side` does with the other side's complete manifest.
pub(crate) fn plan(
    local_side: Side,
    local: &Manifest,
    remote: &Manifest,
    phone_name: &str,
) -> Vec<Step> {
    let mut steps = Vec::new();
    for (path, theirs) in remote {
        match local.get(path) {
            None if theirs.deleted => steps.push(Step::Take {
                path: path.clone(),
                entry: theirs.clone(),
            }),
            None => steps.push(Step::Download {
                path: path.clone(),
                entry: theirs.clone(),
            }),
            Some(ours) if ours.vv == theirs.vv => {}
            Some(ours) if theirs.vv.dominates(ours.vv) => {
                let same = ours.sha256 == theirs.sha256 && ours.deleted == theirs.deleted;
                steps.push(if same || (ours.deleted && theirs.deleted) {
                    Step::Take {
                        path: path.clone(),
                        entry: theirs.clone(),
                    }
                } else if theirs.deleted {
                    Step::Delete {
                        path: path.clone(),
                        entry: theirs.clone(),
                    }
                } else {
                    Step::Download {
                        path: path.clone(),
                        entry: theirs.clone(),
                    }
                });
            }
            Some(ours) if ours.vv.dominates(theirs.vv) => {}
            Some(ours) => {
                // Concurrent.
                let merged = ours.vv.max(theirs.vv);
                if (ours.deleted && theirs.deleted)
                    || (!ours.deleted && !theirs.deleted && ours.sha256 == theirs.sha256)
                {
                    steps.push(Step::Take {
                        path: path.clone(),
                        entry: Entry {
                            vv: merged,
                            ..theirs.clone()
                        },
                    });
                } else if ours.deleted {
                    // Their edit beats our deletion.
                    steps.push(Step::Download {
                        path: path.clone(),
                        entry: Entry {
                            vv: merged,
                            ..theirs.clone()
                        },
                    });
                } else if theirs.deleted {
                    steps.push(Step::Bump {
                        path: path.clone(),
                        vv: merged.bumped(local_side),
                    });
                } else {
                    // Both edited: the desk's keeps the name, the phone's
                    // is kept under the conflict name. Only the phone acts
                    // (renames its own, takes the desk's under a version
                    // that dominates both); the desk just lists it — had it
                    // bumped its counter, the phone would see the desk's as
                    // newer and drop its own.
                    let phone_mtime = match local_side {
                        Side::Desk => theirs.mtime,
                        Side::Phone => ours.mtime,
                    };
                    let copy = conflict_name(path, phone_name, phone_mtime);
                    let winner = merged.bumped(Side::Desk);
                    match local_side {
                        Side::Desk => steps.push(Step::Conflict {
                            path: path.clone(),
                            copy,
                        }),
                        Side::Phone => {
                            steps.push(Step::Rename {
                                from: path.clone(),
                                to: copy,
                            });
                            steps.push(Step::Download {
                                path: path.clone(),
                                entry: Entry {
                                    vv: winner,
                                    ..theirs.clone()
                                },
                            });
                        }
                    }
                }
            }
        }
    }
    renames_by_hash(local, steps)
}

/// A download whose content a deleted (or kept) local file already has
/// becomes a move (or a copy): no transfer.
fn renames_by_hash(local: &Manifest, steps: Vec<Step>) -> Vec<Step> {
    let mut deletes: HashMap<String, (String, Entry)> = HashMap::new();
    for step in &steps {
        if let Step::Delete { path, entry } = step {
            if let Some(ours) = local.get(path).filter(|ours| !ours.deleted && !ours.dir) {
                deletes
                    .entry(ours.sha256.clone())
                    .or_insert((path.clone(), entry.clone()));
            }
        }
    }
    let changing: HashSet<&str> = steps
        .iter()
        .filter_map(|step| match step {
            Step::Delete { path, .. }
            | Step::Download { path, .. }
            | Step::Rename { from: path, .. } => Some(path.as_str()),
            _ => None,
        })
        .collect();
    let kept: HashMap<&str, &str> = local
        .iter()
        .filter(|(path, entry)| !entry.deleted && !entry.dir && !changing.contains(path.as_str()))
        .map(|(path, entry)| (entry.sha256.as_str(), path.as_str()))
        .collect();
    let mut moved: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for step in steps {
        match step {
            Step::Download { path, entry } if entry.dir => out.push(Step::Download { path, entry }),
            Step::Download { path, entry } => {
                if let Some((from, tombstone)) = deletes.remove(&entry.sha256) {
                    moved.insert(from.clone());
                    out.push(Step::Move {
                        from,
                        to: path,
                        entry,
                        tombstone,
                    });
                } else if let Some(from) = kept.get(entry.sha256.as_str()) {
                    out.push(Step::Copy {
                        from: from.to_string(),
                        to: path,
                        entry,
                    });
                } else {
                    out.push(Step::Download { path, entry });
                }
            }
            other => out.push(other),
        }
    }
    out.retain(|step| !matches!(step, Step::Delete { path, .. } if moved.contains(path)));
    out
}

/// Folds a scan into the manifest for `side`: new or changed files bump
/// its counter (hashing only what changed), vanished ones become
/// tombstones. Whether anything changed.
pub(crate) fn apply_scan(
    side: Side,
    manifest: &mut Manifest,
    found: &BTreeMap<String, Stat>,
    hash: &mut dyn FnMut(&str) -> Option<String>,
    now: i64,
) -> bool {
    let mut changed = false;
    for (path, stat) in found {
        if stat.dir {
            let fresh = |vv: Vv| Entry {
                size: 0,
                mtime: 0,
                sha256: String::new(),
                deleted: false,
                dir: true,
                vv,
            };
            match manifest.get_mut(path) {
                Some(entry) if entry.dir && !entry.deleted => {}
                Some(entry) => {
                    *entry = fresh(entry.vv.bumped(side));
                    changed = true;
                }
                None => {
                    if manifest.len() < MAX_FILES {
                        manifest.insert(path.clone(), fresh(Vv::default().bumped(side)));
                        changed = true;
                    }
                }
            }
            continue;
        }
        match manifest.get_mut(path) {
            Some(entry)
                if !entry.deleted
                    && !entry.dir
                    && entry.size == stat.size
                    && entry.mtime == stat.mtime => {}
            existing => {
                let Some(sha256) = hash(path) else {
                    continue;
                };
                match existing {
                    Some(entry) if !entry.deleted && !entry.dir && entry.sha256 == sha256 => {
                        // Touched, not changed.
                        entry.size = stat.size;
                        entry.mtime = stat.mtime;
                    }
                    Some(entry) => {
                        *entry = Entry {
                            size: stat.size,
                            mtime: stat.mtime,
                            sha256,
                            deleted: false,
                            dir: false,
                            vv: entry.vv.bumped(side),
                        };
                    }
                    None => {
                        if manifest.len() >= MAX_FILES {
                            continue;
                        }
                        manifest.insert(
                            path.clone(),
                            Entry {
                                size: stat.size,
                                mtime: stat.mtime,
                                sha256,
                                deleted: false,
                                dir: false,
                                vv: Vv::default().bumped(side),
                            },
                        );
                    }
                }
                changed = true;
            }
        }
    }
    for (path, entry) in manifest.iter_mut() {
        if !entry.deleted && !found.contains_key(path) {
            entry.deleted = true;
            entry.size = 0;
            entry.sha256.clear();
            entry.mtime = now;
            entry.vv = entry.vv.bumped(side);
            changed = true;
        }
    }
    let cutoff = now - TOMBSTONE_DAYS * 24 * 3600 * 1000;
    manifest.retain(|_, entry| !entry.deleted || entry.mtime > cutoff);
    changed
}

/// P5, for one path relative to the root.
pub(crate) fn ignored(path: &str, include_hidden: bool, patterns: &[String]) -> bool {
    path.split('/').any(|segment| {
        segment == ".git"
            || segment == "node_modules"
            || segment.starts_with(".desklink")
            || segment.ends_with(".tmp")
            || segment.ends_with(".part")
            || segment.ends_with('~')
            || (!include_hidden && segment.starts_with('.'))
            || patterns
                .iter()
                .any(|pattern| match pattern.strip_prefix('*') {
                    Some(suffix) => !suffix.is_empty() && segment.ends_with(suffix),
                    None => segment == pattern,
                })
    })
}

// ── Pairs and their state ──────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Pair {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) root: PathBuf,
    pub(crate) device: String,
    #[serde(default)]
    pub(crate) include_hidden: bool,
    #[serde(default)]
    pub(crate) ignore: Vec<String>,
    /// F8b: this phone's Bucket (id `bucket`, default ignore rules).
    #[serde(default)]
    pub(crate) bucket: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Conflict {
    pub(crate) path: String,
    pub(crate) copy: String,
    pub(crate) at: i64,
}

/// A pair's saved state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Book {
    #[serde(default)]
    version: u32,
    manifest: Manifest,
    #[serde(default)]
    conflicts: Vec<Conflict>,
    #[serde(default)]
    last_sync: Option<i64>,
    /// F8b: which side each file came from ("From <device>"). Local.
    #[serde(default)]
    from: BTreeMap<String, Side>,
}

const TRASH_MISSING: &str =
    "Files deleted on the phone could not go to the trash here, so they were kept";

/// What the window shows for a pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    /// The phone is not connected.
    Waiting,
    /// The phone does not list this pair.
    NotOnPhone,
    /// Asked; the other side is choosing a folder.
    Offered,
    Scanning,
    Syncing(usize),
    UpToDate,
    TooMany,
    /// The folder scanned empty though it had files (an unmounted drive?):
    /// nothing is deleted anywhere until the user confirms.
    EmptyHeld,
    Problem(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PairView {
    pub(crate) pair: Pair,
    pub(crate) phase: Phase,
    pub(crate) files: usize,
    pub(crate) last_sync: Option<i64>,
    pub(crate) conflicts: Vec<Conflict>,
    /// F8b, a Bucket only: its files newest first, and the bytes they take.
    pub(crate) items: Vec<Item>,
    pub(crate) bytes: u64,
    /// F8b: folders in the Bucket that do not sync (files only for now).
    pub(crate) skipped: Vec<String>,
}

/// One Bucket item, as the window shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Item {
    /// A folder (F8c): its size is 0; the page adds its children up.
    pub(crate) dir: bool,
    /// Its name: Bucket items sit at the top level.
    pub(crate) path: String,
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    pub(crate) from: Side,
    pub(crate) state: ItemState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ItemState {
    Synced,
    /// The phone asked for it; on its way there.
    Sending,
    /// On its way here.
    Receiving,
    /// The phone has not got it yet and is not connected.
    Waiting,
    Conflict,
}

/// Which directory a Bucket root is: its device and inode, taken only when the
/// directory is there and can be listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RootStamp {
    dev: u64,
    ino: u64,
}

impl RootStamp {
    /// `None` when the root is missing, is not a directory, or cannot be read.
    pub(crate) fn of(root: &Path) -> Option<Self> {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::metadata(root)
            .ok()
            .filter(|metadata| metadata.is_dir())?;
        fs::read_dir(root).ok()?;
        Some(Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
        })
    }
}

/// Whether an emptied Bucket folder was emptied by the person (files
/// deleted one by one, the folder itself still the one that was there) and so
/// is applied to the phone. It is not when the folder is missing or
/// unreadable, was not known before, or is another directory on another
/// device or another inode: a drive that is not mounted, a folder swapped
/// for a new one. (Its modification time says nothing: deleting the last file
/// changes it.)
pub(crate) fn emptied_on_purpose(before: Option<RootStamp>, now: Option<RootStamp>) -> bool {
    matches!((before, now), (Some(before), Some(now)) if before == now)
}

/// A pair the phone proposed, waiting for a folder here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Offer {
    pub(crate) device: String,
    pub(crate) id: String,
    pub(crate) name: String,
    /// The phone asks to turn on the Bucket (no folder to choose here).
    pub(crate) bucket: bool,
}

#[derive(Debug, Default)]
struct Shown {
    pairs: Vec<PairView>,
    offers: Vec<Offer>,
    dropped: u32,
}

static SHOWN: Mutex<Shown> = Mutex::new(Shown {
    pairs: Vec::new(),
    offers: Vec::new(),
    dropped: 0,
});
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Bumped when what the window shows changed.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Whether `device` has its Bucket on.
pub(crate) fn bucket_on(device: &str) -> bool {
    SHOWN.lock().is_ok_and(|shown| {
        shown.pairs.iter().any(|view| {
            view.pair.device == device && view.pair.bucket && view.phase != Phase::Offered
        })
    })
}

pub(crate) fn pairs() -> Vec<PairView> {
    SHOWN
        .lock()
        .map(|shown| shown.pairs.clone())
        .unwrap_or_default()
}

/// How many old folder pairs stopped syncing when the Bucket replaced
/// them (told on the Bucket page until a Bucket is turned on).
pub(crate) fn dropped_pairs() -> u32 {
    SHOWN.lock().map(|shown| shown.dropped).unwrap_or(0)
}

pub(crate) fn offers() -> Vec<Offer> {
    SHOWN
        .lock()
        .map(|shown| shown.offers.clone())
        .unwrap_or_default()
}

/// Debug builds: rows for screenshots without a phone.
#[cfg(debug_assertions)]
pub(crate) fn debug_show(pairs: Vec<PairView>, offers: Vec<Offer>) {
    if let Ok(mut shown) = SHOWN.lock() {
        shown.pairs.extend(pairs);
        shown.offers.extend(offers);
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

// ── Messages to the worker ─────────────────────────────────────────────────

/// How a conflict is resolved (P6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolve {
    /// Keep both files.
    Both,
    /// Keep this computer's (the copy goes to the trash).
    Desk,
    /// Keep the phone's (the copy replaces the original).
    Phone,
}

#[derive(Debug)]
enum Message {
    Packet(String, NetworkPacket),
    Link(String, bool),
    Arrived {
        device: String,
        folder: FolderRef,
        stash: PathBuf,
        sha256: String,
        size: u64,
    },
    Accept {
        device: String,
        id: String,
        root: PathBuf,
    },
    Decline {
        device: String,
        id: String,
    },
    Remove {
        device: String,
        id: String,
    },
    /// "It's empty on purpose" for a held pair.
    ConfirmEmpty {
        device: String,
        id: String,
    },
    Resolve {
        device: String,
        pair: String,
        path: String,
        how: Resolve,
    },
    /// F8b: turn on this phone's Bucket (one tap).
    TurnOnBucket {
        device: String,
    },
    /// F8b: copy files and folders into the phone's Bucket.
    Import {
        device: String,
        sources: Vec<PathBuf>,
        into: String,
    },
    /// A copy lane finished: scan the Bucket soon.
    Touched(String),
    /// A send could not start: its slot is free again.
    SendFailed {
        device: String,
        path: String,
    },
    NewFolder {
        device: String,
        parent: String,
    },
    MoveItems {
        device: String,
        paths: Vec<String>,
        dest: String,
    },
    RemoveItems {
        device: String,
        paths: Vec<String>,
    },
    /// F8b: a pasted picture, text or link, saved as `name` in the Bucket.
    ImportBytes {
        device: String,
        name: String,
        bytes: Vec<u8>,
        into: String,
    },
}

fn queue() -> &'static OnceLock<SyncSender<Message>> {
    static SENDER: OnceLock<SyncSender<Message>> = OnceLock::new();
    &SENDER
}

fn send_message(message: Message) -> bool {
    match queue().get().map(|sender| sender.try_send(message)) {
        Some(Ok(())) => true,
        Some(Err(_)) => {
            log_limited("queue", "Folder sync is busy; a message was dropped");
            false
        }
        None => false,
    }
}

/// A `desklink.folders` packet from `device_id`.
pub(crate) fn on_packet(device_id: &str, packet: &NetworkPacket) {
    static PACKETS: Buckets = Buckets::new(PACKETS_PER_MINUTE, Duration::from_secs(60));
    if !PACKETS.allow(device_id) {
        log_limited("rate", "Too many folder-sync packets from a phone; dropped");
        return;
    }
    send_message(Message::Packet(device_id.to_string(), packet.clone()));
}

/// The link to `device_id` came up or went down: sync runs only while up.
pub(crate) fn link(device_id: &str, ready: bool) {
    #[cfg(test)]
    if let Ok(mut links) = TEST_LINKS.lock() {
        links.push((device_id.to_string(), ready));
    }
    send_message(Message::Link(device_id.to_string(), ready));
}

/// Tests: every call of [`link`], in order.
#[cfg(test)]
static TEST_LINKS: Mutex<Vec<(String, bool)>> = Mutex::new(Vec::new());

pub(crate) fn decline(device_id: &str, id: &str) {
    send_message(Message::Decline {
        device: device_id.to_string(),
        id: id.to_string(),
    });
}

/// F8b: turns on `device_id`'s Bucket (the phone is asked to pick its side).
pub(crate) fn turn_on_bucket(device_id: &str) {
    send_message(Message::TurnOnBucket {
        device: device_id.to_string(),
    });
}

/// F8c: copies `sources` (files and folders, within the depth limit) into
/// `into` ("" the top, `A`, `A/B`) of `device_id`'s Bucket. What was left out
/// comes back through [`take_report`].
pub(crate) fn import(device_id: &str, sources: Vec<PathBuf>, into: &str) -> bool {
    send_message(Message::Import {
        device: device_id.to_string(),
        sources,
        into: into.to_string(),
    })
}

/// F8c: a new folder in `parent` ("" the top).
pub(crate) fn new_folder(device_id: &str, parent: &str) -> bool {
    send_message(Message::NewFolder {
        device: device_id.to_string(),
        parent: parent.to_string(),
    })
}

/// F8c: moves Bucket items (files and folders) into folder `dest` ("" the
/// top). An item that would end up too deep stays where it is.
pub(crate) fn move_items(device_id: &str, paths: Vec<String>, dest: &str) -> bool {
    send_message(Message::MoveItems {
        device: device_id.to_string(),
        paths,
        dest: dest.to_string(),
    })
}

/// F8c: removes Bucket items from both sides (a folder to the trash as one
/// item, its transfers cancelled first).
pub(crate) fn remove_items(device_id: &str, paths: Vec<String>) -> bool {
    send_message(Message::RemoveItems {
        device: device_id.to_string(),
        paths,
    })
}

/// What an add, move or new folder left out, in plain words (the counts are
/// what the user is told).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Report {
    pub(crate) added: usize,
    pub(crate) too_deep: usize,
    pub(crate) too_big: usize,
    pub(crate) bad_names: usize,
    pub(crate) over_items: usize,
    pub(crate) over_bytes: usize,
    /// The Bucket would pass 10 GiB.
    pub(crate) full: bool,
    pub(crate) not_moved: usize,
    /// Hidden, `.git`, `*.tmp`... items the Bucket never syncs: left out at
    /// add time rather than copied and then never sent.
    pub(crate) hidden: usize,
    /// Copies that failed (unreadable, disk full, vanished).
    pub(crate) failed: usize,
    /// Files removed on this side that came back because the other side
    /// changed them (a few names).
    pub(crate) restored: Vec<String>,
    pub(crate) refused: Option<String>,
}

impl Report {
    /// The one message for the user, if there is anything to say.
    pub(crate) fn message(&self) -> Option<String> {
        let mut parts = Vec::new();
        if let Some(refused) = &self.refused {
            parts.push(refused.clone());
        }
        let counted = |what: &str, count: usize| match count {
            1 => what.to_string(),
            count => format!("{what} ({count})"),
        };
        if self.too_big > 0 {
            parts.push(counted(
                "Files over 1 GiB can't go in the Bucket",
                self.too_big,
            ));
        }
        if self.too_deep > 0 {
            parts.push(format!(
                "Some items were more than {MAX_BUCKET_DEPTH} folders deep and were left out ({})",
                self.too_deep
            ));
        }
        if self.bad_names > 0 {
            parts.push(format!(
                "Some names can't sync, so they were left out ({})",
                self.bad_names
            ));
        }
        if self.full {
            parts.push("The Bucket is full (10 GiB)".to_string());
        }
        if self.over_items > 0 {
            parts.push(format!(
                "Only {MAX_ADD_ITEMS} items can be added at once; {} were left out",
                self.over_items
            ));
        }
        if self.over_bytes > 0 {
            parts.push(format!(
                "Only 5 GiB can be added at once; {} were left out",
                self.over_bytes
            ));
        }
        if let Some(first) = self.restored.first() {
            parts.push(match self.restored.len() {
                1 => format!("{first} was restored: it was changed on the other side"),
                count => format!(
                    "{first} and {} more were restored: they were changed on the other side",
                    count - 1
                ),
            });
        }
        if self.hidden > 0 {
            parts.push(format!(
                "Hidden and temporary items don't sync, so they were left out ({})",
                self.hidden
            ));
        }
        if self.failed > 0 {
            parts.push(format!(
                "{} couldn't be copied",
                match self.failed {
                    1 => "1 item".to_string(),
                    count => format!("{count} items"),
                }
            ));
        }
        if self.not_moved > 0 {
            parts.push(format!(
                "{} couldn't be moved: they would be more than {MAX_BUCKET_DEPTH} folders deep",
                match self.not_moved {
                    1 => "1 item".to_string(),
                    count => format!("{count} items"),
                }
            ));
        }
        if self.added > 0 {
            parts.insert(
                0,
                match self.added {
                    1 => "Added 1 item to the Bucket".to_string(),
                    count => format!("Added {count} items to the Bucket"),
                },
            );
        }
        (!parts.is_empty()).then(|| parts.join(". "))
    }
}

/// Messages for the user, waiting for the window (or a notification).
/// (Tests keep one queue per test thread: they all run in one process and
/// used to take each other's messages.)
#[cfg(not(test))]
static REPORTS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
#[cfg(test)]
thread_local! {
    static REPORTS_LOCAL: std::cell::RefCell<VecDeque<String>> = const { std::cell::RefCell::new(VecDeque::new()) };
}

fn with_reports<T>(job: impl FnOnce(&mut VecDeque<String>) -> T) -> Option<T> {
    #[cfg(test)]
    {
        Some(REPORTS_LOCAL.with(|reports| job(&mut reports.borrow_mut())))
    }
    #[cfg(not(test))]
    {
        REPORTS.lock().ok().map(|mut reports| job(&mut reports))
    }
}

fn tell(report: &Report) {
    if let Some(message) = report.message() {
        with_reports(|reports| {
            if reports.len() >= 8 {
                reports.pop_front();
            }
            reports.push_back(message);
        });
    }
}

/// The next message about what the Bucket left out, if any.
pub(crate) fn take_report() -> Option<String> {
    with_reports(|reports| reports.pop_front()).flatten()
}

/// One thing to copy into the Bucket.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Add {
    src: PathBuf,
    rel: String,
    dir: bool,
    size: u64,
}

/// Entries under `dir`, counted (links not followed), up to `cap`.
fn count_below(dir: &Path, cap: usize) -> usize {
    let mut count = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            count += 1;
            if count >= cap {
                return cap;
            }
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    count
}

struct Collector<'a> {
    adds: Vec<Add>,
    report: &'a mut Report,
    used_bytes: u64,
    used_items: usize,
    bytes: u64,
}

impl Collector<'_> {
    /// Whether one more item of `size` fits; says why when it does not.
    fn room(&mut self, size: u64) -> bool {
        if self.adds.len() >= MAX_ADD_ITEMS || self.used_items + self.adds.len() >= MAX_BUCKET_ITEMS
        {
            self.report.over_items += 1;
        } else if self.bytes + size > MAX_ADD_BYTES {
            self.report.over_bytes += 1;
        } else if self.used_bytes + self.bytes + size > BUCKET_FULL_BYTES {
            self.report.full = true;
        } else {
            return true;
        }
        false
    }

    fn walk(&mut self, src: &Path, rel: &str) {
        let Ok(metadata) = fs::symlink_metadata(src) else {
            return;
        };
        // Links and special files are always skipped.
        let name = rel.rsplit('/').next().unwrap_or(rel);
        if metadata.file_type().is_symlink() || !(metadata.is_dir() || metadata.is_file()) {
            return;
        }
        if !name_ok(name) {
            self.report.bad_names += 1;
            return;
        }
        if ignored(rel, false, &[]) {
            self.report.hidden += 1;
            return;
        }
        if metadata.is_dir() {
            if levels(rel) + 1 > MAX_BUCKET_DEPTH {
                self.report.too_deep += 1 + count_below(src, MAX_BUCKET_ITEMS);
                return;
            }
            if !self.room(0) {
                return;
            }
            self.adds.push(Add {
                src: src.to_path_buf(),
                rel: rel.to_string(),
                dir: true,
                size: 0,
            });
            let Ok(children) = fs::read_dir(src) else {
                return;
            };
            let mut names: Vec<String> = children
                .flatten()
                .filter_map(|child| child.file_name().to_str().map(str::to_string))
                .collect();
            names.sort();
            for child in names {
                self.walk(&src.join(&child), &format!("{rel}/{child}"));
            }
        } else if metadata.len() > MAX_FILE_BYTES {
            self.report.too_big += 1;
        } else if self.room(metadata.len()) {
            self.bytes += metadata.len();
            self.adds.push(Add {
                src: src.to_path_buf(),
                rel: rel.to_string(),
                dir: false,
                size: metadata.len(),
            });
        }
    }
}

/// What to copy from `sources` into folder `into` of the Bucket at `dest_root`
/// (a folder is copied with what is in it, within the depth limit; the
/// limits of one add and of the Bucket are applied; every left-out item is
/// counted in `report`).
fn collect(
    sources: &[PathBuf],
    dest_root: &Path,
    into: &str,
    used: (u64, usize),
    report: &mut Report,
) -> Vec<Add> {
    let mut collector = Collector {
        adds: Vec::new(),
        report,
        used_bytes: used.0,
        used_items: used.1,
        bytes: 0,
    };
    let dest = if into.is_empty() {
        dest_root.to_path_buf()
    } else {
        dest_root.join(into)
    };
    let mut taken = HashSet::new();
    for source in sources.iter().take(MAX_BUCKET_ITEMS) {
        // Never the Bucket into itself.
        if source.starts_with(dest_root) || dest_root.starts_with(source) {
            continue;
        }
        let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let target = unique_among(&dest, name, &taken);
        taken.insert(target.clone());
        let Some(unique) = target.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let rel = if into.is_empty() {
            unique.to_string()
        } else {
            format!("{into}/{unique}")
        };
        collector.walk(source, &rel);
    }
    collector.adds
}

/// Writes `adds` under `dest_root`; how many were written and how many
/// failed.
fn copy_adds(adds: &[Add], dest_root: &Path) -> (usize, usize) {
    let mut written = 0;
    let mut failed = 0;
    for add in adds {
        let target = dest_root.join(&add.rel);
        let done = if add.dir {
            fs::create_dir(&target).is_ok() || target.is_dir()
        } else {
            let staged =
                target.with_file_name(format!(".desklink-{}.tmp", uuid::Uuid::new_v4().simple()));
            let mtime = fs::metadata(&add.src)
                .map(|meta| mtime_ms(&meta))
                .unwrap_or(0);
            fs::copy(&add.src, &staged).is_ok() && put_in_place(&staged, &target, mtime).is_ok()
        };
        if done {
            written += 1;
        } else {
            failed += 1;
            log_limited("import", "Could not copy an item into the Bucket");
        }
    }
    (written, failed)
}

/// F8b: saves pasted `bytes` as `name` in `device_id`'s Bucket. Text and
/// links over [`MAX_TEXT_ITEM_BYTES`] are refused here.
pub(crate) fn import_bytes(
    device_id: &str,
    name: &str,
    bytes: Vec<u8>,
    into: &str,
) -> Result<(), String> {
    let text = name.ends_with(".txt") || name.ends_with(".url");
    if text && bytes.len() > MAX_TEXT_ITEM_BYTES {
        return Err("That text is too long for the Bucket (1 MiB at most)".into());
    }
    if bytes.is_empty() {
        return Err("Nothing to add".into());
    }
    // Said now, so the toast is truthful: the worker checks again.
    if !name_ok(name) || !(into.is_empty() || bucket_path_ok(into, true)) {
        return Err("That name cannot be used".into());
    }
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err("Files over 1 GiB can't go in the Bucket".into());
    }
    send_message(Message::ImportBytes {
        device: device_id.to_string(),
        name: name.to_string(),
        bytes,
        into: into.to_string(),
    });
    Ok(())
}

/// A name from `raw` that [`name_ok`] accepts: what it rejects becomes `-`,
/// nothing trails with a dot or space, never empty.
pub(crate) fn clean_name(raw: &str, fallback: &str) -> String {
    let mut name: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':') {
                '-'
            } else {
                c
            }
        })
        .collect();
    while name.len() > MAX_NAME_BYTES {
        name.pop();
    }
    let name = name.trim_end_matches(['.', ' ']).trim_start();
    if name.is_empty() || matches!(name, "." | "..") {
        fallback.to_string()
    } else {
        name.to_string()
    }
}

/// The host of a pasted link as a file name: no `user@`, no `:port`, an IPv6
/// address without its brackets (`[::1]` → `1`).
fn link_host(link: &str) -> String {
    let authority = link
        .split("://")
        .nth(1)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("");
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host.strip_prefix('[') {
        Some(inside) => inside.split(']').next().unwrap_or(inside),
        None => host.rsplit_once(':').map_or(host, |(name, _port)| name),
    };
    let host: String = host.chars().take(80).collect();
    let name = clean_name(&host, "Link");
    match name.trim_matches('-') {
        "" => "Link".to_string(),
        trimmed => trimmed.to_string(),
    }
}

/// `Text <date time>.txt` for pasted text, `<host> <date>.url` (a
/// freedesktop link) for a pasted link: the name and the bytes to save.
pub(crate) fn pasted_text(text: &str, now: &gtk::glib::DateTime) -> (String, Vec<u8>) {
    let trimmed = text.trim();
    let link = (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        && !trimmed.contains(char::is_whitespace);
    if link {
        let host = link_host(trimmed);
        let date = now
            .format("%Y-%m-%d")
            .map(|d| d.to_string())
            .unwrap_or_default();
        let body = format!("[Desktop Entry]\nType=Link\nName={host}\nURL={trimmed}\n");
        (format!("{host} {date}.url"), body.into_bytes())
    } else {
        let stamp = now
            .format("%Y-%m-%d %H.%M.%S")
            .map(|d| d.to_string())
            .unwrap_or_default();
        (format!("Text {stamp}.txt"), text.as_bytes().to_vec())
    }
}

/// The user confirms a pair's folder is meant to be empty now.
pub(crate) fn confirm_empty(device_id: &str, id: &str) {
    send_message(Message::ConfirmEmpty {
        device: device_id.to_string(),
        id: id.to_string(),
    });
}

/// Stops syncing a pair (the Bucket too: its folder and files stay).
pub(crate) fn remove(device_id: &str, id: &str) {
    send_message(Message::Remove {
        device: device_id.to_string(),
        id: id.to_string(),
    });
}

pub(crate) fn resolve(device_id: &str, pair: &str, path: &str, how: Resolve) {
    send_message(Message::Resolve {
        device: device_id.to_string(),
        pair: pair.to_string(),
        path: path.to_string(),
        how,
    });
}

/// What the transfer manager asks before accepting a folder file (P4): a
/// pair of `device_id`'s, a valid path, and a file this side asked for.
pub(crate) fn accepts(device_id: &str, folder: &FolderRef, sha256: &str, size: u64) -> bool {
    bucket_path_ok(&folder.relpath, false)
        && size <= MAX_FILE_BYTES
        && WANTED.lock().is_ok_and(|wanted| {
            wanted
                .get(&(
                    device_id.to_string(),
                    folder.folder.clone(),
                    folder.relpath.clone(),
                ))
                .is_some_and(|entry| {
                    entry.sha256.eq_ignore_ascii_case(sha256) && entry.size == size
                })
        })
}

/// A requested file arrived, stashed at `stash`; the worker puts it in place.
pub(crate) fn arrived(
    device_id: &str,
    folder: FolderRef,
    stash: PathBuf,
    sha256: String,
    size: u64,
) {
    if !send_message(Message::Arrived {
        device: device_id.to_string(),
        folder,
        stash: stash.clone(),
        sha256,
        size,
    }) {
        let _ = fs::remove_file(stash);
    }
}

/// Tests: marks a file as asked for, as a plan would.
#[cfg(test)]
pub(crate) fn test_want(device_id: &str, folder: &FolderRef, sha256: &str, size: u64) {
    if let Ok(mut wanted) = WANTED.lock() {
        wanted.insert(
            (
                device_id.to_string(),
                folder.folder.clone(),
                folder.relpath.clone(),
            ),
            Entry {
                size,
                mtime: 0,
                sha256: sha256.to_string(),
                deleted: false,
                dir: false,
                vv: Vv::default(),
            },
        );
    }
}

/// Requested files, by (device, pair, path): the entry asked for.
/// Shared with the transfer manager's offer check.
static WANTED: Mutex<BTreeMap<(String, String, String), Entry>> = Mutex::new(BTreeMap::new());

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

fn mtime_ms(metadata: &fs::Metadata) -> i64 {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

// ── Disk ───────────────────────────────────────────────────────────────────

/// Every file under `root` that syncs: relative path → size and mtime.
/// Symlinks are never followed; at most [`MAX_FILES`].
/// Why a Bucket item is listed as not synced when another differs only in case.
const CASE_CLASH: &str = "another file differs only in capital letters";

/// "W3 — case clashes" (stageD-protocol.md): among the live items of one
/// directory, names that differ only in case (`A.txt`, `a.txt`; Linux holds
/// both, most other systems cannot) are one name. The one whose name sorts
/// first in UTF-8 byte order (so `A` beats `a`) is synced; the others, and
/// everything below them, are not. Files and folders count together, in a
/// scan and in a received manifest alike, so both sides pick the same one.
/// Returns the paths of the ones left out (not what is below them).
fn drop_case_clashes<V>(items: &mut BTreeMap<String, V>, live: impl Fn(&V) -> bool) -> Vec<String> {
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
        if !first.insert((parent.to_string(), name.to_lowercase())) {
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
    (parent, name.to_lowercase())
}

/// The other side's manifest, minus what clashes in case: within itself
/// ([`drop_case_clashes`]), and with a different live path of `local` (this
/// side's own, already synced or on disk). A remote path that clashes with a
/// local one is never downloaded, whichever of the two would sort first: the
/// local entry stays untouched and the two sides are not asked to agree on a
/// winner that would put both names on a Linux disk. Returns the paths left
/// out (not what is below them).
fn left_out_of_remote(remote: &mut Manifest, local: &Manifest) -> Vec<String> {
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

fn scan(pair: &Pair) -> Result<Scanned, String> {
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
fn keep_existing(found: &mut BTreeMap<String, Stat>, manifest: &Manifest, root: &Path) {
    for (path, entry) in manifest {
        if entry.deleted || found.contains_key(path) {
            continue;
        }
        if read_path(root, path).is_ok_and(|full| fs::symlink_metadata(full).is_ok()) {
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

/// [`STABLE_FOR`] in ms (tests write and scan at once: none unless asked).
fn stable_ms() -> i64 {
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
fn hold_unstable(
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

fn sha256_of(path: &Path) -> Option<String> {
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
fn target_path(root: &Path, relpath: &str) -> Result<PathBuf, String> {
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
fn read_path(root: &Path, relpath: &str) -> Result<PathBuf, String> {
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
fn make_dir(root: &Path, relpath: &str) -> Result<PathBuf, String> {
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
fn unchanged(path: &Path, entry: Option<&Entry>) -> bool {
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
fn bucket_root(home: &Path, phone: &str, taken: &[PathBuf]) -> PathBuf {
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
fn unique_in(dir: &Path, name: &str) -> PathBuf {
    unique_among(dir, name, &HashSet::new())
}

/// [`unique_in`], also avoiding names in `taken` (picked, not yet written).
fn unique_among(dir: &Path, name: &str, taken: &HashSet<PathBuf>) -> PathBuf {
    // The phone's file system may not tell "Photo.jpg" from "photo.jpg"
    // (and neither does a case-folding one here): the same rule as the case
    // clashes of a sync, so a name that differs only by case is taken.
    let lower = |path: &Path| path.to_string_lossy().to_lowercase();
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
fn home() -> PathBuf {
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
    static TEST_HOME: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
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
    static TEST_BOOKMARKS: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Adds (or removes) `folder` from the Files sidebar. Best effort; written
/// atomically, other lines untouched.
fn bookmark(folder: &Path, add: bool) {
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
fn put_in_place(from: &Path, to: &Path, mtime: i64) -> Result<(), String> {
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
fn trash(path: &Path) -> bool {
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
    static TEST_TRASH_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Tests: how long a file must have held before it is scanned (ms).
    static TEST_STABLE_MS: std::cell::Cell<i64> = const { std::cell::Cell::new(0) };
    /// Tests: how many items were sent to the trash (this thread).
    static TEST_TRASHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

// ── Storage ────────────────────────────────────────────────────────────────

const SCHEMA: u32 = 1;
const PAIRS_FILE: &str = "pairs.sealed";
const PAIRS_AAD: &[u8] = b"desklink-folder-pairs";
const BOOK_AAD: &[u8] = b"desklink-folder-manifest";

fn load<T: for<'de> Deserialize<'de> + Default>(dir: &Path, file: &str, aad: &[u8]) -> T {
    let path = dir.join(file);
    match super::vault::open(dir, &path, aad) {
        Some(plain) => serde_json::from_slice(&plain).unwrap_or_else(|_| {
            quarantine(&path);
            T::default()
        }),
        None => {
            if path.exists() {
                quarantine(&path);
            }
            T::default()
        }
    }
}

fn quarantine(path: &Path) {
    let _ = fs::rename(path, path.with_extension(format!("corrupt-{}", now_ms())));
    eprintln!("[Folders] A sync file could not be read; set aside");
}

fn save<T: Serialize>(dir: &Path, file: &str, aad: &[u8], value: &T) {
    let saved = serde_json::to_vec(value)
        .map_err(|error| error.to_string())
        .and_then(|plain| super::vault::seal(dir, &dir.join(file), aad, &plain));
    if saved.is_err() {
        log_limited("save", "Could not save folder sync state");
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Pairs {
    #[serde(default)]
    version: u32,
    pairs: Vec<Pair>,
    /// Folder pairs of the a4a4fdf build that stopped syncing when the
    /// Bucket replaced them: told once on the Bucket page.
    #[serde(default)]
    dropped: u32,
}

// ── The worker ─────────────────────────────────────────────────────────────

/// A pair at run time.
struct Live {
    pair: Pair,
    book: Book,
    phase: Phase,
    /// Watches its directories; a change marks it dirty.
    monitors: HashMap<PathBuf, gtk::gio::FileMonitor>,
    dirty: Option<Instant>,
    scanned: Option<Instant>,
    /// The other side's manifest, being assembled: parts received.
    incoming: BTreeMap<u32, Vec<(String, Entry)>>,
    /// The other side lists this pair.
    on_phone: bool,
    /// Asked of the other side, not yet arrived.
    outstanding: VecDeque<String>,
    /// Waiting to be asked (beyond [`MAX_OUTSTANDING`]).
    to_request: VecDeque<String>,
    /// The user confirmed an empty folder is meant: the next scan applies.
    allow_empty: bool,
    /// Which directory the root was at the last scan that held files: an
    /// emptied root that is still that directory is a deliberate removal; a
    /// different one (a drive unmounted, a folder swapped) is held.
    root_stamp: Option<RootStamp>,
    /// The other side's live files (path → sha256), from its last manifest.
    remote: HashMap<String, String>,
    /// Files the other side asked for, and their sha256: sending.
    sending: HashMap<String, String>,
    /// Files asked of the other side: receiving.
    receiving: BTreeMap<String, Entry>,
    /// What the Bucket does not sync, with why (too deep, a name that can't
    /// sync, over 1 GiB, a link).
    skipped: Vec<String>,
    /// The other side's paths left out of its last manifest because they
    /// differ only in case from another (see [`CASE_CLASH`]).
    remote_clashes: Vec<String>,
    /// F8c: files the other side asked for, waiting for a free slot of
    /// [`MAX_IN_FLIGHT`].
    send_queue: VecDeque<String>,
    /// Files being sent now, and since when.
    flight: HashMap<String, Instant>,
}

struct Worker {
    dir: PathBuf,
    pairs: Vec<Live>,
    ready: HashSet<String>,
    /// Pairs offered by this side, waiting for the phone: (device, id) →
    /// (root, a Bucket, when).
    asked: HashMap<(String, String), (PathBuf, bool, Instant)>,
    /// Offers from phones, waiting here.
    offered: Vec<(Offer, Instant)>,
    dirty: std::rc::Rc<std::cell::RefCell<HashSet<String>>>,
    /// Old folder pairs dropped (see [`Pairs::dropped`]).
    dropped: u32,
}

/// Loads the pairs from `dir` (`<config>/folders`) and starts the worker.
pub(crate) fn start(dir: PathBuf) {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    if queue().set(sender).is_err() {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("desklink-folders".into())
        .spawn(move || {
            let context = gtk::glib::MainContext::new();
            let _ = context.with_thread_default(|| run(&context, dir, &receiver));
        });
}

/// The Bucket is the only pair (F8b). Folder pairs of the a4a4fdf build
/// stop syncing: their files stay where they are; only DeskLink's record of
/// them goes. The Buckets, how many pairs were dropped (in all), and whether
/// the stored list changed.
fn keep_buckets(dir: &Path, stored: Pairs) -> (Vec<Pair>, u32, bool) {
    let (buckets, legacy): (Vec<Pair>, Vec<Pair>) = stored
        .pairs
        .into_iter()
        .partition(|pair| pair.bucket && pair.id == BUCKET_ID);
    for pair in &legacy {
        let _ = fs::remove_file(dir.join(book_file(&pair.device, &pair.id)));
    }
    (
        buckets,
        stored.dropped.saturating_add(legacy.len() as u32),
        !legacy.is_empty(),
    )
}

fn run(context: &gtk::glib::MainContext, dir: PathBuf, messages: &Receiver<Message>) {
    let _ = fs::create_dir_all(&dir);
    let stored: Pairs = load(&dir, PAIRS_FILE, PAIRS_AAD);
    let mut worker = Worker {
        pairs: Vec::new(),
        ready: HashSet::new(),
        asked: HashMap::new(),
        offered: Vec::new(),
        dirty: Default::default(),
        dropped: 0,
        dir,
    };
    let (buckets, dropped, changed) = keep_buckets(&worker.dir, stored);
    worker.dropped = dropped;
    for pair in buckets {
        let book: Book = load(&worker.dir, &book_file(&pair.device, &pair.id), BOOK_AAD);
        worker.pairs.push(Live::new(pair, book));
    }
    if changed {
        worker.save_pairs();
    }
    worker.publish();
    loop {
        // Twice a second while a Bucket is on (file changes, transfers);
        // every few seconds with none, when only a message can matter.
        let nap = if worker.pairs.is_empty() {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(500)
        };
        match messages.recv_timeout(nap) {
            Ok(message) => worker.handle(message),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while context.pending() {
            context.iteration(false);
        }
        worker.tick();
    }
}

/// A pair's state file: per phone, as each phone's Bucket has id `bucket`.
fn book_file(device: &str, id: &str) -> String {
    let device: String = device
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    format!("{device}-{id}.sealed")
}

fn valid_pair_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

fn folders_packet(op: &str, body: Value) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
    packet.set("op", op);
    if let Value::Object(map) = body {
        packet.body.extend(map);
    }
    packet
}

fn send(device: &str, packet: &NetworkPacket) {
    super::desk_features::send(device, packet);
}

/// `manifest` packets for `pair`, each under [`MAX_PACKET_BYTES`].
fn manifest_packets(id: &str, manifest: &Manifest) -> Vec<NetworkPacket> {
    let mut parts: Vec<Vec<Value>> = vec![Vec::new()];
    let mut size = 0;
    // Live entries first, then removals newest first: if the parts run out,
    // the oldest removals are what is left out.
    let mut ordered: Vec<(&String, &Entry)> = manifest.iter().collect();
    ordered.sort_by_key(|(_, entry)| (entry.deleted, std::cmp::Reverse(entry.mtime)));
    for (path, entry) in ordered.into_iter().take(MAX_FILES) {
        let mut value = serde_json::to_value(entry).unwrap_or(Value::Null);
        value["path"] = path.clone().into();
        let length = value.to_string().len() + 1;
        if size + length > MAX_PACKET_BYTES - 512 {
            parts.push(Vec::new());
            size = 0;
        }
        size += length;
        if let Some(part) = parts.last_mut() {
            part.push(value);
        }
    }
    let count = parts.len().min(MAX_PARTS);
    parts
        .into_iter()
        .take(MAX_PARTS)
        .enumerate()
        .map(|(index, entries)| {
            folders_packet(
                "manifest",
                json!({"id": id, "part": index + 1, "parts": count, "entries": entries}),
            )
        })
        .collect()
}

/// Entries of a `manifest` part that are safe to use, minus what this
/// pair ignores (a peer cannot have `.git/hooks/…` or hidden files written
/// here that the next scan would then make it delete).
fn manifest_entries(packet: &NetworkPacket, pair: &Pair) -> Vec<(String, Entry)> {
    packet
        .body
        .get("entries")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .take(MAX_FILES)
                .filter_map(|value| {
                    let path = value.get("path")?.as_str()?;
                    let entry: Entry = serde_json::from_value(value.clone()).ok()?;
                    let sane = validate_relpath(path).is_ok()
                        && (!pair.bucket || bucket_path_ok(path, entry.dir))
                        && !ignored(path, pair.include_hidden, &pair.ignore)
                        && (entry.deleted
                            || entry.dir
                            || (entry.sha256.len() == 64
                                && entry.sha256.bytes().all(|b| b.is_ascii_hexdigit())
                                && entry.size <= MAX_FILE_BYTES));
                    sane.then(|| {
                        (
                            path.to_string(),
                            Entry {
                                sha256: entry.sha256.to_ascii_lowercase(),
                                ..entry
                            },
                        )
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Live {
    fn new(pair: Pair, book: Book) -> Self {
        Self {
            pair,
            book,
            phase: Phase::Waiting,
            monitors: HashMap::new(),
            dirty: None,
            scanned: None,
            incoming: BTreeMap::new(),
            on_phone: true,
            outstanding: VecDeque::new(),
            to_request: VecDeque::new(),
            allow_empty: false,
            root_stamp: None,
            remote: HashMap::new(),
            sending: HashMap::new(),
            receiving: BTreeMap::new(),
            skipped: Vec::new(),
            remote_clashes: Vec::new(),
            send_queue: VecDeque::new(),
            flight: HashMap::new(),
        }
    }

    fn view(&self, _online: bool) -> PairView {
        PairView {
            pair: self.pair.clone(),
            phase: self.phase.clone(),
            files: self
                .book
                .manifest
                .values()
                .filter(|entry| !entry.deleted)
                .count(),
            last_sync: self.book.last_sync,
            conflicts: self.book.conflicts.clone(),
            items: if self.pair.bucket {
                self.items()
            } else {
                Vec::new()
            },
            bytes: self
                .book
                .manifest
                .values()
                .filter(|entry| !entry.deleted)
                .map(|entry| entry.size)
                .sum(),
            skipped: self
                .skipped
                .iter()
                .chain(&self.remote_clashes)
                .take(MAX_SKIPPED_SHOWN)
                .cloned()
                .collect(),
        }
    }

    /// F8b: the Bucket's items, newest first, each with its state.
    fn items(&self) -> Vec<Item> {
        let conflicted: HashSet<&str> = self
            .book
            .conflicts
            .iter()
            .flat_map(|conflict| [conflict.path.as_str(), conflict.copy.as_str()])
            .collect();
        let mut items: Vec<Item> = self
            .book
            .manifest
            .iter()
            .filter(|(_, entry)| !entry.deleted)
            .map(|(path, entry)| {
                let there = self.remote.get(path) == Some(&entry.sha256);
                let state = if conflicted.contains(path.as_str()) {
                    ItemState::Conflict
                } else if entry.dir {
                    ItemState::Synced
                } else if self.sending.contains_key(path) && !there {
                    ItemState::Sending
                } else if !there {
                    // "On the phone too" only once the phone's own manifest
                    // lists this exact content: online, not sending, and not
                    // there yet is waiting (queued, paused or declined), not
                    // synced (S2-5).
                    ItemState::Waiting
                } else {
                    ItemState::Synced
                };
                Item {
                    dir: entry.dir,
                    path: path.clone(),
                    size: entry.size,
                    mtime: entry.mtime,
                    from: self.book.from.get(path).copied().unwrap_or(
                        if entry.vv.desk >= entry.vv.phone {
                            Side::Desk
                        } else {
                            Side::Phone
                        },
                    ),
                    state,
                }
            })
            .collect();
        items.extend(
            self.receiving
                .iter()
                .filter(|(path, _)| self.book.manifest.get(*path).is_none_or(|e| e.deleted))
                .map(|(path, entry)| Item {
                    dir: false,
                    path: path.clone(),
                    size: entry.size,
                    mtime: entry.mtime,
                    from: Side::Phone,
                    state: ItemState::Receiving,
                }),
        );
        items.sort_by_key(|item| std::cmp::Reverse(item.mtime));
        items
    }
}

impl Worker {
    fn publish(&self) {
        if let Ok(mut shown) = SHOWN.lock() {
            shown.pairs = self
                .pairs
                .iter()
                .map(|live| live.view(self.ready.contains(&live.pair.device)))
                .collect();
            // Asked of a phone, waiting for it to choose its folder.
            shown
                .pairs
                .extend(self.asked.iter().map(|((device, id), (root, bucket, _))| {
                    PairView {
                        pair: Pair {
                            id: id.clone(),
                            name: root
                                .file_name()
                                .and_then(|name| name.to_str())
                                .unwrap_or("Folder")
                                .to_string(),
                            root: root.clone(),
                            device: device.clone(),
                            include_hidden: false,
                            ignore: Vec::new(),
                            bucket: *bucket,
                        },
                        phase: Phase::Offered,
                        files: 0,
                        last_sync: None,
                        conflicts: Vec::new(),
                        items: Vec::new(),
                        bytes: 0,
                        skipped: Vec::new(),
                    }
                }));
            shown.dropped = self.dropped;
            shown.offers = self
                .offered
                .iter()
                .map(|(offer, _)| offer.clone())
                .collect();
        }
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }

    fn save_pairs(&self) {
        let pairs = Pairs {
            version: SCHEMA,
            pairs: self.pairs.iter().map(|live| live.pair.clone()).collect(),
            dropped: self.dropped,
        };
        save(&self.dir, PAIRS_FILE, PAIRS_AAD, &pairs);
    }

    fn save_book(&mut self, index: usize) {
        let live = &mut self.pairs[index];
        live.book.version = SCHEMA;
        save(
            &self.dir,
            &book_file(&live.pair.device, &live.pair.id),
            BOOK_AAD,
            &live.book,
        );
    }

    /// A pair, by its phone and id (each phone's Bucket is `bucket`).
    fn index_of(&self, device: &str, id: &str) -> Option<usize> {
        self.pairs
            .iter()
            .position(|live| live.pair.device == device && live.pair.id == id)
    }

    fn handle(&mut self, message: Message) {
        match message {
            Message::Link(device, ready) => {
                if ready {
                    self.ready.insert(device.clone());
                    let ids: Vec<String> = self
                        .pairs
                        .iter()
                        .filter(|live| live.pair.device == device)
                        .map(|live| live.pair.id.clone())
                        .collect();
                    send(&device, &folders_packet("pairs", json!({"ids": ids})));
                    for live in self
                        .pairs
                        .iter_mut()
                        .filter(|live| live.pair.device == device)
                    {
                        // Scan now; the manifest follows the scan.
                        live.dirty = Some(Instant::now() - SETTLE);
                        live.scanned = None;
                    }
                } else {
                    self.ready.remove(&device);
                    for live in self
                        .pairs
                        .iter_mut()
                        .filter(|live| live.pair.device == device)
                    {
                        live.phase = Phase::Waiting;
                        live.monitors.clear();
                        live.incoming.clear();
                        live.outstanding.clear();
                        live.to_request.clear();
                        live.receiving.clear();
                        live.sending.clear();
                        live.send_queue.clear();
                        live.flight.clear();
                    }
                    if let Ok(mut wanted) = WANTED.lock() {
                        wanted.retain(|(owner, _, _), _| *owner != device);
                    }
                }
                self.publish();
            }
            Message::Packet(device, packet) => self.on_packet(&device, &packet),
            Message::Arrived {
                device,
                folder,
                stash,
                sha256,
                size,
            } => {
                self.place(&device, &folder, &stash, &sha256, size);
                let _ = fs::remove_file(&stash);
            }
            Message::Accept { device, id, root } => {
                let Some(position) = self
                    .offered
                    .iter()
                    .position(|(offer, _)| offer.id == id && offer.device == device)
                else {
                    return;
                };
                let (offer, _) = self.offered.remove(position);
                // The phone's Bucket needs no folder choice: it has its place.
                let root = if offer.bucket {
                    match self.make_bucket_root(&device) {
                        Some(root) => root,
                        None => return,
                    }
                } else {
                    root
                };
                if self.index_of(&device, &id).is_none() {
                    self.pairs.push(Live::new(
                        Pair {
                            id: id.clone(),
                            name: if offer.bucket {
                                "DeskLink Bucket".into()
                            } else {
                                offer.name
                            },
                            root,
                            device: device.clone(),
                            include_hidden: false,
                            ignore: Vec::new(),
                            bucket: offer.bucket,
                        },
                        Book::default(),
                    ));
                    self.save_pairs();
                }
                send(&device, &folders_packet("accept", json!({"id": id})));
                if let Some(index) = self.index_of(&device, &id) {
                    self.pairs[index].dirty = Some(Instant::now() - SETTLE);
                }
                self.publish();
            }
            Message::Decline { device, id } => {
                self.offered
                    .retain(|(offer, _)| !(offer.id == id && offer.device == device));
                send(&device, &folders_packet("decline", json!({"id": id})));
                self.publish();
            }
            Message::Remove { device, id } => {
                if let Some(index) = self.index_of(&device, &id) {
                    let live = self.pairs.remove(index);
                    send(&device, &folders_packet("remove", json!({"id": id})));
                    let _ = fs::remove_file(self.dir.join(book_file(&device, &id)));
                    if live.pair.bucket {
                        bookmark(&live.pair.root, false);
                    }
                    self.save_pairs();
                }
                if let Some((root, bucket, _)) = self.asked.remove(&(device.clone(), id.clone())) {
                    send(&device, &folders_packet("remove", json!({"id": id})));
                    if bucket {
                        bookmark(&root, false);
                    }
                }
                self.publish();
            }
            Message::Resolve {
                device,
                pair,
                path,
                how,
            } => self.resolve(&device, &pair, &path, how),
            Message::ConfirmEmpty { device, id } => {
                if let Some(index) = self.index_of(&device, &id) {
                    let live = &mut self.pairs[index];
                    live.allow_empty = true;
                    live.dirty = Some(Instant::now() - SETTLE);
                }
            }
            Message::TurnOnBucket { device } => self.turn_on_bucket(&device),
            Message::Import {
                device,
                sources,
                into,
            } => self.import(&device, sources, &into),
            Message::ImportBytes {
                device,
                name,
                bytes,
                into,
            } => self.import_bytes(&device, &name, bytes, &into),
            Message::Touched(device) => self.touched(&device),
            Message::SendFailed { device, path } => {
                if let Some(index) = self.index_of(&device, BUCKET_ID) {
                    self.pairs[index].flight.remove(&path);
                    self.pump_sends(index, &[]);
                }
            }
            Message::NewFolder { device, parent } => self.new_folder(&device, &parent),
            Message::MoveItems {
                device,
                paths,
                dest,
            } => self.move_items(&device, paths, &dest),
            Message::RemoveItems { device, paths } => self.remove_items(&device, paths),
        }
    }

    /// Creates (or reuses) the folder for `device`'s Bucket and bookmarks
    /// it. `None` when it cannot be made.
    fn make_bucket_root(&mut self, device: &str) -> Option<PathBuf> {
        let taken: Vec<PathBuf> = self
            .pairs
            .iter()
            .filter(|live| live.pair.device != device)
            .map(|live| live.pair.root.clone())
            .chain(
                self.asked
                    .iter()
                    .filter(|((owner, _), _)| owner != device)
                    .map(|(_, (root, _, _))| root.clone()),
            )
            .collect();
        let phone = super::connection_log::name_of(device);
        let root = bucket_root(&home(), &phone, &taken);
        if fs::create_dir_all(&root).is_err() {
            log_limited("bucket-dir", "Could not create the Bucket folder");
            return None;
        }
        bookmark(&root, true);
        Some(root)
    }

    /// F8b: one tap. The folder is made here; the phone picks its side.
    fn turn_on_bucket(&mut self, device: &str) {
        if self.index_of(device, BUCKET_ID).is_some()
            || self
                .asked
                .contains_key(&(device.to_string(), BUCKET_ID.to_string()))
        {
            return;
        }
        // The phone asked first: turning it on here is the answer.
        if self
            .offered
            .iter()
            .any(|(offer, _)| offer.device == device && offer.id == BUCKET_ID)
        {
            self.handle(Message::Accept {
                device: device.to_string(),
                id: BUCKET_ID.to_string(),
                root: PathBuf::new(),
            });
            return;
        }
        let Some(root) = self.make_bucket_root(device) else {
            return;
        };
        if self.dropped > 0 {
            self.dropped = 0;
            self.save_pairs();
        }
        send(
            device,
            &folders_packet(
                "offer",
                json!({"id": BUCKET_ID, "name": "DeskLink Bucket", "bucket": true}),
            ),
        );
        self.asked.insert(
            (device.to_string(), BUCKET_ID.to_string()),
            (root, true, Instant::now()),
        );
        self.publish();
    }

    /// The root things are added to: the Bucket's, while it is on (or
    /// waiting for the phone).
    fn bucket_dir(&self, device: &str) -> Option<PathBuf> {
        self.index_of(device, BUCKET_ID)
            .map(|index| self.pairs[index].pair.root.clone())
            .or_else(|| {
                self.asked
                    .get(&(device.to_string(), BUCKET_ID.to_string()))
                    .map(|(root, _, _)| root.clone())
            })
    }

    /// What `device`'s Bucket holds: bytes and items (files and folders).
    fn bucket_used(&self, device: &str) -> (u64, usize) {
        self.index_of(device, BUCKET_ID)
            .map(|index| {
                let live = self.pairs[index].book.manifest.values();
                live.filter(|entry| !entry.deleted)
                    .fold((0, 0), |(bytes, items), entry| {
                        (bytes + entry.size, items + 1)
                    })
            })
            .unwrap_or((0, 0))
    }

    fn import(&mut self, device: &str, sources: Vec<PathBuf>, into: &str) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        let mut report = Report::default();
        if !(into.is_empty() || bucket_path_ok(into, true))
            || make_dir(&dir, into).is_err() && !into.is_empty()
        {
            report.refused = Some("That folder isn't in the Bucket".into());
            tell(&report);
            return;
        }
        let adds = collect(&sources, &dir, into, self.bucket_used(device), &mut report);
        let owner = device.to_string();
        let job = move || {
            (report.added, report.failed) = copy_adds(&adds, &dir);
            tell(&report);
            send_message(Message::Touched(owner));
        };
        if !offload(&COPY_LANE, "desklink-bucket-copy", Box::new(job)) {
            tell(&Report {
                refused: Some("Still copying. Try again in a moment".into()),
                ..Report::default()
            });
        }
        #[cfg(test)]
        self.touched(device);
    }

    fn import_bytes(&mut self, device: &str, name: &str, bytes: Vec<u8>, into: &str) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        let (used_bytes, used_items) = self.bucket_used(device);
        let mut report = Report::default();
        if !name_ok(name) || name.contains('/') || !(into.is_empty() || bucket_path_ok(into, true))
        {
            report.refused = Some("That name cannot be used".into());
        } else if used_items >= MAX_BUCKET_ITEMS {
            report.over_items = 1;
        } else if used_bytes + bytes.len() as u64 > BUCKET_FULL_BYTES {
            report.full = true;
        }
        if report != Report::default() {
            tell(&report);
            return;
        }
        let Ok(folder) = make_dir(&dir, into).or_else(|_| {
            if into.is_empty() {
                Ok(dir.clone())
            } else {
                Err(())
            }
        }) else {
            return;
        };
        let (owner, name) = (device.to_string(), name.to_string());
        let job = move || {
            let target = unique_in(&folder, &name);
            let staged =
                target.with_file_name(format!(".desklink-{}.tmp", uuid::Uuid::new_v4().simple()));
            let written = fs::write(&staged, &bytes).is_ok()
                && put_in_place(&staged, &target, now_ms()).is_ok();
            if !written {
                let _ = fs::remove_file(&staged);
                log_limited("import", "Could not save a pasted item in the Bucket");
            }
            send_message(Message::Touched(owner));
        };
        if !offload(&COPY_LANE, "desklink-bucket-copy", Box::new(job)) {
            tell(&Report {
                refused: Some("Still copying. Try again in a moment".into()),
                ..Report::default()
            });
        }
        #[cfg(test)]
        self.touched(device);
    }

    /// F8c: "New Folder" in `parent`; not past the depth limit.
    fn new_folder(&mut self, device: &str, parent: &str) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        let depth = if parent.is_empty() {
            0
        } else {
            parent.split('/').count()
        };
        let mut report = Report::default();
        if depth >= MAX_BUCKET_DEPTH || !(parent.is_empty() || bucket_path_ok(parent, true)) {
            report.refused = Some(format!(
                "Folders can only go {MAX_BUCKET_DEPTH} levels deep in the Bucket"
            ));
        } else if self.bucket_used(device).1 >= MAX_BUCKET_ITEMS {
            report.over_items = 1;
        } else if let Ok(folder) = make_dir(&dir, parent) {
            let _ = fs::create_dir(unique_in(&folder, "New Folder"));
            self.touched(device);
        }
        tell(&report);
    }

    /// F8c: moves items into folder `dest` ("" the top); an item that
    /// would end up past the depth limit stays and is counted.
    fn move_items(&mut self, device: &str, paths: Vec<String>, dest: &str) {
        let (Some(index), Some(root)) = (self.index_of(device, BUCKET_ID), self.bucket_dir(device))
        else {
            return;
        };
        let mut report = Report::default();
        if !(dest.is_empty() || bucket_path_ok(dest, true))
            || make_dir(&root, dest).is_err() && !dest.is_empty()
        {
            report.refused = Some("That folder isn't in the Bucket".into());
            tell(&report);
            return;
        }
        let dest_depth = if dest.is_empty() {
            0
        } else {
            dest.split('/').count()
        };
        let manifest = &self.pairs[index].book.manifest;
        let mut moved = false;
        for path in paths.iter().take(MAX_ADD_ITEMS) {
            let Some(entry) = manifest.get(path).filter(|entry| !entry.deleted) else {
                continue;
            };
            let parent = path.rsplit_once('/').map_or("", |(parent, _)| parent);
            if parent == dest {
                continue;
            }
            if dest == path || dest.starts_with(&format!("{path}/")) {
                report.refused = Some("A folder can't go inside itself".into());
                continue;
            }
            if entry.dir {
                let below = format!("{path}/");
                let height = manifest
                    .iter()
                    .filter(|(other, found)| {
                        found.dir && !found.deleted && other.starts_with(&below)
                    })
                    .map(|(other, _)| levels(other) - levels(path))
                    .max()
                    .unwrap_or(0);
                if dest_depth + 1 + height > MAX_BUCKET_DEPTH {
                    report.not_moved += 1;
                    continue;
                }
            }
            let name = path.rsplit('/').next().unwrap_or(path);
            let target = unique_in(&root.join(dest), name);
            moved |= fs::rename(root.join(path), target).is_ok();
        }
        tell(&report);
        if moved {
            self.touched(device);
        }
    }

    /// F8c: removes items from both sides. Transfers of them are cancelled
    /// first; a folder goes to the trash as one item.
    fn remove_items(&mut self, device: &str, paths: Vec<String>) {
        let (Some(index), Some(root)) = (self.index_of(device, BUCKET_ID), self.bucket_dir(device))
        else {
            return;
        };
        let rows = super::webrtc::transfer_board::synced_rows();
        let live = &mut self.pairs[index];
        let mut removed = false;
        for path in paths.iter().take(MAX_BUCKET_ITEMS) {
            if !bucket_path_ok(path, true) && !bucket_path_ok(path, false) {
                continue;
            }
            let below = format!("{path}/");
            for row in rows
                .iter()
                .filter(|row| row.device_id == device)
                .filter(|row| row.name == *path || row.name.starts_with(&below))
            {
                let _ = super::desk_features::cancel_transfer(device, &row.id);
            }
            live.send_queue
                .retain(|queued| queued != path && !queued.starts_with(&below));
            live.flight
                .retain(|sent, _| sent != path && !sent.starts_with(&below));
            live.outstanding
                .retain(|asked| asked != path && !asked.starts_with(&below));
            live.to_request
                .retain(|asked| asked != path && !asked.starts_with(&below));
            // Files of it not fetched yet: gone for the phone too, or the
            // next manifest would bring them back.
            let unfetched: Vec<String> = live
                .receiving
                .keys()
                .filter(|known| *known == path || known.starts_with(&below))
                .cloned()
                .collect();
            for known in unfetched {
                let Some(theirs) = live.receiving.remove(&known) else {
                    continue;
                };
                live.remote.remove(&known);
                live.sending.remove(&known);
                if let Ok(mut wanted) = WANTED.lock() {
                    wanted.remove(&(device.to_string(), live.pair.id.clone(), known.clone()));
                }
                live.book.manifest.insert(
                    known,
                    Entry {
                        size: 0,
                        mtime: now_ms(),
                        sha256: String::new(),
                        deleted: true,
                        dir: false,
                        vv: theirs.vv.bumped(Side::Desk),
                    },
                );
                removed = true;
            }
            if fs::symlink_metadata(root.join(path)).is_ok() {
                removed |= trash(&root.join(path));
            }
        }
        // Everything removed on purpose: the empty folder is meant, so the
        // next scan tells the phone (it is not an unmounted drive).
        if removed && scan(&live.pair).is_ok_and(|(found, ..)| found.is_empty()) {
            live.allow_empty = true;
        }
        live.dirty = Some(Instant::now() - SETTLE);
        self.save_book(index);
        self.publish();
    }

    /// Starts the sends that may start now (see [`due_sends`]).
    fn pump_sends(&mut self, index: usize, ended: &[String]) {
        let live = &mut self.pairs[index];
        let due = due_sends(
            &mut live.send_queue,
            &mut live.flight,
            ended,
            Instant::now(),
        );
        let (device, id, root) = (
            live.pair.device.clone(),
            live.pair.id.clone(),
            live.pair.root.clone(),
        );
        let mut due = due.into_iter();
        while let Some(path) = due.next() {
            let Some(entry) = live.book.manifest.get(&path).filter(|entry| !entry.deleted) else {
                live.flight.remove(&path);
                continue;
            };
            let Ok(source) = read_path(&root, &path) else {
                live.flight.remove(&path);
                continue;
            };
            if !unchanged(&source, Some(entry)) {
                // Changed since it was asked for: scanned again; the new
                // version is asked for next.
                live.flight.remove(&path);
                live.dirty = Some(Instant::now());
                continue;
            }
            let folder = FolderRef {
                folder: id.clone(),
                relpath: path.clone(),
                mtime: Some(entry.mtime),
            };
            // Not on this worker: the transport may wait for the peer.
            let (owner, sent) = (device.clone(), path.clone());
            let content = entry.sha256.clone();
            let job = move || {
                if let Err(error) =
                    super::desk_features::send_folder_file(&owner, &source, folder, &content)
                {
                    log_limited("send", &format!("A synced file was not sent: {error}"));
                    send_message(Message::SendFailed {
                        device: owner,
                        path: sent,
                    });
                }
            };
            if !offload(&SEND_LANE, "desklink-bucket-send", Box::new(job)) {
                // Busy: this and the rest go back to the front of the queue,
                // in order; the next tick retries.
                for waiting in std::iter::once(path).chain(due).rev() {
                    live.flight.remove(&waiting);
                    live.send_queue.push_front(waiting);
                }
                break;
            }
        }
    }

    /// Something was added: scan the Bucket soon.
    fn touched(&mut self, device: &str) {
        if let Some(index) = self.index_of(device, BUCKET_ID) {
            self.pairs[index].dirty = Some(Instant::now() - SETTLE);
        }
    }

    fn on_packet(&mut self, device: &str, packet: &NetworkPacket) {
        let id = packet
            .get_str("id")
            .filter(|id| valid_pair_id(id))
            .map(str::to_string);
        match (packet.get_str("op"), id) {
            (Some("pairs"), _) => {
                let listed: HashSet<String> = packet
                    .body
                    .get("ids")
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .take(64)
                            .filter_map(|id| id.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                for live in self
                    .pairs
                    .iter_mut()
                    .filter(|live| live.pair.device == device)
                {
                    live.on_phone = listed.contains(&live.pair.id);
                    if !live.on_phone {
                        live.phase = Phase::NotOnPhone;
                    }
                }
                self.publish();
            }
            (Some("offer"), Some(id)) => {
                let name: String = packet
                    .get_str("name")
                    .unwrap_or("Folder")
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(MAX_NAME_CHARS)
                    .collect();
                let known = self.index_of(device, &id).is_some()
                    || self
                        .offered
                        .iter()
                        .any(|(offer, _)| offer.id == id && offer.device == device);
                let room = self.offered.len() < MAX_PENDING_OFFERS
                    && self
                        .pairs
                        .iter()
                        .filter(|live| live.pair.device == device)
                        .count()
                        < MAX_PAIRS_PER_DEVICE;
                if known {
                    return;
                }
                if !room {
                    send(device, &folders_packet("decline", json!({"id": id})));
                    return;
                }
                let bucket = id == BUCKET_ID && packet.get_bool("bucket").unwrap_or(false);
                if !bucket {
                    // The Bucket is the only pair now.
                    send(device, &folders_packet("decline", json!({"id": id})));
                    return;
                }
                self.offered.push((
                    Offer {
                        device: device.to_string(),
                        id,
                        name,
                        bucket,
                    },
                    Instant::now(),
                ));
                self.publish();
            }
            (Some("accept"), Some(id)) => {
                let Some((root, bucket, _)) = self.asked.remove(&(device.to_string(), id.clone()))
                else {
                    return;
                };
                let name: String = root
                    .file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("Folder")
                    .chars()
                    .take(MAX_NAME_CHARS)
                    .collect();
                let mut live = Live::new(
                    Pair {
                        id,
                        name,
                        root,
                        device: device.to_string(),
                        include_hidden: false,
                        ignore: Vec::new(),
                        bucket,
                    },
                    Book::default(),
                );
                live.dirty = Some(Instant::now() - SETTLE);
                self.pairs.push(live);
                self.save_pairs();
                self.publish();
            }
            (Some("decline"), Some(id)) => {
                if let Some((root, bucket, _)) = self.asked.remove(&(device.to_string(), id)) {
                    if bucket {
                        bookmark(&root, false);
                    }
                    self.publish();
                }
            }
            (Some("remove"), Some(id)) => {
                if let Some(index) = self.index_of(device, &id) {
                    let live = self.pairs.remove(index);
                    let _ = fs::remove_file(self.dir.join(book_file(device, &id)));
                    if live.pair.bucket {
                        bookmark(&live.pair.root, false);
                    }
                    self.save_pairs();
                }
                self.offered
                    .retain(|(offer, _)| !(offer.id == id && offer.device == device));
                self.asked.remove(&(device.to_string(), id));
                self.publish();
            }
            (Some("manifest"), Some(id)) => self.on_manifest(device, &id, packet),
            (Some("request"), Some(id)) => self.on_request(device, &id, packet),
            _ => {}
        }
    }

    fn on_manifest(&mut self, device: &str, id: &str, packet: &NetworkPacket) {
        let Some(index) = self.index_of(device, id) else {
            return;
        };
        let part = packet.get_i64("part").unwrap_or(0);
        let parts = packet.get_i64("parts").unwrap_or(0);
        if !(1..=MAX_PARTS as i64).contains(&parts) || !(1..=parts).contains(&part) {
            return;
        }
        let live = &mut self.pairs[index];
        live.on_phone = true;
        let held: usize = live.incoming.values().map(Vec::len).sum();
        if part == 1 {
            live.incoming.clear();
        }
        if held >= MAX_FILES * 2 {
            live.incoming.clear();
            return;
        }
        let entries = manifest_entries(packet, &live.pair);
        live.incoming.insert(part as u32, entries);
        if live.incoming.len() as i64 != parts {
            return;
        }
        let mut remote: Manifest = std::mem::take(&mut live.incoming)
            .into_values()
            .flatten()
            .take(MAX_FILES)
            .collect();
        if live.pair.bucket {
            let left_out = left_out_of_remote(&mut remote, &live.book.manifest);
            for path in &left_out {
                debug_log!("[DeskLink] Bucket: {path} is not synced (differs only in case)");
            }
            live.remote_clashes = left_out
                .into_iter()
                .map(|path| format!("{path} ({CASE_CLASH})"))
                .collect();
        }
        self.apply_plan(index, &remote);
    }

    /// P3 for one pair: carry out the plan against the other's manifest.
    fn apply_plan(&mut self, index: usize, remote: &Manifest) {
        let phone = super::connection_log::name_of(&self.pairs[index].pair.device);
        let steps = plan(Side::Desk, &self.pairs[index].book.manifest, remote, &phone);
        let root = self.pairs[index].pair.root.clone();
        let pair_id = self.pairs[index].pair.id.clone();
        let device = self.pairs[index].pair.device.clone();
        let mut downloads = Vec::new();
        let mut restored: Vec<String> = Vec::new();
        let mut rescan = false;
        let mut kept = false;
        {
            let live = &mut self.pairs[index];
            let manifest = &mut live.book.manifest;
            // A folder the other side removed, with only files that go too:
            // one trash item.
            let removals: HashMap<String, Entry> = steps
                .iter()
                .filter_map(|step| match step {
                    Step::Delete { path, entry } => Some((path.clone(), entry.clone())),
                    _ => None,
                })
                .collect();
            let mut handled: HashSet<String> = HashSet::new();
            let mut folders: Vec<&String> = removals
                .iter()
                .filter(|(_, entry)| entry.dir)
                .map(|(path, _)| path)
                .collect();
            folders.sort_by_key(|path| path.len());
            for folder in folders {
                if handled.contains(folder) {
                    continue;
                }
                let below_prefix = format!("{folder}/");
                let below: Vec<(String, Entry)> = manifest
                    .iter()
                    .filter(|(path, entry)| path.starts_with(&below_prefix) && !entry.deleted)
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect();
                let target = root.join(folder);
                let tidy = !below.is_empty()
                    && target.is_dir()
                    && below.iter().all(|(path, entry)| {
                        removals.contains_key(path) && unchanged(&root.join(path), Some(entry))
                    })
                    && count_below(&target, MAX_BUCKET_ITEMS) == below.len();
                if tidy && trash(&target) {
                    handled.insert(folder.clone());
                    manifest.insert(folder.clone(), removals[folder].clone());
                    for (path, _) in below {
                        manifest.insert(path.clone(), removals[&path].clone());
                        handled.insert(path);
                    }
                }
            }
            let mut folder_deletes: Vec<(String, Entry)> = Vec::new();
            for step in steps {
                if matches!(&step, Step::Delete { path, .. } if handled.contains(path)) {
                    continue;
                }
                match step {
                    Step::Take { path, entry } => {
                        manifest.insert(path, entry);
                    }
                    Step::Bump { path, vv } => {
                        if let Some(entry) = manifest.get_mut(&path) {
                            entry.vv = vv;
                        }
                    }
                    Step::Delete { path, entry } if entry.dir => {
                        folder_deletes.push((path, entry));
                    }
                    Step::Delete { path, entry } => match read_path(&root, &path) {
                        Ok(target) if unchanged(&target, manifest.get(&path)) => {
                            if !target.exists() || trash(&target) {
                                manifest.insert(path, entry);
                            } else {
                                // Kept: the next round asks again, the user
                                // is told meanwhile.
                                kept = true;
                            }
                        }
                        _ => rescan = true,
                    },
                    Step::Move {
                        from,
                        to,
                        entry,
                        tombstone,
                    } => {
                        let moved = read_path(&root, &from)
                            .and_then(|source| Ok((source, target_path(&root, &to)?)))
                            .ok()
                            .filter(|(source, target)| {
                                unchanged(source, manifest.get(&from))
                                    && unchanged(target, manifest.get(&to))
                            })
                            .is_some_and(|(source, target)| fs::rename(source, target).is_ok());
                        if moved {
                            manifest.insert(from, tombstone);
                            live.book.from.insert(to.clone(), Side::Phone);
                            manifest.insert(to, entry);
                        } else {
                            rescan = true;
                        }
                    }
                    Step::Copy { from, to, entry } => {
                        let copied = read_path(&root, &from)
                            .and_then(|source| Ok((source, target_path(&root, &to)?)))
                            .ok()
                            .filter(|(source, target)| {
                                unchanged(source, manifest.get(&from))
                                    && unchanged(target, manifest.get(&to))
                            })
                            .is_some_and(|(source, target)| {
                                let staged = target.with_file_name(format!(
                                    ".desklink-{}.tmp",
                                    uuid::Uuid::new_v4().simple()
                                ));
                                fs::copy(&source, &staged).is_ok()
                                    && put_in_place(&staged, &target, entry.mtime).is_ok()
                            });
                        if copied {
                            live.book.from.insert(to.clone(), Side::Phone);
                            manifest.insert(to, entry);
                        } else {
                            downloads.push((to, entry));
                        }
                    }
                    Step::Download { path, entry } if entry.dir => match make_dir(&root, &path) {
                        Ok(_) => {
                            manifest.insert(path, entry);
                        }
                        Err(_) => rescan = true,
                    },
                    Step::Download { path, entry } => {
                        // A file removed here that the other side changed
                        // comes back; the person is told (C10).
                        if live.pair.bucket
                            && manifest.get(&path).is_some_and(|ours| ours.deleted)
                            && !entry.deleted
                        {
                            restored.push(path.clone());
                        }
                        downloads.push((path, entry));
                    }
                    Step::Conflict { path, copy } => {
                        let conflicts = &mut live.book.conflicts;
                        if !conflicts.iter().any(|known| known.copy == copy)
                            && conflicts.len() < MAX_CONFLICTS
                        {
                            conflicts.push(Conflict {
                                path,
                                copy,
                                at: now_ms(),
                            });
                        }
                    }
                    // The desk never renames its own version (P3).
                    Step::Rename { .. } => {}
                }
            }
            // Folders go last, the deepest first, and only when empty: one
            // that still holds something (a change not synced yet) stays, and
            // the next scan tells the other side it is still here.
            folder_deletes.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
            for (path, entry) in folder_deletes {
                let target = root.join(&path);
                if !target.exists() || fs::remove_dir(&target).is_ok() {
                    manifest.insert(path, entry);
                } else if let Some(ours) = manifest.get(&path).cloned() {
                    // Kept: our version now dominates the removal, so the
                    // phone gets the folder back.
                    manifest.insert(
                        path,
                        Entry {
                            vv: ours.vv.max(entry.vv).bumped(Side::Desk),
                            ..ours
                        },
                    );
                }
            }
            if !restored.is_empty() {
                restored.truncate(MAX_SKIPPED_SHOWN);
                tell(&Report {
                    restored: std::mem::take(&mut restored),
                    ..Report::default()
                });
            }
            if let Ok(mut wanted) = WANTED.lock() {
                for (path, entry) in &downloads {
                    if wanted.len() < MAX_OUTSTANDING * MAX_PAIRS_PER_DEVICE * 4 {
                        wanted.insert(
                            (device.clone(), pair_id.clone(), path.clone()),
                            entry.clone(),
                        );
                    }
                }
            }
            live.to_request = downloads.iter().map(|(path, _)| path.clone()).collect();
            live.receiving = downloads.iter().cloned().collect();
            live.outstanding.clear();
            live.remote = remote
                .iter()
                .filter(|(_, entry)| !entry.deleted)
                .map(|(path, entry)| (path.clone(), entry.sha256.clone()))
                .collect();
            live.sending
                .retain(|path, sha| live.remote.get(path) != Some(sha));
            if rescan {
                live.dirty = Some(Instant::now());
            }
        }
        self.request_more(index);
        let live = &mut self.pairs[index];
        live.phase = if kept {
            Phase::Problem(TRASH_MISSING.into())
        } else if live.to_request.is_empty() && live.outstanding.is_empty() {
            live.book.last_sync = Some(now_ms());
            Phase::UpToDate
        } else {
            Phase::Syncing(live.to_request.len() + live.outstanding.len())
        };
        self.save_book(index);
        self.publish();
    }

    /// Asks for the next paths, up to [`MAX_OUTSTANDING`] at a time.
    fn request_more(&mut self, index: usize) {
        let live = &mut self.pairs[index];
        let mut batch = Vec::new();
        // And one data-channel message: paths may be 1 KiB each.
        let mut bytes = 0;
        while live.outstanding.len() < MAX_IN_FLIGHT
            && batch.len() < REQUEST_PATHS
            && bytes < MAX_PACKET_BYTES / 2
        {
            let Some(path) = live.to_request.pop_front() else {
                break;
            };
            bytes += path.len() * 2 + 3;
            live.outstanding.push_back(path.clone());
            batch.push(path);
        }
        if !batch.is_empty() {
            send(
                &live.pair.device,
                &folders_packet("request", json!({"id": live.pair.id, "paths": batch})),
            );
        }
    }

    /// P4: the phone asks for files: each sent if it still matches.
    fn on_request(&mut self, device: &str, id: &str, packet: &NetworkPacket) {
        let Some(index) = self.index_of(device, id) else {
            return;
        };
        let live = &mut self.pairs[index];
        let paths: Vec<String> = packet
            .body
            .get("paths")
            .and_then(Value::as_array)
            .map(|paths| {
                paths
                    .iter()
                    .take(REQUEST_PATHS)
                    .filter_map(|path| path.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for path in paths {
            let Some(entry) = live
                .book
                .manifest
                .get(&path)
                .filter(|entry| !entry.deleted && !entry.dir)
            else {
                continue;
            };
            let Ok(source) = read_path(&live.pair.root, &path) else {
                continue;
            };
            if !unchanged(&source, Some(entry)) {
                live_dirty(&self.dirty, id);
                continue;
            }
            if live.sending.len() < MAX_OUTSTANDING {
                live.sending.insert(path.clone(), entry.sha256.clone());
            }
            if live.send_queue.len() < MAX_OUTSTANDING && !live.flight.contains_key(&path) {
                live.send_queue.push_back(path);
            }
        }
        self.pump_sends(index, &[]);
    }

    /// A requested file arrived: checked, put in place, recorded.
    fn place(&mut self, device: &str, folder: &FolderRef, stash: &Path, sha256: &str, size: u64) {
        let key = (
            device.to_string(),
            folder.folder.clone(),
            folder.relpath.clone(),
        );
        let Some(entry) = WANTED
            .lock()
            .ok()
            .and_then(|mut wanted| wanted.remove(&key))
        else {
            return;
        };
        let Some(index) = self.index_of(device, &folder.folder) else {
            return;
        };
        if !entry.sha256.eq_ignore_ascii_case(sha256) || entry.size != size {
            return;
        }
        let live = &mut self.pairs[index];
        live.outstanding.retain(|path| *path != folder.relpath);
        let placed = target_path(&live.pair.root, &folder.relpath).and_then(|target| {
            if !unchanged(&target, live.book.manifest.get(&folder.relpath)) {
                return Err("changed here meanwhile".into());
            }
            put_in_place(stash, &target, entry.mtime)?;
            let metadata = fs::metadata(&target).map_err(|_| "vanished".to_string())?;
            Ok(Stat {
                size: metadata.len(),
                mtime: mtime_ms(&metadata),
                dir: false,
            })
        });
        match placed {
            Ok(stat) => {
                live.receiving.remove(&folder.relpath);
                live.book.from.insert(folder.relpath.clone(), Side::Phone);
                live.remote
                    .insert(folder.relpath.clone(), entry.sha256.clone());
                live.book.manifest.insert(
                    folder.relpath.clone(),
                    Entry {
                        size: stat.size,
                        mtime: stat.mtime,
                        ..entry
                    },
                );
            }
            Err(_) => live.dirty = Some(Instant::now()),
        }
        self.request_more(index);
        let live = &mut self.pairs[index];
        if live.outstanding.is_empty() && live.to_request.is_empty() {
            live.phase = Phase::UpToDate;
            live.book.last_sync = Some(now_ms());
            self.save_book(index);
            self.send_manifest(index);
        } else {
            live.phase = Phase::Syncing(live.outstanding.len() + live.to_request.len());
        }
        self.publish();
    }

    fn send_manifest(&self, index: usize) {
        let live = &self.pairs[index];
        if !self.ready.contains(&live.pair.device) {
            return;
        }
        for packet in manifest_packets(&live.pair.id, &live.book.manifest) {
            send(&live.pair.device, &packet);
        }
    }

    fn resolve(&mut self, device: &str, pair: &str, path: &str, how: Resolve) {
        let Some(index) = self.index_of(device, pair) else {
            return;
        };
        let live = &mut self.pairs[index];
        let Some(position) = live
            .book
            .conflicts
            .iter()
            .position(|known| known.path == path)
        else {
            return;
        };
        let conflict = live.book.conflicts.remove(position);
        let root = live.pair.root.clone();
        match how {
            Resolve::Both => {}
            Resolve::Desk => {
                if let Ok(copy) = read_path(&root, &conflict.copy) {
                    trash(&copy);
                }
            }
            Resolve::Phone => {
                if let (Ok(copy), Ok(original)) = (
                    read_path(&root, &conflict.copy),
                    read_path(&root, &conflict.path),
                ) {
                    // This computer's version goes to the trash first: the
                    // rename alone replaced it for good (S4). Without a
                    // trash it stays, and the conflict with it.
                    if original.exists() && !trash(&original) {
                        live.book.conflicts.insert(position, conflict);
                        self.publish();
                        return;
                    }
                    let _ = fs::rename(copy, original);
                }
            }
        }
        live.dirty = Some(Instant::now());
        self.save_book(index);
        self.publish();
    }

    fn tick(&mut self) {
        let dirty: Vec<String> = self.dirty.borrow_mut().drain().collect();
        for id in dirty {
            if let Some(live) = self.pairs.iter_mut().find(|live| live.pair.id == id) {
                live.dirty.get_or_insert_with(Instant::now);
            }
        }
        // Transfers that ended free their slots (F8c: 2 in flight).
        let ended = super::webrtc::transfer_board::take_synced_ended();
        for index in 0..self.pairs.len() {
            let device = self.pairs[index].pair.device.clone();
            let sent: Vec<String> = ended
                .iter()
                .filter(|end| end.device == device && !end.incoming)
                .map(|end| end.name.clone())
                .collect();
            let failed: Vec<&str> = ended
                .iter()
                .filter(|end| end.device == device && end.incoming && !end.ok)
                .map(|end| end.name.as_str())
                .collect();
            let live = &mut self.pairs[index];
            if !failed.is_empty() {
                live.outstanding
                    .retain(|path| !failed.contains(&path.as_str()));
            }
            if !sent.is_empty() || !live.send_queue.is_empty() {
                self.pump_sends(index, &sent);
            }
            if !failed.is_empty() {
                self.request_more(index);
            }
        }
        let before = self.offered.len() + self.asked.len();
        self.offered.retain(|(_, at)| at.elapsed() < OFFER_LAPSES);
        self.asked
            .retain(|_, (_, _, at)| at.elapsed() < OFFER_LAPSES);
        let mut changed = before != self.offered.len() + self.asked.len();
        for index in 0..self.pairs.len() {
            let live = &self.pairs[index];
            if !self.ready.contains(&live.pair.device) || !live.on_phone {
                continue;
            }
            let due = live.dirty.is_some_and(|at| at.elapsed() >= SETTLE)
                || live.scanned.is_none_or(|at| at.elapsed() >= RESCAN_EVERY);
            if due {
                self.rescan(index);
                changed = true;
            }
        }
        if changed {
            self.publish();
        }
    }

    /// Scans a pair, re-watches its directories, and sends the manifest
    /// when something changed (or it never went out on this link).
    fn rescan(&mut self, index: usize) {
        let first = self.pairs[index].scanned.is_none();
        let live = &mut self.pairs[index];
        // Spent by this scan, whatever it finds: a later empty scan (an
        // unmounted drive) is held again.
        let allow_empty = std::mem::take(&mut live.allow_empty);
        live.dirty = None;
        live.scanned = Some(Instant::now());
        live.phase = Phase::Scanning;
        let pair = live.pair.clone();
        let scanned = scan(&pair);
        let (mut found, directories, too_many, skipped) = match scanned {
            Ok(scanned) => scanned,
            Err(problem) => {
                live.phase = Phase::Problem(problem);
                return;
            }
        };
        live.skipped = skipped;
        let had_files = live.book.manifest.values().any(|entry| !entry.deleted);
        let stamp = RootStamp::of(&pair.root);
        if found.is_empty()
            && had_files
            && !allow_empty
            && !emptied_on_purpose(live.root_stamp, stamp)
        {
            // An unmounted drive looks exactly like this: deleting every
            // file on the phone too would be the worst outcome.
            live.phase = Phase::EmptyHeld;
            return;
        }
        if stamp.is_some() {
            live.root_stamp = stamp;
        }
        // A file still being written waits until it has held for 2 s.
        if hold_unstable(&mut found, &live.book.manifest, now_ms(), stable_ms()) {
            live.dirty = Some(Instant::now());
        }
        let root = pair.root.clone();
        keep_existing(&mut found, &live.book.manifest, &root);
        let changed = apply_scan(
            Side::Desk,
            &mut live.book.manifest,
            &found,
            &mut |path| sha256_of(&root.join(path)),
            now_ms(),
        );
        // New here: from this computer. Forgotten files go.
        for path in found.keys() {
            if !live.book.from.contains_key(path) {
                live.book.from.insert(path.clone(), Side::Desk);
            }
        }
        let manifest = &live.book.manifest;
        live.book
            .from
            .retain(|path, _| manifest.get(path).is_some_and(|entry| !entry.deleted));
        // Watch what exists now.
        let wanted: HashSet<PathBuf> = directories.into_iter().collect();
        live.monitors.retain(|dir, _| wanted.contains(dir));
        for dir in wanted {
            if live.monitors.contains_key(&dir) || live.monitors.len() >= MAX_WATCHED_DIRS {
                continue;
            }
            let file = gtk::gio::File::for_path(&dir);
            if let Ok(monitor) = gtk::prelude::FileExt::monitor_directory(
                &file,
                gtk::gio::FileMonitorFlags::WATCH_MOVES,
                gtk::gio::Cancellable::NONE,
            ) {
                let (dirty, id) = (std::rc::Rc::clone(&self.dirty), pair.id.clone());
                gtk::prelude::FileMonitorExt::connect_changed(&monitor, move |_, _, _, _| {
                    live_dirty(&dirty, &id);
                });
                live.monitors.insert(dir, monitor);
            }
        }
        live.phase = if too_many {
            Phase::TooMany
        } else if live.outstanding.is_empty() && live.to_request.is_empty() {
            Phase::UpToDate
        } else {
            Phase::Syncing(live.outstanding.len() + live.to_request.len())
        };
        if changed {
            self.save_book(index);
        }
        if changed || first {
            self.send_manifest(index);
        }
    }
}

/// Work that copies or snapshots whole files runs on its own thread, so the
/// folder worker keeps answering packets and ticks (and its queue of 512
/// cannot fill while a gigabyte is copied).
type Job = Box<dyn FnOnce() + Send>;
/// Jobs waiting for a lane; past it the caller is told "busy".
const LANE_QUEUE: usize = 8;
static COPY_LANE: OnceLock<SyncSender<Job>> = OnceLock::new();
static SEND_LANE: OnceLock<SyncSender<Job>> = OnceLock::new();

/// Runs `job` on `lane`'s thread (tests: at once); false when the lane is
/// full.
fn offload(lane: &'static OnceLock<SyncSender<Job>>, name: &str, job: Job) -> bool {
    #[cfg(test)]
    {
        let _ = (lane, name);
        job();
        true
    }
    #[cfg(not(test))]
    {
        let sender = lane.get_or_init(|| {
            let (sender, jobs) = mpsc::sync_channel::<Job>(LANE_QUEUE);
            let _ = std::thread::Builder::new()
                .name(name.to_string())
                .spawn(move || {
                    for job in jobs {
                        crate::guard("folder copy", (), job);
                    }
                });
            sender
        });
        sender.try_send(job).is_ok()
    }
}

/// A send that never showed on the board lapses after this.
const FLIGHT_LAPSES: Duration = Duration::from_secs(1800);

/// F8c: the files to start sending now: the slots of `ended` are freed,
/// then the queue fills up to [`MAX_IN_FLIGHT`] in flight.
fn due_sends(
    queue: &mut VecDeque<String>,
    flight: &mut HashMap<String, Instant>,
    ended: &[String],
    now: Instant,
) -> Vec<String> {
    for path in ended {
        flight.remove(path);
    }
    flight.retain(|_, at| now.saturating_duration_since(*at) < FLIGHT_LAPSES);
    let mut due = Vec::new();
    while flight.len() < MAX_IN_FLIGHT {
        let Some(path) = queue.pop_front() else {
            break;
        };
        if flight.insert(path.clone(), now).is_none() {
            due.push(path);
        }
    }
    due
}

fn live_dirty(dirty: &std::rc::Rc<std::cell::RefCell<HashSet<String>>>, id: &str) {
    let mut dirty = dirty.borrow_mut();
    if dirty.len() < 1024 {
        dirty.insert(id.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_bucket_manifest_fits_the_wire_part_by_part() {
        let mut manifest = Manifest::new();
        for index in 0..MAX_BUCKET_ITEMS {
            manifest.insert(
                format!("{index:05} {}.jpg", "\u{10348}".repeat(40)),
                entry(&"a".repeat(64), (u64::MAX, u64::MAX)),
            );
        }
        let packets = manifest_packets(BUCKET_ID, &manifest);
        assert!(packets.len() <= MAX_PARTS, "{} parts", packets.len());
        for packet in &packets {
            assert!(
                crate::device_links::webrtc::fits_on_wire(packet),
                "{:?}",
                crate::device_links::webrtc::enveloped_len(packet)
            );
        }
    }

    /// C4: a gone folder reads "The folder is missing", and the fix button's
    /// action makes it again (empty, which the engine then holds).
    #[test]
    fn a_missing_bucket_folder_can_be_created_again() {
        let base = std::env::temp_dir().join(format!("desklink-missing-{}", uuid::Uuid::new_v4()));
        let root = base.join("Bucket");
        let pair = Pair {
            id: BUCKET_ID.into(),
            name: "DeskLink Bucket".into(),
            root: root.clone(),
            device: "phone-b".into(),
            include_hidden: false,
            ignore: Vec::new(),
            bucket: true,
        };
        assert_eq!(scan(&pair).err().as_deref(), Some(FOLDER_MISSING));
        recreate_root(&root).unwrap();
        assert!(scan(&pair).is_ok());
        let _ = fs::remove_dir_all(&base);
    }

    fn bucket_worker(base: &Path, root: &Path) -> Worker {
        let mut worker = bucket_worker_for(base, root, "phone-b");
        worker.ready = ["phone-b".to_string()].into();
        worker
    }

    /// A worker with `device`'s Bucket, the phone not yet ready.
    fn bucket_worker_for(base: &Path, root: &Path, device: &str) -> Worker {
        let pair = Pair {
            id: BUCKET_ID.into(),
            name: "DeskLink Bucket".into(),
            root: root.to_path_buf(),
            device: device.into(),
            include_hidden: false,
            ignore: Vec::new(),
            bucket: true,
        };
        let worker = Worker {
            dir: base.join("state"),
            pairs: vec![Live::new(pair, Book::default())],
            ready: HashSet::new(),
            asked: HashMap::new(),
            offered: Vec::new(),
            dirty: Default::default(),
            dropped: 0,
        };
        fs::create_dir_all(&worker.dir).unwrap();
        worker
    }

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("desklink-{name}-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    // ── Folder sync starts when the phone is feature-ready ────────────────

    /// Regression: the W1 removals deleted `folders::link(id, true)` together
    /// with the automations call beside it, so the worker never counted a phone
    /// as ready and the Bucket never synced. The chain: a link goes
    /// feature-ready, the folder worker is told, a file written into the
    /// Bucket's folder is scanned and its manifest goes out.
    #[test]
    fn a_feature_ready_link_makes_the_bucket_sync_and_send_its_manifest() {
        let device = format!(
            "{}{}",
            super::super::desk_features::TEST_OUTBOX_PREFIX,
            uuid::Uuid::new_v4().simple()
        );
        let base = scratch("ready-chain");
        let root = base.join("bucket");
        fs::create_dir_all(&root).unwrap();
        let mut worker = bucket_worker_for(&base, &root, &device);
        fs::write(root.join("hello.txt"), "hi").unwrap();
        let manifests = |device: &str| {
            super::super::desk_features::test_sent(device)
                .into_iter()
                .filter(|packet| {
                    packet.packet_type == PACKET_TYPE_FOLDERS
                        && packet.get_str("op") == Some("manifest")
                })
                .collect::<Vec<_>>()
        };
        // A phone that is not ready is not synced, however long it waits.
        for _ in 0..3 {
            worker.tick();
        }
        assert!(manifests(&device).is_empty());

        // The link goes feature-ready: this is what the daemon calls.
        super::super::desk_features::on_feature_ready(&device);
        let linked = |device: &str| -> Vec<bool> {
            TEST_LINKS
                .lock()
                .unwrap()
                .iter()
                .filter(|(owner, _)| owner == device)
                .map(|(_, ready)| *ready)
                .collect()
        };
        let waited = Instant::now();
        while linked(&device).is_empty() {
            assert!(
                waited.elapsed() < Duration::from_secs(10),
                "feature-ready never told the folder worker"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(linked(&device), [true]);

        // The folder worker hears it, scans, and sends the manifest.
        worker.handle(Message::Link(device.clone(), true));
        worker.tick();
        let sent = manifests(&device);
        assert_eq!(sent.len(), 1, "one manifest");
        let listed = serde_json::Value::Object(sent[0].body.clone()).to_string();
        assert!(listed.contains("hello.txt"), "{listed}");
        let pairs = super::super::desk_features::test_sent(&device)
            .into_iter()
            .any(|packet| packet.get_str("op") == Some("pairs"));
        assert!(pairs, "the phone is told which pairs this side has");
        let _ = fs::remove_dir_all(base);
    }

    // ── F8c ────────────────────────────────────────────────────────────────

    fn scratch_bucket(name: &str) -> (PathBuf, PathBuf, Worker) {
        let base = scratch(name);
        let root = base.join("bucket");
        fs::create_dir_all(&root).unwrap();
        let worker = bucket_worker(&base, &root);
        (base, root, worker)
    }

    fn trashed() -> usize {
        TEST_TRASHED.with(std::cell::Cell::get)
    }

    #[test]
    fn the_limits_are_the_documented_ones() {
        assert_eq!(MAX_FILE_BYTES, 1 << 30, "1 GiB per file");
        assert_eq!(MAX_BUCKET_DEPTH, 2);
        assert_eq!(MAX_IN_FLIGHT, 1, "the phone mirrors it");
        assert_eq!((MAX_ADD_ITEMS, MAX_ADD_BYTES), (500, 5 << 30));
        assert_eq!(BUCKET_WARN_BYTES, 8 << 30, "warning at 8 GiB");
        assert_eq!(BUCKET_FULL_BYTES, 10 << 30, "refusal at 10 GiB");
        assert_eq!(MAX_BUCKET_ITEMS, 10_000);
        assert_eq!(MAX_NAME_BYTES, 255);
        assert_eq!(MAX_TEXT_ITEM_BYTES, 1 << 20);
        assert_eq!(STABLE_FOR, Duration::from_secs(2));
        assert_eq!(LANE_QUEUE, 8, "copy jobs waiting before \"busy\"");
    }

    #[test]
    fn names_that_another_system_cannot_hold_are_not_synced() {
        for good in ["a.txt", "Trip 2026", "naïve – café.md", &"x".repeat(255)] {
            assert!(name_ok(good), "{good}");
        }
        for bad in [
            "",
            ".",
            "..",
            "a:b",
            "a\\b",
            "trailing.",
            "trailing ",
            "a\u{7}b",
            &"x".repeat(256),
            &"é".repeat(128),
        ] {
            assert!(!name_ok(bad), "{bad:?}");
        }
        assert!(bucket_path_ok("A/B", true) && bucket_path_ok("A/B/c.txt", false));
        assert!(!bucket_path_ok("A/B/C", true) && !bucket_path_ok("A/B/C/d.txt", false));
        // Scanned from disk: listed with why, never synced.
        let (base, root, worker) = scratch_bucket("bucket-names");
        fs::write(root.join("ok.txt"), "1").unwrap();
        fs::write(root.join("a:b.txt"), "1").unwrap();
        fs::write(root.join("dot."), "1").unwrap();
        let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
        assert_eq!(found.keys().collect::<Vec<_>>(), ["ok.txt"]);
        assert_eq!(skipped.len(), 2, "{skipped:?}");
        assert!(skipped
            .iter()
            .all(|line| line.contains("the name can't sync")));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn depth_three_is_refused_and_reported_everywhere() {
        let (base, root, worker) = scratch_bucket("bucket-depth");
        fs::create_dir_all(root.join("A/B/C/D")).unwrap();
        fs::write(root.join("A/B/ok.txt"), "x").unwrap();
        fs::write(root.join("A/B/C/deep.txt"), "x").unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), root.join("link")).unwrap();
        // From outside, in the Bucket folder: listed as not synced.
        let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
        assert_eq!(
            found.keys().map(String::as_str).collect::<Vec<_>>(),
            ["A", "A/B", "A/B/ok.txt"]
        );
        assert!(found["A"].dir && found["A/B"].dir && !found["A/B/ok.txt"].dir);
        assert!(skipped
            .iter()
            .any(|line| line == "A/B/C (more than 2 folders deep)"));
        assert!(
            skipped.iter().any(|line| line.starts_with("link (")),
            "links listed"
        );
        // From the phone: a deeper path is dropped, a dir entry needs no hash.
        let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
        let file = |path: &str| {
            json!({"path": path, "size": 1, "mtime": 1, "sha256": "a".repeat(64),
                   "vv": {"desk": 0, "phone": 1}})
        };
        let folder = |path: &str| json!({"path": path, "dir": true, "vv": {"desk": 0, "phone": 1}});
        packet.set(
            "entries",
            json!([
                file("A/B/C/d.txt"),
                file("A/B/e.txt"),
                folder("A/B/C"),
                folder("A/B")
            ]),
        );
        let kept: Vec<String> = manifest_entries(&packet, &worker.pairs[0].pair)
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(kept, ["A/B/e.txt", "A/B"]);
        let wanted = FolderRef {
            folder: BUCKET_ID.into(),
            relpath: "A/B/C/d.txt".into(),
            mtime: None,
        };
        test_want("phone-b", &wanted, &"a".repeat(64), 1);
        assert!(
            !accepts("phone-b", &wanted, &"a".repeat(64), 1),
            "never accepted"
        );
        // From a drop: counted, and said.
        let outside = base.join("outside");
        fs::create_dir_all(outside.join("T/U/V")).unwrap();
        fs::write(outside.join("T/U/V/w.txt"), "x").unwrap();
        fs::write(outside.join("T/U/x.txt"), "x").unwrap();
        let mut report = Report::default();
        let adds = collect(&[outside.join("T")], &root, "A", (0, 0), &mut report);
        // Into A, T is level 2 and U would be level 3.
        assert_eq!(
            adds.iter().map(|add| add.rel.as_str()).collect::<Vec<_>>(),
            ["A/T"]
        );
        assert_eq!(report.too_deep, 4, "U, V and the two files in them");
        assert_eq!(
            report.message().unwrap(),
            "Some items were more than 2 folders deep and were left out (4)"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_nested_round_trip_with_an_empty_folder_converges() {
        let mut desk = Replica {
            side: Side::Desk,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut phone = Replica {
            side: Side::Phone,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        for dir in ["A", "A/B", "Empty"] {
            desk.disk.insert(dir.into(), DIR.into());
        }
        desk.disk.insert("A/B/deep.txt".into(), "deep".into());
        desk.disk.insert("top.txt".into(), "top".into());
        desk.scan(1);
        sync(&mut phone, &desk);
        phone.scan(2);
        assert_eq!(
            desk.disk, phone.disk,
            "folders, the empty one too, and files"
        );
        assert!(phone.manifest["Empty"].dir && phone.manifest["A/B"].dir);
        // And back: the phone adds a folder and a file inside the desk's.
        phone.disk.insert("A/New".into(), DIR.into());
        phone.disk.insert("A/New/p.txt".into(), "phone".into());
        phone.scan(3);
        sync(&mut desk, &phone);
        desk.scan(4);
        assert_eq!(desk.disk, phone.disk);
        // The empty folder is removed on one side: the other follows.
        desk.disk.remove("Empty");
        desk.scan(5);
        sync(&mut phone, &desk);
        phone.scan(6);
        assert_eq!(desk.disk, phone.disk);
        assert!(!phone.disk.contains_key("Empty"));
    }

    #[test]
    fn a_folder_rename_moves_its_files_and_transfers_nothing() {
        let mut desk = Replica {
            side: Side::Desk,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut phone = Replica {
            side: Side::Phone,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        desk.disk.insert("Trips".into(), DIR.into());
        desk.disk.insert("Trips/Rome".into(), DIR.into());
        desk.disk.insert("Trips/Rome/a.jpg".into(), "aaa".into());
        desk.disk.insert("Trips/b.jpg".into(), "bbb".into());
        desk.scan(1);
        sync(&mut phone, &desk);
        phone.scan(2);
        // The desk renames the folder.
        let moved: Vec<(String, String)> = desk
            .disk
            .iter()
            .map(|(path, content)| (path.replacen("Trips", "Journeys", 1), content.clone()))
            .collect();
        desk.disk = moved.into_iter().collect();
        desk.scan(3);
        let steps = plan(Side::Phone, &phone.manifest, &desk.manifest, "P");
        let downloads: Vec<&Step> = steps
            .iter()
            .filter(|step| matches!(step, Step::Download { entry, .. } if !entry.dir))
            .collect();
        assert!(downloads.is_empty(), "nothing re-sent: {downloads:?}");
        assert_eq!(
            steps
                .iter()
                .filter(|step| matches!(step, Step::Move { .. }))
                .count(),
            2,
            "both files moved by hash"
        );
        sync(&mut phone, &desk);
        phone.scan(4);
        assert_eq!(desk.disk, phone.disk);
        assert!(phone.disk.contains_key("Journeys/Rome/a.jpg"));
        assert!(
            !phone.disk.contains_key("Trips"),
            "the old folder went, empty"
        );
    }

    #[test]
    fn a_rename_and_an_edit_on_opposite_sides_lose_nothing_and_converge() {
        let mut desk = Replica {
            side: Side::Desk,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut phone = Replica {
            side: Side::Phone,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        desk.disk.insert("A".into(), DIR.into());
        desk.disk.insert("A/f.txt".into(), "base".into());
        desk.scan(1);
        sync(&mut phone, &desk);
        phone.scan(2);
        // The desk renames the folder; the phone edits a file inside it.
        desk.disk.remove("A");
        desk.disk.remove("A/f.txt");
        desk.disk.insert("B".into(), DIR.into());
        desk.disk.insert("B/f.txt".into(), "base".into());
        phone.disk.insert("A/f.txt".into(), "phone edit".into());
        desk.scan(3);
        phone.scan(3);
        for round in 4..9 {
            sync(&mut desk, &phone);
            desk.scan(round);
            sync(&mut phone, &desk);
            phone.scan(round);
        }
        assert_eq!(desk.disk, phone.disk, "converged");
        assert!(
            desk.disk.values().any(|content| content == "phone edit"),
            "the edit is not lost: {:?}",
            desk.disk
        );
        assert!(desk.disk.contains_key("B/f.txt"), "the rename is applied");
    }

    /// S2-5: "on <phone> too" only when the phone's own manifest lists the
    /// file; online with the phone not having it yet is waiting.
    #[test]
    fn a_file_the_phone_does_not_have_is_not_shown_as_synced() {
        let (base, root, mut worker) = scratch_bucket("bucket-not-there");
        fs::write(root.join("a.txt"), "a").unwrap();
        worker.rescan(0);
        let state = |worker: &Worker| {
            worker.pairs[0]
                .view(true)
                .items
                .iter()
                .find(|item| item.path == "a.txt")
                .map(|item| item.state)
        };
        assert_eq!(state(&worker), Some(ItemState::Waiting));
        let sha = worker.pairs[0].book.manifest["a.txt"].sha256.clone();
        worker.pairs[0].remote.insert("a.txt".into(), sha);
        assert_eq!(state(&worker), Some(ItemState::Synced));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn removing_everything_reaches_the_phone_and_a_later_empty_scan_is_still_held() {
        let (base, root, mut worker) = scratch_bucket("bucket-remove-all");
        fs::create_dir_all(root.join("Folder")).unwrap();
        fs::write(root.join("Folder/in.txt"), "i").unwrap();
        fs::write(root.join("a.txt"), "a").unwrap();
        worker.rescan(0);
        // Select All → Remove from Both.
        let all: Vec<String> = worker.pairs[0]
            .view(true)
            .items
            .iter()
            .filter(|item| !item.path.contains('/'))
            .map(|item| item.path.clone())
            .collect();
        assert_eq!(all.len(), 2);
        worker.handle(Message::RemoveItems {
            device: "phone-b".into(),
            paths: all,
        });
        assert!(
            worker.pairs[0].allow_empty,
            "a deliberate removal that empties the root"
        );
        worker.rescan(0);
        assert_ne!(worker.pairs[0].phase, Phase::EmptyHeld);
        assert!(worker.pairs[0]
            .book
            .manifest
            .values()
            .all(|entry| entry.deleted));
        assert!(!worker.pairs[0].allow_empty, "spent by that scan");
        // The same flag is spent even when the scan was not empty, so an
        // unmounted drive later is held.
        fs::write(root.join("b.txt"), "b").unwrap();
        worker.pairs[0].allow_empty = true;
        worker.rescan(0);
        assert!(!worker.pairs[0].allow_empty);
        // An unmounted drive: the folder is another directory now, empty.
        fs::remove_dir_all(&root).unwrap();
        fs::create_dir_all(&root).unwrap();
        worker.rescan(0);
        assert_eq!(worker.pairs[0].phase, Phase::EmptyHeld);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn removing_a_folder_whose_files_are_not_fetched_yet_keeps_them_from_coming_back() {
        let (base, root, mut worker) = scratch_bucket("bucket-remove-unfetched");
        fs::create_dir_all(root.join("A")).unwrap();
        let sha_a = "a".repeat(64);
        let packet = folders_packet(
            "manifest",
            json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
                {"path": "A", "dir": true, "vv": {"desk": 0, "phone": 1}},
                {"path": "A/x.txt", "size": 5, "mtime": 1, "sha256": sha_a,
                 "vv": {"desk": 0, "phone": 3}},
                {"path": "y.txt", "size": 5, "mtime": 1, "sha256": sha_a,
                 "vv": {"desk": 0, "phone": 2}},
            ]}),
        );
        worker.on_packet("phone-b", &packet);
        assert_eq!(
            worker.pairs[0].receiving.len(),
            2,
            "both asked for, not here"
        );
        let wanted = |path: &str| FolderRef {
            folder: BUCKET_ID.into(),
            relpath: path.into(),
            mtime: None,
        };
        assert!(accepts("phone-b", &wanted("A/x.txt"), &sha_a, 5));
        worker.handle(Message::RemoveItems {
            device: "phone-b".into(),
            paths: vec!["A".into(), "y.txt".into()],
        });
        let live = &worker.pairs[0];
        assert!(live.receiving.is_empty() && live.outstanding.is_empty());
        assert!(
            !accepts("phone-b", &wanted("A/x.txt"), &sha_a, 5),
            "no longer wanted"
        );
        let tomb = &live.book.manifest["A/x.txt"];
        assert!(
            tomb.deleted && tomb.vv == Vv { desk: 1, phone: 3 },
            "{tomb:?}"
        );
        assert!(live.book.manifest["y.txt"].deleted);
        assert!(live
            .view(true)
            .items
            .iter()
            .all(|item| item.state != ItemState::Receiving));
        // The phone's next manifest still lists them: ours dominates.
        let steps = plan(
            Side::Desk,
            &live.book.manifest,
            &{
                let mut theirs = Manifest::new();
                theirs.insert(
                    "A/x.txt".into(),
                    Entry {
                        vv: Vv { desk: 0, phone: 3 },
                        ..entry(&sha_a, (0, 3))
                    },
                );
                theirs
            },
            "P",
        );
        assert!(steps.is_empty(), "nothing comes back: {steps:?}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_future_mtime_is_not_held_for_ever() {
        let now = 10_000_000i64;
        let mut found: BTreeMap<String, Stat> = [
            (
                "future.txt".to_string(),
                Stat {
                    size: 1,
                    mtime: now + 3_600_000,
                    dir: false,
                },
            ),
            (
                "fresh.txt".to_string(),
                Stat {
                    size: 1,
                    mtime: now - 100,
                    dir: false,
                },
            ),
            (
                "exact.txt".to_string(),
                Stat {
                    size: 1,
                    mtime: now - 2_000,
                    dir: false,
                },
            ),
        ]
        .into();
        assert!(hold_unstable(&mut found, &Manifest::new(), now, 2_000));
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            ["exact.txt", "future.txt"],
            "only the fresh one waits"
        );
        let mut found: BTreeMap<String, Stat> = [(
            "future.txt".to_string(),
            Stat {
                size: 1,
                mtime: now + 1,
                dir: false,
            },
        )]
        .into();
        assert!(!hold_unstable(&mut found, &Manifest::new(), now, 2_000));
    }

    #[test]
    fn a_file_the_scan_skipped_or_never_reached_is_not_tombstoned() {
        let (base, root, mut worker) = scratch_bucket("bucket-keep");
        fs::write(root.join("grows.bin"), "small").unwrap();
        fs::write(root.join("stays.txt"), "s").unwrap();
        fs::write(root.join("other.txt"), "o").unwrap();
        worker.rescan(0);
        assert!(!worker.pairs[0].book.manifest["grows.bin"].deleted);
        // It grows past 1 GiB: skipped by the scan, listed, but not gone.
        fs::File::create(root.join("grows.bin"))
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        worker.rescan(0);
        let manifest = &worker.pairs[0].book.manifest;
        assert!(!manifest["grows.bin"].deleted, "the phone keeps its copy");
        assert!(worker.pairs[0]
            .skipped
            .iter()
            .any(|line| line.starts_with("grows.bin (")));
        // A scan cut short (past the cap) lists fewer files than exist.
        let mut found: BTreeMap<String, Stat> = BTreeMap::new();
        keep_existing(&mut found, manifest, &root);
        assert_eq!(
            found.keys().collect::<Vec<_>>(),
            ["grows.bin", "other.txt", "stays.txt"]
        );
        let mut remote = manifest.clone();
        let before = remote.clone();
        let hash = |_: &str| Some("h".to_string());
        assert!(!apply_scan(
            Side::Desk,
            &mut remote,
            &found,
            &mut |p| hash(p),
            now_ms()
        ));
        assert_eq!(remote, before, "nothing tombstoned by a truncated scan");
        // A really missing file is still a deletion.
        fs::remove_file(root.join("stays.txt")).unwrap();
        worker.rescan(0);
        assert!(worker.pairs[0].book.manifest["stays.txt"].deleted);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn reading_never_creates_folders() {
        let (base, root, mut worker) = scratch_bucket("bucket-readonly");
        assert_eq!(
            read_path(&root, "ghost/deep/x.txt").unwrap(),
            root.join("ghost/deep/x.txt")
        );
        assert!(!root.join("ghost").exists());
        std::os::unix::fs::symlink(&base, root.join("link")).unwrap();
        assert!(read_path(&root, "link/x").is_err() && read_path(&root, "../x").is_err());
        // A removal of a file in a folder that is not here: no folder made.
        let mut mine = entry("aaa", (1, 0));
        mine.size = 3;
        worker.pairs[0]
            .book
            .manifest
            .insert("ghost/x.txt".into(), mine);
        let packet = folders_packet(
            "manifest",
            json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
                {"path": "ghost/x.txt", "deleted": true, "vv": {"desk": 1, "phone": 1}},
            ]}),
        );
        worker.on_packet("phone-b", &packet);
        assert!(!root.join("ghost").exists(), "deleting made a folder");
        assert!(
            worker.pairs[0].book.manifest["ghost/x.txt"].deleted || worker.pairs[0].dirty.is_some()
        );
        // A request for a file that is not here makes none either.
        worker.pairs[0]
            .book
            .manifest
            .insert("nowhere/y.txt".into(), entry("bbb", (1, 0)));
        let request = folders_packet(
            "request",
            json!({"id": BUCKET_ID, "paths": ["nowhere/y.txt"]}),
        );
        worker.on_packet("phone-b", &request);
        assert!(!root.join("nowhere").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_manifest_keeps_every_live_entry_and_drops_the_oldest_removals_first() {
        let mut manifest = Manifest::new();
        let long = "\u{10348}".repeat(63);
        for index in 0..MAX_BUCKET_ITEMS {
            manifest.insert(
                format!("{index:05} {long}/{long}/{long}.bin"),
                entry(&"a".repeat(64), (3, 3)),
            );
        }
        for index in 0..MAX_BUCKET_ITEMS {
            let mut gone = tomb((4, 4));
            gone.mtime = index as i64;
            manifest.insert(format!("g{index:05} {long}/{long}/{long}.bin"), gone);
        }
        let packets = manifest_packets(BUCKET_ID, &manifest);
        assert!(packets.len() <= MAX_PARTS, "{}", packets.len());
        let sent: Vec<(String, bool)> = packets
            .iter()
            .flat_map(|packet| {
                packet.body["entries"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
            })
            .map(|value| {
                (
                    value["path"].as_str().unwrap().to_string(),
                    value["deleted"].as_bool().unwrap_or(false),
                )
            })
            .collect();
        assert_eq!(
            sent.iter().filter(|(_, gone)| !gone).count(),
            MAX_BUCKET_ITEMS,
            "no live entry lost"
        );
        assert!(sent.len() <= MAX_FILES);
        // The tombstones that fit are the newest.
        let first_gone = sent.iter().find(|(_, gone)| *gone).unwrap();
        assert!(
            first_gone
                .0
                .starts_with(&format!("g{:05}", MAX_BUCKET_ITEMS - 1)),
            "{}",
            &first_gone.0[..8]
        );
        for packet in &packets {
            assert!(crate::device_links::webrtc::fits_on_wire(packet));
        }
    }

    #[test]
    fn conflict_names_drop_what_another_system_refuses_and_measure_in_bytes() {
        let name = conflict_name("docs/plan.md", "Pixel: 9\\x", 0);
        assert_eq!(name, "docs/plan (conflict from Pixel 9x 1970-01-01).md");
        assert!(name_ok(name.rsplit('/').next().unwrap()));
        // 8 two-byte letters are 16 bytes: an extension; 9 are 18: not one.
        let ext8 = "é".repeat(8);
        let ext9 = "é".repeat(9);
        assert!(conflict_name(&format!("a.{ext8}"), "P", 0).ends_with(&format!(").{ext8}")));
        let long = conflict_name(&format!("a.{ext9}"), "P", 0);
        assert!(
            long.ends_with(")") && long.starts_with(&format!("a.{ext9} (conflict")),
            "{long}"
        );
        assert!(name_ok(&long));
    }

    #[test]
    fn a_send_that_could_not_start_frees_its_slot() {
        let (base, _root, mut worker) = scratch_bucket("bucket-sendfailed");
        worker.pairs[0]
            .flight
            .insert("a.txt".into(), Instant::now());
        worker.handle(Message::SendFailed {
            device: "phone-b".into(),
            path: "a.txt".into(),
        });
        assert!(worker.pairs[0].flight.is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_that_still_holds_something_stays() {
        let mut desk = Replica {
            side: Side::Desk,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut phone = Replica {
            side: Side::Phone,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        desk.disk.insert("A".into(), DIR.into());
        desk.scan(1);
        sync(&mut phone, &desk);
        phone.scan(2);
        // The phone removes it while the desk (unsynced) put a file in.
        phone.disk.remove("A");
        phone.scan(3);
        desk.disk.insert("A/new.txt".into(), "keep me".into());
        desk.scan(3);
        for round in 4..8 {
            sync(&mut desk, &phone);
            desk.scan(round);
            sync(&mut phone, &desk);
            phone.scan(round);
        }
        assert_eq!(
            desk.disk.get("A/new.txt").map(String::as_str),
            Some("keep me")
        );
        assert_eq!(
            desk.disk, phone.disk,
            "the phone got the folder and the file back"
        );
    }

    #[test]
    fn both_sides_creating_the_same_folder_merge() {
        let mut desk = Replica {
            side: Side::Desk,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut phone = Replica {
            side: Side::Phone,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        desk.disk.insert("Trips".into(), DIR.into());
        phone.disk.insert("Trips".into(), DIR.into());
        desk.scan(1);
        phone.scan(1);
        let steps = plan(Side::Desk, &desk.manifest, &phone.manifest, "P");
        assert!(
            matches!(steps.as_slice(), [Step::Take { .. }]),
            "one folder, no conflict: {steps:?}"
        );
        sync(&mut desk, &phone);
        sync(&mut phone, &desk);
        assert_eq!(desk.disk, phone.disk);
        assert_eq!(desk.disk.len(), 1);
    }

    #[test]
    fn a_case_only_rename_is_a_rename_and_no_decision_reads_the_clock() {
        let mut local = Manifest::new();
        local.insert("a.txt".into(), entry("aaa", (1, 0)));
        let mut remote = Manifest::new();
        remote.insert("a.txt".into(), tomb((2, 0)));
        remote.insert("A.txt".into(), entry("aaa", (2, 0)));
        let steps = plan(Side::Phone, &local, &remote, "P");
        assert!(
            steps.iter().any(|step| matches!(step, Step::Move { from, to, .. } if from == "a.txt" && to == "A.txt")),
            "{steps:?}"
        );
        // Clock skew: mtimes a year apart change nothing.
        let mut skewed = remote.clone();
        for entry in skewed.values_mut() {
            entry.mtime += 365 * 24 * 3600 * 1000;
        }
        assert_eq!(
            plan(Side::Phone, &local, &remote, "P").len(),
            plan(Side::Phone, &local, &skewed, "P").len()
        );
        // Same content on both, concurrent: nothing is sent again.
        let mut ours = Manifest::new();
        ours.insert("p.txt".into(), entry("same", (2, 0)));
        let mut theirs = Manifest::new();
        theirs.insert("p.txt".into(), entry("same", (0, 3)));
        let steps = plan(Side::Desk, &ours, &theirs, "P");
        assert!(matches!(steps.as_slice(), [Step::Take { .. }]), "{steps:?}");
    }

    #[test]
    fn an_empty_folder_from_the_phone_is_created_here() {
        let (base, root, mut worker) = scratch_bucket("bucket-dir-in");
        let packet = folders_packet(
            "manifest",
            json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
                {"path": "Trips", "dir": true, "vv": {"desk": 0, "phone": 1}},
                {"path": "Trips/Rome", "dir": true, "vv": {"desk": 0, "phone": 1}},
            ]}),
        );
        worker.on_packet("phone-b", &packet);
        assert!(root.join("Trips/Rome").is_dir());
        assert_eq!(worker.pairs[0].phase, Phase::UpToDate, "nothing to fetch");
        let items = worker.pairs[0].view(true).items;
        assert!(items.iter().all(|item| item.dir));
        // A phone-side removal of an empty folder removes it here.
        let removal = folders_packet(
            "manifest",
            json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
                {"path": "Trips", "dir": true, "vv": {"desk": 0, "phone": 1}},
                {"path": "Trips/Rome", "dir": true, "deleted": true, "vv": {"desk": 0, "phone": 2}},
            ]}),
        );
        worker.on_packet("phone-b", &removal);
        assert!(root.join("Trips").is_dir() && !root.join("Trips/Rome").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folder_removed_on_the_phone_goes_to_the_trash_as_one_item() {
        let (base, root, mut worker) = scratch_bucket("bucket-remote-trash");
        fs::create_dir_all(root.join("A/B")).unwrap();
        fs::write(root.join("A/one.txt"), "1").unwrap();
        fs::write(root.join("A/B/two.txt"), "2").unwrap();
        worker.rescan(0);
        let manifest = worker.pairs[0].book.manifest.clone();
        assert_eq!(manifest.len(), 4);
        let gone = |path: &str| {
            let mut entry = manifest[path].clone();
            let (size, mtime) = (0, entry.mtime);
            entry.deleted = true;
            entry.size = size;
            entry.mtime = mtime;
            entry.sha256.clear();
            entry.vv.phone = entry.vv.desk + 1;
            json!({"path": path, "dir": entry.dir, "deleted": true,
                   "vv": {"desk": entry.vv.desk, "phone": entry.vv.phone}})
        };
        let packet = folders_packet(
            "manifest",
            json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
                gone("A"), gone("A/B"), gone("A/one.txt"), gone("A/B/two.txt"),
            ]}),
        );
        let before = trashed();
        worker.on_packet("phone-b", &packet);
        assert_eq!(trashed() - before, 1, "the folder, once");
        assert!(!root.join("A").exists());
        assert!(worker.pairs[0]
            .book
            .manifest
            .values()
            .all(|entry| entry.deleted));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn removing_a_folder_here_is_one_trash_item_and_cancels_its_transfers() {
        let (base, root, mut worker) = scratch_bucket("bucket-remove");
        fs::create_dir_all(root.join("A/B")).unwrap();
        fs::write(root.join("A/one.txt"), "1").unwrap();
        fs::write(root.join("A/B/two.txt"), "2").unwrap();
        fs::write(root.join("keep.txt"), "k").unwrap();
        worker.rescan(0);
        {
            let live = &mut worker.pairs[0];
            live.send_queue.push_back("A/one.txt".into());
            live.flight.insert("A/B/two.txt".into(), Instant::now());
            live.outstanding.push_back("A/B/three.txt".into());
            live.to_request.push_back("A/four.txt".into());
            live.send_queue.push_back("keep.txt".into());
        }
        let before = trashed();
        worker.handle(Message::RemoveItems {
            device: "phone-b".into(),
            paths: vec!["A".into()],
        });
        assert_eq!(trashed() - before, 1, "one item for the whole folder");
        assert!(!root.join("A").exists() && root.join("keep.txt").exists());
        let live = &worker.pairs[0];
        assert_eq!(live.send_queue, ["keep.txt"], "its transfers are dropped");
        assert!(
            live.flight.is_empty() && live.outstanding.is_empty() && live.to_request.is_empty()
        );
        // The next scan tombstones all four, so the phone removes them.
        worker.rescan(0);
        let manifest = &worker.pairs[0].book.manifest;
        assert!(["A", "A/B", "A/one.txt", "A/B/two.txt"]
            .iter()
            .all(|path| manifest[*path].deleted));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn bulk_remove_trashes_every_selected_item() {
        let (base, root, mut worker) = scratch_bucket("bucket-bulk");
        fs::create_dir_all(root.join("Folder")).unwrap();
        fs::write(root.join("Folder/in.txt"), "i").unwrap();
        for name in ["a.txt", "b.txt", "c.txt"] {
            fs::write(root.join(name), name).unwrap();
        }
        worker.rescan(0);
        let before = trashed();
        worker.handle(Message::RemoveItems {
            device: "phone-b".into(),
            paths: vec!["a.txt".into(), "Folder".into(), "c.txt".into()],
        });
        assert_eq!(trashed() - before, 3);
        assert!(root.join("b.txt").exists());
        assert!(!root.join("a.txt").exists() && !root.join("c.txt").exists());
        assert!(!root.join("Folder").exists());
        worker.rescan(0);
        let live: Vec<&String> = worker.pairs[0]
            .book
            .manifest
            .iter()
            .filter(|(_, entry)| !entry.deleted)
            .map(|(path, _)| path)
            .collect();
        assert_eq!(live, ["b.txt"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_over_1_gb_is_refused_with_the_documented_words() {
        let (base, root, worker) = scratch_bucket("bucket-big");
        let big = base.join("big.bin");
        fs::File::create(&big)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let exact = base.join("exact.bin");
        fs::File::create(&exact)
            .unwrap()
            .set_len(MAX_FILE_BYTES)
            .unwrap();
        let mut report = Report::default();
        let adds = collect(
            &[big.clone(), exact.clone()],
            &root,
            "",
            (0, 0),
            &mut report,
        );
        assert_eq!(
            adds.iter().map(|add| add.rel.as_str()).collect::<Vec<_>>(),
            ["exact.bin"]
        );
        assert_eq!(report.too_big, 1);
        assert_eq!(
            report.message().unwrap(),
            "Files over 1 GiB can't go in the Bucket"
        );
        // In the Bucket folder from outside, and from the phone.
        fs::copy(&big, root.join("huge.bin")).unwrap();
        let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
        assert!(!found.contains_key("huge.bin"));
        assert_eq!(skipped, ["huge.bin (over 1 GiB)"]);
        let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
        packet.set(
            "entries",
            json!([{"path": "p.bin", "size": MAX_FILE_BYTES + 1, "mtime": 1,
                    "sha256": "a".repeat(64), "vv": {"desk": 0, "phone": 1}}]),
        );
        assert!(manifest_entries(&packet, &worker.pairs[0].pair).is_empty());
        let folder = FolderRef {
            folder: BUCKET_ID.into(),
            relpath: "p.bin".into(),
            mtime: None,
        };
        test_want("phone-b", &folder, &"a".repeat(64), MAX_FILE_BYTES + 1);
        assert!(!accepts(
            "phone-b",
            &folder,
            &"a".repeat(64),
            MAX_FILE_BYTES + 1
        ));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_add_is_limited_in_items_and_bytes_and_the_bucket_is_full_at_10_gb() {
        let base = scratch("bucket-add-limits");
        let root = base.join("bucket");
        fs::create_dir_all(&root).unwrap();
        let outside = base.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let many: Vec<PathBuf> = (0..MAX_ADD_ITEMS + 3)
            .map(|index| {
                let path = outside.join(format!("f{index:04}.txt"));
                fs::write(&path, "x").unwrap();
                path
            })
            .collect();
        let mut report = Report::default();
        let adds = collect(&many, &root, "", (0, 0), &mut report);
        assert_eq!((adds.len(), report.over_items), (MAX_ADD_ITEMS, 3));
        assert!(report
            .message()
            .unwrap()
            .contains("Only 500 items can be added at once; 3 were left out"));
        // 5 GiB at once: six 1 GiB files, the sixth left out.
        let big: Vec<PathBuf> = (0..6)
            .map(|index| {
                let path = outside.join(format!("big{index}.bin"));
                fs::File::create(&path)
                    .unwrap()
                    .set_len(MAX_FILE_BYTES)
                    .unwrap();
                path
            })
            .collect();
        let mut report = Report::default();
        let adds = collect(&big, &root, "", (0, 0), &mut report);
        assert_eq!((adds.len(), report.over_bytes), (5, 1));
        // The Bucket would pass 10 GiB.
        let mut report = Report::default();
        let adds = collect(
            &big,
            &root,
            "",
            (BUCKET_FULL_BYTES - 2 * MAX_FILE_BYTES, 10),
            &mut report,
        );
        assert_eq!(adds.len(), 2);
        assert!(report.full);
        assert!(report
            .message()
            .unwrap()
            .contains("The Bucket is full (10 GiB)"));
        // The item cap of a Bucket.
        let mut report = Report::default();
        let adds = collect(
            &many[..5],
            &root,
            "",
            (0, MAX_BUCKET_ITEMS - 2),
            &mut report,
        );
        assert_eq!((adds.len(), report.over_items), (2, 3));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn adding_a_folder_copies_what_is_in_it_within_the_depth() {
        let (base, root, mut worker) = scratch_bucket("bucket-add-folder");
        let outside = base.join("Holiday");
        fs::create_dir_all(outside.join("Day 1/Raw")).unwrap();
        fs::create_dir_all(outside.join("Empty")).unwrap();
        fs::write(outside.join("Day 1/a.jpg"), "a").unwrap();
        fs::write(outside.join("Day 1/Raw/r.cr2"), "r").unwrap();
        std::os::unix::fs::symlink(outside.join("Day 1/a.jpg"), outside.join("link.jpg")).unwrap();
        worker.handle(Message::Import {
            device: "phone-b".into(),
            sources: vec![outside.clone()],
            into: String::new(),
        });
        assert_eq!(
            fs::read_to_string(root.join("Holiday/Day 1/a.jpg")).unwrap(),
            "a"
        );
        assert!(
            root.join("Holiday/Empty").is_dir(),
            "empty folders are copied"
        );
        assert!(
            !root.join("Holiday/Day 1/Raw").exists(),
            "level 3 is left out"
        );
        assert!(!root.join("Holiday/link.jpg").exists(), "links are skipped");
        let message = take_report().expect("what was left out is said");
        assert!(message.contains("left out (2)"), "{message}");
        // Again: the name is taken, so it is a second folder.
        worker.handle(Message::Import {
            device: "phone-b".into(),
            sources: vec![outside],
            into: String::new(),
        });
        assert!(root.join("Holiday (2)/Empty").is_dir());
        let _ = take_report();
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn new_folder_is_refused_at_level_two_and_names_stay_unique() {
        let (base, root, mut worker) = scratch_bucket("bucket-new-folder");
        worker.handle(Message::NewFolder {
            device: "phone-b".into(),
            parent: String::new(),
        });
        worker.handle(Message::NewFolder {
            device: "phone-b".into(),
            parent: String::new(),
        });
        assert!(root.join("New Folder").is_dir() && root.join("New Folder (2)").is_dir());
        worker.handle(Message::NewFolder {
            device: "phone-b".into(),
            parent: "New Folder".into(),
        });
        assert!(
            root.join("New Folder/New Folder").is_dir(),
            "level 2 is allowed"
        );
        let _ = take_report();
        worker.handle(Message::NewFolder {
            device: "phone-b".into(),
            parent: "New Folder/New Folder".into(),
        });
        assert!(!root.join("New Folder/New Folder/New Folder").exists());
        assert_eq!(
            take_report().unwrap(),
            "Folders can only go 2 levels deep in the Bucket"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn moving_into_a_folder_that_would_be_too_deep_is_refused_per_item() {
        let (base, root, mut worker) = scratch_bucket("bucket-move");
        fs::create_dir_all(root.join("A/B")).unwrap();
        fs::create_dir_all(root.join("C/D")).unwrap();
        fs::create_dir_all(root.join("E")).unwrap();
        fs::write(root.join("f.txt"), "f").unwrap();
        worker.rescan(0);
        let _ = take_report();
        worker.handle(Message::MoveItems {
            device: "phone-b".into(),
            paths: vec!["f.txt".into(), "E".into(), "C".into()],
            dest: "A/B".into(),
        });
        assert!(root.join("A/B/f.txt").exists(), "a file fits at level 2");
        assert!(!root.join("A/B/E").exists(), "a folder would be level 3");
        assert!(
            root.join("E").is_dir() && root.join("C/D").is_dir(),
            "left where they are"
        );
        assert_eq!(
            take_report().unwrap(),
            "2 items couldn't be moved: they would be more than 2 folders deep"
        );
        // Into a level-1 folder, a folder with no subfolders fits; one with them does not.
        worker.handle(Message::MoveItems {
            device: "phone-b".into(),
            paths: vec!["E".into(), "C".into()],
            dest: "A".into(),
        });
        assert!(root.join("A/E").is_dir());
        assert!(root.join("C/D").is_dir(), "C holds a folder: level 3");
        assert_eq!(
            take_report().unwrap(),
            "1 item couldn't be moved: they would be more than 2 folders deep"
        );
        // Not into itself.
        worker.handle(Message::MoveItems {
            device: "phone-b".into(),
            paths: vec!["A".into()],
            dest: "A/B".into(),
        });
        assert_eq!(take_report().unwrap(), "A folder can't go inside itself");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn one_transfer_is_in_flight_per_direction_and_the_rest_wait() {
        // Sending: the queue fills the slot; an ended transfer frees it.
        let mut queue: VecDeque<String> = (1..=5).map(|n| format!("f{n}")).collect();
        let mut flight = HashMap::new();
        let now = Instant::now();
        assert_eq!(due_sends(&mut queue, &mut flight, &[], now), ["f1"]);
        assert!(
            due_sends(&mut queue, &mut flight, &[], now).is_empty(),
            "full"
        );
        assert_eq!(
            due_sends(&mut queue, &mut flight, &["f1".into()], now),
            ["f2"]
        );
        assert_eq!(
            due_sends(&mut queue, &mut flight, &["f2".into()], now),
            ["f3"]
        );
        assert_eq!(flight.len(), MAX_IN_FLIGHT);
        // A transfer that never showed lapses, so a lost one cannot hold a slot.
        let later = now + FLIGHT_LAPSES + Duration::from_secs(1);
        queue.push_back("f6".into());
        assert_eq!(due_sends(&mut queue, &mut flight, &[], later), ["f4"]);
        // Receiving: only one path is asked for at a time.
        let (base, _root, mut worker) = scratch_bucket("bucket-flight");
        worker.pairs[0].to_request = (1..=6).map(|n| format!("r{n}")).collect();
        worker.request_more(0);
        assert_eq!(worker.pairs[0].outstanding.len(), MAX_IN_FLIGHT);
        assert_eq!(worker.pairs[0].to_request.len(), 5, "the rest are Waiting…");
        worker.request_more(0);
        assert_eq!(worker.pairs[0].outstanding.len(), MAX_IN_FLIGHT);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_still_being_written_waits_until_it_has_held_for_two_seconds() {
        let now = 10_000_000i64;
        let fresh = Stat {
            size: 5,
            mtime: now - 500,
            dir: false,
        };
        let old = Stat {
            size: 5,
            mtime: now - 5_000,
            dir: false,
        };
        let mut found: BTreeMap<String, Stat> = [
            ("download.part2".to_string(), fresh),
            ("settled.txt".to_string(), old),
            ("edited.txt".to_string(), fresh),
            (
                "folder".to_string(),
                Stat {
                    size: 0,
                    mtime: 0,
                    dir: true,
                },
            ),
        ]
        .into();
        let mut manifest = Manifest::new();
        manifest.insert(
            "edited.txt".into(),
            Entry {
                mtime: now - 90_000,
                size: 4,
                ..entry("old", (1, 0))
            },
        );
        assert!(hold_unstable(&mut found, &manifest, now, 2_000));
        assert!(
            !found.contains_key("download.part2"),
            "new and unstable: not yet"
        );
        assert!(found.contains_key("settled.txt") && found.contains_key("folder"));
        assert_eq!(
            found["edited.txt"].mtime,
            now - 90_000,
            "held at the old version"
        );
        // Two seconds later it is stable.
        let mut found: BTreeMap<String, Stat> = [("download.part2".to_string(), fresh)].into();
        assert!(!hold_unstable(&mut found, &manifest, now + 2_000, 2_000));
        assert!(found.contains_key("download.part2"));
        // The worker rescans soon rather than waiting a minute.
        let (base, root, mut worker) = scratch_bucket("bucket-stable");
        TEST_STABLE_MS.with(|window| window.set(STABLE_FOR.as_millis() as i64));
        fs::write(root.join("new.bin"), "being written").unwrap();
        worker.rescan(0);
        assert!(
            worker.pairs[0].book.manifest.is_empty(),
            "too fresh to sync"
        );
        assert!(worker.pairs[0].dirty.is_some(), "scanned again soon");
        TEST_STABLE_MS.with(|window| window.set(0));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_changed_after_it_was_asked_for_is_not_sent_and_is_scanned_again() {
        let (base, root, mut worker) = scratch_bucket("bucket-changed");
        fs::write(root.join("doc.txt"), "one").unwrap();
        std::thread::sleep(Duration::from_millis(5));
        let old = Duration::from_secs(10);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(root.join("doc.txt"))
            .unwrap();
        file.set_modified(SystemTime::now() - old).unwrap();
        worker.rescan(0);
        assert!(worker.pairs[0].book.manifest.contains_key("doc.txt"));
        fs::write(root.join("doc.txt"), "two, longer").unwrap();
        worker.pairs[0].dirty = None;
        worker.pairs[0].send_queue.push_back("doc.txt".into());
        worker.pump_sends(0, &[]);
        let live = &worker.pairs[0];
        assert!(live.flight.is_empty(), "not sent");
        assert!(
            live.dirty.is_some(),
            "scanned again, so the new version is asked for"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn turning_the_bucket_off_keeps_every_file_and_on_again_resends_nothing_identical() {
        let (base, root, mut worker) = scratch_bucket("bucket-off-on");
        fs::create_dir_all(root.join("A")).unwrap();
        fs::write(root.join("A/x.txt"), "x").unwrap();
        worker.handle(Message::Remove {
            device: "phone-b".into(),
            id: BUCKET_ID.into(),
        });
        assert!(root.join("A/x.txt").exists(), "files stay");
        assert!(worker.pairs.is_empty());
        // Both sides hold the same file under unrelated version counters:
        // taken as is, no transfer.
        let mut desk = Manifest::new();
        desk.insert("x.txt".into(), entry("same", (1, 0)));
        let mut phone = Manifest::new();
        phone.insert("x.txt".into(), entry("same", (0, 4)));
        let steps = plan(Side::Desk, &desk, &phone, "P");
        assert!(
            steps.iter().all(|step| matches!(step, Step::Take { .. })),
            "{steps:?}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_folders_tile_adds_up_and_its_badge_rolls_up() {
        let item = |path: &str, size: u64, state: ItemState, dir: bool| Item {
            dir,
            path: path.into(),
            size,
            mtime: 0,
            from: Side::Desk,
            state,
        };
        let items = vec![
            item("A", 0, ItemState::Synced, true),
            item("A/B", 0, ItemState::Synced, true),
            item("A/B/x", 100, ItemState::Receiving, false),
            item("A/y", 20, ItemState::Synced, false),
            item("Ab", 7, ItemState::Synced, false),
        ];
        assert_eq!(totals(&items, "A"), (3, 120), "A/B, A/B/x, A/y; not Ab");
        assert_eq!(totals(&items, "A/B"), (1, 100));
        assert_eq!(parent_of("A/B/x"), "A/B");
        assert_eq!(parent_of("x"), "");
        use ItemState::*;
        assert_eq!(rollup([Synced, Synced]), Synced);
        assert_eq!(rollup([Synced, Waiting]), Waiting);
        assert_eq!(rollup([Waiting, Sending]), Sending);
        assert_eq!(rollup([Sending, Receiving, Waiting]), Receiving);
        assert_eq!(rollup([Receiving, Conflict, Synced]), Conflict);
        assert_eq!(rollup([]), Synced);
    }

    /// S3: hidden/.git/.tmp items are left out at add time and counted; a
    /// name differing only by case counts as taken (the phone's file system
    /// may fold case).
    /// C10: a file removed here that the phone changed comes back, and the
    /// person is told which.
    #[test]
    fn a_file_that_comes_back_because_the_phone_changed_it_is_reported() {
        let (base, _root, mut worker) = scratch_bucket("bucket-restored");
        let name = format!("c10-{}.txt", uuid::Uuid::new_v4().simple());
        worker.pairs[0].book.manifest.insert(
            name.clone(),
            Entry {
                deleted: true,
                sha256: String::new(),
                size: 0,
                dir: false,
                mtime: 1,
                vv: Vv { desk: 2, phone: 0 },
            },
        );
        let packet = folders_packet(
            "manifest",
            json!({"id": BUCKET_ID, "part": 1, "parts": 1, "entries": [
                {"path": name, "size": 5, "mtime": 2, "sha256": "a".repeat(64),
                 "vv": {"desk": 1, "phone": 1}},
            ]}),
        );
        worker.on_packet("phone-b", &packet);
        let told: Vec<String> = with_reports(|reports| reports.iter().cloned().collect()).unwrap();
        assert!(
            told.iter()
                .any(|message| message.contains(&format!("{name} was restored"))),
            "{told:?}"
        );
        let _ = fs::remove_dir_all(&base);
    }

    /// S4: "Keep the phone's" trashes this computer's version first.
    #[test]
    fn keeping_the_phones_version_trashes_the_desks_first() {
        let (base, root, mut worker) = scratch_bucket("bucket-keep-phone");
        fs::write(root.join("a.txt"), "desk").unwrap();
        fs::write(root.join("a (conflicted copy).txt"), "phone").unwrap();
        worker.pairs[0].book.conflicts.push(Conflict {
            path: "a.txt".into(),
            copy: "a (conflicted copy).txt".into(),
            at: 0,
        });
        let before = trashed();
        worker.resolve("phone-b", BUCKET_ID, "a.txt", Resolve::Phone);
        assert_eq!(
            trashed(),
            before + 1,
            "the desk's version went to the trash"
        );
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "phone");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn an_add_skips_what_never_syncs_and_names_ignore_case() {
        let base = scratch("add-hidden");
        let source = base.join("src");
        fs::create_dir_all(source.join(".git")).unwrap();
        fs::write(source.join(".git/config"), "x").unwrap();
        fs::write(source.join(".hidden"), "x").unwrap();
        fs::write(source.join("scratch.tmp"), "x").unwrap();
        fs::write(source.join("keep.txt"), "x").unwrap();
        let root = base.join("bucket");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("Photo.JPG"), "x").unwrap();
        let mut report = Report::default();
        let adds = collect(
            std::slice::from_ref(&source),
            &root,
            "",
            (0, 0),
            &mut report,
        );
        let rels: Vec<&str> = adds.iter().map(|add| add.rel.as_str()).collect();
        assert!(rels.contains(&"src/keep.txt"), "{rels:?}");
        assert!(!rels
            .iter()
            .any(|rel| rel.contains(".git") || rel.contains(".hidden")));
        assert_eq!(report.hidden, 3, ".git, .hidden and scratch.tmp");
        let taken = HashSet::new();
        let unique = unique_among(&root, "photo.jpg", &taken);
        assert_eq!(unique.file_name().unwrap(), "photo (2).jpg");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn what_an_add_left_out_is_said_plainly() {
        let report = Report {
            added: 3,
            too_big: 2,
            too_deep: 4,
            bad_names: 1,
            full: true,
            over_items: 5,
            over_bytes: 1,
            not_moved: 2,
            hidden: 6,
            failed: 2,
            restored: vec!["a.txt".into(), "b.txt".into()],
            refused: Some("No.".into()),
        };
        let message = report.message().unwrap();
        for part in [
            "Added 3 items to the Bucket",
            "No.",
            "Files over 1 GiB can't go in the Bucket (2)",
            "Some items were more than 2 folders deep and were left out (4)",
            "Some names can't sync, so they were left out (1)",
            "The Bucket is full (10 GiB)",
            "Only 500 items can be added at once; 5 were left out",
            "Only 5 GiB can be added at once; 1 were left out",
            "2 items couldn't be moved",
            "Hidden and temporary items don't sync, so they were left out (6)",
            "2 items couldn't be copied",
            "a.txt and 1 more were restored",
        ] {
            assert!(message.contains(part), "{part}: {message}");
        }
        assert_eq!(Report::default().message(), None);
        assert_eq!(
            Report {
                added: 1,
                ..Report::default()
            }
            .message()
            .unwrap(),
            "Added 1 item to the Bucket"
        );
    }

    #[test]
    fn pasted_text_and_links_become_files() {
        let now = gtk::glib::DateTime::from_local(2026, 10, 22, 14, 5, 9.0).unwrap();
        let (name, bytes) = pasted_text("https://github.com/derpx06/desklink?tab=1", &now);
        assert_eq!(name, "github.com 2026-10-22.url");
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "[Desktop Entry]\nType=Link\nName=github.com\nURL=https://github.com/derpx06/desklink?tab=1\n"
        );
        let (name, bytes) = pasted_text("Door code 4471\nBring the keys", &now);
        assert_eq!(name, "Text 2026-10-22 14.05.09.txt");
        assert_eq!(bytes, b"Door code 4471\nBring the keys");
        // Userinfo, ports and IPv6 never leave a name another system refuses.
        for (link, host) in [
            ("https://user:pw@example.com:8080/x", "example.com"),
            ("http://[::1]:3000/", "1"),
            ("http://[2001:db8::7]/p", "2001-db8--7"),
            ("http://192.168.1.2:8000", "192.168.1.2"),
            ("https://@/x", "Link"),
        ] {
            let (name, _) = pasted_text(link, &now);
            assert_eq!(name, format!("{host} 2026-10-22.url"), "{link}");
            assert!(name_ok(&name), "{name}");
        }
        let (name, _) = pasted_text("see https://a.b and more", &now);
        assert!(name.ends_with(".txt"), "a sentence with a link is text");
        assert!(import_bytes("p", "Text.txt", vec![b'x'; MAX_TEXT_ITEM_BYTES + 1], "").is_err());
        assert!(import_bytes("p", "Text.txt", Vec::new(), "").is_err());
        // Refused now, not after a success toast: a name the scan would drop.
        assert_eq!(
            import_bytes("p", "a:b.url", b"x".to_vec(), "").unwrap_err(),
            "That name cannot be used"
        );
        assert!(import_bytes("p", "ok.url", b"x".to_vec(), "A/B/C").is_err());
        assert!(import_bytes("p", "ok.txt", b"x".to_vec(), "").is_ok());
        assert_eq!(clean_name("a:b\\c. ", "x"), "a-b-c");
        assert_eq!(clean_name("...", "x"), "x");
    }

    #[test]
    fn the_workers_refusals_say_what_is_wrong_not_full() {
        let (base, root, mut worker) = scratch_bucket("bucket-bytes-refusal");
        let refuse = |worker: &mut Worker, name: &str| {
            worker.import_bytes("phone-b", name, b"x".to_vec(), "");
            take_report()
        };
        assert_eq!(
            refuse(&mut worker, "a:b.txt").unwrap(),
            "That name cannot be used"
        );
        assert!(!root.join("a:b.txt").exists());
        assert!(refuse(&mut worker, "fine.txt").is_none());
        assert!(root.join("fine.txt").exists());
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn each_phone_gets_a_sibling_bucket_never_nested() {
        let home = Path::new("/home/me");
        let first = bucket_root(home, "Pixel 9", &[]);
        assert_eq!(first, home.join("DeskLink Bucket"));
        let second = bucket_root(home, "realme/14", std::slice::from_ref(&first));
        assert_eq!(second, home.join("DeskLink Bucket (realme 14)"));
        let third = bucket_root(home, "realme/14", &[first.clone(), second.clone()]);
        assert_eq!(third, home.join("DeskLink Bucket (realme 14 2)"));
        // A pair inside it, or around it, makes it taken too.
        let around = bucket_root(home, "P", &[home.to_path_buf()]);
        assert_ne!(around, home.join("DeskLink Bucket"));
        assert_eq!(
            unique_in(Path::new("/nonexistent"), "a.txt"),
            Path::new("/nonexistent/a.txt")
        );
    }

    #[test]
    fn the_bucket_is_bookmarked_and_unbookmarked() {
        let base = scratch("bookmarks");
        let file = base.join("gtk-3.0").join("bookmarks");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "file:///home/me/Work Work\n").unwrap();
        TEST_BOOKMARKS.with(|path| path.replace(Some(file.clone())));
        let bucket = Path::new("/home/me/DeskLink Bucket");
        bookmark(bucket, true);
        bookmark(bucket, true);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "file:///home/me/Work Work\nfile:///home/me/DeskLink%20Bucket DeskLink Bucket\n",
            "added once, other lines untouched"
        );
        bookmark(bucket, false);
        assert_eq!(
            fs::read_to_string(&file).unwrap(),
            "file:///home/me/Work Work\n"
        );
        TEST_BOOKMARKS.with(|path| path.replace(None));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn turning_the_bucket_on_makes_its_folder_and_old_pairs_are_dropped() {
        let base = scratch("bucket-on");
        TEST_HOME.with(|home| home.replace(Some(base.clone())));
        TEST_BOOKMARKS.with(|path| path.replace(Some(base.join("bookmarks"))));
        let mut worker = bucket_worker(&base, &base.join("unused"));
        worker.pairs.clear();
        worker.dropped = 2;
        worker.handle(Message::TurnOnBucket {
            device: "phone-b".into(),
        });
        assert!(base.join("DeskLink Bucket").is_dir());
        assert_eq!(worker.dropped, 0, "the note goes once a Bucket is on");
        assert!(fs::read_to_string(base.join("bookmarks"))
            .unwrap()
            .contains("DeskLink%20Bucket"));
        // The phone accepts: the pair is a Bucket.
        worker.on_packet(
            "phone-b",
            &folders_packet("accept", json!({"id": BUCKET_ID})),
        );
        assert!(worker.pairs[0].pair.bucket);
        // A phone's folder offer (not a Bucket) is declined, never listed.
        worker.on_packet(
            "phone-b",
            &folders_packet("offer", json!({"id": "x1", "name": "Camera"})),
        );
        assert!(worker.offered.is_empty());
        // Stored pairs: old folder pairs stop, the Bucket stays.
        let old = Pair {
            id: "old".into(),
            bucket: false,
            ..worker.pairs[0].pair.clone()
        };
        let stored = Pairs {
            version: SCHEMA,
            pairs: vec![old, worker.pairs[0].pair.clone()],
            dropped: 0,
        };
        let (kept, dropped, changed) = keep_buckets(&worker.dir, stored);
        assert_eq!((kept.len(), dropped, changed), (1, 1, true));
        TEST_HOME.with(|home| home.replace(None));
        TEST_BOOKMARKS.with(|path| path.replace(None));
        let _ = fs::remove_dir_all(&base);
    }

    fn test_pair() -> Pair {
        Pair {
            id: "pair-t".into(),
            name: "t".into(),
            root: PathBuf::from("/nonexistent"),
            device: "phone-t".into(),
            include_hidden: false,
            ignore: vec!["*.log".into()],
            bucket: false,
        }
    }

    #[test]
    fn a_phone_cannot_send_what_this_pair_ignores() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
        let entry = |path: &str| {
            json!({"path": path, "size": 1, "mtime": 1, "sha256": "a".repeat(64),
                   "vv": {"desk": 0, "phone": 1}})
        };
        packet.set(
            "entries",
            json!([
                entry(".git/hooks/pre-commit"),
                entry("src/.env"),
                entry("build.log"),
                entry("node_modules/x.js"),
                entry("docs/readme.md"),
            ]),
        );
        let kept: Vec<String> = manifest_entries(&packet, &test_pair())
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(kept, ["docs/readme.md"]);
        let with_hidden = Pair {
            include_hidden: true,
            ..test_pair()
        };
        assert_eq!(
            manifest_entries(&packet, &with_hidden).len(),
            2,
            ".env too, never .git"
        );
    }

    fn worker_for(base: &Path, root: &Path) -> Worker {
        let pair = Pair {
            id: "pair-h".into(),
            name: "root".into(),
            root: root.to_path_buf(),
            device: "phone-h".into(),
            include_hidden: false,
            ignore: Vec::new(),
            bucket: false,
        };
        let worker = Worker {
            dir: base.join("state"),
            pairs: vec![Live::new(pair, Book::default())],
            ready: ["phone-h".to_string()].into(),
            asked: HashMap::new(),
            offered: Vec::new(),
            dirty: Default::default(),
            dropped: 0,
        };
        fs::create_dir_all(&worker.dir).unwrap();
        worker
    }

    /// Deleting the last file with `rm` is deliberate: the folder is the
    /// same directory it was, so the deletion goes to the phone.
    #[test]
    fn removing_the_last_file_with_the_folder_still_there_is_applied() {
        let base = std::env::temp_dir().join(format!("desklink-rm-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        fs::write(root.join("b.txt"), "two").unwrap();
        let mut worker = worker_for(&base, &root);
        worker.rescan(0);
        // One at a time, then everything gone in one scan too.
        fs::remove_file(root.join("a.txt")).unwrap();
        worker.rescan(0);
        assert_ne!(worker.pairs[0].phase, Phase::EmptyHeld);
        fs::remove_file(root.join("b.txt")).unwrap();
        worker.rescan(0);
        assert_ne!(worker.pairs[0].phase, Phase::EmptyHeld);
        assert!(worker.pairs[0].book.manifest["b.txt"].deleted, "tombstoned");
        assert!(worker.pairs[0].book.manifest["a.txt"].deleted);
        let _ = fs::remove_dir_all(&base);
    }

    /// A folder that is gone, unreadable, or swapped for another is not a
    /// deliberate removal: nothing is tombstoned.
    #[test]
    fn a_missing_unreadable_or_replaced_folder_is_not_a_deliberate_removal() {
        use std::os::unix::fs::PermissionsExt;
        if !crate::test_privileges::unprivileged(
            "device_links::folders::tests::a_missing_unreadable_or_replaced_folder_is_not_a_deliberate_removal",
        ) {
            return;
        }
        for case in ["missing", "unreadable", "replaced"] {
            let base = std::env::temp_dir().join(format!("desklink-gone-{}", uuid::Uuid::new_v4()));
            let root = base.join("root");
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("a.txt"), "one").unwrap();
            let mut worker = worker_for(&base, &root);
            worker.rescan(0);
            match case {
                "missing" => fs::remove_dir_all(&root).unwrap(),
                "unreadable" => {
                    fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();
                }
                _ => {
                    fs::remove_dir_all(&root).unwrap();
                    fs::create_dir_all(&root).unwrap();
                }
            }
            worker.rescan(0);
            assert!(
                !worker.pairs[0].book.manifest["a.txt"].deleted,
                "{case}: nothing tombstoned"
            );
            assert!(
                matches!(worker.pairs[0].phase, Phase::EmptyHeld | Phase::Problem(_)),
                "{case}: held or a problem, not applied: {:?}",
                worker.pairs[0].phase
            );
            let _ = fs::set_permissions(&root, fs::Permissions::from_mode(0o700));
            let _ = fs::remove_dir_all(&base);
        }
    }

    /// The rule itself, for the cases a test cannot make on one disk: another
    /// device (a drive mounted over the folder, or gone from it), another
    /// inode, or nothing known before.
    #[test]
    fn an_emptied_folder_is_applied_only_when_it_is_the_same_directory() {
        let here = RootStamp {
            dev: 64768,
            ino: 1234,
        };
        assert!(emptied_on_purpose(Some(here), Some(here)));
        let other_device = RootStamp { dev: 2049, ..here };
        let other_inode = RootStamp { ino: 99, ..here };
        assert!(!emptied_on_purpose(Some(here), Some(other_device)));
        assert!(!emptied_on_purpose(Some(here), Some(other_inode)));
        assert!(!emptied_on_purpose(None, Some(here)), "not known before");
        assert!(
            !emptied_on_purpose(Some(here), None),
            "missing or unreadable"
        );
        assert!(!emptied_on_purpose(None, None));
    }

    #[test]
    fn an_empty_folder_is_held_until_confirmed() {
        let base = std::env::temp_dir().join(format!("desklink-hold-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        let mut worker = worker_for(&base, &root);
        worker.rescan(0);
        // The folder is swapped for an empty one (a drive that is not
        // mounted): held, nothing tombstoned.
        fs::remove_dir_all(&root).unwrap();
        fs::create_dir_all(&root).unwrap();
        worker.rescan(0);
        assert_eq!(worker.pairs[0].phase, Phase::EmptyHeld);
        assert!(
            !worker.pairs[0].book.manifest["a.txt"].deleted,
            "nothing tombstoned"
        );
        worker.handle(Message::ConfirmEmpty {
            device: "phone-h".into(),
            id: "pair-h".into(),
        });
        worker.rescan(0);
        assert!(
            worker.pairs[0].book.manifest["a.txt"].deleted,
            "confirmed: synced as deleted"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn without_a_trash_a_deleted_file_is_kept_and_reported() {
        let base = std::env::temp_dir().join(format!("desklink-trash-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        fs::write(root.join("b.txt"), "two").unwrap();
        let mut worker = worker_for(&base, &root);
        worker.rescan(0);
        let a = worker.pairs[0].book.manifest["a.txt"].clone();
        let packet = folders_packet(
            "manifest",
            json!({"id": "pair-h", "part": 1, "parts": 1, "entries": [
                {"path": "a.txt", "size": 0, "mtime": 9, "sha256": "", "deleted": true,
                 "vv": {"desk": a.vv.desk, "phone": 1}}]}),
        );
        TEST_TRASH_FAILS.with(|fails| fails.set(true));
        worker.on_packet("phone-h", &packet);
        TEST_TRASH_FAILS.with(|fails| fails.set(false));
        assert!(root.join("a.txt").exists(), "never deleted for good");
        assert_eq!(worker.pairs[0].phase, Phase::Problem(TRASH_MISSING.into()));
        let _ = fs::remove_dir_all(&base);
    }

    fn entry(sha: &str, vv: (u64, u64)) -> Entry {
        Entry {
            size: sha.len() as u64,
            mtime: 1_700_000_000_000,
            sha256: sha.to_string(),
            deleted: false,
            dir: false,
            vv: Vv {
                desk: vv.0,
                phone: vv.1,
            },
        }
    }

    fn tomb(vv: (u64, u64)) -> Entry {
        Entry {
            deleted: true,
            dir: false,
            sha256: String::new(),
            size: 0,
            ..entry("", vv)
        }
    }

    fn manifest(items: &[(&str, Entry)]) -> Manifest {
        items
            .iter()
            .map(|(path, entry)| (path.to_string(), entry.clone()))
            .collect()
    }

    #[test]
    fn dominance_takes_ties_do_nothing_and_concurrent_edits_conflict() {
        let local = manifest(&[
            ("same", entry("a", (1, 1))),
            ("older", entry("a", (1, 0))),
            ("newer", entry("b", (2, 0))),
            ("both", entry("c", (2, 0))),
            ("deleted-there", entry("d", (1, 0))),
            ("deleted-here", tomb((2, 0))),
        ]);
        let remote = manifest(&[
            ("same", entry("a", (1, 1))),
            ("older", entry("z", (1, 1))),
            ("newer", entry("b0", (1, 0))),
            ("both", entry("q", (1, 1))),
            ("deleted-there", tomb((1, 1))),
            ("deleted-here", entry("e", (1, 1))),
            ("new", entry("n", (0, 1))),
            ("gone", tomb((0, 2))),
        ]);
        let steps = plan(Side::Desk, &local, &remote, "Pixel");
        assert!(steps.contains(&Step::Download {
            path: "older".into(),
            entry: entry("z", (1, 1))
        }));
        assert!(steps.contains(&Step::Delete {
            path: "deleted-there".into(),
            entry: tomb((1, 1))
        }));
        assert!(steps.contains(&Step::Download {
            path: "new".into(),
            entry: entry("n", (0, 1))
        }));
        assert!(steps.contains(&Step::Take {
            path: "gone".into(),
            entry: tomb((0, 2))
        }));
        // Concurrent: both edited → the desk keeps it, a conflict is listed.
        assert!(!steps
            .iter()
            .any(|step| matches!(step, Step::Bump { path, .. } if path == "both")));
        assert!(steps
            .iter()
            .any(|step| matches!(step, Step::Conflict { path, copy }
            if path == "both" && copy == "both (conflict from Pixel 2023-11-14)")));
        // Deleted here, edited there: the edit wins.
        assert!(steps.contains(&Step::Download {
            path: "deleted-here".into(),
            entry: entry("e", (2, 1))
        }));
        assert!(!steps.iter().any(|step| matches!(step,
            Step::Take { path, .. } | Step::Download { path, .. } if path == "same" || path == "newer")));
    }

    #[test]
    fn the_phone_renames_its_own_version_in_a_conflict() {
        let local = manifest(&[("notes.txt", entry("p", (1, 2)))]);
        let remote = manifest(&[("notes.txt", entry("d", (2, 1)))]);
        let steps = plan(Side::Phone, &local, &remote, "Pixel 9");
        assert_eq!(
            steps,
            vec![
                Step::Rename {
                    from: "notes.txt".into(),
                    to: "notes (conflict from Pixel 9 2023-11-14).txt".into()
                },
                Step::Download {
                    path: "notes.txt".into(),
                    entry: entry("d", (3, 2))
                }
            ]
        );
    }

    #[test]
    fn renames_and_copies_are_found_by_hash() {
        let local = manifest(&[
            ("a/old.jpg", entry("x", (1, 0))),
            ("keep.jpg", entry("k", (1, 0))),
        ]);
        let remote = manifest(&[
            ("a/old.jpg", tomb((1, 1))),
            ("b/new.jpg", entry("x", (0, 1))),
            ("dup.jpg", entry("k", (0, 1))),
        ]);
        let steps = plan(Side::Desk, &local, &remote, "P");
        assert_eq!(
            steps,
            vec![
                Step::Move {
                    from: "a/old.jpg".into(),
                    to: "b/new.jpg".into(),
                    entry: entry("x", (0, 1)),
                    tombstone: tomb((1, 1)),
                },
                Step::Copy {
                    from: "keep.jpg".into(),
                    to: "dup.jpg".into(),
                    entry: entry("k", (0, 1)),
                },
            ]
        );
    }

    #[test]
    fn a_scan_bumps_changes_and_tombstones_what_vanished() {
        let mut manifest = manifest(&[
            (
                "kept",
                Entry {
                    size: 1,
                    mtime: 5,
                    ..entry("k", (1, 0))
                },
            ),
            (
                "touched",
                Entry {
                    size: 1,
                    mtime: 5,
                    ..entry("t", (1, 3))
                },
            ),
            (
                "edited",
                Entry {
                    size: 1,
                    mtime: 5,
                    ..entry("e", (1, 3))
                },
            ),
            ("vanished", entry("v", (0, 2))),
        ]);
        let found: BTreeMap<String, Stat> = [
            (
                "kept",
                Stat {
                    size: 1,
                    mtime: 5,
                    dir: false,
                },
            ),
            (
                "touched",
                Stat {
                    size: 1,
                    mtime: 9,
                    dir: false,
                },
            ),
            (
                "edited",
                Stat {
                    size: 2,
                    mtime: 9,
                    dir: false,
                },
            ),
            (
                "new",
                Stat {
                    size: 3,
                    mtime: 9,
                    dir: false,
                },
            ),
        ]
        .into_iter()
        .map(|(path, stat)| (path.to_string(), stat))
        .collect();
        let mut hashed = Vec::new();
        let mut hash = |path: &str| {
            hashed.push(path.to_string());
            Some(match path {
                "touched" => "t".to_string(),
                other => format!("{other}2"),
            })
        };
        assert!(apply_scan(
            Side::Desk,
            &mut manifest,
            &found,
            &mut hash,
            10_000
        ));
        assert_eq!(
            hashed,
            ["edited", "new", "touched"],
            "only what changed is hashed"
        );
        assert_eq!(manifest["kept"].vv, Vv { desk: 1, phone: 0 });
        assert_eq!(
            manifest["touched"].vv,
            Vv { desk: 1, phone: 3 },
            "same content"
        );
        assert_eq!(manifest["edited"].vv, Vv { desk: 2, phone: 3 });
        assert_eq!(manifest["new"].vv, Vv { desk: 1, phone: 0 });
        assert!(manifest["vanished"].deleted);
        assert_eq!(manifest["vanished"].vv, Vv { desk: 1, phone: 2 });
        // Old tombstones are forgotten.
        let later = 10_000 + (TOMBSTONE_DAYS + 1) * 24 * 3600 * 1000;
        apply_scan(Side::Desk, &mut manifest, &found, &mut |_| None, later);
        assert!(!manifest.contains_key("vanished"));
    }

    #[test]
    fn paths_that_escape_or_are_ignored_never_sync() {
        for bad in [
            "", "/abs", "a//b", "../x", "a/../b", "a/./b", ".", "a\\b", "a/b/", "x\0y",
        ] {
            assert!(validate_relpath(bad).is_err(), "{bad:?}");
        }
        assert!(validate_relpath(&["a"; 32].join("/")).is_ok());
        assert!(
            validate_relpath(&vec!["a"; 33].join("/")).is_err(),
            "too deep"
        );
        assert!(validate_relpath(&"a".repeat(1025)).is_err());
        assert!(validate_relpath("photos/2024/IMG 1.jpg").is_ok());
        assert!(ignored("src/.git/HEAD", true, &[]));
        assert!(ignored("web/node_modules/x.js", false, &[]));
        assert!(ignored("a/b.tmp", false, &[]));
        assert!(ignored(".hidden", false, &[]));
        assert!(!ignored(".hidden", true, &[]));
        assert!(ignored(".desklink-x.tmp", true, &[]));
        assert!(ignored("cache/x.log", false, &["*.log".into()]));
        assert!(ignored("build/out", false, &["build".into()]));
        assert!(!ignored("docs/readme.md", false, &["*.log".into()]));
    }

    #[test]
    fn target_paths_stay_under_the_root_and_never_through_a_link() {
        let root = std::env::temp_dir().join(format!("desklink-folders-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let outside = root.with_extension("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(target_path(&root, "link/x").is_err());
        assert!(target_path(&root, "../x").is_err());
        let made = target_path(&root, "new/dir/file.txt").unwrap();
        assert_eq!(made, root.join("new/dir/file.txt"));
        assert!(root.join("new/dir").is_dir());
        let staged = root.join("staged");
        fs::write(&staged, b"hello").unwrap();
        put_in_place(&staged, &made, 1_700_000_000_000).unwrap();
        assert_eq!(fs::read(&made).unwrap(), b"hello");
        assert_eq!(mtime_ms(&fs::metadata(&made).unwrap()), 1_700_000_000_000);
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn manifests_split_under_the_bound_and_bad_entries_are_dropped() {
        let mut big = Manifest::new();
        for index in 0..3000 {
            big.insert(
                format!("folder/file-{index:05}.txt"),
                entry(&"a".repeat(64), (1, 0)),
            );
        }
        let packets = manifest_packets("pair-1", &big);
        assert!(packets.len() > 1);
        let mut total = 0;
        for packet in &packets {
            assert!(packet.serialize().unwrap().len() <= MAX_PACKET_BYTES);
            total += manifest_entries(packet, &test_pair()).len();
        }
        assert_eq!(total, 3000);
        let mut bad = NetworkPacket::new(PACKET_TYPE_FOLDERS);
        bad.set(
            "entries",
            json!([
                {"path": "../etc/passwd", "size": 1, "mtime": 1, "sha256": "a".repeat(64), "vv": {"desk": 1, "phone": 0}},
                {"path": "ok.txt", "size": 1, "mtime": 1, "sha256": "zz", "vv": {"desk": 1, "phone": 0}},
                {"path": "fine.txt", "size": 1, "mtime": 1, "sha256": "A".repeat(64), "vv": {"desk": 1, "phone": 0}},
            ]),
        );
        let entries = manifest_entries(&bad, &test_pair());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].1.sha256, "a".repeat(64), "lowercased");
    }

    #[test]
    fn conflict_names_keep_the_extension_and_stay_valid() {
        assert_eq!(
            conflict_name("docs/plan.final.md", "Pixel/9", 0),
            "docs/plan.final (conflict from Pixel9 1970-01-01).md"
        );
        assert_eq!(
            conflict_name(".env", "P", 0),
            ".env (conflict from P 1970-01-01)"
        );
        let long = conflict_name(&format!("{}.txt", "x".repeat(250)), "P", 0);
        assert!(long.len() <= 255 && validate_relpath(&long).is_ok());
    }

    type Disk = BTreeMap<String, String>;
    /// A folder on a simulated disk.
    const DIR: &str = "\u{0}dir";
    struct Replica {
        side: Side,
        disk: Disk,
        manifest: Manifest,
    }
    impl Replica {
        fn scan(&mut self, now: i64) {
            let found: BTreeMap<String, Stat> = self
                .disk
                .iter()
                .map(|(path, content)| {
                    let dir = content == DIR;
                    (
                        path.clone(),
                        Stat {
                            size: if dir { 0 } else { content.len() as u64 },
                            mtime: if dir { 0 } else { content_mtime(content) },
                            dir,
                        },
                    )
                })
                .collect();
            let disk = self.disk.clone();
            apply_scan(
                self.side,
                &mut self.manifest,
                &found,
                &mut |path| disk.get(path).filter(|c| *c != DIR).map(|c| sha(c)),
                now,
            );
        }
    }
    fn sha(content: &str) -> String {
        format!(
            "{:x}",
            content
                .bytes()
                .fold(7u64, |h, b| h.wrapping_mul(31).wrapping_add(u64::from(b)))
        )
    }
    fn content_mtime(content: &str) -> i64 {
        // As a real disk: each write has its own time (here, a hash).
        (u64::from_str_radix(&sha(content), 16).unwrap_or(0) >> 2) as i64
    }
    /// Applies `local`'s plan; downloads read the other side's disk.
    fn sync(local: &mut Replica, remote: &Replica) {
        let steps = plan(local.side, &local.manifest, &remote.manifest, "P");
        let mut folder_deletes: Vec<(String, Entry)> = Vec::new();
        for step in steps {
            match step {
                Step::Take { path, entry } => {
                    local.manifest.insert(path, entry);
                }
                Step::Bump { path, vv } => {
                    if let Some(entry) = local.manifest.get_mut(&path) {
                        entry.vv = vv;
                    }
                }
                Step::Delete { path, entry } if entry.dir => folder_deletes.push((path, entry)),
                Step::Delete { path, entry } => {
                    local.disk.remove(&path);
                    local.manifest.insert(path, entry);
                }
                Step::Download { path, entry } if entry.dir => {
                    local.disk.insert(path.clone(), DIR.to_string());
                    local.manifest.insert(path, entry);
                }
                Step::Download { path, entry } => {
                    // The holder's file must still be that version.
                    if let Some(content) = remote.disk.get(&path).filter(|c| sha(c) == entry.sha256)
                    {
                        local.disk.insert(path.clone(), content.clone());
                        local.manifest.insert(
                            path,
                            Entry {
                                mtime: content_mtime(content),
                                ..entry
                            },
                        );
                    }
                }
                Step::Move {
                    from,
                    to,
                    entry,
                    tombstone,
                } => {
                    if let Some(content) = local.disk.remove(&from) {
                        local.disk.insert(to.clone(), content);
                    }
                    local.manifest.insert(from, tombstone);
                    local.manifest.insert(to, entry);
                }
                Step::Copy { from, to, entry } => {
                    if let Some(content) = local.disk.get(&from).cloned() {
                        local.disk.insert(to.clone(), content);
                    }
                    local.manifest.insert(to, entry);
                }
                Step::Rename { from, to } => {
                    if let Some(content) = local.disk.remove(&from) {
                        local.disk.insert(to, content);
                    }
                }
                Step::Conflict { .. } => {}
            }
        }
        // Folders last, deepest first, only when empty; a kept one wins.
        folder_deletes.sort_by_key(|(path, _)| std::cmp::Reverse(path.len()));
        for (path, entry) in folder_deletes {
            let below = format!("{path}/");
            if local.disk.keys().any(|other| other.starts_with(&below)) {
                if let Some(ours) = local.manifest.get(&path).cloned() {
                    let vv = ours.vv.max(entry.vv).bumped(local.side);
                    local.manifest.insert(path, Entry { vv, ..ours });
                }
            } else {
                local.disk.remove(&path);
                local.manifest.insert(path, entry);
            }
        }
    }

    /// The worker end to end, without a phone: scan, a phone's manifest, a
    /// deletion, a requested file arriving and put in place.
    #[test]
    fn the_worker_scans_plans_and_places_files() {
        let base = std::env::temp_dir().join(format!("desklink-sync-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        fs::write(root.join("sub/b.txt"), "two").unwrap();
        fs::write(root.join(".hidden"), "x").unwrap();
        let pair = Pair {
            id: "pair-w".into(),
            name: "root".into(),
            root: root.clone(),
            device: "phone-w".into(),
            include_hidden: false,
            ignore: Vec::new(),
            bucket: false,
        };
        let mut worker = Worker {
            dir: base.join("state"),
            pairs: vec![Live::new(pair, Book::default())],
            ready: ["phone-w".to_string()].into(),
            asked: HashMap::new(),
            offered: Vec::new(),
            dirty: Default::default(),
            dropped: 0,
        };
        fs::create_dir_all(&worker.dir).unwrap();
        worker.rescan(0);
        let manifest = &worker.pairs[0].book.manifest;
        assert_eq!(
            manifest.keys().collect::<Vec<_>>(),
            ["a.txt", "sub/b.txt"],
            "hidden skipped"
        );
        let a = manifest["a.txt"].clone();
        let three = "three";
        let three_sha = sha256_of(&{
            let path = base.join("three");
            fs::write(&path, three).unwrap();
            path
        })
        .unwrap();
        let remote = json!([
            {"path": "a.txt", "size": 0, "mtime": 9, "sha256": "", "deleted": true,
             "vv": {"desk": a.vv.desk, "phone": 1}},
            {"path": "c.txt", "size": 5, "mtime": 1_700_000_000_000i64, "sha256": three_sha,
             "vv": {"desk": 0, "phone": 1}},
        ]);
        let packet = folders_packet(
            "manifest",
            json!({"id": "pair-w", "part": 1, "parts": 1, "entries": remote}),
        );
        worker.on_packet("phone-w", &packet);
        assert!(
            !root.join("a.txt").exists(),
            "deleted on the phone, so here"
        );
        assert_eq!(worker.pairs[0].phase, Phase::Syncing(1));
        let folder = FolderRef {
            folder: "pair-w".into(),
            relpath: "c.txt".into(),
            mtime: None,
        };
        assert!(accepts("phone-w", &folder, &three_sha, 5));
        assert!(!accepts("someone-else", &folder, &three_sha, 5));
        let stash = base.join("stash");
        fs::write(&stash, three).unwrap();
        worker.handle(Message::Arrived {
            device: "phone-w".into(),
            folder: folder.clone(),
            stash: stash.clone(),
            sha256: three_sha.clone(),
            size: 5,
        });
        assert_eq!(fs::read_to_string(root.join("c.txt")).unwrap(), three);
        assert_eq!(worker.pairs[0].phase, Phase::UpToDate);
        assert!(!accepts("phone-w", &folder, &three_sha, 5), "asked once");
        // A rescan sees nothing new: our own write is not a change.
        assert!(!apply_scan(
            Side::Desk,
            &mut worker.pairs[0].book.manifest.clone(),
            &scan(&worker.pairs[0].pair).unwrap().0,
            &mut |_| None,
            now_ms()
        ));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn a_concurrent_edit_keeps_both_versions_on_both_sides() {
        let mut desk = Replica {
            side: Side::Desk,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        let mut phone = Replica {
            side: Side::Phone,
            disk: Disk::new(),
            manifest: Manifest::new(),
        };
        desk.disk.insert("notes.txt".into(), "base".into());
        desk.scan(1);
        sync(&mut phone, &desk);
        assert_eq!(phone.disk["notes.txt"], "base");
        desk.disk.insert("notes.txt".into(), "desk edit".into());
        phone.disk.insert("notes.txt".into(), "phone edit".into());
        desk.scan(2);
        phone.scan(2);
        for _ in 0..3 {
            sync(&mut desk, &phone);
            desk.scan(3);
            sync(&mut phone, &desk);
            phone.scan(3);
        }
        assert_eq!(desk.disk, phone.disk);
        assert_eq!(
            desk.disk["notes.txt"], "desk edit",
            "the desk's keeps the name"
        );
        let copies: Vec<&String> = desk.disk.values().filter(|c| *c == "phone edit").collect();
        assert_eq!(copies.len(), 1, "the phone's is kept as a conflict copy");
    }

    /// Property test: two replicas edit, delete, rename and create at
    /// random; after sync rounds both hold the same files.
    #[test]
    fn random_edits_on_both_sides_converge() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |bound: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % bound
        };
        for round in 0..300 {
            let mut desk = Replica {
                side: Side::Desk,
                disk: Disk::new(),
                manifest: Manifest::new(),
            };
            let mut phone = Replica {
                side: Side::Phone,
                disk: Disk::new(),
                manifest: Manifest::new(),
            };
            let mut now = 1_000;
            for _ in 0..6 {
                for replica in [&mut desk, &mut phone] {
                    for _ in 0..next(4) {
                        let path = format!("d{}/f{}", next(2), next(4));
                        match next(4) {
                            0 => {
                                replica.disk.remove(&path);
                            }
                            1 => {
                                if let Some((from, content)) = replica
                                    .disk
                                    .iter()
                                    .next()
                                    .map(|(p, c)| (p.clone(), c.clone()))
                                {
                                    replica.disk.remove(&from);
                                    replica.disk.insert(path, content);
                                }
                            }
                            _ => {
                                replica
                                    .disk
                                    .insert(path, format!("v{}-{round}", next(1000)));
                            }
                        }
                    }
                    now += 1;
                    replica.scan(now);
                }
                // A few rounds of exchange, as the manifests go back and forth.
                for _ in 0..4 {
                    sync(&mut desk, &phone);
                    desk.scan(now);
                    sync(&mut phone, &desk);
                    phone.scan(now);
                }
                assert_eq!(
                    desk.disk, phone.disk,
                    "round {round}: the files differ\ndesk {:?}\nphone {:?}",
                    desk.manifest, phone.manifest
                );
            }
        }
    }

    // ── W3 — case clashes ──────────────────────────────────────────────────

    /// Names that differ only in case, files and folders alike: the one that
    /// sorts first in byte order syncs; the others (and what is below them)
    /// are listed, never offered.
    #[test]
    fn a_scan_syncs_the_first_of_names_that_differ_only_in_case() {
        let (_base, root, worker) = scratch_bucket("bucket-case-scan");
        for name in ["A.txt", "a.txt", "B.txt", "É.txt", "é.txt"] {
            fs::write(root.join(name), name).unwrap();
        }
        fs::create_dir_all(root.join("Dir")).unwrap();
        fs::create_dir_all(root.join("dir/sub")).unwrap();
        fs::write(root.join("Dir/x.txt"), "x").unwrap();
        fs::write(root.join("dir/y.txt"), "y").unwrap();
        fs::write(root.join("dir/sub/z.txt"), "z").unwrap();
        // The same name in two folders is not a clash.
        fs::write(root.join("Dir/a.txt"), "1").unwrap();
        fs::write(root.join("Dir/A.TXT"), "2").unwrap();
        let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
        assert_eq!(
            found.keys().map(String::as_str).collect::<Vec<_>>(),
            ["A.txt", "B.txt", "Dir", "Dir/A.TXT", "Dir/x.txt", "É.txt"],
            "Dir/a.txt lost to Dir/A.TXT, nothing below `dir` is listed"
        );
        let clash = |name: &str| format!("{name} (another file differs only in capital letters)");
        assert_eq!(
            skipped,
            [
                clash("Dir/a.txt"),
                clash("a.txt"),
                clash("dir"),
                clash("é.txt")
            ],
        );
    }

    /// The phone's manifest can hold two paths that differ only in case (its
    /// own scan or an older build): the later is not synced here either, and
    /// a tombstone is never a clash.
    #[test]
    fn a_manifest_with_two_paths_that_differ_only_in_case_loses_the_later() {
        let folder = |vv| Entry {
            dir: true,
            sha256: String::new(),
            size: 0,
            ..entry("", vv)
        };
        let mut remote = manifest(&[
            ("A.txt", entry("a", (0, 1))),
            ("a.txt", entry("b", (0, 1))),
            ("Dir", folder((0, 1))),
            ("dir", folder((0, 1))),
            ("dir/y.txt", entry("y", (0, 1))),
            ("Dir/x.txt", entry("x", (0, 1))),
            ("B.txt", entry("c", (0, 1))),
            ("b.txt", tomb((0, 2))),
        ]);
        let left_out = drop_case_clashes(&mut remote, |entry| !entry.deleted);
        assert_eq!(left_out, ["a.txt", "dir"]);
        assert_eq!(
            remote.keys().map(String::as_str).collect::<Vec<_>>(),
            ["A.txt", "B.txt", "Dir", "Dir/x.txt", "b.txt"],
            "the tombstone stays"
        );
        // Nothing is asked for under the left-out names.
        let steps = plan(Side::Desk, &Manifest::new(), &remote, "Phone");
        assert!(steps.iter().all(|step| !matches!(
            step,
            Step::Download { path, .. } if path == "a.txt" || path.starts_with("dir")
        )));
    }

    /// The desk's own copy of the later name is listed as not synced and is
    /// never an item, so it cannot read "Waiting…"; once the first name is
    /// gone the other syncs.
    #[test]
    fn the_later_name_is_not_synced_never_waiting_and_syncs_once_alone() {
        let (_base, root, mut worker) = scratch_bucket("bucket-case-ui");
        fs::write(root.join("A.txt"), "1").unwrap();
        fs::write(root.join("a.txt"), "2").unwrap();
        worker.rescan(0);
        for online in [false, true] {
            let view = worker.pairs[0].view(online);
            let paths: Vec<&str> = view.items.iter().map(|item| item.path.as_str()).collect();
            assert_eq!(paths, ["A.txt"], "online: {online}");
            assert_eq!(
                view.skipped,
                ["a.txt (another file differs only in capital letters)"]
            );
        }
        assert!(!worker.pairs[0].book.manifest.contains_key("a.txt"));
        // The manifest sent to the phone never names it.
        let sent = manifest_packets("m", &worker.pairs[0].book.manifest);
        assert!(
            !format!("{:?}", sent.iter().map(|p| &p.body).collect::<Vec<_>>()).contains("a.txt")
        );
        fs::remove_file(root.join("A.txt")).unwrap();
        worker.rescan(0);
        let view = worker.pairs[0].view(true);
        assert!(view.skipped.is_empty());
        assert!(view
            .items
            .iter()
            .any(|item| item.path == "a.txt" && !item.dir));
    }

    /// The vector both apps pin: these names in one directory.
    const CASE_VECTOR: [&str; 7] = ["a.txt", "A.txt", "É", "é", "Dir", "dir", "b.txt"];
    const CASE_WINNERS: [&str; 4] = ["A.txt", "É", "Dir", "b.txt"];
    const CASE_LOSERS: [&str; 3] = ["a.txt", "é", "dir"];

    /// Golden: the shared case-clash vector, from a disk scan and from a
    /// manifest alike.
    #[test]
    fn the_shared_case_clash_vector_gives_the_same_winners_everywhere() {
        let (_base, root, worker) = scratch_bucket("bucket-case-vector");
        for name in CASE_VECTOR {
            fs::write(root.join(name), name).unwrap();
        }
        let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
        let winners: Vec<&str> = CASE_VECTOR
            .into_iter()
            .filter(|name| found.contains_key(*name))
            .collect();
        assert_eq!(winners, CASE_WINNERS);
        assert_eq!(found.len(), CASE_WINNERS.len());
        let mut listed = skipped.clone();
        listed.sort();
        let mut expected: Vec<String> = CASE_LOSERS
            .iter()
            .map(|name| format!("{name} (another file differs only in capital letters)"))
            .collect();
        expected.sort();
        assert_eq!(listed, expected);
        // The same, as a received manifest (and in any order of arrival).
        let mut remote: Manifest = CASE_VECTOR
            .iter()
            .map(|name| (name.to_string(), entry(name, (0, 1))))
            .collect();
        let mut left_out = left_out_of_remote(&mut remote, &Manifest::new());
        let winners: Vec<&str> = CASE_VECTOR
            .into_iter()
            .filter(|name| remote.contains_key(*name))
            .collect();
        assert_eq!(winners, CASE_WINNERS);
        left_out.sort();
        let mut losers = CASE_LOSERS.map(String::from).to_vec();
        losers.sort();
        assert_eq!(left_out, losers);
    }

    /// A phone path that clashes in case with one the desk already has is
    /// never downloaded, whichever name would sort first; the desk's entry
    /// and file are left alone and the phone's is listed as not synced.
    #[test]
    fn a_phone_path_that_clashes_with_a_desk_path_is_never_downloaded() {
        // (desk has, phone sends): the phone's loses, then the phone's would win.
        for (desk_has, phone_sends) in [("A.txt", "a.txt"), ("a.txt", "A.txt")] {
            let (_base, root, mut worker) = scratch_bucket("bucket-case-remote");
            fs::write(root.join(desk_has), "mine").unwrap();
            fs::write(root.join("other.txt"), "o").unwrap();
            worker.rescan(0);
            let ours = worker.pairs[0].book.manifest.clone();
            assert!(!ours[desk_has].deleted);
            let phone = manifest(&[
                (phone_sends, entry(&"1".repeat(64), (0, 1))),
                ("other.txt", ours["other.txt"].clone()),
                ("new.txt", entry(&"2".repeat(64), (0, 1))),
            ]);
            for packet in manifest_packets(BUCKET_ID, &phone) {
                worker.on_manifest("phone-b", BUCKET_ID, &packet);
            }
            let live = &worker.pairs[0];
            let manifest = &live.book.manifest;
            assert_eq!(manifest[desk_has], ours[desk_has], "{desk_has} untouched");
            assert!(
                !manifest.contains_key(phone_sends),
                "{phone_sends} not taken"
            );
            assert_eq!(fs::read(root.join(desk_has)).unwrap(), b"mine");
            assert!(!root.join(phone_sends).exists());
            let wanted: Vec<&str> = live.to_request.iter().map(|p| p.as_str()).collect();
            assert!(
                !wanted.contains(&phone_sends)
                    && !live.outstanding.iter().any(|p| p == phone_sends),
                "{phone_sends} is never asked for: {wanted:?}"
            );
            assert!(
                live.view(true)
                    .skipped
                    .contains(&format!("{phone_sends} ({CASE_CLASH})")),
                "{:?}",
                live.view(true).skipped
            );
            assert!(
                !live
                    .view(true)
                    .items
                    .iter()
                    .any(|item| item.path == phone_sends),
                "and it is no item, so never Waiting"
            );
            // What does not clash is still taken.
            assert!(wanted.contains(&"new.txt") || live.outstanding.iter().any(|p| p == "new.txt"));
        }
    }
}

/// Property tests without a crate: a fixed-seed generator, so a failure
/// repeats.
#[cfg(test)]
mod property_tests {
    use super::*;

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn random_entry(rng: &mut Rng) -> Entry {
        let deleted = rng.below(4) == 0;
        Entry {
            size: 1,
            mtime: 1_700_000_000_000,
            sha256: if deleted {
                String::new()
            } else {
                format!("sha{}", rng.below(3))
            },
            deleted,
            dir: false,
            vv: Vv {
                desk: rng.below(4),
                phone: rng.below(4),
            },
        }
    }

    /// The planner never throws away a local edit: a file removed or
    /// replaced here was dominated by the other side's version, or went
    /// aside under a conflict name first.
    #[test]
    fn the_planner_never_loses_a_live_local_edit() {
        let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
        let paths = ["a.txt", "b.txt", "c/d.txt", "e.txt"];
        for case in 0..3000 {
            let mut local = Manifest::new();
            let mut remote = Manifest::new();
            for path in paths {
                if rng.below(4) != 0 {
                    local.insert(path.to_string(), random_entry(&mut rng));
                }
                if rng.below(4) != 0 {
                    remote.insert(path.to_string(), random_entry(&mut rng));
                }
            }
            for side in [Side::Desk, Side::Phone] {
                let steps = plan(side, &local, &remote, "Pixel");
                for step in &steps {
                    let (path, replaces) = match step {
                        Step::Delete { path, .. } | Step::Download { path, .. } => (path, true),
                        _ => continue,
                    };
                    let (Some(ours), Some(theirs)) = (local.get(path), remote.get(path)) else {
                        continue;
                    };
                    if ours.deleted || !replaces {
                        continue;
                    }
                    let kept_aside = steps.iter().any(|other| {
                        matches!(other, Step::Rename { from, .. } if from == path)
                            || matches!(other, Step::Conflict { path: p, .. } if p == path)
                    });
                    assert!(
                        theirs.vv.dominates(ours.vv) || kept_aside || ours.sha256 == theirs.sha256,
                        "case {case} {side:?}: {path} {ours:?} replaced by {theirs:?}: {steps:?}"
                    );
                }
            }
        }
    }

    /// A path the Bucket accepts has no way out of its folder.
    #[test]
    fn no_accepted_bucket_path_can_leave_the_folder() {
        let mut rng = Rng(0xD1B5_4A32_D192_ED03);
        let alphabet: Vec<char> = "ab./\\: \u{0}\u{7}\u{202e}é~".chars().collect();
        for _ in 0..20_000 {
            let length = rng.below(14) as usize;
            let path: String = (0..length)
                .map(|_| alphabet[rng.below(alphabet.len() as u64) as usize])
                .collect();
            for dir in [true, false] {
                if bucket_path_ok(&path, dir) {
                    assert!(!path.starts_with('/'), "{path:?}");
                    assert!(!path.contains('\\'), "{path:?}");
                    assert!(!path
                        .split('/')
                        .any(|s| s.is_empty() || s == "." || s == ".."));
                    assert!(!path.chars().any(char::is_control), "{path:?}");
                    assert!(path.split('/').count() <= MAX_BUCKET_DEPTH + 1);
                }
            }
        }
    }

    /// Whatever bytes a peer sends, decoding a packet or a vCard never
    /// panics.
    #[test]
    fn hostile_bytes_never_panic_the_packet_or_vcard_parsers() {
        let mut rng = Rng(0x1234_5678_9ABC_DEF1);
        let pieces = [
            "{",
            "}",
            "\"",
            ":",
            ",",
            "[",
            "]",
            "null",
            "type",
            "body",
            "desklink.ping",
            "id",
            "-1",
            "BEGIN:VCARD",
            "\r\n",
            "FN:",
            "TEL;TYPE=CELL:",
            "PHOTO;ENCODING=b:",
            "END:VCARD",
            "é",
        ];
        for _ in 0..5_000 {
            let count = rng.below(30);
            let text: String = (0..count)
                .map(|_| pieces[rng.below(pieces.len() as u64) as usize])
                .collect();
            let _ = crate::device_links::packet::NetworkPacket::deserialize(text.as_bytes());
            let _ = crate::device_links::contacts::parse_vcard("u", &text);
        }
        let mut bytes = vec![0u8; 512];
        for _ in 0..2_000 {
            for byte in &mut bytes {
                *byte = rng.next() as u8;
            }
            let _ = crate::device_links::packet::NetworkPacket::deserialize(&bytes);
        }
    }
}
