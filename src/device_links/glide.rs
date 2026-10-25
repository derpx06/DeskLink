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

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use gtk::gdk::prelude::*;
use gtk::{gdk, gio, glib};
use serde::{Deserialize, Serialize};

use crate::device_links::eis::{self, Captured};
use crate::device_links::limits::{log_limited, Buckets};
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST,
};
use crate::device_links::webrtc::remote_desktop_portal::{
    lookup, object_path, options, pipewire_fd_index, request, token, tuple, PORTAL_BUS,
    PORTAL_PATH, SESSION_INTERFACE,
};

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
/// no answer is a refusal: the pointer comes back where it crossed.
const ENTER_WAIT: Duration = Duration::from_millis(300);
/// After a refusal, the edge is left alone this long: a crossing then is
/// released at once, with nothing sent.
const REFUSED_BACKOFF: Duration = Duration::from_secs(5);
/// Input held while the phone has not accepted yet; beyond this, dropped.
const HELD_MAX: usize = 64;
/// Motion is coalesced: at most one motion packet per this long (250/s).
const MOTION_EVERY: Duration = Duration::from_millis(4);
/// How often the worker re-reads the setting and the phone's link.
const CHECK_EVERY: Duration = Duration::from_millis(250);
/// The worker's nap while a session is armed but the pointer is here.
const ARMED_NAP: Duration = Duration::from_millis(50);
/// Re-arming the barrier (after the compositor disabled it) at most this
/// often per minute; then glide stops with a reason instead of looping.
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

/// A screen area, in the portal's zone coordinates (logical pixels).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Zone {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// The barrier on the chosen edge, and the zone it borders.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Barrier {
    edge: Edge,
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
    zone: Zone,
}

impl Barrier {
    /// On the outer side of the zone furthest towards `edge` (as the
    /// portal requires: on the union's boundary, inside one zone).
    fn on(edge: Edge, zones: &[Zone]) -> Option<Self> {
        let zone = *match edge {
            Edge::Left => zones.iter().min_by_key(|zone| zone.x),
            Edge::Right => zones.iter().max_by_key(|zone| zone.x + zone.width),
            Edge::Top => zones.iter().min_by_key(|zone| zone.y),
            Edge::Bottom => zones.iter().max_by_key(|zone| zone.y + zone.height),
        }?;
        if zone.width <= 0 || zone.height <= 0 {
            return None;
        }
        let (right, bottom) = (zone.x + zone.width, zone.y + zone.height);
        let (x1, y1, x2, y2) = match edge {
            Edge::Left => (zone.x, zone.y, zone.x, bottom - 1),
            Edge::Right => (right, zone.y, right, bottom - 1),
            Edge::Top => (zone.x, zone.y, right - 1, zone.y),
            Edge::Bottom => (zone.x, bottom, right - 1, bottom),
        };
        Some(Self {
            edge,
            x1,
            y1,
            x2,
            y2,
            zone,
        })
    }

    fn length(&self) -> i64 {
        i64::from(if self.edge.vertical() {
            self.zone.height
        } else {
            self.zone.width
        })
    }

    /// G1: where the pointer crossed, relative to the barrier's start.
    fn enter_packet(&self, cursor: (f64, f64)) -> NetworkPacket {
        let along = |offset: f64| (offset.round() as i64).clamp(0, self.length());
        let (dx, dy) = (cursor.0 - f64::from(self.x1), cursor.1 - f64::from(self.y1));
        let (dx, dy) = if self.edge.vertical() {
            (dx.round() as i64, along(dy))
        } else {
            (along(dx), dy.round() as i64)
        };
        let mut packet = NetworkPacket::new(PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST);
        packet.set("exitEdge", self.edge.qt());
        packet.set("deltax", dx);
        packet.set("deltay", dy);
        packet.set("edgeLength", self.length());
        packet
    }

    /// A point just inside the zone, at `(x, y)` along the edge.
    fn inside(&self, x: f64, y: f64) -> (f64, f64) {
        /// Pixels inside the edge, so a jitter does not cross back at once.
        const INSET: i32 = 2;
        let zone = self.zone;
        let clamp = |value: f64, low: i32, high: i32| value.clamp(f64::from(low), f64::from(high));
        let x = clamp(x, zone.x, zone.x + zone.width - 1);
        let y = clamp(y, zone.y, zone.y + zone.height - 1);
        match self.edge {
            Edge::Left => (f64::from(zone.x + INSET), y),
            Edge::Right => (f64::from(zone.x + zone.width - 1 - INSET), y),
            Edge::Top => (x, f64::from(zone.y + INSET)),
            Edge::Bottom => (x, f64::from(zone.y + zone.height - 1 - INSET)),
        }
    }

    /// G3: the barrier's start plus the phone's release deltas.
    fn release_point(&self, dx: f64, dy: f64) -> (f64, f64) {
        self.inside(f64::from(self.x1) + dx, f64::from(self.y1) + dy)
    }
}

/// Held modifiers, from the captured keys themselves.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub(crate) struct Mods {
    shift: bool,
    ctrl: bool,
    alt: bool,
    altgr: bool,
    meta: bool,
    caps: bool,
}

/// evdev codes.
const KEY_ESC: u32 = 1;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

impl Mods {
    /// Updates for a modifier key; false when `code` is not one.
    fn update(&mut self, code: u32, pressed: bool) -> bool {
        let held = match code {
            42 | 54 => &mut self.shift,
            29 | 97 => &mut self.ctrl,
            56 => &mut self.alt,
            100 => &mut self.altgr,
            125 | 126 => &mut self.meta,
            58 => return true, // Caps Lock: its state comes as `Locked`.
            _ => return false,
        };
        *held = pressed;
        true
    }

    /// The GDK modifier state that picks a key's level.
    fn level_state(self) -> gdk::ModifierType {
        let mut state = gdk::ModifierType::empty();
        if self.shift {
            state |= gdk::ModifierType::SHIFT_MASK;
        }
        if self.caps {
            state |= gdk::ModifierType::LOCK_MASK;
        }
        if self.altgr {
            // XKB's Mod5 is AltGr (ISO_Level3_Shift) on common layouts.
            state |= gdk::ModifierType::from_bits_retain(1 << 7);
        }
        state
    }
}

/// What the state machine asks the worker to do.
#[derive(Debug, Clone, PartialEq)]
enum Out {
    /// A packet for the phone.
    Send(NetworkPacket),
    /// The pointer just entered the phone: its clipboard follows (G6).
    Clipboard,
    /// End the capture, the pointer at `(x, y)`.
    Release { activation: u32, x: f64, y: f64 },
    /// Gliding started or ended (the indicator).
    Gliding(bool),
    /// The phone did not accept the pointer: tell the person (once).
    Refused,
}

/// One activation of the barrier, start to release (unit-tested).
#[derive(Debug)]
struct Glide {
    barrier: Barrier,
    /// The portal's activation id while the pointer is on the phone.
    active: Option<u32>,
    /// Where the pointer crossed: Esc+Super brings it back there.
    crossed: (f64, f64),
    /// Captured before the Activated signal arrived.
    pending: VecDeque<(Instant, Captured)>,
    mods: Mods,
    /// Motion not sent yet, and when motion was last sent.
    motion: (f64, f64),
    motion_sent: Option<Instant>,
    dropped: u64,
    /// The left button is down on the phone (a drag): every way out lets it
    /// go first, so the phone is never left holding a touch.
    left_down: bool,
    /// The pointer crossed and the phone has not accepted yet (since when):
    /// nothing is streamed, input is held.
    entering: Option<Instant>,
    /// Input held while entering, at most [`HELD_MAX`].
    held: VecDeque<Captured>,
    /// Production: the phone must accept before anything streams. Older
    /// tests of the streaming itself skip it.
    accept_needed: bool,
    /// A refusal: crossings are released at once until then.
    backoff_until: Option<Instant>,
    /// The person was told of a refusal and no accept has come since.
    told: bool,
}

impl Glide {
    fn new(barrier: Barrier) -> Self {
        Self {
            accept_needed: true,
            ..Self::accepting(barrier)
        }
    }

    /// A glide that streams at once (tests of the streaming itself).
    fn accepting(barrier: Barrier) -> Self {
        Self {
            entering: None,
            held: VecDeque::new(),
            accept_needed: false,
            backoff_until: None,
            told: false,
            barrier,
            active: None,
            crossed: (0.0, 0.0),
            pending: VecDeque::new(),
            mods: Mods::default(),
            motion: (0.0, 0.0),
            motion_sent: None,
            dropped: 0,
            left_down: false,
        }
    }

    /// The release of a held left button, once, as the first thing sent when
    /// the pointer leaves the phone.
    fn let_go(&mut self) -> Option<Out> {
        std::mem::take(&mut self.left_down)
            .then(|| Out::Send(mousepad(|packet| packet.set("singlerelease", true))))
    }

    #[cfg(test)]
    fn gliding(&self) -> bool {
        self.active.is_some()
    }

