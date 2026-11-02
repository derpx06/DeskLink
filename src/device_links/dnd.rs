//! Do Not Disturb shared with a phone (W14), off by default, per phone.
//!
//! The phone's Do Not Disturb (reported in its `desklink.battery` packet while
//! the desk's switch for it is on) is mapped to GNOME's
//! `org.gnome.desktop.notifications show-banners`; a change the person makes to
//! that setting is reported back (the desk's `desklink.settings` packet) for
//! the phone to apply when it may. Loop-safe: DeskLink remembers what it wrote
//! and what it last told the phone, so neither is taken for a change by the
//! other side, and a phone's report is acted on only when it changes.
//!
//! What was there before DeskLink changed it is put back when the sharing
//! ends, the phone has been gone for 60 s, or DeskLink quits (and after a
//! crash, at the next start) - but only if the setting still holds what
//! DeskLink wrote: a person who changed it meanwhile keeps their choice.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A phone out of the feature-ready state this long no longer counts.
pub(crate) const GONE_AFTER: Duration = Duration::from_secs(60);

/// The desktop's banner switch (`true` = banners shown = not Do Not Disturb).
pub(crate) trait Banners {
    /// `None` when the desktop has no such setting.
    fn shown(&self) -> Option<bool>;
    fn set_shown(&mut self, shown: bool);
}

#[derive(Debug, Default, Clone, Copy)]
struct Phone {
    /// Its Do Not Disturb, as last reported.
    dnd: Option<bool>,
    can_set: bool,
    unready_since: Option<Instant>,
}

/// What a look asks the caller to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// This desk's Do Not Disturb changed (by the person, or restored): tell
    /// the phones.
    pub(crate) desk_changed: Option<bool>,
    /// What to remember on disk so a crash is repaired: `Some(previous)` after
    /// DeskLink first changed the setting, `None` once it is put back.
    pub(crate) persist: Persist,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) enum Persist {
    #[default]
    Nothing,
    Save {
        previous_shown: bool,
        written_shown: bool,
    },
    Clear,
}

#[derive(Default)]
pub(crate) struct Engine {
    phones: HashMap<String, Phone>,
    /// The banners value from before DeskLink first changed it.
    previous: Option<bool>,
    /// The banners value DeskLink last wrote and still owns.
    written: Option<bool>,
    /// The phones' Do Not Disturb as last applied.
    applied: bool,
    /// The desk's Do Not Disturb as last told to (or learnt from) the phones.
    told: Option<bool>,
}

impl Engine {
    /// A phone's report. `None` for `dnd`: the phone sent none.
    pub(crate) fn on_phone(&mut self, id: &str, dnd: Option<bool>, can_set: bool) {
        let phone = self.phones.entry(id.to_string()).or_default();
        phone.dnd = dnd.or(phone.dnd);
        phone.can_set = can_set;
    }

    /// One look. `shared` are the phones with the switch on, `ready` those
    /// feature-ready now.
    pub(crate) fn look(
        &mut self,
        now: Instant,
        shared: &HashSet<String>,
        ready: &HashSet<String>,
        banners: &mut dyn Banners,
    ) -> Outcome {
        let mut outcome = Outcome::default();
        let Some(shown) = banners.shown() else {
            return outcome;
        };
        self.phones.retain(|id, _| shared.contains(id));
        for (id, phone) in &mut self.phones {
            phone.unready_since = if ready.contains(id) {
                None
            } else {
                Some(phone.unready_since.unwrap_or(now))
            };
        }
        // The person changed what DeskLink wrote: it is theirs now, and the
        // phones hear it.
        if let Some(written) = self.written {
            if shown != written {
                self.written = None;
                self.previous = None;
                outcome.persist = Persist::Clear;
            }
        }
        let want = self.phones.values().any(|phone| {
            phone.dnd == Some(true)
                && phone
                    .unready_since
                    .is_none_or(|since| now.saturating_duration_since(since) < GONE_AFTER)
        });
        let shown = banners.shown().unwrap_or(shown);
        if want != self.applied {
            self.applied = want;
            if want {
                // Already in Do Not Disturb (the person's own): nothing to
                // change, and nothing of ours to put back.
                if shown {
                    if self.written.is_none() {
                        self.previous = Some(shown);
                    }
                    banners.set_shown(false);
                    self.written = Some(false);
                    outcome.persist = Persist::Save {
                        previous_shown: self.previous.unwrap_or(true),
                        written_shown: false,
                    };
                }
            } else if self.written.is_some() {
                banners.set_shown(self.previous.unwrap_or(true));
                self.written = None;
                self.previous = None;
                outcome.persist = Persist::Clear;
            }
            // The phones asked for this: it is not news to them.
            self.told = banners.shown().map(|shown| !shown);
        }
        // What the phones should be told about this desk: only a difference
        // from what they were last told, and never our own write.
        let now_dnd = !banners.shown().unwrap_or(true);
        if self.told != Some(now_dnd) {
            let first = self.told.is_none();
            self.told = Some(now_dnd);
            // Telling the phones the very first state is not a change.
            if !first {
                outcome.desk_changed = Some(now_dnd);
            }
        }
        outcome
    }

