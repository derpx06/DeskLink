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
//!
//! Layout: this file is the facade (limits, the window's snapshot, the
//! messages to the worker and the calls the rest of the crate makes);
//! `model` is the pure planner, `state` the pair and view types, `scan` and
//! `disk` the file system, `store` the sealed files, `wire` the packets,
//! `import`/`report` adding to a Bucket, and `worker` (with its `bucket` and
//! `engine` halves) the thread. There are no cycles between the parts except
//! inside the one `Worker` type. The rest of the crate uses only what is
//! re-exported below. May depend on: `packet`, `limits`, `vault`,
//! `webrtc::file_protocol`, `webrtc::transfer_board`, `desk_features`
//! (sending), `connection_log`.

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

mod bucket;
mod disk;
mod engine;
mod import;
mod model;
mod report;
mod scan;
mod state;
mod store;
mod wire;
mod worker;

// Shared inside `folders`: each part sees the others through the parent.
use disk::*;
use import::*;
use model::*;
use report::*;
use scan::*;
use state::*;
use store::*;
use wire::*;
use worker::*;

// The crate-facing surface of `folders` (everything else stays inside).
pub(crate) use disk::fold_name;
pub(crate) use import::{import_bytes, pasted_text};
pub(crate) use model::{name_ok, parent_of, Side};
pub(crate) use report::take_report;
#[cfg(test)]
pub(crate) use scan::drop_case_clashes_for_tests;
pub(crate) use scan::{recreate_root, FOLDER_MISSING};
pub(crate) use state::{rollup, totals, Conflict, Item, ItemState, Offer, Pair, PairView, Phase};
pub(crate) use worker::start;

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

/// Conflicts for which "Keep the phone's" was refused for want of a trash:
/// only then is "Replace Anyway" offered.
fn refused_trash() -> &'static Mutex<HashSet<(String, String)>> {
    static REFUSED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    REFUSED.get_or_init(Default::default)
}

/// Whether "Replace Anyway" is offered for `path`'s conflict with `device`.
pub(crate) fn replace_anyway_offered(device: &str, path: &str) -> bool {
    refused_trash()
        .lock()
        .is_ok_and(|set| set.contains(&(device.to_string(), path.to_string())))
}

/// Tests of the Bucket page: as if "Keep the phone's" had been refused for
/// want of a trash.
#[cfg(test)]
pub(crate) fn mark_trash_refused(device: &str, path: &str) {
    if let Ok(mut set) = refused_trash().lock() {
        set.insert((device.to_string(), path.to_string()));
    }
}

/// How a conflict is resolved (P6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolve {
    /// Keep both files.
    Both,
    /// Keep this computer's (the copy goes to the trash).
    Desk,
    /// Keep the phone's (the copy replaces the original; this computer's
    /// goes to the trash first, and without a trash nothing is replaced).
    Phone,
    /// Keep the phone's even though there is no trash to put this computer's
    /// in: it is replaced for good. Offered only after `Phone` said why not.
    PhoneAnyway,
}

#[derive(Debug)]
enum Message {
    Packet(String, NetworkPacket),
    Link(String, bool),
    /// The phone was unpaired: its pairs, manifests and offers go; the
    /// files in its Bucket stay on disk.
    Forget(String),
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

/// `device_id` was unpaired: syncing stops and its pairs, manifests, offers
/// and requested-file records are forgotten. The files in its Bucket folder
/// are kept (a phone paired again starts a fresh Bucket over the same folder).
pub(crate) fn forget_device(device_id: &str) {
    if let Ok(mut wanted) = WANTED.lock() {
        wanted.retain(|(owner, _, _), _| owner != device_id);
    }
    if let Ok(mut refused) = refused_trash().lock() {
        refused.retain(|(owner, _)| owner != device_id);
    }
    send_message(Message::Forget(device_id.to_string()));
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

/// Takes the request `key` if it is for exactly `sha256` and `size`; any
/// other request under that key stays.
fn take_wanted_matching(key: &(String, String, String), sha256: &str, size: u64) -> Option<Entry> {
    let mut wanted = WANTED.lock().ok()?;
    let matches = wanted
        .get(key)
        .is_some_and(|entry| entry.sha256.eq_ignore_ascii_case(sha256) && entry.size == size);
    if matches {
        wanted.remove(key)
    } else {
        None
    }
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

#[cfg(test)]
mod property_tests;
#[cfg(test)]
mod tests;
