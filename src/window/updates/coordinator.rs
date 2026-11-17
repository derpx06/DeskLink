//! When the popup shows: the glue over `update::plan_open`. It decides at an
//! open, never from a timer, at most once per open, never while another
//! dialog or the first-run tour is up; a stale answer is refreshed at the open
//! with a 3.5 s budget and a late one is kept for next time, never shown late.
//! An explicit request (the top bar's row, Check for updates) shows it now.
//!
//! What counts as an open: `present_window` (the application) shows the main
//! window and, when it was hidden, calls [`on_window_presented`]. Everything
//! that brings the window up goes through it (a cold start, the tray, a
//! notification click, a `tel:` link, the D-Bus activation, the launcher), so
//! all of them count: DeskLink was opened. A service start at login
//! (`--gapplication-service`) never presents the window and so never counts;
//! a focus change or a page change is not a presentation either.
//!
//! The decisions live in [`State`] (pure, tested without a display): each open
//! gets a fresh id, claimed before anything is planned; a poller or a late
//! answer belongs to one open id and does nothing once a newer open began
//! (or the window was hidden again).
//!
//! Sibling edges: `dialog`, `model`. Outside: `crate::update`.

use super::dialog::{self, Sink};
use super::model::popup as popup_model;
use super::*;
use crate::update::{
    detect_source, handle, plan_open, settle, Channel, Command, Decision, Env, OpenGuard,
    OpenInput, OpenPlan, Snapshot, Status, FORCED_WAIT_SECS, OPEN_BUDGET_MS,
};
use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

/// What an open does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Step {
    Nothing,
    Show {
        required: bool,
    },
    /// A check was sent; look for its answer for at most the open budget.
    Refresh,
}

/// The open ids and pollers: pure state, no widgets.
#[derive(Debug, Default)]
pub(super) struct State {
    guard: OpenGuard,
    current: u64,
}

impl State {
    /// A new open (the window shown after being hidden): its id is claimed
    /// BEFORE anything is planned, which retires the older open's pollers.
    /// Disabled checks plan from the cache alone (no refresh, no network) and
    /// only a Required update shows then.
    pub(super) fn open(&mut self, snapshot: &Snapshot, blocked: bool) -> (u64, Step) {
        let previous = self.guard.decided();
        let id = self.guard.begin();
        self.current = id;
        self.guard.claim(id);
        if matches!(snapshot.status, Status::NotConfigured) {
            return (id, Step::Nothing);
        }
        let disabled = matches!(snapshot.status, Status::Disabled);
        if disabled && !matches!(snapshot.decision, Decision::Required(_)) {
            return (id, Step::Nothing);
        }
        let input = OpenInput {
            open_id: id,
            decided_open: previous,
            active: true,
            cached: snapshot.decision.clone(),
            // Disabled: never stale, so never a refresh.
            cache_age: if disabled {
                Some(0)
            } else {
                snapshot.cache_age
            },
            blocked,
            forced: false,
        };
        let step = match plan_open(&input) {
            OpenPlan::Nothing | OpenPlan::Wait => Step::Nothing,
            OpenPlan::Show { required } => Step::Show { required },
            OpenPlan::Refresh => Step::Refresh,
        };
        (id, step)
    }

    /// Whether `id` is still the latest open (a poller of an older one stops).
    pub(super) fn is_current(&self, id: u64) -> bool {
        self.current == id
    }

    /// The same open asked again: nothing (the id was claimed already).
    #[cfg(test)]
    pub(super) fn again(&self, id: u64, snapshot: &Snapshot, blocked: bool) -> Step {
        let input = OpenInput {
            open_id: id,
            decided_open: self.guard.decided(),
            active: true,
            cached: snapshot.decision.clone(),
            cache_age: snapshot.cache_age,
            blocked,
            forced: false,
        };
        match plan_open(&input) {
            OpenPlan::Show { required } => Step::Show { required },
            OpenPlan::Refresh => Step::Refresh,
            _ => Step::Nothing,
        }
    }

    /// The refresh of open `id` ended: `answered_ms` is when its answer came
    /// (`None` when it failed). A stale open, a hidden window or a busy
    /// screen show nothing.
    pub(super) fn settled(
        &self,
        id: u64,
        visible: bool,
        blocked: bool,
        answered_ms: Option<u64>,
        decision: &Decision,
    ) -> Step {
        if !self.is_current(id) || !visible {
            return Step::Nothing;
        }
        match settle(answered_ms, decision, blocked) {
            OpenPlan::Show { required } => Step::Show { required },
            _ => Step::Nothing,
        }
    }
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
    static SHOWING: RefCell<Option<glib::WeakRef<adw::Dialog>>> = const { RefCell::new(None) };
    /// One poller for explicit requests: a newer request retires the older.
    static REQUEST: Cell<u64> = const { Cell::new(0) };
}

