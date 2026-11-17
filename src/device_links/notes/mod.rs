//! Snippets and notes (DeskLink 1.2, F6): the user's, kept here encrypted
//! whatever happens to the pairing, and synced with each connected phone.
//!
//! Both sides send an index on feature-ready and fetch what they lack or
//! what differs. Snippets: the newer `modified` wins. Notes: a version
//! edited from the other is taken; two versions edited from the same one
//! are both kept — the newer under the id, the other as a "conflicted copy"
//! under an id both sides derive alike, so neither has to send it.
//!
//! Layout: this file holds the limits, the sealed store and its packet entry
//! point, relaying to other phones and the shared size helpers; `model` the
//! snippet and note types, `merge` taking what a peer sends, `wire` the
//! packets, `edits` the window's changes. No cycles.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::packet::{NetworkPacket, PACKET_TYPE_NOTES, PACKET_TYPE_SNIPPETS};

mod edits;
mod merge;
mod model;
mod wire;
#[cfg(test)]
use edits::*;
use merge::*;
use model::*;
use wire::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use edits::{
    add_snippet, delete_snippet, duplicate_copies_preview, edit_note, edit_note_from, new_note,
    new_note_refusal, note, notes, remove_duplicate_copies, snippets,
};
pub(crate) use model::{Note, Snippet};