    /// DeskLink quits: what it changed is put back (if still as it left it).
    pub(crate) fn quit(&mut self, banners: &mut dyn Banners) -> Persist {
        let Some(written) = self.written.take() else {
            return Persist::Nothing;
        };
        if banners.shown() == Some(written) {
            banners.set_shown(self.previous.take().unwrap_or(true));
        }
        self.applied = false;
        Persist::Clear
    }
}

/// After a crash: put back what an earlier run changed, if it is still as it
/// left it.
pub(crate) fn repair(banners: &mut dyn Banners, previous_shown: bool, written_shown: bool) -> bool {
    if banners.shown() == Some(written_shown) {
        banners.set_shown(previous_shown);
        true
    } else {
        false
    }
}

// ── The desktop and the process-wide engine ────────────────────────────────

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);
static DESK: Mutex<Option<bool>> = Mutex::new(None);

fn with<R>(use_engine: impl FnOnce(&mut Engine) -> R) -> Option<R> {
    let mut engine = ENGINE.lock().ok()?;
    Some(use_engine(engine.get_or_insert_with(Engine::default)))
}

/// A phone's `desklink.battery` carried `dnd` / `dndCanSet`.
pub(crate) fn on_phone(id: &str, dnd: Option<bool>, can_set: bool) {
    with(|engine| engine.on_phone(id, dnd, can_set));
}

pub(crate) fn forget(id: &str) {
    with(|engine| {
        engine.phones.remove(id);
    });
}

/// The desk's Do Not Disturb for the settings packet; `None` where the
/// desktop has no such setting (nothing is promised then).
pub(crate) fn desk_state() -> Option<bool> {
    DESK.lock().ok().and_then(|state| *state)
}

#[cfg(test)]
pub(crate) fn set_desk_state_for_tests(state: Option<bool>) {
    if let Ok(mut desk) = DESK.lock() {
        *desk = state;
    }
}

const SCHEMA: &str = "org.gnome.desktop.notifications";
const KEY: &str = "show-banners";

/// GNOME's `show-banners`.
pub(crate) struct GnomeBanners(gtk::gio::Settings);

impl GnomeBanners {
    /// `None` on a desktop without the schema (or the key).
    pub(crate) fn open() -> Option<Self> {
        let source = gtk::gio::SettingsSchemaSource::default()?;
        let schema = source.lookup(SCHEMA, true)?;
        schema
            .has_key(KEY)
            .then(|| Self(gtk::gio::Settings::new(SCHEMA)))
    }

    /// Calls `changed` (on the main loop) whenever the setting changes.
    pub(crate) fn watch(&self, changed: impl Fn() + 'static) {
        use gtk::gio::prelude::*;
        self.0.connect_changed(Some(KEY), move |_, _| changed());
    }
}

impl Banners for GnomeBanners {
    fn shown(&self) -> Option<bool> {
        use gtk::gio::prelude::*;
        Some(self.0.boolean(KEY))
    }

    fn set_shown(&mut self, shown: bool) {
        use gtk::gio::prelude::*;
        let _ = self.0.set_boolean(KEY, shown);
    }
}

/// One look from the main loop's ticker: `shared` and `ready` as in
/// [`Engine::look`]. Returns the desk's new Do Not Disturb when it changed.
pub(crate) fn look(
    shared: &HashSet<String>,
    ready: &HashSet<String>,
    banners: &mut dyn Banners,
) -> Option<bool> {
    let outcome = with(|engine| engine.look(Instant::now(), shared, ready, banners))?;
    if let Ok(mut desk) = DESK.lock() {
        *desk = banners.shown().map(|shown| !shown);
    }
    save(&outcome.persist);
    outcome.desk_changed
}

/// DeskLink is quitting.
pub(crate) fn quit(banners: &mut dyn Banners) {
    if let Some(persist) = with(|engine| engine.quit(banners)) {
        save(&persist);
    }
}

thread_local! {
    /// The desktop's banner switch, opened once on the main thread; the inner
    /// `None` is a desktop without one.
    static GNOME: std::cell::RefCell<Option<Option<GnomeBanners>>> =
        const { std::cell::RefCell::new(None) };
}

