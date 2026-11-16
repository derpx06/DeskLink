//! A source deleted or changed while it is sent.

use super::*;

// ── The source changes ─────────────────────────────────────────────────────

#[test]
fn a_file_deleted_mid_send_fails_with_the_file_no_longer_exists() {
    let rig = Rig::new("deleted");
    let source = rig.source("f.bin", &payload(24 * BLOCK_BYTES as usize, 31));
    let doomed = source.clone();
    let mut seen_second_block = false;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        if parse_frame(frame).unwrap().header.block_index == 1 && !seen_second_block {
            seen_second_block = true;
            fs::remove_file(&doomed).unwrap();
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let reason = eventually("the failure", || failed_with(&id));
    assert_eq!(reason, "The file no longer exists.");
    let cancel = rig.ab.sent(FileAction::Cancel).remove(0);
    assert_eq!(cancel.code.as_deref(), Some(code::SOURCE_GONE));
    eventually("both sides clean", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
}

#[test]
fn a_file_changed_mid_send_begins_again_under_a_new_id_and_arrives_whole() {
    let rig = Rig::new("changed");
    let first = payload(24 * BLOCK_BYTES as usize, 32);
    let source = rig.source("f.bin", &first);
    let edited = payload(24 * BLOCK_BYTES as usize + 4_000, 33);
    let (path, bytes) = (source.clone(), edited.clone());
    let mut changed = false;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        if parse_frame(frame).unwrap().header.block_index == 1 && !changed {
            changed = true;
            fs::write(&path, &bytes).unwrap();
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), edited, "the new content");
    assert_ne!(got.transfer_id, id, "a new transfer");
    let cancel = rig.ab.sent(FileAction::Cancel).remove(0);
    assert_eq!(cancel.code.as_deref(), Some(code::SOURCE_CHANGED));
    assert_eq!(
        names(&rig.downloads("b")).len(),
        1,
        "no half-file next to it"
    );
}

#[test]
fn a_file_that_keeps_changing_fails_after_two_restarts() {
    let rig = Rig::new("churn");
    let source = rig.source("f.bin", &payload(24 * BLOCK_BYTES as usize, 34));
    let path = source.clone();
    let mut last = String::new();
    let mut round = 0u64;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        let header = parse_frame(frame).unwrap().header;
        let id = crate::device_links::webrtc::file_protocol::id_string(&header.transfer);
        if header.block_index == 1 && id != last {
            last = id;
            round += 1;
            // One append, one write(2): the file grows between two reads and is never seen
            // half-written. `fs::write` truncates first, and a sender that looked at the file
            // in that moment (slow machine) offered a torn, shorter file that sent cleanly, so
            // no later block 1 came, no further change, and the test waited for a failure
            // that could not come.
            use std::io::Write;
            fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .unwrap()
                .write_all(&vec![0xAB; round as usize])
                .unwrap();
        }
        Fate::Deliver
    }));
    rig.a().start_send(&source).unwrap();
    // Both sides record a failure under the same id (the receiver's says the
    // file changed, the sender's says it keeps changing): wait for the
    // sender's, whichever was recorded first.
    eventually("the failure", || {
        let cancels = rig.ab.sent(FileAction::Cancel);
        let offers = rig.ab.sent(FileAction::Offer);
        let last = offers.last()?;
        (cancels.len() >= 3
            && outcomes(&last.transfer_id).iter().any(|outcome| {
                matches!(outcome, board::Outcome::Failed(reason)
                    if *reason == words::source_changed_for_good())
            }))
        .then_some(())
    });
    assert_eq!(
        rig.ab.sent(FileAction::Offer).len(),
        3,
        "the original and two restarts"
    );
    eventually("everything cleaned", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
}

#[test]
fn troubles_found_in_later_rounds_end_their_own_sends() {
    // Round 1: send 1 fails; round 2: send 0 fails. Removing 1 then 0 is right;
    // the old reverse of the found order removed 0 then the stale 1 (send 2).
    let order: Vec<usize> = super::super::send::highest_index_first(vec![(1, "b"), (0, "a")])
        .into_iter()
        .map(|(index, _)| index)
        .collect();
    assert_eq!(order, vec![1, 0]);
    let order: Vec<usize> =
        super::super::send::highest_index_first(vec![(0, "a"), (2, "c"), (1, "b")])
            .into_iter()
            .map(|(index, _)| index)
            .collect();
    assert_eq!(order, vec![2, 1, 0]);
}
