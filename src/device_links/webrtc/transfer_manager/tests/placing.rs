//! "Complete" means placed: placing across filesystems, taken names, missing
//! folders, hostile names and links, and stage-file sweeping.

use super::*;

// ── "complete" means placed: the glue, end to end ──────────────────────────

/// A real receive through the manager, the store, the board and the
/// history: three blocks, the bytes checked where they landed (before the
/// sender is told "complete"), then the board and the history row.
#[test]
fn a_received_file_is_complete_only_once_it_is_placed_recorded_and_listed() {
    let hand = Hand::new("glue-ok");
    let (data, hashes) = three_blocks(21);
    send_whole(&hand, id(1), "three.bin", &data, &hashes);
    let complete = eventually("the complete", || hand.reply(FileAction::Complete));
    // At the moment the sender is told, the file is whole where it belongs.
    let placed = hand.base.join("dl").join("three.bin");
    assert_eq!(fs::read(&placed).unwrap(), data, "bytes at the destination");
    assert_eq!(
        complete.file_hash.as_deref(),
        Some(hex(&hash_of_hashes(&hashes)).as_str())
    );
    // Listed on the board as received at that path, and in the history. The board and the
    // history are written by the engine just after it tells the sender (the file itself is
    // placed before), so they are waited for, not read in the same instant.
    eventually("the board and the history row", || {
        let listed = outcomes(id(1))
            .iter()
            .any(|outcome| matches!(outcome, board::Outcome::Received(path) if *path == placed));
        let recorded = crate::device_links::transfer_log::recent()
            .iter()
            .any(|entry| entry.path == placed && entry.incoming && entry.device_id == "alpha");
        (listed && recorded).then_some(())
    });
    assert!(
        names(&hand.base.join("state")).is_empty(),
        "no partial left"
    );
    assert!(hand.link.sent(FileAction::Error).is_empty());
}

/// A destination that refuses the file: the sender gets `error` and never
/// `complete`, nothing is listed as received, the verified part stays, and
/// the same transfer offered again only has to be placed.
#[test]
fn a_file_that_cannot_be_placed_is_an_error_never_a_complete_and_keeps_its_part() {
    use std::os::unix::fs::PermissionsExt;
    // Root writes anywhere: the refusal is staged as `nobody` instead.
    if !crate::test_privileges::unprivileged(
        "device_links::webrtc::transfer_manager::tests::placing::a_file_that_cannot_be_placed_is_an_error_never_a_complete_and_keeps_its_part",
    ) {
        return;
    }
    let hand = Hand::new("glue-refused");
    let (data, hashes) = three_blocks(22);
    let downloads = hand.base.join("dl");
    fs::set_permissions(&downloads, fs::Permissions::from_mode(0o500)).unwrap();
    send_whole(&hand, id(1), "refused.bin", &data, &hashes);
    let error = eventually("the error", || hand.reply(FileAction::Error));
    assert_eq!(error.code.as_deref(), Some(code::INTERNAL));
    assert!(
        error
            .message
            .as_deref()
            .is_some_and(|message| message.contains("not allowed")),
        "{error:?}"
    );
    assert!(
        hand.link.sent(FileAction::Complete).is_empty(),
        "never complete"
    );
    assert!(names(&downloads).is_empty(), "nothing published");
    let failed = eventually("the failure", || failed_with(id(1)));
    assert!(failed.contains("not allowed"), "{failed}");
    assert!(
        !outcomes(id(1))
            .iter()
            .any(|outcome| matches!(outcome, board::Outcome::Received(_))),
        "not listed as received"
    );
    // The verified part and its checkpoint are kept.
    assert!(
        !names(&hand.base.join("state")).is_empty(),
        "the part is kept"
    );
    // The folder is allowed again; the same transfer, offered again, resumes
    // at the end and only has to be placed.
    fs::set_permissions(&downloads, fs::Permissions::from_mode(0o700)).unwrap();
    hand.send(hand.offer(id(1), "refused.bin", data.len() as u64));
    eventually("accepted again", || {
        (hand.link.sent(FileAction::Accept).len() == 2).then_some(())
    });
    say_done(&hand, id(1), data.len() as u64, &hashes);
    eventually("the complete", || hand.reply(FileAction::Complete));
    assert_eq!(fs::read(downloads.join("refused.bin")).unwrap(), data);
}

