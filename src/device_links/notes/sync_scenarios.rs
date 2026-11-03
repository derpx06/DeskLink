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
    fn id(self) -> &'static str {
        match self {
            Side::Desk => "desk",
            Side::Phone => "phone",
        }
    }

    fn other(self) -> Side {
        match self {
            Side::Desk => Side::Phone,
            Side::Phone => Side::Desk,
        }
    }
}

/// Carries `packets` from `x` (called `x_id` by `y`) to `y` and everything
/// that comes back, until quiet. `drop_acks` loses every acknowledgement.
fn pump(
    x: &mut Shelf,
    x_id: &str,
    y: &mut Shelf,
    y_id: &str,
    packets: Vec<NetworkPacket>,
    drop_acks: bool,
) {
    // true: heading to y.
    let mut queue: VecDeque<(bool, NetworkPacket)> =
        packets.into_iter().map(|packet| (true, packet)).collect();
    while let Some((to_y, packet)) = queue.pop_front() {
        // What crosses is what the wire carries.
        let packet = NetworkPacket::deserialize(&packet.serialize().unwrap()).unwrap();
        let (answers, _) = if to_y {
            handle_from(y, Kind::Notes, &packet, x_id)
        } else {
            handle_from(x, Kind::Notes, &packet, y_id)
        };
        queue.extend(
            answers
                .into_iter()
                .filter(|answer| !(drop_acks && answer.get_str("op") == Some("ack")))
                .map(|answer| (!to_y, answer)),
        );
    }
}

struct World {
    desk: Shelf,
    phone: Shelf,
    linked: bool,
    drop_acks: bool,
}

