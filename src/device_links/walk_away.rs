//! Walk-away lock: lock this computer when a paired phone that was connected
//! has been out of reach for a minute. Off unless turned on, per phone
//! (Settings > Privacy & security).
//!
//! Only a phone that was feature-ready can walk away, and only when *nothing*
//! reaches it any more (the kernel's keepalive has given the link up). It is
//! never judged by silence. Every choice here errs towards *not* locking:
//!
//! - A phone that reported `dozing` (screen off: the usual way a phone leaves
//!   in a pocket) is counted too, but for [`DOZING_LOCK_AFTER`] instead of a
//!   minute: its link is only given up after the 90 s a dozing phone is
//!   allowed on the LAN link, so a dead link is already positive evidence, and
//!   the longer count covers a phone whose Wi-Fi sleeps at the desk.
//! - A link through the relay is a link: leaving the LAN with the relay on
//!   does not lock (the phone is still in touch). Only a phone nothing reaches
//!   is away.
//! - While this computer's own network is down, or has just changed, or the
//!   computer has just woken, nothing is counted ([`reset`]).
//! - Any sign of the phone (an authenticated link back) ends the absence. A
//!   beacon is not a sign here: anyone on the network can send one, and ending
//!   the count is what keeps the computer unlocked.
//!
//! Ten seconds before the lock a notification says so and offers Cancel. The
//! computer is only ever locked, never unlocked, and once per absence. With
//! two phones switched on, either one walking away locks it.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Out of reach this long, a warning shows.
pub(crate) const WARN_AFTER: Duration = Duration::from_secs(50);
/// The warning stays up this long before the lock.
pub(crate) const WARN_FOR: Duration = Duration::from_secs(10);
/// Out of reach this long (and warned for [`WARN_FOR`]), the computer locks.
pub(crate) const LOCK_AFTER: Duration = Duration::from_secs(60);
/// The same for a phone whose last heartbeat said it was dozing.
pub(crate) const DOZING_WARN_AFTER: Duration = Duration::from_secs(170);
pub(crate) const DOZING_LOCK_AFTER: Duration = Duration::from_secs(180);

/// What is known of one phone right now.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Seen {
    /// The person turned the walk-away lock on for this phone.
    pub(crate) enabled: bool,
    /// An authenticated link to it is up (LAN or relay).
    pub(crate) reachable: bool,
    /// Its encrypted connection was fully up (features could flow).
    pub(crate) ready: bool,
    /// Its last heartbeat said it is dozing (screen off, frozen).
    pub(crate) dozing: bool,
    /// This computer has a network of its own right now. Without one nothing
    /// says the phone left.
    pub(crate) network_up: bool,
}

impl Seen {
    /// What the daemon's view of a phone says, with the person's setting, the
    /// phone's last doze report and this computer's network. Any link counts
    /// as reachable (the relay too).
    pub(crate) fn of(
        device: &crate::device_links::device::DeviceView,
        enabled: bool,
        dozing: bool,
        network_up: bool,
    ) -> Seen {
        Seen {
            enabled,
            reachable: matches!(
                device.status,
                crate::device_links::device::DeviceStatus::Paired
            ),
            ready: device.feature_transport_ready,
            dozing,
            network_up,
        }
    }
}

/// Whether the network monitor's answer is a network: `Local` is none.
pub(crate) fn network_is_up(connectivity: gtk::gio::NetworkConnectivity) -> bool {
    connectivity != gtk::gio::NetworkConnectivity::Local
}

/// What to do now about one phone.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// Tell the person the computer is about to lock, with Cancel.
    Warn,
    /// Take the warning down: the phone is back, or it was cancelled.
    ClearWarning,
    /// Lock the computer.
    Lock,
}

#[derive(Default)]
struct Phone {
    /// It was feature-ready since the count last had nothing to count.
    armed: bool,
    away_since: Option<Instant>,
    warned_at: Option<Instant>,
    /// Locked, or cancelled, for this absence: nothing more until it ends.
    done: bool,
    /// A reset took the warning back: the next look says to clear it.
    clear_pending: bool,
}

