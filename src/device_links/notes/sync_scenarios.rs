//! Notes sync as two shelves exchanging the real packets: a table of the
//! situations that used to leave "(conflicted copy)" notes for no reason
//! (and the real conflict that should leave exactly one).

use super::*;
use std::collections::VecDeque;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side {
    Desk,
    Phone,
}

impl Side {
    fn other(self) -> Side {
        match self {
            Side::Desk => Side::Phone,
            Side::Phone => Side::Desk,
        }
    }
}

struct World {
    desk: Shelf,
    phone: Shelf,
    linked: bool,
}

impl World {
    fn new() -> Self {
        World {
            desk: Shelf::default(),
            phone: Shelf::default(),
            linked: false,
        }
    }

    fn shelf(&mut self, side: Side) -> &mut Shelf {
        match side {
            Side::Desk => &mut self.desk,
            Side::Phone => &mut self.phone,
        }
    }

    /// Both indexes cross, then everything they ask for, until quiet.
    fn connect(&mut self) {
        self.linked = true;
        for side in [Side::Desk, Side::Phone] {
            let packets = index_packets(self.shelf(side), Kind::Notes);
            self.deliver(side, packets);
        }
    }

    fn disconnect(&mut self) {
        self.linked = false;
    }

    fn deliver(&mut self, from: Side, packets: Vec<NetworkPacket>) {
        let mut queue: VecDeque<(Side, NetworkPacket)> =
            packets.into_iter().map(|packet| (from, packet)).collect();
        while let Some((from, packet)) = queue.pop_front() {
            let to = from.other();
            let (answers, _) = handle(self.shelf(to), Kind::Notes, &packet);
            queue.extend(answers.into_iter().map(|answer| (to, answer)));
        }
    }

    fn create(&mut self, side: Side, body: &str) -> String {
        create_in_shelf(self.shelf(side), body).unwrap()
    }

    /// A local edit (an autosave); not sent until `push`.
    fn edit(&mut self, side: Side, id: &str, body: &str) {
        let body = body.to_string();
        // Two people never save in the same millisecond; a test would.
        std::thread::sleep(std::time::Duration::from_millis(2));
        edit_from_in_shelf(self.shelf(side), id, None, |note| note.body = body).unwrap();
    }

    /// What the side's `push` does: the current notes `ids` go to the peer, if linked.
    fn push(&mut self, side: Side, ids: &[&str]) {
        if !self.linked {
            return;
        }
        let ids: Vec<String> = ids.iter().map(|id| id.to_string()).collect();
        let values = item_values(self.shelf(side), Kind::Notes, &ids);
        let packets = items_packets(Kind::Notes, values);
        self.deliver(side, packets);
    }

    fn live(&mut self, side: Side) -> Vec<Note> {
        self.shelf(side)
            .notes
            .values()
            .filter(|note| !note.deleted)
            .cloned()
            .collect()
    }

    fn copies(&mut self, side: Side) -> usize {
        self.live(side).iter().filter(|note| is_copy(note)).count()
    }

