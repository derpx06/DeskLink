//! Glide (F4): this computer's mouse and keyboard glide onto the phone
//! across an edge of the screen, KDE Connect's shareinputdevices protocol
//! (contract "DeskLink 1.2 — F4 glide").
//!
//! The InputCapture portal puts a pointer barrier on the chosen edge. When
//! the pointer crosses it the compositor stops moving the pointer and hands
//! every input event to us over EIS ([`eis::spawn_receiver`]); we tell the
//! phone where the pointer entered and stream the events as
//! `desklink.mousepad.request`. The phone hands the pointer back with
//! `desklink.shareinputdevices` and we release the capture there. Esc with
//! Super held always brings it back.
//!
//! One worker thread owns the portal session (its D-Bus calls and signals,
//! on its own main context) and the [`Glide`] state machine; everything
//! reaches it through one bounded channel. The session lives while glide is
//! on, so its consent dialog is shown once per run, not on every reconnect.
//!
//! Layout: this file holds the limits, the setting, the status the UI reads,
//! the worker's commands and the phone-facing calls (reports, asks, the
//! hand-back). `machine` is the pure state machine, `input_packets` the
//! packets and modifiers, `portal` the InputCapture session and `worker` the
//! thread. No cycles between the parts. May depend on: `packet`,
//! `desk_features`, `eis`, `webrtc::coordinator`, `logind`, `asks`, `config`,
//! `daemon`.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::gdk::prelude::*;
use gtk::{gdk, gio, glib};
use serde::{Deserialize, Serialize};

use crate::device_links::eis::{self, Captured};
use crate::device_links::glide_consent::{Consent, Outcome, Step, Why};
use crate::device_links::limits::{log_limited, Buckets};
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_SETTINGS_REQUEST, PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST,
};
use crate::device_links::webrtc::remote_desktop_portal::{
    lookup, object_path, options, pipewire_fd_index, request, token, tuple, PORTAL_BUS,
    PORTAL_PATH, SESSION_INTERFACE,
};

mod input_packets;
mod machine;
mod portal;
mod worker;
use input_packets::*;
use machine::*;
use portal::*;
use worker::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use worker::shutdown;

const INTERFACE: &str = "org.freedesktop.portal.InputCapture";

// Limits. Each one bounds what a peer, the compositor or a stuck portal can
// make this module hold or do.
/// Events queued for the worker (captured input and phone releases); a full
/// queue drops, it never blocks the capture reader or grows.
const QUEUE: usize = 1024;
/// Captured events kept while waiting for the portal's Activated signal
/// (they may arrive first); beyond this they are dropped.
const PENDING_MAX: usize = 256;
/// How long input that came before the crossing's Activated is still its own.
const PENDING_FRESH: Duration = Duration::from_millis(300);
/// After the desk says the pointer is entering, the phone has this long to
/// accept (see "W6 — glide allowed state"). Nothing is streamed meanwhile, and
/// no answer is a refusal: the pointer comes back where it crossed. It was
/// 300 ms: a phone whose Wi-Fi had dozed off, or whose event thread was busy
/// with a notification burst, answered later than that, so the crossing was
/// refused and the edge then rested 5 s ("sometimes it doesn't work at all").
const ENTER_WAIT: Duration = Duration::from_millis(1_000);
/// After the phone refuses, the edge is left alone this long: a crossing then
/// is released at once, with nothing sent.
const REFUSED_BACKOFF: Duration = Duration::from_secs(5);
/// After the phone did not answer in time (it said no to nothing), the rest
/// is short: the next push on the edge tries again.
const UNANSWERED_BACKOFF: Duration = Duration::from_secs(1);
/// Input held while the phone has not accepted yet; beyond this, dropped.
const HELD_MAX: usize = 64;
/// Motion is coalesced: at most one motion packet per this long (125/s, one
/// per frame of a 120 Hz phone panel; the phone draws its pointer once per
/// frame anyway). Each packet carries the sum of the motion since the last.
const MOTION_EVERY: Duration = Duration::from_millis(8);
/// How often the worker re-reads the setting and the phone's link.
const CHECK_EVERY: Duration = Duration::from_millis(250);
/// The worker's nap with Glide switched off (nearly always): only the
/// setting can change, and a second is soon enough to notice it switched on.
const OFF_NAP: Duration = Duration::from_secs(1);
/// The worker's nap while a session is armed but the pointer is here.
const ARMED_NAP: Duration = Duration::from_millis(50);
/// Arming the barrier at most this often in any minute. A burst (monitors
/// plugged, the lid closed, Extend started, a phone reconnecting) waits for
/// the minute to roll over; it never turns glide off ([`next_arm_at`]).
const REARMS_PER_MINUTE: usize = 6;
/// Phone releases accepted per second (token bucket, refilled each second).
const RELEASES_PER_SECOND: u32 = 10;
/// A release delta further than this (logical px) is not a position.
const MAX_DELTA: f64 = 100_000.0;
/// The clipboard text that follows the pointer onto the phone (escaped and
/// enveloped, one data-channel message).
const MAX_CLIPBOARD_BYTES: usize = 16 * 1024;
/// D-Bus calls that answer at once (Enable, Release, …).
const CALL_TIMEOUT_MS: i32 = 2_000;
/// How long one key lookup on the main thread may take.
const KEY_LOOKUP: Duration = Duration::from_millis(500);
/// Keys remembered by the main-thread lookup cache.
const KEY_CACHE_MAX: usize = 1024;