#[derive(Default)]
pub(crate) struct Watch {
    phones: HashMap<String, Phone>,
}

impl Watch {
    /// [`Self::observe_when_idle`] for a desk that is idle.
    #[cfg(test)]
    pub(crate) fn observe(&mut self, id: &str, seen: Seen, now: Instant) -> Option<Step> {
        self.observe_when_idle(id, seen, now, &mut || true)
    }

    /// One look at `id` at `now` (the daemon looks every few seconds). `idle`
    /// says whether nobody is using this computer (logind's IdleHint); it is
    /// asked only when a count that began with the phone dozing is about to
    /// warn or lock: a phone that went to sleep on the desk, its radio with
    /// it, must not lock the computer on the person using it.
    pub(crate) fn observe_when_idle(
        &mut self,
        id: &str,
        seen: Seen,
        now: Instant,
        idle: &mut dyn FnMut() -> bool,
    ) -> Option<Step> {
        if !seen.enabled {
            return self.phones.remove(id).and_then(|phone| clears(&phone));
        }
        let phone = self.phones.entry(id.to_string()).or_default();
        if std::mem::take(&mut phone.clear_pending) {
            return Some(Step::ClearWarning);
        }
        if seen.reachable {
            // A sign of the phone: the absence, if any, is over.
            let step = clears(phone);
            *phone = Phone {
                armed: phone.armed || seen.ready,
                ..Phone::default()
            };
            return step;
        }
        if !phone.armed || phone.done {
            return None;
        }
        if !seen.network_up {
            // Our own network is gone: the phone did not leave, we did.
            let step = clears(phone);
            phone.away_since = None;
            phone.warned_at = None;
            return step;
        }
        let (warn_after, lock_after) = if seen.dozing {
            (DOZING_WARN_AFTER, DOZING_LOCK_AFTER)
        } else {
            (WARN_AFTER, LOCK_AFTER)
        };
        let since = *phone.away_since.get_or_insert(now);
        let away = now.saturating_duration_since(since);
        match phone.warned_at {
            None if away >= warn_after => {
                // A dozing phone counts only while the desk is idle.
                if seen.dozing && !idle() {
                    return None;
                }
                phone.warned_at = Some(now);
                Some(Step::Warn)
            }
            Some(warned)
                if away >= lock_after && now.saturating_duration_since(warned) >= WARN_FOR =>
            {
                if seen.dozing && !idle() {
                    // Someone is at the keyboard after all: the warning goes
                    // and the count starts again.
                    phone.warned_at = None;
                    phone.away_since = Some(now);
                    return Some(Step::ClearWarning);
                }
                phone.warned_at = None;
                phone.done = true;
                Some(Step::Lock)
            }
            _ => None,
        }
    }

    /// The person chose Cancel: no lock for this absence. `true` when a
    /// warning was showing.
    pub(crate) fn cancel(&mut self, id: &str) -> bool {
        // A Cancel with no warning up (a stale click) changes nothing.
        let Some(phone) = self.phones.get_mut(id).filter(|p| p.warned_at.is_some()) else {
            return false;
        };
        phone.warned_at = None;
        phone.done = true;
        true
    }

    /// A network change, or the computer woke: every count starts again,
    /// and a warning showing comes down at the next look.
    pub(crate) fn reset(&mut self) {
        for phone in self.phones.values_mut() {
            phone.clear_pending |= phone.warned_at.take().is_some();
            phone.away_since = None;
        }
    }

    pub(crate) fn forget(&mut self, id: &str) {
        self.phones.remove(id);
    }
}

fn clears(phone: &Phone) -> Option<Step> {
    phone.warned_at.is_some().then_some(Step::ClearWarning)
}

static WATCH: Mutex<Option<Watch>> = Mutex::new(None);

