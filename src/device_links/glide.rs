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
/// Motion is coalesced: at most one motion packet per this long (250/s).
const MOTION_EVERY: Duration = Duration::from_millis(4);
/// How often the worker re-reads the setting and the phone's link.
const CHECK_EVERY: Duration = Duration::from_millis(250);
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
    pending: VecDeque<Captured>,
    mods: Mods,
    /// Motion not sent yet, and when motion was last sent.
    motion: (f64, f64),
    motion_sent: Option<Instant>,
    dropped: u64,
}

impl Glide {
    fn new(barrier: Barrier) -> Self {
        Self {
            barrier,
            active: None,
            crossed: (0.0, 0.0),
            pending: VecDeque::new(),
            mods: Mods::default(),
            motion: (0.0, 0.0),
            motion_sent: None,
            dropped: 0,
        }
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
        let was = self.active.replace(activation);
        self.crossed = cursor;
        self.mods = Mods::default();
        self.motion = (0.0, 0.0);
        let mut out = vec![Out::Send(self.barrier.enter_packet(cursor))];
        if was.is_none() {
            out.push(Out::Gliding(true));
            out.push(Out::Clipboard);
        }
        for captured in std::mem::take(&mut self.pending) {
            out.extend(self.input(captured, keys, now));
        }
        out
    }

    /// The compositor ended the capture itself (lock screen, session end).
    fn deactivated(&mut self) -> Vec<Out> {
        self.pending.clear();
        if self.active.take().is_none() {
            return Vec::new();
        }
        vec![Out::Send(Self::taken_back()), Out::Gliding(false)]
    }

    /// G4: the desk takes the pointer back (Esc+Super, link down, glide
    /// off), where it crossed.
    fn take_back(&mut self) -> Vec<Out> {
        self.pending.clear();
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        let (x, y) = self.barrier.inside(self.crossed.0, self.crossed.1);
        vec![
            Out::Send(Self::taken_back()),
            Out::Release { activation, x, y },
            Out::Gliding(false),
        ]
    }

    fn taken_back() -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SHARE_INPUT_DEVICES_REQUEST);
        packet.set("exitEdge", 0);
        packet
    }

    /// G3: the phone hands the pointer back.
    fn phone_released(&mut self, dx: f64, dy: f64) -> Vec<Out> {
        self.pending.clear();
        let Some(activation) = self.active.take() else {
            return Vec::new();
        };
        let (x, y) = self.barrier.release_point(dx, dy);
        vec![Out::Release { activation, x, y }, Out::Gliding(false)]
    }

    /// Motion waiting longer than [`MOTION_EVERY`] goes out.
    fn tick(&mut self, now: Instant) -> Vec<Out> {
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
                self.pending.push_back(captured);
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
                    (BTN_LEFT, true) => "singlehold",
                    (BTN_LEFT, false) => "singlerelease",
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
    type Cache = HashMap<(u32, Mods), Option<u32>>;
    static CACHE: Mutex<Option<Cache>> = Mutex::new(None);
    let level = Mods {
        shift: mods.shift,
        caps: mods.caps,
        altgr: mods.altgr,
        ..Mods::default()
    };
    let known = CACHE.lock().ok().and_then(|cache| {
        cache
            .as_ref()
            .and_then(|cache| cache.get(&(code, level)).copied())
    });
    let keyval = match known {
        Some(keyval) => keyval,
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
            let found = receiver.recv_timeout(KEY_LOOKUP).ok().flatten();
            if let Ok(mut cache) = CACHE.lock() {
                let cache = cache.get_or_insert_with(HashMap::new);
                if cache.len() >= KEY_CACHE_MAX {
                    cache.clear();
                }
                cache.insert((code, level), found);
            }
            found
        }
    }?;
    // SAFETY: any u32 is a valid GDK keyval number.
    Some(unsafe { glib::translate::from_glib::<u32, gdk::Key>(keyval) })
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
    Captured(Captured),
    Released { device: String, dx: f64, dy: f64 },
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
        let mut portal = Self {
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
        eis::spawn_receiver(socket, "DeskLink", |captured| {
            send_command(Command::Captured(captured))
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
            Out::Gliding(on) => {
                debug_log!("[Glide] {}", if on { "on the phone" } else { "back" });
                set_status(|status| status.gliding = on.then(|| device.to_string()));
            }
        }
    }
}

