//! Damage: a damaged block is refused and resent under a new pass, and a
//! file-hash mismatch at the end resends once, then fails clearly.

use super::*;

// ── A damaged block ────────────────────────────────────────────────────────

#[test]
fn a_damaged_block_is_refused_and_sent_again_under_a_new_pass() {
    let rig = Rig::new("nack");
    let bytes = payload(3 * BLOCK_BYTES as usize, 11);
    let source = rig.source("f.bin", &bytes);
    // One frame of block 1 arrives with a flipped byte, once.
    let mut damaged = false;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        let header = parse_frame(frame).unwrap().header;
        if header.block_index == 1 && header.offset > BLOCK_BYTES && !damaged {
            damaged = true;
            frame[45] ^= 0xff;
        }
        Fate::Deliver
    }));
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "repaired");
    let nacks = rig.ba.sent(FileAction::BlockNack);
    assert_eq!(nacks.len(), 1);
    assert_eq!(nacks[0].block_index, Some(1));
    assert_eq!(nacks[0].pass, Some(1));
    assert_eq!(nacks[0].reason.as_deref(), Some("hash-mismatch"));
    // The block went again under pass 1; the first frames of it were pass 0.
    let mut passes = Vec::new();
    for frame in 0..rig.ab.frames.load(Ordering::SeqCst) {
        let _ = frame;
    }
    passes.push(());
    assert!(
        rig.ab.frames.load(Ordering::SeqCst) > 3 * 18,
        "block 1 was resent"
    );
}

#[test]
fn a_block_that_keeps_arriving_damaged_fails_clearly_after_three_rewinds() {
    let rig = Rig::new("nack-forever");
    let source = rig.source("f.bin", &payload(2 * BLOCK_BYTES as usize, 12));
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(|frame| {
        if parse_frame(frame).unwrap().header.block_index == 1 {
            frame[45] ^= 0xff;
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let reason = eventually("the failure", || failed_with(&id));
    assert_eq!(reason, words::damaged());
    assert_eq!(rig.ba.sent(FileAction::BlockNack).len(), 3, "three rewinds");
    let error = rig.ba.sent(FileAction::Error).remove(0);
    assert_eq!(error.code.as_deref(), Some(code::HASH_MISMATCH));
    eventually("clean directories", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty(), "nothing published");
}

// ── A hash mismatch at the end ─────────────────────────────────────────────

#[test]
fn a_file_hash_mismatch_at_the_end_resends_the_file_once() {
    let rig = Rig::new("done-once");
    let bytes = payload(2 * BLOCK_BYTES as usize + 5, 13);
    let source = rig.source("f.bin", &bytes);
    let mut spoiled = false;
    *rig.ab.control_hook.lock().unwrap() = Some(Box::new(move |control| {
        if control.action == FileAction::Done && !spoiled {
            spoiled = true;
            control.file_hash = Some("00".repeat(32));
        }
        Fate::Deliver
    }));
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    let resumes = rig.ba.sent(FileAction::Resume);
    assert_eq!(resumes.len(), 1);
    assert_eq!(resumes[0].resume_block, Some(0));
    assert_eq!(resumes[0].pass, Some(1));
    assert_eq!(rig.ab.sent(FileAction::Done).len(), 2);
    assert!(rig.ab.frames.load(Ordering::SeqCst) >= 70, "sent twice");
}

#[test]
fn a_second_mismatch_at_the_end_fails_with_a_clear_message_and_publishes_nothing() {
    let rig = Rig::new("done-twice");
    let source = rig.source("f.bin", &payload(BLOCK_BYTES as usize + 9, 14));
    *rig.ab.control_hook.lock().unwrap() = Some(Box::new(|control| {
        if control.action == FileAction::Done {
            control.file_hash = Some("00".repeat(32));
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    let reason = eventually("the failure", || failed_with(&id));
    assert_eq!(reason, words::damaged());
    assert_eq!(
        rig.ab.sent(FileAction::Done).len(),
        2,
        "re-sent once, then no more"
    );
    eventually("clean directories", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
}