/// Whether the screen is not free: another dialog (a pairing or transfer
/// prompt, a permission dialog, Settings...), the first-run tour, or this
/// popup already.
pub(super) fn blocked(window: &DesklinkWindow) -> bool {
    window.visible_dialog().is_some()
        || window.imp().onboarding.borrow().is_some()
        || SHOWING.with(|s| s.borrow().as_ref().and_then(|d| d.upgrade()).is_some())
}

fn show_decision(
    window: &DesklinkWindow,
    snapshot: &Snapshot,
    decision: &Decision,
    required: bool,
) {
    let Some(component) = decision.component() else {
        return;
    };
    let source = detect_source(&Env::detect());
    let beta = snapshot.channel == Channel::Beta;
    let model = popup_model(component, snapshot.installed, beta, required, source);
    present_popup(window, model, dialog::app_sink());
}

/// Puts the popup up and remembers it, so nothing stacks on it.
pub(super) fn present_popup(
    window: &DesklinkWindow,
    model: super::model::Popup,
    sink: Sink,
) -> adw::Dialog {
    let dialog = dialog::present(window, model, sink);
    SHOWING.with(|s| *s.borrow_mut() = Some(dialog.downgrade()));
    dialog
}

/// The window was shown after being hidden, or for the first time: an open.
pub(crate) fn on_window_presented(window: &DesklinkWindow) {
    let Some(update) = handle() else { return };
    let snapshot = update.snapshot();
    let (open_id, step) = STATE.with(|s| s.borrow_mut().open(&snapshot, blocked(window)));
    match step {
        Step::Nothing => {}
        Step::Show { required } => {
            // After the window has been put up, so the popup lands over it.
            let window = window.clone();
            glib::idle_add_local_once(move || {
                let current = STATE.with(|s| s.borrow().is_current(open_id));
                if current && window.is_visible() && !blocked(&window) {
                    show_decision(&window, &snapshot, &snapshot.decision.clone(), required);
                }
            });
        }
        Step::Refresh => {
            let before = snapshot.generation;
            let sink: Sink = dialog::app_sink();
            sink(Command::Check { manual: false });
            let started = Instant::now();
            let window = window.clone();
            glib::timeout_add_local(Duration::from_millis(100), move || {
                if !STATE.with(|s| s.borrow().is_current(open_id)) || !window.is_visible() {
                    return glib::ControlFlow::Break;
                }
                let Some(update) = handle() else {
                    return glib::ControlFlow::Break;
                };
                let now = update.snapshot();
                let waited = started.elapsed().as_millis() as u64;
                if now.generation == before && waited <= OPEN_BUDGET_MS {
                    return glib::ControlFlow::Continue;
                }
                // The answer came (`generation` moved), or the budget is
                // spent: a late one is only stored for the next open.
                let answered = (now.generation != before).then_some(waited);
                let failed = matches!(now.last_check, Status::Failed { .. });
                let answered_ms = if failed {
                    None
                } else {
                    answered.or(Some(OPEN_BUDGET_MS + 1))
                };
                let step = STATE.with(|s| {
                    s.borrow().settled(
                        open_id,
                        window.is_visible(),
                        blocked(&window),
                        answered_ms,
                        &now.decision,
                    )
                });
                if let Step::Show { required } = step {
                    show_decision(&window, &now, &now.decision.clone(), required);
                }
                glib::ControlFlow::Break
            });
        }
    }
}

/// An explicit request: the top bar's row, or a manual check that found
/// something. Shows now (as a manual check reads the cache), never over
/// another dialog: it waits for that one for up to 30 s. A newer request
/// retires the older one's wait, so there is only ever one. With no update
/// service (or nothing on offer) it does nothing.
pub(crate) fn show_requested(window: &DesklinkWindow) {
    if handle().is_none() {
        return;
    }
    let token = REQUEST.with(|r| {
        r.set(r.get() + 1);
        r.get()
    });
    let started = Instant::now();
    let window = window.clone();
    let attempt = move || {
        if REQUEST.with(Cell::get) != token {
            return glib::ControlFlow::Break;
        }
        let Some(update) = handle() else {
            return glib::ControlFlow::Break;
        };
        let snapshot = update.snapshot();
        let input = OpenInput {
            open_id: 0,
            decided_open: None,
            active: true,
            cached: snapshot.manual_decision.clone(),
            cache_age: snapshot.cache_age,
            blocked: blocked(&window),
            forced: true,
        };
        match plan_open(&input) {
            OpenPlan::Show { required } => {
                show_decision(
                    &window,
                    &snapshot,
                    &snapshot.manual_decision.clone(),
                    required,
                );
                glib::ControlFlow::Break
            }
            OpenPlan::Wait if started.elapsed() < Duration::from_secs(FORCED_WAIT_SECS) => {
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Break,
        }
    };
    if let glib::ControlFlow::Continue = attempt() {
        glib::timeout_add_local(Duration::from_millis(500), attempt);
    }
}
