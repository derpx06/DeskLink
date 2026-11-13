//! When Glide may ask the desktop for its input-capture permission.
//!
//! `org.freedesktop.portal.InputCapture` shows a consent dialog ("Capture
//! Input") on every `CreateSession`, and the GNOME 50 portal offers no way to
//! remember the answer (see `docs/ARCHITECTURE.md`, Glide). GNOME also ends
//! the session itself when the screen locks (mutter drops it as the shield
//! comes up), so a new dialog after every lock cannot be avoided. The only
//! thing DeskLink controls is *when* it asks: this is that policy, a pure
//! state machine with the clock passed in.
//!
//! * Ask only when Glide is on, a phone that allows it is feature-ready and the
//!   person is at the computer (not locked, not idle).
//! * Ask once per run: the session is kept across reconnects and link bounces.
//! * A session the desktop closed, an unanswered dialog and a failure are
//!   asked again when the person comes back (seen away since, here now: the
//!   unlock after a lock, the wake after a sleep), never on a timer while
//!   they work; a Cancel is remembered ([`DECLINED_FOR`]); at most
//!   [`ASKS_PER_RUN`] automatic asks that did not lead to a working session.
//!   The person's own action (Glide switched, "Turn on again") ends any wait.

use std::time::{Duration, Instant};

/// A Cancel is not asked again for this long (and never on its own in a loop).
pub(crate) const DECLINED_FOR: Duration = Duration::from_secs(8 * 3600);
/// A dialog nobody answered (the portal gives up after two minutes) is asked
/// again when the person comes back, or after this if they never seemed to leave.
pub(crate) const UNANSWERED_FOR: Duration = Duration::from_secs(15 * 60);
/// The portal failed for another reason (or when the person comes back).
pub(crate) const FAILED_FOR: Duration = Duration::from_secs(5 * 60);
/// Automatic asks per run that did not lead to a working session; then only
/// the person asks.
pub(crate) const ASKS_PER_RUN: u32 = 3;
/// A session that lived this long worked: the desktop closing it later (a
/// lock) starts the count of [`ASKS_PER_RUN`] over.
pub(crate) const WORKED_FOR: Duration = Duration::from_secs(60);
/// The person seen away this long before the desktop closed the session
/// still counts as away for it (the lock and the close race each other).
const AWAY_SLACK: Duration = Duration::from_secs(60);
/// How long an answer to "is the person here?" is reused.
const PRESENCE_TTL: Duration = Duration::from_secs(2);

/// Why Glide is waiting instead of asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    Declined,
    Unanswered,
    Failed,
    /// The desktop ended the session while the person was away (a lock, a
    /// sleep: GNOME drops it then).
    Locked,
    /// The desktop ended the session with the person here (a portal restart).
    Closed,
    /// [`ASKS_PER_RUN`] asks used up.
    Limit,
}

impl Why {
    /// The plain words for the window (the person can always turn it on again).
    pub(crate) fn words(self) -> &'static str {
        match self {
            Why::Declined => {
                "Off for now because you said no to sharing the mouse. Glide does not ask \
                 again for 8 hours unless you turn it on again."
            }
            Why::Unanswered => {
                "Nobody answered the permission window. Glide asks again when you are back at \
                 the computer."
            }
            Why::Failed => {
                "The desktop's input capture did not start. Glide tries again when you are back, \
                 or in a few minutes."
            }
            Why::Locked => {
                "Paused: the screen was locked or the computer slept, and GNOME ends Glide then. \
                 Glide asks again when you unlock."
            }
            Why::Closed => {
                "Paused: the desktop ended Glide's permission. Glide asks again when you are back \
                 after a lock or a break, or turn it on again now."
            }
            Why::Limit => "Stopped after asking several times. Turn it on again to ask once more.",
        }
    }

    /// Whether the wait ends when the person comes back to the computer.
    fn ends_on_return(self) -> bool {
        matches!(
            self,
            Why::Unanswered | Why::Failed | Why::Locked | Why::Closed
        )
    }
}

