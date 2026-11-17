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
use std::rc::Rc;
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

/// Where the coordinator reads the update state and sends its commands: the
/// update service in the app, fakes in the tests.
#[derive(Clone)]
pub(super) struct Ports {
    pub(super) snapshot: Rc<dyn Fn() -> Option<Snapshot>>,
    pub(super) sink: Sink,
}

fn app_ports() -> Option<Ports> {
    handle()?;
    Some(Ports {
        snapshot: Rc::new(|| handle().map(|h| h.snapshot())),
        sink: dialog::app_sink(),
    })
}

/// Whether this popup is up now (not merely not yet freed).
fn popup_showing() -> bool {
    SHOWING.with(|s| {
        s.borrow()
            .as_ref()
            .and_then(|d| d.upgrade())
            .is_some_and(|d| d.is_mapped() || d.parent().is_some())
    })
}

/// Whether the screen is not free: another dialog (a pairing or transfer
/// prompt, a permission dialog, Settings...), the first-run tour, or this
/// popup already.
pub(super) fn blocked(window: &DesklinkWindow) -> bool {
    window.visible_dialog().is_some()
        || window.imp().onboarding.borrow().is_some()
        || popup_showing()
}

/// Puts the popup up and remembers it, so nothing stacks on it; forgotten
/// again the moment it closes.
pub(super) fn present_popup(
    window: &DesklinkWindow,
    model: super::model::Popup,
    sink: Sink,
) -> adw::Dialog {
    let dialog = dialog::present(window, model, sink);
    SHOWING.with(|s| *s.borrow_mut() = Some(dialog.downgrade()));
    dialog.connect_closed(|_| SHOWING.with(|s| *s.borrow_mut() = None));
    dialog
}

fn show_decision(
    window: &DesklinkWindow,
    ports: &Ports,
    snapshot: &Snapshot,
    decision: &Decision,
    required: bool,
) {
    let Some(component) = decision.component() else {
        return;
    };
    let source = detect_source(&Env::detect());
    let beta = snapshot.channel == Channel::Beta;
    present_popup(
        window,
        popup_model(component, snapshot.installed, beta, required, source),
        ports.sink.clone(),
    );
}

/// Shows the popup of open `open_id` once the screen is ready: on a cold
/// start the first-run tour is decided a moment after the window comes up,
/// so this waits (up to 5 s) for that decision rather than landing over the
/// tour. It shows nothing if a newer open began, the window was hidden again
/// or the screen is busy.
fn show_when_ready(
    window: &DesklinkWindow,
    ports: &Ports,
    open_id: u64,
    snapshot: Snapshot,
    decision: Decision,
    required: bool,
) {
    let window = window.clone();
    let ports = ports.clone();
    let mut waited = 0u64;
    let attempt = move || {
        if !STATE.with(|s| s.borrow().is_current(open_id)) || !window.is_visible() {
            return glib::ControlFlow::Break;
        }
        if !window.imp().onboarding_decided.get() && waited < 5_000 {
            waited += 100;
            return glib::ControlFlow::Continue;
        }
        if !blocked(&window) {
            show_decision(&window, &ports, &snapshot, &decision, required);
        }
        glib::ControlFlow::Break
    };
    glib::idle_add_local_once({
        let mut attempt = attempt;
        move || {
            if let glib::ControlFlow::Continue = attempt() {
                glib::timeout_add_local(Duration::from_millis(100), attempt);
            }
        }
    });
}

/// The window was shown after being hidden, or for the first time: an open.
pub(crate) fn on_window_presented(window: &DesklinkWindow) {
    if let Some(ports) = app_ports() {
        presented_with(window, &ports);
    }
}

pub(super) fn presented_with(window: &DesklinkWindow, ports: &Ports) {
    let Some(snapshot) = (ports.snapshot)() else {
        return;
    };
    let (open_id, step) = STATE.with(|s| s.borrow_mut().open(&snapshot, blocked(window)));
    match step {
        Step::Nothing => {}
        Step::Show { required } => {
            let decision = snapshot.decision.clone();
            show_when_ready(window, ports, open_id, snapshot, decision, required);
        }
        Step::Refresh => {
            let before = snapshot.generation;
            (ports.sink)(Command::Check { manual: false });
            let started = Instant::now();
            let (window, ports) = (window.clone(), ports.clone());
            glib::timeout_add_local(Duration::from_millis(100), move || {
                if !STATE.with(|s| s.borrow().is_current(open_id)) || !window.is_visible() {
                    return glib::ControlFlow::Break;
                }
                let Some(now) = (ports.snapshot)() else {
                    return glib::ControlFlow::Break;
                };
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
                    let decision = now.decision.clone();
                    show_when_ready(&window, &ports, open_id, now, decision, required);
                }
                glib::ControlFlow::Break
            });
        }
    }
}

/// An explicit request: the top bar's row, or a manual check that found
/// something. Shows now (as a manual check reads the cache), never over
/// another dialog: it waits for that one for up to 30 s. A newer request
/// retires the older one's wait, so there is only ever one; a popup that is
/// already up satisfies the request. With no update service (or nothing on
/// offer) it does nothing.
pub(crate) fn show_requested(window: &DesklinkWindow) {
    if let Some(ports) = app_ports() {
        requested_with(window, &ports);
    }
}

pub(super) fn requested_with(window: &DesklinkWindow, ports: &Ports) {
    if popup_showing() {
        return;
    }
    let token = REQUEST.with(|r| {
        r.set(r.get() + 1);
        r.get()
    });
    let started = Instant::now();
    let (window, ports) = (window.clone(), ports.clone());
    let attempt = move || {
        if REQUEST.with(Cell::get) != token || popup_showing() {
            return glib::ControlFlow::Break;
        }
        let Some(snapshot) = (ports.snapshot)() else {
            return glib::ControlFlow::Break;
        };
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
                    &ports,
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
