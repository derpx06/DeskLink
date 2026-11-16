//! What a pair is and what the window shows of it: [`Pair`], the saved [`Book`],
//! [`Phase`], [`PairView`], Bucket [`Item`]s with [`totals`]/[`rollup`], pair
//! [`Offer`]s, and [`RootStamp`] (whose `of` reads the root's metadata: the
//! one piece of I/O here).
//!
//! Sibling edges: `model`.

use super::*;

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
pub(super) struct Book {
    #[serde(default)]
    pub(super) version: u32,
    pub(super) manifest: Manifest,
    #[serde(default)]
    pub(super) conflicts: Vec<Conflict>,
    #[serde(default)]
    pub(super) last_sync: Option<i64>,
    /// F8b: which side each file came from ("From <device>"). Local.
    #[serde(default)]
    pub(super) from: BTreeMap<String, Side>,
}

pub(super) const TRASH_MISSING: &str =
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
pub(super) struct RootStamp {
    pub(super) dev: u64,
    pub(super) ino: u64,
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
pub(super) fn emptied_on_purpose(before: Option<RootStamp>, now: Option<RootStamp>) -> bool {
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