fn worker(context: &glib::MainContext, commands: &Receiver<Command>) {
    let connection = match gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) {
        Ok(connection) => connection,
        Err(_) => {
            set_status(|status| {
                status.availability = Availability::Unavailable("The desktop bus is not running")
            });
            return;
        }
    };
    let availability = check_availability(&connection);
    set_status(|status| status.availability = availability.clone());
    if availability != Availability::Available {
        return;
    }
    let mut portal: Option<Portal> = None;
    let mut glide: Option<Glide> = None;
    // The setting the session was made for, and whether the user (or the
    // desktop) said no to it: not asked again until the setting changes.
    let mut current: Option<GlideSetting> = None;
    let mut refused = false;
    let mut checked = Instant::now() - CHECK_EVERY;
    loop {
        let mut outs = Vec::new();
        // Up to QUEUE events per turn, then the rest of the loop runs.
        let first = commands.recv_timeout(MOTION_EVERY);
        if matches!(first, Err(mpsc::RecvTimeoutError::Disconnected)) {
            break;
        }
        let device = current.as_ref().map(|setting| setting.device.clone());
        let mut events = Vec::new();
        for command in first.into_iter().chain(commands.try_iter().take(QUEUE)) {
            match command {
                Command::Captured(Captured::Closed) => events.push(Event::ReaderClosed),
                Command::Captured(captured) => {
                    if let Some(glide) = glide.as_mut() {
                        outs.extend(glide.input(captured, &desk_key, Instant::now()));
                    }
                }
                Command::Released {
                    device: from,
                    dx,
                    dy,
                } => {
                    if device.as_deref() == Some(from.as_str()) {
                        if let Some(glide) = glide.as_mut() {
                            outs.extend(glide.phone_released(dx, dy));
                        }
                    }
                }
            }
        }
        while context.pending() {
            context.iteration(false);
        }
        let signals: Vec<Signal> = portal
            .as_ref()
            .map(|portal| portal.signals.borrow_mut().drain(..).collect())
            .unwrap_or_default();
        events.extend(signals.into_iter().map(Event::Signal));
        for event in events {
            let (more, after) = react(event, glide.as_mut());
            outs.extend(more);
            if after == After::Keep {
                continue;
            }
            // The Release first, while the session that captured is open.
            perform(
                current.as_ref().map(|setting| setting.device.as_str()),
                std::mem::take(&mut outs),
                portal.as_mut(),
            );
            match after {
                After::Keep => {}
                After::Rearm => {
                    if let Some(portal) = portal.as_mut() {
                        portal.disable();
                    }
                }
                After::Close(problem) => {
                    if let Some(mut closing) = portal.take() {
                        closing.disable();
                    }
                    glide = None;
                    refused = true;
                    set_status(|status| status.problem = Some(problem.to_string()));
                }
            }
        }
        if let Some(glide) = glide.as_mut() {
            outs.extend(glide.tick(Instant::now()));
        }

        if checked.elapsed() >= CHECK_EVERY {
            checked = Instant::now();
            let setting = crate::device_links::config::app_settings().glide;
            if setting != current {
                // Done now, for the phone it was set for, while the
                // session that captured is still open.
                let mut back = std::mem::take(&mut outs);
                if let Some(glide) = glide.as_mut() {
                    back.extend(glide.take_back());
                }
                perform(
                    current.as_ref().map(|setting| setting.device.as_str()),
                    back,
                    portal.as_mut(),
                );
                // Another phone or off: a new session (and consent) for it.
                if setting.as_ref().map(|s| &s.device) != current.as_ref().map(|s| &s.device) {
                    portal = None;
                }
                if let Some(portal) = portal.as_mut() {
                    // A new edge: barriers are replaced on a disabled session.
                    portal.disable();
                    portal.armed_at.clear();
                }
                current = setting.clone();
                refused = false;
                set_status(|status| status.problem = None);
            }
            let want = !refused
                && current
                    .as_ref()
                    .is_some_and(|setting| ready(&setting.device));
            if want && portal.is_none() {
                match Portal::open(context, &connection) {
                    Ok(opened) => portal = Some(opened),
                    Err(error) => {
                        log_limited("open", &error);
                        refused = true;
                        set_status(|status| status.problem = Some(plain(&error)));
                    }
                }
            }
            if let (Some(open), Some(setting)) = (portal.as_mut(), current.as_ref()) {
                if want && !open.enabled {
                    match open.arm(context, setting.edge) {
                        Ok(barrier) => {
                            glide = Some(Glide::new(barrier));
                            set_status(|status| status.problem = None);
                        }
                        Err(error) => {
                            log_limited("arm", &error);
                            refused = true;
                            set_status(|status| status.problem = Some(plain(&error)));
                        }
                    }
                } else if !want && open.enabled {
                    // The Release (and telling the phone) before Disable.
                    let mut back = std::mem::take(&mut outs);
                    if let Some(glide) = glide.as_mut() {
                        back.extend(glide.take_back());
                    }
                    perform(Some(setting.device.as_str()), back, Some(&mut *open));
                    open.disable();
                }
            }
            if current.is_none() {
                portal = None;
                glide = None;
            }
        }

        let target = current.as_ref().map(|setting| setting.device.clone());
        perform(target.as_deref(), outs, portal.as_mut());
        if glide.as_ref().is_some_and(|glide| glide.dropped > 0) {
            if let Some(glide) = glide.as_mut() {
                log_limited("pending", "Input before the capture started was dropped");
                glide.dropped = 0;
            }
        }
    }
    set_status(|status| status.gliding = None);
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
        let mut glide = Glide::new(Barrier::on(Edge::Right, &screen()).unwrap());
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
        let mut top = Glide::new(Barrier::on(Edge::Top, &screen()).unwrap());
        let outs = top.activated(1, (2000.0, -4.0), &keys, Instant::now());
        assert_eq!(sent(&outs)[0]["exitEdge"], 1);
        assert_eq!(sent(&outs)[0]["deltax"], 1600, "clamped to the edge");
    }

    #[test]
    fn input_before_activation_waits_then_follows_the_crossing() {
        let mut glide = Glide::new(Barrier::on(Edge::Left, &screen()).unwrap());
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
        let mut waiting = Glide::new(Barrier::on(Edge::Left, &screen()).unwrap());
        for _ in 0..PENDING_MAX + 10 {
            waiting.input(Captured::Motion(1.0, 0.0), &keys, now);
        }
        assert_eq!(waiting.pending.len(), PENDING_MAX);
        assert_eq!(waiting.dropped, 10);
    }

    #[test]
    fn motion_is_coalesced_to_one_packet_per_interval() {
        let mut glide = Glide::new(Barrier::on(Edge::Right, &screen()).unwrap());
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
        let mut glide = Glide::new(Barrier::on(Edge::Right, &screen()).unwrap());
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
        let mut glide = Glide::new(Barrier::on(Edge::Right, &screen()).unwrap());
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
        let mut glide = Glide::new(Barrier::on(Edge::Left, &screen()).unwrap());
        assert!(glide.phone_released(0.0, 10.0).is_empty(), "not gliding");
        glide.activated(4, (0.0, 10.0), &keys, Instant::now());
        let outs = glide.phone_released(0.0, 640.0);
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
            glide.phone_released(0.0, 2400.0)[0],
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
            let mut glide = Glide::new(Barrier::on(Edge::Right, &screen()).unwrap());
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
}