fn with_gnome<R>(use_banners: impl FnOnce(&mut dyn Banners) -> R) -> Option<R> {
    GNOME.with(|cell| {
        let mut cell = cell.borrow_mut();
        let opened = cell.get_or_insert_with(|| {
            let mut banners = GnomeBanners::open()?;
            // What an earlier run that did not quit cleanly left changed.
            repair_after_crash(&mut banners);
            if let Ok(mut desk) = DESK.lock() {
                *desk = banners.shown().map(|shown| !shown);
            }
            // The person turning Do Not Disturb on or off at the desk is
            // seen at once, not at the next tick.
            banners.watch(crate::ticker::nudge_all);
            Some(banners)
        });
        opened.as_mut().map(|banners| use_banners(banners))
    })
}

/// Opens the desktop's setting (main thread, at start).
pub(crate) fn init() {
    with_gnome(|_| ());
}

/// One look from the main loop's ticker, with the phones that share and the
/// ones that are ready. Tells the phones when the desk's own state changed.
pub(crate) fn step(shared: &HashSet<String>, ready: &HashSet<String>) {
    if with_gnome(|banners| look(shared, ready, banners))
        .flatten()
        .is_some()
    {
        crate::device_links::desk_features::push_settings();
    }
}

/// DeskLink is quitting (main thread).
pub(crate) fn quit_main() {
    with_gnome(|banners| quit(banners));
}

fn path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return None;
    }
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("dnd-restore.json"),
    )
}

fn save(persist: &Persist) {
    let Some(path) = path() else {
        return;
    };
    match persist {
        Persist::Nothing => {}
        Persist::Clear => {
            let _ = std::fs::remove_file(&path);
        }
        Persist::Save {
            previous_shown,
            written_shown,
        } => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let bytes =
                serde_json::json!({"previousShown": previous_shown, "writtenShown": written_shown});
            let _ = crate::device_links::config::write_private(&path, bytes.to_string().as_bytes());
        }
    }
}

