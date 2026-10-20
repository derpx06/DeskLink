//! Desktop sharing on X11 desktops that have no ScreenCast/RemoteDesktop
//! portal (Cinnamon, MATE, XFCE): the picture comes from `ximagesrc`
//! (`screen::DesktopSource::X11`) and input goes in through XTest.
//!
//! Chosen only when the session is X11 *and* the portal lacks those
//! interfaces; GNOME and KDE on X11 keep the portal and its consent dialog.
//! Nothing here asks at the desk, so a share is allowed only after someone at
//! this computer approved the phone in DeskLink ("Approve…" under Remote
//! Control), which records [`DESK_APPROVAL`] in memory — gone on restart,
//! unpairing or a new certificate, exactly like a remembered portal grant.

use std::cell::Cell;

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::cookie::VoidCookie;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::protocol::xtest::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use super::remote_desktop_portal::{DesktopSession, PortalStream};

/// The remembered "approved at the desk" grant for the X11 path, stored where
/// a portal restore token would be (`config::remote_desktop_token`).
pub(crate) const DESK_APPROVAL: &str = "desklink-x11-desk-approval";

/// One wheel click in the portal's smooth-scroll units (mutter's step).
const SCROLL_STEP: f64 = 10.0;
/// Wheel clicks one scroll event may produce; a bounded delta of 2000 would
/// otherwise be 200 clicks.
const MAX_SCROLL_CLICKS: i32 = 20;
const XK_SHIFT_L: u32 = 0xffe1;

/// Whether desktop shares should use this module instead of the portal.
pub(crate) fn wanted() -> bool {
    use_fallback(
        std::env::var("XDG_SESSION_TYPE").ok().as_deref(),
        super::remote_desktop_portal::portal_available,
    )
}

fn use_fallback(session_type: Option<&str>, portal_available: impl FnOnce() -> bool) -> bool {
    session_type.is_some_and(|kind| kind.eq_ignore_ascii_case("x11")) && !portal_available()
}

/// Group 1 of the server's keyboard mapping.
struct Keymap {
    min_keycode: u8,
    per_keycode: u8,
    keysyms: Vec<u32>,
}

impl Keymap {
    fn rows(&self) -> impl Iterator<Item = (u8, &[u32])> {
        let per = usize::from(self.per_keycode.max(1));
        (self.min_keycode..=u8::MAX).zip(self.keysyms.chunks(per))
    }

    /// The keycode that types `keysym`, and whether it needs Shift (column 0
    /// is unshifted, column 1 shifted). Unshifted wins wherever both exist.
    fn lookup(&self, keysym: u32) -> Option<(u8, bool)> {
        if keysym == 0 {
            return None;
        }
        (0..usize::from(self.per_keycode).min(2)).find_map(|column| {
            self.rows()
                .find(|(_, row)| row.get(column) == Some(&keysym))
                .map(|(keycode, _)| (keycode, column == 1))
        })
    }

    /// The last keycode with nothing on it, free to borrow for a character
    /// the layout cannot type.
    fn spare(&self) -> Option<u8> {
        self.rows()
            .filter(|(_, row)| row.iter().all(|&keysym| keysym == 0))
            .map(|(keycode, _)| keycode)
            .last()
    }
}

/// Whole steps in `total`, and what is left over for next time. Sub-step
/// motion would otherwise never move the pointer at all.
fn whole_steps(total: f64, step: f64) -> (i32, f64) {
    let steps = (total / step).trunc();
    (steps as i32, total - steps * step)
}

/// X core pointer button for an evdev button code.
fn x_button(evdev: i32) -> Option<u8> {
    Some(match evdev {
        0x110 => 1, // BTN_LEFT
        0x111 => 3, // BTN_RIGHT
        0x112 => 2, // BTN_MIDDLE
        0x113 => 8, // BTN_SIDE
        0x114 => 9, // BTN_EXTRA
        _ => return None,
    })
}

pub(crate) struct X11Desktop {
    conn: RustConnection,
    root: xproto::Window,
    width: i32,
    height: i32,
    xtest: bool,
    keymap: Keymap,
    /// The keysym currently borrowed onto `keymap.spare()`, if any.
    borrowed: Cell<Option<u32>>,
    motion_rest: Cell<(f64, f64)>,
    scroll_rest: Cell<(f64, f64)>,
}

