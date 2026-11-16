//! The glide worker thread: when a phone may glide (link, dozing, allowed),
//! the keymap lookup, reacting to portal events, performing the machine's
//! outputs, the clipboard hand-over, shutdown, and the periodic check that
//! arms or releases.
//!
//! Sibling edges: `machine`, `portal`, `input_packets`. Outside:
//! `desk_features`, `webrtc::coordinator` (liveness), `logind`, `asks`,
//! `config`, `daemon` (state, clipboard), `connection_log`, `features`.

use super::*;

use glib::translate::IntoGlib;

/// Debug builds: an `e2e-*` id, or a window `fake-device`, glides with no
/// phone (its packets are logged instead of sent).
#[cfg(debug_assertions)]
fn test_device(device: &str) -> bool {
    device.starts_with("e2e-") || device.starts_with("fake")
}

/// Whether the phone says it is dozing (a test device never is).
fn is_dozing(device: &str) -> bool {
    #[cfg(debug_assertions)]
    if test_device(device) {
        return false;
    }
    crate::device_links::webrtc::coordinator::is_dozing(device)
}

/// How long the phone has been silent (a test device never is).
fn inbound_silence(device: &str) -> Option<Duration> {
    #[cfg(debug_assertions)]
    if test_device(device) {
        return None;
    }
    crate::device_links::desk_features::inbound_silence(device)
}

/// Whether the person is at the computer: the session is not locked and has
/// not gone idle. Unknown counts as here (a desktop without logind still glides).
fn person_is_here(device: &str) -> bool {
    #[cfg(debug_assertions)]
    if test_device(device) {
        return true;
    }
    let _ = device;
    crate::device_links::logind::locked() != Some(true)
        && crate::device_links::logind::idle_hint() != Some(true)
}

/// The barrier is armed only for a phone whose link is ready, that is not
/// silent and that has said it allows glide. Nothing else turns it off for
/// good: a failed arming is tried again ([`arm_retry_after`]).
pub(super) fn wants_barrier(silent: bool, ready: bool, allowed: bool) -> bool {
    !silent && ready && allowed
}

/// Whether the phone has said it allows glide (debug builds: or a test
/// device, which has no phone to say it).
pub(super) fn allows_glide(device: &str) -> bool {
    #[cfg(debug_assertions)]
    if test_device(device) {
        return true;
    }
    phone_allows(device) == Some(true)
}

/// Whether the phone's link is ready (debug builds: or a test device).
fn ready(device: &str) -> bool {
    #[cfg(debug_assertions)]
    if test_device(device) {
        return true;
    }
    crate::device_links::desk_features::ready_devices()
        .iter()
        .any(|id| id == device)
}

fn deliver(device: &str, packet: &NetworkPacket) {
    #[cfg(debug_assertions)]
    if test_device(device) {
        debug_log!(
            "[Glide] → {} {}",
            packet.packet_type,
            serde_json::Value::Object(packet.body.clone())
        );
        return;
    }
    crate::device_links::desk_features::send(device, packet);
}

/// G6's packet: non-empty text within [`MAX_CLIPBOARD_BYTES`].
pub(super) fn clipboard_packet(text: String) -> Option<NetworkPacket> {
    if text.is_empty() || text.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_CLIPBOARD);
    packet.set("content", text);
    crate::device_links::webrtc::fits_on_wire(&packet).then_some(packet)
}

