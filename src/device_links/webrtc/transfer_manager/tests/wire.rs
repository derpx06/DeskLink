//! The wire contract: offer, frames and hashes; reading the source in place;
//! the channel window and the unacknowledged limit; two files at once.

use super::*;

// ── Wire details ───────────────────────────────────────────────────────────

#[test]
fn the_offer_the_frames_and_the_hashes_follow_the_contract() {
    let rig = Rig::new("contract");
    let bytes = payload(2 * BLOCK_BYTES as usize + 77, 9);
    let source = rig.source("doc.pdf", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    rig.received();
    let offer = rig.ab.sent(FileAction::Offer).remove(0);
    assert_eq!(offer.protocol_version, 2);
    assert_eq!(offer.filename.as_deref(), Some("doc.pdf"));
    assert_eq!(offer.total_size, Some(bytes.len() as u64));
    assert_eq!(offer.block_size, Some(BLOCK_BYTES));
    assert_eq!(offer.max_frame_bytes, Some(FRAME_BYTES_PROVEN));
    assert!(offer.mtime.is_some_and(|mtime| mtime > 1_000_000_000_000));
    assert_eq!(offer.transfer_id, id);
    assert_eq!(offer.transfer_token.len(), 32, "a 32-hex token");
    // Per-block hashes, then the hash of the hashes.
    let blocks: Vec<[u8; 32]> = bytes
        .chunks(BLOCK_BYTES as usize)
        .map(openssl::sha::sha256)
        .collect();
    let acks = rig.ba.sent(FileAction::BlockAck);
    assert_eq!(acks.len(), 3);
    for (index, ack) in acks.iter().enumerate() {
        assert_eq!(ack.block_index, Some(index as u64));
        assert_eq!(ack.sha256.as_deref(), Some(hex(&blocks[index]).as_str()));
    }
    let done = rig.ab.sent(FileAction::Done).remove(0);
    assert_eq!(
        done.file_hash.as_deref(),
        Some(hex(&hash_of_hashes(&blocks)).as_str())
    );
    assert_eq!(
        (done.blocks, done.total_size),
        (Some(3), Some(bytes.len() as u64))
    );
    let complete = rig.ba.sent(FileAction::Complete).remove(0);
    assert_eq!(complete.file_hash, done.file_hash);
    // Every control message carries the common fields of the binding.
    for control in rig.ab.log() {
        assert_eq!(control.device_id, "alpha");
        assert_eq!((control.session_id, control.connection_generation), (7, 1));
        assert_eq!(control.transfer_token, offer.transfer_token);
    }
}

#[test]
fn the_source_is_read_in_place_with_no_copy_in_the_state_directory() {
    let rig = Rig::new("no-copy");
    rig.ab.freeze();
    let big = rig.source("big.bin", &payload(5 * BLOCK_BYTES as usize, 1));
    let id = rig.a().start_send(&big).unwrap();
    // Only the small record of the send, never a copy of the file.
    eventually("the send's record", || {
        names(&rig.state("a"))
            .contains(&format!(".desklink-{id}.send.json"))
            .then_some(())
    });
    for name in names(&rig.state("a")) {
        let size = fs::metadata(rig.state("a").join(&name)).unwrap().len();
        assert!(size < 4096, "{name} is {size} bytes: a copy of the file?");
    }
    rig.ab.thaw();
    rig.received();
}

#[test]
fn a_sender_stays_inside_the_window_the_channel_allows() {
    let rig = Rig::new("window");
    rig.ab.buffered.store(IN_FLIGHT_TARGET, Ordering::SeqCst);
    let source = rig.source("f.bin", &payload(3 * BLOCK_BYTES as usize, 2));
    rig.a().start_send(&source).unwrap();
    // The offer goes; with the channel full, no frame does.
    eventually("the offer", || {
        (!rig.ab.sent(FileAction::Offer).is_empty()).then_some(())
    });
    steady(
        "no frame while the channel is full",
        Duration::from_millis(200),
        || rig.ab.frames.load(Ordering::SeqCst) == 0,
    );
    rig.ab.buffered.store(0, Ordering::SeqCst);
    rig.received();
}

#[test]
fn a_sender_never_has_more_than_eight_mebibytes_unacknowledged() {
    let rig = Rig::new("unacked");
    // The receiver's acknowledgements never arrive.
    *rig.ba.control_hook.lock().unwrap() = Some(Box::new(|control| {
        if control.action == FileAction::BlockAck {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    }));
    let source = rig.source("f.bin", &payload(20 * BLOCK_BYTES as usize, 3));
    rig.a().start_send(&source).unwrap();
    eventually("the sender to fill its window", || {
        (rig.ab.frame_bytes.load(Ordering::SeqCst) >= UNACKED_MAX).then_some(())
    });
    std::thread::sleep(Duration::from_millis(300));
    let sent = rig.ab.frame_bytes.load(Ordering::SeqCst);
    // Eight blocks of payload plus their frame overhead, not a byte more.
    assert!(
        sent <= UNACKED_MAX + 8 * 18 * 72,
        "{sent} bytes sent into a window of {UNACKED_MAX}"
    );
}

#[test]
fn two_files_move_at_once_and_the_third_waits_for_a_slot() {
    let rig = Rig::new("slots");
    rig.ab.freeze();
    let files: Vec<PathBuf> = (0..3)
        .map(|n| rig.source(&format!("f{n}.bin"), &payload(300_000, n)))
        .collect();
    for file in &files {
        rig.a().start_send(file).unwrap();
    }
    eventually("two offers", || {
        (rig.ab.sent(FileAction::Offer).len() >= 2).then_some(())
    });
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 2, "the third waits");
    rig.ab.thaw();
    for _ in 0..3 {
        rig.received();
    }
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 3);
    assert_eq!(names(&rig.downloads("b")).len(), 3);
}