fn with<R>(use_watch: impl FnOnce(&mut Watch) -> R) -> Option<R> {
    let mut watch = WATCH.lock().ok()?;
    Some(use_watch(watch.get_or_insert_with(Watch::default)))
}

pub(crate) fn observe(id: &str, seen: Seen) -> Option<Step> {
    with(|watch| {
        watch.observe_when_idle(id, seen, Instant::now(), &mut || {
            crate::device_links::logind::idle_hint().unwrap_or(false)
        })
    })
    .flatten()
}

pub(crate) fn cancel(id: &str) -> bool {
    with(|watch| watch.cancel(id)).unwrap_or(false)
}

/// See [`Watch::reset`].
pub(crate) fn reset() {
    with(|watch| watch.reset());
}

pub(crate) fn forget(id: &str) {
    with(|watch| watch.forget(id));
}

#[cfg(test)]
mod tests {
    use super::*;

    const AWAY: Seen = Seen {
        enabled: true,
        reachable: false,
        ready: false,
        dozing: false,
        network_up: true,
    };
    const HERE: Seen = Seen {
        enabled: true,
        reachable: true,
        ready: true,
        dozing: false,
        network_up: true,
    };

    /// A fake clock: seconds after a fixed start.
    struct Clock(Instant);
    impl Clock {
        fn at(&self, seconds: u64) -> Instant {
            self.0 + Duration::from_secs(seconds)
        }
    }

    fn clock() -> Clock {
        Clock(Instant::now())
    }

    /// Looks once a second from `from` to `to` (inclusive); every step taken.
    fn run(watch: &mut Watch, clock: &Clock, seen: Seen, from: u64, to: u64) -> Vec<(u64, Step)> {
        (from..=to)
            .filter_map(|second| {
                watch
                    .observe("p", seen, clock.at(second))
                    .map(|step| (second, step))
            })
            .collect()
    }

    #[test]
    fn it_warns_at_fifty_seconds_and_locks_at_sixty() {
        let (mut watch, clock) = (Watch::default(), clock());
        assert_eq!(run(&mut watch, &clock, HERE, 0, 5), []);
        // Gone from second 6: the count starts at the first look without it.
        let steps = run(&mut watch, &clock, AWAY, 6, 90);
        assert_eq!(steps, [(56, Step::Warn), (66, Step::Lock)]);
    }