/// G6, off the worker thread (a clipboard read may wait on another app).
fn send_clipboard(device: &str) {
    static BUSY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !crate::device_links::config::app_settings().clipboard_sync
        || BUSY.swap(true, std::sync::atomic::Ordering::AcqRel)
    {
        return;
    }
    let device = device.to_string();
    let spawned = std::thread::Builder::new()
        .name("desklink-glide-clip".into())
        .spawn(move || {
            // The same guard as the clipboard sync: a password manager's
            // secret never follows the pointer (I6).
            let text = if crate::device_links::daemon::clipboard::clipboard_is_password() {
                String::new()
            } else {
                arboard::Clipboard::new()
                    .and_then(|mut clipboard| clipboard.get_text())
                    .unwrap_or_default()
            };
            if let Some(packet) = clipboard_packet(text) {
                deliver(&device, &packet);
            }
            BUSY.store(false, std::sync::atomic::Ordering::Release);
        });
    if spawned.is_err() {
        BUSY.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Something that changes the capture session.
#[derive(Debug)]
pub(super) enum Event {
    Signal(Signal),
    /// The EIS reader stopped (the compositor closed the socket).
    ReaderClosed,
}

/// What the worker does with the session after an [`Event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum After {
    Keep,
    /// Disable, so the next check sets the barrier again (new screens).
    Rearm,
    /// Drop the session (asked for again when the person is back); the
    /// reason is logged.
    Close(&'static str),
}

/// The outs and the session change for `event` (tested). A capture that may
/// still be live is always released, never just forgotten.
pub(super) fn react(event: Event, glide: Option<&mut Glide>) -> (Vec<Out>, After) {
    let mut outs = Vec::new();
    let after = match event {
        Event::Signal(Signal::Activated { id, cursor }) => {
            if let Some(glide) = glide {
                outs = glide.activated(id, cursor, &desk_key, Instant::now());
            }
            After::Keep
        }
        Event::Signal(Signal::Deactivated) => {
            if let Some(glide) = glide {
                outs = glide.deactivated();
            }
            After::Keep
        }
        // The compositor ended capture itself: nothing left to release.
        Event::Signal(Signal::Disabled) => {
            if let Some(glide) = glide {
                outs = glide.deactivated();
            }
            After::Rearm
        }
        // Screens changed (a monitor plugged mid-glide): the capture may
        // still hold the pointer, so it is released, then re-armed.
        Event::Signal(Signal::ZonesChanged) => {
            if let Some(glide) = glide {
                outs = glide.take_back();
            }
            After::Rearm
        }
        Event::Signal(Signal::Closed) => {
            if let Some(glide) = glide {
                outs = glide.deactivated();
            }
            After::Close("the desktop closed the session")
        }
        Event::ReaderClosed => {
            if let Some(glide) = glide {
                outs = glide.take_back();
            }
            After::Close("the desktop ended the input socket")
        }
    };
    (outs, after)
}

/// Carries out what the state machine asked, for `device`. A Release the
/// desktop refuses falls back to disabling the session, so the pointer is
/// never left captured.
fn perform(device: Option<&str>, outs: Vec<Out>, portal: Option<&mut Portal>) {
    let Some(device) = device else {
        return;
    };
    let mut portal = portal;
    for out in outs {
        match out {
            Out::Send(packet) => deliver(device, &packet),
            Out::Clipboard => send_clipboard(device),
            Out::Release { activation, x, y } => {
                if let Some(portal) = portal.as_deref_mut() {
                    if !portal.release(activation, x, y) {
                        portal.disable();
                    }
                }
            }
            Out::Refused => {
                let phone = crate::device_links::connection_log::name_of(device);
                log_limited(
                    "glide-refused",
                    &format!("{phone} did not accept the pointer"),
                );
                crate::device_links::webrtc::capture::notify(
                    crate::device_links::daemon::handle::DaemonEvent::GlideNotice {
                        device_id: device.to_string(),
                        message: format!("{phone} did not accept the pointer"),
                    },
                );
            }
            Out::Gliding(on) => {
                debug_log!("[Glide] {}", if on { "on the phone" } else { "back" });
                set_status(|status| status.gliding = on.then(|| device.to_string()));
            }
        }
    }
}

/// Asked by [`shutdown`]: the worker releases everything and ends.
pub(super) static SHUTDOWN: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);
/// Set by the worker once it has released and closed everything.
static ENDED: (Mutex<bool>, std::sync::Condvar) = (Mutex::new(false), std::sync::Condvar::new());

/// How long [`shutdown`] waits for the pointer to be given back.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(2_500);

/// How long a phone may be silent while the pointer is on it. A phone that
/// is there answers the desk's 5 s liveness probe at once (the desk is the
/// one talking during glide), so silence past the probe's time plus three
/// seconds means the link is gone: the pointer comes back then, instead of
/// waiting out the 15 s the general rule allows.
pub(super) const GLIDE_SILENT_AFTER: Duration =
    Duration::from_secs(crate::device_links::webrtc::coordinator::PEER_PROBE_AFTER.as_secs() + 3);

/// Whether a phone the pointer is on has gone quiet for too long. A dozing
/// phone (screen off, frozen by its OS) is silent on the data channel by
/// nature and is kept alive by the LAN heartbeat, so silence alone never
/// hands the pointer back from one.
pub(super) fn link_silent(gliding: bool, silence: Option<Duration>, dozing: bool) -> bool {
    gliding && !dozing && silence.is_some_and(|silence| silence > GLIDE_SILENT_AFTER)
}

/// The app is quitting: if the pointer is on the phone, tell the phone and
/// give the pointer back, then close the capture session. Bounded; a
/// compositor that does not answer cannot hold the app open.
pub(crate) fn shutdown() {
    if queue().get().is_none() {
        return;
    }
    SHUTDOWN.store(true, std::sync::atomic::Ordering::Release);
    let Ok(mut ended) = ENDED.0.lock() else {
        return;
    };
    let deadline = Instant::now() + SHUTDOWN_WAIT;
    while !*ended {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return;
        }
        match ENDED.1.wait_timeout(ended, left) {
            Ok((guard, _)) => ended = guard,
            Err(_) => return,
        }
    }
}

