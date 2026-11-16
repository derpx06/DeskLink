//! A sender told to wait, one gibibyte, the Bucket's slot, modification
//! times and synced files.

use super::*;

// ── A sender that is told to wait ──────────────────────────────────────────

#[test]
fn a_sender_told_busy_waits_and_offers_again_until_a_slot_frees() {
    let rig = Rig::new("busy-sender");
    // The receiver already holds two transfers from somewhere else.
    for (index, id) in [id(2), id(3)].into_iter().enumerate() {
        let mut offer = FileControl::new(&wire_of("alpha", "beta"), FileAction::Offer, id, "other");
        offer.filename = Some(format!("held-{index}.bin"));
        offer.total_size = Some(10);
        offer.mtime = Some(1_700_000_000_000);
        offer.block_size = Some(BLOCK_BYTES);
        offer.max_frame_bytes = Some(FRAME_BYTES_PROVEN);
        rig.b().on_control(offer);
    }
    eventually("two held", || {
        (rig.b().probe(|engine| engine.incoming_ids()).len() == 2).then_some(())
    });
    let bytes = payload(200_000, 81);
    let source = rig.source("waiting.bin", &bytes);
    let wait = rig.a().start_send(&source).unwrap();
    // Told busy, it offers again (every 100 ms in these tests), never failing.
    eventually("two offers", || {
        (rig.ab.sent(FileAction::Offer).len() >= 3).then_some(())
    });
    assert!(failed_with(&wait).is_none(), "busy is never shown");
    // A slot frees: the next offer is accepted.
    let mut cancel = FileControl::new(
        &wire_of("alpha", "beta"),
        FileAction::Cancel,
        id(2),
        "other",
    );
    cancel.code = Some(code::USER.to_string());
    rig.b().on_control(cancel);
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
}

// ── One gibibyte ───────────────────────────────────────────────────────────

/// Edge-case row "1 GiB file": exactly 2^30 bytes cross intact. Opt-in, on a
/// real disk (`$DESKLINK_BENCH_DIR`, default `~/.cache/desklink-bench`):
/// `cargo test --release -- --ignored one_gibibyte`.
#[test]
#[ignore = "writes 2 GiB; run with --ignored one_gibibyte"]
fn one_gibibyte_crosses_intact() {
    use std::io::{Read, Write};
    let base = std::env::var_os("DESKLINK_BENCH_DIR")
        .map(PathBuf::from)
        .or_else(|| dirs::cache_dir().map(|cache| cache.join("desklink-bench")))
        .unwrap_or_else(std::env::temp_dir)
        .join(format!("one-gib-{}", std::process::id()));
    fs::create_dir_all(&base).unwrap();
    let rig = Rig::with(
        "one-gib",
        fast(),
        fast(),
        crate::device_links::webrtc::transfer_manager::FILE_QUEUE_MAX_BYTES,
    );
    let source = base.join("one-gib.bin");
    let mut file = fs::File::create(&source).unwrap();
    let block = payload(1 << 20, 99);
    for index in 0..1024u32 {
        let mut varied = block.clone();
        varied[..4].copy_from_slice(&index.to_be_bytes());
        file.write_all(&varied).unwrap();
    }
    file.sync_all().unwrap();
    assert_eq!(fs::metadata(&source).unwrap().len(), 1 << 30);
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    let digest = |path: &Path| {
        let mut file = fs::File::open(path).unwrap();
        let mut hasher = openssl::sha::Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        loop {
            let count = file.read(&mut buffer).unwrap();
            if count == 0 {
                break hex(&hasher.finish());
            }
            hasher.update(&buffer[..count]);
        }
    };
    assert_eq!(fs::metadata(&got.path).unwrap().len(), 1 << 30);
    assert_eq!(digest(&got.path), digest(&source));
    assert_eq!(rig.ba.sent(FileAction::BlockAck).len(), 1024);
    let _ = fs::remove_dir_all(base);
}