/// The downloads folder is on another filesystem (the link fails with
/// EXDEV), a name is taken, or the folder is gone: the file is still placed
/// whole, never over anything, and the part goes only after.
#[test]
fn placing_copes_with_another_filesystem_a_taken_name_and_a_missing_folder() {
    use crate::device_links::webrtc::transfer_manager::store::Store;
    let base =
        std::env::temp_dir().join(format!("desklink-place-{}", uuid::Uuid::new_v4().simple()));
    let (state, downloads) = (base.join("state"), base.join("dl"));
    let store = Store::new(state, downloads.clone()).unwrap();
    let exdev = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EXDEV));
    let write_part = |id: &str, bytes: &[u8]| {
        fs::write(store.part_path(id).unwrap(), bytes).unwrap();
    };
    // Another filesystem: copied, synced, put in place; no staging file left.
    write_part("p1", b"first file");
    let path = store.publish_with("p1", "a.bin", "a.bin", &exdev).unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"first file");
    assert_eq!(names(&downloads), ["a.bin"], "no staging file left behind");
    assert!(!store.part_path("p1").unwrap().exists(), "part goes after");
    // A delivered file is 0666 & !umask whichever way it got there.
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode(&path),
            crate::device_links::webrtc::transfer_manager::store::download_mode(),
            "copy fallback"
        );
        write_part("p1b", b"linked");
        let linked = store
            .publish_with("p1b", "linked.bin", "linked.bin", &|from, to| {
                fs::hard_link(from, to)
            })
            .unwrap();
        assert_eq!(
            mode(&linked),
            crate::device_links::webrtc::transfer_manager::store::download_mode(),
            "hard link"
        );
    }
    // The same name again: the next free one, the first untouched.
    write_part("p2", b"second file");
    let again = store.publish_with("p2", "a.bin", "a.bin", &exdev).unwrap();
    assert_ne!(again, path);
    assert_eq!(fs::read(&path).unwrap(), b"first file");
    assert_eq!(fs::read(&again).unwrap(), b"second file");
    // The folder vanished while the file arrived: made again.
    fs::remove_dir_all(&downloads).unwrap();
    write_part("p3", b"third file");
    let third = store.publish("p3", "c.bin", "c.bin").unwrap();
    assert_eq!(fs::read(third).unwrap(), b"third file");
    // A refusal for any other reason keeps the part and says why in words.
    write_part("p4", b"fourth file");
    let nospace = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::ENOSPC));
    let error = store
        .publish_with("p4", "d.bin", "d.bin", &nospace)
        .unwrap_err();
    assert!(error.contains("no room"), "{error}");
    assert!(store.part_path("p4").unwrap().exists(), "the part is kept");
    let denied = |_: &Path, _: &Path| Err(std::io::Error::from_raw_os_error(libc::EACCES));
    let error = store
        .publish_with("p4", "d.bin", "d.bin", &denied)
        .unwrap_err();
    assert!(error.contains("not allowed"), "{error}");
    let _ = fs::remove_dir_all(&base);
}

