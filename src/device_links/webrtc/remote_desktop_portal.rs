//! GNOME RemoteDesktop + ScreenCast portal adapter.
//!
//! A phone request is not itself permission to observe or inject. This module
//! obtains the shared XDG portal session first; the compositor presents its
//! local consent UI, and only a successful `Start` yields a PipeWire source and
//! permission to send input. Keep each instance on a dedicated worker thread:
//! portal requests may wait for a human and must not block WebRTC signaling.

use std::cell::Cell;
use std::cell::RefCell;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use gtk::gio::prelude::*;
use gtk::glib::variant::{ObjectPath, ToVariant};
use gtk::{gio, glib};

const PORTAL_BUS: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REQUEST_INTERFACE: &str = "org.freedesktop.portal.Request";
const SESSION_INTERFACE: &str = "org.freedesktop.portal.Session";
const REMOTE_INTERFACE: &str = "org.freedesktop.portal.RemoteDesktop";
const SCREEN_INTERFACE: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// RemoteDesktop `persist_mode`: remember while this process runs.
///
/// Measured on GNOME 50: asking for a *permanent* grant (2) removes the "Allow
/// Remote Interaction" switch from the consent dialog, so every remembered
/// grant came back view-only and phone control was refused. The session mode
/// keeps the switch; after one approval at the desk, later phone sessions
/// resume silently (0.1 s, keyboard + pointer) until DeskLink restarts.
const PERSIST_FOR_SESSION: u32 = 1;
/// Largest screen dimension accepted anywhere in the remote-desktop path.
const MAX_SCREEN_DIMENSION: f64 = 8192.0;
/// Longest run of typed text one input packet may carry.
const MAX_TYPED_TEXT_BYTES: usize = 1024;

/// How to put the pointer at `(x, y)` in the screen's logical space (what the
/// portal reports and the phone maps into): an absolute move in stream units,
/// then, when needed, a relative nudge in logical pixels.
///
/// Measured on GNOME 50 with a 133 % panel (1920x1200 logical, 2560x1600
/// pixels): `NotifyPointerMotionAbsolute` *places* the pointer at
/// `position / scale` — so it wants real pixels — but *refuses* ("Invalid
/// position") anything past the logical size in those same units. Absolute
/// moves alone can therefore never reach the right and bottom quarter of the
/// screen, the dock included. Relative motion is unbounded and unaccelerated,
/// so the remainder is covered with it.
pub(crate) fn pointer_move(
    x: f64,
    y: f64,
    logical: (i32, i32),
    pixels: Option<(i32, i32)>,
) -> ((f64, f64), Option<(f64, f64)>) {
    let x = x.clamp(0.0, (logical.0 - 1).max(0) as f64);
    let y = y.clamp(0.0, (logical.1 - 1).max(0) as f64);
    let (sx, sy) = match pixels {
        Some((pw, ph)) if logical.0 > 0 && logical.1 > 0 => {
            (pw as f64 / logical.0 as f64, ph as f64 / logical.1 as f64)
        }
        _ => (1.0, 1.0),
    };
    let (ax, ay) = (
        (x * sx).min((logical.0 - 1).max(0) as f64),
        (y * sy).min((logical.1 - 1).max(0) as f64),
    );
    let (rx, ry) = (x - ax / sx, y - ay / sy);
    let nudge = (rx.abs() >= 0.5 || ry.abs() >= 0.5).then_some((rx, ry));
    ((ax, ay), nudge)
}

/// One shared screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PortalStream {
    pub node_id: u32,
    pub width: i32,
    pub height: i32,
    pub pipewire_serial: Option<u64>,
}

fn parse_stream(stream: &glib::Variant) -> Result<PortalStream, String> {
    let node_id = stream
        .child_value(0)
        .get::<u32>()
        .ok_or_else(|| "Desktop portal returned an invalid PipeWire node".to_string())?;
    let properties = stream.child_value(1);
    let (width, height) = dict_value(&properties, "size")
        .and_then(|value| value.get::<(i32, i32)>())
        .ok_or_else(|| "Desktop portal omitted screen stream dimensions".to_string())?;
    if node_id == 0 || !(1..=8192).contains(&width) || !(1..=8192).contains(&height) {
        return Err("Desktop portal returned invalid screen stream metadata".to_string());
    }
    Ok(PortalStream {
        node_id,
        width,
        height,
        pipewire_serial: dict_value(&properties, "pipewire-serial")
            .and_then(|value| value.get::<u64>()),
    })
}

pub(crate) struct PortalScreenSource {
    pub fd: OwnedFd,
    pub node_id: u32,
    pub pipewire_serial: Option<u64>,
    pub width: i32,
    pub height: i32,
    pub granted_device_types: u32,
    /// Resumes this grant next time without the consent dialog; present only
    /// if the user chose to remember it. Single-use: replaces the old one.
    pub restore_token: Option<String>,
}

impl PortalScreenSource {
    /// The restore token worth keeping: only one for a grant that includes
    /// keyboard **and** pointer. A remembered view-only grant would resume
    /// silently forever and refuse the phone control, with nobody at the desk
    /// ever asked again; not remembering it brings the dialog back next time.
    pub(crate) fn token_to_remember(&self) -> Option<&str> {
        self.restore_token
            .as_deref()
            .filter(|_| self.granted_device_types & 3 == 3)
    }
}

