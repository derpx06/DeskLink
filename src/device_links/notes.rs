//! Snippets and notes (DeskLink 1.2, F6): the user's, kept here encrypted
//! whatever happens to the pairing, and synced with each connected phone.
//!
//! Both sides send an index on feature-ready and fetch what they lack or
//! what differs. Snippets: the newer `modified` wins. Notes: a version
//! edited from the other is taken; two versions edited from the same one
//! are both kept — the newer under the id, the other as a "conflicted copy"
//! under an id both sides derive alike, so neither has to send it.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::packet::{NetworkPacket, PACKET_TYPE_NOTES, PACKET_TYPE_SNIPPETS};

const MAX_ID_CHARS: usize = 128;
const MAX_SNIPPETS: usize = 500;
const MAX_SNIPPET_BYTES: usize = 16 * 1024;
const MAX_SNIPPET_TITLE_CHARS: usize = 120;
const MAX_NOTES: usize = 2000;
const MAX_NOTE_TITLE_CHARS: usize = 200;
const MAX_NOTE_BYTES: usize = 64 * 1024;
/// ids per `fetch`.
const FETCH_IDS: usize = 100;
/// One `items` packet stays under the WebRTC payload bound (128 KiB).
const MAX_PACKET_BYTES: usize = 96 * 1024;
const TOMBSTONE_DAYS: i64 = 90;
/// Entries kept, live and tombstones: a peer cannot grow the shelf past
/// these (new ids are refused, logged).
const MAX_SNIPPET_ENTRIES: usize = MAX_SNIPPETS * 2;
const MAX_NOTE_ENTRIES: usize = MAX_NOTES * 2;
/// Items read from one `items` or `index` packet.
const MAX_ITEMS_PER_PACKET: usize = 200;
const MAX_INDEX_ENTRIES: usize = MAX_NOTE_ENTRIES;
/// Snippet and note packets accepted per phone per minute.
const PACKETS_PER_MINUTE: u32 = 240;

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
    #[serde(default, skip_serializing_if = "is_false")]
    pub(crate) deleted: bool,
}

impl Note {
    /// Its title, or its first line, or "New Note".
    pub(crate) fn label(&self) -> String {
        let label = if self.title.trim().is_empty() {
            self.body
                .lines()
                .map(|line| line.trim_start_matches(['#', ' ', '-', '*']).trim())
                .find(|line| !line.is_empty())
                .unwrap_or("New Note")
        } else {
            self.title.trim()
        };
        label.chars().take(80).collect()
    }

    fn same_content(&self, other: &Note) -> bool {
        self.title == other.title
            && self.body == other.body
            && self.pinned == other.pinned
            && self.deleted == other.deleted
    }
}

/// Everything kept, on disk as one sealed file.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Shelf {
    snippets: BTreeMap<String, Snippet>,
    notes: BTreeMap<String, Note>,
    /// Per note: the `modified` both sides last had.
    #[serde(default)]
    synced: BTreeMap<String, i64>,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

// ── Validation (on receipt: over a bound is dropped, not truncated) ────────

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().count() <= MAX_ID_CHARS && !id.chars().any(char::is_control)
}

fn valid_snippet(snippet: &Snippet) -> bool {
    valid_id(&snippet.id)
        && snippet.text.len() <= MAX_SNIPPET_BYTES
        && snippet.title.chars().count() <= MAX_SNIPPET_TITLE_CHARS
        && (snippet.deleted || !snippet.text.is_empty())
        && snippet.modified > 0
}

fn valid_note(note: &Note) -> bool {
    valid_id(&note.id)
        && note.body.len() <= MAX_NOTE_BYTES
        && note.title.chars().count() <= MAX_NOTE_TITLE_CHARS
        && note.modified > 0
}

// ── Merging ────────────────────────────────────────────────────────────────

