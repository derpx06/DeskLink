//! The engine on a fake clock and a fake network: the transitions of 4.4.

use super::*;
use crate::update::engine::{Engine, Status};
use crate::update::model::Decision;

fn engine(fake: &Fake, name: &str) -> Engine<Fake> {
    Engine::new(
        Some(fake.clone()),
        keys(),
        v("0.3.0"),
        Some(scratch(name)),
        NOW,
    )
}

fn manifest(name: &str) -> Vec<u8> {
    fixture(&format!("manifests/{name}.json"))
}

fn offered(e: &Engine<Fake>, now: u64, manual: bool) -> String {
    match e.decision(&settings(), now, manual) {
        Decision::UpToDate => "up".into(),
        Decision::Available(c) => format!("offer {}", c.version),
        Decision::Required(c) => format!("required {}", c.version),
        Decision::Skipped(c) => format!("skipped {}", c.version),
        Decision::Snoozed(c, _) => format!("snoozed {}", c.version),
    }
}

#[test]
fn a_good_manifest_is_stored_and_offered_then_a_304_keeps_it() {
    let fake = Fake::default();
    let mut e = engine(&fake, "good");
    fake.body(manifest("valid-beta"), "\"e1\"");
    assert_eq!(e.check(&settings(), NOW, false), Status::Idle);
    assert_eq!(offered(&e, NOW, false), "offer 0.4.0");
    fake.push(Ok(Fetched::NotModified));
    e.check(&settings(), NOW + 100, false);
    assert_eq!(
        fake.etags.lock().unwrap().last().unwrap().as_deref(),
        Some("\"e1\"")
    );
    assert_eq!(offered(&e, NOW + 100, false), "offer 0.4.0");
    assert_eq!(e.store.last_checked, NOW + 100);
}

#[test]
fn a_bad_manifest_changes_nothing_and_keeps_no_etag() {
    let fake = Fake::default();
    let mut e = engine(&fake, "bad");
    fake.body(manifest("valid-beta"), "\"good\"");
    e.check(&settings(), NOW, false);
    for (step, (bad, code)) in [
        ("tampered-payload", "signature"),
        ("expired", "expired"),
        ("replayed", "replay"),
        ("schema-2", "schema"),
    ]
    .into_iter()
    .enumerate()
    {
        fake.body(manifest(bad), "\"bad\"");
        let status = e.check(&settings(), NOW + 10_000 + step as u64 * 10, true);
        assert!(
            matches!(status, Status::Failed { reason, .. } if reason == code),
            "{bad}: {status:?}"
        );
        assert_eq!(
            e.store.etag.as_deref(),
            Some("\"good\""),
            "{bad}: the bad body's ETag must not be kept"
        );
        assert_eq!(
            offered(&e, NOW + 10_000, false),
            "offer 0.4.0",
            "{bad}: what was verified still stands"
        );
    }
}

#[test]
fn failures_are_bounded_and_the_streak_keeps_its_start() {
    let fake = Fake::default();
    let mut e = engine(&fake, "fail");
    for (i, error) in [FetchError::Offline, FetchError::Timeout, FetchError::Http]
        .into_iter()
        .enumerate()
    {
        fake.push(Err(error));
        let status = e.check(&settings(), NOW + i as u64 * 100, true);
        assert!(matches!(status, Status::Failed { since, .. } if since == NOW));
    }
    assert_eq!(e.store.failures, 3);
    fake.body(manifest("valid-beta"), "\"e\"");
    assert_eq!(e.check(&settings(), NOW + 1000, true), Status::Idle);
    assert_eq!((e.store.failures, e.store.failed_since), (0, None));
}

#[test]
fn off_and_unconfigured_never_touch_the_network() {
    let fake = Fake::default();
    let mut e = engine(&fake, "off");
    let off = Settings {
        auto_check: false,
        ..settings()
    };
    assert_eq!(e.check(&off, NOW, false), Status::Disabled);
    assert_eq!(fake.calls(), 0);
    // By hand still works while automatic checks are off.
    fake.body(manifest("valid-beta"), "\"e\"");
    e.check(&off, NOW, true);
    assert_eq!(fake.calls(), 1);
    let mut none = Engine::<Fake>::new(None, keys(), v("0.3.0"), Some(scratch("none")), NOW);
    assert_eq!(none.check(&settings(), NOW, true), Status::NotConfigured);
    assert_eq!(none.check(&settings(), NOW, false), Status::NotConfigured);
}

