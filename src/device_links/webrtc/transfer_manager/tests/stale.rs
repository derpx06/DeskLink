//! Stale frames around a restart or a lane switch.

use super::*;

// ── Stale frames around a restart or a lane switch ─────────────────────────

const STALE_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

/// A receiver with blocks 0 and 1 verified, and part of block 2 in.
fn hand_part_way(label: &str) -> (Hand, Vec<u8>, Vec<Vec<Vec<u8>>>) {
    let hand = Hand::new(label);
    let file = payload(4 * BLOCK_BYTES as usize, 41);
    hand.send(hand.offer(STALE_ID, "stale.bin", file.len() as u64));
    let blocks: Vec<Vec<Vec<u8>>> = (0..4)
        .map(|index| block_frames(STALE_ID, 0, &file, index, false))
        .collect();
    for block in &blocks[..2] {
        for frame in block {
            hand.manager.on_frame(frame.clone());
        }
    }
    for frame in &blocks[2][..5] {
        hand.manager.on_frame(frame.clone());
    }
    hand.settle();
    (hand, file, blocks)
}

fn verified_of(hand: &Hand) -> Option<u64> {
    hand.manager.probe(|engine| {
        engine
            .incoming
            .values()
            .next()
            .map(|incoming| incoming.verified_blocks())
    })
}

/// A promotion re-offers while the old stream is still in flight on the data
/// channel (a different SCTP stream from the control channel): its frames
/// reach the receiver after the new accept, and must change nothing.
#[test]
fn stale_frames_of_the_old_stream_after_a_re_offer_are_dropped_not_fatal() {
    let (hand, file, blocks) = hand_part_way("stale-reoffer");
    assert_eq!(verified_of(&hand), Some(2));
    hand.send(hand.offer(STALE_ID, "stale.bin", file.len() as u64));
    // The rest of the old stream, mid-block, then block 3 whole: all of it
    // stale (the sender has not answered the accept yet).
    for frame in &blocks[2][5..] {
        hand.manager.on_frame(frame.clone());
    }
    for frame in &blocks[3] {
        hand.manager.on_frame(frame.clone());
    }
    hand.settle();
    assert!(
        hand.reply(FileAction::Error).is_none(),
        "no error to the sender"
    );
    assert_eq!(hand.incoming().len(), 1, "the transfer lives");
    assert_eq!(verified_of(&hand), Some(2));
    // The new stream, under the next pass, from the receiver's own prefix.
    for index in [2, 3] {
        for frame in block_frames(STALE_ID, 1, &file, index, false) {
            hand.manager.on_frame(frame);
        }
    }
    hand.settle();
    assert_eq!(verified_of(&hand), Some(4));
    assert!(hand.reply(FileAction::Error).is_none());
}

/// The reverse: after a failover to the data channel, frames of the lane that
/// were already on the wire arrive under the old pass and are dropped.
#[test]
fn stale_lane_frames_after_a_failover_are_dropped() {
    let (hand, file, blocks) = hand_part_way("stale-lane");
    let mut switch = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Lane,
        STALE_ID,
        "token0",
    );
    switch.lane =
        Some(crate::device_links::webrtc::file_protocol::LaneField::switch("webrtc", 2, 1));
    hand.send(switch);
    for frame in &blocks[2][5..] {
        hand.manager.on_frame(frame.clone());
    }
    for frame in &blocks[3] {
        hand.manager.on_frame(frame.clone());
    }
    hand.settle();
    assert!(hand.reply(FileAction::Error).is_none());
    assert_eq!(
        verified_of(&hand),
        Some(2),
        "nothing of the old pass counted"
    );
    for index in [2, 3] {
        for frame in block_frames(STALE_ID, 1, &file, index, false) {
            hand.manager.on_frame(frame);
        }
    }
    hand.settle();
    assert_eq!(verified_of(&hand), Some(4));
}

/// A failover whose start is below what the receiver verified (acknowledgements
/// still on their way) keeps the verified blocks and says where it is.
#[test]
fn a_failover_below_the_receivers_verified_blocks_keeps_them_and_says_where_it_is() {
    let (hand, _file, _blocks) = hand_part_way("lane-keeps");
    let mut switch = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Lane,
        STALE_ID,
        "token0",
    );
    switch.lane =
        Some(crate::device_links::webrtc::file_protocol::LaneField::switch("webrtc", 0, 1));
    hand.send(switch);
    hand.settle();
    assert_eq!(
        verified_of(&hand),
        Some(2),
        "nothing verified is thrown away"
    );
    let said = hand.link.sent(FileAction::BlockNack);
    assert_eq!(said.len(), 1);
    assert_eq!(
        (said[0].block_index, said[0].pass, said[0].reason.as_deref()),
        (Some(2), Some(1), Some("lane"))
    );
}

/// The receiver's word about a lane: a look at a lane changes nothing; a lane
/// that opened is the one that can end it; a late word about another does not.
#[test]
fn only_the_lane_that_is_up_can_end_it_and_a_look_changes_nothing() {
    let hand = Hand::new("lane-identity");
    hand.send(hand.offer(STALE_ID, "lane.bin", 4 * BLOCK_BYTES));
    let body = crate::device_links::webrtc::file_lane::TokenBody {
        transfer_id: STALE_ID.into(),
        token: String::new(),
        pass: 0,
        start_block: 0,
    };
    let (checked, up, late, current) = hand.manager.probe(move |engine| {
        let key = id_bytes(STALE_ID).unwrap();
        engine.incoming.get_mut(&key).unwrap().lane_wanted = true;
        let checked = engine.lane_check(&body);
        let after_check = engine.incoming[&key].on_lane;
        engine.lane_opened(&body, 5);
        let up = engine.incoming[&key].on_lane;
        // The lane before this one, ending late.
        engine.lane_lost(STALE_ID, 4);
        let late = engine.incoming[&key].on_lane;
        engine.lane_lost(STALE_ID, 5);
        let current = engine.incoming[&key].on_lane;
        (checked, (after_check, up), late, current)
    });
    assert_eq!(checked, Ok(()));
    assert_eq!(up, (false, true), "opened by the open, not by the look");
    assert!(late, "an older lane's end changes nothing");
    assert!(!current, "the current lane's end does");
    let nacks = hand.link.sent(FileAction::BlockNack);
    assert_eq!(nacks.len(), 1, "told once");
    assert_eq!(nacks[0].reason.as_deref(), Some("lane"));
}

/// Asking a link for its path can wait on webrtcbin: a small file (a Bucket
/// sync of many) never does.
#[test]
fn small_transfers_never_ask_the_link_for_its_path() {
    let rig = Rig::new("path-small");
    let source = rig.source("small.bin", &payload(2 * BLOCK_BYTES as usize, 51));
    rig.a().start_send(&source).unwrap();
    rig.received();
    rig.sent_ok();
    assert_eq!(rig.ab.relayed_asked.load(Ordering::SeqCst), 0);
    assert_eq!(rig.ba.relayed_asked.load(Ordering::SeqCst), 0);
    let big = Rig::new("path-big");
    let source = big.source("big.bin", &payload(9 * BLOCK_BYTES as usize, 52));
    big.a().start_send(&source).unwrap();
    big.received();
    assert!(big.ab.relayed_asked.load(Ordering::SeqCst) > 0);
}