/// S2: the newer snippet wins. Whether `incoming` was taken.
fn merge_snippet(shelf: &mut Shelf, incoming: Snippet) -> bool {
    match shelf.snippets.get(&incoming.id) {
        Some(local) if local.modified >= incoming.modified => false,
        _ => {
            shelf.snippets.insert(incoming.id.clone(), incoming);
            true
        }
    }
}

/// What a received note did.
#[derive(Debug, PartialEq, Eq)]
enum Merged {
    /// Ours stays; the peer has an older version (send it ours).
    KeptOurs,
    Taken,
    /// Already the same.
    Same,
    /// Siblings: both are kept now.
    Conflict,
}

/// N2.
fn merge_note(shelf: &mut Shelf, incoming: Note) -> Merged {
    let Some(local) = shelf.notes.get(&incoming.id).cloned() else {
        shelf.synced.insert(incoming.id.clone(), incoming.modified);
        shelf.notes.insert(incoming.id.clone(), incoming);
        return Merged::Taken;
    };
    if local.modified == incoming.modified {
        shelf.synced.insert(local.id.clone(), local.modified);
        return Merged::Same;
    }
    if incoming.parent == local.modified {
        shelf.synced.insert(incoming.id.clone(), incoming.modified);
        shelf.notes.insert(incoming.id.clone(), incoming);
        return Merged::Taken;
    }
    if local.parent == incoming.modified {
        return Merged::KeptOurs;
    }
    // Siblings, edited from the same version.
    let incoming_wins = if local.same_content(&incoming) || local.deleted != incoming.deleted {
        if local.deleted != incoming.deleted {
            // A deletion never beats a concurrent edit.
            local.deleted
        } else {
            incoming.modified > local.modified
        }
    } else {
        (incoming.modified, incoming.body.as_bytes()) > (local.modified, local.body.as_bytes())
    };
    let (winner, loser) = if incoming_wins {
        (incoming, local)
    } else {
        (local, incoming)
    };
    let conflict = !winner.same_content(&loser) && !winner.deleted && !loser.deleted;
    let copy = conflict.then(|| Note {
        id: format!("{}~{}", loser.id, loser.modified),
        title: format!("{} (conflicted copy)", loser.label()),
        parent: loser.modified,
        ..loser
    });
    // Ours stays and the copy is already here: resolved before.
    let known = copy
        .as_ref()
        .is_none_or(|copy| shelf.notes.get(&copy.id) == Some(copy));
    if !incoming_wins && known {
        return Merged::KeptOurs;
    }
    if let Some(copy) = copy {
        shelf.synced.insert(copy.id.clone(), copy.modified);
        shelf.notes.insert(copy.id.clone(), copy);
    }
    shelf.synced.insert(winner.id.clone(), winner.modified);
    shelf.notes.insert(winner.id.clone(), winner);
    if conflict {
        Merged::Conflict
    } else {
        Merged::Taken
    }
}

/// The ids in a peer's index we should fetch.
fn wanted(shelf: &Shelf, kind: Kind, index: &[Value]) -> Vec<String> {
    index
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id")?.as_str()?;
            let modified = entry.get("modified")?.as_i64()?;
            if !valid_id(id) {
                return None;
            }
            let ours = match kind {
                Kind::Snippets => shelf.snippets.get(id).map(|snippet| snippet.modified),
                Kind::Notes => shelf.notes.get(id).map(|note| note.modified),
            };
            let fetch = match (kind, ours) {
                (_, None) => true,
                (Kind::Snippets, Some(ours)) => modified > ours,
                (Kind::Notes, Some(ours)) => modified != ours,
            };
            fetch.then(|| id.to_string())
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Snippets,
    Notes,
}

impl Kind {
    fn packet_type(self) -> &'static str {
        match self {
            Kind::Snippets => PACKET_TYPE_SNIPPETS,
            Kind::Notes => PACKET_TYPE_NOTES,
        }
    }

    fn from_packet(packet_type: &str) -> Option<Self> {
        match packet_type {
            PACKET_TYPE_SNIPPETS => Some(Kind::Snippets),
            PACKET_TYPE_NOTES => Some(Kind::Notes),
            _ => None,
        }
    }
}