    /// The portal's Activated: tell the phone where, then what came early.
    fn activated(
        &mut self,
        activation: u32,
        cursor: (f64, f64),
        keys: &dyn Fn(u32, Mods) -> Option<gdk::Key>,
        now: Instant,
    ) -> Vec<Out> {
        // A refusal just now: the pointer goes straight back, nothing is
        // sent, nothing streams.
        if self.backoff_until.is_some_and(|until| now < until) {
            self.pending.clear();
            let (x, y) = self.barrier.inside(cursor.0, cursor.1);
            return vec![Out::Release { activation, x, y }];
        }
        let was = self.active.replace(activation);
        // Already on the phone (and accepted) when the portal activates again:
        // nothing new to wait for.
        let waiting = was.is_none() || self.entering.is_some();
        self.crossed = cursor;
        self.mods = Mods::default();
        self.motion = (0.0, 0.0);
        self.held.clear();
        self.entering = (self.accept_needed && waiting).then_some(now);
        let mut out = vec![Out::Send(self.barrier.enter_packet(cursor))];
        if was.is_none() {
            out.push(Out::Gliding(true));
            out.push(Out::Clipboard);
        }
        // Only what came just before the crossing belongs to it: input that
        // arrived after an earlier release and waited (a stale tail of the
        // last capture, say) is not replayed onto the phone.
        for (at, captured) in std::mem::take(&mut self.pending) {
            if now.saturating_duration_since(at) <= PENDING_FRESH {
                out.extend(self.input(captured, keys, now));
            }
        }
        out
    }

    /// The compositor ended the capture itself (lock screen, session end).
    fn deactivated(&mut self) -> Vec<Out> {
        self.pending.clear();
        self.held.clear();
        self.entering = None;
        if self.active.take().is_none() {
            return Vec::new();
        }
        let mut out: Vec<Out> = self.let_go().into_iter().collect();
        out.push(Out::Send(Self::taken_back()));
        out.push(Out::Gliding(false));
        out
    }

    /// G4: the desk takes the pointer back (Esc+Super, link down, glide
    /// off), where it crossed.
    fn take_back(&mut self) -> Vec<Out> {
        self.pending.clear();
        self.held.clear();
        self.entering = None;
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        let (x, y) = self.barrier.inside(self.crossed.0, self.crossed.1);
        let mut out: Vec<Out> = self.let_go().into_iter().collect();
        out.extend([
            Out::Send(Self::taken_back()),
            Out::Release { activation, x, y },
            Out::Gliding(false),
        ]);
        out
    }

    fn taken_back() -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST);
        packet.set("exitEdge", 0);
        packet
    }

    /// G3: the phone hands the pointer back.
    fn phone_released(&mut self, dx: f64, dy: f64, now: Instant) -> Vec<Out> {
        self.pending.clear();
        // Before it accepted, a hand-back is a refusal: back where it crossed.
        if self.entering.is_some() {
            return self.refused(now, false);
        }
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        let (x, y) = self.barrier.release_point(dx, dy);
        let mut out: Vec<Out> = self.let_go().into_iter().collect();
        out.extend([Out::Release { activation, x, y }, Out::Gliding(false)]);
        out
    }

    /// The phone accepted the pointer: what was held goes out, in order.
    fn accepted(&mut self, keys: &dyn Fn(u32, Mods) -> Option<gdk::Key>, now: Instant) -> Vec<Out> {
        if self.entering.take().is_none() {
            return Vec::new();
        }
        self.told = false;
        self.mods = Mods::default();
        let mut out = Vec::new();
        for captured in std::mem::take(&mut self.held) {
            out.extend(self.input(captured, keys, now));
        }
        out.extend(self.flush_motion(now));
        out
    }

    /// The phone did not accept (it handed the pointer back, said it does not
    /// allow glide, or did not answer in time): the pointer is released where
    /// it crossed, nothing was streamed, and this edge is left alone for
    /// [`REFUSED_BACKOFF`]. `tell_phone`: the phone is told to forget the
    /// entry (it never answered).
    fn refused(&mut self, now: Instant, tell_phone: bool) -> Vec<Out> {
        self.pending.clear();
        self.held.clear();
        self.entering = None;
        self.motion = (0.0, 0.0);
        self.backoff_until = Some(now + REFUSED_BACKOFF);
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        let (x, y) = self.barrier.inside(self.crossed.0, self.crossed.1);
        let mut out = Vec::new();
        if tell_phone {
            out.push(Out::Send(Self::taken_back()));
        }
        out.extend([Out::Release { activation, x, y }, Out::Gliding(false)]);
        if !std::mem::replace(&mut self.told, true) {
            out.push(Out::Refused);
        }
        out
    }

    /// The phone says it no longer allows glide (a state report): a pointer
    /// on it, or on its way, comes back at once.
    fn disallowed(&mut self, now: Instant) -> Vec<Out> {
        if self.entering.is_some() {
            return self.refused(now, false);
        }
        self.take_back()
    }

    /// Motion waiting longer than [`MOTION_EVERY`] goes out.
    fn tick(&mut self, now: Instant) -> Vec<Out> {
        if let Some(since) = self.entering {
            // No answer in time is a refusal.
            return if now.saturating_duration_since(since) >= ENTER_WAIT {
                self.refused(now, true)
            } else {
                Vec::new()
            };
        }
        let due = self
            .motion_sent
            .is_none_or(|sent| now.duration_since(sent) >= MOTION_EVERY);
        if due {
            self.flush_motion(now)
        } else {
            Vec::new()
        }
    }

    fn flush_motion(&mut self, now: Instant) -> Vec<Out> {
        let (dx, dy) = std::mem::take(&mut self.motion);
        if dx == 0.0 && dy == 0.0 {
            return Vec::new();
        }
        self.motion_sent = Some(now);
        vec![Out::Send(mousepad(|packet| {
            packet.set("dx", dx);
            packet.set("dy", dy);
        }))]
    }

    fn input(
        &mut self,
        captured: Captured,
        keys: &dyn Fn(u32, Mods) -> Option<gdk::Key>,
        now: Instant,
    ) -> Vec<Out> {
        if self.active.is_none() {
            if matches!(captured, Captured::Start(_) | Captured::Closed) {
                return Vec::new();
            }
            if self.pending.len() < PENDING_MAX {
                self.pending.push_back((now, captured));
            } else {
                self.dropped += 1;
            }
            return Vec::new();
        }
        if self.entering.is_some() {
            // Nothing streams before the phone accepts; Esc+Super still works.
            if let Captured::Key(code, pressed) = captured {
                if !self.mods.update(code, pressed) && pressed && code == KEY_ESC && self.mods.meta
                {
                    return self.take_back();
                }
            }
            if self.held.len() < HELD_MAX {
                self.held.push_back(captured);
            } else {
                self.dropped += 1;
            }
            return Vec::new();
        }
        if let Captured::Motion(dx, dy) = captured {
            if dx.is_finite() && dy.is_finite() {
                self.motion.0 += dx;
                self.motion.1 += dy;
            }
            return self.tick(now);
        }
        // Order matters: motion first, then the click or key.
        let mut out = self.flush_motion(now);
        match captured {
            Captured::Button(code, pressed) => {
                let key = match (code, pressed) {
                    (BTN_LEFT, true) => {
                        self.left_down = true;
                        "singlehold"
                    }
                    (BTN_LEFT, false) => {
                        self.left_down = false;
                        "singlerelease"
                    }
                    (BTN_RIGHT, true) => "rightclick",
                    (BTN_MIDDLE, true) => "middleclick",
                    (BTN_SIDE, true) => "backclick",
                    (BTN_EXTRA, true) => "forwardclick",
                    _ => return out,
                };
                out.push(Out::Send(mousepad(|packet| packet.set(key, true))));
            }
            Captured::Scroll(dx, dy) if dx.is_finite() && dy.is_finite() => {
                out.push(Out::Send(scroll(dx, dy)));
            }
            Captured::Discrete(dx, dy) => {
                // KDE's conversion: 15° per click, y inverted.
                let step = 15.0 / 120.0;
                out.push(Out::Send(scroll(
                    f64::from(dx) * step,
                    -f64::from(dy) * step,
                )));
            }
            Captured::Locked(mask) => self.mods.caps = mask & 2 != 0,
            Captured::Key(code, pressed) => {
                if self.mods.update(code, pressed) || !pressed {
                    return out;
                }
                if code == KEY_ESC && self.mods.meta {
                    out.extend(self.take_back());
                    return out;
                }
                if let Some(packet) =
                    keys(code, self.mods).and_then(|key| key_packet(key, self.mods))
                {
                    out.push(Out::Send(packet));
                }
            }
            _ => {}
        }
        out
    }
}

fn mousepad(build: impl FnOnce(&mut NetworkPacket)) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_MOUSEPAD_REQUEST);
    build(&mut packet);
    packet
}