impl X11Desktop {
    pub(crate) fn connect() -> Result<Self, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|error| {
            format!("DeskLink could not open the X display to share this desktop: {error}")
        })?;
        let setup = conn.setup();
        let root = setup
            .roots
            .get(screen)
            .ok_or("The X server reported no screen to share")?;
        let (root, width, height) = (
            root.root,
            i32::from(root.width_in_pixels),
            i32::from(root.height_in_pixels),
        );
        let (min_keycode, max_keycode) = (setup.min_keycode, setup.max_keycode);
        let xtest = conn
            .extension_information(xtest::X11_EXTENSION_NAME)
            .ok()
            .flatten()
            .is_some()
            && conn
                .xtest_get_version(2, 2)
                .ok()
                .and_then(|cookie| cookie.reply().ok())
                .is_some();
        let mapping = conn
            .get_keyboard_mapping(
                min_keycode,
                max_keycode.saturating_sub(min_keycode).saturating_add(1),
            )
            .map_err(|error| error.to_string())
            .and_then(|cookie| cookie.reply().map_err(|error| error.to_string()))
            .map_err(|error| format!("Could not read the X keyboard mapping: {error}"))?;
        if !xtest {
            eprintln!(
                "[DeskLink] This X server has no XTest extension: the phone can watch this desktop but not control it"
            );
        }
        Ok(Self {
            conn,
            root,
            width,
            height,
            xtest,
            keymap: Keymap {
                min_keycode,
                per_keycode: mapping.keysyms_per_keycode,
                keysyms: mapping.keysyms,
            },
            borrowed: Cell::new(None),
            motion_rest: Cell::new((0.0, 0.0)),
            scroll_rest: Cell::new((0.0, 0.0)),
        })
    }

    /// Whether input can be injected (the XTest extension exists).
    pub(crate) fn can_control(&self) -> bool {
        self.xtest
    }

    fn sent(
        &self,
        cookie: Result<VoidCookie<'_, RustConnection>, x11rb::errors::ConnectionError>,
    ) -> Result<(), String> {
        cookie
            .map(VoidCookie::ignore_error)
            .and_then(|()| self.conn.flush())
            .map_err(|error| format!("The X server connection failed: {error}"))
    }

    fn fake(&self, kind: u8, detail: u8, x: i16, y: i16) -> Result<(), String> {
        if !self.xtest {
            return Err("This X server has no XTest extension, so the phone can only watch".into());
        }
        self.sent(
            self.conn
                .xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, self.root, x, y, 0),
        )
    }

    fn click(&self, button: u8) -> Result<(), String> {
        self.fake(xproto::BUTTON_PRESS_EVENT, button, 0, 0)?;
        self.fake(xproto::BUTTON_RELEASE_EVENT, button, 0, 0)
    }

    /// Puts `keysym` on the spare keycode (as xdotool does) for a character
    /// the layout has no key for.
    // ponytail: one borrowed keycode, and clients see the remap through an
    // async MappingNotify; a fast run of unmapped characters can race it.
    fn borrow_keycode(&self, keysym: u32) -> Result<u8, String> {
        let keycode = self
            .keymap
            .spare()
            .ok_or("The X keyboard layout has no free key to type this character with")?;
        if self.borrowed.get() != Some(keysym) {
            let per = self.keymap.per_keycode.max(1);
            let keysyms = vec![keysym; usize::from(per)];
            self.sent(self.conn.change_keyboard_mapping(1, keycode, per, &keysyms))?;
            // A round trip, so the mapping is in place before the key press.
            self.conn
                .get_input_focus()
                .map_err(|error| error.to_string())?
                .reply()
                .map_err(|error| error.to_string())?;
            self.borrowed.set(Some(keysym));
        }
        Ok(keycode)
    }
}

impl DesktopSession for X11Desktop {
    fn active(&self) -> PortalStream {
        PortalStream {
            node_id: 0,
            width: self.width,
            height: self.height,
            pipewire_serial: None,
        }
    }

    /// `ximagesrc` captures the whole root window, every monitor at once.
    fn stream_count(&self) -> usize {
        1
    }

    fn next_stream(&self) -> PortalStream {
        self.active()
    }

    /// X11 has no logical/pixel split: positions are already pixels.
    fn knows_active_pixels(&self) -> bool {
        true
    }

    fn set_active_pixels(&self, _size: (i32, i32)) {}

