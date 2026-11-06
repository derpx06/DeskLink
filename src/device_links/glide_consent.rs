//! When Glide may ask the desktop for its input-capture permission.
//!
//! `org.freedesktop.portal.InputCapture` shows a consent dialog ("Capture
//! Input") on every `CreateSession`, and the GNOME 50 portal offers no way to
//! remember the answer (see `docs/ARCHITECTURE.md`, Glide). So the only thing
//! DeskLink controls is how often it asks: this is that policy, a pure state
//! machine with the clock passed in.
//!
//! * Ask only when Glide is on, a phone that allows it is feature-ready and the
//!   person is at the computer (not locked, not idle).
//! * Ask once per run: the session is kept across reconnects and link bounces.
//! * A Cancel is remembered ([`DECLINED_FOR`]); an unanswered or failed ask,
//!   or a session the desktop closed, waits a while; at most [`ASKS_PER_RUN`]
//!   automatic asks in all. The person's own action (Glide switched, "Ask
//!   again") clears the wait.

use std::time::{Duration, Instant};

/// A Cancel is not asked again for this long (and never on its own in a loop).
pub(crate) const DECLINED_FOR: Duration = Duration::from_secs(8 * 3600);
/// A dialog nobody answered (the portal gives up after two minutes).
pub(crate) const UNANSWERED_FOR: Duration = Duration::from_secs(15 * 60);
/// The portal failed for another reason.
pub(crate) const FAILED_FOR: Duration = Duration::from_secs(5 * 60);
/// The desktop closed a session that was working.
pub(crate) const CLOSED_FOR: Duration = Duration::from_secs(5 * 60);
/// Automatic asks per run, however they ended; then only the person asks.
pub(crate) const ASKS_PER_RUN: u32 = 3;
/// How long an answer to "is the person here?" is reused.
const PRESENCE_TTL: Duration = Duration::from_secs(2);

/// Why Glide is waiting instead of asking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Why {
    Declined,
    Unanswered,
    Failed,
    Closed,
    /// [`ASKS_PER_RUN`] asks used up.
    Limit,
}

impl Why {
    /// The plain words for the window (the person can always "Ask again").
    pub(crate) fn words(self) -> &'static str {
        match self {
            Why::Declined => "Glide needs your OK to capture the mouse. Turn it on again to ask.",
            Why::Unanswered => {
                "The permission window was not answered, so Glide is waiting. Ask again when you \
                 are at the computer."
            }
            Why::Failed => "The desktop's input capture did not start. Ask again to retry.",
            Why::Closed => {
                "The desktop stopped Glide. DeskLink tries again in a few minutes, or ask now."
            }
            Why::Limit => "Glide stopped after asking several times. Ask again to try once more.",
        }
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
    /// A session exists (kept for the run).
    Open,
    /// Waiting, until the time given or (`None`) until the person acts.
    Held(Why, Option<Instant>),
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
}

impl Consent {
    pub(crate) const fn new() -> Self {
        Self {
            phase: Phase::Idle,
            asks: 0,
            present: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> Phase {
        self.phase
    }

    pub(crate) fn held(&self) -> Option<Why> {
        match self.phase {
            Phase::Held(why, _) => Some(why),
            _ => None,
        }
    }

    /// Whether a session may be created now. `ready` is Glide on with a
    /// feature-ready phone that allows it; `present` says whether the person
    /// is at the computer and is only called when everything else holds.
    pub(crate) fn step(
        &mut self,
        now: Instant,
        ready: bool,
        present: impl FnOnce() -> bool,
    ) -> Step {
        match self.phase {
            Phase::Open | Phase::Asking => return Step::Wait,
            Phase::Held(_, Some(until)) if now >= until => self.phase = Phase::Idle,
            Phase::Held(..) => return Step::Wait,
            Phase::Idle => {}
        }
        if !ready {
            return Step::Wait;
        }
        if self.asks >= ASKS_PER_RUN {
            self.phase = Phase::Held(Why::Limit, None);
            return Step::Wait;
        }
        let here = match self.present {
            Some((at, here)) if now.saturating_duration_since(at) < PRESENCE_TTL => here,
            _ => {
                let here = present();
                self.present = Some((now, here));
                here
            }
        };
        if !here {
            return Step::Wait;
        }
        self.asks += 1;
        self.phase = Phase::Asking;
        Step::Ask
    }

    /// The portal answered the ask.
    pub(crate) fn answered(&mut self, now: Instant, outcome: Outcome) {
        self.phase = match outcome {
            Outcome::Allowed => Phase::Open,
            Outcome::Declined => Phase::Held(Why::Declined, Some(now + DECLINED_FOR)),
            Outcome::Unanswered => Phase::Held(Why::Unanswered, Some(now + UNANSWERED_FOR)),
            Outcome::Failed => Phase::Held(Why::Failed, Some(now + FAILED_FOR)),
        };
    }

    /// The desktop ended the session (or its input socket).
    pub(crate) fn closed(&mut self, now: Instant) {
        self.phase = Phase::Held(Why::Closed, Some(now + CLOSED_FOR));
    }

    /// The person acted (switched Glide, pressed "Ask again"): any wait ends
    /// and the count starts over. A live session or an open dialog stays.
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
        assert_eq!(c.phase(), Phase::Open);
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
        // Reconnecting, hours later, still no dialog.
        for n in [1, 60, 3600, 7 * 3600] {
            assert_eq!(c.step(t + secs(n), true, HERE), Step::Wait, "{n}");
        }
        // After the long wait it may ask once more on its own.
        assert_eq!(c.step(t + DECLINED_FOR, true, HERE), Step::Ask);
    }

    #[test]
    fn the_person_asking_again_ends_the_wait_at_once() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        c.answered(t, Outcome::Declined);
        assert_eq!(c.step(t + secs(5), true, HERE), Step::Wait);
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

    #[test]
    fn an_unanswered_dialog_or_a_failure_waits_then_tries_again() {
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
    fn a_session_the_desktop_closed_comes_back_later_not_at_once() {
        let (mut c, t) = (Consent::new(), Instant::now());
        assert_eq!(c.step(t, true, HERE), Step::Ask);
        c.answered(t, Outcome::Allowed);
        c.closed(t + secs(100));
        assert_eq!(c.held(), Some(Why::Closed));
        assert_eq!(c.step(t + secs(101), true, HERE), Step::Wait);
        assert_eq!(c.step(t + secs(100) + CLOSED_FOR, true, HERE), Step::Ask);
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
        assert_eq!(c.phase(), Phase::Open);
    }
}
