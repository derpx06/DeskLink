//! Do Not Disturb shared with a phone (W14), off by default, per phone.
//!
//! The phone's Do Not Disturb (reported in its `desklink.battery` packet while
//! the desk's switch for it is on) is mapped to GNOME's
//! `org.gnome.desktop.notifications show-banners`; a change the person makes to
//! that setting is reported back (the desk's `desklink.settings` packet) for
//! the phone to apply when it may. Loop-safe: DeskLink remembers what it wrote,
//! and a phone's report is acted on only when it changes.
//!
//! One phone owns the state at a time (the first to turn Do Not Disturb on);
//! the others are told the desk's state and follow it, and a follower's report
//! never keeps the desk in Do Not Disturb after its owner has ended it.
//!
//! What was there before DeskLink changed it is put back when the sharing
//! ends, the phone has been gone for 60 s, or DeskLink quits (and after a
//! crash, at the next start) - but only if the setting still holds what
//! DeskLink wrote: a person who changed it meanwhile keeps their choice. The
//! note for a crash is written *before* the setting is changed, and when it
//! cannot be written the setting is left alone.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A phone out of the feature-ready state this long no longer counts.
pub(crate) const GONE_AFTER: Duration = Duration::from_secs(60);

/// The desktop's banner switch (`true` = banners shown = not Do Not Disturb).
pub(crate) trait Banners {
    /// `None` when the desktop has no such setting.
    fn shown(&self) -> Option<bool>;
    /// Writes it and makes sure it has reached the settings store.
    fn set_shown(&mut self, shown: bool);
}

/// The note that lets the next start put a changed setting back.
pub(crate) trait Journal {
    fn save(&mut self, previous_shown: bool, written_shown: bool) -> Result<(), String>;
    fn clear(&mut self);
}

#[derive(Debug, Default, Clone, Copy)]
struct Phone {
    /// Its Do Not Disturb, as last reported.
    dnd: Option<bool>,
    unready_since: Option<Instant>,
    /// It is in Do Not Disturb because the desk said so, not on its own.
    follow: bool,
}

impl Phone {
    fn active(&self, now: Instant) -> bool {
        self.unready_since
            .is_none_or(|since| now.saturating_duration_since(since) < GONE_AFTER)
    }
}

/// What a look asks the caller to do.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    /// This desk's Do Not Disturb changed (by the person, or by DeskLink):
    /// tell the phones.
    pub(crate) desk_changed: Option<bool>,
    /// Something the person should be told (the setting was left alone).
    pub(crate) error: Option<String>,
}

#[derive(Default)]
pub(crate) struct Engine {
    phones: HashMap<String, Phone>,
    /// The phone whose Do Not Disturb the desk is following.
    owner: Option<String>,
    /// The banners value from before DeskLink first changed it.
    previous: Option<bool>,
    /// The banners value DeskLink last wrote and still owns.
    written: Option<bool>,
    /// Whether the desk is in Do Not Disturb on a phone's account.
    applied: bool,
    /// The desk's Do Not Disturb as last told to (or learnt from) the phones.
    told: Option<bool>,
}

impl Engine {
    /// A phone's report. `None` for `dnd`: the phone sent none.
    pub(crate) fn on_phone(&mut self, id: &str, dnd: Option<bool>) {
        let phone = self.phones.entry(id.to_string()).or_default();
        phone.dnd = dnd.or(phone.dnd);
        if dnd == Some(false) {
            phone.follow = false;
        }
    }

