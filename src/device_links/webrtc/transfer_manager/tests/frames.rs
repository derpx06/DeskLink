//! Frames and offers that must change nothing or be answered: duplicates,
//! out-of-order and unknown frames, old passes, slots, offers with no room,
//! wrong bindings, re-offers, and unpairing mid-transfer.

use super::*;

#[test]
fn a_duplicate_frame_is_ignored_and_the_file_is_not_corrupted() {
    let hand = Hand::new("duplicate");
    let data = payload(100, 1);
    hand.send(hand.offer(id(1), "d.bin", 100));
    eventually("the accept", || hand.reply(FileAction::Accept));
    let frame = frame_of(id(1), 0, 0, &data, 100);
    hand.manager.on_frame(frame.clone());
    hand.manager.on_frame(frame);
    hand.settle();
    assert_eq!(
        hand.link.sent(FileAction::BlockAck).len(),
        1,
        "acknowledged once"
    );
    let mut done = FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
    done.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&data)])));
    done.blocks = Some(1);
    done.total_size = Some(100);
    hand.send(done);
    eventually("the complete", || hand.reply(FileAction::Complete));
    assert_eq!(fs::read(hand.base.join("dl").join("d.bin")).unwrap(), data);
}

#[test]
fn a_frame_out_of_order_ends_the_transfer_without_publishing_anything() {
    let hand = Hand::new("gap");
    let total = BLOCK_BYTES + 10;
    hand.send(hand.offer(id(1), "g.bin", total));
    eventually("the accept", || hand.reply(FileAction::Accept));
    // Block 1 first: block 0 never came.
    hand.manager
        .on_frame(frame_of(id(1), 0, BLOCK_BYTES, &payload(10, 2), total));
    let error = eventually("the error", || hand.reply(FileAction::Error));
    assert_eq!(error.code.as_deref(), Some(code::BAD_FRAME));
    assert!(hand.incoming().is_empty());
    assert!(
        names(&hand.base.join("state")).is_empty(),
        "no partial left"
    );
    assert!(names(&hand.base.join("dl")).is_empty());
    assert!(failed_with(id(1)).is_some());
}

#[test]
fn a_frame_for_an_unknown_transfer_changes_nothing_and_gets_no_reply() {
    let hand = Hand::new("unknown");
    hand.manager
        .on_frame(frame_of(id(2), 0, 0, &payload(50, 3), 50));
    hand.settle();
    assert!(hand.incoming().is_empty());
    assert!(hand.link.log().is_empty(), "no reply to a stranger");
    assert!(names(&hand.base.join("state")).is_empty());
    // Not a frame at all.
    hand.manager.on_frame(vec![1, 2, 3]);
    hand.manager.on_frame(b"DLF1 an old frame".to_vec());
    hand.settle();
    assert!(hand.link.log().is_empty());
}

#[test]
fn a_frame_past_the_size_is_dropped_and_the_transfer_goes_on() {
    let hand = Hand::new("past-size");
    let data = payload(100, 4);
    hand.send(hand.offer(id(1), "p.bin", 100));
    eventually("the accept", || hand.reply(FileAction::Accept));
    // An offset beyond the end, and a block index that disagrees.
    hand.manager
        .on_frame(frame_of(id(1), 0, 5_000, &payload(10, 5), 100_000));
    let mut lying = frame_of(id(1), 0, 0, &data, 100);
    lying[27] = 9;
    hand.manager.on_frame(lying);
    hand.settle();
    assert!(hand.link.sent(FileAction::Error).is_empty());
    assert_eq!(hand.incoming().len(), 1, "still receiving");
    hand.manager.on_frame(frame_of(id(1), 0, 0, &data, 100));
    eventually("the acknowledgement", || hand.reply(FileAction::BlockAck));
}

