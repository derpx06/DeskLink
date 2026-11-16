//! Tests of snippets and notes: validation, merging and conflicted copies,
//! duplicates, indexes and items within their budgets, the store and local
//! edits.

use super::*;

#[test]
fn every_items_packet_fits_one_wire_message() {
    // Notes at the bound, quotes and backslashes doubling on escape.
    let values: Vec<Value> = (0..12)
        .map(|index| {
            serde_json::to_value(Note {
                id: format!("{index}-{}", "i".repeat(100)),
                title: "\"".repeat(MAX_NOTE_TITLE_CHARS),
                body: "\"\\x".repeat(MAX_NOTE_BYTES / 3),
                pinned: true,
                created: i64::MAX,
                modified: i64::MAX,
                parent: i64::MAX,
                ancestors: vec![i64::MAX; MAX_ANCESTORS],
                deleted: false,
            })
            .unwrap()
        })
        .collect();
    let packets = items_packets(Kind::Notes, values);
    assert!(!packets.is_empty());
    for packet in &packets {
        assert!(
            crate::device_links::webrtc::fits_on_wire(packet),
            "{:?}",
            crate::device_links::webrtc::enveloped_len(packet)
        );
    }
    // One that can never fit is not sent at all.
    let huge = json!({"id": "h", "body": "\u{1}".repeat(MAX_PACKET_BYTES)});
    assert!(items_packets(Kind::Notes, vec![huge]).is_empty());
}

/// What a peer may send, per item: over a bound is refused, not cut.
#[test]
fn a_snippet_or_note_over_its_bounds_is_refused() {
    let snippet = |text: String, title: String| Snippet {
        id: "a".into(),
        text,
        title,
        modified: 1,
        deleted: false,
    };
    assert!(valid_snippet(&snippet(
        "x".repeat(MAX_SNIPPET_BYTES),
        String::new()
    )));
    assert!(!valid_snippet(&snippet(
        "x".repeat(MAX_SNIPPET_BYTES + 1),
        String::new()
    )));
    assert!(!valid_snippet(&snippet(
        "x".into(),
        "t".repeat(MAX_SNIPPET_TITLE_CHARS + 1)
    )));
    let mut long_id = snippet("x".into(), String::new());
    long_id.id = "i".repeat(MAX_ID_CHARS + 1);
    assert!(!valid_snippet(&long_id));
    long_id.id = "a\nb".into();
    assert!(!valid_snippet(&long_id), "no control characters in an id");
    // Pinning from this side says so in words.
    let error = add_snippet(&"x".repeat(MAX_SNIPPET_BYTES + 1)).unwrap_err();
    assert!(error.contains("16 KiB"), "{error}");
    assert!(add_snippet("   \n").is_err());
    assert_eq!((MAX_SNIPPETS, MAX_NOTES), (500, 2000));
}

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

/// The tests that `install` the one global shelf take turns.
static ONE_SHELF: Mutex<()> = Mutex::new(());

/// Any change to the shelf clears identical copies, and the result is saved.
#[test]
fn identical_copies_are_cleared_after_any_change() {
    let _shelf = ONE_SHELF.lock().unwrap_or_else(|p| p.into_inner());
    let base = std::env::temp_dir().join(format!("desklink-tidy-{}", uuid::Uuid::new_v4()));
    install(base.join("notes"));
    let id = new_note("Plan\nbody").unwrap();
    let original = super::note(&id).unwrap();
    let copy_id = format!("{id}~11");
    with_shelf(|shelf| {
        shelf.notes.insert(
            copy_id.clone(),
            Note {
                id: copy_id.clone(),
                title: format!("Plan {COPY_SUFFIX}"),
                modified: 11,
                parent: 11,
                ..original.clone()
            },
        );
        ((), true)
    });
    assert!(
        super::note(&copy_id).is_none(),
        "cleared by the change that added it"
    );
    assert!(super::note(&id).is_some());
    crate::device_links::vault::flush();
}

/// S2: a shelf that could not be read is not saved over; what is typed
/// meanwhile is kept, and Try Again merges it into the file once it reads.
#[test]
fn typed_notes_survive_an_unreadable_start_and_try_again_merges_them() {
    let _shelf = ONE_SHELF.lock().unwrap_or_else(|p| p.into_inner());
    let base = std::env::temp_dir().join(format!("desklink-blocked-{}", uuid::Uuid::new_v4()));
    let dir = base.join("notes");
    install(dir.clone());
    let earlier = new_note("written before").unwrap();
    crate::device_links::vault::flush();
    // The key goes unreadable (a directory stands in) and DeskLink restarts.
    let key_file = dir.join("key");
    let good = std::fs::read(&key_file).unwrap();
    std::fs::remove_file(&key_file).unwrap();
    std::fs::create_dir(&key_file).unwrap();
    install(dir.clone());
    assert!(
        super::note(&earlier).is_none(),
        "not shown: it could not be read"
    );
    assert!(!crate::device_links::vault::blocked_under("notes").is_empty());
    let typed = new_note("typed meanwhile").unwrap();
    crate::device_links::vault::flush();
    assert!(!retry_blocked(), "still unreadable: nothing changes");
    assert!(super::note(&typed).is_some(), "what was typed is kept");
    // The key comes back.
    std::fs::remove_dir(&key_file).unwrap();
    std::fs::write(&key_file, good).unwrap();
    assert!(retry_blocked());
    assert!(super::note(&earlier).is_some(), "the file's notes are back");
    assert!(super::note(&typed).is_some(), "and the typed one merged in");
    crate::device_links::vault::flush();
    install(dir.clone());
    assert!(
        super::note(&earlier).is_some() && super::note(&typed).is_some(),
        "saved"
    );
    let _ = std::fs::remove_dir_all(base);
}

