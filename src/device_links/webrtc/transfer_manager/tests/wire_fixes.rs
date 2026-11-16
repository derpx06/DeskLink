//! Wire-fix rules the golden transcripts do not cover: pause reasons, bad
//! block acks, the first frame after an accept, unknown codes.

use super::*;

// ── Wire-fix rules (the golden transcripts cover the rest) ────────────────

/// A sender alone, its receiver played by hand.
struct Solo {
    manager: Manager,
    link: Arc<Pipe>,
    base: PathBuf,
    id: String,
    token: String,
}

impl Solo {
    fn new(label: &str, length: usize) -> Self {
        let base = std::env::temp_dir().join(format!(
            "desklink-v2-solo-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(base.join("src")).unwrap();
        let manager = Manager::with(
            base.join("state"),
            base.join("dl"),
            fast(),
            crate::device_links::webrtc::transfer_manager::FILE_QUEUE_MAX_BYTES,
        )
        .unwrap();
        let link = Pipe::new();
        manager.attach(wire_of("alpha", "beta"), link.clone());
        let source = base.join("src").join("f.bin");
        fs::write(&source, payload(length, 5)).unwrap();
        let id = manager.start_send(&source).unwrap();
        let offer = eventually("the offer", || {
            link.sent(FileAction::Offer).into_iter().next()
        });
        Self {
            manager,
            link,
            base,
            id,
            token: offer.transfer_token,
        }
    }

    fn reply(&self, action: FileAction) -> FileControl {
        FileControl::new(&wire_of("beta", "alpha"), action, &self.id, &self.token)
    }

    fn accept(&self) {
        let mut accept = self.reply(FileAction::Accept);
        accept.resume_block = Some(0);
        accept.prefix_sha256 = Some(hex(&hash_of_hashes(&[])));
        accept.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
        self.manager.on_control(accept);
    }
}

impl Drop for Solo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn an_unknown_pause_reason_holds_the_sender_like_user_and_a_bare_resume_releases_it() {
    let solo = Solo::new("unknown-pause", 200_000);
    let mut pause = solo.reply(FileAction::Pause);
    pause.reason = Some("brand-new-reason".to_string());
    solo.manager.on_control(pause);
    solo.accept();
    steady(
        "nothing is sent while paused",
        Duration::from_millis(250),
        || solo.link.frames.load(Ordering::SeqCst) == 0,
    );
    // A bare resume (no fields) is valid and releases it.
    solo.manager.on_control(solo.reply(FileAction::Resume));
    eventually("frames", || {
        (solo.link.frames.load(Ordering::SeqCst) > 0).then_some(())
    });
}

/// The rule for a `block-ack` whose hash is not the one the sender read:
/// the sender rewinds to that block under the next pass (a receiver adopts
/// it); after `MAX_REWINDS` of them it ends with `error {hash-mismatch}`.
#[test]
fn a_block_ack_with_the_wrong_hash_rewinds_under_the_next_pass_then_fails() {
    let solo = Solo::new("ack-mismatch", 70_000);
    let seen = Arc::new(Mutex::new(Vec::<(u8, u64)>::new()));
    let log = Arc::clone(&seen);
    *solo.link.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        let header = parse_frame(frame).unwrap().header;
        log.lock().unwrap().push((header.pass, header.offset));
        Fate::Deliver
    }));
    solo.accept();
    eventually("the first pass", || {
        (seen.lock().unwrap().len() >= 2).then_some(())
    });
    let wrong = |block: u64| {
        let mut ack = solo.reply(FileAction::BlockAck);
        ack.block_index = Some(block);
        ack.sha256 = Some("ee".repeat(32));
        ack
    };
    for round in 1..=3u8 {
        solo.manager.on_control(wrong(0));
        eventually("a rewind", || {
            seen.lock()
                .unwrap()
                .iter()
                .any(|(pass, offset)| *pass == round && *offset == 0)
                .then_some(())
        });
    }
    // The fourth wrong acknowledgement is one rewind too many.
    solo.manager.on_control(wrong(0));
    let error = eventually("the error", || {
        solo.link.sent(FileAction::Error).into_iter().next()
    });
    assert_eq!(error.code.as_deref(), Some(code::HASH_MISMATCH));
}

/// An older phone restarts at block 0 under the SAME pass when it cannot
/// reproduce the prefix: the first frame after an accept is authoritative,
/// not a duplicate.
#[test]
fn the_first_frame_after_an_accept_is_authoritative_even_under_the_same_pass() {
    let hand = Hand::new("same-pass-restart");
    let total = 2 * BLOCK_BYTES;
    let file = payload(total as usize, 9);
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("the accept", || hand.reply(FileAction::Accept));
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0", || hand.reply(FileAction::BlockAck));
    // A re-offer: the receiver says where it is, and waits for the stream.
    hand.send(hand.offer(id(1), "r.bin", total));
    eventually("a second accept", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    // The sender starts over at block 0, still under pass 0.
    for frame in block_frames(id(1), 0, &file, 0, false) {
        hand.manager.on_frame(frame);
    }
    eventually("block 0 acknowledged again", || {
        (hand.link.sent(FileAction::BlockAck).len() == 2).then_some(())
    });
}

/// A source read error is `error {internal}`, never a cancel with a code the
/// phone rejects; unknown cancel and error codes from a peer are a generic
/// end.
#[test]
fn unknown_cancel_and_error_codes_from_a_peer_end_the_transfer_generically() {
    let hand = Hand::new("unknown-codes");
    hand.send(hand.offer(id(1), "one.bin", 10));
    hand.send(hand.offer(id(2), "two.bin", 10));
    eventually("both accepted", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    let mut cancel = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(1),
        "token0",
    );
    cancel.code = Some("brand-new-code".to_string());
    hand.send(cancel);
    let mut error = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Error,
        id(2),
        "token0",
    );
    error.code = Some("another-new-code".to_string());
    hand.send(error);
    eventually("both slots free", || {
        hand.incoming().is_empty().then_some(())
    });
}