#[test]
fn a_frame_from_an_old_pass_is_dropped_after_a_refusal() {
    let hand = Hand::new("stale-pass");
    let file = payload(BLOCK_BYTES as usize, 6);
    hand.send(hand.offer(id(1), "s.bin", BLOCK_BYTES));
    eventually("the accept", || hand.reply(FileAction::Accept));
    // A block whose hash is wrong: refused, pass 1 from now on.
    for frame in block_frames(id(1), 0, &file, 0, true) {
        hand.manager.on_frame(frame);
    }
    eventually("the nack", || hand.reply(FileAction::BlockNack));
    // Frames still in flight from pass 0 are ignored...
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    hand.settle();
    assert!(hand.link.sent(FileAction::BlockAck).is_empty());
    // ...the resent block, under pass 1, is taken.
    for frame in block_frames(id(1), 1, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("the acknowledgement", || hand.reply(FileAction::BlockAck));
}

#[test]
fn a_third_incoming_transfer_is_told_to_wait_and_is_accepted_when_a_slot_frees() {
    let hand = Hand::new("busy");
    hand.send(hand.offer(id(1), "one.bin", 10));
    hand.send(hand.offer(id(2), "two.bin", 10));
    hand.send(hand.offer(id(3), "three.bin", 10));
    let busy = eventually("busy", || hand.reply(FileAction::Error));
    assert_eq!(busy.code.as_deref(), Some(code::BUSY));
    assert_eq!(busy.transfer_id, id(3));
    assert_eq!(hand.link.sent(FileAction::Accept).len(), 2);
    assert!(failed_with(id(3)).is_none(), "busy is never shown");
    // The sender offers again once one is done.
    let mut cancel = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    cancel.code = Some(code::USER.to_string());
    hand.send(cancel);
    hand.send(hand.offer(id(3), "three.bin", 10));
    eventually("the third is accepted", || {
        (hand.link.sent(FileAction::Accept).len() == 3).then_some(())
    });
}

/// An offer is refused at the door, in words, when the disk would keep less
/// than the margin or the phone's unfinished downloads would pass the cap;
/// not halfway through the file.
#[test]
fn an_offer_with_no_room_is_refused_before_a_byte_arrives() {
    let hand = Hand::new("no-room");
    hand.manager.probe(|engine| {
        engine.faults.free_bytes =
            Some(crate::device_links::webrtc::transfer_manager::recv::FREE_MARGIN_BYTES)
    });
    hand.send(hand.offer(id(1), "big.bin", 10 * BLOCK_BYTES));
    let error = eventually("the refusal", || hand.reply(FileAction::Error));
    assert_eq!(error.code.as_deref(), Some(code::REFUSED));
    assert!(error.message.unwrap_or_default().contains("free space"));
    assert!(hand.incoming().is_empty());

    // Four 4 GiB partials from this phone already wait (16 GiB): one more is
    // refused.
    let full = Hand::new("partials-full");
    let fresh = || uuid::Uuid::new_v4().hyphenated().to_string();
    let (mine, theirs): (Vec<String>, String) = ((0..4).map(|_| fresh()).collect(), fresh());
    full.manager.probe(move |engine| {
        engine.faults.free_bytes = Some(1 << 50);
        for held in &mine {
            write_checkpoint(
                &engine.store,
                held,
                "alpha",
                crate::device_links::webrtc::transfer_manager::MAX_TRANSFER_BYTES,
            );
        }
        write_checkpoint(
            &engine.store,
            &theirs,
            "someone-else",
            crate::device_links::webrtc::transfer_manager::MAX_TRANSFER_BYTES,
        );
    });
    full.send(full.offer(id(1), "one-more.bin", BLOCK_BYTES));
    let error = eventually("the refusal", || full.reply(FileAction::Error));
    assert_eq!(error.code.as_deref(), Some(code::REFUSED));
    assert!(error.message.unwrap_or_default().contains("unfinished"));

    // Unknown free space is not a refusal; only this phone's partials count.
    assert_eq!(
        crate::device_links::webrtc::transfer_manager::recv::room_refusal(10, 0, None, 0),
        None
    );
    assert_eq!(
        crate::device_links::webrtc::transfer_manager::recv::room_refusal(10, 0, Some(1 << 40), 0),
        None
    );
    assert_eq!(
        full.manager
            .probe(|engine| engine.store.held_for("alpha").len()),
        4
    );
}

fn write_checkpoint(
    store: &crate::device_links::webrtc::transfer_manager::store::Store,
    id: &str,
    device: &str,
    total: u64,
) {
    let checkpoint = crate::device_links::webrtc::transfer_manager::store::ReceiveCheckpoint {
        transfer_id: id.to_string(),
        device_id: device.to_string(),
        token: "t".into(),
        filename: "f.bin".into(),
        destination_name: "f.bin".into(),
        total_size: total,
        mtime: 0,
        blocks_done: 0,
        clipboard: false,
        gallery: false,
        capture_id: None,
        folder: None,
        content_sha256: None,
    };
    store.save_receive(&checkpoint, &[]).unwrap();
}

#[test]
fn an_offer_that_cannot_be_accepted_is_answered_not_dropped() {
    let hand = Hand::new("refuse");
    // Too large for DeskLink to receive.
    let mut huge = hand.offer(id(1), "huge.bin", 10);
    huge.total_size = Some(crate::device_links::webrtc::transfer_manager::MAX_TRANSFER_BYTES + 1);
    hand.send(huge);
    let error = eventually("the answer", || hand.reply(FileAction::Error));
    assert_eq!(error.code.as_deref(), Some(code::TOO_LARGE));
    // A block size that is not the contract's.
    let mut odd = hand.offer(id(2), "odd.bin", 10);
    odd.block_size = Some(1000);
    hand.send(odd);
    eventually("the second answer", || {
        (hand.link.sent(FileAction::Error).len() == 2).then_some(())
    });
    // An unsafe name.
    hand.send(hand.offer(id(3), "../escape", 10));
    eventually("the third answer", || {
        (hand.link.sent(FileAction::Error).len() == 3).then_some(())
    });
    assert!(hand.incoming().is_empty());
    assert!(names(&hand.base.join("state")).is_empty());
}

#[test]
fn messages_with_a_wrong_binding_or_token_change_nothing() {
    let hand = Hand::new("binding");
    hand.send(hand.offer(id(1), "b.bin", 100));
    eventually("the accept", || hand.reply(FileAction::Accept));
    let before = hand.link.log().len();
    // The wrong session, the wrong device, the wrong token.
    let mut wrong_session = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    wrong_session.session_id = 99;
    let mut wrong_device = FileControl::new(
        &wire_of("mallory", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    wrong_device.code = Some(code::USER.to_string());
    let wrong_token = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "not-the-token",
    );
    for message in [wrong_session, wrong_device, wrong_token] {
        hand.send(message);
    }
    // A forged error and a forged pause with the wrong token too.
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Error,
        id(1),
        "nope",
    ));
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Pause,
        id(1),
        "nope",
    ));
    hand.settle();
    assert_eq!(hand.incoming(), [id(1)], "still there");
    assert_eq!(hand.link.log().len(), before, "and nothing was answered");
    // About a transfer we do not have: no reply either.
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(2),
        "token0",
    ));
    hand.send(FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::BlockAck,
        id(2),
        "token0",
    ));
    hand.settle();
    assert_eq!(hand.link.log().len(), before);
}

