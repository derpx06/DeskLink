//! A full disk, a receiver far behind, a queue overflow and a silent link.

use super::*;

// ── Disk, backlog, stalls ──────────────────────────────────────────────────

#[test]
fn a_full_disk_pauses_with_storage_full_and_the_transfer_resumes_when_space_frees() {
    let rig = Rig::new("disk-full");
    let bytes = payload(3 * BLOCK_BYTES as usize, 41);
    let source = rig.source("f.bin", &bytes);
    rig.b().probe(|engine| engine.faults.disk_full = true);
    let id = rig.a().start_send(&source).unwrap();
    let pause = eventually("the pause", || {
        rig.ba.sent(FileAction::Pause).into_iter().next()
    });
    assert_eq!(pause.reason.as_deref(), Some("storage-full"));
    // The person is told, in words, on the receiving side...
    eventually("the note", || {
        board::notes_shown(&id)
            .contains(&words::storage_full())
            .then_some(())
    });
    // ...and the sender shows why it waits. (Both sides' notes land on the
    // one board of this process, each replacing the other's: what was shown,
    // not what shows at the moment of a poll.)
    eventually("the sender's note", || {
        board::notes_shown(&id)
            .iter()
            .any(|note| note.contains("no space"))
            .then_some(())
    });
    steady(
        "nothing is published while the disk is full",
        Duration::from_millis(250),
        || names(&rig.downloads("b")).is_empty() && failed_with(&id).is_none(),
    );
    rig.b().probe(|engine| {
        engine.faults.disk_full = false;
        engine.faults.free_bytes = Some(1 << 40);
    });
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    assert!(
        !rig.ba.sent(FileAction::Resume).is_empty(),
        "the receiver restarted the sender at its verified block"
    );
}

#[test]
fn a_receiver_far_behind_asks_the_sender_to_wait_and_then_restarts_it() {
    let rig = Rig::new("backlog");
    let bytes = payload(4 * BLOCK_BYTES as usize, 42);
    let source = rig.source("f.bin", &bytes);
    // The receiver's answers are held, so the sender sends no data frame
    // while the queue counter is set by hand below (a frame in flight
    // decremented a counter that had been overwritten, and wrapped it).
    rig.ba.freeze();
    let id = rig.a().start_send(&source).unwrap();
    eventually("the transfer to be open", || {
        rig.b()
            .probe(|engine| engine.incoming_ids())
            .first()
            .cloned()
    });
    // The queue of unhandled frames is long (made so by hand)...
    let limit = rig.b().probe(|engine| {
        engine
            .shared
            .queued
            .store(engine.tun.queue_pause, Ordering::SeqCst);
        engine.apply_backlog();
        engine.tun.queue_pause
    });
    assert!(limit > 0);
    let pause = rig.ba.sent(FileAction::Pause);
    assert_eq!(
        pause.last().and_then(|p| p.reason.as_deref()),
        Some("backlog")
    );
    // ...and has drained: the sender begins again at the last verified block.
    rig.b().probe(|engine| {
        engine.shared.queued.store(0, Ordering::SeqCst);
        engine.apply_backlog();
    });
    rig.ba.thaw();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    assert!(failed_with(&id).is_none(), "paused, never failed");
    let resume = rig.ba.sent(FileAction::Resume).remove(0);
    assert_eq!(resume.pass, Some(1));
}

#[test]
fn frames_that_overflow_the_queue_restart_the_transfer_instead_of_being_lost() {
    // A queue of two frames' worth, and a receiver thread held up.
    let rig = Rig::with("overflow", fast(), fast(), 2 * 61_440);
    let bytes = payload(3 * BLOCK_BYTES as usize, 43);
    let source = rig.source("f.bin", &bytes);
    // The accept is held back until the receiver's thread is held up.
    rig.ba.freeze();
    rig.a().start_send(&source).unwrap();
    eventually("the accept", || {
        (!rig.ba.sent(FileAction::Accept).is_empty()).then_some(())
    });
    let b = rig.b().clone();
    let (held_up, release) = channel::<()>();
    let (started_tx, started) = channel::<()>();
    let blocker = std::thread::spawn(move || {
        b.probe(move |_| {
            let _ = started_tx.send(());
            let _ = release.recv_timeout(Duration::from_secs(10));
        });
    });
    started.recv().unwrap();
    rig.ba.thaw();
    // The sender pours frames in while the receiver cannot read them.
    eventually("frames to be sent", || {
        (rig.ab.frames.load(Ordering::SeqCst) > 8).then_some(())
    });
    held_up.send(()).unwrap();
    blocker.join().unwrap();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "every byte, none lost");
    assert!(
        !rig.ba.sent(FileAction::Resume).is_empty(),
        "the overflow restarted the sender"
    );
}

#[test]
fn a_silent_link_only_shows_a_note_and_the_transfer_carries_on_when_it_wakes() {
    // A phone that dozes mid-transfer: nothing moves either way for a while.
    let rig = Rig::with(
        "doze",
        stalls(),
        stalls(),
        crate::device_links::webrtc::transfer_manager::FILE_QUEUE_MAX_BYTES,
    );
    let bytes = payload(48 * BLOCK_BYTES as usize, 44);
    let source = rig.source("f.bin", &bytes);
    // The link stalls after a few blocks (the frames of the rest wait).
    let ab = rig.ab.clone();
    let mut seen = 0;
    *rig.ab.frame_hook.lock().unwrap() = Some(Box::new(move |frame| {
        if parse_frame(frame).unwrap().header.last_of_block {
            seen += 1;
            if seen == 3 {
                ab.freeze();
            }
        }
        Fate::Deliver
    }));
    let id = rig.a().start_send(&source).unwrap();
    // Longer than the (shortened) no-progress time: no deadline cancels it.
    eventually("the waiting note", || {
        board::note(&id).filter(|note| *note == words::no_progress())
    });
    std::thread::sleep(Duration::from_millis(300));
    assert!(failed_with(&id).is_none(), "no deadline");
    assert!(!was_cancelled(&id));
    // The informational pause was said by someone, in either direction.
    let paused = |pipe: &Pipe| {
        pipe.sent(FileAction::Pause)
            .iter()
            .any(|pause| pause.reason.as_deref() == Some("no-progress"))
    };
    assert!(paused(&rig.ab) || paused(&rig.ba));
    *rig.ab.frame_hook.lock().unwrap() = None;
    rig.ab.thaw();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    eventually("the note to go", || {
        board::note(&id).is_none().then_some(())
    });
}