/// Where the phone sits, beside this screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub(crate) const ALL: [Edge; 4] = [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom];

    /// The desk's edge as the wire's `exitEdge` (a Qt::Edge).
    fn qt(self) -> i64 {
        match self {
            Edge::Top => 1,
            Edge::Left => 2,
            Edge::Right => 4,
            Edge::Bottom => 8,
        }
    }

    /// "left of", for the Preferences words.
    pub(crate) fn words(self) -> &'static str {
        match self {
            Edge::Left => "left of",
            Edge::Right => "right of",
            Edge::Top => "above",
            Edge::Bottom => "below",
        }
    }

    fn vertical(self) -> bool {
        matches!(self, Edge::Left | Edge::Right)
    }
}

/// Preferences → Glide: which phone, and where it sits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GlideSetting {
    pub device: String,
    pub edge: Edge,
}

/// Whether this desktop can glide at all, and if not, why (plain words).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Availability {
    /// Not checked yet (the worker checks at start).
    Checking,
    Available,
    Unavailable(&'static str),
}

/// What the window and the top bar show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) availability: Availability,
    /// The phone the pointer is on now.
    pub(crate) gliding: Option<String>,
    /// Why glide is not working, in plain words.
    pub(crate) problem: Option<String>,
    /// The desktop's "Capture Input" dialog is up and waiting for the person.
    pub(crate) asking: bool,
    /// Glide is waiting instead of asking: the window offers "Turn on again".
    pub(crate) can_ask_again: bool,
    /// The barrier is armed on this edge: moving off it reaches the phone.
    pub(crate) ready: Option<Edge>,
}

static STATUS: Mutex<Status> = Mutex::new(Status {
    availability: Availability::Checking,
    gliding: None,
    problem: None,
    asking: false,
    can_ask_again: false,
    ready: None,
});

pub(crate) fn status() -> Status {
    STATUS
        .lock()
        .map(|status| status.clone())
        .unwrap_or(Status {
            availability: Availability::Unavailable("Glide stopped working"),
            gliding: None,
            problem: None,
            asking: false,
            can_ask_again: false,
            ready: None,
        })
}

fn set_status(change: impl FnOnce(&mut Status)) {
    if let Ok(mut status) = STATUS.lock() {
        change(&mut status);
    }
}

/// What reaches the worker.
#[derive(Debug)]
enum Command {
    /// From the EIS reader of session `generation`.
    Captured(u64, Captured),
    Released {
        device: String,
        dx: f64,
        dy: f64,
    },
    /// The phone's state report (`allowed`), and whether it also accepts the
    /// pointer that just entered (`entered`).
    State {
        device: String,
        allowed: bool,
        entered: bool,
    },
    /// The phone hands the pointer back with `exitEdge: 0`: a refusal.
    HandedBack {
        device: String,
    },
}

/// The key in `desklink.settings.request {set: {glide: true}}`: this desk
/// asks the phone to turn Glide on. The phone shows a prompt and the person
/// decides there; nothing is switched for them (the phone's
/// `Glide.SETTING_KEY`).
pub(crate) const PHONE_SETTING_KEY: &str = "glide";

/// The ask itself (`asks::request`; `asks::ask_phone` sends it).
#[cfg(test)]
pub(crate) fn turn_on_request() -> NetworkPacket {
    crate::device_links::asks::request(PHONE_SETTING_KEY)
}

/// Whether the phone's build takes the ask (`incoming` is what it said it
/// takes). An older phone only gets the words: turn it on there.
pub(crate) fn can_ask_phone(incoming: &[String]) -> bool {
    incoming
        .iter()
        .any(|packet| packet == PACKET_TYPE_SETTINGS_REQUEST)
}