fn scroll(dx: f64, dy: f64) -> NetworkPacket {
    mousepad(|packet| {
        packet.set("scroll", true);
        packet.set("dx", dx);
        packet.set("dy", dy);
    })
}

/// A key press for the phone: a special key's number or the text it types,
/// with the modifiers held.
fn key_packet(key: gdk::Key, mods: Mods) -> Option<NetworkPacket> {
    let special = crate::remote_control::map_gdk_key_to_special(key);
    let text = key
        .to_unicode()
        .filter(|character| !character.is_control())
        .map(|character| character.to_string());
    if special == 0 && text.is_none() {
        return None;
    }
    Some(mousepad(|packet| {
        if special != 0 {
            packet.set("specialKey", special);
        } else if let Some(text) = text {
            packet.set("key", text);
        }
        packet.set("shift", mods.shift);
        packet.set("ctrl", mods.ctrl);
        packet.set("alt", mods.alt);
        packet.set("super", mods.meta);
    }))
}

/// The key an evdev `code` types with `mods`, through this desktop's keymap
/// (GDK, on the main thread; cached).
fn desk_key(code: u32, mods: Mods) -> Option<gdk::Key> {
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

/// What the keymap said for each (key, level), for [`KEY_TTL`] (the user may
/// switch layout), at most [`KEY_CACHE_MAX`] of them.
#[derive(Default)]
struct KeyCache(HashMap<(u32, Mods), (Option<u32>, Instant)>);

/// How long a keymap answer is kept.
const KEY_TTL: Duration = Duration::from_secs(30);

impl KeyCache {
    fn get(&self, key: (u32, Mods), now: Instant) -> Option<Option<u32>> {
        self.0
            .get(&key)
            .filter(|(_, at)| now.duration_since(*at) < KEY_TTL)
            .map(|(found, _)| *found)
    }

    fn put(&mut self, key: (u32, Mods), found: Option<u32>, now: Instant) {
        if self.0.len() >= KEY_CACHE_MAX {
            self.0.clear();
        }
        self.0.insert(key, (found, now));
    }
}

use glib::translate::IntoGlib;

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
}

static STATUS: Mutex<Status> = Mutex::new(Status {
    availability: Availability::Checking,
    gliding: None,
    problem: None,
});

