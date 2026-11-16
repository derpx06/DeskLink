//! Taking what a peer sends: validation (over a bound is dropped, not
//! truncated), merging snippets and notes (a concurrent edit becomes a
//! conflicted copy), acknowledgements and removing duplicate copies.
//!
//! Sibling edges: `model`.

use super::*;

// ── Validation (on receipt: over a bound is dropped, not truncated) ────────

pub(super) fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().count() <= MAX_ID_CHARS && !id.chars().any(char::is_control)
}

pub(super) fn valid_snippet(snippet: &Snippet) -> bool {
    valid_id(&snippet.id)
        && snippet.text.len() <= MAX_SNIPPET_BYTES
        && snippet.title.chars().count() <= MAX_SNIPPET_TITLE_CHARS
        && (snippet.deleted || !snippet.text.is_empty())
        && snippet.modified > 0
}

pub(super) fn valid_note(note: &Note) -> bool {
    valid_id(&note.id)
        && note.ancestors.len() <= MAX_ANCESTORS
        && note.body.len() <= MAX_NOTE_BYTES
        && note.title.chars().count() <= MAX_NOTE_TITLE_CHARS
        && note.modified > 0
}

// ── Merging ────────────────────────────────────────────────────────────────

/// S2: the newer snippet wins. Whether `incoming` was taken.
pub(super) fn merge_snippet(shelf: &mut Shelf, incoming: Snippet) -> bool {
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
pub(super) enum Merged {
    /// Ours stays; the peer has an older version (send it ours).
    KeptOurs,
    Taken,
    /// Already the same.
    Same,
    /// Siblings: both are kept now.
    Conflict,
}

/// N2, for a note that came from no particular phone (a retry after an unreadable start).
pub(super) fn merge_note(shelf: &mut Shelf, incoming: Note) -> Merged {
    merge_note_from(shelf, incoming, "")
}

/// N2: `incoming` came from phone `peer` (empty: unknown), which therefore holds it.
pub(super) fn merge_note_from(shelf: &mut Shelf, incoming: Note, peer: &str) -> Merged {
    let Some(local) = shelf.notes.get(&incoming.id).cloned() else {
        shelf.synced.insert(incoming.id.clone(), incoming.modified);
        shelf.record_held(&incoming.id, peer, incoming.modified);
        shelf.notes.insert(incoming.id.clone(), incoming);
        return Merged::Taken;
    };
    // Equal `modified` is the same version only if it says the same (two sides
    // can edit in the same millisecond); otherwise they are siblings.
    if local.modified == incoming.modified && local.same_content(&incoming) {
        shelf.synced.insert(local.id.clone(), local.modified);
        shelf.record_held(&local.id, peer, local.modified);
        return Merged::Same;
    }
    // Edited from what we hold (directly, or some edits ago): ours is an
    // ancestor of theirs, so theirs replaces it.
    if incoming.parent == local.modified || incoming.ancestors.contains(&local.modified) {
        shelf.synced.insert(incoming.id.clone(), incoming.modified);
        shelf.record_held(&incoming.id, peer, incoming.modified);
        shelf.notes.insert(incoming.id.clone(), incoming);
        return Merged::Taken;
    }
    if local.parent == incoming.modified || local.ancestors.contains(&incoming.modified) {
        // Its version is an ancestor of ours: that phone holds it.
        shelf.record_held(&local.id, peer, incoming.modified);
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
    let incoming_modified = incoming.modified;
    let (winner, loser) = if incoming_wins {
        (incoming, local)
    } else {
        (local, incoming)
    };
    let conflict = !winner.same_content(&loser) && !winner.deleted && !loser.deleted;
    let copy = conflict.then(|| Note {
        id: format!("{}~{}", loser.id, loser.modified),
        title: format!("{} {COPY_SUFFIX}", loser.label()),
        parent: loser.modified,
        ..loser
    });
    // A copy that is already here, as it was, edited or deleted, was resolved
    // before: it is never written over (that reverted the person's edits to it
    // and brought deleted copies back).
    let copy = copy.filter(|copy| !shelf.notes.contains_key(&copy.id));
    if !incoming_wins && (!conflict || copy.is_none()) {
        return Merged::KeptOurs;
    }
    let fresh_copy = copy.is_some();
    if let Some(copy) = copy {
        shelf.synced.insert(copy.id.clone(), copy.modified);
        shelf.notes.insert(copy.id.clone(), copy);
    }
    shelf.synced.insert(winner.id.clone(), winner.modified);
    if incoming_wins {
        shelf.record_held(&winner.id, peer, incoming_modified);
    }
    shelf.notes.insert(winner.id.clone(), winner);
    if fresh_copy {
        Merged::Conflict
    } else {
        Merged::Taken
    }
}

/// Phone `peer` holds `modified` of note `id` (it took it, or listed it): the
/// version our next edit names as its `parent`, if it is one of ours and newer
/// than what that phone was known to hold. Whether anything moved.
pub(super) fn acknowledge(shelf: &mut Shelf, id: &str, modified: i64, peer: &str) -> bool {
    let Some(note) = shelf.notes.get(id) else {
        return false;
    };
    let chain: Vec<i64> = std::iter::once(note.modified)
        .chain(note.ancestors.iter().copied())
        .collect();
    let Some(at) = chain.iter().position(|m| *m == modified) else {
        return false;
    };
    let position = |version: Option<&i64>| {
        version
            .and_then(|version| chain.iter().position(|m| m == version))
            .unwrap_or(usize::MAX)
    };
    let mut moved = false;
    if at < position(shelf.synced.get(id)) {
        shelf.synced.insert(id.to_string(), modified);
        moved = true;
    }
    if !peer.is_empty() && at < position(shelf.held.get(id).and_then(|peers| peers.get(peer))) {
        shelf.record_held(id, peer, modified);
        moved = true;
    }
    moved
}

// ── Duplicate conflicted copies ───────────────────────────────────────────

/// The note a conflicted copy was made from: copies are `<id>~<modified>`.
fn copy_base(id: &str) -> Option<&str> {
    let (base, modified) = id.rsplit_once('~')?;
    (!base.is_empty() && !modified.is_empty() && modified.bytes().all(|b| b.is_ascii_digit()))
        .then_some(base)
}

pub(super) fn is_copy(note: &Note) -> bool {
    copy_base(&note.id).is_some() && note.title.ends_with(COPY_SUFFIX)
}

/// What a note says, for telling duplicates: a copy's title without the suffix,
/// or a note's whole title (its label when it has none), and the body. Whole
/// titles, not the 80-character label: two titles that only agree up to there
/// are different.
pub(super) fn content_key(note: &Note) -> (String, &str) {
    let mut title = note.title.as_str();
    if is_copy(note) {
        while let Some(rest) = title.strip_suffix(COPY_SUFFIX) {
            title = trim_note(rest);
        }
        return (title.to_string(), note.body.as_str());
    }
    let title = match trim_note(title) {
        "" => note.label(),
        title => title.to_string(),
    };
    (title, note.body.as_str())
}

/// Which live copies to delete, each with the note that stays and says the same.
/// A copy goes only when something that stays says the same: `everywhere` is the
/// person's "Remove duplicate copies" (any other live note, or the lowest-id one
/// of identical copies); without it, only the note it was made from, or an
/// identical copy of the same note. A copy whose text is anywhere else unique is
/// never listed. Each key is computed once, and only for the notes whose body is
/// as long as some copy's.
pub(super) fn duplicate_copies(shelf: &Shelf, everywhere: bool) -> Vec<(String, String)> {
    let copies: Vec<&Note> = shelf
        .notes
        .values()
        .filter(|note| !note.deleted && is_copy(note))
        .collect();
    if copies.is_empty() {
        return Vec::new();
    }
    let lengths: std::collections::HashSet<usize> =
        copies.iter().map(|copy| copy.body.len()).collect();
    let mut groups: std::collections::HashMap<(String, &str), Vec<&Note>> =
        std::collections::HashMap::new();
    for note in shelf
        .notes
        .values()
        .filter(|note| !note.deleted && lengths.contains(&note.body.len()))
    {
        groups.entry(content_key(note)).or_default().push(note);
    }
    let mut found = Vec::new();
    for copy in copies {
        let base = copy_base(&copy.id).unwrap_or_default();
        let Some(group) = groups.get(&content_key(copy)) else {
            continue;
        };
        // Something that is not itself a copy, or the made-from note, says it too.
        let by_original = group.iter().find(|other| {
            !is_copy(other) && other.id != copy.id && (everywhere || other.id == base)
        });
        // An identical copy with a lower id stays (ids are the same on both sides).
        let by_sibling = group.iter().find(|other| {
            is_copy(other)
                && other.id < copy.id
                && (everywhere || copy_base(&other.id) == Some(base))
        });
        if let Some(survivor) = by_original.or(by_sibling) {
            found.push((copy.id.clone(), survivor.id.clone()));
        }
    }
    found
}

/// Deletes (as the person would: a tombstone that syncs) the copies
/// [`duplicate_copies`] lists. A pinned copy pins the note that stays, so a pin
/// is never lost. The ids changed (to send).
pub(super) fn remove_duplicates_in(shelf: &mut Shelf, everywhere: bool) -> Vec<String> {
    let doomed: std::collections::BTreeMap<String, String> =
        duplicate_copies(shelf, everywhere).into_iter().collect();
    let mut to_pin = std::collections::BTreeSet::new();
    for (copy, survivor) in &doomed {
        // The survivor may itself be going (a lower-id copy): follow to what stays.
        let mut stays = survivor;
        while let Some(next) = doomed.get(stays) {
            stays = next;
        }
        let pinned = |id: &String| shelf.notes.get(id).is_some_and(|note| note.pinned);
        if pinned(copy) && !pinned(stays) {
            to_pin.insert(stays.clone());
        }
    }
    let mut changed = Vec::new();
    for id in to_pin {
        if edit_from_in_shelf(shelf, &id, None, |note| note.pinned = true)
            .is_ok_and(|(done, _)| done)
        {
            changed.push(id);
        }
    }
    for id in doomed.into_keys() {
        if edit_from_in_shelf(shelf, &id, None, |note| note.deleted = true)
            .is_ok_and(|(done, _)| done)
        {
            changed.push(id);
        }
    }
    changed
}

/// [`edit_in_shelf`] for an editor that loaded version `loaded` of the note:
/// if the stored note has moved on since (the other side's edit landed while
/// the person was typing), what arrived is kept as a "(conflicted copy)"
/// instead of being overwritten silently. Also returns the copy's id.
pub(super) fn edit_from_in_shelf(
    shelf: &mut Shelf,
    id: &str,
    loaded: Option<i64>,
    change: impl FnOnce(&mut Note),
) -> Result<(bool, Option<String>), String> {
    let base = shelf.base_of(id);
    let keep = shelf.must_keep(id);
    let Some(note) = shelf.notes.get_mut(id) else {
        return Ok((false, None));
    };
    let before = note.clone();
    // The version the person typed over is not the one stored now.
    // (A tombstone is not a version the person typed over: no copy of it.)
    let overtaken = loaded
        .filter(|loaded| *loaded != before.modified && !before.deleted)
        .map(|_| Note {
            id: format!("{}~{}", before.id, before.modified),
            title: format!("{} {COPY_SUFFIX}", before.label()),
            parent: before.modified,
            ..before.clone()
        });
    change(note);
    // Text typed over a note that was deleted underneath the editor is kept, not
    // wiped: it brings the note back (as an edit of the deletion).
    if before.deleted && note.deleted && !(note.title.is_empty() && note.body.is_empty()) {
        note.deleted = false;
    }
    note.title = note.title.chars().take(MAX_NOTE_TITLE_CHARS).collect();
    // The bound on what crosses the wire, escapes included.
    if note.body.len() > MAX_NOTE_BYTES || !fits_one_packet(&*note) {
        *note = before;
        return Err(TOO_LONG.to_string());
    }
    if note.same_content(&before) {
        return Ok((false, None));
    }
    if note.deleted {
        note.title.clear();
        note.body.clear();
    }
    note.parent = base;
    note.ancestors = ancestors_after(&before, &keep);
    // Strictly later, even within one millisecond.
    note.modified = now_ms().max(before.modified.saturating_add(1));
    // Never over a copy that is already here (edited, or deleted on purpose).
    let overtaken = overtaken.filter(|copy| !shelf.notes.contains_key(&copy.id));
    let copy_id = overtaken.map(|copy| {
        let copy_id = copy.id.clone();
        shelf.synced.insert(copy_id.clone(), copy.modified);
        shelf.notes.insert(copy_id.clone(), copy);
        copy_id
    });
    Ok((true, copy_id))
}