/// Everything the worker holds between turns.
pub(super) struct State {
    pub(super) portal: Option<Portal>,
    pub(super) glide: Option<Glide>,
    /// The setting the session was made for.
    pub(super) current: Option<GlideSetting>,
    /// Arming failed this many times in a row, and when to try again.
    pub(super) arm_failures: u32,
    pub(super) retry_at: Option<Instant>,
    /// A refusal's back-off and whether the person was told, kept across
    /// re-armed sessions: (until, told).
    pub(super) refusal: (Option<Instant>, bool),
    /// The wait the window shows (`problem`, "Turn on again").
    pub(super) held_shown: Option<Why>,
    /// When the portal may be asked for a session (once per run).
    pub(super) consent: Consent,
    /// The phone's link was ready, and the person here, at the last check.
    pub(super) was_ready: bool,
    pub(super) was_here: bool,
    /// [`suspended_so_far`] at the last check, to notice a sleep.
    pub(super) suspended: Duration,
    /// When the desktop disabled the edge this last minute ([`rearm_delay`]).
    pub(super) disables: VecDeque<Instant>,
    /// GetZones said the session is gone once: one more fresh try first.
    pub(super) gone_once: bool,
    /// The person just set Glide to this phone: if the phone says Glide is
    /// off there, it is asked once, so the prompt comes without hunting for
    /// "Ask on phone" ([`auto_ask`]).
    pub(super) auto_ask: Option<String>,
    /// The first check (the setting read at start is not the person acting).
    pub(super) checked_once: bool,
}

impl State {
    pub(super) fn new() -> Self {
        Self {
            portal: None,
            glide: None,
            current: None,
            arm_failures: 0,
            retry_at: None,
            refusal: (None, false),
            held_shown: None,
            consent: Consent::new(),
            was_ready: false,
            was_here: true,
            suspended: suspended_so_far(),
            disables: VecDeque::new(),
            gone_once: false,
            auto_ask: None,
            checked_once: false,
        }
    }

    /// The desktop disabled the edge or changed the screens (the session is
    /// still good): nothing acts on the old barrier any more, and the edge is
    /// set again on fresh zones, at once unless it keeps happening.
    pub(super) fn disabled_by_desktop(&mut self, now: Instant) {
        self.glide = None;
        self.disables
            .retain(|at| now.saturating_duration_since(*at) < MINUTE);
        if self.disables.len() >= 16 {
            self.disables.pop_front();
        }
        self.disables.push_back(now);
        self.arm_failures = 0;
        let delay = rearm_delay(self.disables.len());
        self.retry_at = (!delay.is_zero()).then(|| now + delay);
    }

