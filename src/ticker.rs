//! The main loop's periodic look at the daemon, at the pace the moment needs:
//! every second while a window shows or a phone is reachable, every five
//! seconds when there is nothing to show and nobody to talk to. A change in
//! who is reachable wakes the window at once (see `daemon::state::wake_ui`),
//! so the slow tick is only a safety net.

use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::time::Duration;

use gtk::glib;

use crate::device_links::daemon::tick_seconds;

struct Ticker {
    run: RefCell<Box<dyn FnMut() -> glib::ControlFlow>>,
    /// The wait now pending (`None` while it is running).
    waiting: RefCell<Option<glib::SourceId>>,
}

thread_local! {
    static TICKERS: RefCell<Vec<Weak<Ticker>>> = const { RefCell::new(Vec::new()) };
    /// Other main-loop timers with their own pace, told to look now.
    static HOOKS: RefCell<Vec<Box<dyn Fn()>>> = const { RefCell::new(Vec::new()) };
}

/// Runs `run` on the main loop at the daemon's pace until it says
/// [`glib::ControlFlow::Break`].
pub(crate) fn every_tick(run: impl FnMut() -> glib::ControlFlow + 'static) {
    let ticker = Rc::new(Ticker {
        run: RefCell::new(Box::new(run)),
        waiting: RefCell::new(None),
    });
    TICKERS.with(|tickers| tickers.borrow_mut().push(Rc::downgrade(&ticker)));
    arm(&ticker, Duration::from_secs(u64::from(tick_seconds())));
}

fn arm(ticker: &Rc<Ticker>, after: Duration) {
    let this = Rc::clone(ticker);
    let id = glib::timeout_add_local_once(after, move || {
        this.waiting.take();
        let flow = (this.run.borrow_mut())();
        if flow == glib::ControlFlow::Continue {
            arm(&this, Duration::from_secs(u64::from(tick_seconds())));
        }
    });
    ticker.waiting.replace(Some(id));
}

/// Something else that keeps its own time (the Bucket's sampler) says what to
/// do when the tickers are told to look now.
pub(crate) fn on_nudge(hook: impl Fn() + 'static) {
    HOOKS.with(|hooks| hooks.borrow_mut().push(Box::new(hook)));
}

/// Every ticker looks now instead of when its wait ends, and waits again at
/// the pace of the moment: who is reachable changed, or a window was shown.
/// Main thread only.
pub(crate) fn nudge_all() {
    let live: Vec<Rc<Ticker>> = TICKERS.with(|tickers| {
        let mut tickers = tickers.borrow_mut();
        tickers.retain(|ticker| ticker.strong_count() > 0);
        tickers.iter().filter_map(Weak::upgrade).collect()
    });
    for ticker in live {
        // One that is running now is looking already.
        if let Some(id) = ticker.waiting.take() {
            id.remove();
            arm(&ticker, Duration::ZERO);
        }
    }
    HOOKS.with(|hooks| hooks.borrow().iter().for_each(|hook| hook()));
}

#[cfg(test)]
mod tests {
    use crate::device_links::daemon::{tick_for, IDLE_TICK_SECONDS};

    /// A nudge (who is reachable changed, or a window was shown) makes every
    /// ticker look now, not when its slow wait ends.
    #[test]
    fn a_nudge_runs_a_slow_ticker_at_once() {
        use super::{every_tick, nudge_all, on_nudge};
        use std::cell::Cell;
        use std::rc::Rc;
        // The timers attach to the default context: this test owns it.
        let context = gtk::glib::MainContext::default();
        let _owner = context.acquire().expect("the test owns the main context");
        let looked = Rc::new(Cell::new(0));
        let hooked = Rc::new(Cell::new(0));
        {
            let looked = Rc::clone(&looked);
            every_tick(move || {
                looked.set(looked.get() + 1);
                gtk::glib::ControlFlow::Continue
            });
            let hooked = Rc::clone(&hooked);
            on_nudge(move || hooked.set(hooked.get() + 1));
        }
        // Nothing yet: the wait is seconds long.
        while context.iteration(false) {}
        assert_eq!(looked.get(), 0);
        nudge_all();
        std::thread::sleep(std::time::Duration::from_millis(20));
        while context.iteration(false) {}
        assert_eq!(looked.get(), 1, "looked at once");
        assert_eq!(hooked.get(), 1, "the other timers were told");
        // And waits again: a second nudge looks again, once.
        nudge_all();
        std::thread::sleep(std::time::Duration::from_millis(20));
        while context.iteration(false) {}
        assert_eq!(looked.get(), 2);
    }

    #[test]
    fn the_tick_is_slow_only_with_nothing_shown_and_nobody_reachable() {
        assert_eq!(tick_for(true, true), 1);
        assert_eq!(tick_for(true, false), 1, "a window shows its clock");
        assert_eq!(tick_for(false, true), 1, "a phone is live");
        assert_eq!(tick_for(false, false), IDLE_TICK_SECONDS);
        const { assert!(IDLE_TICK_SECONDS > 1) };
    }
}