    /// One look. `shared` are the phones with the switch on, `ready` those
    /// feature-ready now.
    pub(crate) fn look(
        &mut self,
        now: Instant,
        shared: &HashSet<String>,
        ready: &HashSet<String>,
        banners: &mut dyn Banners,
        journal: &mut dyn Journal,
    ) -> Outcome {
        let mut outcome = Outcome::default();
        let Some(shown) = banners.shown() else {
            return outcome;
        };
        self.phones.retain(|id, _| shared.contains(id));
        // Every sharing phone is known, even one that has not reported yet:
        // it may be told to follow before it ever does.
        for id in shared {
            self.phones.entry(id.clone()).or_default();
        }
        for (id, phone) in &mut self.phones {
            if ready.contains(id) {
                // Back after being given up on: what it said then is stale
                // until it says it again.
                if phone
                    .unready_since
                    .take()
                    .is_some_and(|since| now.saturating_duration_since(since) >= GONE_AFTER)
                {
                    phone.dnd = None;
                    phone.follow = false;
                }
            } else {
                phone.unready_since.get_or_insert(now);
            }
        }
        // The person changed what DeskLink wrote: it is theirs now, and the
        // phones hear it.
        if self.written.is_some_and(|written| shown != written) {
            self.written = None;
            self.previous = None;
            journal.clear();
        }
        // The owner holds while it is there and in Do Not Disturb; else the
        // first phone that put itself there (not one following the desk).
        if self.owner.as_ref().is_some_and(|owner| {
            !self
                .phones
                .get(owner)
                .is_some_and(|p| p.active(now) && p.dnd == Some(true))
        }) {
            self.owner = None;
        }
        if self.owner.is_none() {
            let mut candidates: Vec<&String> = self
                .phones
                .iter()
                .filter(|(_, p)| p.active(now) && p.dnd == Some(true) && !p.follow)
                .map(|(id, _)| id)
                .collect();
            candidates.sort();
            self.owner = candidates.first().map(|id| (*id).clone());
        }
        let want = self.owner.is_some();
        let shown = banners.shown().unwrap_or(shown);
        let mut changed = false;
        if want != self.applied {
            self.applied = want;
            if want {
                let owner = self.owner.clone();
                for (id, phone) in &mut self.phones {
                    phone.follow = Some(id) != owner.as_ref();
                }
                // Already in Do Not Disturb (the person's own): nothing to
                // change, and nothing of ours to put back.
                if shown {
                    let previous = self.previous.unwrap_or(shown);
                    // The note first: without it a crash could leave banners
                    // off for good.
                    match journal.save(previous, false) {
                        Ok(()) => {
                            self.previous = Some(previous);
                            banners.set_shown(false);
                            self.written = Some(false);
                            changed = true;
                        }
                        Err(error) => {
                            outcome.error = Some(format!(
                                "Could not share Do Not Disturb (the safety note could not be \
                                 written: {error}); banners were left as they are"
                            ));
                        }
                    }
                }
            } else if self.written.is_some() {
                banners.set_shown(self.previous.unwrap_or(true));
                self.written = None;
                self.previous = None;
                journal.clear();
                changed = true;
            }
        }
        // What the phones should be told about this desk: what DeskLink just
        // did (the followers hear it too), or what the person did.
        let now_dnd = !banners.shown().unwrap_or(true);
        if self.told != Some(now_dnd) {
            let first = self.told.is_none();
            self.told = Some(now_dnd);
            // Telling the phones the very first state is not a change.
            if !first || changed {
                outcome.desk_changed = Some(now_dnd);
            }
        } else if changed {
            outcome.desk_changed = Some(now_dnd);
        }
        outcome
    }