impl World {
    fn new() -> Self {
        World {
            desk: Shelf::default(),
            phone: Shelf::default(),
            linked: false,
            drop_acks: false,
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
        let drop_acks = self.drop_acks;
        let World { desk, phone, .. } = self;
        match from {
            Side::Desk => pump(desk, "desk", phone, "phone", packets, drop_acks),
            Side::Phone => pump(phone, "phone", desk, "desk", packets, drop_acks),
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
        // What a push records (see `push`).
        sent_to(self.shelf(side), Kind::Notes, &ids, side.other().id());
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
    assert!(acknowledge(&mut shelf, &id, second, "p"));
    assert!(!acknowledge(&mut shelf, &id, first, "p"), "never backwards");
    assert!(
        !acknowledge(&mut shelf, &id, 12345, "p"),
        "not a version of ours"
    );
    assert!(!acknowledge(&mut shelf, "nope", second, "p"));
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
    let unique_before = unique_texts(&shelf);
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
    // Every distinct text that was there is still there, and nothing new appeared.
    assert_eq!(unique_texts(&shelf), unique_before);
    assert!(live_texts(&shelf).contains(&key));
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

/// Every distinct (title, body) the live notes say, copies by what they kept.
fn unique_texts(shelf: &Shelf) -> std::collections::BTreeSet<(String, String)> {
    shelf
        .notes
        .values()
        .filter(|note| !note.deleted)
        .map(|note| {
            let (title, body) = content_key(note);
            (title, body.to_string())
        })
        .collect()
}

// ── Two phones, lost and late acknowledgements ─────────────────────────────

fn nap() {
    // Two saves never share a millisecond; a test would.
    std::thread::sleep(std::time::Duration::from_millis(2));
}

/// The desk and one phone exchange their indexes, and what they ask for.
fn sync_with(desk: &mut Shelf, phone: &mut Shelf, phone_id: &str) {
    let packets = index_packets(desk, Kind::Notes);
    pump(desk, "desk", phone, phone_id, packets, false);
    let packets = index_packets(phone, Kind::Notes);
    pump(phone, phone_id, desk, "desk", packets, false);
}

/// The desk pushes note `id` to one phone.
fn push_to(desk: &mut Shelf, phone: &mut Shelf, phone_id: &str, id: &str) {
    let ids = vec![id.to_string()];
    sent_to(desk, Kind::Notes, &ids, phone_id);
    let packets = items_packets(Kind::Notes, item_values(desk, Kind::Notes, &ids));
    pump(desk, "desk", phone, phone_id, packets, false);
}

fn edit_body(shelf: &mut Shelf, id: &str, body: &str) {
    nap();
    let body = body.to_string();
    edit_from_in_shelf(shelf, id, None, |note| note.body = body).unwrap();
}

fn copies_in(shelf: &Shelf) -> usize {
    shelf
        .notes
        .values()
        .filter(|note| !note.deleted && is_copy(note))
        .count()
}

/// The desk is the side with several phones: one that was away through many edits,
/// while another kept acknowledging, still finds the next version continues its own.
#[test]
fn a_phone_away_through_many_edits_is_not_a_rival_when_another_phone_acknowledged() {
    let (mut desk, mut a, mut b) = (Shelf::default(), Shelf::default(), Shelf::default());
    let id = create_in_shelf(&mut desk, "v0").unwrap();
    sync_with(&mut desk, &mut a, "a");
    sync_with(&mut desk, &mut b, "b");
    // B is away; A takes and acknowledges every edit.
    for n in 1..=12 {
        edit_body(&mut desk, &id, &format!("v{n}"));
        push_to(&mut desk, &mut a, "a", &id);
    }
    assert_eq!(a.notes[&id].body, "v12");
    sync_with(&mut desk, &mut b, "b");
    for (name, shelf) in [("desk", &desk), ("a", &a), ("b", &b)] {
        assert_eq!(copies_in(shelf), 0, "{name}");
        assert_eq!(shelf.notes[&id].body, "v12", "{name}");
    }
    // And the other way round: A away, B present, then A edits from its old version
    // while the desk moved on: a real conflict, exactly one copy everywhere.
    for n in 13..=24 {
        edit_body(&mut desk, &id, &format!("v{n}"));
        push_to(&mut desk, &mut b, "b", &id);
    }
    edit_body(&mut a, &id, "a says");
    sync_with(&mut desk, &mut a, "a");
    sync_with(&mut desk, &mut b, "b");
    assert_eq!(copies_in(&desk), 1);
    assert_eq!(copies_in(&a), 1);
    assert_eq!(copies_in(&b), 1);
}

#[test]
fn a_lost_acknowledgement_and_many_edits_are_still_one_sided() {
    let mut world = World::new();
    let id = world.create(Side::Desk, "v0");
    world.connect();
    // The phone takes v1; its acknowledgement never arrives.
    world.drop_acks = true;
    world.edit(Side::Desk, &id, "v1");
    world.push(Side::Desk, &[&id]);
    world.drop_acks = false;
    assert_eq!(world.phone.notes[&id].body, "v1");
    world.disconnect();
    for n in 2..=12 {
        world.edit(Side::Desk, &id, &format!("v{n}"));
    }
    world.connect();
    world.assert_no_copies();
    world.assert_same_notes();
    assert_eq!(world.live(Side::Phone)[0].body, "v12");
}

#[test]
fn an_acknowledgement_does_not_hide_a_real_concurrent_edit() {
    // Ack, then both edit from what was acknowledged: still a conflict, one copy.
    let mut world = World::new();
    let id = world.create(Side::Desk, "base");
    world.connect();
    world.edit(Side::Desk, &id, "d1");
    world.push(Side::Desk, &[&id]);
    world.disconnect();
    world.edit(Side::Desk, &id, "d2");
    world.edit(Side::Phone, &id, "p1");
    world.connect();
    for side in SIDES {
        assert_eq!(world.copies(side), 1, "{side:?}");
    }
    world.assert_same_notes();
}

#[test]
fn a_late_acknowledgement_names_an_older_version_of_the_chain_and_conflicts_stay_real() {
    let (mut desk, mut phone) = (Shelf::default(), Shelf::default());
    let id = create_in_shelf(&mut desk, "base").unwrap();
    sync_with(&mut desk, &mut phone, "p");
    edit_body(&mut desk, &id, "d1");
    let d1 = desk.notes[&id].modified;
    let ids = vec![id.clone()];
    sent_to(&mut desk, Kind::Notes, &ids, "p");
    let items = items_packets(Kind::Notes, item_values(&desk, Kind::Notes, &ids));
    // The phone takes d1 and answers; the answer is held back.
    let (mut answers, _) = handle_from(&mut phone, Kind::Notes, &items[0], "desk");
    let ack = answers.pop().unwrap();
    assert_eq!(ack.get_str("op"), Some("ack"));
    // Meanwhile the desk edits again.
    edit_body(&mut desk, &id, "d2");
    // The late acknowledgement arrives: the phone holds d1 (not d2).
    handle_from(&mut desk, Kind::Notes, &ack, "p");
    assert_eq!(desk.held[&id]["p"], d1);
    // The phone edits from d1 while the desk made d2: a real conflict.
    edit_body(&mut phone, &id, "p1");
    sync_with(&mut desk, &mut phone, "p");
    assert_eq!(copies_in(&desk), 1);
    assert_eq!(copies_in(&phone), 1);
    // And the ack never goes backwards: an older one after a newer changes nothing.
    let held = desk.held.clone();
    let stale = {
        let mut packet = NetworkPacket::new(PACKET_TYPE_NOTES);
        packet.set("op", "ack");
        packet.set("items", json!([{"id": id, "modified": d1 - 1}]));
        packet
    };
    handle_from(&mut desk, Kind::Notes, &stale, "p");
    assert_eq!(desk.held, held);
}

/// Real packets, the whole path: items out, serialized, the ack back, serialized.
#[test]
fn the_acknowledgement_travels_the_real_handle_path() {
    let (mut desk, mut phone) = (Shelf::default(), Shelf::default());
    let id = create_in_shelf(&mut desk, "hello").unwrap();
    let modified = desk.notes[&id].modified;
    let items = items_packets(
        Kind::Notes,
        item_values(&desk, Kind::Notes, std::slice::from_ref(&id)),
    );
    let wire =
        |packet: &NetworkPacket| NetworkPacket::deserialize(&packet.serialize().unwrap()).unwrap();
    let (answers, changed) = handle_from(&mut phone, Kind::Notes, &wire(&items[0]), "desk");
    assert!(changed);
    let ack = answers
        .iter()
        .find(|p| p.get_str("op") == Some("ack"))
        .expect("an ack");
    assert_eq!(ack.packet_type, PACKET_TYPE_NOTES);
    assert_eq!(ack.body["items"], json!([{"id": id, "modified": modified}]));
    assert!(desk.held.is_empty());
    let (back, desk_changed) = handle_from(&mut desk, Kind::Notes, &wire(ack), "phone");
    assert!(back.is_empty() && desk_changed);
    assert_eq!(desk.held[&id]["phone"], modified);
    // The same again changes nothing (and asks for nothing).
    let (back, desk_changed) = handle_from(&mut desk, Kind::Notes, &wire(ack), "phone");
    assert!(back.is_empty() && !desk_changed);
    // A snippet shelf never acknowledges.
    let mut snippet = NetworkPacket::new(PACKET_TYPE_SNIPPETS);
    snippet.set("op", "items");
    snippet.set("items", json!([{"id": "s", "text": "t", "modified": 5}]));
    let (answers, _) = handle_from(&mut phone, Kind::Snippets, &snippet, "desk");
    assert!(answers.iter().all(|p| p.get_str("op") != Some("ack")));
}

#[test]
fn what_a_phone_held_is_forgotten_when_it_is_unpaired() {
    let (mut desk, mut phone) = (Shelf::default(), Shelf::default());
    let id = create_in_shelf(&mut desk, "x").unwrap();
    sync_with(&mut desk, &mut phone, "p");
    assert!(desk.held.contains_key(&id) || desk.sent.contains_key(&id));
    desk.forget_phone("p");
    assert!(desk.held.is_empty() && desk.sent.is_empty());
}

// ── An editor over a note that changed underneath it ───────────────────────

#[test]
fn text_typed_over_a_note_deleted_underneath_is_kept_and_brings_it_back() {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "mine").unwrap();
    let loaded = shelf.notes[&id].modified;
    // The phone deletes it while the editor is open.
    nap();
    edit_from_in_shelf(&mut shelf, &id, None, |note| note.deleted = true).unwrap();
    nap();
    let (changed, copy) = edit_from_in_shelf(&mut shelf, &id, Some(loaded), |note| {
        note.body = "typed after".into()
    })
    .unwrap();
    assert!(changed);
    assert!(copy.is_none(), "no copy of a tombstone");
    let note = &shelf.notes[&id];
    assert!(!note.deleted && note.body == "typed after");
    assert_eq!(shelf.notes.len(), 1, "no junk 'New Note (conflicted copy)'");
    // Deleting again an already deleted note stays a deletion.
    edit_from_in_shelf(&mut shelf, &id, None, |note| note.deleted = true).unwrap();
    nap();
    let (changed, _) =
        edit_from_in_shelf(&mut shelf, &id, None, |note| note.deleted = true).unwrap();
    assert!(!changed);
    assert!(shelf.notes[&id].deleted);
}

#[test]
fn saving_an_unchanged_editor_over_a_newer_version_changes_nothing() {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "same").unwrap();
    let loaded = shelf.notes[&id].modified;
    nap();
    edit_from_in_shelf(&mut shelf, &id, None, |note| note.body = "phone's".into()).unwrap();
    let before = shelf.notes.clone();
    let (changed, copy) = edit_from_in_shelf(&mut shelf, &id, Some(loaded), |_| {}).unwrap();
    assert!(!changed && copy.is_none());
    assert_eq!(shelf.notes, before);
}

// ── Empty notes, pins, titles ──────────────────────────────────────────────

#[test]
fn an_empty_never_edited_note_is_not_in_the_index_until_it_is_written_in() {
    let mut shelf = Shelf::default();
    let blank = create_in_shelf(&mut shelf, "").unwrap();
    let spaces = create_in_shelf(&mut shelf, " \n").unwrap();
    let written = create_in_shelf(&mut shelf, "text").unwrap();
    let listed = |shelf: &Shelf| -> Vec<String> {
        index_entries(shelf, Kind::Notes)
            .iter()
            .filter_map(|entry| entry["id"].as_str().map(str::to_string))
            .collect()
    };
    assert_eq!(listed(&shelf), vec![written.clone()]);
    // Fetching it by id still answers, and its first edit lists it.
    assert_eq!(
        item_values(&shelf, Kind::Notes, std::slice::from_ref(&blank)).len(),
        1
    );
    edit_body(&mut shelf, &blank, "now it has words");
    assert!(listed(&shelf).contains(&blank));
    assert!(!listed(&shelf).contains(&spaces));
    // One that was written in and emptied again is a real note and is listed.
    edit_body(&mut shelf, &blank, "");
    assert!(listed(&shelf).contains(&blank));
}

#[test]
fn a_pinned_duplicate_pins_the_note_that_stays() {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "Plan\nbody").unwrap();
    let base = shelf.notes[&id].clone();
    let mut copy = copy_of(&base, "Plan", "Plan\nbody", 51);
    copy.pinned = true;
    shelf.notes.insert(copy.id.clone(), copy.clone());
    let changed = remove_duplicates_in(&mut shelf, false);
    assert!(
        changed.contains(&copy.id) && changed.contains(&id),
        "{changed:?}"
    );
    assert!(shelf.notes[&copy.id].deleted);
    assert!(shelf.notes[&id].pinned, "the pin moved to the survivor");
    // Copy of a copy chain: the pin ends on what stays.
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "Plan\nbody").unwrap();
    let base = shelf.notes[&id].clone();
    let first = copy_of(&base, "Plan", "Plan\nbody", 61);
    let mut second = copy_of(&base, "Plan", "Plan\nbody", 62);
    second.pinned = true;
    shelf.notes.insert(first.id.clone(), first);
    shelf.notes.insert(second.id.clone(), second);
    remove_duplicates_in(&mut shelf, false);
    assert!(shelf.notes[&id].pinned);
}

