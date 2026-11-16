//! Bucket resume is keyed by content, not by transfer id.

use super::*;

// ── Bucket resume is keyed by content, not by transfer id ──────────────────

/// A Bucket file of `blocks` blocks (and a bit), asked for by `hand`'s
/// manager; the offer for it under `id` and `token`.
fn bucket_offer(
    hand: &Hand,
    id: &str,
    token: &str,
    relpath: &str,
    sha: &str,
    total: u64,
) -> FileControl {
    let mut offer = hand.offer(id, relpath.rsplit('/').next().unwrap(), total);
    offer.transfer_token = token.to_string();
    offer.folder = Some("bucket".into());
    offer.relpath = Some(relpath.to_string());
    offer.content_sha256 = Some(sha.to_string());
    offer
}

fn want_bucket_file(relpath: &str, sha: &str, total: u64) {
    use crate::device_links::webrtc::file_protocol::FolderRef;
    crate::device_links::folders::test_want(
        "alpha",
        &FolderRef {
            folder: "bucket".into(),
            relpath: relpath.to_string(),
            mtime: None,
        },
        sha,
        total,
    );
}

/// A cut at 30% of a Bucket file: asked for again under a new id, the
/// receiver carries on from the verified blocks it holds (the whole 30%),
/// whether it noticed the cut (the old id is closed and adopted) or not (the
/// files are on disk). The rest arrives and the file completes.
#[test]
fn a_bucket_file_cut_at_thirty_percent_resumes_under_a_new_id_with_nothing_redone() {
    for cut_noticed in [true, false] {
        let hand = Hand::new("bucket-resume");
        let relpath = format!("movies/{}.bin", uuid::Uuid::new_v4().simple());
        let blocks = 30u64;
        let total = blocks * BLOCK_BYTES + 5;
        let data = payload(total as usize, 61);
        let hashes: Vec<[u8; 32]> = data
            .chunks(BLOCK_BYTES as usize)
            .map(openssl::sha::sha256)
            .collect();
        let sha = hex(&openssl::sha::sha256(&data));
        want_bucket_file(&relpath, &sha, total);
        hand.send(bucket_offer(&hand, id(1), "token0", &relpath, &sha, total));
        eventually("the accept", || hand.reply(FileAction::Accept));
        let cut_at = 9; // 30% of 30 blocks
        for index in 0..cut_at {
            for frame in block_frames(id(1), 0, &data, index, false) {
                hand.manager.on_frame(frame);
            }
        }
        eventually("the acks", || {
            (hand.link.sent(FileAction::BlockAck).len() as u64 == cut_at).then_some(())
        });
        if cut_noticed {
            // The link drops: the receiver checkpoints what it holds.
            hand.manager.detach();
            hand.settle();
            hand.manager
                .attach(wire_of("beta", "alpha"), hand.link.clone());
        }
        // Asked for again, as the Bucket does after a rebuild: a new id.
        hand.send(bucket_offer(&hand, id(2), "token1", &relpath, &sha, total));
        let accept = eventually("the new accept", || {
            hand.link
                .sent(FileAction::Accept)
                .into_iter()
                .find(|accept| accept.transfer_id == id(2))
        });
        let resume = accept.resume_block.expect("a resume block");
        let redone = cut_at * BLOCK_BYTES - resume * BLOCK_BYTES;
        assert!(
            redone < BLOCK_BYTES,
            "cut noticed: {cut_noticed}; resumed at block {resume} of the {cut_at} held, {redone} bytes redone"
        );
        assert_eq!(
            accept.prefix_sha256.as_deref(),
            Some(hex(&hash_of_hashes(&hashes[..resume as usize])).as_str())
        );
        // The rest, from there, completes the file under the new id.
        for index in resume..blocks + 1 {
            for frame in block_frames(id(2), 0, &data, index, false) {
                hand.manager.on_frame(frame);
            }
        }
        let mut done =
            FileControl::new(&wire_of("alpha", "beta"), FileAction::Done, id(2), "token1");
        done.file_hash = Some(hex(&hash_of_hashes(&hashes)));
        done.blocks = Some(blocks + 1);
        done.total_size = Some(total);
        hand.send(done);
        eventually("the complete", || {
            hand.link
                .sent(FileAction::Complete)
                .into_iter()
                .find(|complete| complete.transfer_id == id(2))
        });
        assert!(
            names(&hand.base.join("state"))
                .iter()
                .all(|name| !name.contains(id(1))),
            "nothing of the old id is left"
        );
    }
}

