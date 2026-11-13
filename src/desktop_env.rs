//! Which Linux desktop this is, and what that means for DeskLink: where it
//! shows in the panel, and how its lock screen is locked, recognised and
//! unlocked from the phone. The research behind the table is
//! `docs/DESKTOP-INTEGRATION.md` (workspace).
//!
//! The panel: GNOME has DeskLink's own extension; every other desktop shows
//! the StatusNotifierItem in `tray.rs` once it has a tray host, and
//! [`Desktop::tray_setup`] says how to get one.
//!
//! The lock screen: the desktops' own lock screens (gnome-shell,
//! kscreenlocker, cosmic-greeter, cinnamon-, mate-, xfce4-screensaver,
//! light-locker) unlock on logind's `Session.Unlock`. Standalone Wayland and
//! X11 lockers do not; swaylock and hyprlock instead unlock on `SIGUSR1`
//! (documented in swaylock(1), `handleUnlockSignal` in hyprlock), and the
//! rest cannot be unlocked from outside at all, which the phone is told.

use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use gtk::gio;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Desktop {
    Gnome,
    Kde,
    Cosmic,
    Xfce,
    Cinnamon,
    Mate,
    Budgie,
    Lxqt,
    Pantheon,
    Deepin,
    Sway,
    Hyprland,
    I3,
    /// Anything else: another compositor or window manager, or none named.
    Other,
}

/// The desktop from `XDG_CURRENT_DESKTOP` (colon-separated, any case:
/// `ubuntu:GNOME`, `X-Cinnamon`, `Budgie:GNOME`, empty under a bare WM).
/// The first name that is known wins, so `Budgie:GNOME` is Budgie.
pub(crate) fn detect(current_desktop: &str) -> Desktop {
    current_desktop
        .split(':')
        .map(|name| name.trim().to_ascii_lowercase())
        .find_map(|name| {
            Some(match name.as_str() {
                "gnome" | "gnome-classic" | "gnome-flashback" => Desktop::Gnome,
                "kde" => Desktop::Kde,
                "cosmic" => Desktop::Cosmic,
                "xfce" => Desktop::Xfce,
                "x-cinnamon" | "cinnamon" => Desktop::Cinnamon,
                "mate" => Desktop::Mate,
                "budgie" => Desktop::Budgie,
                "lxqt" => Desktop::Lxqt,
                "pantheon" => Desktop::Pantheon,
                "deepin" | "dde" => Desktop::Deepin,
                "sway" => Desktop::Sway,
                "hyprland" => Desktop::Hyprland,
                "i3" => Desktop::I3,
                _ => return None,
            })
        })
        .unwrap_or(Desktop::Other)
}

/// This session's desktop.
pub(crate) fn current() -> Desktop {
    detect(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
}

impl Desktop {
    /// How to get a StatusNotifier tray showing DeskLink here, for when none
    /// is (one or two sentences, plain words).
    pub(crate) fn tray_setup(self) -> &'static str {
        match self {
            Self::Gnome => {
                "GNOME shows DeskLink through its own top bar menu. Without it, the \
                 AppIndicator extension shows a tray icon."
            }
            Self::Kde => {
                "Plasma's System Tray shows DeskLink. If your panel has none, right-click the \
                 panel, choose Add Widgets and add System Tray."
            }
            Self::Cosmic => {
                "Add the Notifications Tray applet to the panel: Settings, Desktop, Panel, \
                 Configure panel applets."
            }
            Self::Xfce => {
                "Add the Status Tray Plugin to a panel (Xfce 4.16 or newer): right-click the \
                 panel, Panel, Add New Items."
            }
            Self::Cinnamon => {
                "Cinnamon shows DeskLink with the XApp Status applet. If it is missing, \
                 right-click the panel, choose Applets and add it."
            }
            Self::Mate => {
                "Add the Indicator Applet Complete (or the Notification Area) to the panel: \
                 right-click the panel, Add to Panel."
            }
            Self::Budgie => {
                "Add the System Tray applet (Budgie 10.8 or newer) in Budgie Desktop \
                 Settings, Panel."
            }
            Self::Lxqt => "Add the Status Notifier widget: right-click the panel, Manage Widgets.",
            Self::Pantheon => {
                "elementary OS has no tray by default. An indicator such as \
                 wingpanel-indicator-namarupa adds one; until then DeskLink uses notifications."
            }
            Self::Deepin => {
                "The dock's tray area shows DeskLink. Turn it on in the dock's settings."
            }
            Self::Sway => {
                "swaybar shows DeskLink when built with tray support. With Waybar, add the \
                 \"tray\" module."
            }
            Self::Hyprland => "Add the \"tray\" module to Waybar (or use another bar with a tray).",
            Self::I3 => {
                "i3bar shows only older tray icons. Run snixembed, or use a bar with a \
                 modern tray, such as Waybar or polybar with its tray module."
            }
            Self::Other => {
                "Your panel needs a modern system tray (Plasma, Xfce, MATE, Cinnamon, LXQt, \
                 COSMIC, Waybar and swaybar have one)."
            }
        }
    }

    /// A compositor or window manager with no lock screen of its own: logind's
    /// `Lock` locks it only if swayidle/hypridle/xss-lock was set up to.
    fn bare(self) -> bool {
        matches!(self, Self::Sway | Self::Hyprland | Self::I3 | Self::Other)
    }
}