#[test]
fn a_note_too_long_is_refused_out_loud_and_kept_as_it_was() {
    let _shelf = ONE_SHELF.lock().unwrap_or_else(|p| p.into_inner());
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
        ancestors: Vec::new(),
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
    // Nothing to send back but the acknowledgement that it took the note.
    assert!(changed && back.iter().all(|p| p.get_str("op") == Some("ack")));
    assert_eq!(back.len(), 1);
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
    assert!(
        packets
            .iter()
            .all(|packet| serde_json::to_string(&packet.body).unwrap().len()
                < MAX_PACKET_BYTES + 1024)
    );
    let mut shelf = Shelf::default();
    let mut old = note("old", "", 1, 0);
    old.deleted = true;
    shelf.notes.insert("old".into(), old);
    shelf.notes.insert("live".into(), note("live", "x", 1, 0));
    prune(&mut shelf, 100 * 24 * 3600 * 1000);
    assert!(!shelf.notes.contains_key("old") && shelf.notes.contains_key("live"));
}

/// **The conflicted-copy bug.** Two edits in a row by one side, merged
/// into the other side's shelf, are one lineage: the second used to make
/// a "(conflicted copy)" of the first, and so did the first edit of a new
/// note. Run through both sides' real edit and merge code.
#[test]
fn consecutive_edits_by_one_side_never_make_a_conflicted_copy() {
    let mut desk = Shelf::default();
    let mut phone = Shelf::default();
    // A note created at the desk and pushed at once.
    desk.notes.insert("n".into(), note("n", "", 100, 0));
    assert_eq!(
        merge_note(&mut phone, desk.notes["n"].clone()),
        Merged::Taken
    );
    // Edit 1 and edit 2 in a row, before anything is acknowledged.
    for (n, body) in ["one", "two", "three", "four"].into_iter().enumerate() {
        assert_eq!(
            edit_in_shelf(&mut desk, "n", |note| note.body = body.into()),
            Ok(true)
        );
        assert!(desk.notes["n"].modified > 100 + n as i64);
        let merged = merge_note(&mut phone, desk.notes["n"].clone());
        assert_eq!(merged, Merged::Taken, "edit {n}: {body}");
    }
    assert_eq!(phone.notes.len(), 1, "no conflicted copy");
    assert_eq!(phone.notes["n"].body, "four");
    // And the other way round: the phone edits what it took.
    assert_eq!(
        edit_in_shelf(&mut phone, "n", |note| note.body = "five".into()),
        Ok(true)
    );
    assert_eq!(
        merge_note(&mut desk, phone.notes["n"].clone()),
        Merged::Taken
    );
    assert_eq!(desk.notes.len(), 1);
    // A real conflict (edited on both sides from the same version) still
    // makes a copy.
    std::thread::sleep(std::time::Duration::from_millis(20));
    let mut a = desk.clone();
    let mut b = desk.clone();
    edit_in_shelf(&mut a, "n", |note| note.body = "from a".into()).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(3));
    edit_in_shelf(&mut b, "n", |note| note.body = "from b".into()).unwrap();
    assert_eq!(merge_note(&mut a, b.notes["n"].clone()), Merged::Conflict);
    assert_eq!(a.notes.len(), 2);
}

#[test]
fn a_note_names_at_most_eight_ancestors_and_older_peers_still_work() {
    let mut shelf = Shelf::default();
    shelf.notes.insert("n".into(), note("n", "", 100, 0));
    for body in 0..20 {
        edit_in_shelf(&mut shelf, "n", |note| note.body = body.to_string()).unwrap();
    }
    assert_eq!(shelf.notes["n"].ancestors.len(), MAX_ANCESTORS);
    assert!(valid_note(&shelf.notes["n"]));
    let mut too_many = shelf.notes["n"].clone();
    too_many.ancestors = vec![1; MAX_ANCESTORS + 1];
    assert!(!valid_note(&too_many));
    // An older build sends none: `parent` alone decides, as before.
    let json = serde_json::json!({"id": "x", "created": 1, "modified": 5});
    let parsed: Note = serde_json::from_value(json).unwrap();
    assert!(parsed.ancestors.is_empty());
    assert!(!serde_json::to_string(&parsed)
        .unwrap()
        .contains("ancestors"));
}

