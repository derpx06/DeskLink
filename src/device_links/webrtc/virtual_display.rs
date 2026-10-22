//! The phone as a second display (F2, `request-view {display: "extend"}`).
//!
//! A RemoteDesktop portal session of its own selects a VIRTUAL source. The
//! portal's stream has no size: the capture pins the phone's pixel size on
//! PipeWire, and Mutter creates the monitor at that size. Once it appears,
//! [`apply_scale`] gives that monitor alone a scale (through Mutter's
//! DisplayConfig, temporarily) so it is not unusably small, leaving every
//! other monitor as it was. Input goes through EIS: the portal refuses
//! absolute positions on a stream without a size, so its D-Bus calls cannot
//! place the pointer there.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use gtk::gio;
use gtk::glib::{self, prelude::*};

use super::remote_desktop_portal::{
    DesktopSession, PortalScreenSource, PortalStream, RemoteDesktopPortal,
};
use crate::device_links::eis::{Command, EisInput};

/// The logical long side an extended display aims for: big enough to hold
/// windows, small enough to read on a phone.
const LOGICAL_LONG_SIDE: (f64, f64) = (1280.0, 1600.0);
/// evdev KEY_LEFTSHIFT and KEY_RIGHTALT (AltGr), for keysyms on a shifted
/// level.
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTALT: u32 = 100;

/// Whether this desktop can add a virtual monitor: a Wayland session whose
/// ScreenCast portal offers the VIRTUAL source type.
pub(crate) fn supported() -> bool {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return false;
    }
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>) else {
        return false;
    };
    bus.call_sync(
        Some("org.freedesktop.portal.Desktop"),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(&("org.freedesktop.portal.ScreenCast", "AvailableSourceTypes").to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        3_000,
        None::<&gio::Cancellable>,
    )
    .ok()
    .and_then(|reply| reply.child_value(0).as_variant())
    .and_then(|value| value.get::<u32>())
    .is_some_and(|types| types & 4 != 0)
}

/// The scale (from `supported`) that brings a `pixels` monitor's logical
/// long side closest to [`LOGICAL_LONG_SIDE`]: about 2 on a phone, 1.5 on a
/// tablet.
pub(crate) fn pick_scale(pixels: (i32, i32), supported: &[f64]) -> f64 {
    let long = f64::from(pixels.0.max(pixels.1).max(1));
    let (low, high) = LOGICAL_LONG_SIDE;
    let distance = |scale: f64| {
        let logical = long / scale;
        if logical < low {
            low - logical
        } else if logical > high {
            logical - high
        } else {
            (logical - (low + high) / 2.0).abs() / 1000.0
        }
    };
    supported
        .iter()
        .copied()
        .filter(|scale| *scale >= 1.0)
        .min_by(|a, b| distance(*a).total_cmp(&distance(*b)))
        .unwrap_or(1.0)
}

/// The logical size of a `pixels` monitor at `scale`.
pub(crate) fn logical_size(pixels: (i32, i32), scale: f64) -> (i32, i32) {
    let scale = scale.max(1.0);
    (
        (f64::from(pixels.0) / scale).round() as i32,
        (f64::from(pixels.1) / scale).round() as i32,
    )
}

fn display_config(
    method: &str,
    args: Option<glib::Variant>,
    reply: Option<&str>,
) -> Result<glib::Variant, String> {
    let bus = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|error| error.to_string())?;
    bus.call_sync(
        Some("org.gnome.Mutter.DisplayConfig"),
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
        method,
        args.as_ref(),
        reply
            .map(glib::VariantTy::new)
            .transpose()
            .map_err(|e| e.to_string())?,
        gio::DBusCallFlags::NONE,
        5_000,
        None::<&gio::Cancellable>,
    )
    .map_err(|error| format!("DisplayConfig {method}: {error}"))
}

const CURRENT_STATE: &str = "(ua((ssss)a(siiddada{sv})a{sv})a(iiduba(ssss)a{sv})a{sv})";

