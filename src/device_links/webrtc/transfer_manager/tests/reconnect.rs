//! Reconnecting: resuming at the last verified block or checkpoint, and
//! which sends are offered again.

use super::*;

// ── Reconnecting ───────────────────────────────────────────────────────────

#[test]
fn wifi_pulled_mid_transfer_resumes_at_the_last_block_with_the_right_hash() {
    let mut rig = Rig::new("pulled");
    let bytes = payload(10 * BLOCK_BYTES as usize + 321, 51);
    let source = rig.source("f.bin", &bytes);
    pull_after_blocks(&rig, 4);
    let id = rig.a().start_send(&source).unwrap();
    eventually("the link to be pulled", || {
        (!rig.ab.up.load(Ordering::SeqCst)).then_some(())
    });
    let verified = rig.b().probe(|engine| {
        engine
            .incoming
            .values()
            .map(|incoming| incoming.verified_blocks())
            .max()
            .unwrap_or(0)
    });
    assert!(verified >= 3, "{verified} blocks held");
    // The connection comes back: a new link, new managers, the same disks.
    rig.stop(false);
    rig.connect();
    rig.a().resume_sends();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "final hash correct");
    let accept = rig.ba.sent(FileAction::Accept).remove(0);
    assert!(
        accept
            .resume_block
            .is_some_and(|block| block >= verified.min(3)),
        "resumed at block {:?}, not from the start (had {verified})",
        accept.resume_block
    );
    // The same transfer, under the same id and token.
    assert_eq!(rig.ab.sent(FileAction::Offer)[0].transfer_id, id);
    // Only the rest was sent again.
    let resent = rig.ab.frames.load(Ordering::SeqCst);
    assert!(
        resent < 7 * 18 + 6,
        "{resent} frames sent after the reconnect"
    );
    rig.sent_ok();
}

#[test]
fn an_app_killed_mid_receive_resumes_from_its_last_checkpoint() {
    let mut rig = Rig::new("killed");
    let bytes = payload(12 * BLOCK_BYTES as usize + 99, 52);
    let source = rig.source("f.bin", &bytes);
    pull_after_blocks(&rig, 7);
    rig.a().start_send(&source).unwrap();
    eventually("the link to be pulled", || {
        (!rig.ab.up.load(Ordering::SeqCst)).then_some(())
    });
    // Let the periodic checkpoint run, then die without a final one.
    std::thread::sleep(Duration::from_millis(150));
    rig.stop(true);
    rig.connect();
    rig.a().resume_sends();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes);
    let accept = rig.ba.sent(FileAction::Accept).remove(0);
    let from = accept.resume_block.unwrap();
    assert!(
        (1..=7).contains(&from),
        "resumed at block {from}: the last checkpoint, never beyond what was verified"
    );
}

#[test]
fn a_prefix_the_sender_cannot_reproduce_restarts_at_the_beginning_under_a_new_pass() {
    let mut rig = Rig::new("bad-prefix");
    let bytes = payload(6 * BLOCK_BYTES as usize + 7, 53);
    let source = rig.source("f.bin", &bytes);
    pull_after_blocks(&rig, 3);
    rig.a().start_send(&source).unwrap();
    eventually("the link to be pulled", || {
        (!rig.ab.up.load(Ordering::SeqCst)).then_some(())
    });
    rig.stop(false);
    rig.connect();
    // The receiver's accept carries a prefix that is not the file's.
    *rig.ba.control_hook.lock().unwrap() = Some(Box::new(|control| {
        if control.action == FileAction::Accept && control.resume_block.is_some_and(|b| b > 0) {
            control.prefix_sha256 = Some("ab".repeat(32));
        }
        Fate::Deliver
    }));
    rig.a().resume_sends();
    let got = rig.received();
    assert_eq!(fs::read(got.path).unwrap(), bytes, "still correct");
    // It began again from block 0 (every block went again).
    assert!(rig.ab.frames.load(Ordering::SeqCst) >= 6 * 17);
}

#[test]
fn only_the_reconnecting_devices_intact_sends_are_offered_again() {
    let rig = Rig::new("resume-filter");
    let state = rig.state("a");
    // A record for another device, one whose file is gone, one intact.
    let intact = rig.source("intact.bin", b"intact");
    let metadata = fs::metadata(&intact).unwrap();
    let record = |id: &str, device: &str, source: &Path, size: u64| {
        let checkpoint = crate::device_links::webrtc::transfer_manager::store::SendCheckpoint {
            transfer_id: id.to_string(),
            device_id: device.to_string(),
            token: "t".repeat(32),
            filename: source.file_name().unwrap().to_string_lossy().into_owned(),
            total_size: size,
            mtime: crate::device_links::webrtc::transfer_manager::store::modified_ms(&metadata),
            source: source.to_path_buf(),
            history: false,
            clipboard_mime: None,
            folder: None,
            content_sha256: None,
            restarts: 0,
        };
        fs::write(
            state.join(format!(".desklink-{id}.send.json")),
            serde_json::to_vec(&checkpoint).unwrap(),
        )
        .unwrap();
    };
    record(id(1), "beta", &intact, metadata.len());
    record(id(2), "someone-else", &intact, metadata.len());
    record(id(3), "beta", &rig.base.join("src").join("vanished.bin"), 5);
    rig.a().resume_sends();
    rig.received();
    let offers = rig.ab.sent(FileAction::Offer);
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].transfer_id, id(1));
    // The vanished one is dropped for good; the other device's stays.
    eventually("the records", || {
        let left = names(&state);
        (!left.iter().any(|n| n.contains(id(3))) && left.iter().any(|n| n.contains(id(2))))
            .then_some(())
    });
    // Once per connection.
    rig.a().resume_sends();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(rig.ab.sent(FileAction::Offer).len(), 1);
}