/// How an ask ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Allowed,
    /// Cancel, or the desktop said no.
    Declined,
    /// Timed out with nobody answering.
    Unanswered,
    Failed,
}

impl Outcome {
    /// From the text of a portal error (`request` in `remote_desktop_portal`).
    pub(crate) fn of_error(error: &str) -> Self {
        if error.contains("was cancelled") || error.contains("was denied") {
            Outcome::Declined
        } else if error.starts_with("Timed out") {
            Outcome::Unanswered
        } else {
            Outcome::Failed
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    /// No session; free to ask when the conditions hold.
    Idle,
    /// The dialog is up.
    Asking,
    /// A session exists (kept for the run), since then.
    Open(Instant),
    /// Waiting since the first time, until the second (`None`: no timer),
    /// or the person's return when [`Why::ends_on_return`], or their action.
    Held(Why, Instant, Option<Instant>),
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Wait,
    /// Create the session now (and tell the person first).
    Ask,
}

#[derive(Debug)]
pub(crate) struct Consent {
    phase: Phase,
    asks: u32,
    present: Option<(Instant, bool)>,
    /// The last time the person was seen away (locked, idle, asleep).
    away_at: Option<Instant>,
}

impl Consent {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            asks: 0,
            present: None,
            away_at: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }

    pub(crate) fn held(&self) -> Option<Why> {
        match self.phase {
            Phase::Held(why, ..) => Some(why),
            _ => None,
        }
    }

    /// Whether the person is at the computer, looked up at most once per
    /// [`PRESENCE_TTL`]; a "no" is remembered as a time they were away.
    pub(crate) fn here(&mut self, now: Instant, present: &mut impl FnMut() -> bool) -> bool {
        let here = match self.present {
            Some((at, here)) if now.saturating_duration_since(at) < PRESENCE_TTL => here,
            _ => {
                let here = present();
                self.present = Some((now, here));
                here
            }
        };
        if !here {
            self.away_at = Some(now);
        }
        here
    }

    /// The person was away without the session saying so (the computer
    /// slept with no lock screen).
    pub(crate) fn was_away(&mut self, now: Instant) {
        self.away_at = Some(now);
        self.present = None;
    }

    /// Whether a session may be created now. `ready` is Glide on with a
    /// feature-ready phone that allows it; `present` says whether the person
    /// is at the computer and is only called when its answer matters.
    pub(crate) fn step(
        &mut self,
        now: Instant,
        ready: bool,
        mut present: impl FnMut() -> bool,
    ) -> Step {
        match self.phase {
            Phase::Open(_) | Phase::Asking => return Step::Wait,
            Phase::Held(why, since, until) => {
                let timed_out = until.is_some_and(|until| now >= until);
                let here = why.ends_on_return() && self.here(now, &mut present);
                if why == Why::Closed && !here {
                    // Seen away after all: the words say what happened.
                    self.phase = Phase::Held(Why::Locked, since, until);
                }
                let back = here && self.away_at.is_some_and(|at| at + AWAY_SLACK >= since);
                if !timed_out && !back {
                    return Step::Wait;
                }
                self.phase = Phase::Idle;
            }
            Phase::Idle => {}
        }
        if !ready {
            return Step::Wait;
        }
        if self.asks >= ASKS_PER_RUN {
            self.phase = Phase::Held(Why::Limit, now, None);
            return Step::Wait;
        }
        if !self.here(now, &mut present) {
            return Step::Wait;
        }
        self.asks += 1;
        self.phase = Phase::Asking;
        Step::Ask
    }

    /// The portal answered the ask.
    pub(crate) fn answered(&mut self, now: Instant, outcome: Outcome) {
        self.phase = match outcome {
            Outcome::Allowed => Phase::Open(now),
            Outcome::Declined => Phase::Held(Why::Declined, now, Some(now + DECLINED_FOR)),
            Outcome::Unanswered => Phase::Held(Why::Unanswered, now, Some(now + UNANSWERED_FOR)),
            Outcome::Failed => Phase::Held(Why::Failed, now, Some(now + FAILED_FOR)),
        };
    }

    /// The desktop ended the session (or its input socket). Asked again when
    /// the person comes back; a session that worked a while does not count
    /// towards [`ASKS_PER_RUN`] (a lock is not a loop).
    pub(crate) fn closed(&mut self, now: Instant) {
        if let Phase::Open(since) = self.phase {
            if now.saturating_duration_since(since) >= WORKED_FOR {
                self.asks = 0;
            }
        }
        let away = self.away_at.is_some_and(|at| at + AWAY_SLACK >= now);
        let why = if away { Why::Locked } else { Why::Closed };
        self.phase = Phase::Held(why, now, None);
    }

    /// The person acted (switched Glide, pressed "Turn on again"): any wait
    /// ends and the count starts over. A live session or an open dialog stays.
    pub(crate) fn person_acts(&mut self) {
        self.asks = 0;
        self.present = None;
        if matches!(self.phase, Phase::Held(..)) {
            self.phase = Phase::Idle;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HERE: fn() -> bool = || true;
    const AWAY: fn() -> bool = || false;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn open(c: &mut Consent, t: Instant) {
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        c.answered(t, Outcome::Allowed);
    }

    #[test]
    fn nothing_is_asked_until_glide_is_on_with_a_ready_phone_and_a_person_here() {
        let (mut c, t) = (Consent::new(), Instant::now());
        // Glide off / no ready phone: the presence is not even looked up.
        assert_eq!(c.step(t, false, || panic!("looked up")), Step::Wait);
        // Ready but the person is away (locked, idle): no dialog on an empty room.
        assert_eq!(c.step(t, true, AWAY), Step::Wait);
        assert_eq!(c.phase(), Phase::Idle);
        // Back at the computer (a fresh answer after the reuse window).
        assert_eq!(c.step(t + secs(3), true, HERE), Step::Ask);
        assert_eq!(c.phase(), Phase::Asking);
    }

    #[test]
    fn presence_is_looked_up_at_most_once_in_two_seconds() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, AWAY), Step::Wait);
        assert_eq!(
            c.step(t + Duration::from_millis(250), true, || panic!("again")),
            Step::Wait
        );
    }

