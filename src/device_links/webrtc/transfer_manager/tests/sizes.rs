//! Files of every size class round-trip: empty, one byte, one frame, one
//! block, a block and a byte, several blocks with a ragged end.

use super::*;

// ── Sizes ──────────────────────────────────────────────────────────────────

fn round_trip(label: &str, length: usize) {
    let rig = Rig::new(label);
    let bytes = payload(length, length as u64);
    let source = rig.source("file.bin", &bytes);
    let id = rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(&got.path).unwrap(), bytes, "{length} bytes");
    assert_eq!(got.path.parent().unwrap(), rig.downloads("b"));
    assert!(got.incoming);
    let back = rig.sent_ok();
    assert_eq!(
        (back.transfer_id.as_str(), back.incoming),
        (id.as_str(), false)
    );
    // Nothing is left behind on either side but the file itself.
    eventually("clean state directories", || {
        (names(&rig.state("a")).is_empty() && names(&rig.state("b")).is_empty()).then_some(())
    });
    assert_eq!(names(&rig.downloads("b")), ["file.bin"]);
}

#[test]
fn an_empty_file_has_no_frames_and_arrives() {
    let rig = Rig::new("empty");
    let source = rig.source("empty.txt", b"");
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), b"");
    rig.sent_ok();
    assert_eq!(
        rig.ab.frames.load(Ordering::SeqCst),
        0,
        "a 0-byte file has no frames"
    );
    let done = rig.ab.sent(FileAction::Done);
    assert_eq!(done[0].blocks, Some(0));
    assert_eq!(
        done[0].file_hash.as_deref(),
        Some(hex(&hash_of_hashes(&[])).as_str())
    );
}

#[test]
fn one_byte() {
    round_trip("one-byte", 1);
}

#[test]
fn exactly_one_frame_of_payload() {
    round_trip("one-frame", 61_368);
}

#[test]
fn exactly_one_mebibyte_is_one_block() {
    round_trip("one-mib", BLOCK_BYTES as usize);
    let rig = Rig::new("one-mib-frames");
    let source = rig.source("f.bin", &payload(BLOCK_BYTES as usize, 5));
    rig.a().start_send(&source).unwrap();
    rig.received();
    // 18 frames of the default size carry one block, the last with the hash.
    assert_eq!(rig.ab.frames.load(Ordering::SeqCst), 18);
    assert_eq!(rig.ba.sent(FileAction::BlockAck).len(), 1);
    assert_eq!(rig.ba.sent(FileAction::Accept)[0].resume_block, Some(0));
}

#[test]
fn one_byte_over_a_block_makes_a_second_block() {
    round_trip("block-plus-one", BLOCK_BYTES as usize + 1);
}

#[test]
fn several_blocks_and_a_ragged_end() {
    round_trip("ragged", 3 * BLOCK_BYTES as usize + 123_457);
}