const MAX_ID_CHARS: usize = 128;
const MAX_SNIPPETS: usize = 500;
const MAX_SNIPPET_BYTES: usize = 16 * 1024;
const MAX_SNIPPET_TITLE_CHARS: usize = 120;
const MAX_NOTES: usize = 2000;
const MAX_NOTE_TITLE_CHARS: usize = 200;
/// A note's body: its JSON (escaped) must fit one packet on the wire, so
/// half the packet budget.
const MAX_NOTE_BYTES: usize = 20 * 1024;
/// ids per `fetch`.
const FETCH_IDS: usize = 100;
/// One `items` packet stays one data-channel message once enveloped.
const MAX_PACKET_BYTES: usize = crate::device_links::webrtc::MAX_FEATURE_PACKET_BYTES;
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
    remove_duplicates_in(&mut shelf, false);
    if let Ok(mut store) = store().lock() {
        *store = Some(Store { dir, shelf });
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// "Try Again" after the shelf could not be read: reads the file again; if
/// that works, what was typed meanwhile (kept in memory) is merged into what
/// the file holds and saved. `true` when the shelf can be saved again.
pub(crate) fn retry_blocked() -> bool {
    let cleared = super::vault::clear_blocked_under("notes");
    if cleared.is_empty() {
        return true;
    }
    let Ok(mut guard) = store().lock() else {
        return false;
    };
    let Some(store) = guard.as_mut() else {
        return false;
    };
    let mut disk: Shelf = super::vault::open_or_quarantine(&store.dir, &store.dir.join(FILE), AAD);
    if !super::vault::blocked_under("notes").is_empty() {
        return false;
    }
    let typed = std::mem::take(&mut store.shelf);
    for (_, snippet) in typed.snippets {
        merge_snippet(&mut disk, snippet);
    }
    for (_, note) in typed.notes {
        merge_note(&mut disk, note);
    }
    store.shelf = disk;
    if let Ok(plain) = serde_json::to_vec(&store.shelf) {
        super::vault::seal_later(&store.dir, &store.dir.join(FILE), AAD, plain);
    }
    GENERATION.fetch_add(1, Ordering::Relaxed);
    true
}

/// "Start Fresh": gives up on the old file (a copy of it is kept beside it)
/// and saves what is in memory, typed since, over it.
pub(crate) fn start_fresh_blocked() {
    super::vault::clear_blocked_under("notes");
    with_shelf(|_| ((), true));
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
    shelf.held.retain(|id, _| notes.contains_key(id));
    shelf.sent.retain(|id, _| notes.contains_key(id));
}

/// Runs `job` on the shelf; saves it and bumps the generation when it says
/// it changed it. After any change, identical conflicted copies are cleared
/// (the same trigger on the phone) and the result is saved with it and sent.
fn with_shelf<T>(job: impl FnOnce(&mut Shelf) -> (T, bool)) -> Option<T> {
    let mut guard = store().lock().ok()?;
    let store = guard.as_mut()?;
    let (result, changed) = job(&mut store.shelf);
    let mut tidied = Vec::new();
    if changed {
        tidied = remove_duplicates_in(&mut store.shelf, false);
        // Sealed and written off this thread (often the GTK one, typing).
        match serde_json::to_vec(&store.shelf) {
            Ok(plain) => super::vault::seal_later(&store.dir, &store.dir.join(FILE), AAD, plain),
            Err(_) => eprintln!("[Notes] Could not save snippets and notes"),
        }
        GENERATION.fetch_add(1, Ordering::Relaxed);
    }
    drop(guard);
    if !tidied.is_empty() {
        push(Kind::Notes, tidied);
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
    let before = GENERATION.load(Ordering::Relaxed);
    let answers =
        with_shelf(|shelf| handle_from(shelf, kind, packet, device_id)).unwrap_or_default();
    // What one phone changed reaches the others now, not when they next
    // reconnect (I15).
    if packet.get_str("op") == Some("items") && GENERATION.load(Ordering::Relaxed) != before {
        let others = relay_targets(&super::desk_features::ready_devices(), device_id);
        if !others.is_empty() {
            let relayed = packet.clone();
            mark_relayed(&others, &relayed);
            super::desk_features::run_later(move || {
                for device in &others {
                    super::desk_features::send(device, &relayed);
                }
            });
        }
    }
    debug_log!(
        "[Notes] {} {} from {device_id}: {} answers",
        packet.packet_type,
        packet.get_str("op").unwrap_or("?"),
        answers.len()
    );
    answers
}

/// A packet of notes relayed to `others`: they will hold those versions.
fn mark_relayed(others: &[String], packet: &NetworkPacket) {
    if packet.packet_type != PACKET_TYPE_NOTES {
        return;
    }
    let items: Vec<(String, i64)> = packet
        .body
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|item| {
            Some((
                item.get("id")?.as_str()?.to_string(),
                item.get("modified")?.as_i64()?,
            ))
        })
        .collect();
    with_shelf(|shelf| {
        for device in others {
            for (id, modified) in &items {
                shelf.record_sent(id, device, *modified);
            }
        }
        ((), false)
    });
}

/// A phone was unpaired: what the notes remembered of it goes (the notes stay).
pub(crate) fn forget_peer(device_id: &str) {
    with_shelf(|shelf| {
        shelf.forget_phone(device_id);
        ((), false)
    });
}

/// The ready phones other than `source`: where a change it made goes next.
fn relay_targets(ready: &[String], source: &str) -> Vec<String> {
    ready
        .iter()
        .filter(|device| device.as_str() != source)
        .cloned()
        .collect()
}

/// The shared `notes_merge.json` case: `local` (or null) meets `incoming`;
/// the outcome as the fixture names it.
#[cfg(test)]
pub(crate) fn merge_for_fixture(local: &serde_json::Value, incoming: &serde_json::Value) -> String {
    let note = |value: &serde_json::Value| -> Note {
        let mut value = value.clone();
        let object = value.as_object_mut().unwrap();
        object.entry("created").or_insert(serde_json::json!(1));
        serde_json::from_value(value).unwrap()
    };
    let mut shelf = Shelf::default();
    if !local.is_null() {
        let local = note(local);
        shelf.notes.insert(local.id.clone(), local);
    }
    match merge_note(&mut shelf, note(incoming)) {
        Merged::Taken => "taken",
        Merged::KeptOurs => "keptOurs",
        Merged::Same => "same",
        Merged::Conflict => "conflict",
    }
    .to_string()
}

/// Both indexes, for a phone whose connection just came up: one group of
/// packets per kind (a group is sent whole or not at all).
pub(crate) fn indexes() -> Vec<Vec<NetworkPacket>> {
    with_shelf(|shelf| {
        (
            vec![
                index_packets(shelf, Kind::Snippets),
                index_packets(shelf, Kind::Notes),
            ],
            false,
        )
    })
    .unwrap_or_default()
}

/// Sends `ids` to every connected phone (a local change).
fn push(kind: Kind, ids: Vec<String>) {
    let devices = super::desk_features::ready_devices();
    let Some(packets) = with_shelf(|shelf| {
        let packets = items_packets(kind, item_values(shelf, kind, &ids));
        // Each of them will hold what is being sent (or the version it held).
        for device in &devices {
            sent_to(shelf, kind, &ids, device);
        }
        (packets, false)
    }) else {
        return;
    };
    super::desk_features::run_later(move || {
        for device in &devices {
            for packet in &packets {
                super::desk_features::send(device, packet);
            }
        }
    });
}

/// What the editor says when a note passes its size bound.
pub(crate) const TOO_LONG: &str =
    "This note is too long to save (20 KiB at most). Shorten it: the last changes are not saved yet.";

/// The `items` packet's own JSON around its items.
const PACKET_OVERHEAD: usize = 256;

/// Whether `item` can go in an `items` packet at all.
fn fits_one_packet(item: &impl Serialize) -> bool {
    serde_json::to_string(item).is_ok_and(|json| json.len() < MAX_PACKET_BYTES - PACKET_OVERHEAD)
}

#[cfg(test)]
mod relay_tests;
#[cfg(test)]
mod sync_scenarios;
#[cfg(test)]
pub(crate) mod tests;