    /// DeskLink quits: what it changed is put back (if still as it left it).
    pub(crate) fn quit(&mut self, banners: &mut dyn Banners, journal: &mut dyn Journal) {
        let Some(written) = self.written.take() else {
            return;
        };
        if banners.shown() == Some(written) {
            banners.set_shown(self.previous.take().unwrap_or(true));
        }
        self.applied = false;
        self.owner = None;
        journal.clear();
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

/// A phone's `desklink.battery` carried `dnd`.
pub(crate) fn on_phone(id: &str, dnd: Option<bool>) {
    with(|engine| engine.on_phone(id, dnd));
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
        // dconf writes are asynchronous: this one must have reached the store
        // before the process can end.
        gtk::gio::Settings::sync();
    }
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

/// The note on disk.
struct FileJournal;

impl Journal for FileJournal {
    fn save(&mut self, previous_shown: bool, written_shown: bool) -> Result<(), String> {
        let Some(path) = path() else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
        }
        let bytes =
            serde_json::json!({"previousShown": previous_shown, "writtenShown": written_shown});
        crate::device_links::config::write_private(&path, bytes.to_string().as_bytes())
    }

    fn clear(&mut self) {
        if let Some(path) = path() {
            let _ = std::fs::remove_file(path);
        }
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
/// ones that are ready. Tells the phones when the desk's own state changed;
/// returns something to tell the person, if any.
pub(crate) fn step(shared: &HashSet<String>, ready: &HashSet<String>) -> Option<String> {
    let outcome = with_gnome(|banners| {
        with(|engine| engine.look(Instant::now(), shared, ready, banners, &mut FileJournal))
    })
    .flatten()?;
    if let Ok(mut desk) = DESK.lock() {
        *desk = with_gnome(|banners| banners.shown().map(|shown| !shown)).flatten();
    }
    if outcome.desk_changed.is_some() {
        crate::device_links::desk_features::push_settings();
    }
    outcome.error
}

/// DeskLink is quitting (main thread): first thing, so a slow flush after it
/// cannot cost the restore.
pub(crate) fn quit_main() {
    with_gnome(|banners| {
        with(|engine| engine.quit(banners, &mut FileJournal));
    });
}

/// At start: repairs what a run that did not quit cleanly left changed.
pub(crate) fn repair_after_crash(banners: &mut dyn Banners) {
    let Some(path) = path() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
        if let (Some(previous), Some(written)) = (
            value["previousShown"].as_bool(),
            value["writtenShown"].as_bool(),
        ) {
            repair(banners, previous, written);
        }
    }
    // Only after the setting is back (set_shown has synced).
    let _ = std::fs::remove_file(&path);
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        shown: Option<bool>,
        writes: Vec<bool>,
        /// What the journal held when each write happened.
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }
    impl Banners for Fake {
        fn shown(&self) -> Option<bool> {
            self.shown
        }
        fn set_shown(&mut self, shown: bool) {
            self.shown = Some(shown);
            self.writes.push(shown);
            self.log.borrow_mut().push(format!("set {shown}"));
        }
    }

    struct Note {
        fail: bool,
        log: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    }
    impl Journal for Note {
        fn save(&mut self, previous: bool, written: bool) -> Result<(), String> {
            if self.fail {
                return Err("disk full".into());
            }
            self.log
                .borrow_mut()
                .push(format!("save {previous} {written}"));
            Ok(())
        }
        fn clear(&mut self) {
            self.log.borrow_mut().push("clear".into());
        }
    }

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|id| id.to_string()).collect()
    }

    fn rig(shown: bool) -> (Engine, Fake, Note) {
        let log: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
        (
            Engine::default(),
            Fake {
                shown: Some(shown),
                writes: Vec::new(),
                log: log.clone(),
            },
            Note { fail: false, log },
        )
    }

    #[test]
    fn the_phones_do_not_disturb_turns_banners_off_and_back_when_it_ends() {
        let (mut engine, mut banners, mut note) = rig(true);
        let (t0, both) = (Instant::now(), set(&["p"]));
        assert_eq!(
            engine.look(t0, &both, &both, &mut banners, &mut note),
            Outcome::default()
        );
        engine.on_phone("p", Some(true));
        let out = engine.look(t0, &both, &both, &mut banners, &mut note);
        assert_eq!(banners.shown, Some(false));
        assert_eq!(out.desk_changed, Some(true), "the followers hear it too");
        // The note is written before the setting changes.
        assert_eq!(*banners.log.borrow(), ["save true false", "set false"]);
        assert_eq!(
            engine.look(t0, &both, &both, &mut banners, &mut note),
            Outcome::default()
        );
        assert_eq!(banners.writes, [false]);
        engine.on_phone("p", Some(false));
        let out = engine.look(t0, &both, &both, &mut banners, &mut note);
        assert_eq!(banners.shown, Some(true), "put back");
        assert_eq!(out.desk_changed, Some(false));
        assert_eq!(
            banners.log.borrow()[2..],
            ["set true", "clear"],
            "the note goes after the setting is back"
        );
    }

    #[test]
    fn when_the_safety_note_cannot_be_written_the_setting_is_left_alone_and_the_person_told() {
        let (mut engine, mut banners, mut note) = rig(true);
        note.fail = true;
        let (t0, p) = (Instant::now(), set(&["p"]));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        engine.on_phone("p", Some(true));
        let out = engine.look(t0, &p, &p, &mut banners, &mut note);
        assert!(banners.writes.is_empty(), "not flipped");
        assert!(out.error.as_deref().unwrap().contains("disk full"));
        // Said once, not every tick.
        let again = engine.look(t0, &p, &p, &mut banners, &mut note);
        assert_eq!(again.error, None);
    }

    #[test]
    fn what_was_there_before_comes_back_even_if_it_was_already_do_not_disturb() {
        let (mut engine, mut banners, mut note) = rig(false);
        let (t0, p) = (Instant::now(), set(&["p"]));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        engine.on_phone("p", Some(true));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        engine.on_phone("p", Some(false));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        assert_eq!(banners.shown, Some(false), "the person had it on");
        assert!(banners.writes.is_empty());
    }

    #[test]
    fn a_person_who_changes_it_meanwhile_keeps_their_choice_and_the_phone_hears_it() {
        let (mut engine, mut banners, mut note) = rig(true);
        let (t0, p) = (Instant::now(), set(&["p"]));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        engine.on_phone("p", Some(true));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        banners.shown = Some(true);
        let out = engine.look(t0, &p, &p, &mut banners, &mut note);
        assert_eq!(out.desk_changed, Some(false), "the phone is told");
        assert_eq!(banners.writes, [false], "and DeskLink does not fight back");
        engine.on_phone("p", Some(false));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        assert_eq!(banners.writes, [false]);
        assert_eq!(banners.shown, Some(true));
    }

    #[test]
    fn a_desk_do_not_disturb_is_reported_once_and_never_echoed() {
        let (mut engine, mut banners, mut note) = rig(true);
        let (t0, p) = (Instant::now(), set(&["p"]));
        assert_eq!(
            engine
                .look(t0, &p, &p, &mut banners, &mut note)
                .desk_changed,
            None,
            "first state"
        );
        banners.shown = Some(false);
        assert_eq!(
            engine
                .look(t0, &p, &p, &mut banners, &mut note)
                .desk_changed,
            Some(true)
        );
        assert_eq!(
            engine
                .look(t0, &p, &p, &mut banners, &mut note)
                .desk_changed,
            None,
            "once"
        );
        // The phone then reports the same state back: nothing is written.
        engine.on_phone("p", Some(true));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        assert!(banners.writes.is_empty());
    }

    #[test]
    fn a_phone_gone_for_a_minute_or_unshared_puts_it_back() {
        let (mut engine, mut banners, mut note) = rig(true);
        let (t0, p, none) = (Instant::now(), set(&["p"]), set(&[]));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        engine.on_phone("p", Some(true));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        assert_eq!(banners.shown, Some(false));
        engine.look(t0, &p, &none, &mut banners, &mut note);
        engine.look(
            t0 + Duration::from_secs(59),
            &p,
            &none,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(false));
        engine.look(
            t0 + Duration::from_secs(60),
            &p,
            &none,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(true));
        // It comes back: what it said before it went is stale until it says
        // it again, so Do Not Disturb it already ended is not re-applied.
        engine.look(
            t0 + Duration::from_secs(61),
            &p,
            &p,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(true));
        engine.on_phone("p", Some(true));
        engine.look(
            t0 + Duration::from_secs(62),
            &p,
            &p,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(false), "and applied when it says so");
        // The switch is turned off: put back at once.
        engine.look(
            t0 + Duration::from_secs(63),
            &set(&[]),
            &p,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(true));
    }

    #[test]
    fn quitting_and_a_crash_put_it_back_only_if_it_is_as_we_left_it() {
        let (mut engine, mut banners, mut note) = rig(true);
        let (t0, p) = (Instant::now(), set(&["p"]));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        engine.on_phone("p", Some(true));
        engine.look(t0, &p, &p, &mut banners, &mut note);
        engine.quit(&mut banners, &mut note);
        assert_eq!(banners.shown, Some(true));
        assert_eq!(
            banners.log.borrow().last().map(String::as_str),
            Some("clear")
        );
        let (_, mut left, _) = rig(false);
        assert!(repair(&mut left, true, false));
        assert_eq!(left.shown, Some(true));
        let (_, mut changed, _) = rig(true);
        assert!(!repair(&mut changed, true, false), "the person changed it");
        assert!(changed.writes.is_empty());
    }

    #[test]
    fn a_desktop_without_the_setting_is_left_alone() {
        let (mut engine, mut banners, mut note) = rig(true);
        banners.shown = None;
        let (t0, p) = (Instant::now(), set(&["p"]));
        engine.on_phone("p", Some(true));
        assert_eq!(
            engine.look(t0, &p, &p, &mut banners, &mut note),
            Outcome::default()
        );
        assert!(banners.writes.is_empty());
    }

    /// F: A starts Do Not Disturb; B follows the desk (its report is an echo);
    /// when A ends it and B has not yet caught up, the desk still lets go, and
    /// the phones are told so B follows.
    #[test]
    fn a_follower_does_not_hold_the_desk_after_its_owner_ends_it() {
        let (mut engine, mut banners, mut note) = rig(true);
        let (t0, both) = (Instant::now(), set(&["a", "b"]));
        engine.look(t0, &both, &both, &mut banners, &mut note);
        engine.on_phone("a", Some(true));
        engine.on_phone("b", Some(false));
        let out = engine.look(t0, &both, &both, &mut banners, &mut note);
        assert_eq!(banners.shown, Some(false));
        assert_eq!(out.desk_changed, Some(true), "B is told");
        // B follows and says so.
        engine.on_phone("b", Some(true));
        engine.look(t0, &both, &both, &mut banners, &mut note);
        assert_eq!(banners.shown, Some(false));
        // A ends it; B has not caught up.
        engine.on_phone("a", Some(false));
        let out = engine.look(t0, &both, &both, &mut banners, &mut note);
        assert_eq!(banners.shown, Some(true), "not held by the follower");
        assert_eq!(out.desk_changed, Some(false), "B is told to let go");
        // B catches up; nothing flaps.
        engine.on_phone("b", Some(false));
        engine.look(t0, &both, &both, &mut banners, &mut note);
        assert_eq!(banners.writes, [false, true]);
    }

    #[test]
    fn an_owner_that_leaves_is_replaced_by_a_phone_that_started_on_its_own() {
        let (mut engine, mut banners, mut note) = rig(true);
        let (t0, both, only_b) = (Instant::now(), set(&["a", "b"]), set(&["b"]));
        engine.look(t0, &both, &both, &mut banners, &mut note);
        engine.on_phone("a", Some(true));
        engine.look(t0, &both, &both, &mut banners, &mut note);
        // B follows (its report is an echo of the desk's state).
        engine.on_phone("b", Some(true));
        // A leaves: held for a minute, then the desk lets go though B says so.
        engine.look(
            t0 + Duration::from_secs(1),
            &both,
            &only_b,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(false), "A is only just gone");
        engine.look(
            t0 + Duration::from_secs(70),
            &both,
            &only_b,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(true), "A gone; B was only following");
        // B hears the desk let go, drops out, and later starts on its own.
        engine.on_phone("b", Some(false));
        engine.on_phone("b", Some(true));
        engine.look(
            t0 + Duration::from_secs(71),
            &both,
            &only_b,
            &mut banners,
            &mut note,
        );
        assert_eq!(banners.shown, Some(false), "B on its own");
    }
}