/// Lives on one dedicated worker thread. The portal D-Bus object is deliberately
/// not exposed: all OS input calls must pass through a validated input lease in
/// the WebRTC coordinator before reaching these methods.
pub(crate) struct RemoteDesktopPortal {
    connection: gio::DBusConnection,
    session_handle: String,
    streams: Vec<PortalStream>,
    /// The screen being shown and controlled; pointer positions are in its
    /// coordinates.
    active_stream: Cell<usize>,
    /// The active screen's real pixel size, once the capture knows it.
    active_pixels: Cell<Option<(i32, i32)>>,
    session_guard: PortalSessionGuard,
    context: glib::MainContext,
    session_closed: Rc<Cell<bool>>,
    closed_subscription: Option<gio::SignalSubscriptionId>,
}

struct PortalSessionGuard {
    connection: gio::DBusConnection,
    handle: String,
    closed: bool,
}

impl PortalSessionGuard {
    fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        let _ = self.connection.call_sync(
            Some(PORTAL_BUS),
            &self.handle,
            SESSION_INTERFACE,
            "Close",
            None,
            None,
            gio::DBusCallFlags::NONE,
            2_000,
            None::<&gio::Cancellable>,
        );
    }
}

/// Unsubscribes from a D-Bus signal when dropped, unless disarmed.
struct SubscriptionGuard {
    connection: gio::DBusConnection,
    id: Option<gio::SignalSubscriptionId>,
}

impl SubscriptionGuard {
    /// Hands the subscription to a longer-lived owner.
    fn disarm(mut self) -> gio::SignalSubscriptionId {
        self.id.take().expect("an armed subscription has an id")
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.connection.signal_unsubscribe(id);
        }
    }
}

impl Drop for PortalSessionGuard {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PortalInputAction {
    RelativeMotion(f64, f64),
    AbsoluteMotion(f64, f64),
    Button { evdev_code: i32, pressed: bool },
    Scroll(f64, f64),
    Key { keysym: i32, pressed: bool },
    TypeText { text: String, modifiers: u32 },
}

/// What a desktop share needs from the system whose screen it shares: the
/// XDG portal session ([`RemoteDesktopPortal`]) or, on X11 desktops without
/// one, [`super::x11_desktop::X11Desktop`]. Every call comes from the share's
/// worker thread, after the coordinator has checked the phone's input lease.
pub(crate) trait DesktopSession {
    /// The screen being shown and controlled.
    fn active(&self) -> PortalStream;
    /// How many screens are shared.
    fn stream_count(&self) -> usize;
    /// Makes the next shared screen the active one and returns it.
    fn next_stream(&self) -> PortalStream;
    fn knows_active_pixels(&self) -> bool;
    /// Records the active screen's real pixel size (from the capture).
    fn set_active_pixels(&self, size: (i32, i32));
    /// Whether the session ended underneath the share (revoked, X server gone).
    fn is_session_closed(&self) -> bool;
    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<(), String>;
    /// `(x, y)` in the active screen's logical coordinates.
    fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), String>;
    fn pointer_button(&self, evdev_button: i32, pressed: bool) -> Result<(), String>;
    fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) -> Result<(), String>;
    fn keyboard_keysym(&self, keysym: i32, pressed: bool) -> Result<(), String>;

    fn apply(&self, action: PortalInputAction) -> Result<(), String> {
        match action {
            PortalInputAction::RelativeMotion(dx, dy) => self.pointer_motion(dx, dy),
            PortalInputAction::AbsoluteMotion(x, y) => self.pointer_motion_absolute(x, y),
            PortalInputAction::Button {
                evdev_code,
                pressed,
            } => self.pointer_button(evdev_code, pressed),
            // One open gesture while the fingers move; the service loop
            // finishes it once they stop (see `SCROLL_FINISH_AFTER`). Finishing
            // every step made each one a separate fling.
            PortalInputAction::Scroll(dx, dy) => self.pointer_axis(dx, dy, false),
            PortalInputAction::Key { keysym, pressed } => self.keyboard_keysym(keysym, pressed),
            PortalInputAction::TypeText { text, modifiers } => type_text(self, &text, modifiers),
        }
    }

    fn release_all(&self) {
        // The service loop tracks keys/buttons it pressed and passes explicit
        // releases. This neutral motion/axis finish ensures a final scroll
        // gesture is closed when a lease or transport disappears.
        let _ = self.pointer_axis(0.0, 0.0, true);
    }
}