/// I16: two Bucket files cannot hold both slots; a file the person chose
/// gets the free one at once.
#[test]
fn the_bucket_never_holds_the_last_slot_so_a_shared_file_goes_at_once() {
    use crate::device_links::webrtc::file_protocol::FolderRef;
    let rig = Rig::new("bucket-reserve");
    rig.ab.freeze();
    for n in 0..2u64 {
        let bytes = payload(200_000, 70 + n);
        let source = rig.source(&format!("b{n}.jpg"), &bytes);
        let sha = hex(&openssl::sha::sha256(&bytes));
        let folder = FolderRef {
            folder: "bucket".into(),
            relpath: format!("Res/{n}-{}.jpg", uuid::Uuid::new_v4().simple()),
            mtime: Some(1),
        };
        crate::device_links::folders::test_want("alpha", &folder, &sha, bytes.len() as u64);
        rig.a().start_send_folder(&source, folder, &sha).unwrap();
    }
    let shared = rig.source("shared.bin", &payload(100_000, 5));
    rig.a().start_send(&shared).unwrap();
    eventually("two offers", || {
        (rig.ab.sent(FileAction::Offer).len() >= 2).then_some(())
    });
    std::thread::sleep(Duration::from_millis(150));
    let offers = rig.ab.sent(FileAction::Offer);
    assert_eq!(offers.len(), 2, "the second Bucket file waits");
    assert!(
        offers.iter().any(|offer| offer.folder.is_none()),
        "the shared file was offered"
    );
    rig.ab.thaw();
}

/// S4: a received file keeps the sender's modification time.
#[test]
fn a_received_file_keeps_the_senders_modification_time() {
    let rig = Rig::new("mtime");
    let source = rig.source("old.bin", &payload(10_000, 77));
    let year_ago = std::time::SystemTime::now() - Duration::from_secs(365 * 24 * 3600);
    std::fs::OpenOptions::new()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(year_ago)
        .unwrap();
    rig.a().start_send(&source).unwrap();
    let got = rig.received();
    let modified = fs::metadata(got.path).unwrap().modified().unwrap();
    let difference = modified
        .duration_since(year_ago)
        .unwrap_or_else(|error| error.duration());
    assert!(difference < Duration::from_secs(2), "{difference:?}");
}

#[test]
fn a_synced_file_is_offered_with_its_pair_path_and_hash_and_taken_when_asked_for() {
    use crate::device_links::webrtc::file_protocol::FolderRef;
    let rig = Rig::new("bucket-send");
    let bytes = payload(150_000, 91);
    let relpath = format!("Trips/{}.jpg", uuid::Uuid::new_v4().simple());
    let source = rig.source("synced.jpg", &bytes);
    let sha = hex(&openssl::sha::sha256(&bytes));
    let folder = FolderRef {
        folder: "bucket".into(),
        relpath: relpath.clone(),
        mtime: Some(1),
    };
    // The receiver asked for exactly this file.
    crate::device_links::folders::test_want("alpha", &folder, &sha, bytes.len() as u64);
    let id = rig.a().start_send_folder(&source, folder, &sha).unwrap();
    let got = rig.received();
    // It went to folder sync (no worker here, so the stash is dropped), not to
    // the downloads.
    assert!(names(&rig.downloads("b")).is_empty());
    assert!(got.transfer_id == id);
    let offer = rig.ab.sent(FileAction::Offer).remove(0);
    assert_eq!(offer.folder.as_deref(), Some("bucket"));
    assert_eq!(offer.relpath.as_deref(), Some(relpath.as_str()));
    assert_eq!(offer.content_sha256.as_deref(), Some(sha.as_str()));
    assert_eq!(offer.filename.as_deref(), relpath.rsplit('/').next());
    // A hash that is not a hash is refused before anything is offered.
    let again = FolderRef {
        folder: "bucket".into(),
        relpath: "x.jpg".into(),
        mtime: None,
    };
    assert!(rig.a().start_send_folder(&source, again, "nothex").is_err());
}
