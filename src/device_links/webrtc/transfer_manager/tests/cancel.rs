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

/// Cancelled while the phone is away (the engine is detached, no one to
/// tell): remembered, so the phone's re-offer at the next connection is
/// refused instead of starting it again.
#[test]
fn a_cancel_while_detached_is_remembered_for_the_re_offer() {
    let rig = Rig::new("cancel-away");
    let source = rig.source("f.bin", &payload(6 * BLOCK_BYTES as usize, 24));
    let id = rig.a().start_send(&source).unwrap();
    eventually("some blocks", || {
        (!rig.ba.sent(FileAction::BlockAck).is_empty()).then_some(())
    });
    rig.a().detach();
    rig.a().cancel_transfer(&id, code::USER).unwrap();
    assert!(
        crate::device_links::webrtc::transfer_manager::store::cancelled_while_away(&rig.ids.1, &id)
    );
}

// ── A finished transfer offered or closed again (audit X9) ─────────────────

/// A manager for the same peer and the same directories, with a link of its
/// own: the next negotiation attempt after a link loss, or the next run.
fn reconnected(hand: &Hand) -> (Arc<Manager>, Arc<Pipe>) {
    let manager = Arc::new(
        Manager::with(
            hand.base.join("state"),
            hand.base.join("dl"),
            fast(),
            crate::device_links::webrtc::transfer_manager::FILE_QUEUE_MAX_BYTES,
        )
        .unwrap(),
    );
    let link = Pipe::new();
    manager.attach(wire_of("beta", "alpha"), link.clone());
    (manager, link)
}

/// The receiver has placed a file and its `complete` never reached the sender
/// (a link lost in between). After a reconnect (a new manager, as every
/// negotiation attempt makes) the sender's `done` or its re-offer of the same
/// id is answered `complete` / accepted at the end, never opened again from
/// block 1 and placed a second time ("name (2)").
#[test]
fn a_finished_transfer_offered_or_closed_again_after_a_reconnect_is_answered_not_received_twice() {
    let hand = Hand::new("finished-again");
    let data = payload(50_000, 91); // one frame (at most 61 440 bytes)
    let hash = hex(&hash_of_hashes(&[openssl::sha::sha256(&data)]));
    let offer = |name: &str| hand.offer(id(1), name, data.len() as u64);
    let done = |hash: &str| {
        let mut done =
            FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
        done.file_hash = Some(hash.to_string());
        done.blocks = Some(1);
        done.total_size = Some(data.len() as u64);
        done
    };
    hand.send(offer("again.bin"));
    eventually("the accept", || hand.reply(FileAction::Accept));
    hand.manager
        .on_frame(frame_of(id(1), 0, 0, &data, data.len() as u64));
    hand.send(done(&hash));
    eventually("the complete", || hand.reply(FileAction::Complete));
    let placed = names(&hand.base.join("dl"));
    assert_eq!(placed.len(), 1, "{placed:?}");

    // The link was lost before the sender heard: a new manager, the same
    // directories.
    let (manager, link) = reconnected(&hand);
    let settle = || manager.probe(|_| ());
    // The sender sends `done` again.
    manager.on_control(done(&hash));
    settle();
    assert_eq!(link.sent(FileAction::Complete).len(), 1, "answered once");
    assert_eq!(
        link.sent(FileAction::Complete)[0].file_hash.as_deref(),
        Some(hash.as_str())
    );
    // A `done` for another file under that id is an error, as on the phone.
    manager.on_control(done(&"cd".repeat(32)));
    settle();
    let refusal = &link.sent(FileAction::Error)[0];
    assert_eq!(refusal.code.as_deref(), Some(code::HASH_MISMATCH));
    // Or it offers the same transfer again: accepted as already whole.
    manager.on_control(offer("again.bin"));
    settle();
    let accept = &link.sent(FileAction::Accept)[0];
    assert_eq!(accept.resume_block, Some(1), "resume at the end");
    assert_eq!(accept.prefix_sha256.as_deref(), Some(hash.as_str()));
    assert!(manager.probe(|engine| engine.incoming_ids()).is_empty());
    assert_eq!(names(&hand.base.join("dl")), placed, "no second copy");

    // Not the same file (another name under that id) or another id with the
    // same bytes: each a transfer of its own, opened and accepted from block 0.
    manager.on_control(offer("other.bin"));
    manager.on_control(hand.offer(id(2), "again.bin", data.len() as u64));
    settle();
    assert_eq!(link.sent(FileAction::Accept).len(), 3);
    assert_eq!(link.sent(FileAction::Accept)[1].resume_block, Some(0));
    assert_eq!(link.sent(FileAction::Accept)[2].resume_block, Some(0));
    assert_eq!(manager.probe(|engine| engine.incoming_ids()).len(), 2);
}

/// CONC-2: a Bucket file placed here has no wanted entry any more. Its
/// re-offer after a lost `complete` is answered from the done record (accepted
/// at the end), not refused by the folder gate that ran before it.
#[test]
fn a_placed_bucket_files_reoffer_is_answered_from_the_done_record_not_refused() {
    let hand = Hand::new("finished-bucket");
    let data = payload(50_000, 92);
    let hash = hex(&hash_of_hashes(&[openssl::sha::sha256(&data)]));
    let mut done = FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
    done.file_hash = Some(hash.clone());
    done.blocks = Some(1);
    done.total_size = Some(data.len() as u64);
    hand.send(hand.offer(id(1), "bucket.bin", data.len() as u64));
    eventually("the accept", || hand.reply(FileAction::Accept));
    hand.manager
        .on_frame(frame_of(id(1), 0, 0, &data, data.len() as u64));
    hand.send(done);
    eventually("the complete", || hand.reply(FileAction::Complete));

    let (manager, link) = reconnected(&hand);
    let mut again = hand.offer(id(1), "bucket.bin", data.len() as u64);
    again.folder = Some("bucket".into());
    again.relpath = Some("Trips/bucket.bin".into());
    again.content_sha256 = Some(hex(&openssl::sha::sha256(&data)));
    manager.on_control(again);
    manager.probe(|_| ());
    assert!(link.sent(FileAction::Error).is_empty(), "not refused");
    let accept = &link.sent(FileAction::Accept)[0];
    assert_eq!(accept.resume_block, Some(1));
    assert_eq!(accept.prefix_sha256.as_deref(), Some(hash.as_str()));
}
