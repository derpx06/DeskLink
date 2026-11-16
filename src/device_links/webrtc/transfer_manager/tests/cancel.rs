//! The Bucket's requested files, and cancelling from either side.

use super::*;

// ── The Bucket ─────────────────────────────────────────────────────────────

#[test]
fn unrequested_bucket_data_is_refused_and_a_requested_file_is_accepted() {
    use crate::device_links::webrtc::file_protocol::FolderRef;
    let hand = Hand::new("bucket");
    let relpath = format!("docs/{}.txt", uuid::Uuid::new_v4().simple());
    let data = payload(200, 8);
    let sha = hex(&openssl::sha::sha256(&data));
    let bucket_offer = |id: &str, sha: &str, size: u64| {
        let mut offer = hand.offer(id, relpath.rsplit('/').next().unwrap(), size);
        offer.folder = Some("bucket".into());
        offer.relpath = Some(relpath.clone());
        offer.content_sha256 = Some(sha.to_string());
        offer
    };
    // Nobody asked for it.
    hand.send(bucket_offer(id(1), &sha, 200));
    let refusal = eventually("the refusal", || hand.reply(FileAction::Error));
    assert_eq!(refusal.code.as_deref(), Some(code::REFUSED));
    assert_eq!(refusal.message.as_deref(), Some("folder-refused"));
    assert!(hand.incoming().is_empty());
    assert!(names(&hand.base.join("state")).is_empty(), "nothing stored");
    // Asked for, but the hash or the size differ: refused as well.
    let wanted = FolderRef {
        folder: "bucket".into(),
        relpath: relpath.clone(),
        mtime: None,
    };
    crate::device_links::folders::test_want("alpha", &wanted, &sha, 200);
    hand.send(bucket_offer(id(2), &"ab".repeat(32), 200));
    hand.send(bucket_offer(id(3), &sha, 201));
    eventually("two more refusals", || {
        (hand.link.sent(FileAction::Error).len() == 3).then_some(())
    });
    // Asked for: accepted, taken, and handed to folder sync (not Downloads).
    hand.send(bucket_offer(id(1), &sha, 200));
    eventually("the accept", || hand.reply(FileAction::Accept));
    hand.manager.on_frame(frame_of(id(1), 0, 0, &data, 200));
    let mut done = FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
    done.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&data)])));
    done.blocks = Some(1);
    done.total_size = Some(200);
    hand.send(done);
    eventually("the complete", || hand.reply(FileAction::Complete));
    assert!(names(&hand.base.join("dl")).is_empty(), "not a download");
}

// ── Cancel ─────────────────────────────────────────────────────────────────

#[test]
fn the_sender_cancelling_cleans_up_both_sides_and_later_frames_are_ignored() {
    let rig = Rig::new("cancel-sender");
    let source = rig.source("f.bin", &payload(6 * BLOCK_BYTES as usize, 21));
    let id = rig.a().start_send(&source).unwrap();
    eventually("some blocks", || {
        (!rig.ba.sent(FileAction::BlockAck).is_empty()).then_some(())
    });
    rig.ab.freeze();
    rig.a().cancel_transfer(&id, code::USER).unwrap();
    // Frames already on the wire arrive after the cancel: they find nothing.
    rig.ab.thaw();
    eventually("the receiver to clean up", || {
        (names(&rig.state("b")).is_empty() && names(&rig.state("a")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
    assert!(was_cancelled(&id));
    assert!(rig
        .ab
        .sent(FileAction::Cancel)
        .iter()
        .all(|c| c.code.as_deref() == Some("user")));
    assert!(
        rig.a().cancel_transfer(&id, code::USER).is_err(),
        "no longer running"
    );
}

#[test]
fn the_receiver_cancelling_cleans_up_both_sides() {
    let rig = Rig::new("cancel-receiver");
    let source = rig.source("f.bin", &payload(6 * BLOCK_BYTES as usize, 22));
    rig.ab.freeze();
    let id = rig.a().start_send(&source).unwrap();
    // Let a little arrive first.
    rig.ab.thaw();
    eventually("the receiver to know the transfer", || {
        rig.b()
            .probe(|engine| engine.incoming_ids())
            .first()
            .cloned()
    });
    rig.b().cancel_transfer(&id, code::USER).unwrap();
    eventually("both to clean up", || {
        (names(&rig.state("b")).is_empty() && names(&rig.state("a")).is_empty()).then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty());
    assert!(rig.a().cancel_transfer(&id, code::USER).is_err());
}

#[test]
fn a_bucket_item_removed_while_it_moves_is_cancelled_as_removed() {
    let rig = Rig::new("removed");
    let source = rig.source("f.bin", &payload(8 * BLOCK_BYTES as usize, 23));
    rig.ab.freeze();
    let id = rig.a().start_send(&source).unwrap();
    eventually("the offer", || {
        (!rig.ab.sent(FileAction::Offer).is_empty()).then_some(())
    });
    rig.a().cancel_transfer(&id, code::REMOVED).unwrap();
    let cancel = rig.ab.sent(FileAction::Cancel).remove(0);
    assert_eq!(cancel.code.as_deref(), Some("removed"));
}