    fn is_session_closed(&self) -> bool {
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(_)) => continue,
                Ok(None) => return false,
                Err(_) => return true,
            }
        }
    }

    fn pointer_motion(&self, dx: f64, dy: f64) -> Result<(), String> {
        let (rest_x, rest_y) = self.motion_rest.get();
        let (x, rest_x) = whole_steps(rest_x + dx, 1.0);
        let (y, rest_y) = whole_steps(rest_y + dy, 1.0);
        self.motion_rest.set((rest_x, rest_y));
        if x == 0 && y == 0 {
            return Ok(());
        }
        let clamp = |value: i32| value.clamp(i16::MIN.into(), i16::MAX.into()) as i16;
        // detail 1: relative motion.
        self.fake(xproto::MOTION_NOTIFY_EVENT, 1, clamp(x), clamp(y))
    }

    fn pointer_motion_absolute(&self, x: f64, y: f64) -> Result<(), String> {
        let x = x.round().clamp(0.0, f64::from(self.width - 1)) as i16;
        let y = y.round().clamp(0.0, f64::from(self.height - 1)) as i16;
        self.fake(xproto::MOTION_NOTIFY_EVENT, 0, x, y)
    }

    fn pointer_button(&self, evdev_button: i32, pressed: bool) -> Result<(), String> {
        let button = x_button(evdev_button)
            .ok_or_else(|| format!("No X pointer button for evdev code {evdev_button:#x}"))?;
        let kind = if pressed {
            xproto::BUTTON_PRESS_EVENT
        } else {
            xproto::BUTTON_RELEASE_EVENT
        };
        self.fake(kind, button, 0, 0)
    }

    /// Smooth scroll becomes wheel clicks (buttons 4/5 vertical, 6/7
    /// horizontal), one per [`SCROLL_STEP`], carrying the remainder.
    fn pointer_axis(&self, dx: f64, dy: f64, finish: bool) -> Result<(), String> {
        let (rest_x, rest_y) = self.scroll_rest.get();
        let (x, rest_x) = whole_steps(rest_x + dx, SCROLL_STEP);
        let (y, rest_y) = whole_steps(rest_y + dy, SCROLL_STEP);
        self.scroll_rest
            .set(if finish { (0.0, 0.0) } else { (rest_x, rest_y) });
        for (clicks, back, forward) in [(y, 4, 5), (x, 6, 7)] {
            let button = if clicks < 0 { back } else { forward };
            for _ in 0..clicks.abs().min(MAX_SCROLL_CLICKS) {
                self.click(button)?;
            }
        }
        Ok(())
    }

    fn keyboard_keysym(&self, keysym: i32, pressed: bool) -> Result<(), String> {
        let keysym = keysym as u32;
        let (keycode, shifted) = match self.keymap.lookup(keysym) {
            Some(found) => found,
            None => (self.borrow_keycode(keysym)?, false),
        };
        let shift = shifted
            .then(|| self.keymap.lookup(XK_SHIFT_L).map(|(code, _)| code))
            .flatten();
        let kind = if pressed {
            xproto::KEY_PRESS_EVENT
        } else {
            xproto::KEY_RELEASE_EVENT
        };
        if let (Some(shift), true) = (shift, pressed) {
            self.fake(xproto::KEY_PRESS_EVENT, shift, 0, 0)?;
        }
        self.fake(kind, keycode, 0, 0)?;
        if let (Some(shift), false) = (shift, pressed) {
            self.fake(xproto::KEY_RELEASE_EVENT, shift, 0, 0)?;
        }
        Ok(())
    }
}

impl Drop for X11Desktop {
    fn drop(&mut self) {
        // Give the borrowed keycode back to the layout.
        if let (Some(_), Some(keycode)) = (self.borrowed.get(), self.keymap.spare()) {
            let per = self.keymap.per_keycode.max(1);
            let _ = self.sent(self.conn.change_keyboard_mapping(
                1,
                keycode,
                per,
                &vec![0; usize::from(per)],
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keymap() -> Keymap {
        // keycode 8: a A, 9: 1 !, 10: Shift_L, 11: (empty), 12: ! (unshifted)
        Keymap {
            min_keycode: 8,
            per_keycode: 2,
            keysyms: vec![0x61, 0x41, 0x31, 0x21, XK_SHIFT_L, 0, 0, 0, 0x21, 0],
        }
    }

    #[test]
    fn keysyms_map_to_keycodes_preferring_the_unshifted_column() {
        let map = keymap();
        assert_eq!(map.lookup(0x61), Some((8, false)));
        assert_eq!(map.lookup(0x41), Some((8, true)));
        // `!` is shifted on 9 but unshifted on 12: no Shift needed.
        assert_eq!(map.lookup(0x21), Some((12, false)));
        assert_eq!(map.lookup(XK_SHIFT_L), Some((10, false)));
        assert_eq!(map.lookup(0x20ac), None); // € is not on this layout
        assert_eq!(map.lookup(0), None);
        assert_eq!(map.spare(), Some(11));
    }

    #[test]
    fn sub_step_motion_and_scroll_carry_over() {
        assert_eq!(whole_steps(0.4, 1.0), (0, 0.4));
        assert_eq!(whole_steps(-2.5, 1.0), (-2, -0.5));
        let (clicks, rest) = whole_steps(25.0, SCROLL_STEP);
        assert_eq!(clicks, 2);
        assert!((rest - 5.0).abs() < 1e-9);
    }

    #[test]
    fn evdev_buttons_map_to_x_buttons() {
        assert_eq!(x_button(0x110), Some(1));
        assert_eq!(x_button(0x111), Some(3));
        assert_eq!(x_button(0x112), Some(2));
        assert_eq!(x_button(0x200), None);
    }

    #[test]
    fn the_fallback_is_for_x11_sessions_without_a_portal_only() {
        assert!(use_fallback(Some("x11"), || false));
        assert!(!use_fallback(Some("x11"), || true));
        assert!(!use_fallback(Some("wayland"), || false));
        assert!(!use_fallback(None, || false));
    }
}
