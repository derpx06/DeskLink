//! The show-on-open rules on a fake clock (4.1 and 4.5).

use super::*;
use crate::update::model::{Component, Decision};
use crate::update::open::*;

fn component() -> Component {
    Component {
        version: v("0.4.0"),
        released: None,
        min_supported: v("0.0.0"),
        required_reason: None,
        rollout: 100,
        notes: vec![],
        notes_url: None,
        release_url: None,
        tarball: None,
    }
}

fn input(cached: Decision, age: Option<u64>) -> OpenInput {
    OpenInput {
        open_id: 1,
        decided_open: None,
        active: true,
        cached,
        cache_age: age,
        blocked: false,
        forced: false,
    }
}

#[test]
fn a_fresh_cache_shows_from_the_cache_without_the_network() {
    let i = input(Decision::Available(component()), Some(60));
    assert_eq!(plan_open(&i), OpenPlan::Show { required: false });
    let r = input(Decision::Required(component()), Some(60));
    assert_eq!(plan_open(&r), OpenPlan::Show { required: true });
    assert_eq!(
        plan_open(&input(Decision::UpToDate, Some(60))),
        OpenPlan::Nothing
    );
    assert_eq!(
        plan_open(&input(Decision::Skipped(component()), Some(60))),
        OpenPlan::Nothing
    );
    assert_eq!(
        plan_open(&input(Decision::Snoozed(component(), 9), Some(60))),
        OpenPlan::Nothing
    );
}

#[test]
fn at_most_once_per_open_and_never_when_off() {
    let mut guard = OpenGuard::default();
    let open = guard.begin();
    assert!(guard.claim(open));
    assert!(
        !guard.claim(open),
        "a second call in the same open does nothing"
    );
    let mut i = input(Decision::Available(component()), Some(1));
    i.open_id = open;
    i.decided_open = guard.decided();
    assert_eq!(plan_open(&i), OpenPlan::Nothing);
    let second = guard.begin();
    assert!(second > open && guard.claim(second));
    let mut off = input(Decision::Available(component()), Some(1));
    off.active = false;
    assert_eq!(plan_open(&off), OpenPlan::Nothing);
}

#[test]
fn a_busy_screen_skips_this_open_and_does_not_retry() {
    for decision in [
        Decision::Available(component()),
        Decision::Required(component()),
    ] {
        let mut i = input(decision.clone(), Some(1));
        i.blocked = true;
        assert_eq!(plan_open(&i), OpenPlan::Nothing);
        assert_eq!(settle(Some(10), &decision, true), OpenPlan::Nothing);
    }
}

#[test]
fn a_stale_cache_refreshes_at_the_open_with_a_3_5_second_budget() {
    let stale = input(Decision::Available(component()), Some(86_401));
    assert_eq!(plan_open(&stale), OpenPlan::Refresh);
    assert_eq!(
        plan_open(&input(Decision::Available(component()), Some(86_400))),
        OpenPlan::Show { required: false }
    );
    assert_eq!(
        plan_open(&input(Decision::UpToDate, None)),
        OpenPlan::Refresh
    );
    let d = Decision::Available(component());
    assert_eq!(
        settle(Some(3_400), &d, false),
        OpenPlan::Show { required: false }
    );
    assert_eq!(
        settle(Some(3_500), &d, false),
        OpenPlan::Show { required: false }
    );
    assert_eq!(
        settle(Some(3_501), &d, false),
        OpenPlan::Nothing,
        "a late answer is stored, never shown late"
    );
    // A failed refresh falls back to the cache.
    assert_eq!(settle(None, &d, false), OpenPlan::Show { required: false });
    assert_eq!(settle(None, &Decision::UpToDate, false), OpenPlan::Nothing);
}

#[test]
fn required_is_shown_at_every_open_even_with_a_stale_cache() {
    let mut i = input(Decision::Required(component()), Some(999_999));
    assert_eq!(plan_open(&i), OpenPlan::Show { required: true });
    i.open_id = 2;
    i.decided_open = Some(1);
    assert_eq!(plan_open(&i), OpenPlan::Show { required: true });
}

#[test]
fn an_explicit_request_bypasses_once_and_snooze_but_never_stacks() {
    let mut i = input(Decision::Available(component()), Some(1));
    i.forced = true;
    i.decided_open = Some(1);
    assert_eq!(plan_open(&i), OpenPlan::Show { required: false });
    i.blocked = true;
    assert_eq!(plan_open(&i), OpenPlan::Wait);
    i.cached = Decision::UpToDate;
    assert_eq!(plan_open(&i), OpenPlan::Nothing);
    assert_eq!(FORCED_WAIT_SECS, 30);
}

#[test]
fn later_comes_back_at_the_first_open_after_a_day() {
    // Fake clock through the engine: a snooze made at T hides the popup
    // until T + 24 h, then the cached decision is Available again.
    let fake = Fake::default();
    let mut e = crate::update::engine::Engine::new(
        Some(fake.clone()),
        keys(),
        v("0.3.0"),
        Some(scratch("later")),
        NOW,
    );
    fake.body(fixture("manifests/valid-beta.json"), "\"e\"");
    e.check(&settings(), NOW, false);
    e.snooze(v("0.4.0"), NOW);
    let plan_at = |t: u64| plan_open(&input(e.decision(&settings(), t, false), Some(100)));
    assert_eq!(plan_at(NOW + 3_600), OpenPlan::Nothing);
    assert_eq!(plan_at(NOW + 86_399), OpenPlan::Nothing);
    assert_eq!(plan_at(NOW + 86_400), OpenPlan::Show { required: false });
}