#[test]
fn titles_are_compared_whole_and_trimmed_alike_on_both_sides() {
    // A 100-character title and a copy that only kept its first 80: not duplicates.
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "body").unwrap();
    shelf.notes.get_mut(&id).unwrap().title = "a".repeat(100);
    let base = shelf.notes[&id].clone();
    let copy = copy_of(&base, &"a".repeat(80), "body", 71);
    shelf.notes.insert(copy.id.clone(), copy);
    assert!(remove_duplicates_in(&mut shelf, true).is_empty());
    // The trimmed characters are an explicit set (the phone trims the same).
    let note = |title: &str, body: &str| Note {
        title: title.into(),
        body: body.into(),
        ..create_note_for_test()
    };
    assert_eq!(note("\u{a0}Hi\u{3000}", "").label(), "Hi");
    assert_eq!(note("", "\u{a0}# Head \u{2028}\nrest").label(), "# Head");
    assert_eq!(note("", "\r\n  \n- item").label(), "item");
    assert_eq!(
        note("\u{85}x\u{85}", "").label(),
        "\u{85}x\u{85}",
        "U+0085 is not trimmed"
    );
}

fn create_note_for_test() -> Note {
    let mut shelf = Shelf::default();
    let id = create_in_shelf(&mut shelf, "").unwrap();
    shelf.notes.remove(&id).unwrap()
}