/// The sender: after its last block is acknowledged it says "Finishing…",
/// not "sent"; "sent" waits for the receiver's `complete`. An `error` in
/// place of it is a failure that can be tried again.
#[test]
fn a_send_is_finishing_until_the_receiver_says_complete_and_an_error_then_is_a_failure() {
    let rig = Rig::new("finishing");
    let (data, _) = three_blocks(23);
    let source = rig.source("fin.bin", &data);
    // The receiver never hears `done`: it has not placed anything, so the
    // sender must not call the file sent.
    *rig.ab.control_hook.lock().unwrap() = Some(Box::new(|control| {
        if control.action == FileAction::Done {
            Fate::Drop
        } else {
            Fate::Deliver
        }
    }));
    let id = rig.a().start_send(&source).unwrap();
    eventually("done sent", || {
        (!rig.ab.sent(FileAction::Done).is_empty()).then_some(())
    });
    eventually("finishing", || {
        (board::note(&id).as_deref() == Some("Finishing…")).then_some(())
    });
    // Every block was acknowledged, but the sender has not been told the
    // file was placed: it is not "sent", not in its history, and nobody was
    // notified.
    assert!(!was_sent(&id));
    assert!(rig.done_a.try_recv().is_err(), "no completion announced");
    assert!(board::rows().iter().any(|row| row.id == id), "still listed");
    // The receiver's word is an error: a failure, with the file to retry.
    let mut error = FileControl::new(
        &wire_of("beta", "alpha"),
        FileAction::Error,
        &id,
        &rig.ab.sent(FileAction::Offer)[0].transfer_token,
    );
    error.code = Some(code::INTERNAL.to_string());
    error.message = Some("There is no room to save the download.".to_string());
    rig.a().on_control(error);
    let reason = eventually("the failure", || failed_with(&id));
    assert!(reason.contains("no room"), "{reason}");
    let finished = board::peek_finished(&id)
        .into_iter()
        .find(|item| matches!(item.outcome, board::Outcome::Failed(_)))
        .expect("the sender's failure");
    assert_eq!(
        finished.retry.as_deref(),
        Some(source.as_path()),
        "try again"
    );
    assert!(!was_sent(&id));
}

// ── Placing across filesystems, hostile names, links ───────────────────────

fn exdev(_: &Path, _: &Path) -> std::io::Result<()> {
    Err(std::io::Error::from_raw_os_error(libc::EXDEV))
}

/// A stall never replaces "Finishing…", and ending one puts it back.
#[test]
fn a_stall_note_never_overwrites_finishing() {
    use crate::device_links::webrtc::transfer_manager::{stall_note, StallChange};
    assert_eq!(stall_note(&StallChange::Began, true), None);
    assert_eq!(
        stall_note(&StallChange::Ended, true),
        Some(Some(words::finishing()))
    );
    assert_eq!(
        stall_note(&StallChange::Began, false),
        Some(Some(words::no_progress()))
    );
    assert_eq!(stall_note(&StallChange::Ended, false), Some(None));
    assert_eq!(stall_note(&StallChange::None, true), None);
}

/// Two receives of the same name at once, on a downloads folder the link
/// cannot cross into: both succeed (one as the next free name), each file
/// whole, and nothing is left staged.
#[test]
fn two_same_name_receives_on_another_filesystem_both_succeed() {
    use crate::device_links::webrtc::transfer_manager::store::Store;
    let base = scratch("stage-race");
    let downloads = base.join("dl");
    let store = Arc::new(Store::new(base.join("state"), downloads.clone()).unwrap());
    let (one, two) = (payload(6 << 20, 31), payload(6 << 20, 32));
    for (id, bytes) in [("race-a", &one), ("race-b", &two)] {
        fs::write(store.part_path(id).unwrap(), bytes).unwrap();
    }
    let gate = Arc::new(std::sync::Barrier::new(2));
    let runs: Vec<_> = ["race-a", "race-b"]
        .into_iter()
        .map(|id| {
            let (store, gate) = (Arc::clone(&store), Arc::clone(&gate));
            std::thread::spawn(move || {
                gate.wait();
                store.publish_with(id, "same.bin", "same.bin", &exdev)
            })
        })
        .collect();
    let paths: Vec<PathBuf> = runs
        .into_iter()
        .map(|run| run.join().unwrap().expect("both are placed"))
        .collect();
    assert_ne!(paths[0], paths[1]);
    assert_eq!(fs::read(&paths[0]).unwrap(), one);
    assert_eq!(fs::read(&paths[1]).unwrap(), two);
    assert_eq!(names(&downloads).len(), 2, "no staging file left");
    let _ = fs::remove_dir_all(&base);
}