impl RemoteDesktopPortal {
    /// Creates one portal session, selects a single monitor, and asks the host
    /// to approve screen capture plus keyboard/pointer control.
    /// `cancel` is polled while a request waits for the human, so a phone that
    /// released (or replaced) the share does not leave a consent dialog open —
    /// and an approval nobody is waiting for cannot start a capture.
    pub(crate) fn start(
        parent_window: &str,
        cancel: &AtomicBool,
        restore_token: Option<&str>,
    ) -> Result<(Self, PortalScreenSource), String> {
        let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
            .map_err(|error| format!("Could not connect to the desktop portal: {error}"))?;
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let create_options = options(|dict| {
                    dict.insert("handle_token", token());
                    dict.insert("session_handle_token", token());
                });
                let created = request(
                    &context,
                    &connection,
                    cancel,
                    PORTAL_PATH,
                    REMOTE_INTERFACE,
                    "CreateSession",
                    tuple([create_options]),
                )?;
                let session_handle = lookup::<String>(&created, "session_handle")?
                    .ok_or_else(|| "Desktop portal returned no session handle".to_string())?;
                ObjectPath::try_from(session_handle.clone())
                    .map_err(|_| "Desktop portal returned an invalid session handle".to_string())?;
                let session_guard = PortalSessionGuard {
                    connection: connection.clone(),
                    handle: session_handle.clone(),
                    closed: false,
                };
                let session_closed = Rc::new(Cell::new(false));
                let closed_flag = Rc::clone(&session_closed);
                // Held in a guard so that every early `?` below unsubscribes;
                // each denied session used to leak a D-Bus match rule.
                let closed_subscription = SubscriptionGuard {
                    connection: connection.clone(),
                    id: Some(connection.signal_subscribe(
                        Some(PORTAL_BUS),
                        Some(SESSION_INTERFACE),
                        Some("Closed"),
                        Some(&session_handle),
                        None,
                        gio::DBusSignalFlags::NONE,
                        move |_, _, _, _, _, _| closed_flag.set(true),
                    )),
                };

                let session_arg = object_path(&session_handle)?;
                let device_options = options(|dict| {
                    // Keyboard and pointer. The grant is remembered so the
                    // phone alone can resume it later; a stored token resumes
                    // it now. A stale or revoked token brings the dialog back.
                    dict.insert("types", 3u32);
                    // Debug builds: DESKLINK_PORTAL_PERSIST overrides, to test
                    // how GNOME's dialog behaves in each mode.
                    let persist_mode = if cfg!(debug_assertions) {
                        std::env::var("DESKLINK_PORTAL_PERSIST")
                            .ok()
                            .and_then(|mode| mode.parse().ok())
                            .unwrap_or(PERSIST_FOR_SESSION)
                    } else {
                        PERSIST_FOR_SESSION
                    };
                    dict.insert("persist_mode", persist_mode);
                    if let Some(restore) = restore_token {
                        dict.insert("restore_token", restore);
                    }
                    dict.insert("handle_token", token());
                });
                request(
                    &context,
                    &connection,
                    cancel,
                    PORTAL_PATH,
                    REMOTE_INTERFACE,
                    "SelectDevices",
                    tuple([session_arg.clone(), device_options]),
                )?;

                let source_options = options(|dict| {
                    dict.insert("types", 1u32); // MONITOR
                                                // All screens may be shared at once; the phone switches.
                    dict.insert("multiple", true);
                    dict.insert("cursor_mode", 2u32); // EMBEDDED
                    dict.insert("handle_token", token());
                });
                request(
                    &context,
                    &connection,
                    cancel,
                    PORTAL_PATH,
                    SCREEN_INTERFACE,
                    "SelectSources",
                    tuple([session_arg.clone(), source_options]),
                )?;

                let start_options = options(|dict| {
                    dict.insert("handle_token", token());
                });
                let started = request(
                    &context,
                    &connection,
                    cancel,
                    PORTAL_PATH,
                    REMOTE_INTERFACE,
                    "Start",
                    tuple([
                        session_arg.clone(),
                        parent_window.to_variant(),
                        start_options,
                    ]),
                )?;
                // A portal that reports no `devices` granted none. That must
                // leave the screen view working; only control is withheld.
                let granted_device_types = lookup::<u32>(&started, "devices")?.unwrap_or(0);
                let restore_token = lookup::<String>(&started, "restore_token")?;
                let streams = dict_value(&started, "streams")
                    .ok_or_else(|| "Desktop portal returned no screen stream".to_string())?;
                // Every screen the user shared — a laptop and a monitor, say —
                // so the phone can switch between them without another dialog.
                let streams = (0..streams.n_children())
                    .map(|index| parse_stream(&streams.child_value(index)))
                    .collect::<Result<Vec<_>, _>>()?;
                let first = *streams
                    .first()
                    .ok_or_else(|| "Desktop portal returned no screen stream".to_string())?;
                let (node_id, size, pipewire_serial) = (
                    first.node_id,
                    (first.width, first.height),
                    first.pipewire_serial,
                );

                let pipewire_reply = connection
                    .call_with_unix_fd_list_sync(
                        Some(PORTAL_BUS),
                        PORTAL_PATH,
                        SCREEN_INTERFACE,
                        "OpenPipeWireRemote",
                        Some(&tuple([session_arg, options(|_| {})])),
                        None,
                        gio::DBusCallFlags::NONE,
                        10_000,
                        None::<&gio::UnixFDList>,
                        None::<&gio::Cancellable>,
                    )
                    .map_err(|error| {
                        format!("Could not open the approved PipeWire stream: {error}")
                    })?;
                let fd_index = pipewire_fd_index(&pipewire_reply.0)?;
                let fd_list = pipewire_reply
                    .1
                    .ok_or_else(|| "Desktop portal omitted the PipeWire FD list".to_string())?;
                let raw_fd = fd_list.get(fd_index).map_err(|error| {
                    format!("Could not receive the approved PipeWire FD: {error}")
                })?;
                let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };

                Ok((
                    Self {
                        connection: connection.clone(),
                        session_handle,
                        streams: streams.clone(),
                        active_stream: Cell::new(0),
                        active_pixels: Cell::new(None),
                        session_guard,
                        context: context.clone(),
                        session_closed,
                        closed_subscription: Some(closed_subscription.disarm()),
                    },
                    PortalScreenSource {
                        fd,
                        node_id,
                        pipewire_serial,
                        width: size.0,
                        height: size.1,
                        granted_device_types,
                        restore_token,
                    },
                ))
            })
            .map_err(|error| error.to_string())?
    }

    pub(crate) fn keyboard_keycode(&self, keycode: i32, pressed: bool) -> Result<(), String> {
        self.notify(
            "NotifyKeyboardKeycode",
            tuple([
                object_path(&self.session_handle)?,
                options(|_| {}),
                keycode.to_variant(),
                u32::from(pressed).to_variant(),
            ]),
        )
    }

    fn notify(&self, method: &str, parameters: glib::Variant) -> Result<(), String> {
        self.connection
            .call_sync(
                Some(PORTAL_BUS),
                PORTAL_PATH,
                REMOTE_INTERFACE,
                method,
                Some(&parameters),
                None,
                gio::DBusCallFlags::NONE,
                2_000,
                None::<&gio::Cancellable>,
            )
            .map(|_| ())
            .map_err(|error| format!("Desktop portal rejected {method}: {error}"))
    }
}

impl DesktopSession for RemoteDesktopPortal {
    fn active(&self) -> PortalStream {
        self.streams[self.active_stream.get().min(self.streams.len() - 1)]
    }

    /// How many screens the user shared.
    fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Makes the next shared screen the active one and returns it.
    fn next_stream(&self) -> PortalStream {
        self.active_stream
            .set((self.active_stream.get() + 1) % self.streams.len());
        self.active_pixels.set(None);
        self.active()
    }

    fn knows_active_pixels(&self) -> bool {
        self.active_pixels.get().is_some()
    }

    /// Records the active screen's real pixel size (from the capture).
    fn set_active_pixels(&self, size: (i32, i32)) {
        if size.0 > 0 && size.1 > 0 {
            self.active_pixels.set(Some(size));
        }
    }

    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<(), String> {
        self.notify(
            "NotifyPointerMotion",
            tuple([
                object_path(&self.session_handle)?,
                options(|_| {}),
                dx.to_variant(),
                dy.to_variant(),
            ]),
        )
    }

    fn is_session_closed(&self) -> bool {
        while self.context.pending() {
            self.context.iteration(false);
        }
        self.session_closed.get()
    }

    fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), String> {
        let stream = self.active();
        let ((x, y), nudge) = pointer_move(
            x,
            y,
            (stream.width, stream.height),
            self.active_pixels.get(),
        );
        self.notify(
            "NotifyPointerMotionAbsolute",
            tuple([
                object_path(&self.session_handle)?,
                options(|_| {}),
                stream.node_id.to_variant(),
                x.to_variant(),
                y.to_variant(),
            ]),
        )?;
        if let Some((dx, dy)) = nudge {
            self.pointer_motion(dx, dy)?;
        }
        Ok(())
    }

    fn pointer_button(&self, evdev_button: i32, pressed: bool) -> Result<(), String> {
        self.notify(
            "NotifyPointerButton",
            tuple([
                object_path(&self.session_handle)?,
                options(|_| {}),
                evdev_button.to_variant(),
                u32::from(pressed).to_variant(),
            ]),
        )
    }

    fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) -> Result<(), String> {
        self.notify(
            "NotifyPointerAxis",
            tuple([
                object_path(&self.session_handle)?,
                options(|dict| dict.insert("finish", finish)),
                dx.to_variant(),
                dy.to_variant(),
            ]),
        )
    }

    fn keyboard_keysym(&self, keysym: i32, pressed: bool) -> Result<(), String> {
        self.notify(
            "NotifyKeyboardKeysym",
            tuple([
                object_path(&self.session_handle)?,
                options(|_| {}),
                keysym.to_variant(),
                u32::from(pressed).to_variant(),
            ]),
        )
    }
}

impl Drop for RemoteDesktopPortal {
    fn drop(&mut self) {
        if let Some(subscription) = self.closed_subscription.take() {
            self.connection.signal_unsubscribe(subscription);
        }
        // `session_guard` closes the session after the signal is detached.
    }
}