#[test]
fn the_manual_button_is_rate_limited_to_once_per_five_seconds() {
    let fake = Fake::default();
    let mut e = engine(&fake, "rate");
    fake.push(Err(FetchError::Offline));
    fake.push(Err(FetchError::Offline));
    e.check(&settings(), NOW, true);
    e.check(&settings(), NOW + 4, true);
    assert_eq!(fake.calls(), 1);
    e.check(&settings(), NOW + 5, true);
    assert_eq!(fake.calls(), 2);
}

#[test]
fn skip_snooze_and_a_newer_version_in_order() {
    let fake = Fake::default();
    let mut e = engine(&fake, "prefs");
    fake.body(manifest("valid-beta"), "\"e\"");
    e.check(&settings(), NOW, false);
    e.skip(v("0.4.0"));
    assert_eq!(offered(&e, NOW, false), "skipped 0.4.0");
    assert_eq!(
        offered(&e, NOW, true),
        "offer 0.4.0",
        "a manual check shows it without clearing the skip"
    );
    assert_eq!(offered(&e, NOW, false), "skipped 0.4.0");
    e.unskip();
    e.snooze(v("0.4.0"), NOW);
    assert_eq!(offered(&e, NOW + 86_399, false), "snoozed 0.4.0");
    assert_eq!(offered(&e, NOW + 86_400, false), "offer 0.4.0");
    // A newer version clears an older skip.
    e.skip(v("0.4.0"));
    fake.body(manifest("stable-newer-than-beta"), "\"e2\"");
    e.check(&settings(), NOW + 10, true);
    assert_eq!(e.store.skipped_version, None);
    assert_eq!(offered(&e, NOW + 10, false), "offer 1.0.0");
}

#[test]
fn required_ignores_skip_and_snooze_and_installing_drops_everything() {
    let fake = Fake::default();
    let mut e = engine(&fake, "required");
    fake.body(manifest("required"), "\"e\"");
    e.check(&settings(), NOW, false);
    e.skip(v("0.4.0"));
    e.snooze(v("0.4.0"), NOW);
    assert_eq!(offered(&e, NOW, false), "required 0.4.0");
    e.set_installed(v("0.4.0"));
    assert_eq!(offered(&e, NOW, false), "up");
    assert_eq!(
        (
            e.store.skipped_version.clone(),
            e.store.snoozed_version.clone()
        ),
        (None, None)
    );
}

#[test]
fn the_state_survives_a_restart_and_the_cache_is_checked_again() {
    let fake = Fake::default();
    let path = scratch("restart");
    let mut e = Engine::new(
        Some(fake.clone()),
        keys(),
        v("0.3.0"),
        Some(path.clone()),
        NOW,
    );
    fake.body(manifest("valid-beta"), "\"e\"");
    e.check(&settings(), NOW, false);
    e.skip(v("0.4.0"));
    let id = e.store.install_id.clone();
    let again = Engine::new(
        Some(fake.clone()),
        keys(),
        v("0.3.0"),
        Some(path.clone()),
        NOW + 50,
    );
    assert_eq!(again.store.install_id, id);
    assert_eq!(offered(&again, NOW + 50, false), "skipped 0.4.0");
    // The file's mode is private.
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // An edited cache no longer verifies: it is dropped, with its ETag.
    let mut stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let mut cached: serde_json::Value =
        serde_json::from_str(stored["cached"].as_str().unwrap()).unwrap();
    let payload = cached["payload"].as_str().unwrap().to_string();
    cached["payload"] = format!("AAAA{payload}").into();
    stored["cached"] = cached.to_string().into();
    std::fs::write(&path, stored.to_string()).unwrap();
    let forged = Engine::new(Some(fake.clone()), keys(), v("0.3.0"), Some(path), NOW + 60);
    assert_eq!(
        (offered(&forged, NOW + 60, false), forged.store.etag.clone()),
        ("up".to_string(), None)
    );
}