/// A name of 80 CJK letters (240 bytes) is a valid name, and its staging file
/// is short: it can be received across filesystems.
#[test]
fn a_long_cjk_name_can_be_placed_across_filesystems() {
    use crate::device_links::webrtc::transfer_manager::store::Store;
    let base = scratch("stage-cjk");
    let downloads = base.join("dl");
    let store = Store::new(base.join("state"), downloads.clone()).unwrap();
    let name = format!("{}.bin", "界".repeat(80));
    assert!(name.len() <= 255);
    fs::write(store.part_path("cjk").unwrap(), b"cjk bytes").unwrap();
    let path = store.publish_with("cjk", &name, &name, &exdev).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"cjk bytes");
    assert_eq!(names(&downloads), [name]);
    let _ = fs::remove_dir_all(&base);
}

/// A link left at the name (pointing nowhere) is never written through or
/// replaced: the file goes to the next free name, by either way of placing.
#[test]
fn a_dangling_symlink_at_the_name_is_never_replaced() {
    use crate::device_links::webrtc::transfer_manager::store::Store;
    let base = scratch("stage-dangling");
    let downloads = base.join("dl");
    let store = Store::new(base.join("state"), downloads.clone()).unwrap();
    let trap = downloads.join("trap.bin");
    std::os::unix::fs::symlink("/nonexistent/desklink-nowhere", &trap).unwrap();
    for (id, link) in [("d1", true), ("d2", false)] {
        fs::write(store.part_path(id).unwrap(), id.as_bytes()).unwrap();
        let path = if link {
            store.publish(id, "trap.bin", "trap.bin").unwrap()
        } else {
            store
                .publish_with(id, "trap.bin", "trap.bin", &exdev)
                .unwrap()
        };
        assert_ne!(path, trap, "{id}");
        assert_eq!(fs::read(&path).unwrap(), id.as_bytes());
        assert_eq!(
            fs::read_link(&trap).unwrap(),
            Path::new("/nonexistent/desklink-nowhere"),
            "the link is untouched"
        );
    }
    let _ = fs::remove_dir_all(&base);
}

/// A staging file a dead process left is swept once it is old; a fresh one,
/// and anything that is not exactly a staging file, stays.
#[test]
fn an_old_stage_file_is_swept_and_a_fresh_one_is_kept() {
    use crate::device_links::webrtc::transfer_manager::store::Store;
    let base = scratch("stage-sweep");
    let downloads = base.join("dl");
    fs::create_dir_all(&downloads).unwrap();
    let old = downloads.join(".desklink-dead.stage");
    let fresh = downloads.join(".desklink-live.stage");
    fs::write(&old, b"x").unwrap();
    fs::write(&fresh, b"y").unwrap();
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(std::time::SystemTime::now() - Duration::from_secs(8 * 24 * 3600))
        .unwrap();
    // Neither a regular file nor a stage name: untouched, however old.
    let lookalike_dir = downloads.join(".desklink-dir.stage");
    fs::create_dir(&lookalike_dir).unwrap();
    let other = downloads.join(".desklink-notes.txt");
    fs::write(&other, b"z").unwrap();
    let link = downloads.join(".desklink-link.stage");
    std::os::unix::fs::symlink(&fresh, &link).unwrap();
    let _store = Store::new(base.join("state"), downloads.clone()).unwrap();
    assert!(!old.exists(), "swept");
    assert!(fresh.exists() && lookalike_dir.exists() && other.exists());
    assert!(fs::symlink_metadata(&link).is_ok(), "a link is not touched");
    let _ = fs::remove_dir_all(&base);
}

/// `~/Downloads` as a link (to another disk, say): its target is used.
#[test]
fn a_downloads_folder_that_is_a_symlink_works() {
    use crate::device_links::webrtc::transfer_manager::store::Store;
    let base = scratch("stage-linked");
    let real = base.join("elsewhere");
    fs::create_dir_all(&real).unwrap();
    let linked = base.join("Downloads");
    std::os::unix::fs::symlink(&real, &linked).unwrap();
    let store = Store::new(base.join("state"), linked).unwrap();
    fs::write(store.part_path("l1").unwrap(), b"through the link").unwrap();
    let path = store.publish("l1", "l.bin", "l.bin").unwrap();
    assert_eq!(fs::read(real.join("l.bin")).unwrap(), b"through the link");
    assert_eq!(path.parent().unwrap(), fs::canonicalize(&real).unwrap());
    let _ = fs::remove_dir_all(&base);
}