    fn assert_same_notes(&self) {
        let view = |shelf: &Shelf| {
            shelf
                .notes
                .values()
                .map(|n| {
                    (
                        n.id.clone(),
                        n.title.clone(),
                        n.body.clone(),
                        n.pinned,
                        n.deleted,
                        n.modified,
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(view(&self.desk), view(&self.phone), "the two sides differ");
    }

    fn assert_no_copies(&mut self) {
        for side in [Side::Desk, Side::Phone] {
            assert_eq!(self.copies(side), 0, "{side:?} has a conflicted copy");
        }
    }
}

const SIDES: [Side; 2] = [Side::Desk, Side::Phone];

#[test]
fn a_one_sided_edit_never_makes_a_copy() {
    for creator in SIDES {
        for editor in SIDES {
            let mut world = World::new();
            let id = world.create(creator, "first");
            world.connect();
            world.edit(editor, &id, "second");
            world.push(editor, &[&id]);
            world.assert_no_copies();
            world.assert_same_notes();
        }
    }
}

#[test]
fn an_echo_of_our_own_edit_is_not_a_second_edit() {
    let mut world = World::new();
    let id = world.create(Side::Desk, "a");
    world.connect();
    world.edit(Side::Desk, &id, "b");
    world.push(Side::Desk, &[&id]);
    // The phone sends back what it holds (a debounced push after taking it).
    world.push(Side::Phone, &[&id]);
    world.push(Side::Phone, &[&id]);
    world.assert_no_copies();
    world.assert_same_notes();
}

#[test]
fn the_same_edit_on_both_sides_is_one_note() {
    let mut world = World::new();
    let id = world.create(Side::Desk, "base");
    world.connect();
    world.disconnect();
    world.edit(Side::Desk, &id, "same text");
    world.edit(Side::Phone, &id, "same text");
    world.connect();
    world.assert_no_copies();
    world.assert_same_notes();
}

#[test]
fn reconnecting_with_unchanged_notes_changes_nothing() {
    let mut world = World::new();
    let one = world.create(Side::Desk, "one");
    let two = world.create(Side::Phone, "two");
    world.connect();
    world.edit(Side::Phone, &one, "one!");
    world.push(Side::Phone, &[&one]);
    world.assert_same_notes();
    let before = world.desk.notes.clone();
    for _ in 0..5 {
        world.disconnect();
        world.connect();
    }
    assert_eq!(world.desk.notes, before);
    assert_eq!(world.desk.notes.len(), 2);
    assert!(world.desk.notes.contains_key(&two));
    world.assert_same_notes();
}

/// The main cause: more autosaves than a note can name as ancestors (8) after
/// the peer last held a version, with that version's push never acknowledged.
#[test]
fn many_autosaves_after_the_peer_last_took_a_version_are_still_one_sided() {
    for creator in SIDES {
        for editor in SIDES {
            // Edited while offline.
            let mut world = World::new();
            let id = world.create(creator, "v0");
            world.connect();
            world.edit(editor, &id, "v1");
            world.push(editor, &[&id]);
            world.disconnect();
            for n in 2..=14 {
                world.edit(editor, &id, &format!("v{n}"));
            }
            world.connect();
            world.assert_no_copies();
            world.assert_same_notes();
            assert_eq!(world.live(creator.other())[0].body, "v14");

            // Typing on, connected, pushed only after the pause (the phone's debounce).
            let mut world = World::new();
            let id = world.create(creator, "v0");
            world.connect();
            world.edit(editor, &id, "v1");
            world.push(editor, &[&id]);
            for n in 2..=14 {
                world.edit(editor, &id, &format!("v{n}"));
            }
            world.push(editor, &[&id]);
            world.assert_no_copies();
            world.assert_same_notes();
        }
    }
}

#[test]
fn edit_while_the_other_side_is_offline_then_reconnect() {
    for editor in SIDES {
        let mut world = World::new();
        let id = world.create(editor, "kept");
        world.connect();
        world.disconnect();
        world.edit(editor, &id, "changed while away");
        world.connect();
        world.assert_no_copies();
        world.assert_same_notes();
    }
}

#[test]
fn a_real_conflict_leaves_exactly_one_copy_and_stays_one() {
    let mut world = World::new();
    let id = world.create(Side::Desk, "base");
    world.connect();
    world.disconnect();
    world.edit(Side::Desk, &id, "desk says");
    world.edit(Side::Phone, &id, "phone says");
    world.connect();
    for side in SIDES {
        assert_eq!(world.copies(side), 1, "{side:?}");
    }
    world.assert_same_notes();
    let bodies: Vec<String> = world.live(Side::Desk).into_iter().map(|n| n.body).collect();
    assert!(
        bodies.contains(&"desk says".to_string()) && bodies.contains(&"phone says".to_string())
    );
    for _ in 0..5 {
        world.disconnect();
        world.connect();
        world.push(Side::Desk, &[&id]);
        world.push(Side::Phone, &[&id]);
    }
    for side in SIDES {
        assert_eq!(world.live(side).len(), 2, "{side:?}");
    }
    world.assert_same_notes();
}

#[test]
fn deletes_and_edits() {
    // A delete after many edits is not undone by the older version.
    let mut world = World::new();
    let id = world.create(Side::Desk, "v0");
    world.connect();
    world.edit(Side::Phone, &id, "v1");
    world.push(Side::Phone, &[&id]);
    world.disconnect();
    for n in 2..=12 {
        world.edit(Side::Phone, &id, &format!("v{n}"));
    }
    edit_from_in_shelf(&mut world.phone, &id, None, |note| note.deleted = true).unwrap();
    world.connect();
    assert!(world.live(Side::Desk).is_empty() && world.live(Side::Phone).is_empty());
    world.assert_same_notes();

    // A delete against a concurrent edit: the edit stays, with no copy.
    let mut world = World::new();
    let id = world.create(Side::Desk, "v0");
    world.connect();
    world.disconnect();
    edit_from_in_shelf(&mut world.desk, &id, None, |note| note.deleted = true).unwrap();
    world.edit(Side::Phone, &id, "kept");
    world.connect();
    world.assert_no_copies();
    assert_eq!(world.live(Side::Desk).len(), 1);
    assert_eq!(world.live(Side::Phone)[0].body, "kept");
    world.assert_same_notes();
}

#[test]
fn a_copy_that_was_edited_or_deleted_is_not_written_over_by_the_same_conflict() {
    let mut world = World::new();
    let id = world.create(Side::Desk, "base");
    world.connect();
    world.disconnect();
    world.edit(Side::Desk, &id, "desk says");
    world.edit(Side::Phone, &id, "phone says");
    world.connect();
    let copy = world.live(Side::Desk).into_iter().find(is_copy).unwrap().id;
    // The person edits the copy on the desk, deletes the other side's.
    world.edit(Side::Desk, &copy, "my own words");
    world.push(Side::Desk, &[&copy]);
    edit_from_in_shelf(&mut world.phone, &copy, None, |note| note.deleted = true).unwrap();
    // The same siblings meet again (a re-fetch after a reconnect): the loser is the copy's `modified`.
    let again = |shelf: &Shelf| Note {
        modified: copy.rsplit('~').next().unwrap().parse().unwrap(),
        body: "something else".into(),
        parent: 1,
        ancestors: Vec::new(),
        ..shelf.notes[&id].clone()
    };
    let incoming = again(&world.desk);
    merge_note(&mut world.desk, incoming);
    assert_eq!(
        world.desk.notes[&copy].body, "my own words",
        "an edited copy stays as edited"
    );
    let incoming = again(&world.phone);
    merge_note(&mut world.phone, incoming);
    assert!(
        world.phone.notes[&copy].deleted,
        "a deleted copy is not brought back"
    );
    // Nor by reconnecting.
    world.disconnect();
    world.connect();
    assert!(world.phone.notes[&copy].deleted);
}

#[test]
fn typing_over_a_version_that_landed_keeps_it_as_one_copy_and_reports_the_new_version() {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "mine").unwrap();
    let loaded = shelf.notes[&id].modified;
    // The phone's edit lands while the person types.
    let mut phone = shelf.notes[&id].clone();
    phone.body = "phone".into();
    phone.parent = loaded;
    phone.modified = loaded + 10;
    phone.ancestors = vec![loaded];
    assert_eq!(merge_note(&mut shelf, phone), Merged::Taken);
    let (changed, copy) = edit_from_in_shelf(&mut shelf, &id, Some(loaded), |note| {
        note.body = "typed".into()
    })
    .unwrap();
    assert!(changed && copy.is_some());
    // Saving again with the version that save left does not make another.
    let current = shelf.notes[&id].modified;
    let (_, again) = edit_from_in_shelf(&mut shelf, &id, Some(current), |note| {
        note.body = "typed more".into()
    })
    .unwrap();
    assert!(again.is_none());
    assert_eq!(shelf.notes.values().filter(|n| is_copy(n)).count(), 1);
}

#[test]
fn rapid_autosaves_while_a_snapshot_arrives() {
    let mut world = World::new();
    let id = world.create(Side::Desk, "v0");
    world.connect();
    for n in 1..=20 {
        world.edit(Side::Desk, &id, &format!("v{n}"));
        world.push(Side::Desk, &[&id]);
        if n % 5 == 0 {
            // A snapshot of the phone's notes (a reconnect burst) crosses.
            let index = index_packets(&world.phone, Kind::Notes);
            world.deliver(Side::Phone, index);
        }
    }
    world.assert_no_copies();
    world.assert_same_notes();
}

#[test]
fn acks_are_ignored_by_what_does_not_know_them_and_never_move_synced_back() {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "a").unwrap();
    let first = shelf.notes[&id].modified;
    edit_from_in_shelf(&mut shelf, &id, None, |n| n.body = "b".into()).unwrap();
    let second = shelf.notes[&id].modified;
    assert!(acknowledge(&mut shelf, &id, second));
    assert!(!acknowledge(&mut shelf, &id, first), "never backwards");
    assert!(
        !acknowledge(&mut shelf, &id, 12345),
        "not a version of ours"
    );
    assert!(!acknowledge(&mut shelf, "nope", second));
    assert_eq!(shelf.synced[&id], second);
    let mut ack = NetworkPacket::new(PACKET_TYPE_SNIPPETS);
    ack.set("op", "ack");
    ack.set("items", json!([{"id": id, "modified": first}]));
    assert_eq!(
        handle(&mut shelf, Kind::Snippets, &ack),
        (Vec::new(), false)
    );
}

// ── Removing duplicate copies ──────────────────────────────────────────────

fn copy_of(base: &Note, title: &str, body: &str, modified: i64) -> Note {
    Note {
        id: format!("{}~{modified}", base.id),
        title: format!("{title} {COPY_SUFFIX}"),
        body: body.into(),
        modified,
        parent: modified,
        ..base.clone()
    }
}

fn live_texts(shelf: &Shelf) -> Vec<(String, String)> {
    shelf
        .notes
        .values()
        .filter(|n| !n.deleted)
        .map(|n| (n.label(), n.body.clone()))
        .collect()
}

#[test]
fn duplicate_copies_go_and_copies_with_their_own_text_stay() {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "Shopping\nmilk").unwrap();
    let base = shelf.notes[&id].clone();
    let key = (base.label(), base.body.clone());
    // Equal to the note, equal to each other, differing by one character, differing in title only.
    for (modified, title, body) in [
        (11, "Shopping", "Shopping\nmilk"),
        (12, "Shopping", "Shopping\nmilk"),
        (13, "Shopping", "Shopping\nmilk!"),
        (14, "Other title", "Shopping\nmilk"),
    ] {
        let copy = copy_of(&base, title, body, modified);
        shelf.notes.insert(copy.id.clone(), copy);
    }
    let before_texts: Vec<(String, String)> = live_texts(&shelf);
    // Automatic: only those equal to the note or to another copy of it.
    let removed = remove_duplicates_in(&mut shelf, false);
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert!(shelf.notes[&format!("{id}~11")].deleted);
    assert!(shelf.notes[&format!("{id}~12")].deleted);
    assert!(
        !shelf.notes[&format!("{id}~13")].deleted,
        "differs by a character"
    );
    assert!(!shelf.notes[&format!("{id}~14")].deleted, "another title");
    // Every distinct text that was there is still there.
    let after: Vec<(String, String)> = live_texts(&shelf);
    for text in &before_texts {
        assert!(
            after.contains(text)
                || *text == key
                || before_texts.iter().filter(|t| *t == text).count() > 1
        );
    }
    assert!(after.contains(&key));
    // Nothing more to do, and a tombstone is a normal one (it syncs).
    assert!(remove_duplicates_in(&mut shelf, true).is_empty());
    let tombstone = &shelf.notes[&format!("{id}~11")];
    assert!(tombstone.title.is_empty() && tombstone.body.is_empty() && tombstone.modified > 11);
}

#[test]
fn the_manual_action_also_removes_a_copy_equal_to_any_other_note_but_never_the_last_one() {
    let mut shelf = Shelf::default();
    let a = create_in_shelf(&mut shelf, "same words").unwrap();
    let b = create_in_shelf(&mut shelf, "unrelated").unwrap();
    let base_b = shelf.notes[&b].clone();
    // A copy of b whose text equals note a's; two copies equal to each other and to nothing else.
    let on_a = copy_of(&base_b, "same words", "same words", 21);
    let lone1 = copy_of(&base_b, "lone", "only here", 22);
    let lone2 = copy_of(&base_b, "lone", "only here", 23);
    for copy in [on_a, lone1, lone2] {
        shelf.notes.insert(copy.id.clone(), copy);
    }
    assert!(
        remove_duplicates_in(&mut shelf, false).len() == 1,
        "only the second lone copy"
    );
    let removed = remove_duplicates_in(&mut shelf, true);
    assert_eq!(removed, vec![format!("{b}~21")]);
    let texts = live_texts(&shelf);
    assert!(texts.iter().any(|(_, body)| body == "same words"));
    assert_eq!(
        texts.iter().filter(|(_, body)| body == "only here").count(),
        1,
        "the last one stays"
    );
    assert!(shelf.notes.contains_key(&a));
}

#[test]
fn a_copy_whose_original_was_deleted_is_the_only_text_left_and_stays() {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "words").unwrap();
    let base = shelf.notes[&id].clone();
    let copy = copy_of(&base, "words", "words", 31);
    shelf.notes.insert(copy.id.clone(), copy);
    edit_from_in_shelf(&mut shelf, &id, None, |n| n.deleted = true).unwrap();
    assert!(remove_duplicates_in(&mut shelf, false).is_empty());
    assert!(remove_duplicates_in(&mut shelf, true).is_empty());
    let texts: Vec<String> = live_texts(&shelf)
        .into_iter()
        .map(|(_, body)| body)
        .collect();
    assert_eq!(texts, vec!["words".to_string()]);
}

