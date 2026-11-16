//! The snippets/notes packets: indexes within their byte budget, items within
//! one packet, what to fetch, and handling a peer's packet.
//!
//! Sibling edges: `model`, `merge` (applying what arrives).

use super::*;

/// The ids in a peer's index we should fetch.
pub(super) fn wanted(shelf: &Shelf, kind: Kind, index: &[Value]) -> Vec<String> {
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
pub(super) enum Kind {
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

    pub(super) fn from_packet(packet_type: &str) -> Option<Self> {
        match packet_type {
            PACKET_TYPE_SNIPPETS => Some(Kind::Snippets),
            PACKET_TYPE_NOTES => Some(Kind::Notes),
            _ => None,
        }
    }
}

/// The most JSON bytes of index entries in one packet (the phone splits the
/// same way): over about 570 entries one packet no longer fitted the 64 KiB
/// wire message and the phone never got the desk's index.
pub(super) const INDEX_BUDGET: usize = 40 * 1024;

/// The index of `kind` as one or more packets, each within [`INDEX_BUDGET`]
/// (at least one, even for an empty shelf).
pub(super) fn index_packets(shelf: &Shelf, kind: Kind) -> Vec<NetworkPacket> {
    let entries = index_entries(shelf, kind);
    let mut parts: Vec<Vec<Value>> = vec![Vec::new()];
    let mut size = 0;
    for entry in entries {
        let length = entry.to_string().len() + 1;
        if size + length > INDEX_BUDGET && parts.last().is_some_and(|part| !part.is_empty()) {
            parts.push(Vec::new());
            size = 0;
        }
        size += length;
        if let Some(part) = parts.last_mut() {
            part.push(entry);
        }
    }
    parts
        .into_iter()
        .map(|items| {
            let mut packet = NetworkPacket::new(kind.packet_type());
            packet.set("op", "index");
            packet.set("items", items);
            packet
        })
        .collect()
}

#[cfg(test)]
pub(super) fn index_packet(shelf: &Shelf, kind: Kind) -> NetworkPacket {
    let mut parts = index_packets(shelf, kind);
    assert_eq!(parts.len(), 1, "a test index fits one packet");
    parts.remove(0)
}

/// A note nobody has written in or edited yet.
fn is_blank_new(note: &Note) -> bool {
    !note.deleted
        && note.ancestors.is_empty()
        && note.parent == 0
        && trim_note(&note.title).is_empty()
        && trim_note(&note.body).is_empty()
}

pub(super) fn index_entries(shelf: &Shelf, kind: Kind) -> Vec<Value> {
    match kind {
        Kind::Snippets => shelf
            .snippets
            .values()
            .map(|s| json!({"id": s.id, "modified": s.modified, "deleted": s.deleted}))
            .collect(),
        Kind::Notes => shelf
            .notes
            .values()
            // A note made with "+" and never written in stays here until its first
            // edit (which sends it): it used to fill the phone's list with blanks.
            .filter(|n| !is_blank_new(n))
            .map(|n| json!({"id": n.id, "modified": n.modified, "deleted": n.deleted}))
            .collect(),
    }
}

/// `items` packets for `values`, split to stay under the size bound.
pub(super) fn items_packets(kind: Kind, values: Vec<Value>) -> Vec<NetworkPacket> {
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
        let length = value.to_string().len() + 1;
        // One that cannot fit any packet is never sent (edits are refused
        // before they get this big; see `fits_one_packet`).
        if length > MAX_PACKET_BYTES - PACKET_OVERHEAD {
            crate::device_links::limits::log_limited(
                "notes-item-size",
                "An item too large to sync was skipped",
            );
            continue;
        }
        if size + length > MAX_PACKET_BYTES - PACKET_OVERHEAD {
            flush(&mut batch, &mut packets);
            size = 0;
        }
        size += length;
        batch.push(value);
    }
    flush(&mut batch, &mut packets);
    packets
}