// ---- lock screens ----------------------------------------------------------

/// How a standalone locker can be unlocked from outside (the desktops' own
/// lock screens use logind's `Session.Unlock`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnlockBy {
    /// It unlocks on `SIGUSR1` (and ignores logind).
    Signal,
    /// Nothing outside it can unlock it.
    Cannot,
}

/// Standalone lockers, by process name (`/proc/<pid>/comm`). Each runs only
/// while the screen is locked, so a running one also means "locked".
const LOCKERS: [(&str, UnlockBy); 8] = [
    ("swaylock", UnlockBy::Signal),
    ("hyprlock", UnlockBy::Signal),
    ("gtklock", UnlockBy::Cannot),
    ("waylock", UnlockBy::Cannot),
    ("i3lock", UnlockBy::Cannot),
    ("xsecurelock", UnlockBy::Cannot),
    ("slock", UnlockBy::Cannot),
    ("xtrlock", UnlockBy::Cannot),
];

/// What an unlock does here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Route {
    /// logind's `Session.Unlock` (the desktops' own lock screens).
    Logind,
    /// `SIGUSR1` to this locker process (and logind's Unlock, for anything
    /// else that listens, such as hypridle).
    Signal { pid: i32, name: String },
    /// This locker cannot be unlocked from the phone.
    Cannot(String),
}

/// The unlock decision from the lockers running for this user (`(pid, name)`):
/// a standalone locker decides; without one, the desktop's own lock screen
/// is asked through logind.
pub(crate) fn route(running: &[(i32, String)]) -> Route {
    for (pid, name) in running {
        match LOCKERS.iter().find(|(locker, _)| locker == name) {
            Some((_, UnlockBy::Signal)) => {
                return Route::Signal {
                    pid: *pid,
                    name: name.clone(),
                }
            }
            Some((_, UnlockBy::Cannot)) => return Route::Cannot(name.clone()),
            None => {}
        }
    }
    Route::Logind
}

/// The known lockers running as this user, from `/proc`. Inside a sandbox
/// only the sandbox's processes are visible: nothing is found and logind is
/// used, as before.
pub(crate) fn running_lockers() -> Vec<(i32, String)> {
    let uid = unsafe { libc::getuid() };
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid: i32 = entry.file_name().to_str()?.parse().ok()?;
            if entry.metadata().ok()?.uid() != uid {
                return None;
            }
            let comm = std::fs::read_to_string(entry.path().join("comm")).ok()?;
            let comm = comm.trim();
            LOCKERS
                .iter()
                .any(|(name, _)| *name == comm)
                .then(|| (pid, comm.to_string()))
        })
        .collect()
}

/// Why an unlock did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UnlockError {
    /// This lock screen cannot be unlocked from outside (its name).
    Unsupported(String),
    Failed(String),
}