/// A big shelf's index goes out in parts that each fit one wire message,
/// and together name every entry once: past ~570 entries a single packet
/// never reached the phone.
#[test]
fn a_big_shelfs_index_is_split_and_every_part_fits_the_wire() {
    let mut shelf = Shelf::default();
    for n in 0..MAX_NOTE_ENTRIES {
        let id = format!("{n:08}-{}", uuid::Uuid::new_v4());
        shelf.notes.insert(
            id.clone(),
            note(&id, "text", 1_700_000_000_000 + n as i64, 0),
        );
    }
    let parts = index_packets(&shelf, Kind::Notes);
    assert!(
        parts.len() > 1,
        "{} entries in one packet",
        MAX_NOTE_ENTRIES
    );
    let mut named = 0;
    for part in &parts {
        assert!(
            crate::device_links::webrtc::fits_on_wire(part),
            "a part of {} bytes",
            part.serialize_line().map(|line| line.len()).unwrap_or(0)
        );
        assert!(part.serialize_line().unwrap().len() <= INDEX_BUDGET + 1024);
        named += part
            .body
            .get("items")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
    }
    assert_eq!(named, MAX_NOTE_ENTRIES);
    // An empty shelf still says so, in one packet.
    assert_eq!(index_packets(&Shelf::default(), Kind::Snippets).len(), 1);
    // A phone takes the parts one by one, as it does the desk's own.
    let mut phone = Shelf::default();
    let mut wanted_total = 0;
    for part in &parts {
        let (fetches, _) = handle(&mut phone, Kind::Notes, part);
        wanted_total += fetches
            .iter()
            .map(|fetch| {
                fetch
                    .body
                    .get("ids")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            })
            .sum::<usize>();
    }
    assert_eq!(wanted_total, MAX_NOTE_ENTRIES);
}

/// A shelf holds 500 live snippets; the 501st is refused in words, an
/// identical one still moves to the top, and the stored cap is 1000 on
/// both sides (the phone's was 2000).
#[test]
fn the_501st_snippet_is_refused_and_the_stored_cap_is_a_thousand() {
    let mut shelf = Shelf::default();
    for n in 0..MAX_SNIPPETS {
        add_in_shelf(&mut shelf, &format!("snippet {n}")).unwrap();
    }
    assert_eq!(
        add_in_shelf(&mut shelf, "one more"),
        Err(TOO_MANY_SNIPPETS.to_string())
    );
    assert!(
        add_in_shelf(&mut shelf, "snippet 7").is_ok(),
        "an identical one moves"
    );
    assert_eq!(shelf.snippets.len(), MAX_SNIPPETS);
    assert_eq!(MAX_SNIPPET_ENTRIES, 1000);
}

/// The other side's edit lands while the person is typing over the version
/// they loaded: both survive, the arrival as a conflicted copy, instead of
/// the save overwriting it silently.
#[test]
fn typing_over_a_note_that_changed_underneath_keeps_the_arrival_as_a_copy() {
    let mut shelf = Shelf::default();
    shelf.notes.insert("n".into(), note("n", "base", 100, 0));
    // The editor loaded version 100; the phone's edit (200) then arrived.
    shelf
        .notes
        .insert("n".into(), note("n", "the phone's words", 200, 100));
    let (changed, copy) = edit_from_in_shelf(&mut shelf, "n", Some(100), |note| {
        note.body = "what I typed".into()
    })
    .unwrap();
    assert!(changed);
    let copy = copy.expect("the arrival is kept");
    assert_eq!(shelf.notes["n"].body, "what I typed");
    assert_eq!(shelf.notes[&copy].body, "the phone's words");
    assert!(shelf.notes[&copy].title.ends_with("(conflicted copy)"));
    // No surprise when nothing changed underneath.
    let current = shelf.notes["n"].modified;
    let (_, none) = edit_from_in_shelf(&mut shelf, "n", Some(current), |note| {
        note.body = "more".into()
    })
    .unwrap();
    assert!(none.is_none());
}

/// A full shelf still takes a straight edit of a note it holds, but not a
/// sibling edit: that makes a conflict copy, a new entry, and 50 of them grew
/// a full shelf to 4050 entries.
#[test]
fn a_full_shelf_takes_straight_edits_but_no_conflict_copies() {
    let notes: Vec<Note> = (0..MAX_NOTE_ENTRIES)
        .map(|index| note(&format!("n{index}"), "old", 100, 50))
        .collect();
    let mut shelf = shelf_with(&notes);
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTES);
    packet.set("op", "items");
    let mut items = vec![serde_json::to_value(note("n1", "edited from ours", 200, 100)).unwrap()];
    for index in 2..52 {
        // Edited from a version we never had: siblings of ours.
        items.push(serde_json::to_value(note(&format!("n{index}"), "sibling", 200, 70)).unwrap());
    }
    packet.set("items", items);
    handle(&mut shelf, Kind::Notes, &packet);
    assert_eq!(shelf.notes["n1"].body, "edited from ours");
    assert_eq!(
        shelf.notes.len(),
        MAX_NOTE_ENTRIES,
        "no conflict copy on a full shelf"
    );
    assert_eq!(shelf.notes["n2"].body, "old");
}
