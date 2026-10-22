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
use super::webrtc::file_protocol::{validate_relpath, FolderRef, MAX_TRANSFER_BYTES};

// Limits. Each bounds what a peer, or a huge folder, can make this hold or do.
/// Pairs per phone.
pub(crate) const MAX_PAIRS_PER_DEVICE: usize = 8;
/// Manifest entries per pair (live and tombstones).
const MAX_FILES: usize = 20_000;
/// A file larger than this is skipped (the transfer bound).
const MAX_FILE_BYTES: u64 = MAX_TRANSFER_BYTES;
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
/// A Bucket larger than this shows a warning.
pub(crate) const BUCKET_WARN_BYTES: u64 = 10 * 1024 * 1024 * 1024;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Entry {
    pub(crate) size: u64,
    pub(crate) mtime: i64,
    pub(crate) sha256: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
    pub(crate) vv: Vv,
}

pub(crate) type Manifest = BTreeMap<String, Entry>;

/// A local file's size and mtime (ms), from a scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stat {
    pub(crate) size: u64,
    pub(crate) mtime: i64,
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
        .filter(|c| !matches!(c, '/' | '\\' | '\0') && !c.is_control())
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
            if let Some(ours) = local.get(path).filter(|ours| !ours.deleted) {
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
        .filter(|(path, entry)| !entry.deleted && !changing.contains(path.as_str()))
        .map(|(path, entry)| (entry.sha256.as_str(), path.as_str()))
        .collect();
    let mut moved: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for step in steps {
        match step {
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
        match manifest.get_mut(path) {
            Some(entry)
                if !entry.deleted && entry.size == stat.size && entry.mtime == stat.mtime => {}
            existing => {
                let Some(sha256) = hash(path) else {
                    continue;
                };
                match existing {
                    Some(entry) if !entry.deleted && entry.sha256 == sha256 => {
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
    },
    /// F8b: a pasted picture, text or link, saved as `name` in the Bucket.
    ImportBytes {
        device: String,
        name: String,
        bytes: Vec<u8>,
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
    send_message(Message::Link(device_id.to_string(), ready));
}

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

/// F8b: copies the files among `sources` into `device_id`'s Bucket
/// (folders are not taken; see [`split_files`]).
pub(crate) fn import(device_id: &str, sources: Vec<PathBuf>) -> bool {
    send_message(Message::Import {
        device: device_id.to_string(),
        sources,
    })
}

/// What can go in the Bucket: the regular files of `paths`, and whether any
/// folder was among them (said to the user: files only for now). Links are
/// not followed.
pub(crate) fn split_files(paths: Vec<PathBuf>) -> (Vec<PathBuf>, bool) {
    let mut folder = false;
    let files = paths
        .into_iter()
        .filter(|path| match fs::symlink_metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                folder = true;
                false
            }
            Ok(metadata) => metadata.is_file(),
            Err(_) => false,
        })
        .collect();
    (files, folder)
}

/// What the desk says when folders are dropped or pasted (F8b: files only).
pub(crate) const FOLDERS_NOT_YET: &str =
    "Folders aren't supported in the Bucket yet. Drop the files instead.";

/// F8b: saves pasted `bytes` as `name` in `device_id`'s Bucket. Text and
/// links over [`MAX_TEXT_ITEM_BYTES`] are refused here.
pub(crate) fn import_bytes(device_id: &str, name: &str, bytes: Vec<u8>) -> Result<(), String> {
    let text = name.ends_with(".txt") || name.ends_with(".url");
    if text && bytes.len() > MAX_TEXT_ITEM_BYTES {
        return Err("That text is too long for the Bucket (1 MB at most)".into());
    }
    if bytes.is_empty() {
        return Err("Nothing to add".into());
    }
    send_message(Message::ImportBytes {
        device: device_id.to_string(),
        name: name.to_string(),
        bytes,
    });
    Ok(())
}

/// `Text <date time>.txt` for pasted text, `<host> <date>.url` (a
/// freedesktop link) for a pasted link: the name and the bytes to save.
pub(crate) fn pasted_text(text: &str, now: &gtk::glib::DateTime) -> (String, Vec<u8>) {
    let trimmed = text.trim();
    let link = (trimmed.starts_with("https://") || trimmed.starts_with("http://"))
        && !trimmed.contains(char::is_whitespace);
    if link {
        let host: String = trimmed
            .split("://")
            .nth(1)
            .and_then(|rest| rest.split(['/', '?', '#']).next())
            .unwrap_or("Link")
            .chars()
            .filter(|c| !matches!(c, '/' | '\\' | '\0') && !c.is_control())
            .take(80)
            .collect();
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
    validate_relpath(&folder.relpath).is_ok()
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
/// A scan: the files, the directories to watch, whether there were more
/// files than allowed, and (a Bucket) the folders in it left unsynced.
type Scanned = (BTreeMap<String, Stat>, Vec<PathBuf>, bool, Vec<String>);

fn scan(pair: &Pair) -> Result<Scanned, String> {
    let mut found = BTreeMap::new();
    let mut directories = Vec::new();
    let mut skipped = Vec::new();
    let mut too_many = false;
    let root_meta =
        fs::symlink_metadata(&pair.root).map_err(|_| "The folder is missing".to_string())?;
    if !root_meta.is_dir() {
        return Err("The folder is missing".into());
    }
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
            // Links are never followed, special files never read.
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                if pair.bucket {
                    // F8b: files only. A folder put in from outside is
                    // left alone and listed as not synced.
                    if skipped.len() < MAX_SKIPPED_SHOWN {
                        skipped.push(name);
                    }
                } else if depth + 1 < MAX_DEPTH {
                    stack.push((entry.path(), path, depth + 1));
                }
                continue;
            }
            if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES {
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
                },
            );
        }
    }
    skipped.sort();
    Ok((found, directories, too_many, skipped))
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

/// Whether the local file still matches what the manifest says (absent
/// when the manifest has no live entry).
fn unchanged(path: &Path, entry: Option<&Entry>) -> bool {
    match (
        fs::symlink_metadata(path),
        entry.filter(|entry| !entry.deleted),
    ) {
        (Err(_), None) => true,
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
    let first = dir.join(name);
    if fs::symlink_metadata(&first).is_err() {
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
        .find(|path| fs::symlink_metadata(path).is_err())
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

/// Copies the regular file `source` to `target` (F8b: files only; links,
/// folders and special files are left out), through a temporary name and a
/// rename; at most `budget` files.
fn copy_in(source: &Path, target: &Path, budget: &mut usize) -> Result<(), String> {
    let metadata = fs::symlink_metadata(source).map_err(|_| "A file to add is gone".to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_FILE_BYTES || *budget == 0 {
        return Ok(());
    }
    *budget -= 1;
    let staged = target.with_file_name(format!(".desklink-{}.tmp", uuid::Uuid::new_v4().simple()));
    fs::copy(source, &staged).map_err(|_| "Could not copy a file into the Bucket".to_string())?;
    put_in_place(&staged, target, mtime_ms(&metadata))
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
        !TEST_TRASH_FAILS.with(std::cell::Cell::get) && fs::remove_file(path).is_ok()
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
    /// The other side's live files (path → sha256), from its last manifest.
    remote: HashMap<String, String>,
    /// Files the other side asked for, and their sha256: sending.
    sending: HashMap<String, String>,
    /// Files asked of the other side: receiving.
    receiving: BTreeMap<String, Entry>,
    /// F8b: folders found in the Bucket, not synced (files only).
    skipped: Vec<String>,
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
        match messages.recv_timeout(Duration::from_millis(500)) {
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
    for (path, entry) in manifest {
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
                        && !(pair.bucket && path.contains('/'))
                        && !ignored(path, pair.include_hidden, &pair.ignore)
                        && (entry.deleted
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
            remote: HashMap::new(),
            sending: HashMap::new(),
            receiving: BTreeMap::new(),
            skipped: Vec::new(),
        }
    }

    fn view(&self, online: bool) -> PairView {
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
                self.items(online)
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
            skipped: self.skipped.clone(),
        }
    }

    /// F8b: the Bucket's items, newest first, each with its state.
    fn items(&self, online: bool) -> Vec<Item> {
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
                } else if self.sending.contains_key(path) && !there {
                    ItemState::Sending
                } else if !there && !online {
                    ItemState::Waiting
                } else {
                    ItemState::Synced
                };
                Item {
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
            Message::Import { device, sources } => self.import(&device, sources),
            Message::ImportBytes {
                device,
                name,
                bytes,
            } => self.import_bytes(&device, &name, &bytes),
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

    /// Room left in `device`'s Bucket, in items.
    fn bucket_room(&self, device: &str) -> usize {
        let used = self
            .index_of(device, BUCKET_ID)
            .map(|index| {
                self.pairs[index]
                    .book
                    .manifest
                    .values()
                    .filter(|entry| !entry.deleted)
                    .count()
            })
            .unwrap_or(0);
        MAX_BUCKET_ITEMS.saturating_sub(used)
    }

    fn import(&mut self, device: &str, sources: Vec<PathBuf>) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        let mut budget = self.bucket_room(device);
        for source in sources.into_iter().take(MAX_BUCKET_ITEMS) {
            // Never the Bucket into itself.
            if source.starts_with(&dir) || dir.starts_with(&source) {
                continue;
            }
            let Some(name) = source.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let target = unique_in(&dir, name);
            if let Err(error) = copy_in(&source, &target, &mut budget) {
                log_limited("import", &error);
            }
        }
        self.touched(device);
    }

    fn import_bytes(&mut self, device: &str, name: &str, bytes: &[u8]) {
        let Some(dir) = self.bucket_dir(device) else {
            return;
        };
        if self.bucket_room(device) == 0 || validate_relpath(name).is_err() || name.contains('/') {
            return;
        }
        let target = unique_in(&dir, name);
        let staged =
            target.with_file_name(format!(".desklink-{}.tmp", uuid::Uuid::new_v4().simple()));
        let written =
            fs::write(&staged, bytes).is_ok() && put_in_place(&staged, &target, now_ms()).is_ok();
        if !written {
            let _ = fs::remove_file(&staged);
            log_limited("import", "Could not save a pasted item in the Bucket");
        }
        self.touched(device);
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
        let remote: Manifest = std::mem::take(&mut live.incoming)
            .into_values()
            .flatten()
            .take(MAX_FILES)
            .collect();
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
        let mut rescan = false;
        let mut kept = false;
        {
            let live = &mut self.pairs[index];
            let manifest = &mut live.book.manifest;
            for step in steps {
                match step {
                    Step::Take { path, entry } => {
                        manifest.insert(path, entry);
                    }
                    Step::Bump { path, vv } => {
                        if let Some(entry) = manifest.get_mut(&path) {
                            entry.vv = vv;
                        }
                    }
                    Step::Delete { path, entry } => match target_path(&root, &path) {
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
                        let moved = target_path(&root, &from)
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
                        let copied = target_path(&root, &from)
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
                    Step::Download { path, entry } => downloads.push((path, entry)),
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
        while live.outstanding.len() < MAX_OUTSTANDING
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
            let Some(entry) = live.book.manifest.get(&path).filter(|entry| !entry.deleted) else {
                continue;
            };
            let Ok(source) = target_path(&live.pair.root, &path) else {
                continue;
            };
            if !unchanged(&source, Some(entry)) {
                live_dirty(&self.dirty, id);
                continue;
            }
            if live.sending.len() < MAX_OUTSTANDING {
                live.sending.insert(path.clone(), entry.sha256.clone());
            }
            let folder = FolderRef {
                folder: id.to_string(),
                relpath: path,
                mtime: Some(entry.mtime),
            };
            if let Err(error) = super::desk_features::send_folder_file(device, &source, folder) {
                log_limited("send", &format!("A synced file was not sent: {error}"));
            }
        }
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
                if let Ok(copy) = target_path(&root, &conflict.copy) {
                    trash(&copy);
                }
            }
            Resolve::Phone => {
                if let (Ok(copy), Ok(original)) = (
                    target_path(&root, &conflict.copy),
                    target_path(&root, &conflict.path),
                ) {
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
        live.dirty = None;
        live.scanned = Some(Instant::now());
        live.phase = Phase::Scanning;
        let pair = live.pair.clone();
        let scanned = scan(&pair);
        let (found, directories, too_many, skipped) = match scanned {
            Ok(scanned) => scanned,
            Err(problem) => {
                live.phase = Phase::Problem(problem);
                return;
            }
        };
        live.skipped = skipped;
        let had_files = live.book.manifest.values().any(|entry| !entry.deleted);
        if found.is_empty() && had_files && !std::mem::take(&mut live.allow_empty) {
            // An unmounted drive looks exactly like this: deleting every
            // file on the phone too would be the worst outcome.
            live.phase = Phase::EmptyHeld;
            return;
        }
        let root = pair.root.clone();
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

    fn bucket_worker(base: &Path, root: &Path) -> Worker {
        let pair = Pair {
            id: BUCKET_ID.into(),
            name: "DeskLink Bucket".into(),
            root: root.to_path_buf(),
            device: "phone-b".into(),
            include_hidden: false,
            ignore: Vec::new(),
            bucket: true,
        };
        let worker = Worker {
            dir: base.join("state"),
            pairs: vec![Live::new(pair, Book::default())],
            ready: ["phone-b".to_string()].into(),
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

    #[test]
    fn a_bucket_holds_top_level_files_only() {
        let base = scratch("bucket-scan");
        let root = base.join("bucket");
        fs::create_dir_all(root.join("Old photos/2024")).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        fs::write(root.join("Old photos/b.jpg"), "two").unwrap();
        std::os::unix::fs::symlink(base.join("elsewhere"), root.join("link")).unwrap();
        let worker = bucket_worker(&base, &root);
        let (found, _, _, skipped) = scan(&worker.pairs[0].pair).unwrap();
        assert_eq!(found.keys().collect::<Vec<_>>(), ["a.txt"]);
        assert_eq!(skipped, ["Old photos"], "listed as not synced");
        // A peer cannot put a nested path in a Bucket either.
        let mut packet = NetworkPacket::new(PACKET_TYPE_FOLDERS);
        let entry = |path: &str| {
            json!({"path": path, "size": 1, "mtime": 1, "sha256": "a".repeat(64),
                   "vv": {"desk": 0, "phone": 1}})
        };
        packet.set("entries", json!([entry("x/y.txt"), entry("z.txt")]));
        let kept: Vec<String> = manifest_entries(&packet, &worker.pairs[0].pair)
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        assert_eq!(kept, ["z.txt"]);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn dropped_folders_and_links_are_refused_files_are_added() {
        let base = scratch("bucket-drop");
        let root = base.join("bucket");
        fs::create_dir_all(&root).unwrap();
        let outside = base.join("outside");
        fs::create_dir_all(outside.join("nested/empty")).unwrap();
        fs::write(outside.join("photo.jpg"), "jpeg").unwrap();
        fs::write(outside.join("nested/inner.txt"), "inner").unwrap();
        std::os::unix::fs::symlink(outside.join("photo.jpg"), outside.join("link.jpg")).unwrap();
        fs::write(root.join("photo.jpg"), "already here").unwrap();
        let (files, folder) = split_files(vec![
            outside.join("photo.jpg"),
            outside.join("nested"),
            outside.join("link.jpg"),
            outside.join("gone.txt"),
        ]);
        assert!(folder, "a folder was among them");
        assert_eq!(
            files,
            [outside.join("photo.jpg")],
            "no folder, link or missing file"
        );
        let mut worker = bucket_worker(&base, &root);
        worker.handle(Message::Import {
            device: "phone-b".into(),
            sources: vec![outside.join("photo.jpg"), outside.join("nested")],
        });
        assert_eq!(
            fs::read_to_string(root.join("photo (2).jpg")).unwrap(),
            "jpeg"
        );
        assert_eq!(
            fs::read_to_string(root.join("photo.jpg")).unwrap(),
            "already here"
        );
        assert!(!root.join("nested").exists(), "folders never copied in");
        worker.rescan(0);
        let items = &worker.pairs[0].view(true).items;
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|item| item.from == Side::Desk));
        assert!(items
            .iter()
            .all(|item| item.state == ItemState::Synced || item.state == ItemState::Waiting));
        let _ = fs::remove_dir_all(&base);
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
        let (name, _) = pasted_text("see https://a.b and more", &now);
        assert!(name.ends_with(".txt"), "a sentence with a link is text");
        assert!(import_bytes("p", "Text.txt", vec![b'x'; MAX_TEXT_ITEM_BYTES + 1]).is_err());
        assert!(import_bytes("p", "Text.txt", Vec::new()).is_err());
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

    #[test]
    fn an_empty_folder_is_held_until_confirmed() {
        let base = std::env::temp_dir().join(format!("desklink-hold-{}", uuid::Uuid::new_v4()));
        let root = base.join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.txt"), "one").unwrap();
        let mut worker = worker_for(&base, &root);
        worker.rescan(0);
        fs::remove_file(root.join("a.txt")).unwrap();
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
            vv: Vv {
                desk: vv.0,
                phone: vv.1,
            },
        }
    }

    fn tomb(vv: (u64, u64)) -> Entry {
        Entry {
            deleted: true,
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
            ("kept", Stat { size: 1, mtime: 5 }),
            ("touched", Stat { size: 1, mtime: 9 }),
            ("edited", Stat { size: 2, mtime: 9 }),
            ("new", Stat { size: 3, mtime: 9 }),
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
                    (
                        path.clone(),
                        Stat {
                            size: content.len() as u64,
                            mtime: content_mtime(content),
                        },
                    )
                })
                .collect();
            let disk = self.disk.clone();
            apply_scan(
                self.side,
                &mut self.manifest,
                &found,
                &mut |path| disk.get(path).map(|c| sha(c)),
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
                Step::Delete { path, entry } => {
                    local.disk.remove(&path);
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
}