#[test]
fn both_sides_clearing_the_same_duplicates_at_once_agree() {
    let mut world = World::new();
    let id = world.create(Side::Desk, "Plan\nbody");
    world.connect();
    let base = world.desk.notes[&id].clone();
    for modified in 81..=86 {
        let copy = copy_of(&base, "Plan", "Plan\nbody", modified);
        world.desk.notes.insert(copy.id.clone(), copy.clone());
        world.phone.notes.insert(copy.id.clone(), copy);
    }
    let unique = copy_of(&base, "Plan", "Plan\nbody v2", 87);
    world.desk.notes.insert(unique.id.clone(), unique.clone());
    world.phone.notes.insert(unique.id.clone(), unique.clone());
    world.disconnect();
    // Each side tidies on its own, a moment apart.
    assert_eq!(remove_duplicates_in(&mut world.desk, false).len(), 6);
    nap();
    assert_eq!(remove_duplicates_in(&mut world.phone, false).len(), 6);
    world.connect();
    for side in SIDES {
        let live = world.live(side);
        assert_eq!(live.len(), 2, "{side:?}");
        assert!(live.iter().any(|n| n.id == unique.id));
    }
    world.assert_same_notes();
    // Nothing comes back by syncing again.
    world.disconnect();
    world.connect();
    world.assert_same_notes();
    assert_eq!(world.live(Side::Desk).len(), 2);
}

// ── Speed ──────────────────────────────────────────────────────────────────

#[test]
fn finding_duplicates_in_a_big_shelf_is_quick() {
    let mut shelf = Shelf::default();
    let filler = |n: usize, tag: &str| format!("{tag}{n} {}", "word ".repeat(400));
    for n in 0..200 {
        let id = create_in_shelf(&mut shelf, &filler(n, "note")).unwrap();
        if n < 150 {
            // 300 copies: two per note for 150 notes, each as long as its note.
            let base = shelf.notes[&id].clone();
            for modified in [1000 + n as i64, 2000 + n as i64] {
                let copy = copy_of(&base, &base.label(), &base.body, modified);
                shelf.notes.insert(copy.id.clone(), copy);
            }
        }
    }
    assert_eq!(shelf.notes.len(), 500);
    let start = std::time::Instant::now();
    let doomed = duplicate_copies(&shelf, true);
    let tidied = remove_duplicates_in(&mut shelf, false);
    assert_eq!(doomed.len(), 300);
    assert_eq!(tidied.len(), 300);
    assert!(
        start.elapsed() < std::time::Duration::from_millis(900),
        "took {:?}",
        start.elapsed()
    );
}