#[test]
fn an_offer_for_a_transfer_already_open_is_answered_from_where_it_is() {
    let hand = Hand::new("replay");
    let total = 2 * BLOCK_BYTES;
    let file = payload(total as usize, 7);
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("the accept", || hand.reply(FileAction::Accept));
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0", || hand.reply(FileAction::BlockAck));
    // The same offer again: not a second transfer, and it says what it has.
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("a second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    let again = hand.link.sent(FileAction::Accept).remove(1);
    assert_eq!(again.resume_block, Some(1));
    let first_block = openssl::sha::sha256(&file[..BLOCK_BYTES as usize]);
    assert_eq!(
        again.prefix_sha256.as_deref(),
        Some(hex(&hash_of_hashes(&[first_block])).as_str())
    );
    assert_eq!(hand.incoming().len(), 1);
    // An offer with the same id and another token is refused.
    let mut forged = hand.offer(id(1), "r.bin", total);
    forged.transfer_token = "another".to_string();
    hand.send(forged);
    eventually("the refusal", || hand.reply(FileAction::Error));
}

#[test]
fn a_re_offer_without_a_lane_stops_wanting_one() {
    let hand = Hand::new("reoffer-lane");
    let total = 9 << 20;
    hand.send(hand.offer(id(4), "big.bin", total));
    eventually("the accept", || hand.reply(FileAction::Accept));
    // The first offer asked for a lane and this side could take it...
    hand.manager.probe(|engine| {
        for incoming in engine.incoming.values_mut() {
            incoming.lane_wanted = true;
        }
    });
    // ...the same transfer is offered again without one (the sender gave up on
    // lanes): this offer decides, so no token is issued for it.
    hand.send(hand.offer(id(4), "big.bin", total));
    eventually("a second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    let wanted = hand.manager.probe(|engine| {
        engine
            .incoming
            .values()
            .any(|incoming| incoming.lane_wanted)
    });
    assert!(!wanted);
}