/// Unlocks this user's session the way its lock screen listens. Returns the
/// route taken, so the caller can check it worked.
pub(crate) fn unlock() -> Result<Route, UnlockError> {
    let route = route(&running_lockers());
    match &route {
        Route::Cannot(name) => return Err(UnlockError::Unsupported(name.clone())),
        Route::Signal { pid, name } => {
            // hypridle/swayidle `unlock` hooks hear it too.
            let _ = crate::device_links::logind::unlock();
            if unsafe { libc::kill(*pid, libc::SIGUSR1) } != 0 {
                return Err(UnlockError::Failed(format!(
                    "{name}: {}",
                    std::io::Error::last_os_error()
                )));
            }
        }
        Route::Logind => crate::device_links::logind::unlock().map_err(UnlockError::Failed)?,
    }
    Ok(route)
}

/// The session-bus screensavers: each answers `GetActive` and `Lock`.
const SCREENSAVERS: [(&str, &str); 6] = [
    (
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
    ),
    ("org.freedesktop.ScreenSaver", "/ScreenSaver"),
    ("org.gnome.ScreenSaver", "/org/gnome/ScreenSaver"),
    ("org.cinnamon.ScreenSaver", "/org/cinnamon/ScreenSaver"),
    ("org.mate.ScreenSaver", "/org/mate/ScreenSaver"),
    ("org.xfce.ScreenSaver", "/org/xfce/ScreenSaver"),
];

/// Calls `method` on one screensaver; never starts one.
fn screensaver_call(
    bus: &gio::DBusConnection,
    (name, path): (&str, &str),
    method: &str,
) -> Option<gtk::glib::Variant> {
    bus.call_sync(
        Some(name),
        path,
        name,
        method,
        None,
        None,
        gio::DBusCallFlags::NO_AUTO_START,
        1_000,
        gio::Cancellable::NONE,
    )
    .ok()
}

/// Whether a session-bus screensaver says it is active. For lock screens
/// that do not keep logind's `LockedHint` (cinnamon-, mate-,
/// xfce4-screensaver, light-locker, cosmic-greeter).
fn screensaver_active() -> bool {
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return false;
    };
    SCREENSAVERS.iter().any(|screensaver| {
        screensaver_call(&bus, *screensaver, "GetActive")
            .and_then(|reply| reply.get::<(bool,)>())
            .is_some_and(|(active,)| active)
    })
}

/// Whether this session is locked: logind's `LockedHint`, a standalone
/// locker running, or the desktop's screensaver saying so.
pub(crate) fn locked() -> bool {
    crate::device_links::logind::locked() == Some(true)
        || !running_lockers().is_empty()
        || screensaver_active()
}

/// The lockers DeskLink may start itself on a bare compositor or window
/// manager where nothing answered logind's `Lock`, in order of preference.
fn locker_commands(desktop: Desktop, wayland: bool) -> &'static [&'static [&'static str]] {
    match desktop {
        Desktop::Hyprland => &[&["hyprlock"], &["swaylock", "-f"]],
        Desktop::Sway => &[&["swaylock", "-f"], &["hyprlock"]],
        Desktop::Other if wayland => &[&["swaylock", "-f"], &["hyprlock"], &["waylock"]],
        Desktop::I3 | Desktop::Other => &[&["i3lock"], &["xdg-screensaver", "lock"], &["slock"]],
        _ => &[],
    }
}

/// How long a lock screen gets to come up before DeskLink starts a locker.
const LOCK_CHECK_AFTER: Duration = Duration::from_millis(1_500);

