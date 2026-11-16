//! A phone-style offer for a multi-block file, and dropped-frame reports.

use super::*;

// ── What a phone's offer for a multi-block file looks like ─────────────────

/// A phone sending a spooled content-URI file: no `contentSha256`, no
/// folder, an `mtime` the phone makes up (0, or the time it was spooled),
/// whatever frame size it likes. Every such offer for a 3 MiB file is
/// accepted and the file arrives whole.
#[test]
fn a_phone_style_offer_for_a_three_mebibyte_file_is_accepted_and_arrives() {
    let (data, hashes) = three_blocks(41);
    let total = data.len() as u64;
    // 2 MiB + 777 bytes is three blocks; use 3 MiB exactly as well.
    let variants: [(&str, i64, Option<usize>); 5] = [
        ("zero", 0, Some(FRAME_BYTES_PROVEN)),
        ("negative", -1, Some(FRAME_BYTES_PROVEN)),
        ("spooled-now", 1_760_000_000_123, None),
        ("big-frame", 1_700_000_000_000, Some(196_608)),
        ("max-frame", 1_700_000_000_000, Some(65_536)),
    ];
    for (n, (label, mtime, frame)) in variants.into_iter().enumerate() {
        let hand = Hand::new(&format!("phone-offer-{label}"));
        let mut offer = hand.offer(id(1), &format!("{label}.bin"), total);
        offer.mtime = Some(mtime);
        offer.max_frame_bytes = frame;
        hand.send(offer);
        let accept = eventually("the accept", || hand.reply(FileAction::Accept));
        assert_eq!(accept.resume_block, Some(0), "{label} (variant {n})");
        for index in 0..hashes.len() as u64 {
            for frame in block_frames(id(1), 0, &data, index, false) {
                hand.manager.on_frame(frame);
            }
        }
        say_done(&hand, id(1), total, &hashes);
        eventually("the complete", || hand.reply(FileAction::Complete));
        assert_eq!(
            fs::read(hand.base.join("dl").join(format!("{label}.bin"))).unwrap(),
            data,
            "{label}"
        );
        assert!(hand.link.sent(FileAction::Error).is_empty(), "{label}");
    }
}

/// A frame with no transfer is dropped with its id and the reason: never
/// offered, refused (and the person is shown the refusal), or already
/// finished.
#[test]
fn a_dropped_frame_says_which_transfer_and_why() {
    let hand = Hand::new("why-unknown");
    let key = id_bytes(id(1)).unwrap();
    let why = |hand: &Hand| hand.manager.probe(move |engine| engine.why_unknown(&key));
    assert_eq!(why(&hand), "never offered");
    // An offer that is refused (over 4 GiB) is listed as failed and remembered.
    hand.send(hand.offer(
        id(1),
        "huge.bin",
        crate::device_links::webrtc::transfer_manager::MAX_TRANSFER_BYTES + 1,
    ));
    eventually("the refusal", || hand.reply(FileAction::Error));
    assert!(failed_with(id(1)).is_some_and(|words| words.contains("4 GiB")));
    assert_eq!(why(&hand), "refused");
    // A finished one.
    let (data, hashes) = three_blocks(51);
    let key2 = id_bytes(id(2)).unwrap();
    send_whole(&hand, id(2), "ok.bin", &data, &hashes);
    eventually("the complete", || hand.reply(FileAction::Complete));
    assert_eq!(
        hand.manager.probe(move |engine| engine.why_unknown(&key2)),
        "already finished"
    );
    // A frame for it now is dropped (and nothing changes).
    for frame in block_frames(id(2), 0, &data, 0, false) {
        hand.manager.on_frame(frame);
    }
    hand.settle();
    assert!(hand.incoming().is_empty());
}