pub(super) fn item_values(shelf: &Shelf, kind: Kind, ids: &[String]) -> Vec<Value> {
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

/// [`handle_from`] for a packet from no particular phone.
#[cfg(test)]
pub(super) fn handle(
    shelf: &mut Shelf,
    kind: Kind,
    packet: &NetworkPacket,
) -> (Vec<NetworkPacket>, bool) {
    handle_from(shelf, kind, packet, "")
}

/// A packet from phone `peer`: what to send back. `changed` says whether the
/// shelf changed (to save).
pub(super) fn handle_from(
    shelf: &mut Shelf,
    kind: Kind,
    packet: &NetworkPacket,
    peer: &str,
) -> (Vec<NetworkPacket>, bool) {
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
            // What the peer lists exactly as we hold it, it holds: the base of our next edit.
            let mut changed = false;
            if kind == Kind::Notes {
                for entry in &index {
                    if let (Some(id), Some(modified)) = (
                        entry.get("id").and_then(Value::as_str),
                        entry.get("modified").and_then(Value::as_i64),
                    ) {
                        changed |= acknowledge(shelf, id, modified, peer);
                    }
                }
            }
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
            (fetches, changed)
        }
        Some("ack") if kind == Kind::Notes => {
            // The peer took these versions of ours (older builds never send this).
            let mut changed = false;
            for entry in packet
                .body
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .take(MAX_ITEMS_PER_PACKET)
            {
                if let (Some(id), Some(modified)) = (
                    entry.get("id").and_then(Value::as_str),
                    entry.get("modified").and_then(Value::as_i64),
                ) {
                    changed |= acknowledge(shelf, id, modified, peer);
                }
            }
            (Vec::new(), changed)
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
            sent_to(shelf, kind, &ids, peer);
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
            let mut held = Vec::new();
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
                            crate::device_links::limits::log_limited(
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
                        let modified = note.modified;
                        // A full shelf takes only what adds no entry: a straight
                        // edit of a note it holds (or the same version, or one
                        // it already descends from). A new id needs room, and so
                        // does a sibling edit, which makes a conflict copy: let
                        // through, those grew the shelf past its cap.
                        let adds_no_entry = shelf
                            .notes
                            .get(&id)
                            .is_some_and(|local| super::merge::not_siblings(local, &note));
                        if !adds_no_entry && shelf.notes.len() >= MAX_NOTE_ENTRIES {
                            crate::device_links::limits::log_limited(
                                "notes-full",
                                "Notes are full; one was refused",
                            );
                            continue;
                        }
                        match merge_note_from(shelf, note, peer) {
                            Merged::KeptOurs => send_back.push(id),
                            Merged::Same => held.push((id, modified)),
                            Merged::Taken => {
                                changed = true;
                                held.push((id, modified));
                            }
                            // The peer resolves it alike once it has ours.
                            Merged::Conflict => {
                                changed = true;
                                send_back.push(id);
                            }
                        }
                    }
                }
            }
            sent_to(shelf, kind, &send_back, peer);
            let mut answers = items_packets(kind, item_values(shelf, kind, &send_back));
            // Tell the sender it now has these versions here (its next edit then
            // names one as its base instead of an older one).
            for chunk in held.chunks(FETCH_IDS) {
                let mut ack = NetworkPacket::new(kind.packet_type());
                ack.set("op", "ack");
                ack.set(
                    "items",
                    chunk
                        .iter()
                        .map(|(id, modified)| json!({"id": id, "modified": modified}))
                        .collect::<Vec<_>>(),
                );
                answers.push(ack);
            }
            (answers, changed)
        }
        _ => (Vec::new(), false),
    }
}

/// Notes `ids` are going to phone `peer`: it will hold that version, or the one
/// it held, so later edits keep both among their ancestors.
pub(super) fn sent_to(shelf: &mut Shelf, kind: Kind, ids: &[String], peer: &str) {
    if kind != Kind::Notes || peer.is_empty() {
        return;
    }
    for id in ids {
        if let Some(modified) = shelf.notes.get(id).map(|note| note.modified) {
            shelf.record_sent(id, peer, modified);
        }
    }
}