/// Every connector Mutter knows now (empty without DisplayConfig).
pub(crate) fn connectors() -> Vec<String> {
    display_config("GetCurrentState", None, Some(CURRENT_STATE))
        .map(|state| {
            let monitors = state.child_value(1);
            (0..monitors.n_children())
                .filter_map(|i| {
                    monitors
                        .child_value(i)
                        .child_value(0)
                        .child_value(0)
                        .str()
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Waits for the monitor the capture created (a connector not in
/// `before`), then sets its scale alone with [`pick_scale`]. Every other
/// logical monitor is passed back exactly as it is. Returns the scale in
/// effect (1 when DisplayConfig is absent or refuses).
pub(crate) fn apply_scale(before: &[String], pixels: (i32, i32)) -> f64 {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        match try_apply_scale(before, pixels) {
            Ok(Some(scale)) => return scale,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                eprintln!("[DeskLink] The extended display did not appear in the display layout");
                return 1.0;
            }
            Err(error) => {
                eprintln!("[DeskLink] Could not scale the extended display: {error}");
                return 1.0;
            }
        }
    }
}

fn try_apply_scale(before: &[String], pixels: (i32, i32)) -> Result<Option<f64>, String> {
    let state = display_config("GetCurrentState", None, Some(CURRENT_STATE))?;
    let serial = state.child_value(0).get::<u32>().unwrap_or(0);
    let monitors = state.child_value(1);
    // connector -> (current mode id, supported scales)
    let mut modes: HashMap<String, (String, Vec<f64>)> = HashMap::new();
    for i in 0..monitors.n_children() {
        let monitor = monitors.child_value(i);
        let Some(connector) = monitor
            .child_value(0)
            .child_value(0)
            .str()
            .map(str::to_string)
        else {
            continue;
        };
        let list = monitor.child_value(1);
        for m in 0..list.n_children() {
            let mode = list.child_value(m);
            let current = glib::VariantDict::new(Some(&mode.child_value(6)))
                .lookup::<bool>("is-current")
                .ok()
                .flatten()
                .unwrap_or(false);
            if current {
                let id = mode.child_value(0).str().unwrap_or_default().to_string();
                let scales = mode.child_value(5).get::<Vec<f64>>().unwrap_or_default();
                modes.insert(connector.clone(), (id, scales));
            }
        }
    }
    let Some(added) = modes.keys().find(|c| !before.contains(c)).cloned() else {
        return Ok(None);
    };
    let Some((_, scales)) = modes.get(&added).cloned() else {
        return Ok(None);
    };
    let scale = pick_scale(pixels, &scales);
    let logical = state.child_value(2);
    let mut config = Vec::new();
    for i in 0..logical.n_children() {
        let entry = logical.child_value(i);
        let x = entry.child_value(0).get::<i32>().unwrap_or(0);
        let y = entry.child_value(1).get::<i32>().unwrap_or(0);
        let mut monitor_scale = entry.child_value(2).get::<f64>().unwrap_or(1.0);
        let transform = entry.child_value(3).get::<u32>().unwrap_or(0);
        let primary = entry.child_value(4).get::<bool>().unwrap_or(false);
        let specs = entry.child_value(5);
        let mut members = Vec::new();
        for s in 0..specs.n_children() {
            let connector = specs
                .child_value(s)
                .child_value(0)
                .str()
                .unwrap_or_default()
                .to_string();
            if connector == added {
                monitor_scale = scale;
            }
            let mode = modes
                .get(&connector)
                .map(|(id, _)| id.clone())
                .ok_or_else(|| format!("{connector} has no current mode"))?;
            members.push((connector, mode, HashMap::<String, glib::Variant>::new()));
        }
        config.push((x, y, monitor_scale, transform, primary, members));
    }
    // Method 1: temporary. Nothing is saved to the user's monitors.xml.
    display_config(
        "ApplyMonitorsConfig",
        Some(
            (
                serial,
                1u32,
                config,
                HashMap::<String, glib::Variant>::new(),
            )
                .to_variant(),
        ),
        None,
    )?;
    Ok(Some(scale))
}

/// The keycode (evdev) and shift level that type `keysym` with the
/// desktop's keymap, looked up once through GDK on the main thread.
fn keycode_for(keysym: u32) -> Option<(u32, i32)> {
    /// keysym → (evdev keycode, level), `None` when no key types it.
    type Keys = HashMap<u32, Option<(u32, i32)>>;
    static CACHE: Mutex<Option<Keys>> = Mutex::new(None);
    if let Some(known) = CACHE
        .lock()
        .ok()
        .and_then(|cache| cache.as_ref().and_then(|cache| cache.get(&keysym).copied()))
    {
        return known;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    glib::MainContext::default().invoke(move || {
        let found = gtk::gdk::Display::default()
            .and_then(|display| {
                // SAFETY: any u32 is a valid GDK keyval number.
                let key = unsafe { glib::translate::from_glib::<u32, gtk::gdk::Key>(keysym) };
                gtk::gdk::prelude::DisplayExtManual::map_keyval(&display, key)
            })
            .and_then(|keys| {
                keys.into_iter()
                    .filter(|key| key.group() == 0 && key.keycode() >= 8)
                    .min_by_key(|key| key.level())
                    .map(|key| (key.keycode() - 8, key.level()))
            });
        let _ = sender.send(found);
    });
    let found = receiver.recv_timeout(Duration::from_secs(1)).ok().flatten();
    if let Ok(mut cache) = CACHE.lock() {
        cache.get_or_insert_with(HashMap::new).insert(keysym, found);
    }
    found
}

/// An extended display's session: the portal session (kept for its
/// lifetime) and its EIS input.
pub(crate) struct VirtualDisplay {
    portal: RemoteDesktopPortal,
    input: Option<EisInput>,
    mapping_id: Option<String>,
    stream: Cell<PortalStream>,
}

impl VirtualDisplay {
    /// Asks for a virtual monitor of `pixels` (the portal's dialog, unless
    /// `restore_token` resumes a grant), and opens its input.
    pub(crate) fn start(
        cancel: &AtomicBool,
        restore_token: Option<&str>,
        pixels: (i32, i32),
    ) -> Result<(Self, PortalScreenSource), String> {
        let (portal, source) = RemoteDesktopPortal::start_virtual(cancel, restore_token, pixels)?;
        let input = if source.granted_device_types & 3 == 3 {
            match portal
                .connect_to_eis()
                .and_then(|socket| EisInput::connect(socket, "DeskLink"))
            {
                Ok(input) => Some(input),
                Err(error) => {
                    eprintln!("[DeskLink] The extended display is view-only: {error}");
                    None
                }
            }
        } else {
            None
        };
        let stream = portal.active();
        Ok((
            Self {
                portal,
                input,
                mapping_id: source.mapping_id.clone(),
                stream: Cell::new(stream),
            },
            source,
        ))
    }

    /// Records the monitor's logical size, once its scale is set, and waits
    /// (briefly) for its input region.
    pub(crate) fn settle(&self, logical: (i32, i32)) {
        let mut stream = self.stream.get();
        stream.width = logical.0.max(1);
        stream.height = logical.1.max(1);
        self.stream.set(stream);
        if let Some(input) = &self.input {
            let width = Some(stream.width as u32);
            if !input.wait_for_region(self.mapping_id.as_deref(), width, Duration::from_secs(4)) {
                eprintln!("[DeskLink] The extended display's input region did not appear");
            }
            // Measured on GNOME 50: the second absolute move after the
            // region appears keeps its old x. Two priming moves to the
            // middle take that hit before the phone's first tap.
            for _ in 0..2 {
                let _ = input.send(Command::Absolute {
                    mapping_id: self.mapping_id.clone(),
                    x: 0.5,
                    y: 0.5,
                });
                std::thread::sleep(Duration::from_millis(30));
            }
        }
    }

    pub(crate) fn can_control(&self) -> bool {
        self.input.is_some()
    }

    fn send(&self, command: Command) -> Result<(), String> {
        self.input
            .as_ref()
            .ok_or_else(|| "The extended display is view-only".to_string())?
            .send(command)
    }
}

impl DesktopSession for VirtualDisplay {
    fn active(&self) -> PortalStream {
        self.stream.get()
    }

    fn stream_count(&self) -> usize {
        1
    }

    fn next_stream(&self) -> PortalStream {
        self.stream.get()
    }

    fn knows_active_pixels(&self) -> bool {
        true
    }

    fn set_active_pixels(&self, _size: (i32, i32)) {}

    fn is_session_closed(&self) -> bool {
        self.portal.is_session_closed()
            || self
                .input
                .as_ref()
                .is_some_and(|input| !input.is_connected())
    }

    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<(), String> {
        self.send(Command::Relative(dx, dy))
    }

    fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), String> {
        let stream = self.stream.get();
        self.send(Command::Absolute {
            mapping_id: self.mapping_id.clone(),
            x: x / f64::from(stream.width.max(1)),
            y: y / f64::from(stream.height.max(1)),
        })
    }

    fn pointer_button(&self, evdev_button: i32, pressed: bool) -> Result<(), String> {
        self.send(Command::Button(evdev_button.max(0) as u32, pressed))
    }

    fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) -> Result<(), String> {
        self.send(Command::Scroll {
            dx,
            dy,
            stop: finish,
        })
    }

    fn keyboard_keysym(&self, keysym: i32, pressed: bool) -> Result<(), String> {
        let (code, level) = keycode_for(keysym as u32)
            .ok_or_else(|| format!("No key types keysym {keysym:#x} here"))?;
        // A shifted level is pressed with its modifier around it.
        let modifier = match level {
            1 => Some(KEY_LEFTSHIFT),
            2 => Some(KEY_RIGHTALT),
            3 => Some(KEY_LEFTSHIFT),
            _ => None,
        };
        if pressed {
            if let Some(modifier) = modifier {
                self.send(Command::Key(modifier, true))?;
            }
            self.send(Command::Key(code, true))
        } else {
            self.send(Command::Key(code, false))?;
            match modifier {
                Some(modifier) => self.send(Command::Key(modifier, false)),
                None => Ok(()),
            }
        }
    }
}

/// Debug builds: the extended-display path without a phone (portal dialog,
/// virtual monitor, scale, EIS pointer), for the isolated GNOME harness.
/// Moves the pointer to three points of the new display, three seconds
/// apart, so a test can read where it went.
#[cfg(debug_assertions)]
pub(crate) fn selftest(pixels: (i32, i32)) -> Result<String, String> {
    use gst::prelude::*;
    gst::init().map_err(|error| error.to_string())?;
    let cancel = AtomicBool::new(false);
    let (display, source) = VirtualDisplay::start(&cancel, None, pixels)?;
    let before = connectors();
    let pipeline = gst::Pipeline::new();
    let capture =
        super::screen::pipewire_source(&source.fd, source.node_id, source.pipewire_serial)?;
    let caps = gst::ElementFactory::make("capsfilter")
        .property(
            "caps",
            gst::Caps::builder("video/x-raw")
                .field("width", pixels.0)
                .field("height", pixels.1)
                .build(),
        )
        .build()
        .map_err(|error| error.to_string())?;
    let sink = gst::ElementFactory::make("fakesink")
        .property("sync", false)
        .build()
        .map_err(|error| error.to_string())?;
    pipeline
        .add_many([&capture, &caps, &sink])
        .map_err(|error| error.to_string())?;
    gst::Element::link_many([&capture, &caps, &sink]).map_err(|error| error.to_string())?;
    pipeline
        .set_state(gst::State::Playing)
        .map_err(|error| format!("{error:?}"))?;
    let scale = apply_scale(&before, pixels);
    let logical = logical_size(pixels, scale);
    display.settle(logical);
    let mut moves = Vec::new();
    for (x, y) in [
        (0.0, 0.0),
        (0.25, 0.5),
        (0.5, 0.5),
        (0.75, 0.5),
        (0.5, 0.1),
        (0.999, 0.999),
    ] {
        let at = (f64::from(logical.0) * x, f64::from(logical.1) * y);
        moves.push(format!(
            "{:.0},{:.0}: {:?}",
            at.0,
            at.1,
            display.pointer_motion_absolute(at.0, at.1)
        ));
        debug_log!("[E2E] extend pointer at {:.0},{:.0}", at.0, at.1);
        std::thread::sleep(Duration::from_secs(2));
    }
    // Typed through EIS (a test opens a text field first): "Hi~".
    let typed = [0x48, 0x69, 0x7e]
        .iter()
        .map(|keysym| {
            display
                .keyboard_keysym(*keysym, true)
                .and_then(|_| display.keyboard_keysym(*keysym, false))
        })
        .collect::<Vec<_>>();
    debug_log!("[E2E] extend typed {typed:?}");
    std::thread::sleep(Duration::from_secs(2));
    let keys = [0x61u32, 0x41, 0x7e, 0xffe1]
        .iter()
        .map(|keysym| format!("{keysym:#x}->{:?}", keycode_for(*keysym)))
        .collect::<Vec<_>>();
    let layout = connectors();
    let _ = pipeline.set_state(gst::State::Null);
    Ok(format!(
        "pixels {pixels:?}, scale {scale}, logical {logical:?}, control {}, mapping {:?}, \
         connectors before {before:?} after {layout:?}, moves [{}], keys [{}]",
        display.can_control(),
        display.mapping_id,
        moves.join("; "),
        keys.join(" ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_extended_display_gets_a_readable_logical_size() {
        let gnome = [
            1.0,
            1.25,
            1.333_333_373,
            1.5,
            1.666_666_627,
            2.0,
            2.5,
            2.666_666_746,
        ];
        for pixels in [
            (2400, 1080),
            (2340, 1080),
            (2560, 1600),
            (2000, 1200),
            (3200, 1440),
        ] {
            let scale = pick_scale(pixels, &gnome);
            let (w, h) = logical_size(pixels, scale);
            assert!(
                (1280..=1600).contains(&w.max(h)),
                "{pixels:?} at {scale} is {w}x{h}"
            );
        }
        assert_eq!(pick_scale((1280, 720), &gnome), 1.0, "already that small");
        assert_eq!(pick_scale((2400, 1080), &[]), 1.0, "no scales offered");
        assert_eq!(logical_size((2400, 1080), 2.0), (1200, 540));
    }
}
