//! Local edits from the window: adding and deleting snippets, creating,
//! editing, pinning and deleting notes, and the limits' words.
//!
//! Sibling edges: `model`, `merge` (copies and duplicates), `wire` (pushing
//! a change).

use super::*;

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
        return Err("Too long for a snippet (16 KiB at most)".into());
    }
    let id = with_shelf(|shelf| {
        let added = add_in_shelf(shelf, text);
        let saved = added.is_ok();
        (added, saved)
    })
    .ok_or("Snippets are not available")??;
    push(Kind::Snippets, vec![id]);
    Ok(())
}

/// The refusal when a shelf holds as many live snippets as it may.
pub(super) const TOO_MANY_SNIPPETS: &str = "You have 500 snippets. Unpin one first.";

/// Pins `text` in `shelf`: its id. An identical snippet is moved to the top;
/// a new one is refused at [`MAX_SNIPPETS`] live (it used to be dropped from
/// view past that, and kept, hidden, in the file).
pub(super) fn add_in_shelf(shelf: &mut Shelf, text: &str) -> Result<String, String> {
    let existing = shelf
        .snippets
        .values()
        .find(|snippet| !snippet.deleted && snippet.text == text)
        .map(|snippet| snippet.id.clone());
    if existing.is_none()
        && shelf
            .snippets
            .values()
            .filter(|snippet| !snippet.deleted)
            .count()
            >= MAX_SNIPPETS
    {
        return Err(TOO_MANY_SNIPPETS.to_string());
    }
    let id = existing.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    shelf.snippets.insert(
        id.clone(),
        Snippet {
            id: id.clone(),
            text: text.to_string(),
            title: String::new(),
            modified: now_ms(),
            deleted: false,
        },
    );
    Ok(id)
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
        let id = create_in_shelf(shelf, body);
        let saved = id.is_some();
        (id, saved)
    })
    .flatten()?;
    // An empty note stays here until it has something in it (its first edit
    // pushes it): the "+" button used to fill the phone's list with blanks.
    if !body.trim().is_empty() {
        push(Kind::Notes, vec![id.clone()]);
    }
    Some(id)
}

/// Why a new note with `body` would be refused for its size, in the words
/// the editor uses: the same bound as an edit (20 KiB, one packet), so a
/// note that could never sync or be edited is not created.
pub(crate) fn new_note_refusal(body: &str) -> Option<&'static str> {
    if body.len() > MAX_NOTE_BYTES {
        return Some(TOO_LONG);
    }
    let probe = Note {
        id: uuid::Uuid::nil().to_string(),
        title: String::new(),
        body: body.to_string(),
        pinned: false,
        created: 0,
        modified: 0,
        parent: 0,
        ancestors: Vec::new(),
        deleted: false,
    };
    (!fits_one_packet(&probe)).then_some(TOO_LONG)
}

pub(super) fn create_in_shelf(shelf: &mut Shelf, body: &str) -> Option<String> {
    if shelf.notes.values().filter(|note| !note.deleted).count() >= MAX_NOTES
        || new_note_refusal(body).is_some()
    {
        return None;
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
            ancestors: Vec::new(),
            deleted: false,
        },
    );
    Some(id)
}

/// One local edit of note `id` in `shelf`: whether something changed.
#[cfg(test)]
pub(super) fn edit_in_shelf(
    shelf: &mut Shelf,
    id: &str,
    change: impl FnOnce(&mut Note),
) -> Result<bool, String> {
    edit_from_in_shelf(shelf, id, None, change).map(|(changed, _)| changed)
}

/// Saves an edit to note `id`: title, body, pinned, or deleted.
/// Saves an edit to note `id`. Refused, in words the editor shows, when
/// the note would pass 64 KB — never dropped silently.
pub(crate) fn edit_note(id: &str, change: impl FnOnce(&mut Note)) -> Result<(), String> {
    edit_note_from(id, None, change).map(|_| ())
}

/// [`edit_note`] for an editor that loaded the version with this `modified`
/// (see [`edit_from_in_shelf`]).
///
/// Returns the `modified` the note has after the call, read in the same step as
/// the edit: reading it afterwards could pick up a version that landed from the
/// phone in between, which the editor would then type over without a word.
pub(crate) fn edit_note_from(
    id: &str,
    loaded: Option<i64>,
    change: impl FnOnce(&mut Note),
) -> Result<Option<i64>, String> {
    let changed = with_shelf(|shelf| {
        let result = edit_from_in_shelf(shelf, id, loaded, change);
        let saved = matches!(result, Ok((true, _)));
        let modified = shelf.notes.get(id).map(|note| note.modified);
        ((result, modified), saved)
    });
    match changed {
        Some((Ok((true, copy)), modified)) => {
            let mut ids = vec![id.to_string()];
            ids.extend(copy);
            push(Kind::Notes, ids);
            Ok(modified)
        }
        // Nothing changed: the editor keeps the version it loaded (a newer one that
        // landed is shown when it reloads), it is not told the stored one is its own.
        Some((Ok((false, _)), _)) => Ok(None),
        None => Ok(None),
        Some((Err(error), _)) => Err(error),
    }
}

/// "Remove duplicate copies": (copies that would go, copies that stay because
/// their text exists nowhere else).
pub(crate) fn duplicate_copies_preview() -> (usize, usize) {
    with_shelf(|shelf| {
        let doomed = duplicate_copies(shelf, true).len();
        let copies = shelf
            .notes
            .values()
            .filter(|note| !note.deleted && is_copy(note))
            .count();
        ((doomed, copies - doomed), false)
    })
    .unwrap_or_default()
}

/// Deletes the duplicate copies; how many went.
pub(crate) fn remove_duplicate_copies() -> usize {
    let removed = with_shelf(|shelf| {
        let removed = remove_duplicates_in(shelf, true);
        let saved = !removed.is_empty();
        (removed, saved)
    })
    .unwrap_or_default();
    let count = removed.len();
    if count > 0 {
        push(Kind::Notes, removed);
    }
    count
}