/// Asks at the desk, now, for the grant a phone will later resume silently —
/// so the person holding only the phone never meets a dialog. Returns whether
/// control (not just the picture) was granted; only then is it remembered.
/// Blocks until the dialog is answered; run it off the UI thread.
pub(crate) fn preauthorize_remote_control(device_id: &str) -> Result<bool, String> {
    if super::x11_desktop::wanted() {
        // No portal, so no dialog: this click at the desk is the approval.
        let desk = super::x11_desktop::X11Desktop::connect()?;
        crate::device_links::config::set_remote_desktop_token(
            device_id,
            Some(super::x11_desktop::DESK_APPROVAL),
        );
        return Ok(desk.can_control());
    }
    let cancel = AtomicBool::new(false);
    let (portal, source) = RemoteDesktopPortal::start("", &cancel, None)?;
    let token = source.token_to_remember();
    crate::device_links::config::set_remote_desktop_token(device_id, token);
    drop(portal);
    Ok(token.is_some())
}

/// Whether the desktop portal offers RemoteDesktop and ScreenCast at all. The
/// portals of Cinnamon, MATE and XFCE do not export them.
pub(crate) fn portal_available() -> bool {
    let Ok(connection) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        return false;
    };
    [REMOTE_INTERFACE, SCREEN_INTERFACE]
        .into_iter()
        .all(|interface| {
            connection
                .call_sync(
                    Some(PORTAL_BUS),
                    PORTAL_PATH,
                    "org.freedesktop.DBus.Properties",
                    "Get",
                    Some(&(interface, "version").to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    2_000,
                    None::<&gio::Cancellable>,
                )
                .is_ok()
        })
}

pub(crate) fn input_actions(
    packet: &crate::device_links::packet::NetworkPacket,
) -> Result<Vec<PortalInputAction>, String> {
    use PortalInputAction as Action;

    let bounded = |name: &str, low: f64, high: f64| -> Result<f64, String> {
        let value = packet
            .body
            .get(name)
            .and_then(serde_json::Value::as_f64)
            .unwrap_or(0.0);
        if value.is_finite() {
            Ok(value.clamp(low, high))
        } else {
            Err(format!("DeskLink remote input {name} is not finite"))
        }
    };
    // A relative delta is a per-event nudge; a position is a place on screen.
    // Clamping positions like deltas pinned taps on any desktop wider than 2000
    // logical pixels.
    let number = |name: &str| bounded(name, -2000.0, 2000.0);
    let position = |name: &str| bounded(name, 0.0, MAX_SCREEN_DIMENSION);
    let dx = number("dx")?;
    let dy = number("dy")?;
    let has_x = packet.body.contains_key("x");
    let has_y = packet.body.contains_key("y");
    if has_x != has_y {
        return Err("DeskLink absolute pointer input needs both x and y".to_string());
    }
    let mut actions = Vec::new();
    if has_x {
        actions.push(Action::AbsoluteMotion(position("x")?, position("y")?));
    }

    if packet.get_bool("scroll").unwrap_or(false) {
        actions.push(Action::Scroll(dx, dy));
    } else if packet.get_bool("singleclick").unwrap_or(false) {
        actions.extend(click_actions(0x110, 1)); // BTN_LEFT
    } else if packet.get_bool("doubleclick").unwrap_or(false) {
        actions.extend(click_actions(0x110, 2));
    } else if packet.get_bool("middleclick").unwrap_or(false) {
        actions.extend(click_actions(0x112, 1)); // BTN_MIDDLE
    } else if packet.get_bool("rightclick").unwrap_or(false) {
        actions.extend(click_actions(0x111, 1)); // BTN_RIGHT
    } else if packet.get_bool("singlehold").unwrap_or(false) {
        actions.push(Action::Button {
            evdev_code: 0x110,
            pressed: true,
        });
    } else if packet.get_bool("singlerelease").unwrap_or(false) {
        actions.push(Action::Button {
            evdev_code: 0x110,
            pressed: false,
        });
    } else if let Some(key) = packet.get_str("key") {
        if key.len() > MAX_TYPED_TEXT_BYTES {
            return Err("DeskLink remote keyboard input is too long".to_string());
        }
        actions.push(Action::TypeText {
            text: key.to_string(),
            modifiers: modifier_mask(packet),
        });
    } else if let Some(key) = packet.get_i64("specialKey") {
        if let Some(keysym) = special_key_to_keysym(key) {
            let modifiers = modifier_keysyms(modifier_mask(packet));
            for modifier in &modifiers {
                actions.push(Action::Key {
                    keysym: *modifier,
                    pressed: true,
                });
            }
            actions.push(Action::Key {
                keysym,
                pressed: true,
            });
            actions.push(Action::Key {
                keysym,
                pressed: false,
            });
            for modifier in modifiers.into_iter().rev() {
                actions.push(Action::Key {
                    keysym: modifier,
                    pressed: false,
                });
            }
        }
    }

    if actions.is_empty() && (dx != 0.0 || dy != 0.0) {
        actions.push(Action::RelativeMotion(dx, dy));
    }
    Ok(actions)
}

fn click_actions(evdev_code: i32, click_count: u8) -> Vec<PortalInputAction> {
    let mut actions = Vec::with_capacity(click_count as usize * 2);
    for _ in 0..click_count {
        actions.push(PortalInputAction::Button {
            evdev_code,
            pressed: true,
        });
        actions.push(PortalInputAction::Button {
            evdev_code,
            pressed: false,
        });
    }
    actions
}