/// At start: repairs what a run that did not quit cleanly left changed.
pub(crate) fn repair_after_crash(banners: &mut dyn Banners) {
    let Some(path) = path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let _ = std::fs::remove_file(&path);
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
        if let (Some(previous), Some(written)) = (
            value["previousShown"].as_bool(),
            value["writtenShown"].as_bool(),
        ) {
            repair(banners, previous, written);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        shown: Option<bool>,
        writes: Vec<bool>,
    }
    impl Banners for Fake {
        fn shown(&self) -> Option<bool> {
            self.shown
        }
        fn set_shown(&mut self, shown: bool) {
            self.shown = Some(shown);
            self.writes.push(shown);
        }
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn fake(shown: bool) -> Fake {
        Fake {
            shown: Some(shown),
            writes: Vec::new(),
        }
    }

    #[test]
    fn the_phones_do_not_disturb_turns_banners_off_and_back_when_it_ends() {
        let (mut engine, mut banners, t0) = (Engine::default(), fake(true), Instant::now());
        let both = set(&["p"]);
        assert_eq!(
            engine.look(t0, &both, &both, &mut banners),
            Outcome::default()
        );
        engine.on_phone("p", Some(true), true);
        let out = engine.look(t0, &both, &both, &mut banners);
        assert_eq!(banners.shown, Some(false));
        assert_eq!(
            out.persist,
            Persist::Save {
                previous_shown: true,
                written_shown: false
            }
        );
        assert_eq!(
            out.desk_changed, None,
            "our own write is not news to the phone"
        );
        // Nothing more happens while nothing changes.
        assert_eq!(
            engine.look(t0, &both, &both, &mut banners),
            Outcome::default()
        );
        assert_eq!(banners.writes, [false]);
        engine.on_phone("p", Some(false), true);
        let out = engine.look(t0, &both, &both, &mut banners);
        assert_eq!(banners.shown, Some(true), "put back");
        assert_eq!(out.persist, Persist::Clear);
        assert_eq!(out.desk_changed, None);
    }

    #[test]
    fn what_was_there_before_comes_back_even_if_it_was_already_do_not_disturb() {
        let (mut engine, mut banners, t0) = (Engine::default(), fake(false), Instant::now());
        let p = set(&["p"]);
        engine.look(t0, &p, &p, &mut banners);
        engine.on_phone("p", Some(true), true);
        engine.look(t0, &p, &p, &mut banners);
        engine.on_phone("p", Some(false), true);
        engine.look(t0, &p, &p, &mut banners);
        assert_eq!(banners.shown, Some(false), "the person had it on");
    }

    #[test]
    fn a_person_who_changes_it_meanwhile_keeps_their_choice_and_the_phone_hears_it() {
        let (mut engine, mut banners, t0) = (Engine::default(), fake(true), Instant::now());
        let p = set(&["p"]);
        engine.look(t0, &p, &p, &mut banners);
        engine.on_phone("p", Some(true), true);
        engine.look(t0, &p, &p, &mut banners);
        // The person turns banners back on at the desk.
        banners.shown = Some(true);
        let out = engine.look(t0, &p, &p, &mut banners);
        assert_eq!(out.desk_changed, Some(false), "the phone is told");
        assert_eq!(out.persist, Persist::Clear);
        assert_eq!(banners.writes, [false], "and DeskLink does not fight back");
        // The phone ending its Do Not Disturb later does not touch it either.
        engine.on_phone("p", Some(false), true);
        engine.look(t0, &p, &p, &mut banners);
        assert_eq!(banners.writes, [false]);
        assert_eq!(banners.shown, Some(true));
    }

    #[test]
    fn a_desk_do_not_disturb_is_reported_once_and_never_echoed() {
        let (mut engine, mut banners, t0) = (Engine::default(), fake(true), Instant::now());
        let p = set(&["p"]);
        assert_eq!(
            engine.look(t0, &p, &p, &mut banners).desk_changed,
            None,
            "first state"
        );
        banners.shown = Some(false);
        assert_eq!(
            engine.look(t0, &p, &p, &mut banners).desk_changed,
            Some(true)
        );
        assert_eq!(
            engine.look(t0, &p, &p, &mut banners).desk_changed,
            None,
            "once"
        );
        // The phone then reports the same state back: nothing is written.
        engine.on_phone("p", Some(true), true);
        engine.look(t0, &p, &p, &mut banners);
        assert!(banners.writes.is_empty());
    }

    #[test]
    fn a_phone_gone_for_a_minute_or_unshared_puts_it_back() {
        let (mut engine, mut banners, t0) = (Engine::default(), fake(true), Instant::now());
        let p = set(&["p"]);
        engine.look(t0, &p, &p, &mut banners);
        engine.on_phone("p", Some(true), false);
        engine.look(t0, &p, &p, &mut banners);
        assert_eq!(banners.shown, Some(false));
        // Not ready: held for 59 s, put back at 60 s.
        let none = set(&[]);
        engine.look(t0, &p, &none, &mut banners);
        engine.look(t0 + Duration::from_secs(59), &p, &none, &mut banners);
        assert_eq!(banners.shown, Some(false));
        engine.look(t0 + Duration::from_secs(60), &p, &none, &mut banners);
        assert_eq!(banners.shown, Some(true));
        // It comes back still in Do Not Disturb: applied again.
        engine.look(t0 + Duration::from_secs(61), &p, &p, &mut banners);
        assert_eq!(banners.shown, Some(false));
        // The switch is turned off: put back at once.
        engine.look(t0 + Duration::from_secs(62), &set(&[]), &p, &mut banners);
        assert_eq!(banners.shown, Some(true));
    }

    #[test]
    fn quitting_and_a_crash_put_it_back_only_if_it_is_as_we_left_it() {
        let (mut engine, mut banners, t0) = (Engine::default(), fake(true), Instant::now());
        let p = set(&["p"]);
        engine.look(t0, &p, &p, &mut banners);
        engine.on_phone("p", Some(true), true);
        engine.look(t0, &p, &p, &mut banners);
        assert_eq!(engine.quit(&mut banners), Persist::Clear);
        assert_eq!(banners.shown, Some(true));
        // A crash: the next start repairs only an untouched setting.
        let mut left = fake(false);
        assert!(repair(&mut left, true, false));
        assert_eq!(left.shown, Some(true));
        let mut changed = fake(true);
        assert!(!repair(&mut changed, true, false), "the person changed it");
        assert!(changed.writes.is_empty());
    }

    #[test]
    fn a_desktop_without_the_setting_is_left_alone() {
        let (mut engine, t0) = (Engine::default(), Instant::now());
        let mut none = Fake {
            shown: None,
            writes: Vec::new(),
        };
        let p = set(&["p"]);
        engine.on_phone("p", Some(true), true);
        assert_eq!(engine.look(t0, &p, &p, &mut none), Outcome::default());
        assert!(none.writes.is_empty());
    }

    #[test]
    fn any_sharing_phone_in_do_not_disturb_wins() {
        let (mut engine, mut banners, t0) = (Engine::default(), fake(true), Instant::now());
        let both = set(&["a", "b"]);
        engine.look(t0, &both, &both, &mut banners);
        engine.on_phone("a", Some(true), true);
        engine.on_phone("b", Some(false), true);
        engine.look(t0, &both, &both, &mut banners);
        assert_eq!(banners.shown, Some(false));
        engine.on_phone("a", Some(false), true);
        engine.look(t0, &both, &both, &mut banners);
        assert_eq!(banners.shown, Some(true));
    }
}