#[test]
fn an_unpair_deletes_the_phones_partial_download_and_its_closing_peer_does_not_write_it_back() {
    let peer = format!("unpair-{}", uuid::Uuid::new_v4().simple());
    let hand = Hand::with_peer("unpair", &peer);
    let total = 2 * BLOCK_BYTES;
    let file = payload(total as usize, 9);
    let mut offer = FileControl::new(&wire_of(&peer, "beta"), FileAction::Offer, id(2), "token0");
    offer.filename = Some("u.bin".to_string());
    offer.total_size = Some(total);
    offer.mtime = Some(1_700_000_000_000);
    offer.block_size = Some(BLOCK_BYTES);
    offer.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
    hand.send(offer);
    eventually("the accept", || hand.reply(FileAction::Accept));
    for frame in block_frames(id(2), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0", || hand.reply(FileAction::BlockAck));
    let state = hand.base.join("state");
    assert!(!names(&state).is_empty(), "a partial is kept while paired");
    // Unpaired: its files go, and the peer closing behind it checkpoints nothing.
    crate::device_links::webrtc::transfer_manager::forget_device(&peer);
    hand.manager.detach();
    hand.settle();
    assert_eq!(names(&state), Vec::<String>::new());
    assert!(hand.incoming().is_empty());
}

#[test]
fn a_phone_paired_again_right_after_an_unpair_keeps_its_new_transfer_through_a_detach() {
    let peer = format!("repair-{}", uuid::Uuid::new_v4().simple());
    let old = Hand::with_peer("repair-old", &peer);
    crate::device_links::webrtc::transfer_manager::forget_device(&peer);
    // Same device id (it is frozen), a new pairing, a new peer and manager.
    let hand = Hand::with_peer("repair-new", &peer);
    let total = 2 * BLOCK_BYTES;
    let file = payload(total as usize, 10);
    let mut offer = FileControl::new(&wire_of(&peer, "beta"), FileAction::Offer, id(3), "token0");
    offer.filename = Some("r.bin".to_string());
    offer.total_size = Some(total);
    offer.mtime = Some(1_700_000_000_000);
    offer.block_size = Some(BLOCK_BYTES);
    offer.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
    hand.send(offer);
    eventually("the accept", || hand.reply(FileAction::Accept));
    for frame in block_frames(id(3), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0", || hand.reply(FileAction::BlockAck));
    hand.manager.detach();
    hand.settle();
    assert!(!names(&hand.base.join("state")).is_empty(), "checkpointed");
    assert_eq!(hand.incoming().len(), 1, "the new transfer is not dropped");
    // The old peer's engine, closing behind the unpair, drops what it held.
    old.manager.detach();
    old.settle();
}
