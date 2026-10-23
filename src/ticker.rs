//! The main loop's periodic look at the daemon, at the pace the moment needs:
//! every second while a window shows or a phone is reachable, every five
//! seconds when there is nothing to show and nobody to talk to. A change in
//! who is reachable wakes the window at once (see `daemon::state::wake_ui`),
//! so the slow tick is only a safety net.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::glib;

use crate::device_links::daemon::tick_seconds;

/// Runs `run` on the main loop at the daemon's pace until it says
/// [`glib::ControlFlow::Break`].
pub(crate) fn every_tick(run: impl FnMut() -> glib::ControlFlow + 'static) {
    arm(Rc::new(RefCell::new(run)));
}

fn arm(run: Rc<RefCell<dyn FnMut() -> glib::ControlFlow>>) {
    glib::timeout_add_seconds_local_once(tick_seconds(), move || {
        let flow = (run.borrow_mut())();
        if flow == glib::ControlFlow::Continue {
            arm(run);
        }
    });
}

#[cfg(test)]
mod tests {
    use crate::device_links::daemon::{tick_for, IDLE_TICK_SECONDS};

    #[test]
    fn the_tick_is_slow_only_with_nothing_shown_and_nobody_reachable() {
        assert_eq!(tick_for(true, true), 1);
        assert_eq!(tick_for(true, false), 1, "a window shows its clock");
        assert_eq!(tick_for(false, true), 1, "a phone is live");
        assert_eq!(tick_for(false, false), IDLE_TICK_SECONDS);
        const { assert!(IDLE_TICK_SECONDS > 1) };
    }
}