    #[test]
    fn a_coarse_look_still_gives_ten_seconds_of_warning() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        assert_eq!(watch.observe("p", AWAY, clock.at(1)), None);
        // The next look is far later: the warning first, then ten seconds.
        assert_eq!(watch.observe("p", AWAY, clock.at(100)), Some(Step::Warn));
        assert_eq!(watch.observe("p", AWAY, clock.at(105)), None);
        assert_eq!(watch.observe("p", AWAY, clock.at(110)), Some(Step::Lock));
    }

    /// F1: a phone that went dozing in a pocket and left is counted, for three
    /// minutes instead of one (its link was already given up after the dozing
    /// allowance); a dozing phone whose link is still up never counts.
    #[test]
    fn a_dozing_phone_that_left_locks_after_three_minutes() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        let dozing = Seen {
            dozing: true,
            ..AWAY
        };
        assert_eq!(
            run(&mut watch, &clock, dozing, 1, 400),
            [(171, Step::Warn), (181, Step::Lock)]
        );
        // Still linked and dozing (it answers the LAN heartbeat): never.
        let (mut watch, clock) = (Watch::default(), Clock(Instant::now()));
        let linked = Seen {
            dozing: true,
            ..HERE
        };
        assert_eq!(run(&mut watch, &clock, linked, 0, 1000), []);
    }

    /// A phone that fell asleep on the desk with its radio off does not lock
    /// the computer on the person using it: a dozing count warns and locks
    /// only while the desk is idle (logind's IdleHint), and an unknown idle
    /// state is not idle.
    #[test]
    fn a_dozing_phone_never_locks_a_desk_that_is_in_use() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        let dozing = Seen {
            dozing: true,
            ..AWAY
        };
        let mut busy = || false;
        let mut at = |watch: &mut Watch, second, idle: &mut dyn FnMut() -> bool| {
            watch.observe_when_idle("p", dozing, clock.at(second), idle)
        };
        // Typing the whole time: not even a warning.
        for second in 1..=400 {
            assert_eq!(at(&mut watch, second, &mut busy), None);
        }
        // The person stops: the warning, then the lock, ten seconds on.
        let mut idle = || true;
        assert_eq!(at(&mut watch, 401, &mut idle), Some(Step::Warn));
        // Back at the keyboard during the warning: it goes, the count restarts.
        assert_eq!(at(&mut watch, 405, &mut busy), None);
        assert_eq!(at(&mut watch, 411, &mut busy), Some(Step::ClearWarning));
        assert_eq!(at(&mut watch, 412, &mut idle), None, "a fresh count");
        assert_eq!(at(&mut watch, 411 + 170, &mut idle), Some(Step::Warn));
        assert_eq!(at(&mut watch, 411 + 180, &mut idle), Some(Step::Lock));
        // An awake phone's minute does not depend on idleness.
        let (mut awake, clock2) = (Watch::default(), Clock(Instant::now()));
        run(&mut awake, &clock2, HERE, 0, 0);
        assert_eq!(
            awake.observe_when_idle("p", AWAY, clock2.at(1), &mut busy),
            None
        );
        assert_eq!(
            awake.observe_when_idle("p", AWAY, clock2.at(51), &mut busy),
            Some(Step::Warn)
        );
    }

    /// Waking ends a doze: the count goes on with the ordinary minute from
    /// when it began.
    #[test]
    fn a_phone_that_stops_dozing_while_gone_is_counted_as_awake() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        let dozing = Seen {
            dozing: true,
            ..AWAY
        };
        assert_eq!(run(&mut watch, &clock, dozing, 1, 100), []);
        // At 101 it reports awake (it is back on a network somewhere, but not
        // reachable): 100 s already elapsed, so the minute is long over.
        assert_eq!(watch.observe("p", AWAY, clock.at(101)), Some(Step::Warn));
        assert_eq!(watch.observe("p", AWAY, clock.at(111)), Some(Step::Lock));
    }

    /// F3: this computer's own network being down is not the phone leaving;
    /// nothing is counted, and the count starts fresh when it is back.
    #[test]
    fn nothing_is_counted_while_this_computers_network_is_down() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        let down = Seen {
            network_up: false,
            ..AWAY
        };
        assert_eq!(run(&mut watch, &clock, down, 1, 600), []);
        // Back: a fresh minute from the first look with the network up.
        assert_eq!(
            run(&mut watch, &clock, AWAY, 601, 700),
            [(651, Step::Warn), (661, Step::Lock)]
        );
        // A warning up when the network goes comes down.
        let (mut watch, clock) = (Watch::default(), Clock(Instant::now()));
        run(&mut watch, &clock, HERE, 0, 0);
        assert_eq!(run(&mut watch, &clock, AWAY, 1, 55), [(51, Step::Warn)]);
        assert_eq!(
            watch.observe("p", down, clock.at(56)),
            Some(Step::ClearWarning)
        );
    }

    /// F2: a link through the relay is still a link; leaving the LAN with the
    /// relay on is not walking away.
    #[test]
    fn a_relay_link_is_a_link() {
        use crate::device_links::device::{DeviceStatus, DeviceView};
        let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.connection_path = Some("relay".into());
        let seen = Seen::of(&device, true, false, true);
        assert!(seen.reachable && seen.ready);
        device.status = DeviceStatus::Unreachable;
        device.feature_transport_ready = false;
        assert!(!Seen::of(&device, true, false, true).reachable);
        assert!(network_is_up(gtk::gio::NetworkConnectivity::Full));
        assert!(network_is_up(gtk::gio::NetworkConnectivity::Limited));
        assert!(!network_is_up(gtk::gio::NetworkConnectivity::Local));
    }

    /// A stale Cancel click (no warning up) does not end the next absence.
    #[test]
    fn a_cancel_with_no_warning_up_changes_nothing() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        assert!(!watch.cancel("p"));
        assert_eq!(
            run(&mut watch, &clock, AWAY, 1, 80),
            [(51, Step::Warn), (61, Step::Lock)]
        );
    }

    #[test]
    fn a_phone_that_comes_back_takes_the_warning_down_and_starts_over() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        assert_eq!(run(&mut watch, &clock, AWAY, 1, 55), [(51, Step::Warn)]);
        assert_eq!(
            watch.observe("p", HERE, clock.at(56)),
            Some(Step::ClearWarning)
        );
        // Gone again: a full minute again, not the 55 s already spent.
        assert_eq!(
            run(&mut watch, &clock, AWAY, 57, 130),
            [(107, Step::Warn), (117, Step::Lock)]
        );
    }

    #[test]
    fn nothing_happens_with_the_setting_off_or_for_a_phone_never_ready() {
        let (mut watch, clock) = (Watch::default(), clock());
        let off = Seen {
            enabled: false,
            ..HERE
        };
        run(&mut watch, &clock, off, 0, 0);
        let off_away = Seen {
            enabled: false,
            ..AWAY
        };
        assert_eq!(run(&mut watch, &clock, off_away, 1, 600), []);
        // Turned on while it is already gone: it was not seen here, so it
        // cannot have walked away.
        assert_eq!(run(&mut watch, &clock, AWAY, 601, 1200), []);
        // Reachable but never feature-ready (a half-made link) is not seen.
        let half = Seen {
            ready: false,
            ..HERE
        };
        assert_eq!(run(&mut watch, &clock, half, 1201, 1201), []);
        assert_eq!(run(&mut watch, &clock, AWAY, 1202, 1800), []);
    }

    #[test]
    fn it_locks_once_per_absence() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        let steps = run(&mut watch, &clock, AWAY, 1, 3000);
        assert_eq!(steps, [(51, Step::Warn), (61, Step::Lock)]);
        // Back, then gone again: a new absence, locks once more.
        run(&mut watch, &clock, HERE, 3001, 3001);
        let again = run(&mut watch, &clock, AWAY, 3002, 3100);
        assert_eq!(again, [(3052, Step::Warn), (3062, Step::Lock)]);
    }

    #[test]
    fn cancel_keeps_the_computer_unlocked_for_that_absence() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        assert_eq!(run(&mut watch, &clock, AWAY, 1, 55), [(51, Step::Warn)]);
        assert!(watch.cancel("p"), "a warning was showing");
        assert_eq!(run(&mut watch, &clock, AWAY, 56, 3000), []);
        // The next absence counts again.
        run(&mut watch, &clock, HERE, 3001, 3001);
        assert_eq!(
            run(&mut watch, &clock, AWAY, 3002, 3070),
            [(3052, Step::Warn), (3062, Step::Lock)]
        );
        assert!(!watch.cancel("unknown"));
    }

    #[test]
    fn a_network_change_or_waking_starts_the_count_again() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        assert_eq!(run(&mut watch, &clock, AWAY, 1, 55), [(51, Step::Warn)]);
        watch.reset();
        assert_eq!(
            watch.observe("p", AWAY, clock.at(56)),
            Some(Step::ClearWarning),
            "the warning comes down"
        );
        // Not locked at 61 s: a fresh minute from 57.
        assert_eq!(
            run(&mut watch, &clock, AWAY, 57, 130),
            [(107, Step::Warn), (117, Step::Lock)]
        );
    }

    #[test]
    fn unpairing_forgets_the_phone() {
        let (mut watch, clock) = (Watch::default(), clock());
        run(&mut watch, &clock, HERE, 0, 0);
        watch.forget("p");
        assert_eq!(run(&mut watch, &clock, AWAY, 1, 600), []);
    }
}