/// The same path with different content is a different file: nothing of the
/// old partial is reused, and it is dropped.
#[test]
fn a_bucket_file_with_other_content_does_not_resume_from_an_old_partial() {
    let hand = Hand::new("bucket-other");
    let relpath = format!("docs/{}.bin", uuid::Uuid::new_v4().simple());
    let total = 4 * BLOCK_BYTES;
    let old = payload(total as usize, 71);
    let old_sha = hex(&openssl::sha::sha256(&old));
    want_bucket_file(&relpath, &old_sha, total);
    hand.send(bucket_offer(
        &hand,
        id(1),
        "token0",
        &relpath,
        &old_sha,
        total,
    ));
    eventually("the accept", || hand.reply(FileAction::Accept));
    for index in 0..2 {
        for frame in block_frames(id(1), 0, &old, index, false) {
            hand.manager.on_frame(frame);
        }
    }
    eventually("the acks", || {
        (hand.link.sent(FileAction::BlockAck).len() == 2).then_some(())
    });
    hand.manager.detach();
    hand.settle();
    hand.manager
        .attach(wire_of("beta", "alpha"), hand.link.clone());
    // The file changed on the other side: another hash, the same path.
    let new = payload(total as usize, 72);
    let new_sha = hex(&openssl::sha::sha256(&new));
    want_bucket_file(&relpath, &new_sha, total);
    hand.send(bucket_offer(
        &hand,
        id(2),
        "token1",
        &relpath,
        &new_sha,
        total,
    ));
    let accept = eventually("the new accept", || {
        hand.link
            .sent(FileAction::Accept)
            .into_iter()
            .find(|accept| accept.transfer_id == id(2))
    });
    assert_eq!(
        accept.resume_block,
        Some(0),
        "a different file starts again"
    );
    assert!(
        names(&hand.base.join("state"))
            .iter()
            .all(|name| !name.contains(id(1))),
        "the old partial is gone"
    );
}

/// The sending half of the rule: a synced file asked for again after a cut
/// is the same transfer, whether the connection already resumed it (the
/// request is absorbed) or not (the kept checkpoint is offered again under
/// its id). Cut at 30%, less than a block is sent twice.
#[test]
fn a_synced_file_asked_for_again_after_a_cut_goes_on_from_the_last_block_under_its_id() {
    use crate::device_links::webrtc::file_protocol::FolderRef;
    for resumed_first in [false, true] {
        let mut rig = Rig::new("bucket-send-resume");
        let blocks = 30usize;
        let bytes = payload(blocks * BLOCK_BYTES as usize + 321, 81);
        let relpath = format!("Trips/{}.mov", uuid::Uuid::new_v4().simple());
        let source = rig.source("big.mov", &bytes);
        let sha = hex(&openssl::sha::sha256(&bytes));
        let folder = FolderRef {
            folder: "bucket".into(),
            relpath: relpath.clone(),
            mtime: Some(1),
        };
        crate::device_links::folders::test_want("alpha", &folder, &sha, bytes.len() as u64);
        pull_after_blocks(&rig, 9);
        let id = rig
            .a()
            .start_send_folder(&source, folder.clone(), &sha)
            .unwrap();
        eventually("the link to be pulled", || {
            (!rig.ab.up.load(Ordering::SeqCst)).then_some(())
        });
        let held = rig.b().probe(|engine| {
            engine
                .incoming
                .values()
                .map(|incoming| incoming.verified_blocks())
                .max()
                .unwrap_or(0)
        });
        assert!(held >= 8, "{held} blocks held");
        rig.stop(false);
        rig.connect();
        if resumed_first {
            rig.a().resume_sends();
        }
        // The Bucket asks for the file again: a request with a new id.
        let again = rig
            .a()
            .start_send_folder(&source, folder.clone(), &sha)
            .unwrap();
        assert_ne!(again, id, "a new request, a new id of its own");
        // It completes (to folder sync, not the downloads: no worker here).
        let got = rig.received();
        assert_eq!(got.transfer_id, id);
        let offers = rig.ab.sent(FileAction::Offer);
        assert!(
            offers.iter().all(|offer| offer.transfer_id == id),
            "resumed_first {resumed_first}: only ever offered as {id}: {:?}",
            offers
                .iter()
                .map(|o| o.transfer_id.clone())
                .collect::<Vec<_>>()
        );
        let accept = rig.ba.sent(FileAction::Accept).remove(0);
        let resume = accept.resume_block.expect("a resume block");
        assert!(
            resume + 1 >= held,
            "resumed at {resume}, {held} blocks were held"
        );
        let resent = rig.ab.frames.load(Ordering::SeqCst);
        assert!(
            resent < (blocks - held as usize + 3) * 18,
            "{resent} frames after the reconnect (resumed first: {resumed_first})"
        );
    }
}