fn modifier_mask(packet: &crate::device_links::packet::NetworkPacket) -> u32 {
    u32::from(packet.get_bool("ctrl").unwrap_or(false))
        | (u32::from(packet.get_bool("alt").unwrap_or(false)) << 1)
        | (u32::from(packet.get_bool("shift").unwrap_or(false)) << 2)
        | (u32::from(packet.get_bool("super").unwrap_or(false)) << 3)
}

fn modifier_keysyms(mask: u32) -> Vec<i32> {
    [(1, 0xffe3), (2, 0xffe9), (4, 0xffe1), (8, 0xffeb)]
        .into_iter()
        .filter_map(|(bit, keysym)| (mask & bit != 0).then_some(keysym))
        .collect()
}

fn special_key_to_keysym(key: i64) -> Option<i32> {
    Some(match key {
        1 => 0xff08,                           // Backspace
        2 => 0xff09,                           // Tab
        3 | 12 => 0xff0d,                      // Return
        4 => 0xff51,                           // Left
        5 => 0xff52,                           // Up
        6 => 0xff53,                           // Right
        7 => 0xff54,                           // Down
        8 => 0xff55,                           // PageUp
        9 => 0xff56,                           // PageDown
        10 => 0xff50,                          // Home
        11 => 0xff57,                          // End
        13 => 0xffff,                          // Delete
        14 => 0xff1b,                          // Escape
        21..=32 => 0xffbe + (key as i32 - 21), // F1..F12
        40 => 0xffeb,                          // Super (GNOME Activities)
        _ => return None,
    })
}

fn type_text(
    portal: &(impl DesktopSession + ?Sized),
    text: &str,
    modifier_mask: u32,
) -> Result<(), String> {
    let modifiers = modifier_keysyms(modifier_mask);
    for keysym in text_keysyms(text) {
        for modifier in &modifiers {
            portal.keyboard_keysym(*modifier, true)?;
        }
        portal.keyboard_keysym(keysym, true)?;
        portal.keyboard_keysym(keysym, false)?;
        for modifier in modifiers.iter().rev() {
            portal.keyboard_keysym(*modifier, false)?;
        }
    }
    Ok(())
}

/// Position of the PipeWire fd in the reply's fd list.
///
/// `OpenPipeWireRemote` returns `(h)`: a D-Bus *handle*, which is an index into
/// the fd list that travels with the reply. It is a different variant type from
/// `i32`, so reading it as one never succeeds.
fn pipewire_fd_index(reply: &glib::Variant) -> Result<i32, String> {
    reply
        .child_value(0)
        .get::<glib::variant::Handle>()
        .map(|handle| handle.0)
        .ok_or_else(|| "Desktop portal returned an invalid PipeWire FD handle".to_string())
}

/// The keysyms that type `text`.
///
/// Newline and tab are keys, not characters: sent as raw keysyms 0x0a / 0x09
/// they press nothing, so typed text silently stopped at the first line break.
/// Other control characters are dropped rather than sent as meaningless keys.
fn text_keysyms(text: &str) -> Vec<i32> {
    text.chars()
        .filter_map(|character| match character {
            '\n' | '\r' => Some(0xff0d), // Return
            '\t' => Some(0xff09),        // Tab
            control if control.is_control() => None,
            latin1 if (latin1 as u32) <= 0xff => Some(latin1 as i32),
            other => Some(0x0100_0000 | other as i32),
        })
        .collect()
}

fn request(
    context: &glib::MainContext,
    connection: &gio::DBusConnection,
    cancel: &AtomicBool,
    object_path: &str,
    interface: &str,
    method: &str,
    parameters: glib::Variant,
) -> Result<glib::Variant, String> {
    let method_name = method.to_string();
    let callback_method = method_name.clone();
    let response_path = Rc::new(RefCell::new(None::<String>));
    let response = Rc::new(RefCell::new(None::<Result<glib::Variant, String>>));
    context
        .with_thread_default(|| {
            let callback_path = Rc::clone(&response_path);
            let callback_response = Rc::clone(&response);
            // Subscribe on the dedicated portal context before invoking the
            // method. Portal responses may arrive before the method reply.
            let subscription = connection.signal_subscribe(
                Some(PORTAL_BUS),
                Some(REQUEST_INTERFACE),
                Some("Response"),
                None,
                None,
                gio::DBusSignalFlags::NONE,
                move |_, _, path, _, _, params| {
                    if callback_path.borrow().as_deref() != Some(path) {
                        return;
                    }
                    let code = params.child_value(0).get::<u32>();
                    let values = params.child_value(1);
                    let result = match code {
                        Some(0) => Ok(values),
                        Some(1) => Err(format!(
                            "Desktop permission request {callback_method} was cancelled"
                        )),
                        Some(_) => Err(format!(
                            "Desktop permission request {callback_method} was denied"
                        )),
                        None => Err("Desktop portal returned a malformed response".to_string()),
                    };
                    *callback_response.borrow_mut() = Some(result);
                },
            );
            let result = (|| {
                let reply = connection
                    .call_sync(
                        Some(PORTAL_BUS),
                        object_path,
                        interface,
                        &method_name,
                        Some(&parameters),
                        None,
                        gio::DBusCallFlags::NONE,
                        10_000,
                        None::<&gio::Cancellable>,
                    )
                    .map_err(|error| format!("Desktop portal {method} failed: {error}"))?;
                let path = reply
                    .child_value(0)
                    .get::<String>()
                    .ok_or_else(|| format!("Desktop portal {method} returned no request handle"))?;
                if !glib::Variant::is_object_path(&path) {
                    return Err(format!(
                        "Desktop portal {method} returned an invalid request handle"
                    ));
                }
                *response_path.borrow_mut() = Some(path);

                let deadline = Instant::now() + REQUEST_TIMEOUT;
                loop {
                    if let Some(result) = response.borrow_mut().take() {
                        return result;
                    }
                    if cancel.load(Ordering::Acquire) {
                        return Err(format!("Desktop permission request {method} was cancelled"));
                    }
                    if Instant::now() >= deadline {
                        return Err(format!("Timed out waiting for desktop portal {method}"));
                    }
                    while context.pending() {
                        context.iteration(false);
                    }
                    thread::sleep(Duration::from_millis(5));
                }
            })();
            connection.signal_unsubscribe(subscription);
            result
        })
        .map_err(|error| error.to_string())?
}