fn index_packet(shelf: &Shelf, kind: Kind) -> NetworkPacket {
    let items: Vec<Value> = match kind {
        Kind::Snippets => shelf
            .snippets
            .values()
            .map(|s| json!({"id": s.id, "modified": s.modified, "deleted": s.deleted}))
            .collect(),
        Kind::Notes => shelf
            .notes
            .values()
            .map(|n| json!({"id": n.id, "modified": n.modified, "deleted": n.deleted}))
            .collect(),
    };
    let mut packet = NetworkPacket::new(kind.packet_type());
    packet.set("op", "index");
    packet.set("items", items);
    packet
}

/// `items` packets for `values`, split to stay under the size bound.
fn items_packets(kind: Kind, values: Vec<Value>) -> Vec<NetworkPacket> {
    let mut packets = Vec::new();
    let mut batch: Vec<Value> = Vec::new();
    let mut size = 0;
    let flush = |batch: &mut Vec<Value>, packets: &mut Vec<NetworkPacket>| {
        if !batch.is_empty() {
            let mut packet = NetworkPacket::new(kind.packet_type());
            packet.set("op", "items");
            packet.set("items", std::mem::take(batch));
            packets.push(packet);
        }
    };
    for value in values {
        let length = value.to_string().len();
        if size + length > MAX_PACKET_BYTES {
            flush(&mut batch, &mut packets);
            size = 0;
        }
        size += length;
        batch.push(value);
    }
    flush(&mut batch, &mut packets);
    packets
}

fn item_values(shelf: &Shelf, kind: Kind, ids: &[String]) -> Vec<Value> {
    ids.iter()
        .filter_map(|id| match kind {
            Kind::Snippets => shelf
                .snippets
                .get(id)
                .and_then(|s| serde_json::to_value(s).ok()),
            Kind::Notes => shelf
                .notes
                .get(id)
                .and_then(|n| serde_json::to_value(n).ok()),
        })
        .collect()
}

/// A packet from a peer: what to send back. `changed` says whether the
/// shelf changed (to save).
fn handle(shelf: &mut Shelf, kind: Kind, packet: &NetworkPacket) -> (Vec<NetworkPacket>, bool) {
    match packet.get_str("op") {
        Some("index") => {
            let index: Vec<Value> = packet
                .body
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(MAX_INDEX_ENTRIES)
                .cloned()
                .collect();
            let ids = wanted(shelf, kind, &index);
            let fetches = ids
                .chunks(FETCH_IDS)
                .map(|chunk| {
                    let mut fetch = NetworkPacket::new(kind.packet_type());
                    fetch.set("op", "fetch");
                    fetch.set("ids", chunk.to_vec());
                    fetch
                })
                .collect();
            (fetches, false)
        }
        Some("fetch") => {
            let ids: Vec<String> = packet
                .body
                .get("ids")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(FETCH_IDS)
                .map(str::to_string)
                .collect();
            (items_packets(kind, item_values(shelf, kind, &ids)), false)
        }
        Some("items") => {
            let items: Vec<Value> = packet
                .body
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(MAX_ITEMS_PER_PACKET)
                .cloned()
                .collect();
            let mut changed = false;
            let mut send_back = Vec::new();
            for item in items {
                match kind {
                    Kind::Snippets => {
                        let Ok(snippet) = serde_json::from_value::<Snippet>(item) else {
                            continue;
                        };
                        if !valid_snippet(&snippet) {
                            continue;
                        }
                        let id = snippet.id.clone();
                        if !shelf.snippets.contains_key(&id)
                            && shelf.snippets.len() >= MAX_SNIPPET_ENTRIES
                        {
                            super::limits::log_limited(
                                "snippets-full",
                                "Snippets are full; one was refused",
                            );
                            continue;
                        }
                        let older = shelf
                            .snippets
                            .get(&id)
                            .is_some_and(|local| local.modified > snippet.modified);
                        if merge_snippet(shelf, snippet) {
                            changed = true;
                        } else if older {
                            send_back.push(id);
                        }
                    }
                    Kind::Notes => {
                        let Ok(note) = serde_json::from_value::<Note>(item) else {
                            continue;
                        };
                        if !valid_note(&note) {
                            continue;
                        }
                        let id = note.id.clone();
                        // A new id, or a conflict copy, needs room.
                        if shelf.notes.len() >= MAX_NOTE_ENTRIES {
                            super::limits::log_limited(
                                "notes-full",
                                "Notes are full; one was refused",
                            );
                            continue;
                        }
                        match merge_note(shelf, note) {
                            Merged::KeptOurs => send_back.push(id),
                            Merged::Same => {}
                            Merged::Taken => changed = true,
                            // The peer resolves it alike once it has ours.
                            Merged::Conflict => {
                                changed = true;
                                send_back.push(id);
                            }
                        }
                    }
                }
            }
            (
                items_packets(kind, item_values(shelf, kind, &send_back)),
                changed,
            )
        }
        _ => (Vec::new(), false),
    }
}

