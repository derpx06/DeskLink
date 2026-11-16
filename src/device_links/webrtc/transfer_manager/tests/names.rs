//! Names, the clipboard, gallery captures, unsendable sources and progress.

use super::*;

// ── Names, clipboard, captures ─────────────────────────────────────────────

#[test]
fn a_name_that_is_taken_becomes_name_2() {
    let rig = Rig::new("collision");
    fs::create_dir_all(rig.downloads("b")).unwrap();
    fs::write(rig.downloads("b").join("report.pdf"), b"the first one").unwrap();
    let source = rig.source("report.pdf", b"the second one");
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(got.path.file_name().unwrap(), "report (2).pdf");
    assert_eq!(fs::read(got.path).unwrap(), b"the second one");
    assert_eq!(
        fs::read(rig.downloads("b").join("report.pdf")).unwrap(),
        b"the first one",
        "never overwritten"
    );
}

#[test]
fn two_in_flight_transfers_with_the_same_name_both_survive() {
    let rig = Rig::new("same-name");
    let one = rig.base.join("src").join("one");
    let two = rig.base.join("src").join("two");
    fs::create_dir_all(&one).unwrap();
    fs::create_dir_all(&two).unwrap();
    fs::write(one.join("same.txt"), payload(3_000_000, 1)).unwrap();
    fs::write(two.join("same.txt"), payload(3_000_000, 2)).unwrap();
    rig.a().start_send(&one.join("same.txt")).unwrap();
    rig.a().start_send(&two.join("same.txt")).unwrap();
    rig.received();
    rig.received();
    let mut got = names(&rig.downloads("b"));
    got.sort();
    assert_eq!(got, ["same (2).txt", "same.txt"]);
}

#[test]
fn a_clipboard_image_is_placed_and_not_saved() {
    let rig = Rig::new("clipboard");
    let mut data = b"\x89PNG\r\n\x1a\n".to_vec();
    data.extend(payload(1_000, 61));
    let source = rig.source("clip.png", &data);
    rig.a().start_send_as(&source, Some("image/png")).unwrap();
    let got = rig.received();
    assert!(got.capture_id.is_none());
    eventually("the image on the clipboard", || {
        crate::device_links::daemon::clipboard::TEST_PLACED
            .lock()
            .unwrap()
            .contains(&data)
            .then_some(())
    });
    assert!(names(&rig.downloads("b")).is_empty(), "not a download");
    eventually("clean state", || {
        names(&rig.state("b")).is_empty().then_some(())
    });
    let offer = rig.ab.sent(FileAction::Offer).remove(0);
    assert_eq!(offer.clipboard, Some(true));
    assert_eq!(offer.mime_type.as_deref(), Some("image/png"));
}