pub(crate) fn status() -> Status {
    STATUS
        .lock()
        .map(|status| status.clone())
        .unwrap_or(Status {
            availability: Availability::Unavailable("Glide stopped working"),
            gliding: None,
            problem: None,
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

fn queue() -> &'static OnceLock<SyncSender<Command>> {
    static QUEUE_SENDER: OnceLock<SyncSender<Command>> = OnceLock::new();
    &QUEUE_SENDER
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

/// What each phone last reported about glide (`allowed`). Unknown until it
/// reports, and forgotten whenever its link is not ready: the phone reports
/// on every feature-ready.
static ALLOWED: Mutex<Option<HashMap<String, bool>>> = Mutex::new(None);

/// Whether the phone has said it allows glide: `None` until it reports (an
/// older phone never does).
pub(crate) fn phone_allows(device_id: &str) -> Option<bool> {
    ALLOWED.lock().ok().and_then(|known| {
        known
            .as_ref()
            .and_then(|known| known.get(device_id).copied())
    })
}

fn set_allowed(device_id: &str, allowed: Option<bool>) {
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
    let release =
        packet.body.contains_key("releaseDeltax") || packet.body.contains_key("releaseDeltay");
    if let (false, Some(allowed)) = (
        release,
        packet
            .body
            .get("allowed")
            .and_then(serde_json::Value::as_bool),
    ) {
        set_allowed(device_id, Some(allowed));
        send_command(Command::State {
            device: device_id.to_string(),
            allowed,
            entered: allowed
                && packet
                    .body
                    .get("entered")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true),
        });
    } else if packet
        .body
        .get("exitEdge")
        .and_then(serde_json::Value::as_i64)
        == Some(0)
    {
        send_command(Command::HandedBack {
            device: device_id.to_string(),
        });
    } else {
        phone_released(device_id, packet);
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

/// A portal signal, queued by its handler for the worker.
#[derive(Debug, Clone, PartialEq)]
enum Signal {
    Activated { id: u32, cursor: (f64, f64) },
    Deactivated,
    Disabled,
    ZonesChanged,
    Closed,
}

/// The live InputCapture session. Dropping it closes it.
struct Portal {
    /// Told to this session's EIS reader, so a late word from an older one
    /// is ignored.
    generation: u64,
    connection: gio::DBusConnection,
    handle: String,
    signals: std::rc::Rc<std::cell::RefCell<VecDeque<Signal>>>,
    subscriptions: Vec<gio::SignalSubscriptionId>,
    enabled: bool,
    /// When the barrier was last (re)armed, for [`REARMS_PER_MINUTE`].
    armed_at: VecDeque<Instant>,
}

impl Portal {
    /// CreateSession (the consent dialog), and its signals and EIS socket.
    fn open(context: &glib::MainContext, connection: &gio::DBusConnection) -> Result<Self, String> {
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let created = request(
            context,
            connection,
            &cancel,
            PORTAL_PATH,
            INTERFACE,
            "CreateSession",
            tuple([
                "".to_variant(),
                options(|dict| {
                    dict.insert("handle_token", token());
                    dict.insert("session_handle_token", token());
                    // Keyboard and pointer.
                    dict.insert("capabilities", 3u32);
                }),
            ]),
        )?;
        let handle = lookup::<String>(&created, "session_handle")?
            .ok_or_else(|| "The desktop did not start input capture".to_string())?;
        let granted = lookup::<u32>(&created, "capabilities")?.unwrap_or(0);
        let signals = std::rc::Rc::new(std::cell::RefCell::new(VecDeque::new()));
        static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let generation = GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut portal = Self {
            generation,
            connection: connection.clone(),
            handle,
            signals,
            subscriptions: Vec::new(),
            enabled: false,
            armed_at: VecDeque::new(),
        };
        if granted & 3 != 3 {
            return Err("The desktop allowed only part of the input".to_string());
        }
        portal.subscribe();
        let socket = portal.connect_to_eis()?;
        eis::spawn_receiver(socket, "DeskLink", move |captured| {
            if matches!(captured, Captured::Closed) {
                send_closed(generation)
            } else {
                send_command(Command::Captured(generation, captured))
            }
        })?;
        Ok(portal)
    }

    fn subscribe(&mut self) {
        let queue = std::rc::Rc::clone(&self.signals);
        let handle = self.handle.clone();
        let id = self.connection.signal_subscribe(
            Some(PORTAL_BUS),
            Some(INTERFACE),
            None,
            Some(PORTAL_PATH),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, member, parameters| {
                crate::guard("glide signal", (), || {
                    if parameters.child_value(0).str() != Some(handle.as_str()) {
                        return;
                    }
                    let values = parameters.child_value(1);
                    let signal = match member {
                        "Activated" => {
                            let id = lookup::<u32>(&values, "activation_id").ok().flatten();
                            let cursor = lookup::<(f64, f64)>(&values, "cursor_position")
                                .ok()
                                .flatten();
                            match (id, cursor) {
                                (Some(id), Some(cursor)) => Signal::Activated { id, cursor },
                                _ => return,
                            }
                        }
                        "Deactivated" => Signal::Deactivated,
                        "Disabled" => Signal::Disabled,
                        "ZonesChanged" => Signal::ZonesChanged,
                        _ => return,
                    };
                    let mut queue = queue.borrow_mut();
                    if queue.len() < PENDING_MAX {
                        queue.push_back(signal);
                    }
                })
            },
        );
        self.subscriptions.push(id);
        let queue = std::rc::Rc::clone(&self.signals);
        let id = self.connection.signal_subscribe(
            Some(PORTAL_BUS),
            Some(SESSION_INTERFACE),
            Some("Closed"),
            Some(&self.handle),
            None,
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, _| queue.borrow_mut().push_back(Signal::Closed),
        );
        self.subscriptions.push(id);
    }

    fn call(&self, method: &str, parameters: glib::Variant) -> Result<glib::Variant, String> {
        self.connection
            .call_sync(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                INTERFACE,
                method,
                Some(&parameters),
                None,
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| format!("Input capture {method} failed: {error}"))
    }

    fn connect_to_eis(&self) -> Result<std::os::fd::OwnedFd, String> {
        let (reply, list) = self
            .connection
            .call_with_unix_fd_list_sync(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                INTERFACE,
                "ConnectToEIS",
                Some(&tuple([object_path(&self.handle)?, options(|_| {})])),
                None,
                gio::DBusCallFlags::NONE,
                CALL_TIMEOUT_MS * 5,
                None::<&gio::UnixFDList>,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| format!("Input capture ConnectToEIS failed: {error}"))?;
        let index = pipewire_fd_index(&reply)?;
        let raw = list
            .ok_or_else(|| "The desktop sent no input socket".to_string())?
            .get(index)
            .map_err(|error| format!("Could not receive the input socket: {error}"))?;
        // SAFETY: `get` returned a fresh descriptor this side now owns.
        Ok(unsafe { std::os::fd::FromRawFd::from_raw_fd(raw) })
    }

    /// GetZones → SetPointerBarriers on `edge` → Enable.
    fn arm(&mut self, context: &glib::MainContext, edge: Edge) -> Result<Barrier, String> {
        let now = Instant::now();
        self.armed_at
            .retain(|at| now.duration_since(*at) < Duration::from_secs(60));
        if self.armed_at.len() >= REARMS_PER_MINUTE {
            return Err("The desktop keeps turning glide off".to_string());
        }
        self.armed_at.push_back(now);
        let cancel = std::sync::atomic::AtomicBool::new(false);
        let session = object_path(&self.handle)?;
        let zones = request(
            context,
            &self.connection,
            &cancel,
            PORTAL_PATH,
            INTERFACE,
            "GetZones",
            tuple([
                session.clone(),
                options(|dict| dict.insert("handle_token", token())),
            ]),
        )?;
        let zone_set = lookup::<u32>(&zones, "zone_set")?.unwrap_or(0);
        let zones: Vec<Zone> = lookup::<Vec<(u32, u32, i32, i32)>>(&zones, "zones")?
            .unwrap_or_default()
            .into_iter()
            .take(64)
            .map(|(width, height, x, y)| Zone {
                x,
                y,
                width: width.min(1 << 20) as i32,
                height: height.min(1 << 20) as i32,
            })
            .collect();
        let barrier = Barrier::on(edge, &zones)
            .ok_or_else(|| "This screen has no edge to glide from".to_string())?;
        let position = glib::VariantDict::new(None);
        position.insert("barrier_id", 1u32);
        position.insert("position", (barrier.x1, barrier.y1, barrier.x2, barrier.y2));
        let set = request(
            context,
            &self.connection,
            &cancel,
            PORTAL_PATH,
            INTERFACE,
            "SetPointerBarriers",
            tuple([
                session.clone(),
                options(|dict| dict.insert("handle_token", token())),
                glib::Variant::array_from_iter_with_type(
                    glib::VariantTy::VARDICT,
                    [position.end()],
                ),
                zone_set.to_variant(),
            ]),
        )?;
        if lookup::<Vec<u32>>(&set, "failed_barriers")?.is_some_and(|failed| !failed.is_empty()) {
            return Err("The desktop refused that edge".to_string());
        }
        self.call("Enable", tuple([session, options(|_| {})]))?;
        self.enabled = true;
        Ok(barrier)
    }

    fn disable(&mut self) {
        if let Ok(session) = object_path(&self.handle) {
            let _ = self.call("Disable", tuple([session, options(|_| {})]));
        }
        self.enabled = false;
    }

    /// Ends the capture; false when the desktop refused (the caller then
    /// disables the session, which ends it too).
    fn release(&self, activation: u32, x: f64, y: f64) -> bool {
        let Ok(session) = object_path(&self.handle) else {
            return false;
        };
        if let Err(error) = self.call(
            "Release",
            tuple([
                session,
                options(|dict| {
                    dict.insert("activation_id", activation);
                    dict.insert("cursor_position", (x, y));
                }),
            ]),
        ) {
            log_limited("release", &error);
            return false;
        }
        true
    }
}

impl Drop for Portal {
    fn drop(&mut self) {
        for id in self.subscriptions.drain(..) {
            self.connection.signal_unsubscribe(id);
        }
        let _ = self.connection.call_sync(
            Some(PORTAL_BUS),
            &self.handle,
            SESSION_INTERFACE,
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        );
    }
}

/// Whether this session can glide: Wayland, and a portal with InputCapture
/// for pointer and keyboard.
fn check_availability(connection: &gio::DBusConnection) -> Availability {
    // The window's own backend: X11 (or forced to it) cannot capture.
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some()
        && !std::env::var("GDK_BACKEND").is_ok_and(|backend| backend.starts_with("x11"));
    if !wayland {
        return Availability::Unavailable("Glide needs a Wayland session; this one is X11");
    }
    let supported = connection
        .call_sync(
            Some(PORTAL_BUS),
            PORTAL_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&(INTERFACE, "SupportedCapabilities").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            CALL_TIMEOUT_MS,
            None::<&gio::Cancellable>,
        )
        .ok()
        .and_then(|reply| reply.child_value(0).as_variant())
        .and_then(|value| value.get::<u32>());
    match supported {
        Some(capabilities) if capabilities & 3 == 3 => Availability::Available,
        _ => Availability::Unavailable(
            "This desktop does not offer input capture (it needs GNOME 45 or later, or \
             KDE Plasma 6.1 or later)",
        ),
    }
}

/// Plain words for a portal failure.
fn plain(error: &str) -> String {
    if error.contains("cancelled") || error.contains("denied") {
        "Glide was not allowed. Turn it off and on again to ask again.".to_string()
    } else if error.starts_with("Input capture") || error.contains("portal") {
        "This desktop's input capture did not answer".to_string()
    } else {
        error.to_string()
    }
}

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

/// The barrier is armed only for a phone whose link is ready, that is not
/// silent, that has said it allows glide, and while glide was not refused.
fn wants_barrier(refused: bool, silent: bool, ready: bool, allowed: bool) -> bool {
    !refused && !silent && ready && allowed
}

/// Whether the phone has said it allows glide (debug builds: or a test
/// device, which has no phone to say it).
fn allows_glide(device: &str) -> bool {
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
fn clipboard_packet(text: String) -> Option<NetworkPacket> {
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
            let text = arboard::Clipboard::new()
                .and_then(|mut clipboard| clipboard.get_text())
                .unwrap_or_default();
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
enum Event {
    Signal(Signal),
    /// The EIS reader stopped (the compositor closed the socket).
    ReaderClosed,
}

/// What the worker does with the session after an [`Event`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum After {
    Keep,
    /// Disable, so the next check sets the barrier again (new screens).
    Rearm,
    /// Disable and drop the session; glide stops with this reason.
    Close(&'static str),
}

/// The outs and the session change for `event` (tested). A capture that may
/// still be live is always released, never just forgotten.
fn react(event: Event, glide: Option<&mut Glide>) -> (Vec<Out>, After) {
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
            After::Close("The desktop stopped glide. Turn it off and on again to restart it.")
        }
        Event::ReaderClosed => {
            if let Some(glide) = glide {
                outs = glide.take_back();
            }
            After::Close(
                "Glide lost its connection to the desktop. Turn it off and on again to restart it.",
            )
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
static SHUTDOWN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set by the worker once it has released and closed everything.
static ENDED: (Mutex<bool>, std::sync::Condvar) = (Mutex::new(false), std::sync::Condvar::new());

/// How long [`shutdown`] waits for the pointer to be given back.
const SHUTDOWN_WAIT: Duration = Duration::from_millis(2_500);

/// How long a phone may be silent while the pointer is on it. A phone that
/// is there answers the desk's 5 s liveness probe at once (the desk is the
/// one talking during glide), so silence past the probe's time plus three
/// seconds means the link is gone: the pointer comes back then, instead of
/// waiting out the 15 s the general rule allows.
pub(crate) const GLIDE_SILENT_AFTER: Duration =
    Duration::from_secs(crate::device_links::webrtc::coordinator::PEER_PROBE_AFTER.as_secs() + 3);

/// Whether a phone the pointer is on has gone quiet for too long. A dozing
/// phone (screen off, frozen by its OS) is silent on the data channel by
/// nature and is kept alive by the LAN heartbeat, so silence alone never
/// hands the pointer back from one.
fn link_silent(gliding: bool, silence: Option<Duration>, dozing: bool) -> bool {
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
struct State {
    portal: Option<Portal>,
    glide: Option<Glide>,
    /// The setting the session was made for.
    current: Option<GlideSetting>,
    /// The user (or the desktop) said no to it: not asked again until the
    /// setting changes.
    refused: bool,
    /// Arming failed this many times in a row, and when to try again.
    arm_failures: u32,
    retry_at: Option<Instant>,
    /// A refusal's back-off and whether the person was told, kept across
    /// re-armed sessions: (until, told).
    refusal: (Option<Instant>, bool),
}

/// What a failed arming leads to: the desktop may be busy for a moment (a
/// lock screen, a portal restarting), so it is tried again a few times, then
/// glide says it is not working and waits for the user.
#[derive(Debug, PartialEq, Eq)]
enum ArmNext {
    Retry(Duration),
    GiveUp,
}

const ARM_RETRIES: u32 = 3;
const ARM_RETRY_AFTER: Duration = Duration::from_secs(5);

fn after_arm_error(failures: u32) -> ArmNext {
    if failures < ARM_RETRIES {
        ArmNext::Retry(ARM_RETRY_AFTER)
    } else {
        ArmNext::GiveUp
    }
}

impl State {
    /// Every way out ends here: the pointer goes back where it crossed, the
    /// phone is told, the session is disabled and closed. Used when the
    /// worker ends, however it ends.
    fn release_all(&mut self) {
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
fn nap(state: &State) -> Duration {
    if state
        .glide
        .as_ref()
        .is_some_and(|glide| glide.active.is_some())
    {
        MOTION_EVERY
    } else if state.portal.is_some() {
        ARMED_NAP
    } else {
        CHECK_EVERY
    }
}

fn worker(context: &glib::MainContext, commands: &Receiver<Command>) {
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
    let mut state = State {
        portal: None,
        glide: None,
        current: None,
        refused: false,
        arm_failures: 0,
        retry_at: None,
        refusal: (None, false),
    };
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
                After::Rearm => {
                    if let Some(portal) = state.portal.as_mut() {
                        portal.disable();
                    }
                }
                After::Close(problem) => {
                    if let Some(mut closing) = state.portal.take() {
                        closing.disable();
                    }
                    state.glide = None;
                    state.refused = true;
                    set_status(|status| {
                        status.gliding = None;
                        status.problem = Some(problem.to_string());
                    });
                }
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

/// Every [`CHECK_EVERY`]: the setting and the link are compared with what
/// the session was made for, and the session follows.
fn check(
    context: &glib::MainContext,
    connection: &gio::DBusConnection,
    state: &mut State,
    outs: &mut Vec<Out>,
) {
    let setting = crate::device_links::config::app_settings().glide;
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
        // Another phone or off: a new session (and consent) for it.
        if setting.as_ref().map(|s| &s.device) != state.current.as_ref().map(|s| &s.device) {
            state.portal = None;
            state.glide = None;
        }
        if let Some(portal) = state.portal.as_mut() {
            // A new edge: barriers are replaced on a disabled session.
            portal.disable();
            portal.armed_at.clear();
            state.glide = None;
        }
        state.current = setting;
        state.refusal = (None, false);
        state.refused = false;
        state.arm_failures = 0;
        state.retry_at = None;
        set_status(|status| {
            status.problem = None;
            status.gliding = None;
        });
    }
    let gliding = state
        .glide
        .as_ref()
        .is_some_and(|glide| glide.active.is_some());
    let silent = link_silent(
        gliding,
        state
            .current
            .as_ref()
            .and_then(|setting| inbound_silence(&setting.device)),
        state
            .current
            .as_ref()
            .is_some_and(|setting| is_dozing(&setting.device)),
    );
    let is_ready = state
        .current
        .as_ref()
        .is_some_and(|setting| ready(&setting.device));
    // The phone reports on every feature-ready: what it said before the link
    // went away is not known any more.
    if let (false, Some(setting)) = (is_ready, state.current.as_ref()) {
        set_allowed(&setting.device, None);
    }
    // The barrier is armed only while the phone says it allows glide: an
    // unknown state (an older phone, no report yet) is not a yes.
    let allowed = state
        .current
        .as_ref()
        .is_some_and(|setting| allows_glide(&setting.device));
    let want = wants_barrier(state.refused, silent, is_ready, allowed);
    if want && state.portal.is_none() {
        match Portal::open(context, connection) {
            Ok(opened) => state.portal = Some(opened),
            Err(error) => {
                log_limited("open", &error);
                state.refused = true;
                set_status(|status| status.problem = Some(plain(&error)));
            }
        }
    }
    if let (Some(open), Some(setting)) = (state.portal.as_mut(), state.current.as_ref()) {
        if want && !open.enabled && state.retry_at.is_none_or(|at| Instant::now() >= at) {
            match open.arm(context, setting.edge) {
                Ok(barrier) => {
                    let mut glide = Glide::new(barrier);
                    #[cfg(debug_assertions)]
                    if test_device(&setting.device) {
                        // A test device has no phone to answer.
                        glide.accept_needed = false;
                    }
                    (glide.backoff_until, glide.told) = state.refusal;
                    state.glide = Some(glide);
                    state.arm_failures = 0;
                    state.retry_at = None;
                    set_status(|status| status.problem = None);
                }
                Err(error) => {
                    log_limited("arm", &error);
                    state.arm_failures += 1;
                    // The rate limit is the desktop turning glide off over
                    // and over: not retried.
                    let next = if error.contains("keeps turning glide off") {
                        ArmNext::GiveUp
                    } else {
                        after_arm_error(state.arm_failures)
                    };
                    match next {
                        ArmNext::Retry(after) => state.retry_at = Some(Instant::now() + after),
                        ArmNext::GiveUp => {
                            state.refused = true;
                            set_status(|status| status.problem = Some(plain(&error)));
                        }
                    }
                }
            }
        } else if !want && open.enabled {
            // The Release (and telling the phone) before Disable.
            let mut back = std::mem::take(outs);
            if let Some(glide) = state.glide.as_mut() {
                back.extend(glide.take_back());
            }
            perform(Some(setting.device.as_str()), back, Some(&mut *open));
            open.disable();
        }
    }
    if state.current.is_none() {
        state.portal = None;
        state.glide = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glide_packets_fit_one_wire_message() {
        let barrier = Barrier::on(Edge::Right, &screen()).unwrap();
        assert!(crate::device_links::webrtc::fits_on_wire(
            &barrier.enter_packet((f64::MAX, f64::MAX))
        ));
        let quotes = "\"".repeat(MAX_CLIPBOARD_BYTES);
        assert!(clipboard_packet(quotes)
            .is_some_and(|packet| { crate::device_links::webrtc::fits_on_wire(&packet) }));
        let controls = "\u{1}".repeat(MAX_CLIPBOARD_BYTES);
        // Escaped six times over: does not fit, so it is not sent.
        assert!(clipboard_packet(controls).is_none());
    }

    fn screen() -> Vec<Zone> {
        vec![Zone {
            x: 0,
            y: 0,
            width: 1600,
            height: 1000,
        }]
    }

    fn keys(code: u32, mods: Mods) -> Option<gdk::Key> {
        match (code, mods.shift) {
            (30, false) => Some(gdk::Key::a),
            (30, true) => Some(gdk::Key::A),
            (28, _) => Some(gdk::Key::Return),
            (1, _) => Some(gdk::Key::Escape),
            _ => None,
        }
    }

    fn sent(outs: &[Out]) -> Vec<serde_json::Value> {
        outs.iter()
            .filter_map(|out| match out {
                Out::Send(packet) => Some(serde_json::Value::Object(packet.body.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn barriers_sit_on_the_outer_edge_like_the_portal_requires() {
        let right = Barrier::on(Edge::Right, &screen()).unwrap();
        assert_eq!(
            (right.x1, right.y1, right.x2, right.y2),
            (1600, 0, 1600, 999)
        );
        let left = Barrier::on(Edge::Left, &screen()).unwrap();
        assert_eq!((left.x1, left.y1, left.x2, left.y2), (0, 0, 0, 999));
        let top = Barrier::on(Edge::Top, &screen()).unwrap();
        assert_eq!((top.x1, top.y1, top.x2, top.y2), (0, 0, 1599, 0));
        let bottom = Barrier::on(Edge::Bottom, &screen()).unwrap();
        assert_eq!(
            (bottom.x1, bottom.y1, bottom.x2, bottom.y2),
            (0, 1000, 1599, 1000)
        );
        // Two screens side by side: the right edge is the second one's.
        let two = [
            screen()[0],
            Zone {
                x: 1600,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        assert_eq!(Barrier::on(Edge::Right, &two).unwrap().x1, 3520);
        assert_eq!(Barrier::on(Edge::Left, &[]), None);
    }

    #[test]
    fn the_crossing_tells_the_phone_the_edge_and_where_along_it() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        let outs = glide.activated(7, (1603.0, 400.4), &keys, Instant::now());
        assert_eq!(
            sent(&outs),
            vec![serde_json::json!({
                "exitEdge": 4, "deltax": 3, "deltay": 400, "edgeLength": 1000
            })]
        );
        assert!(outs.contains(&Out::Gliding(true)));
        assert!(outs.contains(&Out::Clipboard));
        // The top edge is Qt's 1, and its along-the-edge delta is x.
        let mut top = Glide::accepting(Barrier::on(Edge::Top, &screen()).unwrap());
        let outs = top.activated(1, (2000.0, -4.0), &keys, Instant::now());
        assert_eq!(sent(&outs)[0]["exitEdge"], 1);
        assert_eq!(sent(&outs)[0]["deltax"], 1600, "clamped to the edge");
    }

    #[test]
    fn input_before_activation_waits_then_follows_the_crossing() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
        let now = Instant::now();
        assert!(glide
            .input(Captured::Motion(-5.0, 1.0), &keys, now)
            .is_empty());
        assert!(glide
            .input(Captured::Button(BTN_LEFT, true), &keys, now)
            .is_empty());
        let bodies = sent(&glide.activated(3, (-2.0, 10.0), &keys, now));
        assert_eq!(bodies[0]["exitEdge"], 2);
        assert_eq!(bodies[1], serde_json::json!({"dx": -5.0, "dy": 1.0}));
        assert_eq!(bodies[2], serde_json::json!({"singlehold": true}));
        // Bounded while waiting.
        let mut waiting = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
        for _ in 0..PENDING_MAX + 10 {
            waiting.input(Captured::Motion(1.0, 0.0), &keys, now);
        }
        assert_eq!(waiting.pending.len(), PENDING_MAX);
        assert_eq!(waiting.dropped, 10);
    }

    /// Input that waited too long for an Activated (a tail left over after a
    /// release) is dropped at the next crossing, not typed into the phone.
    #[test]
    fn stale_waiting_input_is_not_replayed_at_the_next_crossing() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
        let then = Instant::now();
        glide.input(Captured::Button(BTN_LEFT, true), &keys, then);
        glide.input(Captured::Key(30, true), &keys, then);
        // A minute later the pointer crosses; two events just before it.
        let now = then + Duration::from_secs(60);
        glide.input(
            Captured::Motion(-3.0, 0.0),
            &keys,
            now - Duration::from_millis(40),
        );
        let bodies = sent(&glide.activated(5, (-1.0, 4.0), &keys, now));
        assert_eq!(bodies.len(), 2, "{bodies:?}");
        assert_eq!(bodies[0]["exitEdge"], 2);
        assert_eq!(bodies[1], serde_json::json!({"dx": -3.0, "dy": 0.0}));
        assert!(glide.pending.is_empty());
    }

    #[test]
    fn motion_is_coalesced_to_one_packet_per_interval() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        let start = Instant::now();
        glide.activated(1, (1600.0, 0.0), &keys, start);
        let first = glide.input(Captured::Motion(1.0, 2.0), &keys, start);
        assert_eq!(
            sent(&first),
            vec![serde_json::json!({"dx": 1.0, "dy": 2.0})]
        );
        assert!(glide
            .input(Captured::Motion(3.0, 0.0), &keys, start)
            .is_empty());
        assert!(glide
            .input(Captured::Motion(1.0, 1.0), &keys, start)
            .is_empty());
        let later = glide.tick(start + MOTION_EVERY);
        assert_eq!(
            sent(&later),
            vec![serde_json::json!({"dx": 4.0, "dy": 1.0})]
        );
        // A click sends the waiting motion first.
        glide.input(Captured::Motion(2.0, 2.0), &keys, start + MOTION_EVERY);
        let click = glide.input(
            Captured::Button(BTN_RIGHT, true),
            &keys,
            start + MOTION_EVERY,
        );
        assert_eq!(
            sent(&click),
            vec![
                serde_json::json!({"dx": 2.0, "dy": 2.0}),
                serde_json::json!({"rightclick": true})
            ]
        );
        // Not finite: ignored.
        glide.input(Captured::Motion(f64::NAN, 1.0), &keys, start);
        assert_eq!(glide.motion, (0.0, 0.0));
    }

    #[test]
    fn buttons_scroll_and_keys_use_the_kde_shapes() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        let now = Instant::now();
        glide.activated(1, (1600.0, 0.0), &keys, now);
        let mut bodies = Vec::new();
        for captured in [
            Captured::Button(BTN_LEFT, false),
            Captured::Button(BTN_RIGHT, false),
            Captured::Button(BTN_MIDDLE, true),
            Captured::Button(BTN_SIDE, true),
            Captured::Discrete(0, 120),
            Captured::Scroll(0.0, 3.5),
            Captured::Key(42, true),
            Captured::Key(30, true),
            Captured::Key(30, false),
            Captured::Key(42, false),
            Captured::Key(29, true),
            Captured::Key(30, true),
            Captured::Key(28, true),
        ] {
            bodies.extend(sent(&glide.input(captured, &keys, now)));
        }
        let no_mods = |mut value: serde_json::Value, shift: bool, ctrl: bool| {
            for (key, on) in [
                ("shift", shift),
                ("ctrl", ctrl),
                ("alt", false),
                ("super", false),
            ] {
                value[key] = on.into();
            }
            value
        };
        assert_eq!(
            bodies,
            vec![
                serde_json::json!({"singlerelease": true}),
                serde_json::json!({"middleclick": true}),
                serde_json::json!({"backclick": true}),
                serde_json::json!({"scroll": true, "dx": 0.0, "dy": -15.0}),
                serde_json::json!({"scroll": true, "dx": 0.0, "dy": 3.5}),
                no_mods(serde_json::json!({"key": "A"}), true, false),
                no_mods(serde_json::json!({"key": "a"}), false, true),
                no_mods(serde_json::json!({"specialKey": 12}), false, true),
            ]
        );
    }

    #[test]
    fn esc_with_super_always_comes_back_where_it_crossed() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        let now = Instant::now();
        glide.activated(9, (1600.0, 300.0), &keys, now);
        assert!(sent(&glide.input(Captured::Key(125, true), &keys, now)).is_empty());
        let outs = glide.input(Captured::Key(KEY_ESC, true), &keys, now);
        assert_eq!(sent(&outs), vec![serde_json::json!({"exitEdge": 0})]);
        assert!(outs.contains(&Out::Release {
            activation: 9,
            x: 1597.0,
            y: 300.0
        }));
        assert!(outs.contains(&Out::Gliding(false)));
        assert!(!glide.gliding());
        // Esc alone goes to the phone.
        glide.activated(10, (1600.0, 300.0), &keys, now);
        let esc = glide.input(Captured::Key(KEY_ESC, true), &keys, now);
        assert_eq!(sent(&esc)[0]["specialKey"], 14);
        assert!(glide.gliding());
    }

    #[test]
    fn the_phone_hands_back_at_its_release_point_inside_the_screen() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Left, &screen()).unwrap());
        assert!(
            glide.phone_released(0.0, 10.0, Instant::now()).is_empty(),
            "not gliding"
        );
        glide.activated(4, (0.0, 10.0), &keys, Instant::now());
        let outs = glide.phone_released(0.0, 640.0, Instant::now());
        assert_eq!(
            outs,
            vec![
                Out::Release {
                    activation: 4,
                    x: 2.0,
                    y: 640.0
                },
                Out::Gliding(false)
            ]
        );
        // An old phone's raw pixels past the screen are clamped into it.
        glide.activated(5, (0.0, 10.0), &keys, Instant::now());
        assert_eq!(
            glide.phone_released(0.0, 2400.0, Instant::now())[0],
            Out::Release {
                activation: 5,
                x: 2.0,
                y: 999.0
            }
        );
        // The compositor ending it tells the phone, and releases nothing.
        glide.activated(6, (0.0, 10.0), &keys, Instant::now());
        assert_eq!(
            glide.deactivated(),
            vec![Out::Send(Glide::taken_back()), Out::Gliding(false)]
        );
    }

    #[test]
    fn a_release_needs_both_deltas_finite_and_bounded() {
        let release = |body: serde_json::Value| {
            let mut packet = NetworkPacket::new("desklink.shareinputdevices");
            packet.body = body.as_object().cloned().unwrap_or_default();
            release_deltas(&packet)
        };
        assert_eq!(
            release(serde_json::json!({"releaseDeltax": 0, "releaseDeltay": 640.5})),
            Some((0.0, 640.5))
        );
        assert_eq!(release(serde_json::json!({"releaseDeltay": 3})), None);
        assert_eq!(
            release(serde_json::json!({"releaseDeltax": 0, "releaseDeltay": "3"})),
            None
        );
        assert_eq!(
            release(serde_json::json!({"releaseDeltax": 0, "releaseDeltay": MAX_DELTA + 1.0})),
            None
        );
    }

    #[test]
    fn the_clipboard_follows_only_within_its_bound() {
        assert!(clipboard_packet(String::new()).is_none());
        assert!(clipboard_packet("x".repeat(MAX_CLIPBOARD_BYTES)).is_some());
        assert!(clipboard_packet("x".repeat(MAX_CLIPBOARD_BYTES + 1)).is_none());
    }

    #[test]
    fn a_lost_reader_or_new_screens_release_the_pointer() {
        let live = || {
            let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
            glide.activated(3, (1600.0, 200.0), &keys, Instant::now());
            glide
        };
        for (event, expected) in [
            (Event::ReaderClosed, "Close"),
            (Event::Signal(Signal::ZonesChanged), "Rearm"),
        ] {
            let mut glide = live();
            let (outs, after) = react(event, Some(&mut glide));
            assert!(
                outs.iter()
                    .any(|out| matches!(out, Out::Release { activation: 3, .. })),
                "{expected}: the capture is released"
            );
            assert!(outs.contains(&Out::Gliding(false)));
            assert!(format!("{after:?}").starts_with(expected));
            assert!(!glide.gliding());
        }
        // Not gliding: nothing to release, the session still changes.
        let (outs, after) = react(Event::ReaderClosed, None);
        assert!(outs.is_empty() && matches!(after, After::Close(_)));
        let (_, after) = react(Event::Signal(Signal::Disabled), Some(&mut live()));
        assert_eq!(after, After::Rearm);
    }

    /// A drag in progress when the pointer comes back by any road: the phone
    /// is told the button is up before anything else.
    #[test]
    fn a_held_button_is_let_go_on_every_way_out() {
        let held = || {
            let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
            glide.activated(3, (1600.0, 200.0), &keys, Instant::now());
            glide.input(Captured::Button(BTN_LEFT, true), &keys, Instant::now());
            assert!(glide.left_down);
            glide
        };
        let released = |outs: &[Out]| {
            sent(outs)
                .first()
                .is_some_and(|body| body["singlerelease"] == true)
        };
        assert!(released(&held().take_back()));
        assert!(released(&held().deactivated()));
        assert!(released(&held().phone_released(0.0, 10.0, Instant::now())));
        // Up before the way out: nothing extra.
        let mut glide = held();
        glide.input(Captured::Button(BTN_LEFT, false), &keys, Instant::now());
        assert!(!released(&glide.take_back()));
        // Once only.
        let mut glide = held();
        glide.take_back();
        assert!(glide.let_go().is_none());
    }

    /// The reader of a session that was closed reports its end a moment
    /// later; that must not stop the session opened since.
    #[test]
    fn a_late_word_from_an_old_reader_is_ignored() {
        assert!(is_current(4, Some(4)));
        assert!(!is_current(3, Some(4)));
        assert!(!is_current(4, None));
    }

    /// The worker ending, for any reason, gives the pointer back and clears
    /// what the top bar shows.
    #[test]
    fn ending_the_worker_releases_a_capture_that_is_live() {
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        glide.activated(9, (1600.0, 200.0), &keys, Instant::now());
        let mut state = State {
            portal: None,
            glide: Some(glide),
            current: Some(GlideSetting {
                device: "e2e-release-all".into(),
                edge: Edge::Right,
            }),
            refused: false,
            arm_failures: 0,
            retry_at: None,
            refusal: (None, false),
        };
        set_status(|status| status.gliding = Some("e2e-release-all".into()));
        state.release_all();
        assert!(state.glide.is_none() && state.portal.is_none());
        assert_eq!(status().gliding, None);
    }

    /// A desktop that is busy for a moment is asked again; one that keeps
    /// failing is given up on, with a reason shown.
    #[test]
    fn arming_is_retried_a_few_times_then_given_up() {
        assert_eq!(after_arm_error(1), ArmNext::Retry(ARM_RETRY_AFTER));
        assert_eq!(after_arm_error(2), ArmNext::Retry(ARM_RETRY_AFTER));
        assert_eq!(after_arm_error(ARM_RETRIES), ArmNext::GiveUp);
        assert_eq!(after_arm_error(ARM_RETRIES + 5), ArmNext::GiveUp);
    }

    /// The keymap's answers are remembered for a while and then asked again;
    /// an answer of "no such key" is one, but a lookup that timed out never
    /// reaches the cache (it returns before `put`).
    #[test]
    fn the_key_cache_expires_and_is_bounded() {
        let mut cache = KeyCache::default();
        let now = Instant::now();
        let key = (30, Mods::default());
        assert_eq!(cache.get(key, now), None, "never asked");
        cache.put(key, None, now);
        assert_eq!(cache.get(key, now), Some(None), "no such key, answered");
        cache.put((31, Mods::default()), Some(97), now);
        assert_eq!(cache.get((31, Mods::default()), now), Some(Some(97)));
        assert_eq!(
            cache.get(key, now + KEY_TTL + Duration::from_secs(1)),
            None,
            "a layout switch is noticed"
        );
        for code in 0..(KEY_CACHE_MAX as u32 + 10) {
            cache.put((code + 100, Mods::default()), Some(1), now);
        }
        assert!(cache.0.len() <= KEY_CACHE_MAX);
    }

    /// A link that went quiet under the pointer gives it back; a quiet phone
    /// the pointer is not on is simply idle.
    #[test]
    fn a_quiet_link_under_the_pointer_gives_it_back() {
        let quiet = Some(GLIDE_SILENT_AFTER + Duration::from_millis(1));
        assert!(link_silent(true, quiet, false));
        assert!(!link_silent(false, quiet, false), "idle, not gone");
        assert!(
            !link_silent(true, quiet, true),
            "a dozing phone is not handed back"
        );
        assert!(!link_silent(true, Some(Duration::from_secs(5)), false));
        assert!(!link_silent(true, None, false));
        // Longer than the liveness probe, shorter than the 15 s rebuild.
        assert!(GLIDE_SILENT_AFTER > crate::device_links::webrtc::coordinator::PEER_PROBE_AFTER);
        assert!(GLIDE_SILENT_AFTER < crate::device_links::webrtc::coordinator::PEER_DEAD_AFTER);
    }

    /// A simulated minute, a tick a quarter second, with the two clocks the
    /// way a real phone drives them: features and heartbeats on the data
    /// channel, and only the reply to the desk's 10 s probe on the LAN (a
    /// sawtooth of 0 to 10.x s). `data_every` is how often the data channel
    /// speaks (None: never). Whether the pointer was ever handed back.
    fn handed_back_within(data_every: Option<u64>, dozing: bool) -> Option<u64> {
        use crate::device_links::webrtc::coordinator::link_silence;
        const TICK_MS: u64 = 250;
        let (mut lan_ms, mut data_ms) = (0u64, 0u64);
        for tick in 1..=(60_000 / TICK_MS) {
            let now = tick * TICK_MS;
            lan_ms += TICK_MS;
            data_ms += TICK_MS;
            // The LAN probe goes out after 10 s of LAN silence and is
            // answered a moment later.
            if lan_ms > 10_500 {
                lan_ms = 0;
            }
            if data_every.is_some_and(|every| now % (every * 1000) == 0) {
                data_ms = 0;
            }
            let silence = link_silence(
                Some(Duration::from_millis(lan_ms)),
                data_every.map(|_| Duration::from_millis(data_ms)),
            );
            if link_silent(true, silence, dozing) {
                return Some(now / 1000);
            }
        }
        None
    }

    #[test]
    fn a_healthy_phone_is_never_handed_back_and_a_silent_data_channel_is() {
        // Talks every 2 s: not once in a minute (the LAN sawtooth alone would).
        assert_eq!(handed_back_within(Some(2), false), None);
        // Talks every 5 s (the liveness probe's pace): still never.
        assert_eq!(handed_back_within(Some(5), false), None);
        // The LAN clock alone, as it was: the bug (some second under 20).
        let lan_alone = {
            let mut handed = None;
            let mut lan_ms = 0u64;
            for tick in 1..=240u64 {
                lan_ms += 250;
                if lan_ms > 10_500 {
                    lan_ms = 0;
                }
                if link_silent(true, Some(Duration::from_millis(lan_ms)), false) {
                    handed = Some(tick / 4);
                    break;
                }
            }
            handed
        };
        assert!(lan_alone.is_some_and(|second| second < 20), "{lan_alone:?}");
        // A phone whose data channel never speaks is handed back at 8 s or
        // when the LAN probe's sawtooth next passes it (it never does here: it
        // resets before 11 s, so it is the sawtooth's last stretch).
        assert!(handed_back_within(None, false).is_some_and(|second| second <= 12));
        // Dozing: never, whatever the clocks say.
        assert_eq!(handed_back_within(None, true), None);
    }

    #[test]
    fn the_two_clocks_say_the_shorter_silence() {
        use crate::device_links::webrtc::coordinator::link_silence;
        let s = Duration::from_secs;
        assert_eq!(link_silence(Some(s(9)), Some(s(1))), Some(s(1)));
        assert_eq!(link_silence(Some(s(1)), Some(s(9))), Some(s(1)));
        assert_eq!(link_silence(Some(s(4)), None), Some(s(4)));
        assert_eq!(link_silence(None, Some(s(4))), Some(s(4)));
        assert_eq!(link_silence(None, None), None);
    }

    /// Nothing wakes the worker 250 times a second while glide is off.
    #[test]
    fn the_worker_sleeps_long_when_glide_is_off() {
        let mut state = State {
            portal: None,
            glide: None,
            current: None,
            refused: false,
            arm_failures: 0,
            retry_at: None,
            refusal: (None, false),
        };
        assert_eq!(nap(&state), CHECK_EVERY);
        let mut glide = Glide::accepting(Barrier::on(Edge::Right, &screen()).unwrap());
        state.glide = Some(Glide::accepting(
            Barrier::on(Edge::Right, &screen()).unwrap(),
        ));
        assert_eq!(
            nap(&state),
            CHECK_EVERY,
            "armed without a portal is still off"
        );
        glide.activated(1, (1600.0, 1.0), &keys, Instant::now());
        state.glide = Some(glide);
        assert_eq!(nap(&state), MOTION_EVERY);
    }

    #[test]
    fn the_setting_reads_and_writes_plain_edge_names() {
        let setting = GlideSetting {
            device: "p".into(),
            edge: Edge::Bottom,
        };
        let json = serde_json::to_string(&setting).unwrap();
        assert_eq!(json, r#"{"device":"p","edge":"bottom"}"#);
        assert_eq!(
            serde_json::from_str::<GlideSetting>(&json).unwrap(),
            setting
        );
    }

    // ── W6: the phone must allow glide, and accept each entry ───────────────

    fn left() -> Glide {
        Glide::new(Barrier::on(Edge::Left, &screen()).unwrap())
    }

    fn count_refused(outs: &[Out]) -> usize {
        outs.iter().filter(|out| **out == Out::Refused).count()
    }

    fn released_at(outs: &[Out]) -> Option<(f64, f64)> {
        outs.iter().find_map(|out| match out {
            Out::Release { x, y, .. } => Some((*x, *y)),
            _ => None,
        })
    }

    /// A phone that hands the pointer back: it is released where it crossed,
    /// nothing but the entry was streamed, this edge is left alone for 5 s
    /// and the person is told once.
    #[test]
    fn a_phone_that_hands_the_pointer_back_gets_nothing_and_the_edge_rests() {
        let mut glide = left();
        let start = Instant::now();
        let mut streamed = sent(&glide.activated(1, (-2.0, 300.0), &keys, start)).len();
        assert_eq!(streamed, 1, "only the entry itself");
        // The mouse keeps moving and clicking while the phone has not answered.
        for step in 0..50 {
            let at = start + Duration::from_millis(step);
            streamed += sent(&glide.input(Captured::Motion(-3.0, 1.0), &keys, at)).len();
            streamed += sent(&glide.input(Captured::Button(BTN_LEFT, true), &keys, at)).len();
        }
        assert_eq!(streamed, 1, "no event streams before an accept");
        // The phone refuses 20 ms in.
        let at = start + Duration::from_millis(20);
        let outs = glide.phone_released(0.0, 0.0, at);
        let (x, y) = released_at(&outs).expect("the pointer is given back");
        assert!(
            (0.0..=5.0).contains(&x) && (y - 300.0).abs() < 2.0,
            "{x},{y}: at the crossing"
        );
        assert!(
            sent(&outs).is_empty(),
            "nothing is said to a phone that said no"
        );
        assert!(outs.contains(&Out::Gliding(false)));
        assert_eq!(count_refused(&outs), 1, "one toast");
        assert!(!glide.gliding());
        // Pushing on the edge again within 5 s: released at once, silent.
        let again = glide.activated(2, (-1.0, 310.0), &keys, start + Duration::from_secs(4));
        assert!(sent(&again).is_empty() && released_at(&again).is_some());
        assert!(!glide.gliding(), "never captured");
        // After 5 s it tries again; a second refusal is not announced again.
        let later = start + Duration::from_secs(6);
        assert_eq!(
            sent(&glide.activated(3, (-2.0, 300.0), &keys, later)).len(),
            1
        );
        let outs = glide.phone_released(0.0, 0.0, later + Duration::from_millis(10));
        assert_eq!(count_refused(&outs), 0, "told once");
        // An accept ends the silence: the next refusal is announced.
        let t = later + Duration::from_secs(6);
        glide.activated(4, (-2.0, 300.0), &keys, t);
        glide.accepted(&keys, t);
        glide.take_back();
        let t = t + Duration::from_secs(6);
        glide.activated(5, (-2.0, 300.0), &keys, t);
        let outs = glide.phone_released(0.0, 0.0, t);
        assert_eq!(count_refused(&outs), 1);
    }

    /// No answer within 300 ms is a refusal too: the pointer is back where it
    /// crossed, the phone is told to forget the entry.
    #[test]
    fn an_entry_the_phone_never_answers_is_given_back_after_300_ms() {
        let mut glide = left();
        let start = Instant::now();
        glide.activated(1, (-2.0, 120.0), &keys, start);
        glide.input(Captured::Motion(-9.0, 0.0), &keys, start);
        assert!(glide.tick(start + Duration::from_millis(299)).is_empty());
        assert!(glide.gliding());
        let outs = glide.tick(start + ENTER_WAIT);
        assert_eq!(sent(&outs), vec![serde_json::json!({"exitEdge": 0})]);
        assert!(released_at(&outs).is_some());
        assert_eq!(count_refused(&outs), 1);
        assert!(!glide.gliding());
        assert!(glide.tick(start + Duration::from_secs(1)).is_empty());
    }

    /// Accepted: what was held goes out in order, then it streams normally.
    #[test]
    fn an_accepted_entry_replays_what_was_held_and_then_streams() {
        let mut glide = left();
        let start = Instant::now();
        glide.activated(1, (-2.0, 120.0), &keys, start);
        glide.input(Captured::Motion(-4.0, 2.0), &keys, start);
        glide.input(Captured::Button(BTN_LEFT, true), &keys, start);
        glide.input(Captured::Button(BTN_LEFT, false), &keys, start);
        let at = start + Duration::from_millis(40);
        let outs = glide.accepted(&keys, at);
        assert_eq!(
            sent(&outs),
            vec![
                serde_json::json!({"dx": -4.0, "dy": 2.0}),
                serde_json::json!({"singlehold": true}),
                serde_json::json!({"singlerelease": true}),
            ]
        );
        // After the accept: streaming at once, and no timeout any more.
        assert!(glide.tick(start + Duration::from_secs(2)).is_empty());
        assert!(glide.gliding());
        let outs = glide.input(
            Captured::Motion(1.0, 1.0),
            &keys,
            at + Duration::from_millis(10),
        );
        assert_eq!(sent(&outs).len(), 1);
        // A second accept changes nothing.
        assert!(glide.accepted(&keys, at).is_empty());
    }

    /// Held input is bounded, and Esc+Super still brings the pointer back
    /// while the phone has not answered.
    #[test]
    fn held_input_is_bounded_and_esc_super_works_while_waiting() {
        let mut glide = left();
        let now = Instant::now();
        glide.activated(1, (-2.0, 120.0), &keys, now);
        for _ in 0..HELD_MAX + 20 {
            glide.input(Captured::Motion(1.0, 0.0), &keys, now);
        }
        assert_eq!(glide.held.len(), HELD_MAX);
        glide.input(Captured::Key(125, true), &keys, now);
        let outs = glide.input(Captured::Key(KEY_ESC, true), &keys, now);
        assert!(released_at(&outs).is_some(), "Esc+Super comes back");
        assert!(!glide.gliding());
        assert!(glide.held.is_empty());
    }

    /// The phone says glide is no longer allowed: a pointer on it, or on its
    /// way, comes back at once.
    #[test]
    fn a_flip_to_not_allowed_gives_the_pointer_back_at_once() {
        let now = Instant::now();
        let mut on = left();
        on.activated(1, (-2.0, 200.0), &keys, now);
        on.accepted(&keys, now);
        on.input(Captured::Button(BTN_LEFT, true), &keys, now);
        let outs = on.disallowed(now);
        assert_eq!(
            sent(&outs),
            vec![
                serde_json::json!({"singlerelease": true}),
                serde_json::json!({"exitEdge": 0})
            ]
        );
        assert!(released_at(&outs).is_some() && !on.gliding());
        // Still entering: a refusal, nothing said.
        let mut entering = left();
        entering.activated(1, (-2.0, 200.0), &keys, now);
        let outs = entering.disallowed(now);
        assert!(released_at(&outs).is_some() && sent(&outs).is_empty());
    }

    /// The barrier is armed only for a phone that reported "allowed: true";
    /// unknown (an older phone) and false both leave it disabled.
    #[test]
    fn the_barrier_is_armed_only_when_the_phone_allows_glide() {
        assert!(wants_barrier(false, false, true, true));
        assert!(
            !wants_barrier(false, false, true, false),
            "not allowed or unknown"
        );
        assert!(!wants_barrier(false, false, false, true), "link not ready");
        assert!(!wants_barrier(false, true, true, true), "link silent");
        assert!(
            !wants_barrier(true, false, true, true),
            "refused by the desktop"
        );
        let phone = "w6f-phone-state";
        assert_eq!(phone_allows(phone), None, "no report yet");
        assert!(!allows_glide(phone));
        let report = |allowed: bool| {
            let mut packet =
                NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_INPUT_DEVICES);
            packet.set("allowed", allowed);
            packet
        };
        on_phone_packet(phone, &report(false));
        assert_eq!(phone_allows(phone), Some(false));
        assert!(!allows_glide(phone));
        on_phone_packet(phone, &report(true));
        assert!(allows_glide(phone));
        on_phone_packet(phone, &report(false));
        assert!(!allows_glide(phone), "a flip to false is seen at once");
        // A release (G3) is not a state report.
        let mut release =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_INPUT_DEVICES);
        release.set("releaseDeltax", 3);
        release.set("releaseDeltay", 4);
        on_phone_packet(phone, &release);
        assert_eq!(phone_allows(phone), Some(false));
        set_allowed(phone, None);
        assert_eq!(phone_allows(phone), None, "forgotten when the link goes");
    }
}