    /// GetZones said the session is gone: true when it should be dropped
    /// now. The first time, one more fresh GetZones is tried at once (a
    /// portal that was mid-restart can answer that way once).
    pub(super) fn session_gone(&mut self) -> bool {
        if std::mem::replace(&mut self.gone_once, true) {
            return true;
        }
        self.retry_at = None;
        false
    }

    /// Arm again as soon as the rate allows (new screens, a wake, an unlock).
    fn arm_now(&mut self) {
        self.arm_failures = 0;
        self.retry_at = None;
    }

    /// The session is gone (the desktop closed it, ended its input socket, or
    /// no longer knows it): it is dropped, and a new one is asked for when
    /// the person is back. Any capture was released by the caller.
    fn close_session(&mut self, reason: &str) {
        debug_log!("[Glide] session closed: {reason}; asks again when the person is back");
        if let Some(mut closing) = self.portal.take() {
            closing.disable();
        }
        self.glide = None;
        self.arm_now();
        self.gone_once = false;
        self.consent.closed(Instant::now());
        self.held_shown = self.consent.held();
        let words = self.held_shown.map(|why| why.words().to_string());
        set_status(|status| {
            status.gliding = None;
            status.ready = None;
            status.problem = words;
            status.can_ask_again = true;
        });
    }

    /// Every way out ends here: the pointer goes back where it crossed, the
    /// phone is told, the session is disabled and closed. Used when the
    /// worker ends, however it ends.
    pub(super) fn release_all(&mut self) {
        let back = self
            .glide
            .as_mut()
            .map(Glide::take_back)
            .unwrap_or_default();
        perform(
            self.current.as_ref().map(|setting| setting.device.as_str()),
            back,
            self.portal.as_mut(),
        );
        if let Some(mut portal) = self.portal.take() {
            portal.disable();
        }
        self.glide = None;
        set_status(|status| status.gliding = None);
    }
}

/// How long the worker sleeps for the next event: short while the pointer
/// may be on the phone (motion is coalesced to [`MOTION_EVERY`]) or a
/// session is armed (its signals are only read between sleeps), long while
/// glide is off, which is nearly always.
pub(super) fn nap(state: &State) -> Duration {
    if state
        .glide
        .as_ref()
        .is_some_and(|glide| glide.active.is_some())
    {
        MOTION_EVERY
    } else if state.portal.is_some() {
        ARMED_NAP
    } else if state.current.is_none() && state.glide.is_none() {
        OFF_NAP
    } else {
        CHECK_EVERY
    }
}

pub(super) fn worker(context: &glib::MainContext, commands: &Receiver<Command>) {
    let connection = match gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) {
        Ok(connection) => connection,
        Err(_) => {
            set_status(|status| {
                status.availability = Availability::Unavailable("The desktop bus is not running")
            });
            end_worker();
            return;
        }
    };
    let availability = check_availability(&connection);
    set_status(|status| status.availability = availability.clone());
    if availability != Availability::Available {
        end_worker();
        return;
    }
    let mut state = State::new();
    // A panic must not leave the pointer captured, nor the top bar saying
    // glide is on: release, then say glide stopped.
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(context, commands, &connection, &mut state)
    }));
    state.release_all();
    if ran.is_err() {
        eprintln!("[DeskLink] Glide failed; the pointer was given back");
        set_status(|status| {
            status.availability = Availability::Unavailable("Glide stopped working");
        });
    }
    end_worker();
}

fn end_worker() {
    if let Ok(mut ended) = ENDED.0.lock() {
        *ended = true;
        ENDED.1.notify_all();
    }
}