/// Where an "Ask on phone" stands, until the phone allows Glide.
pub(crate) type PhoneAsk = crate::device_links::asks::AskState;

/// Where the last "Ask on phone" to `device_id` stands (the window and the
/// needs list show it).
pub(crate) fn phone_ask(device_id: &str) -> Option<PhoneAsk> {
    crate::device_links::asks::phone_ask(device_id, PHONE_SETTING_KEY)
}

/// What the person is told about an ask, in one line (the shared words).
pub(crate) fn ask_words(phone: &str, ask: PhoneAsk) -> String {
    crate::device_links::asks::words(ask, phone)
}

fn queue() -> &'static OnceLock<SyncSender<Command>> {
    static QUEUE_SENDER: OnceLock<SyncSender<Command>> = OnceLock::new();
    &QUEUE_SENDER
}

/// Set by the window's "Ask again": taken by the next check.
static ASK_AGAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The person asks Glide to request the desktop's permission again, ending
/// any wait after a Cancel, an unanswered dialog or a closed session.
pub(crate) fn ask_again() {
    ASK_AGAIN.store(true, std::sync::atomic::Ordering::Release);
}

/// Starts the worker (once, at daemon start).
pub(crate) fn start() {
    let (sender, receiver) = mpsc::sync_channel(QUEUE);
    if queue().set(sender).is_err() {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("desklink-glide".into())
        .spawn(move || {
            let context = glib::MainContext::new();
            let _ = context.with_thread_default(|| worker(&context, &receiver));
        });
    if spawned.is_err() {
        set_status(|status| {
            status.availability = Availability::Unavailable("Glide could not start");
        });
    }
}

/// What each phone last reported about glide: `allowed`, whether it said no
/// only because it is locked (`reason: "locked"`), and whether only because
/// its accessibility service is off or not bound (`reason: "accessibility"`). Unknown until it
/// reports, and forgotten whenever its link is not ready: the phone reports
/// on every feature-ready.
/// `(allowed, locked, service_off)`.
type Report = (bool, bool, bool);

static ALLOWED: Mutex<Option<HashMap<String, Report>>> = Mutex::new(None);

/// Whether the phone has said it allows glide: `None` until it reports (an
/// older phone never does).
pub(crate) fn phone_allows(device_id: &str) -> Option<bool> {
    report_of(device_id).map(|(allowed, ..)| allowed)
}

/// Whether the phone said glide waits only for it to be unlocked (cleared by
/// its next `allowed: true`).
pub(crate) fn phone_locked(device_id: &str) -> bool {
    report_of(device_id).is_some_and(|(_, locked, _)| locked)
}

/// Whether the phone said Glide is allowed there but its accessibility
/// service (which places the pointer) is off: ColorOS switches it off after
/// an update or a force-stop. Allowing Glide again would change nothing.
pub(crate) fn phone_needs_service(device_id: &str) -> bool {
    report_of(device_id).is_some_and(|(_, _, service)| service)
}

/// What the person is told while the service is off: the step on the phone.
pub(crate) fn service_words(phone: &str) -> String {
    format!("Turn on DeskLink in {phone}'s Accessibility settings to use Glide")
}

/// What the person is told while the phone is locked: no fix, the step.
pub(crate) fn unlock_words(phone: &str) -> String {
    format!("Unlock {phone} to use Glide")
}

fn report_of(device_id: &str) -> Option<Report> {
    ALLOWED.lock().ok().and_then(|known| {
        known
            .as_ref()
            .and_then(|known| known.get(device_id).copied())
    })
}

fn set_allowed(device_id: &str, allowed: Option<Report>) {
    if let Ok(mut known) = ALLOWED.lock() {
        let known = known.get_or_insert_with(HashMap::new);
        match allowed {
            Some(allowed) => {
                if known.len() >= 64 && !known.contains_key(device_id) {
                    return;
                }
                known.insert(device_id.to_string(), allowed);
            }
            None => {
                known.remove(device_id);
            }
        }
    }
}

/// The phone is no longer ours: what it said about glide goes.
pub(crate) fn forget_phone(device_id: &str) {
    set_allowed(device_id, None);
}

/// Everything the phone says on `desklink.shareinputdevices` (W6): a state
/// report (`allowed`, plus `entered: true` when it accepts the pointer that
/// just crossed), a hand-back with `exitEdge: 0` (a refusal), or G3's release
/// point. Each is a report, never an instruction to capture anything.
pub(crate) fn on_phone_packet(device_id: &str, packet: &NetworkPacket) {
    match classify_phone_packet(packet) {
        PhoneReport::State {
            allowed,
            entered,
            locked,
            declined,
            service_off,
        } => {
            set_allowed(device_id, Some((allowed, locked, service_off)));
            if allowed {
                crate::device_links::asks::note_on(device_id, PHONE_SETTING_KEY);
            } else if declined {
                crate::device_links::asks::note_declined(device_id, PHONE_SETTING_KEY);
            }
            send_command(Command::State {
                device: device_id.to_string(),
                allowed,
                entered,
            });
        }
        PhoneReport::HandedBack => {
            send_command(Command::HandedBack {
                device: device_id.to_string(),
            });
        }
        PhoneReport::Release | PhoneReport::Ignored => phone_released(device_id, packet),
    }
}

/// What a `desklink.shareinputdevices` body from the phone says (the shared
/// `glide_bodies.json` pins it for both apps).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PhoneReport {
    State {
        allowed: bool,
        entered: bool,
        /// Not allowed only because the phone is locked (`reason: "locked"`).
        locked: bool,
        /// Not allowed: the person tapped "Not now" on the desk's ask
        /// (`reason: "declined"`).
        declined: bool,
        /// Not allowed only because its accessibility service is off
        /// (`reason: "accessibility"`).
        service_off: bool,
    },
    HandedBack,
    /// A release point (or a body that is neither: `phone_released` drops it).
    Release,
    Ignored,
}