fn options(build: impl FnOnce(&glib::VariantDict)) -> glib::Variant {
    let dict = glib::VariantDict::new(None);
    build(&dict);
    dict.end()
}

fn tuple(values: impl IntoIterator<Item = glib::Variant>) -> glib::Variant {
    glib::Variant::tuple_from_iter(values)
}

fn object_path(path: &str) -> Result<glib::Variant, String> {
    ObjectPath::try_from(path.to_string())
        .map(|path| path.to_variant())
        .map_err(|_| "Desktop portal returned an invalid object path".to_string())
}

fn lookup<T: glib::variant::FromVariant>(
    values: &glib::Variant,
    key: &str,
) -> Result<Option<T>, String> {
    let value = dict_value(values, key);
    value
        .map(|value| {
            value
                .get::<T>()
                .ok_or_else(|| format!("Desktop portal result {key} has the wrong type"))
        })
        .transpose()
}

fn dict_value(values: &glib::Variant, key: &str) -> Option<glib::Variant> {
    let dict = glib::VariantDict::new(Some(values));
    dict.lookup_value(key, None)
}

fn token() -> String {
    format!("desklink_{}", uuid::Uuid::new_v4().simple())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::packet::NetworkPacket;

    #[test]
    fn input_translation_preserves_absolute_position_and_click_order() {
        let mut packet = NetworkPacket::new("desklink.mousepad.request");
        packet.set("x", 400);
        packet.set("y", 250);
        packet.set("singleclick", true);

        assert_eq!(
            input_actions(&packet).unwrap(),
            vec![
                PortalInputAction::AbsoluteMotion(400.0, 250.0),
                PortalInputAction::Button {
                    evdev_code: 0x110,
                    pressed: true,
                },
                PortalInputAction::Button {
                    evdev_code: 0x110,
                    pressed: false,
                },
            ]
        );
    }

    /// A laptop and a monitor come back as two streams; each keeps its own
    /// node and size so the phone can switch between them.
    #[test]
    fn every_shared_screen_is_parsed_with_its_own_node_and_size() {
        let dict = |w: i32, h: i32| {
            let d = glib::VariantDict::new(None);
            d.insert("size", (w, h));
            d.end()
        };
        let stream =
            |node: u32, w, h| glib::Variant::tuple_from_iter([node.to_variant(), dict(w, h)]);
        assert_eq!(
            parse_stream(&stream(41, 1920, 1080)).unwrap(),
            PortalStream {
                node_id: 41,
                width: 1920,
                height: 1080,
                pipewire_serial: None
            }
        );
        assert_eq!(parse_stream(&stream(42, 2560, 1600)).unwrap().width, 2560);
        assert!(
            parse_stream(&stream(0, 1920, 1080)).is_err(),
            "node 0 is not a stream"
        );
        assert!(parse_stream(&stream(5, 0, 1080)).is_err());
    }

    #[test]
    fn every_point_of_a_scaled_screen_is_reachable() {
        // The 133 % laptop panel: 1920x1200 logical, 2560x1600 pixels.
        let (logical, pixels) = ((1920, 1200), Some((2560, 1600)));
        // Inside the area GNOME accepts: one absolute move, in pixels.
        assert_eq!(
            pointer_move(960.0, 600.0, logical, pixels),
            ((1280.0, 800.0), None)
        );
        // Past it (the dock): the furthest accepted point, then a nudge.
        let ((ax, ay), nudge) = pointer_move(1800.0, 1150.0, logical, pixels);
        assert_eq!((ax, ay), (1919.0, 1199.0));
        let (dx, dy) = nudge.unwrap();
        assert!((ax * 0.75 + dx - 1800.0).abs() < 1e-9 && (ay * 0.75 + dy - 1150.0).abs() < 1e-9);
        // An unscaled screen: logical numbers as they are, never a nudge.
        assert_eq!(
            pointer_move(1800.0, 1000.0, (1920, 1080), Some((1920, 1080))),
            ((1800.0, 1000.0), None)
        );
        assert_eq!(
            pointer_move(1800.0, 1000.0, (1920, 1080), None),
            ((1800.0, 1000.0), None)
        );
        // Off-screen requests stay on the screen.
        assert_eq!(
            pointer_move(5000.0, -3.0, (1920, 1080), None),
            ((1919.0, 0.0), None)
        );
    }

    #[test]
    fn only_a_grant_with_control_is_remembered() {
        let source = |devices: u32, token: Option<&str>| PortalScreenSource {
            fd: std::fs::File::open("/dev/null").unwrap().into(),
            node_id: 1,
            pipewire_serial: None,
            width: 10,
            height: 10,
            granted_device_types: devices,
            restore_token: token.map(str::to_string),
        };
        assert_eq!(source(3, Some("t")).token_to_remember(), Some("t"));
        assert_eq!(source(0, Some("t")).token_to_remember(), None, "view-only");
        assert_eq!(
            source(1, Some("t")).token_to_remember(),
            None,
            "keyboard only"
        );
        assert_eq!(
            source(3, None).token_to_remember(),
            None,
            "user chose not to remember"
        );
    }

    #[test]
    fn input_translation_bounds_motion_and_maps_special_keys() {
        let mut packet = NetworkPacket::new("desklink.mousepad.request");
        packet.set("dx", 50_000.0);
        packet.set("dy", -50_000.0);
        assert_eq!(
            input_actions(&packet).unwrap(),
            vec![PortalInputAction::RelativeMotion(2000.0, -2000.0)]
        );

        packet.body.clear();
        packet.set("specialKey", 21);
        assert_eq!(
            input_actions(&packet).unwrap(),
            vec![
                PortalInputAction::Key {
                    keysym: 0xffbe,
                    pressed: true,
                },
                PortalInputAction::Key {
                    keysym: 0xffbe,
                    pressed: false,
                },
            ]
        );

        // The phone's Super button: a tap of Super alone opens Activities.
        packet.body.clear();
        packet.set("specialKey", 40);
        assert_eq!(
            input_actions(&packet).unwrap(),
            vec![
                PortalInputAction::Key {
                    keysym: 0xffeb,
                    pressed: true
                },
                PortalInputAction::Key {
                    keysym: 0xffeb,
                    pressed: false
                },
            ]
        );
    }

    /// `OpenPipeWireRemote` replies `(h)`: the fd is a D-Bus *handle* (an index
    /// into the reply's fd list). It was read as `i32`, which never matches the
    /// `h` type, so every session failed with "invalid PipeWire FD handle" right
    /// after the user approved the GNOME dialog.
    #[test]
    fn the_pipewire_fd_is_read_as_a_dbus_handle() {
        let reply = glib::Variant::tuple_from_iter([glib::variant::Handle(3).to_variant()]);
        assert_eq!(pipewire_fd_index(&reply), Ok(3));

        let integer = glib::Variant::tuple_from_iter([3i32.to_variant()]);
        assert!(
            pipewire_fd_index(&integer).is_err(),
            "an int32 is not a handle and must not be accepted"
        );
    }

    /// Desktops wider than 2000 logical pixels are normal. A ±2000 clamp on
    /// *positions* pinned every tap on the right of a 2560-wide screen.
    #[test]
    fn absolute_positions_are_only_bounded_by_the_largest_supported_screen() {
        let mut packet = NetworkPacket::new("desklink.mousepad.request");
        packet.set("x", 2500);
        packet.set("y", 1400);
        assert_eq!(
            input_actions(&packet).unwrap(),
            vec![PortalInputAction::AbsoluteMotion(2500.0, 1400.0)]
        );

        packet.set("x", 1_000_000);
        packet.set("y", -50);
        assert_eq!(
            input_actions(&packet).unwrap(),
            vec![PortalInputAction::AbsoluteMotion(8192.0, 0.0)]
        );
    }

    /// A newline typed on the phone must press Return; sent as keysym 0x0a it
    /// is not a key at all and the text silently stops there.
    #[test]
    fn typed_text_maps_newline_and_tab_and_drops_other_control_characters() {
        assert_eq!(text_keysyms("a\nb\t"), vec![0x61, 0xff0d, 0x62, 0xff09]);
        assert_eq!(text_keysyms("x\u{7}y\r"), vec![0x78, 0x79, 0xff0d]);
        assert_eq!(text_keysyms("é"), vec![0xe9]);
        assert_eq!(text_keysyms("€"), vec![0x0100_20ac]);
    }

    #[test]
    fn typed_text_is_bounded_but_long_enough_for_a_sentence() {
        let mut packet = NetworkPacket::new("desklink.mousepad.request");
        packet.set("key", "x".repeat(200));
        assert!(input_actions(&packet).is_ok());

        packet.set("key", "x".repeat(2000));
        assert!(input_actions(&packet).is_err());
    }
}
