//! Snippets and notes as stored and synced: [`Snippet`], [`Note`] with its
//! ancestors, and the shelf that holds both with tombstones and per-peer marks.
//!
//! Sibling edges: none.

use super::*;

fn is_false(value: &bool) -> bool {
    !value
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Snippet {
    pub(crate) id: String,
    pub(crate) text: String,
    #[serde(default)]
    pub(crate) title: String,
    pub(crate) modified: i64,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
}

impl Snippet {
    /// Its title, or its first line.
    pub(crate) fn label(&self) -> String {
        let label = if self.title.trim().is_empty() {
            self.text
                .lines()
                .find(|line| !line.trim().is_empty())
                .unwrap_or_default()
        } else {
            self.title.as_str()
        };
        label.trim().chars().take(60).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Note {
    pub(crate) id: String,
    #[serde(default)]
    pub(crate) title: String,
    #[serde(default)]
    pub(crate) body: String,
    #[serde(default)]
    pub(crate) pinned: bool,
    pub(crate) created: i64,
    pub(crate) modified: i64,
    /// The `modified` of the version this one was edited from (N1).
    #[serde(default)]
    pub(crate) parent: i64,
    /// The `modified` values this note had before this version, newest first,
    /// at most [`MAX_ANCESTORS`] (W7 "notes ancestors"). A peer that holds any
    /// of them holds an ancestor of this version, so the version simply
    /// replaces it: without them, only the *synced* base counted, and every
    /// second edit made in a row made a "(conflicted copy)" of the first.
    /// Absent from older builds, which then fall back to `parent`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) ancestors: Vec<i64>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
}

/// What a conflicted copy's title ends with.
pub(super) const COPY_SUFFIX: &str = "(conflicted copy)";

/// How many earlier versions a note names as ancestors.
pub(super) const MAX_ANCESTORS: usize = 8;

/// `before` and what it already listed: the ancestors of the version made
/// from it, newest first, at most [`MAX_ANCESTORS`]. When there are more, the
/// versions in `keep` (held by, or sent to, a phone) stay and the rest are the
/// newest others: a phone that was away through many edits holds one of those.
pub(super) fn ancestors_after(before: &Note, keep: &[i64]) -> Vec<i64> {
    let chain: Vec<i64> = std::iter::once(before.modified)
        .chain(before.ancestors.iter().copied())
        .collect();
    if chain.len() <= MAX_ANCESTORS {
        return chain;
    }
    let mut kept: Vec<i64> = chain.iter().copied().filter(|m| keep.contains(m)).collect();
    kept.truncate(MAX_ANCESTORS);
    let room = MAX_ANCESTORS - kept.len();
    let recent: Vec<i64> = chain
        .iter()
        .copied()
        .filter(|m| !kept.contains(m))
        .take(room)
        .collect();
    let mut all = kept;
    all.extend(recent);
    all.sort_unstable_by(|a, b| b.cmp(a));
    all
}

/// What labels and duplicate checks trim, spelled out so the phone trims exactly the
/// same characters (`trim()` differs between Rust and Kotlin at U+0085 and U+200B).
const TRIM: &[char] = &[
    ' ', '\t', '\n', '\r', '\u{b}', '\u{c}', '\u{a0}', '\u{2028}', '\u{2029}', '\u{3000}',
];

pub(super) fn trim_note(text: &str) -> &str {
    text.trim_matches(TRIM)
}

impl Note {
    /// Its title, or its first line, or "New Note".
    pub(crate) fn label(&self) -> String {
        let label = if trim_note(&self.title).is_empty() {
            self.body
                .lines()
                .map(|line| trim_note(line.trim_start_matches(['#', ' ', '-', '*'])))
                .find(|line| !line.is_empty())
                .unwrap_or("New Note")
        } else {
            trim_note(&self.title)
        };
        label.chars().take(80).collect()
    }

    pub(super) fn same_content(&self, other: &Note) -> bool {
        self.title == other.title
            && self.body == other.body
            && self.pinned == other.pinned
            && self.deleted == other.deleted
    }
}

/// Everything kept, on disk as one sealed file.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub(super) struct Shelf {
    pub(super) snippets: BTreeMap<String, Snippet>,
    pub(super) notes: BTreeMap<String, Note>,
    /// Per note: the `modified` both sides last had.
    #[serde(default)]
    pub(super) synced: BTreeMap<String, i64>,
    /// Per note, per phone: the version that phone is known to hold (it sent it,
    /// listed it, or acknowledged it). The next edit names the oldest of them as
    /// its `parent` and keeps every one of them among its `ancestors`, so a phone
    /// that was away through many edits still recognises the next one.
    #[serde(default)]
    pub(super) held: BTreeMap<String, BTreeMap<String, i64>>,
    /// Per note, per phone: the latest version we sent it (it holds that one or the
    /// `held` one; the acknowledgement may have been lost).
    #[serde(default)]
    pub(super) sent: BTreeMap<String, BTreeMap<String, i64>>,
}

/// Phones remembered per note; past it the one holding the oldest version goes.
const MAX_PEERS_PER_NOTE: usize = 3;

fn record_peer(marks: &mut BTreeMap<String, BTreeMap<String, i64>>, id: &str, peer: &str, v: i64) {
    if peer.is_empty() {
        return;
    }
    let peers = marks.entry(id.to_string()).or_default();
    peers.insert(peer.to_string(), v);
    while peers.len() > MAX_PEERS_PER_NOTE {
        let oldest = peers
            .iter()
            .min_by_key(|(_, version)| **version)
            .map(|(peer, _)| peer.clone());
        match oldest {
            Some(oldest) => peers.remove(&oldest),
            None => break,
        };
    }
}

impl Shelf {
    pub(super) fn record_held(&mut self, id: &str, peer: &str, version: i64) {
        record_peer(&mut self.held, id, peer, version);
    }

    pub(super) fn record_sent(&mut self, id: &str, peer: &str, version: i64) {
        record_peer(&mut self.sent, id, peer, version);
    }

    /// The `parent` of the next edit of `id`: the oldest version a phone is known
    /// to hold (every newer phone finds its own among the ancestors), else the
    /// version both sides last had.
    pub(super) fn base_of(&self, id: &str) -> i64 {
        self.held
            .get(id)
            .and_then(|peers| peers.values().min().copied())
            .or_else(|| self.synced.get(id).copied())
            .unwrap_or(0)
    }

    /// The versions of `id` that must stay among its ancestors however many edits
    /// follow: what the phones hold, and what was sent to them.
    pub(super) fn must_keep(&self, id: &str) -> Vec<i64> {
        [self.held.get(id), self.sent.get(id)]
            .into_iter()
            .flatten()
            .flat_map(|peers| peers.values().copied())
            .collect()
    }

    pub(super) fn forget_phone(&mut self, device_id: &str) {
        for marks in [&mut self.held, &mut self.sent] {
            marks.retain(|_, peers| {
                peers.remove(device_id);
                !peers.is_empty()
            });
        }
    }
}

pub(super) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}