fn run(
    context: &glib::MainContext,
    commands: &Receiver<Command>,
    connection: &gio::DBusConnection,
    state: &mut State,
) {
    let mut checked = Instant::now() - CHECK_EVERY;
    loop {
        if SHUTDOWN.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let mut outs = Vec::new();
        // Up to QUEUE events per turn, then the rest of the loop runs.
        let first = commands.recv_timeout(nap(state));
        if matches!(first, Err(mpsc::RecvTimeoutError::Disconnected)) {
            break;
        }
        let device = state.current.as_ref().map(|setting| setting.device.clone());
        let session = state.portal.as_ref().map(|portal| portal.generation);
        let mut events = Vec::new();
        for command in first.into_iter().chain(commands.try_iter().take(QUEUE)) {
            match command {
                Command::Captured(from, _) if !is_current(from, session) => {}
                Command::Captured(_, Captured::Closed) => events.push(Event::ReaderClosed),
                Command::Captured(_, captured) => {
                    if let Some(glide) = state.glide.as_mut() {
                        outs.extend(glide.input(captured, &desk_key, Instant::now()));
                    }
                }
                Command::Released {
                    device: from,
                    dx,
                    dy,
                } => {
                    if device.as_deref() == Some(from.as_str()) {
                        if let Some(glide) = state.glide.as_mut() {
                            outs.extend(glide.phone_released(dx, dy, Instant::now()));
                        }
                    }
                }
                Command::State {
                    device: from,
                    allowed,
                    entered,
                } => {
                    if device.as_deref() == Some(from.as_str()) {
                        if let Some(glide) = state.glide.as_mut() {
                            let now = Instant::now();
                            outs.extend(match (allowed, entered) {
                                (false, _) => glide.disallowed(now),
                                (true, true) => glide.accepted(&desk_key, now),
                                (true, false) => Vec::new(),
                            });
                        }
                    }
                }
                Command::HandedBack { device: from } => {
                    if device.as_deref() == Some(from.as_str()) {
                        if let Some(glide) = state.glide.as_mut() {
                            outs.extend(glide.disallowed(Instant::now()));
                        }
                    }
                }
            }
        }
        while context.pending() {
            context.iteration(false);
        }
        let signals: Vec<Signal> = state
            .portal
            .as_ref()
            .map(|portal| portal.signals.borrow_mut().drain(..).collect())
            .unwrap_or_default();
        events.extend(signals.into_iter().map(Event::Signal));
        for event in events {
            let (more, after) = react(event, state.glide.as_mut());
            outs.extend(more);
            if after == After::Keep {
                continue;
            }
            // The Release first, while the session that captured is open.
            perform(
                state
                    .current
                    .as_ref()
                    .map(|setting| setting.device.as_str()),
                std::mem::take(&mut outs),
                state.portal.as_mut(),
            );
            match after {
                After::Keep => {}
                // The session is still good: zones are read again and the
                // barrier set and enabled at the next check, no dialog.
                After::Rearm => {
                    if let Some(portal) = state.portal.as_mut() {
                        debug_log!(
                            "[Glide] the desktop disabled the edge or changed screens; re-arming"
                        );
                        portal.disable();
                        state.disabled_by_desktop(Instant::now());
                    }
                }
                After::Close(reason) => state.close_session(reason),
            }
        }
        if let Some(glide) = state.glide.as_mut() {
            outs.extend(glide.tick(Instant::now()));
        }

        if checked.elapsed() >= CHECK_EVERY {
            checked = Instant::now();
            check(context, connection, state, &mut outs);
        }

        let target = state.current.as_ref().map(|setting| setting.device.clone());
        perform(target.as_deref(), outs, state.portal.as_mut());
        if let Some(glide) = state.glide.as_ref() {
            state.refusal = (glide.backoff_until, glide.told);
        }
        if let Some(glide) = state.glide.as_mut() {
            if glide.dropped > 0 {
                log_limited("pending", "Input before the capture started was dropped");
                glide.dropped = 0;
            }
        }
    }
}

/// What to do about asking the phone after the person set Glide to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AutoAsk {
    /// Ask now: the phone says Glide is off there.
    Now,
    /// Nothing to ask (it allows Glide, or is only locked), or nothing pending.
    Never,
    /// Its report has not come yet.
    Later,
}

/// `pending`: the person just set Glide to this (ready) phone (tested).
pub(super) fn auto_ask(pending: bool, allowed: Option<bool>, locked: bool) -> AutoAsk {
    match (pending, allowed) {
        (false, _) | (true, Some(true)) => AutoAsk::Never,
        (true, None) => AutoAsk::Later,
        (true, Some(false)) if locked => AutoAsk::Never,
        (true, Some(false)) => AutoAsk::Now,
    }
}