    #[test]
    fn one_session_serves_the_whole_run() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        // The dialog is up: nothing else asks.
        assert_eq!(c.step(t, true, HERE), Step::Wait);
        c.answered(t, Outcome::Allowed);
        // Reconnects and link bounces any number of times: never again.
        for n in 1..50 {
            assert_eq!(c.step(t + secs(n * 600), true, HERE), Step::Wait);
            assert_eq!(c.step(t + secs(n * 600), false, HERE), Step::Wait);
        }
        assert!(matches!(c.phase(), Phase::Open(_)));
    }

    #[test]
    fn a_cancel_is_remembered_and_not_asked_in_a_loop() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        c.answered(
            t,
            Outcome::of_error("Desktop permission request CreateSession was cancelled"),
        );
        assert_eq!(c.held(), Some(Why::Declined));
        // Reconnecting, a lock and an unlock, hours later: still no dialog.
        assert_eq!(c.step(t + secs(60), true, AWAY), Step::Wait);
        for n in [61, 3600, 7 * 3600] {
            assert_eq!(c.step(t + secs(n), true, HERE), Step::Wait, "{n}");
        }
        // After the long wait it may ask once more on its own.
        assert_eq!(c.step(t + DECLINED_FOR, true, HERE), Step::Ask);
    }

    #[test]
    fn the_person_turning_it_on_again_ends_any_wait_at_once() {
        for outcome in [Outcome::Declined, Outcome::Unanswered, Outcome::Failed] {
            let (mut c, t) = (Consent::new(), Instant::now());
            assert_eq!(c.step(t, true, HERE), Step::Ask);
            c.answered(t, outcome);
            assert_eq!(c.step(t + secs(5), true, HERE), Step::Wait, "{outcome:?}");
            c.person_acts();
            assert_eq!(c.step(t + secs(6), true, HERE), Step::Ask, "{outcome:?}");
        }
        let (mut c, t) = (Consent::new(), Instant::now());
        open(&mut c, t);
        c.closed(t + secs(5));
        c.person_acts();
        assert_eq!(c.step(t + secs(6), true, HERE), Step::Ask);
    }

    #[test]
    fn errors_are_told_apart() {
        assert_eq!(
            Outcome::of_error("Desktop permission request X was denied"),
            Outcome::Declined
        );
        assert_eq!(
            Outcome::of_error("Timed out waiting for desktop portal CreateSession"),
            Outcome::Unanswered
        );
        assert_eq!(
            Outcome::of_error("Input capture ConnectToEIS failed: x"),
            Outcome::Failed
        );
    }

    /// Before: a lock closed the session and Glide asked again on a 5 minute
    /// timer, wherever the person was, so the dialog came out of nowhere.
    #[test]
    fn a_lock_that_closes_the_session_asks_again_at_the_unlock_not_on_a_timer() {
        let (mut c, t) = (Consent::new(), Instant::now());
        open(&mut c, t);
        // GNOME drops the session as the lock screen comes up.
        let lock = t + secs(600);
        c.closed(lock);
        assert_eq!(c.held(), Some(Why::Closed), "not seen locked yet");
        // Locked: no dialog, however long, and the words say why.
        for n in [3, 300, 3600] {
            assert_eq!(c.step(lock + secs(n), true, AWAY), Step::Wait, "{n}");
            assert_eq!(c.held(), Some(Why::Locked));
        }
        // Unlocked: asked at once, the moment the person is back.
        assert_eq!(c.step(lock + secs(3603), true, HERE), Step::Ask);
    }

    #[test]
    fn a_close_while_the_person_works_waits_for_them_and_never_pops_up_on_a_timer() {
        let (mut c, t) = (Consent::new(), Instant::now());
        open(&mut c, t);
        let close = t + secs(600);
        c.closed(close);
        // They keep working: never a sudden dialog, and the words do not
        // blame a lock that did not happen.
        for n in [3, 301, 3600, 86_400] {
            assert_eq!(c.step(close + secs(n), true, HERE), Step::Wait, "{n}");
            assert_eq!(c.held(), Some(Why::Closed));
        }
        // They lock and come back: asked then.
        assert_eq!(c.step(close + secs(86_403), true, AWAY), Step::Wait);
        assert_eq!(c.step(close + secs(86_500), true, HERE), Step::Ask);
    }

    #[test]
    fn a_sleep_without_a_lock_screen_counts_as_away() {
        let (mut c, t) = (Consent::new(), Instant::now());
        open(&mut c, t);
        c.closed(t + secs(600));
        assert_eq!(c.step(t + secs(603), true, HERE), Step::Wait);
        c.was_away(t + secs(604));
        assert_eq!(c.step(t + secs(605), true, HERE), Step::Ask);
    }

    #[test]
    fn the_lock_seen_just_before_the_close_counts() {
        let (mut c, t) = (Consent::new(), Instant::now());
        open(&mut c, t);
        // Seen locked, then the desktop's close is noticed 30 s later, unlocked.
        assert!(!c.here(t + secs(100), &mut || false));
        c.closed(t + secs(130));
        assert_eq!(c.held(), Some(Why::Locked));
        assert_eq!(c.step(t + secs(133), true, HERE), Step::Ask);
        // An away long before the close does not.
        let (mut c, t) = (Consent::new(), Instant::now());
        assert!(!c.here(t, &mut || false));
        open(&mut c, t + secs(3));
        c.closed(t + secs(600));
        assert_eq!(c.step(t + secs(603), true, HERE), Step::Wait);
    }

    #[test]
    fn an_unanswered_dialog_asks_again_when_the_person_returns() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        // The person had walked off; logind says so once the dialog times out.
        let timeout = t + secs(120);
        c.answered(timeout, Outcome::Unanswered);
        assert_eq!(c.step(timeout + secs(1), true, AWAY), Step::Wait);
        assert_eq!(c.step(timeout + secs(4), true, HERE), Step::Ask);
    }

    #[test]
    fn an_unanswered_dialog_or_a_failure_with_the_person_here_waits_then_tries_again() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        c.answered(t, Outcome::Unanswered);
        assert_eq!(c.step(t + UNANSWERED_FOR - secs(1), true, HERE), Step::Wait);
        let t2 = t + UNANSWERED_FOR;
        assert_eq!(c.step(t2, true, HERE), Step::Ask);
        c.answered(t2, Outcome::Failed);
        assert_eq!(c.step(t2 + FAILED_FOR - secs(1), true, HERE), Step::Wait);
        assert_eq!(c.step(t2 + FAILED_FOR, true, HERE), Step::Ask);
    }

    #[test]
    fn a_return_with_no_ready_phone_waits_for_the_phone_not_for_another_return() {
        let (mut c, t) = (Consent::new(), Instant::now());
        open(&mut c, t);
        c.closed(t + secs(600));
        assert_eq!(c.step(t + secs(603), false, AWAY), Step::Wait);
        // Back, but the phone is still reconnecting.
        assert_eq!(c.step(t + secs(606), false, HERE), Step::Wait);
        assert_eq!(c.phase(), Phase::Idle);
        assert_eq!(c.step(t + secs(620), true, HERE), Step::Ask);
    }

    /// Before: every ask counted, so the third lock of a day left Glide off
    /// for the rest of the run.
    #[test]
    fn locks_all_day_never_use_up_the_asks() {
        let (mut c, mut t) = (Consent::new(), Instant::now());
        for _ in 0..10 {
            open(&mut c, t);
            t += WORKED_FOR;
            c.closed(t);
            assert_eq!(c.step(t + secs(3), true, AWAY), Step::Wait);
            t += secs(6);
        }
        assert_eq!(c.step(t, true, HERE), Step::Ask);
    }

    #[test]
    fn a_session_the_desktop_keeps_closing_at_once_is_a_loop_and_stops() {
        let (mut c, mut t) = (Consent::new(), Instant::now());
        for _ in 0..ASKS_PER_RUN {
            open(&mut c, t);
            c.closed(t + secs(1));
            assert_eq!(c.step(t + secs(4), true, AWAY), Step::Wait);
            t += secs(7);
        }
        assert_eq!(c.step(t, true, HERE), Step::Wait);
        assert_eq!(c.held(), Some(Why::Limit));
        assert_eq!(c.step(t + secs(10), true, AWAY), Step::Wait);
        assert_eq!(c.step(t + secs(13), true, HERE), Step::Wait);
        c.person_acts();
        assert_eq!(c.step(t + secs(14), true, HERE), Step::Ask);
    }

    #[test]
    fn automatic_asks_are_capped_per_run_until_the_person_acts() {
        let (mut c, mut t) = (Consent::new(), Instant::now());
        for _ in 0..ASKS_PER_RUN {
            assert_eq!(c.step(t, true, HERE), Step::Ask);
            c.answered(t, Outcome::Failed);
            t += FAILED_FOR;
        }
        assert_eq!(c.step(t, true, HERE), Step::Wait);
        assert_eq!(c.held(), Some(Why::Limit));
        assert_eq!(c.step(t + secs(86_400), true, HERE), Step::Wait);
        c.person_acts();
        assert_eq!(c.step(t + secs(86_401), true, HERE), Step::Ask);
    }

    #[test]
    fn the_persons_action_does_not_end_a_live_session_or_an_open_dialog() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        c.person_acts();
        assert_eq!(c.phase(), Phase::Asking);
        c.answered(t, Outcome::Allowed);
        c.person_acts();
        assert!(matches!(c.phase(), Phase::Open(_)));
    }

    #[test]
    fn every_wait_says_what_ends_it() {
        for why in [
            Why::Declined,
            Why::Unanswered,
            Why::Failed,
            Why::Locked,
            Why::Closed,
            Why::Limit,
        ] {
            let words = why.words().to_lowercase();
            assert!(
                words.contains("back")
                    || words.contains("unlock")
                    || words.contains("turn it on again"),
                "{why:?}: {words}"
            );
        }
    }
}
