//! The file thread: the stale sweep's rate and an idle thread's wakeups.

use super::*;

/// Every negotiation attempt and inbound offer makes a manager, and with it a
/// store: the stale-file sweep reads Downloads and the state directory, so it
/// runs once an hour per state directory, not once per store.
#[test]
fn the_stale_sweep_runs_once_an_hour_not_for_every_store() {
    let base = scratch("sweep-once");
    let (state, downloads) = (base.join("state"), base.join("downloads"));
    let first = crate::device_links::webrtc::transfer_manager::store::Store::new(
        state.clone(),
        downloads.clone(),
    )
    .unwrap();
    // A stale file that appears after the first sweep is not read for again
    // by the next stores made within the hour.
    let old = state.join(".desklink-stale.part");
    fs::write(&old, b"x").unwrap();
    let long_ago = std::time::SystemTime::now() - Duration::from_secs(60 * 60 * 24 * 30);
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(long_ago)
        .unwrap();
    let _second = crate::device_links::webrtc::transfer_manager::store::Store::new(
        state.clone(),
        downloads.clone(),
    )
    .unwrap();
    assert!(old.exists(), "no second sweep within the hour");
    // Due again an hour later.
    assert!(!first.sweep_is_due(Instant::now()));
    assert!(first.sweep_is_due(Instant::now() + Duration::from_secs(61 * 60)));
    first.sweep_stale(crate::device_links::webrtc::transfer_manager::store::STALE_AFTER);
    assert!(!old.exists(), "the sweep itself still removes a stale file");
}

/// With no transfer in either direction the file thread has no timer to keep
/// (it woke twenty times a second, for every peer ever built); a transfer
/// makes it keep its timers again; and `shut_down` ends the thread whoever
/// still holds the manager.
#[test]
fn an_idle_file_thread_sleeps_and_shut_down_ends_it() {
    let hand = Hand::new("idle-thread");
    let idle = hand.manager.probe(|engine| engine.wait_hint());
    assert!(idle >= Duration::from_secs(1), "idle wait is {idle:?}");
    // A transfer in flight: timers (checkpoints, stall notes) are short again.
    hand.send(hand.offer(id(1), "busy.bin", 3 * BLOCK_BYTES));
    eventually("the accept", || hand.reply(FileAction::Accept));
    let busy = hand.manager.probe(|engine| engine.wait_hint());
    assert!(busy <= Duration::from_millis(50), "busy wait is {busy:?}");
    // Ended: the thread finishes, though the manager is still held.
    let held = Arc::clone(&hand.manager);
    held.shut_down();
    let handle = held.thread.lock().unwrap().take().expect("a thread");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !handle.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(handle.is_finished(), "the file thread did not end");
    handle.join().unwrap();
}

/// Voluntary context switches of thread `tid` so far: how often it woke.
fn wakeups_of(tid: i64) -> u64 {
    let status = fs::read_to_string(format!("/proc/self/task/{tid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("voluntary_ctxt_switches:"))
        .and_then(|count| count.trim().parse().ok())
        .unwrap_or(0)
}

/// Measured: an idle file thread (a peer with no transfer) used to wake 20
/// times a second; now it wakes for a command or once in five seconds.
#[test]
fn an_idle_file_thread_wakes_less_than_once_a_second() {
    let hand = Hand::new("idle-wakeups");
    let tid = hand
        .manager
        .probe(|_| unsafe { libc::syscall(libc::SYS_gettid) });
    std::thread::sleep(Duration::from_millis(300));
    let before = wakeups_of(tid);
    std::thread::sleep(Duration::from_secs(3));
    let woke = wakeups_of(tid) - before;
    eprintln!("[test] idle file thread: {woke} wakeups in 3 s (was ~60)");
    assert!(woke <= 3, "{woke} wakeups in 3 s");
}