/// Every [`CHECK_EVERY`]: the setting and the link are compared with what
/// the session was made for, and the session follows.
fn check(
    context: &glib::MainContext,
    connection: &gio::DBusConnection,
    state: &mut State,
    outs: &mut Vec<Out>,
) {
    let now = Instant::now();
    let suspended = suspended_so_far();
    if slept(
        std::mem::replace(&mut state.suspended, suspended),
        suspended,
    ) {
        // A wake is a return (even with no lock screen) and a new chance to arm.
        debug_log!("[Glide] the computer slept");
        state.consent.was_away(now);
        state.arm_now();
    }
    let setting = crate::device_links::config::app_settings().glide;
    let asked_again = ASK_AGAIN.swap(false, std::sync::atomic::Ordering::AcqRel);
    if asked_again && setting == state.current {
        // The person ended the wait; nothing else changed.
        debug_log!("[Glide] turned on again by the person: every wait ends");
        state.consent.person_acts();
        // And the edge is set again, so a session the desktop dropped
        // without a word is found out (and asked for) now.
        if let Some(portal) = state.portal.as_mut() {
            let back = state
                .glide
                .as_mut()
                .map(Glide::take_back)
                .unwrap_or_default();
            perform(
                state
                    .current
                    .as_ref()
                    .map(|setting| setting.device.as_str()),
                back,
                Some(&mut *portal),
            );
            portal.disable();
            state.glide = None;
        }
        state.arm_now();
        state.held_shown = None;
        set_status(|status| {
            status.problem = None;
            status.can_ask_again = false;
        });
    }
    if setting != state.current {
        // Done now, for the phone it was set for, while the session that
        // captured is still open.
        let mut back = std::mem::take(outs);
        if let Some(glide) = state.glide.as_mut() {
            back.extend(glide.take_back());
        }
        perform(
            state
                .current
                .as_ref()
                .map(|setting| setting.device.as_str()),
            back,
            state.portal.as_mut(),
        );
        // The session (and the consent behind it) belongs to this run, not
        // to a phone or a setting: switching Glide off and on, or to another
        // phone, only moves the barrier.
        if let Some(portal) = state.portal.as_mut() {
            // A new edge: barriers are replaced on a disabled session.
            portal.disable();
            portal.armed_at.clear();
            state.glide = None;
        }
        debug_log!("[Glide] setting now {setting:?}");
        state.auto_ask = setting
            .as_ref()
            .filter(|_| state.checked_once)
            .map(|setting| setting.device.clone());
        state.current = setting;
        state.refusal = (None, false);
        state.arm_now();
        // Switching Glide is the person acting: a wait after a Cancel ends.
        state.consent.person_acts();
        state.held_shown = None;
        set_status(|status| {
            status.problem = None;
            status.gliding = None;
            status.can_ask_again = false;
        });
    }
    let device = state.current.as_ref().map(|setting| setting.device.clone());
    let gliding = state
        .glide
        .as_ref()
        .is_some_and(|glide| glide.active.is_some());
    // Looked at all the time Glide is on (at most every 2 s; not while the
    // pointer is on the phone, the person is plainly here), so a lock or an
    // idle spell is seen even while the session is open.
    let here = gliding
        || device
            .as_deref()
            .is_none_or(|device| state.consent.here(now, &mut || person_is_here(device)));
    let back = here && !std::mem::replace(&mut state.was_here, here);
    if back && !gliding {
        // Back at the computer (an unlock, the end of an idle spell): the
        // edge is set again on fresh zones, which also finds out at once
        // whether the desktop kept the session through the lock.
        if let Some(portal) = state.portal.as_mut() {
            debug_log!("[Glide] the person is back; re-arming");
            if portal.enabled {
                portal.disable();
            }
            state.glide = None;
            state.arm_now();
        }
    }
    let silent = link_silent(
        gliding,
        device.as_deref().and_then(inbound_silence),
        device.as_deref().is_some_and(is_dozing),
    );
    let is_ready = device.as_deref().is_some_and(ready);
    // The phone reports on every feature-ready: what it said before the link
    // went away is not known any more. Forgotten when the link goes, not on
    // every check while it is down: a report that arrives a moment before
    // this side counts the link as ready must not be wiped.
    if let (true, false, Some(device)) = (state.was_ready, is_ready, device.as_deref()) {
        set_allowed(device, None);
    }
    state.was_ready = is_ready;
    state.checked_once = true;
    if let Some(device) = device.as_deref().filter(|_| is_ready) {
        let ask = auto_ask(
            state.auto_ask.as_deref() == Some(device),
            phone_allows(device),
            phone_locked(device),
        );
        if ask != AutoAsk::Later {
            state.auto_ask = None;
        }
        if ask == AutoAsk::Now
            && crate::device_links::features::peer_accepts(device, PACKET_TYPE_SETTINGS_REQUEST)
        {
            debug_log!("[Glide] Glide is off on {device}; asking there");
            // Not pressed for: the shared rate limit holds it to one per
            // `asks::REASK`.
            crate::device_links::asks::ask_phone(device, PHONE_SETTING_KEY, false);
        }
    }
    // The barrier is armed only while the phone says it allows glide: an
    // unknown state (an older phone, no report yet) is not a yes.
    let allowed = device.as_deref().is_some_and(allows_glide);
    let want = wants_barrier(silent, is_ready, allowed);
    if state.portal.is_none() {
        // The one place a session is created, and so the one place the
        // desktop's dialog can come from.
        let ready = want && device.is_some();
        let was_held = state.consent.held();
        let step = state
            .consent
            .step(now, ready, || device.as_deref().is_none_or(person_is_here));
        if was_held.is_some() && state.consent.held().is_none() {
            debug_log!("[Glide] the wait ({was_held:?}) is over");
        }
        if step == Step::Ask {
            debug_log!("[Glide] asking the desktop for input capture");
            set_status(|status| {
                status.asking = true;
                status.problem = None;
                status.can_ask_again = false;
            });
            // The notice is posted by the window's tick; the dialog follows.
            crate::device_links::daemon::state::nudge_tickers();
            let opened = Portal::open(context, connection);
            set_status(|status| status.asking = false);
            match opened {
                Ok(opened) => {
                    debug_log!("[Glide] allowed");
                    state.consent.answered(Instant::now(), Outcome::Allowed);
                    state.portal = Some(opened);
                    state.arm_now();
                    state.held_shown = None;
                }
                Err(error) => {
                    log_limited("open", &error);
                    let outcome = Outcome::of_error(&error);
                    debug_log!("[Glide] the ask ended {outcome:?}: {error}");
                    state.consent.answered(Instant::now(), outcome);
                    let words = match state.consent.held() {
                        Some(why) => why.words().to_string(),
                        None => plain(&error),
                    };
                    state.held_shown = state.consent.held();
                    set_status(|status| {
                        status.problem = Some(words);
                        status.can_ask_again = true;
                    });
                }
            }
            crate::device_links::daemon::state::nudge_tickers();
        } else if let Some(why) = state.consent.held() {
            if state.held_shown != Some(why) && device.is_some() {
                state.held_shown = Some(why);
                set_status(|status| {
                    status.problem = Some(why.words().to_string());
                    status.can_ask_again = true;
                });
            }
        } else if state.held_shown.is_some() {
            // The wait ended on its own (the person came back, the time ran out).
            state.held_shown = None;
            set_status(|status| {
                status.problem = None;
                status.can_ask_again = false;
            });
        }
    }
    let mut gone = None;
    if let (Some(open), Some(setting)) = (state.portal.as_mut(), state.current.as_ref()) {
        // Not armed behind a lock screen: GNOME refuses it there, and drops
        // the session anyway; the unlock arms (or asks) at once.
        let due = state.retry_at.is_none_or(|at| now >= at);
        if want && here && !open.enabled && due {
            let at = next_arm_at(&open.armed_at, now);
            if at > now {
                debug_log!(
                    "[Glide] armed {REARMS_PER_MINUTE} times this minute; next in {:?}",
                    at - now
                );
                state.retry_at = Some(at);
            } else {
                match open.arm(context, setting.edge) {
                    Ok(barrier) => {
                        debug_log!(
                            "[Glide] armed: {:?} edge ({}, {})-({}, {})",
                            barrier.edge,
                            barrier.x1,
                            barrier.y1,
                            barrier.x2,
                            barrier.y2
                        );
                        let mut glide = Glide::new(barrier);
                        #[cfg(debug_assertions)]
                        if test_device(&setting.device) {
                            // A test device has no phone to answer.
                            glide.accept_needed = false;
                        }
                        (glide.backoff_until, glide.told) = state.refusal;
                        state.glide = Some(glide);
                        state.arm_now();
                        state.gone_once = false;
                        set_status(|status| {
                            status.problem = None;
                            status.can_ask_again = false;
                        });
                    }
                    Err(ArmError::Gone(error)) => {
                        log_limited("arm", &error);
                        if state.session_gone() {
                            gone = Some(error);
                        } else {
                            debug_log!("[Glide] the desktop says the session is gone ({error}); one more try");
                        }
                    }
                    Err(ArmError::Retry(error)) => {
                        log_limited("arm", &error);
                        state.arm_failures += 1;
                        let after = arm_retry_after(state.arm_failures);
                        debug_log!("[Glide] arming failed ({error}); again in {after:?}");
                        state.retry_at = Some(now + after);
                        if state.arm_failures == ARM_RETRIES {
                            let words = format!(
                                "Glide could not set up the screen edge ({}). It keeps trying.",
                                plain(&error)
                            );
                            set_status(|status| {
                                status.problem = Some(words);
                                status.can_ask_again = true;
                            });
                        }
                    }
                }
            }
        } else if !want && open.enabled {
            // The Release (and telling the phone) before Disable.
            debug_log!("[Glide] disarmed (ready {is_ready}, allowed {allowed}, silent {silent})");
            let mut back = std::mem::take(outs);
            if let Some(glide) = state.glide.as_mut() {
                back.extend(glide.take_back());
            }
            perform(Some(setting.device.as_str()), back, Some(&mut *open));
            open.disable();
            // Nothing acts on a barrier that is not there.
            state.glide = None;
        }
    }
    if let Some(error) = gone {
        state.close_session(&format!("the desktop no longer knows it ({error})"));
    }
    let edge = state
        .portal
        .as_ref()
        .filter(|portal| portal.enabled)
        .and(state.current.as_ref())
        .map(|setting| setting.edge);
    if status().ready != edge {
        set_status(|status| status.ready = edge);
    }
}