#[test]
fn a_damaged_state_file_starts_fresh() {
    for junk in [
        &b"{"[..],
        b"",
        b"\xff\xfe",
        b"[1,2]",
        b"{\"install_id\":5}",
        b"{\"install_id\":\"zz\",\"etag\":\"a\\nb\"}",
    ] {
        let path = scratch("junk");
        std::fs::write(&path, junk).unwrap();
        let fake = Fake::default();
        let e = Engine::new(Some(fake), keys(), v("0.3.0"), Some(path), NOW);
        assert_eq!(e.store.install_id.len(), 32);
        assert_eq!(e.store.etag, None);
        assert_eq!(e.status, Status::Idle);
    }
}

#[test]
fn a_304_without_a_cache_is_not_trusted() {
    let fake = Fake::default();
    let mut e = engine(&fake, "304");
    fake.push(Ok(Fetched::NotModified));
    assert!(matches!(
        e.check(&settings(), NOW, false),
        Status::Failed { .. }
    ));
    assert_eq!(offered(&e, NOW, false), "up");
}

#[test]
fn the_install_id_is_never_part_of_what_the_fetcher_is_given() {
    // The only input to a fetch is the ETag.
    let fake = Fake::default();
    let mut e = engine(&fake, "noid");
    fake.body(manifest("valid-beta"), "\"e\"");
    e.check(&settings(), NOW, false);
    fake.push(Ok(Fetched::NotModified));
    e.check(&settings(), NOW + 100, false);
    let id = e.store.install_id.clone();
    assert!(fake
        .etags
        .lock()
        .unwrap()
        .iter()
        .flatten()
        .all(|t| !t.contains(&id)));
}

#[test]
fn an_open_does_not_refetch_and_a_failure_backs_off() {
    let fake = Fake::default();
    let mut e = engine(&fake, "gap");
    fake.body(manifest("valid-beta"), "\"e\"");
    e.check(&settings(), NOW, false);
    assert_eq!(fake.calls(), 1);
    // A window opened a moment later asks again: no request.
    e.check(&settings(), NOW + 30, false);
    assert_eq!(fake.calls(), 1);
    fake.push(Err(FetchError::Offline));
    e.check(&settings(), NOW + 61, false);
    assert_eq!(fake.calls(), 2);
    // Offline: the next automatic check waits out the first backoff (1 h).
    for t in [NOW + 120, NOW + 3_000, NOW + 61 + 3_599] {
        e.check(&settings(), t, false);
    }
    assert_eq!(fake.calls(), 2, "one request, not one per open");
    fake.push(Err(FetchError::Offline));
    e.check(&settings(), NOW + 61 + 3_600, false);
    assert_eq!(fake.calls(), 3);
    // By hand is never held by the backoff (only the 5 s gap).
    fake.push(Err(FetchError::Offline));
    e.check(&settings(), NOW + 61 + 3_606, true);
    assert_eq!(fake.calls(), 4);
}

#[test]
fn a_304_on_an_expired_cache_is_a_failure_and_drops_the_cache() {
    let fake = Fake::default();
    let mut e = engine(&fake, "304x");
    fake.body(manifest("valid-beta"), "\"e\"");
    e.check(&settings(), NOW, false);
    assert_eq!(offered(&e, NOW, false), "offer 0.4.0");
    let later = NOW + 40 * 86_400;
    fake.push(Ok(Fetched::NotModified));
    let status = e.check(&settings(), later, false);
    assert!(
        matches!(
            status,
            Status::Failed {
                reason: "expired",
                ..
            }
        ),
        "{status:?}"
    );
    assert_eq!((e.store.etag.clone(), e.store.cached.clone()), (None, None));
    assert_eq!(offered(&e, later, false), "up");
}

#[test]
fn an_unconfigured_build_has_no_thread_state_or_wakeups() {
    use crate::update::service::{spawn, wait_secs};
    let handle = spawn::<Fake>(None, keys(), v("0.3.0"), Some(scratch("inert")));
    let snapshot = handle.snapshot();
    assert_eq!(snapshot.status, Status::NotConfigured);
    assert!(!scratch("inert").exists(), "nothing was written");
    // Checks off: the thread sleeps for a year, whatever is overdue.
    assert!(wait_secs(false, 0, NOW) >= 365 * 86_400);
    assert!(wait_secs(false, NOW + 10, NOW) >= 365 * 86_400);
    // Checks on: until the next one is due, never under 30 s.
    assert_eq!(wait_secs(true, NOW + 5, NOW), 30);
    assert_eq!(wait_secs(true, NOW + 7_200, NOW), 7_200);
    assert_eq!(wait_secs(true, 0, NOW), 30);
}
