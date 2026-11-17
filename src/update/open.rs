//! When the popup may be shown: the pure half of the show-on-open
//! coordinator (`docs/UPDATES.md` 4.1). It decides, from facts handed in, and
//! does nothing itself; the GTK side (phase 2) gathers the facts and acts on
//! the answer.
//!
//! The rules: at most once per open; only at an open, never from a timer;
//! never while another dialog, a pairing or transfer prompt, a permission
//! dialog or onboarding is up (the open is then skipped, not retried); a
//! cache older than a day is refreshed at the open with a 3.5 s budget, and an
//! answer that comes later is kept for the next open, never shown late. A
//! Required update is shown at every open. An explicit request (`ShowUpdate`
//! from the top bar) bypasses "once per open" and snooze, but never stacks on
//! another dialog: it waits for it, for 30 s at most.
//!
//! One departure from the section's order, on purpose: a stale cache is
//! refreshed first whatever it says, because a cache that says "up to date"
//! is exactly the one that may be missing a release (a computer that was off
//! for a week). The section's order would only ever refresh a cache that
//! already had something to show.
//!
//! Sibling edges: `model` (`Decision`), `schedule` (`STALE_AFTER`).

use super::model::Decision;
use super::schedule::STALE_AFTER;

/// How long an open waits for a refresh before storing its answer for next time.
pub(crate) const OPEN_BUDGET_MS: u64 = 3_500;
/// How long an explicit request waits for another dialog to close.
pub(crate) const FORCED_WAIT_SECS: u64 = 30;

/// Facts at an open.
#[derive(Debug, Clone)]
pub(crate) struct OpenInput {
    pub(crate) open_id: u64,
    /// The open the popup was last decided for.
    pub(crate) decided_open: Option<u64>,
    /// Updates are on and the build has an address (not Disabled/NotConfigured).
    pub(crate) active: bool,
    /// The decision from the cache now (`manual = false`).
    pub(crate) cached: Decision,
    /// Seconds since the cache was confirmed; `None` when there is no cache.
    pub(crate) cache_age: Option<u64>,
    /// Another dialog, a prompt, onboarding: the screen is not free.
    pub(crate) blocked: bool,
    /// Asked for by `ShowUpdate` (the top bar), not by an open.
    pub(crate) forced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpenPlan {
    /// Do nothing at this open.
    Nothing,
    /// Show the popup now: `required` is the variant with only "How to update".
    Show { required: bool },
    /// Refresh within the budget, then call [`settle`].
    Refresh,
    /// Forced, but the screen is busy: wait (up to [`FORCED_WAIT_SECS`]) and ask again.
    Wait,
}

fn showable(decision: &Decision) -> Option<bool> {
    match decision {
        Decision::Available(_) => Some(false),
        Decision::Required(_) => Some(true),
        _ => None,
    }
}

pub(crate) fn plan_open(input: &OpenInput) -> OpenPlan {
    if input.forced {
        // An explicit request: the cached answer as a manual check reads it,
        // so the caller passes `cached` computed with `manual = true`.
        return match showable(&input.cached) {
            None => OpenPlan::Nothing,
            Some(_) if input.blocked => OpenPlan::Wait,
            Some(required) => OpenPlan::Show { required },
        };
    }
    if !input.active || input.decided_open == Some(input.open_id) {
        return OpenPlan::Nothing;
    }
    if input.cache_age.is_none_or(|age| age > STALE_AFTER)
        && !matches!(input.cached, Decision::Required(_))
    {
        return OpenPlan::Refresh;
    }
    match showable(&input.cached) {
        None => OpenPlan::Nothing,
        Some(_) if input.blocked => OpenPlan::Nothing,
        Some(required) => OpenPlan::Show { required },
    }
}

/// After the refresh of [`OpenPlan::Refresh`]. `answer_ms` is when the
/// verified answer arrived (`None`: it failed, so the cache stands) and
/// `decision` is read from the cache after it, `blocked` is the screen at
/// that moment.
pub(crate) fn settle(answer_ms: Option<u64>, decision: &Decision, blocked: bool) -> OpenPlan {
    if answer_ms.is_some_and(|ms| ms > OPEN_BUDGET_MS) || blocked {
        return OpenPlan::Nothing;
    }
    match showable(decision) {
        Some(required) => OpenPlan::Show { required },
        None => OpenPlan::Nothing,
    }
}

/// Remembers which open the popup was decided for, so a second call in the
/// same open does nothing.
#[derive(Debug, Default)]
pub(crate) struct OpenGuard {
    decided: Option<u64>,
    next: u64,
}

impl OpenGuard {
    /// A new open (a cold start whose window is shown, or a window presented
    /// again after being hidden). Its id goes into [`OpenInput::open_id`].
    pub(crate) fn begin(&mut self) -> u64 {
        self.next += 1;
        self.next
    }

    pub(crate) fn decided(&self) -> Option<u64> {
        self.decided
    }

    /// Marks the open as decided; false when it already was.
    pub(crate) fn claim(&mut self, open_id: u64) -> bool {
        if self.decided == Some(open_id) {
            return false;
        }
        self.decided = Some(open_id);
        true
    }
}