pub(crate) fn classify_phone_packet(packet: &NetworkPacket) -> PhoneReport {
    let reason = packet
        .body
        .get("reason")
        .and_then(serde_json::Value::as_str);
    let release =
        packet.body.contains_key("releaseDeltax") || packet.body.contains_key("releaseDeltay");
    if let (false, Some(allowed)) = (
        release,
        packet
            .body
            .get("allowed")
            .and_then(serde_json::Value::as_bool),
    ) {
        PhoneReport::State {
            allowed,
            entered: allowed
                && packet
                    .body
                    .get("entered")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true),
            locked: !allowed && reason == Some("locked"),
            declined: !allowed && reason == Some("declined"),
            service_off: !allowed && reason == Some("accessibility"),
        }
    } else if packet
        .body
        .get("exitEdge")
        .and_then(serde_json::Value::as_i64)
        == Some(0)
    {
        PhoneReport::HandedBack
    } else if release_deltas(packet).is_some() {
        PhoneReport::Release
    } else {
        PhoneReport::Ignored
    }
}

/// G3 from the phone (`desklink.shareinputdevices`).
pub(crate) fn phone_released(device_id: &str, packet: &NetworkPacket) {
    let Some((dx, dy)) = release_deltas(packet) else {
        log_limited("bad-release", "A release without a position was ignored");
        return;
    };
    static RELEASES: Buckets = Buckets::new(RELEASES_PER_SECOND, Duration::from_secs(1));
    if !RELEASES.allow(device_id) {
        log_limited("release-rate", "Too many releases from a phone; dropped");
        return;
    }
    send_command(Command::Released {
        device: device_id.to_string(),
        dx,
        dy,
    });
}

/// G3's deltas: both present, finite and within [`MAX_DELTA`].
fn release_deltas(packet: &NetworkPacket) -> Option<(f64, f64)> {
    let number = |key: &str| {
        packet
            .body
            .get(key)
            .and_then(serde_json::Value::as_f64)
            .filter(|value| value.is_finite() && value.abs() <= MAX_DELTA)
    };
    Some((number("releaseDeltax")?, number("releaseDeltay")?))
}

/// A reader's end is never dropped for a full queue: the worker waits for
/// room (the reader has nothing else to do), so a dead reader is always
/// noticed and the pointer released.
fn send_closed(generation: u64) -> bool {
    queue().get().is_some_and(|sender| {
        sender
            .send(Command::Captured(generation, Captured::Closed))
            .is_ok()
    })
}

/// Whether an event from reader `from` belongs to the session now open: the
/// reader of a session that was just closed reports its end a moment later,
/// and that must not stop the new one.
fn is_current(from: u64, session: Option<u64>) -> bool {
    session == Some(from)
}

fn send_command(command: Command) -> bool {
    let Some(sender) = queue().get() else {
        return false;
    };
    match sender.try_send(command) {
        Ok(()) => true,
        Err(TrySendError::Full(_)) => {
            log_limited("queue-full", "Glide queue full; an event was dropped");
            true
        }
        Err(TrySendError::Disconnected(_)) => false,
    }
}

/// This computer's name as the phone shows it (its host name).
pub(crate) fn desk_name() -> String {
    hostname::get()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "this computer".to_string())
}

#[cfg(test)]
mod tests;