#[test]
fn removed_duplicates_sync_as_deletes_and_do_not_come_back() {
    // Many identical copies, as the owner's devices have them.
    let mut world = World::new();
    let id = world.create(Side::Desk, "Plan\nbody");
    world.connect();
    let base = world.desk.notes[&id].clone();
    for modified in 41..=45 {
        let copy = copy_of(&base, "Plan", "Plan\nbody", modified);
        world.desk.notes.insert(copy.id.clone(), copy.clone());
        world.phone.notes.insert(copy.id.clone(), copy);
    }
    let unique = copy_of(&base, "Plan", "Plan\nbody v2", 46);
    world.desk.notes.insert(unique.id.clone(), unique.clone());
    world.phone.notes.insert(unique.id.clone(), unique.clone());
    let removed = remove_duplicates_in(&mut world.desk, true);
    assert_eq!(removed.len(), 5);
    let ids: Vec<&str> = removed.iter().map(String::as_str).collect();
    world.push(Side::Desk, &ids);
    world.disconnect();
    world.connect();
    for side in SIDES {
        let live = world.live(side);
        assert_eq!(
            live.len(),
            2,
            "{side:?}: the note and the one different copy"
        );
        assert!(live.iter().any(|n| n.id == unique.id));
    }
    world.assert_same_notes();
    // The phone's own cleanup finds nothing left, and nothing is revived by it.
    assert!(remove_duplicates_in(&mut world.phone, false).is_empty());
}

#[test]
fn two_edits_in_the_same_millisecond_are_siblings_not_the_same_version() {
    let mut desk = Shelf::default();
    let id = create_in_shelf(&mut desk, "base").unwrap();
    let mut phone = desk.clone();
    let m = desk.notes[&id].modified + 5;
    for (shelf, body) in [(&mut desk, "desk"), (&mut phone, "phone")] {
        let note = shelf.notes.get_mut(&id).unwrap();
        note.body = body.into();
        note.modified = m;
        note.ancestors = vec![note.created];
    }
    let mut world = World::new();
    world.desk = desk;
    world.phone = phone;
    // (The index lists only `modified`, so a tie is met when an edit is pushed.)
    world.linked = true;
    world.push(Side::Desk, &[&id]);
    world.push(Side::Phone, &[&id]);
    for side in SIDES {
        assert_eq!(world.copies(side), 1, "{side:?}");
    }
    world.assert_same_notes();
}