/// W12: a gallery offer is refused at the door when the person has not turned
/// photos on for the phone (nothing stored), and when it is not an image;
/// accepted otherwise, and the finished image is kept under the gallery folder,
/// not in Downloads and not on the Transfers page.
#[test]
fn a_gallery_image_is_refused_unless_turned_on_and_is_kept_in_the_gallery() {
    use crate::device_links::gallery;
    let hand = Hand::new("gallery");
    let mut data = vec![0xff, 0xd8, 0xff, 0xe0];
    data.extend(payload(900, 71));
    let gallery_offer = |id: &str, mime: &str| {
        let mut offer = hand.offer(id, "Screenshot_1.jpg", data.len() as u64);
        offer.gallery = Some(true);
        offer.mime_type = Some(mime.into());
        offer
    };
    // Off: refused, with the reason, and nothing is stored.
    crate::device_links::config::set_gallery_for_tests("alpha", false);
    hand.send(gallery_offer(id(1), "image/jpeg"));
    let refusal = eventually("the refusal", || hand.reply(FileAction::Error));
    assert_eq!(refusal.code.as_deref(), Some(code::REFUSED));
    assert_eq!(refusal.message.as_deref(), Some(gallery::OFF));
    assert!(hand.incoming().is_empty());
    // On, but not an image: refused as well.
    crate::device_links::config::set_gallery_for_tests("alpha", true);
    hand.send(gallery_offer(id(2), "application/pdf"));
    eventually("the second refusal", || {
        (hand.link.sent(FileAction::Error).len() == 2).then_some(())
    });
    assert_eq!(
        hand.link.sent(FileAction::Error)[1].message.as_deref(),
        Some(gallery::TYPE)
    );
    // On and an image: accepted, whole, kept in the gallery.
    let before = gallery::charged("alpha");
    hand.send(gallery_offer(id(3), "image/jpeg"));
    eventually("the accept", || hand.reply(FileAction::Accept));
    assert_eq!(
        gallery::charged("alpha"),
        before + 1,
        "charged once accepted"
    );
    // The phone offers it again (a cut link): answered, not charged again.
    hand.send(gallery_offer(id(3), "image/jpeg"));
    eventually("the second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    assert_eq!(gallery::charged("alpha"), before + 1, "a re-offer is free");
    hand.manager
        .on_frame(frame_of(id(3), 0, 0, &data, data.len() as u64));
    let mut message =
        FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(3), "token0");
    message.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&data)])));
    message.blocks = Some(1);
    message.total_size = Some(data.len() as u64);
    hand.send(message);
    eventually("the image in the gallery", || {
        (!gallery::list("alpha").is_empty()).then_some(())
    });
    let kept = gallery::list("alpha");
    assert_eq!(kept[0].name, "Screenshot_1.jpg");
    assert_eq!(std::fs::read(&kept[0].path).unwrap(), data);
    assert!(
        crate::device_links::webrtc::transfer_board::rows()
            .iter()
            .all(|row| row.name != "Screenshot_1.jpg"),
        "no row on the Transfers page"
    );
    assert!(
        names(&hand.base.join("state")).is_empty()
            || !names(&hand.base.join("state"))
                .iter()
                .any(|n| n.ends_with(".jpg"))
    );
    // Turned off while a second one is on its way, or not a picture at all:
    // not kept, and the sender is told.
    let junk = b"<html>not a picture</html>".to_vec();
    hand.send({
        let mut offer = hand.offer(id(4), "x.png", junk.len() as u64);
        offer.gallery = Some(true);
        offer.mime_type = Some("image/png".into());
        offer
    });
    eventually("the third accept", || {
        (hand.link.sent(FileAction::Accept).len() == 3).then_some(())
    });
    hand.manager
        .on_frame(frame_of(id(4), 0, 0, &junk, junk.len() as u64));
    let mut done = FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(4), "token0");
    done.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&junk)])));
    done.blocks = Some(1);
    done.total_size = Some(junk.len() as u64);
    hand.send(done);
    eventually("the refusal to keep it", || {
        (!hand.link.sent(FileAction::Error).is_empty()).then_some(())
    });
    assert_eq!(gallery::list("alpha").len(), 1, "the junk was not kept");
    crate::device_links::config::set_gallery_for_tests("alpha", false);
    gallery::forget("alpha");
    assert!(gallery::list("alpha").is_empty());
}

#[test]
fn a_capture_id_on_an_offer_reaches_the_completion() {
    let hand = Hand::new("capture");
    let capture = uuid::Uuid::new_v4().to_string();
    let mut data = vec![0xff, 0xd8, 0xff, 0xe0];
    data.extend(payload(500, 62));
    let (tx, done) = channel();
    let tx = Mutex::new(tx);
    hand.manager
        .set_completion_hook(move |completed| tx.lock().unwrap().send(completed.clone()).unwrap());
    let mut offer = hand.offer(id(1), "photo.jpg", data.len() as u64);
    offer.clipboard = Some(true);
    offer.mime_type = Some("image/jpeg".into());
    offer.capture_id = Some(capture.clone());
    hand.send(offer);
    eventually("the accept", || hand.reply(FileAction::Accept));
    hand.manager
        .on_frame(frame_of(id(1), 0, 0, &data, data.len() as u64));
    let mut message =
        FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(1), "token0");
    message.file_hash = Some(hex(&hash_of_hashes(&[openssl::sha::sha256(&data)])));
    message.blocks = Some(1);
    message.total_size = Some(data.len() as u64);
    hand.send(message);
    let completed = done.recv_timeout(Duration::from_secs(10)).unwrap();
    assert_eq!(completed.capture_id.as_deref(), Some(capture.as_str()));
    // Not a UUID: ignored.
    let mut odd = hand.offer(id(2), "photo2.jpg", 10);
    odd.capture_id = Some("not-a-uuid".into());
    odd.clipboard = Some(true);
    odd.mime_type = Some("image/jpeg".into());
    hand.send(odd);
    eventually("the second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
}