/// Locks this session every way a desktop might listen: logind's `Lock`,
/// else the session-bus screensavers; and on a bare compositor that ignored
/// both (no swayidle/hypridle/xss-lock `lock` hook), a locker started here.
pub(crate) fn lock() {
    let mut asked = match crate::device_links::logind::lock() {
        Ok(()) => true,
        Err(error) => {
            debug_log!("[Desktop] logind could not lock the session: {error}");
            false
        }
    };
    if !asked {
        if let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
            asked = SCREENSAVERS
                .iter()
                .any(|screensaver| screensaver_call(&bus, *screensaver, "Lock").is_some());
        }
    }
    let desktop = current();
    if asked && !desktop.bare() {
        return;
    }
    let wayland = std::env::var_os("WAYLAND_DISPLAY").is_some();
    std::thread::spawn(move || {
        if asked {
            std::thread::sleep(LOCK_CHECK_AFTER);
            if locked() {
                return;
            }
        }
        for command in locker_commands(desktop, wayland) {
            if let Ok(mut child) = std::process::Command::new(command[0])
                .args(&command[1..])
                .spawn()
            {
                // Reaped here, so it never lingers as a zombie.
                let _ = child.wait();
                return;
            }
        }
        eprintln!("[DeskLink] Nothing locked this session: no lock screen answered and no locker is installed");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_desktop_is_recognised_from_its_session_name() {
        for (name, desktop) in [
            ("GNOME", Desktop::Gnome),
            ("ubuntu:GNOME", Desktop::Gnome),
            ("KDE", Desktop::Kde),
            ("COSMIC", Desktop::Cosmic),
            ("XFCE", Desktop::Xfce),
            ("X-Cinnamon", Desktop::Cinnamon),
            ("MATE", Desktop::Mate),
            ("Budgie:GNOME", Desktop::Budgie),
            ("LXQt", Desktop::Lxqt),
            ("Pantheon", Desktop::Pantheon),
            ("Deepin", Desktop::Deepin),
            ("sway", Desktop::Sway),
            ("Hyprland", Desktop::Hyprland),
            ("i3", Desktop::I3),
            ("niri", Desktop::Other),
            ("", Desktop::Other),
        ] {
            assert_eq!(detect(name), desktop, "{name}");
        }
    }

    #[test]
    fn every_desktop_has_its_own_tray_steps() {
        let all = [
            Desktop::Gnome,
            Desktop::Kde,
            Desktop::Cosmic,
            Desktop::Xfce,
            Desktop::Cinnamon,
            Desktop::Mate,
            Desktop::Budgie,
            Desktop::Lxqt,
            Desktop::Pantheon,
            Desktop::Deepin,
            Desktop::Sway,
            Desktop::Hyprland,
            Desktop::I3,
            Desktop::Other,
        ];
        let mut steps = std::collections::HashSet::new();
        for desktop in all {
            assert!(
                steps.insert(desktop.tray_setup()),
                "{desktop:?} has its own steps"
            );
        }
        assert!(Desktop::Kde.tray_setup().contains("System Tray"));
        assert!(Desktop::Cosmic.tray_setup().contains("Notifications Tray"));
        assert!(Desktop::Hyprland.tray_setup().contains("tray"));
    }

    #[test]
    fn the_lock_screen_decides_how_the_phone_unlocks() {
        let running = |names: &[&str]| -> Vec<(i32, String)> {
            names
                .iter()
                .enumerate()
                .map(|(pid, name)| (pid as i32 + 100, name.to_string()))
                .collect()
        };
        // The desktops' own lock screens: logind.
        assert_eq!(route(&running(&[])), Route::Logind);
        assert_eq!(route(&running(&["gnome-shell", "firefox"])), Route::Logind);
        // swaylock and hyprlock unlock on SIGUSR1, to that very process.
        assert_eq!(
            route(&running(&["swaylock"])),
            Route::Signal {
                pid: 100,
                name: "swaylock".into()
            }
        );
        assert_eq!(
            route(&running(&["waybar", "hyprlock"])),
            Route::Signal {
                pid: 101,
                name: "hyprlock".into()
            }
        );
        // The rest cannot be unlocked from outside: said, not tried.
        for locker in ["i3lock", "gtklock", "waylock", "xsecurelock", "slock"] {
            assert_eq!(
                route(&running(&[locker])),
                Route::Cannot(locker.into()),
                "{locker}"
            );
        }
        // A near name is not a locker.
        assert_eq!(route(&running(&["swaylock-helper"])), Route::Logind);
    }

    #[test]
    fn a_bare_compositor_gets_a_locker_and_a_desktop_keeps_its_own() {
        assert_eq!(locker_commands(Desktop::Hyprland, true)[0], ["hyprlock"]);
        assert_eq!(locker_commands(Desktop::Sway, true)[0], ["swaylock", "-f"]);
        assert_eq!(locker_commands(Desktop::I3, false)[0], ["i3lock"]);
        assert_eq!(locker_commands(Desktop::Other, true)[0], ["swaylock", "-f"]);
        assert_eq!(locker_commands(Desktop::Other, false)[0], ["i3lock"]);
        for desktop in [Desktop::Gnome, Desktop::Kde, Desktop::Cosmic, Desktop::Xfce] {
            assert!(locker_commands(desktop, true).is_empty(), "{desktop:?}");
            assert!(!desktop.bare());
        }
    }
}