// ── The store ──────────────────────────────────────────────────────────────

struct Store {
    dir: PathBuf,
    shelf: Shelf,
}

fn store() -> &'static Mutex<Option<Store>> {
    static STORE: OnceLock<Mutex<Option<Store>>> = OnceLock::new();
    STORE.get_or_init(Default::default)
}

static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Bumped on every change, for the UI, tray and top bar to redraw.
pub(crate) fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

const FILE: &str = "shelf.sealed";
const AAD: &[u8] = b"desklink-snippets-and-notes";

/// Loads the shelf from `dir` (`<config>/notes`). Called once at startup.
pub(crate) fn install(dir: PathBuf) {
    let mut shelf: Shelf = super::vault::open_or_quarantine(&dir, &dir.join(FILE), AAD);
    prune(&mut shelf, now_ms());
    if let Ok(mut store) = store().lock() {
        *store = Some(Store { dir, shelf });
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// Drops tombstones older than [`TOMBSTONE_DAYS`].
fn prune(shelf: &mut Shelf, now: i64) {
    let cutoff = now - TOMBSTONE_DAYS * 24 * 3600 * 1000;
    shelf
        .snippets
        .retain(|_, snippet| !snippet.deleted || snippet.modified > cutoff);
    shelf
        .notes
        .retain(|_, note| !note.deleted || note.modified > cutoff);
    let notes = &shelf.notes;
    shelf.synced.retain(|id, _| notes.contains_key(id));
}

/// Runs `job` on the shelf; saves it and bumps the generation when it says
/// it changed it.
fn with_shelf<T>(job: impl FnOnce(&mut Shelf) -> (T, bool)) -> Option<T> {
    let mut guard = store().lock().ok()?;
    let store = guard.as_mut()?;
    let (result, changed) = job(&mut store.shelf);
    if changed {
        // Sealed and written off this thread (often the GTK one, typing).
        match serde_json::to_vec(&store.shelf) {
            Ok(plain) => super::vault::seal_later(&store.dir, &store.dir.join(FILE), AAD, plain),
            Err(_) => eprintln!("[Notes] Could not save snippets and notes"),
        }
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    Some(result)
}

/// A `desklink.snippets` or `desklink.notes` packet from `device_id`: the
/// packets to answer with.
pub(crate) fn on_packet(device_id: &str, packet: &NetworkPacket) -> Vec<NetworkPacket> {
    static PACKETS: super::limits::Buckets =
        super::limits::Buckets::new(PACKETS_PER_MINUTE, std::time::Duration::from_secs(60));
    let Some(kind) = Kind::from_packet(&packet.packet_type) else {
        return Vec::new();
    };
    if !PACKETS.allow(device_id) {
        super::limits::log_limited("notes-rate", "Too many note packets from a phone; dropped");
        return Vec::new();
    }
    let answers = with_shelf(|shelf| handle(shelf, kind, packet)).unwrap_or_default();
    debug_log!(
        "[Notes] {} {} from {device_id}: {} answers",
        packet.packet_type,
        packet.get_str("op").unwrap_or("?"),
        answers.len()
    );
    answers
}

/// Both indexes, for a phone whose connection just came up.
pub(crate) fn indexes() -> Vec<NetworkPacket> {
    with_shelf(|shelf| {
        (
            vec![
                index_packet(shelf, Kind::Snippets),
                index_packet(shelf, Kind::Notes),
            ],
            false,
        )
    })
    .unwrap_or_default()
}

/// Sends `ids` to every connected phone (a local change).
fn push(kind: Kind, ids: Vec<String>) {
    let Some(packets) =
        with_shelf(|shelf| (items_packets(kind, item_values(shelf, kind, &ids)), false))
    else {
        return;
    };
    super::desk_features::run_later(move || {
        for device in super::desk_features::ready_devices() {
            for packet in &packets {
                super::desk_features::send(&device, packet);
            }
        }
    });
}

// ── Local edits ────────────────────────────────────────────────────────────

/// Live snippets, newest first.
pub(crate) fn snippets() -> Vec<Snippet> {
    let mut list: Vec<Snippet> = with_shelf(|shelf| {
        (
            shelf
                .snippets
                .values()
                .filter(|snippet| !snippet.deleted)
                .cloned()
                .collect(),
            false,
        )
    })
    .unwrap_or_default();
    list.sort_by_key(|snippet| std::cmp::Reverse(snippet.modified));
    list.truncate(MAX_SNIPPETS);
    list
}

/// Pins `text` as a snippet (an identical one is moved to the top).
pub(crate) fn add_snippet(text: &str) -> Result<(), String> {
    let text = text.trim_end_matches('\n');
    if text.trim().is_empty() {
        return Err("Nothing to pin".into());
    }
    if text.len() > MAX_SNIPPET_BYTES {
        return Err("Too long for a snippet (16 KB at most)".into());
    }
    let id = with_shelf(|shelf| {
        let existing = shelf
            .snippets
            .values()
            .find(|snippet| !snippet.deleted && snippet.text == text)
            .map(|snippet| snippet.id.clone());
        let id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let snippet = Snippet {
            id: id.clone(),
            text: text.to_string(),
            title: String::new(),
            modified: now_ms(),
            deleted: false,
        };
        shelf.snippets.insert(id.clone(), snippet);
        (id, true)
    })
    .ok_or("Snippets are not available")?;
    push(Kind::Snippets, vec![id]);
    Ok(())
}

pub(crate) fn delete_snippet(id: &str) {
    let done = with_shelf(|shelf| match shelf.snippets.get_mut(id) {
        Some(snippet) if !snippet.deleted => {
            snippet.deleted = true;
            snippet.text.clear();
            snippet.title.clear();
            snippet.modified = now_ms();
            (true, true)
        }
        _ => (false, false),
    });
    if done == Some(true) {
        push(Kind::Snippets, vec![id.to_string()]);
    }
}

/// Live notes: pinned first, then newest first (N3).
pub(crate) fn notes() -> Vec<Note> {
    let mut list: Vec<Note> = with_shelf(|shelf| {
        (
            shelf
                .notes
                .values()
                .filter(|note| !note.deleted)
                .cloned()
                .collect(),
            false,
        )
    })
    .unwrap_or_default();
    list.sort_by_key(|note| {
        (
            std::cmp::Reverse(note.pinned),
            std::cmp::Reverse(note.modified),
        )
    });
    list
}

pub(crate) fn note(id: &str) -> Option<Note> {
    with_shelf(|shelf| {
        (
            shelf.notes.get(id).filter(|note| !note.deleted).cloned(),
            false,
        )
    })
    .flatten()
}

/// A new empty note (N1: `parent` 0); its id.
pub(crate) fn new_note(body: &str) -> Option<String> {
    let id = with_shelf(|shelf| {
        if shelf.notes.values().filter(|note| !note.deleted).count() >= MAX_NOTES {
            return (None, false);
        }
        let now = now_ms();
        let id = uuid::Uuid::new_v4().to_string();
        shelf.notes.insert(
            id.clone(),
            Note {
                id: id.clone(),
                title: String::new(),
                body: body.to_string(),
                pinned: false,
                created: now,
                modified: now,
                parent: 0,
                deleted: false,
            },
        );
        (Some(id), true)
    })
    .flatten()?;
    push(Kind::Notes, vec![id.clone()]);
    Some(id)
}

/// Saves an edit to note `id`: title, body, pinned, or deleted.
/// Saves an edit to note `id`. Refused, in words the editor shows, when
/// the note would pass 64 KB — never dropped silently.
pub(crate) fn edit_note(id: &str, change: impl FnOnce(&mut Note)) -> Result<(), String> {
    let changed = with_shelf(|shelf| {
        let synced = shelf.synced.get(id).copied().unwrap_or(0);
        let Some(note) = shelf.notes.get_mut(id) else {
            return (Ok(false), false);
        };
        let before = note.clone();
        change(note);
        note.title = note.title.chars().take(MAX_NOTE_TITLE_CHARS).collect();
        if note.body.len() > MAX_NOTE_BYTES {
            *note = before;
            return (Err(TOO_LONG.to_string()), false);
        }
        if note.same_content(&before) {
            return (Ok(false), false);
        }
        if note.deleted {
            note.title.clear();
            note.body.clear();
        }
        note.parent = synced;
        // Strictly later, even within one millisecond.
        note.modified = now_ms().max(before.modified.saturating_add(1));
        (Ok(true), true)
    });
    match changed {
        Some(Ok(true)) => {
            push(Kind::Notes, vec![id.to_string()]);
            Ok(())
        }
        Some(Ok(false)) | None => Ok(()),
        Some(Err(error)) => Err(error),
    }
}

/// What the editor says when a note passes its size bound.
pub(crate) const TOO_LONG: &str =
    "This note is too long to save (64 KB at most). Shorten it: the last changes are not saved yet.";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_peer_cannot_grow_the_shelf_past_its_bounds() {
        let mut shelf = Shelf::default();
        let now = now_ms();
        for index in 0..MAX_SNIPPET_ENTRIES {
            shelf.snippets.insert(
                format!("s{index}"),
                Snippet {
                    id: format!("s{index}"),
                    text: "x".into(),
                    title: String::new(),
                    modified: now,
                    deleted: false,
                },
            );
        }
        let mut packet = NetworkPacket::new(PACKET_TYPE_SNIPPETS);
        packet.set("op", "items");
        packet.set(
            "items",
            json!([{"id": "new-one", "text": "hi", "modified": now + 1},
                   {"id": "s1", "text": "changed", "modified": now + 1}]),
        );
        handle(&mut shelf, Kind::Snippets, &packet);
        assert!(
            !shelf.snippets.contains_key("new-one"),
            "full: a new id is refused"
        );
        assert_eq!(
            shelf.snippets["s1"].text, "changed",
            "a known one still updates"
        );
        // At most MAX_ITEMS_PER_PACKET items are read from one packet.
        let mut empty = Shelf::default();
        let many: Vec<Value> = (0..MAX_ITEMS_PER_PACKET + 50)
            .map(|index| json!({"id": format!("n{index}"), "text": "t", "modified": now}))
            .collect();
        packet.set("items", many);
        handle(&mut empty, Kind::Snippets, &packet);
        assert_eq!(empty.snippets.len(), MAX_ITEMS_PER_PACKET);
    }

    #[test]
    fn a_note_too_long_is_refused_out_loud_and_kept_as_it_was() {
        let dir = std::env::temp_dir().join(format!("desklink-notes-{}", uuid::Uuid::new_v4()));
        install(dir.clone());
        let id = new_note("short").unwrap();
        let result = edit_note(&id, |note| note.body = "x".repeat(MAX_NOTE_BYTES + 1));
        assert_eq!(result, Err(TOO_LONG.to_string()));
        assert_eq!(
            super::note(&id).unwrap().body,
            "short",
            "the saved note is unchanged"
        );
        assert_eq!(edit_note(&id, |note| note.body = "fine".into()), Ok(()));
        let _ = std::fs::remove_dir_all(dir);
    }

    fn note(id: &str, body: &str, modified: i64, parent: i64) -> Note {
        Note {
            id: id.into(),
            title: String::new(),
            body: body.into(),
            pinned: false,
            created: 1,
            modified,
            parent,
            deleted: false,
        }
    }

    fn shelf_with(notes: &[Note]) -> Shelf {
        let mut shelf = Shelf::default();
        for n in notes {
            shelf.notes.insert(n.id.clone(), n.clone());
            shelf.synced.insert(n.id.clone(), n.parent);
        }
        shelf
    }

    #[test]
    fn a_note_edited_from_ours_is_taken_and_ours_edited_from_theirs_is_kept() {
        // Agreed version at 10.
        let mut shelf = shelf_with(&[note("n", "v1", 10, 0)]);
        assert_eq!(
            merge_note(&mut shelf, note("n", "v2", 20, 10)),
            Merged::Taken
        );
        assert_eq!(shelf.notes["n"].body, "v2");
        assert_eq!(shelf.synced["n"], 20);
        // We edit (from 20); the peer still has 20.
        shelf.notes.insert("n".into(), note("n", "v3", 30, 20));
        assert_eq!(
            merge_note(&mut shelf, note("n", "v2", 20, 10)),
            Merged::KeptOurs
        );
        assert_eq!(shelf.notes["n"].body, "v3");
        assert_eq!(
            merge_note(&mut shelf, note("n", "v3", 30, 20)),
            Merged::Same
        );
    }

    /// N2: both sides resolve two concurrent edits to the same two notes.
    #[test]
    fn concurrent_edits_keep_both_alike_on_both_sides() {
        let desk = note("n", "desk text", 31, 20);
        let phone = note("n", "phone text", 35, 20);
        let mut here = shelf_with(std::slice::from_ref(&desk));
        let mut there = shelf_with(std::slice::from_ref(&phone));
        assert_eq!(merge_note(&mut here, phone.clone()), Merged::Conflict);
        assert_eq!(merge_note(&mut there, desk.clone()), Merged::Conflict);
        assert_eq!(here.notes, there.notes, "the same result on both sides");
        assert_eq!(here.notes["n"].body, "phone text", "the newer keeps the id");
        let copy = &here.notes["n~31"];
        assert_eq!(copy.body, "desk text");
        assert_eq!(copy.title, "desk text (conflicted copy)");
        // Seen again (an index round later): nothing changes.
        assert_eq!(merge_note(&mut here, desk), Merged::KeptOurs);
        assert_eq!(merge_note(&mut there, phone), Merged::Same);
        assert_eq!(here.notes.len(), 2);
        // The copy, sent anyway, is a no-op.
        let copy = here.notes["n~31"].clone();
        assert_eq!(merge_note(&mut there, copy), Merged::Same);
    }

    #[test]
    fn a_deletion_never_beats_a_concurrent_edit() {
        let mut deleted = note("n", "", 40, 20);
        deleted.deleted = true;
        let edited = note("n", "kept", 35, 20);
        let mut here = shelf_with(std::slice::from_ref(&edited));
        merge_note(&mut here, deleted.clone());
        assert!(!here.notes["n"].deleted);
        assert_eq!(here.notes.len(), 1, "no copy of a deletion");
        let mut there = shelf_with(&[deleted]);
        merge_note(&mut there, edited);
        assert_eq!(there.notes["n"].body, "kept", "the note comes back");
    }

    #[test]
    fn snippets_take_the_newer_and_fetch_what_differs() {
        let snippet = |id: &str, text: &str, modified| Snippet {
            id: id.into(),
            text: text.into(),
            title: String::new(),
            modified,
            deleted: false,
        };
        let mut shelf = Shelf::default();
        assert!(merge_snippet(&mut shelf, snippet("a", "one", 10)));
        assert!(!merge_snippet(&mut shelf, snippet("a", "old", 5)));
        assert!(
            !merge_snippet(&mut shelf, snippet("a", "tie", 10)),
            "equal: keep ours"
        );
        assert!(merge_snippet(&mut shelf, snippet("a", "two", 11)));
        let index = vec![
            json!({"id": "a", "modified": 11}),
            json!({"id": "b", "modified": 1}),
            json!({"id": "a2", "modified": 3}),
        ];
        shelf.snippets.insert("a2".into(), snippet("a2", "x", 9));
        assert_eq!(wanted(&shelf, Kind::Snippets, &index), vec!["b"]);
        // Notes fetch any difference.
        let mut notes = shelf_with(&[note("n", "x", 5, 0)]);
        notes.notes.insert("m".into(), note("m", "y", 9, 0));
        let index = vec![
            json!({"id": "n", "modified": 4}),
            json!({"id": "m", "modified": 9}),
        ];
        assert_eq!(wanted(&notes, Kind::Notes, &index), vec!["n"]);
    }

    #[test]
    fn an_index_round_trip_brings_a_peer_up_to_date() {
        let mut desk = Shelf::default();
        desk.notes.insert("n".into(), note("n", "hello", 10, 0));
        let mut phone = Shelf::default();
        // Phone gets the desk's index, fetches, the desk answers.
        let (fetches, _) = handle(&mut phone, Kind::Notes, &index_packet(&desk, Kind::Notes));
        assert_eq!(fetches.len(), 1);
        let (items, _) = handle(&mut desk, Kind::Notes, &fetches[0]);
        let (back, changed) = handle(&mut phone, Kind::Notes, &items[0]);
        assert!(changed && back.is_empty());
        assert_eq!(phone.notes["n"].body, "hello");
        // Junk and over-bound items are dropped.
        let mut junk = NetworkPacket::new(PACKET_TYPE_NOTES);
        junk.set("op", "items");
        junk.set(
            "items",
            json!([{"id": "", "modified": 1, "created": 1},
                   {"id": "big", "modified": 1, "created": 1, "body": "x".repeat(MAX_NOTE_BYTES + 1)},
                   "nonsense"]),
        );
        let (_, changed) = handle(&mut phone, Kind::Notes, &junk);
        assert!(!changed);
    }

    #[test]
    fn big_replies_are_split_and_tombstones_expire() {
        let values: Vec<Value> = (0..40)
            .map(|n| json!({"id": n.to_string(), "body": "x".repeat(16 * 1024)}))
            .collect();
        let packets = items_packets(Kind::Notes, values);
        assert!(packets.len() >= 3);
        assert!(packets
            .iter()
            .all(|packet| serde_json::to_string(&packet.body).unwrap().len()
                < MAX_PACKET_BYTES + 1024));
        let mut shelf = Shelf::default();
        let mut old = note("old", "", 1, 0);
        old.deleted = true;
        shelf.notes.insert("old".into(), old);
        shelf.notes.insert("live".into(), note("live", "x", 1, 0));
        prune(&mut shelf, 100 * 24 * 3600 * 1000);
        assert!(!shelf.notes.contains_key("old") && shelf.notes.contains_key("live"));
    }
}