/// The key an evdev `code` types with `mods`, through this desktop's keymap
/// (GDK, on the main thread; cached).
pub(super) fn desk_key(code: u32, mods: Mods) -> Option<gdk::Key> {
    static CACHE: Mutex<Option<KeyCache>> = Mutex::new(None);
    let level = Mods {
        shift: mods.shift,
        caps: mods.caps,
        altgr: mods.altgr,
        ..Mods::default()
    };
    let known = CACHE.lock().ok().and_then(|cache| {
        cache
            .as_ref()
            .and_then(|cache| cache.get((code, level), Instant::now()))
    });
    let keyval = match known {
        Some(keyval) => keyval,
        // Quitting: a key not known is not worth waiting for.
        None if SHUTDOWN.load(std::sync::atomic::Ordering::Acquire) => None,
        None => {
            let (sender, receiver) = mpsc::sync_channel(1);
            glib::MainContext::default().invoke(move || {
                let found = gdk::Display::default().and_then(|display| {
                    display
                        .translate_key(code + 8, level.level_state(), 0)
                        .map(|(key, ..)| key.into_glib())
                });
                let _ = sender.send(found);
            });
            // A main thread that did not answer in time is "don't know", and
            // is not remembered as "no key types this".
            match receiver.recv_timeout(KEY_LOOKUP) {
                Ok(found) => {
                    if let Ok(mut cache) = CACHE.lock() {
                        cache.get_or_insert_with(KeyCache::default).put(
                            (code, level),
                            found,
                            Instant::now(),
                        );
                    }
                    found
                }
                Err(_) => None,
            }
        }
    }?;
    // SAFETY: any u32 is a valid GDK keyval number.
    Some(unsafe { glib::translate::from_glib::<u32, gdk::Key>(keyval) })
}