#[test]
fn unsendable_sources_are_refused_before_anything_is_offered() {
    let rig = Rig::new("unsendable");
    let directory = rig.base.join("src");
    assert!(rig.a().start_send(&directory).is_err(), "a directory");
    assert!(
        rig.a().start_send(&rig.base.join("nope.bin")).is_err(),
        "missing"
    );
    // A name the peer would refuse.
    let odd = rig.base.join("src").join("a\u{1}b");
    fs::write(&odd, b"x").unwrap();
    assert!(
        rig.a().start_send(&odd).is_ok(),
        "a control character is only a name"
    );
    std::thread::sleep(Duration::from_millis(100));
    // Nothing for the refused ones.
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 1);
    // Detached: nothing starts.
    rig.a().detach();
    assert!(rig.a().start_send(&odd).is_err());
}

#[test]
fn a_progress_row_moves_with_the_blocks_and_never_goes_back() {
    // A sender alone (the receiver is played by hand), so the row is its own.
    let link = Pipe::new();
    let base = std::env::temp_dir().join(format!("desklink-v2-progress-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(base.join("src")).unwrap();
    let manager = Manager::with(
        base.join("state"),
        base.join("dl"),
        fast(),
        crate::device_links::webrtc::transfer_manager::FILE_QUEUE_MAX_BYTES,
    )
    .unwrap();
    manager.attach(wire_of("alpha", "beta"), link.clone());
    let file = payload(4 * BLOCK_BYTES as usize, 71);
    let source = base.join("src").join("f.bin");
    fs::write(&source, &file).unwrap();
    let id = manager.start_send(&source).unwrap();
    let offer = eventually("the offer", || {
        link.sent(FileAction::Offer).into_iter().next()
    });
    let reply = |action: FileAction| {
        FileControl::new(
            &wire_of("beta", "alpha"),
            action,
            &id,
            &offer.transfer_token,
        )
    };
    let mut accept = reply(FileAction::Accept);
    accept.resume_block = Some(0);
    accept.prefix_sha256 = Some(hex(&hash_of_hashes(&[])));
    accept.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
    manager.on_control(accept);
    let total = 4 * BLOCK_BYTES;
    let mut last = 0;
    // The sender's window lets all four blocks out; none is acknowledged yet.
    eventually("the frames", || {
        (link.frames.load(Ordering::SeqCst) >= 4 * 18).then_some(())
    });
    std::thread::sleep(Duration::from_millis(50));
    let row = board::peek_row(&id).expect("listed");
    assert_eq!(row.total, total);
    last = last.max(row.done);
    // Each acknowledgement moves it forward by a block.
    for block in 0..4u64 {
        let mut ack = reply(FileAction::BlockAck);
        ack.block_index = Some(block);
        ack.sha256 = Some(hex(&openssl::sha::sha256(
            &file[(block * BLOCK_BYTES) as usize..((block + 1) * BLOCK_BYTES) as usize],
        )));
        manager.on_control(ack);
        let wanted = (block + 1) * BLOCK_BYTES;
        let row = eventually("the row to move", || {
            board::peek_row(&id).filter(|row| row.done >= wanted.min(total))
        });
        assert!(row.done >= last, "went back from {last} to {}", row.done);
        last = row.done;
    }
    // Everything acknowledged: `done`, then the receiver's `complete` ends it.
    let done = eventually("done", || link.sent(FileAction::Done).into_iter().next());
    let mut complete = reply(FileAction::Complete);
    complete.file_hash = done.file_hash.clone();
    manager.on_control(complete);
    eventually("the sent row to be recorded", || {
        was_sent(&id).then_some(())
    });
    assert_eq!(last, total);
    let _ = fs::remove_dir_all(base);
}

#[test]
fn blocks_for_matches_what_the_frames_carry() {
    assert_eq!(blocks_for(3 * BLOCK_BYTES + 1), 4);
}
