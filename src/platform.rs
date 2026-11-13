//! What this computer can do, found out once at start-up and explained in
//! plain words: Settings -> Advanced -> System check lists it, and rows that
//! cannot work here say why instead of failing quietly.
//!
//! DeskLink runs on any Linux desktop, so nothing here assumes GNOME. Each
//! feature goes through a freedesktop standard (StatusNotifierItem,
//! org.freedesktop.Notifications, xdg-desktop-portal, logind, XDG autostart);
//! GNOME and KDE extras are bonuses. [`evaluate`] is a pure function of a
//! [`Probe`] (the facts), so tests fake any desktop; [`Probe::real`] reads the
//! facts from this system and is the only part that touches it.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use gtk::gio;
use gtk::glib::{self, prelude::*};

/// How well a feature works here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    /// Works, with something missing that the detail names.
    Limited,
    /// Does not work here; the detail says why and what to install.
    Missing,
}

/// The package that provides something, by distribution family.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Packages {
    pub debian: &'static str,
    pub fedora: &'static str,
    pub arch: &'static str,
    pub opensuse: &'static str,
}

impl Packages {
    const fn same(name: &'static str) -> Self {
        Self {
            debian: name,
            fedora: name,
            arch: name,
            opensuse: name,
        }
    }

    /// One line per family, for the dialog and the docs.
    pub fn lines(&self) -> String {
        format!(
            "Debian/Ubuntu: {}\nFedora: {}\nArch: {}\nopenSUSE: {}",
            self.debian, self.fedora, self.arch, self.opensuse
        )
    }
}

/// The distribution family, for the install command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Debian,
    Fedora,
    Arch,
    Suse,
    Nix,
}

/// The family named by `/etc/os-release` text (`ID` first, then `ID_LIKE`).
pub fn distro_from_os_release(text: &str) -> Option<Distro> {
    let value = |key: &str| -> Vec<String> {
        text.lines()
            .find_map(|line| line.strip_prefix(key)?.strip_prefix('='))
            .map(|v| {
                v.trim_matches(|c| c == '"' || c == '\'')
                    .split_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect()
            })
            .unwrap_or_default()
    };
    value("ID")
        .into_iter()
        .chain(value("ID_LIKE"))
        .find_map(|id| {
            Some(match id.as_str() {
                "debian" | "ubuntu" | "linuxmint" | "pop" | "zorin" | "raspbian" | "neon"
                | "elementary" | "kali" => Distro::Debian,
                "fedora" | "rhel" | "centos" | "rocky" | "almalinux" | "nobara" => Distro::Fedora,
                "arch" | "manjaro" | "endeavouros" | "cachyos" | "garuda" | "arco" => Distro::Arch,
                "opensuse" | "opensuse-leap" | "opensuse-tumbleweed" | "suse" | "sles" => {
                    Distro::Suse
                }
                "nixos" => Distro::Nix,
                _ => return None,
            })
        })
}

/// This computer's family, from `/etc/os-release` (or `/usr/lib/os-release`).
pub fn distro() -> Option<Distro> {
    ["/etc/os-release", "/usr/lib/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| distro_from_os_release(&text))
}

impl Packages {
    /// What to type to install them on `distro`; every family's list when it
    /// is not known.
    pub fn command(&self, distro: Option<Distro>) -> String {
        match distro {
            Some(Distro::Debian) => format!("sudo apt install {}", self.debian),
            Some(Distro::Fedora) => format!("sudo dnf install {}", self.fedora),
            Some(Distro::Arch) => format!("sudo pacman -S {}", self.arch),
            Some(Distro::Suse) => format!("sudo zypper install {}", self.opensuse),
            // Nothing is installed imperatively on NixOS.
            Some(Distro::Nix) => "add the GStreamer plugins to your NixOS configuration \
                                  (the DeskLink flake wraps them in already)"
                .to_string(),
            None => self.lines(),
        }
    }
}

/// One row of the system check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// What `reason` looks a row up by.
    pub id: &'static str,
    pub title: &'static str,
    pub level: Level,
    /// Plain words for the person.
    pub detail: String,
    pub install: Option<Packages>,
    /// The technical names behind it (components, services, variables), for
    /// the copied report, `desklink --check` and the log only; empty if none.
    pub tech: String,
}

impl Check {
    fn tech(mut self, tech: impl Into<String>) -> Self {
        self.tech = tech.into();
        self
    }
}

/// The start of the share row's words when the desktop's own screen sharing
/// (the portal) is used; onboarding tells that case apart by it.
pub const SHARE_THROUGH_DESKTOP: &str = "Through your desktop's own screen sharing.";

/// The facts [`evaluate`] judges. `Default` is an empty machine.
#[derive(Debug, Clone, Default)]
pub struct Probe {
    /// `XDG_CURRENT_DESKTOP`, colon-separated and possibly empty (a bare
    /// window manager sets none).
    pub desktop: String,
    /// `XDG_SESSION_TYPE`.
    pub session_type: Option<String>,
    pub wayland_display: bool,
    pub x11_display: bool,
    pub runtime_dir: bool,
    pub flatpak: bool,
    pub snap: bool,
    pub appimage: bool,
    pub session_bus: bool,
    pub system_bus: bool,
    /// Names owned or activatable on the session bus.
    pub session_names: BTreeSet<String>,
    /// Names owned or activatable on the system bus.
    pub system_names: BTreeSet<String>,
    /// A StatusNotifier host is registered with the watcher.
    pub tray_host: bool,
    /// Portal interfaces that answered (`ScreenCast`, `RemoteDesktop`, ...).
    pub portals: BTreeSet<String>,
    /// ScreenCast `AvailableSourceTypes` (bit 4 is a virtual monitor).
    pub screencast_sources: Option<u32>,
    /// InputCapture `SupportedCapabilities` (3 is keyboard and pointer).
    pub input_capture: Option<u32>,
    /// GStreamer elements found, from the list [`WATCHED_ELEMENTS`].
    pub elements: BTreeSet<String>,
    pub pipewire_socket: bool,
    pub pulse_socket: bool,
    /// Known terminal programs found on `PATH`.
    pub terminals: BTreeSet<String>,
    pub config_writable: bool,
    pub toolkit: String,
}

/// The elements the probe asks GStreamer about.
const WATCHED_ELEMENTS: [&str; 17] = [
    "webrtcbin",
    "dtlsdec",
    "dtlssrtpenc",
    "nicesrc",
    "opusenc",
    "opusdec",
    "vp8dec",
    "vp8enc",
    "avdec_h264",
    "openh264dec",
    "vah264dec",
    "x264enc",
    "openh264enc",
    "vah264enc",
    "pipewiresrc",
    "pipewiresink",
    "ximagesrc",
];

const PORTAL_INTERFACES: [&str; 5] = [
    "ScreenCast",
    "RemoteDesktop",
    "InputCapture",
    "Background",
    "Notification",
];

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";
const LOGIND: &str = "org.freedesktop.login1";
const PORTAL: &str = "org.freedesktop.portal.Desktop";

// ---- the desktop families --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Gnome,
    Kde,
    Cosmic,
    /// sway, Hyprland, river, labwc, Wayfire, niri: wlroots-style compositors.
    Wlroots,
    /// Xfce, MATE, Cinnamon, Budgie, LXQt, Pantheon, Unity: GTK/Qt desktops
    /// with their own session manager.
    Classic,
    /// i3, openbox, bspwm, dwm and the like: nothing sets a desktop name and
    /// nothing runs XDG autostart.
    BareWm,
}

/// The desktop family from `XDG_CURRENT_DESKTOP` (colon-separated, any case:
/// `ubuntu:GNOME`, `X-Cinnamon`, `KDE`, `sway`, `COSMIC`, empty under i3).
fn family(current_desktop: &str) -> Family {
    let names: Vec<String> = current_desktop
        .split(':')
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    let has = |wanted: &[&str]| names.iter().any(|name| wanted.contains(&name.as_str()));
    // Budgie and GNOME Flashback list `GNOME` too (`Budgie:GNOME`) but have
    // their own panel, tray and lock screen, and no GNOME Shell.
    if has(&["budgie", "gnome-flashback"]) {
        Family::Classic
    } else if has(&["gnome", "gnome-classic"]) {
        Family::Gnome
    } else if has(&["kde"]) {
        Family::Kde
    } else if has(&["cosmic"]) {
        Family::Cosmic
    } else if has(&[
        "sway", "hyprland", "river", "labwc", "wayfire", "niri", "wlroots", "miracle",
    ]) {
        Family::Wlroots
    } else if has(&[
        "xfce",
        "mate",
        "x-cinnamon",
        "cinnamon",
        "budgie",
        "lxqt",
        "lxde",
        "pantheon",
        "unity",
        "deepin",
        "ukui",
        "enlightenment",
    ]) {
        Family::Classic
    } else {
        Family::BareWm
    }
}

/// Whether GNOME Shell is the desktop: its extensions, search providers and
/// top bar exist nowhere else (not on Budgie or GNOME Flashback, which also
/// list `GNOME`).
pub fn is_gnome_shell(current_desktop: &str) -> bool {
    family(current_desktop) == Family::Gnome
}

/// Whether this is an X11 session, the one rule for the probe and for the
/// share itself (`x11_desktop::wanted`). `XDG_SESSION_TYPE` decides when it
/// says x11 or wayland; where it is unset or `tty` (startx from a console
/// login, a bare window manager) a `DISPLAY` without a `WAYLAND_DISPLAY` is
/// X11.
pub fn x11_session(session_type: Option<&str>, wayland_display: bool, x11_display: bool) -> bool {
    match session_type.map(str::to_ascii_lowercase).as_deref() {
        Some("x11") => true,
        Some("wayland") => false,
        _ => x11_display && !wayland_display,
    }
}

/// A short name for the desktop for the first row.
fn desktop_name(current_desktop: &str) -> String {
    let name = current_desktop
        .split(':')
        .rfind(|part| !part.trim().is_empty())
        .unwrap_or("");
    if name.is_empty() {
        "an unnamed desktop or window manager".to_string()
    } else {
        name.trim().to_string()
    }
}

/// The portal backend this desktop family needs, by package name.
fn portal_backend(family: Family) -> &'static str {
    match family {
        Family::Gnome => "xdg-desktop-portal-gnome",
        Family::Kde => "xdg-desktop-portal-kde",
        Family::Cosmic => "xdg-desktop-portal-cosmic",
        Family::Wlroots => "xdg-desktop-portal-wlr (or xdg-desktop-portal-hyprland on Hyprland)",
        Family::Classic | Family::BareWm => "xdg-desktop-portal-gtk",
    }
}

// ---- the judgement ----------------------------------------------------------

fn check(
    id: &'static str,
    title: &'static str,
    level: Level,
    detail: impl Into<String>,
    install: Option<Packages>,
) -> Check {
    Check {
        id,
        title,
        level,
        detail: detail.into(),
        install,
        tech: String::new(),
    }
}

const GST_BAD: Packages = Packages {
    debian: "gstreamer1.0-plugins-bad gstreamer1.0-nice",
    fedora: "gstreamer1-plugins-bad-free libnice-gstreamer1",
    arch: "gst-plugins-bad libnice",
    opensuse: "gstreamer-plugins-bad gstreamer-libnice",
};
const GST_BASE: Packages = Packages {
    debian: "gstreamer1.0-plugins-base",
    fedora: "gstreamer1-plugins-base",
    arch: "gst-plugins-base",
    opensuse: "gstreamer-plugins-base",
};
const GST_GOOD: Packages = Packages {
    debian: "gstreamer1.0-plugins-good gstreamer1.0-x",
    fedora: "gstreamer1-plugins-good",
    arch: "gst-plugins-good",
    opensuse: "gstreamer-plugins-good",
};
const GST_VIDEO: Packages = Packages {
    debian: "gstreamer1.0-plugins-good gstreamer1.0-libav",
    fedora: "gstreamer1-plugins-good gstreamer1-plugin-libav",
    arch: "gst-plugins-good gst-libav",
    opensuse: "gstreamer-plugins-good gstreamer-plugins-libav",
};
const GST_PIPEWIRE: Packages = Packages {
    debian: "gstreamer1.0-pipewire",
    fedora: "pipewire-gstreamer",
    arch: "gst-plugin-pipewire",
    opensuse: "gstreamer-plugin-pipewire",
};
const PIPEWIRE: Packages = Packages {
    debian: "pipewire pipewire-pulse wireplumber",
    fedora: "pipewire pipewire-pulseaudio wireplumber",
    arch: "pipewire pipewire-pulse wireplumber",
    opensuse: "pipewire pipewire-pulseaudio wireplumber",
};
const APPINDICATOR: Packages = Packages::same("gnome-shell-extension-appindicator");
const NOTIFIER: Packages = Packages::same("dunst");

/// Judges `probe`: one row per feature that depends on the system, in the
/// order they are listed.
pub fn evaluate(probe: &Probe) -> Vec<Check> {
    let family = family(&probe.desktop);
    let has = |name: &str| probe.elements.contains(name);
    let any = |names: &[&str]| names.iter().any(|name| has(name));
    let wayland = probe.wayland_display
        && probe
            .session_type
            .as_deref()
            .is_none_or(|kind| !kind.eq_ignore_ascii_case("x11"));
    let portal = probe.session_names.contains(PORTAL);
    let has_portal = |name: &str| portal && probe.portals.contains(name);
    let mut rows = Vec::new();

    // The session.
    let session = if wayland {
        "Wayland"
    } else if probe.x11_display {
        "X11"
    } else {
        "no display"
    };
    let packaging = if probe.flatpak {
        ", as a Flatpak"
    } else if probe.snap {
        ", as a Snap"
    } else if probe.appimage {
        ", as an AppImage"
    } else {
        ""
    };
    rows.push(
        check(
            "session",
            "Desktop session",
            if wayland || probe.x11_display {
                Level::Ok
            } else {
                Level::Missing
            },
            format!("{} on {session}{packaging}.", desktop_name(&probe.desktop)),
            None,
        )
        .tech(probe.toolkit.clone()),
    );

    // The bus.
    rows.push(if probe.session_bus {
        check("bus", "Desktop session services", Level::Ok, "Found.", None)
    } else {
        check(
            "bus",
            "Desktop session services",
            Level::Missing,
            "DeskLink was started outside your desktop session, so the tray, notifications, \
             screen sharing and locking cannot work. Start it from your desktop's app menu.",
            None,
        )
        .tech("No D-Bus session bus; outside a session, run `dbus-run-session desklink`.")
    });

    // The link itself.
    let missing: Vec<&str> = crate::device_links::webrtc::REQUIRED_ELEMENTS
        .iter()
        .copied()
        .filter(|name| !has(name))
        .collect();
    rows.push(if missing.is_empty() {
        check(
            "link",
            "Phone link",
            Level::Ok,
            "Everything DeskLink needs to talk to phones is installed.",
            None,
        )
    } else {
        check(
            "link",
            "Phone link",
            Level::Missing,
            "Phones cannot connect: parts DeskLink needs to talk to them are not installed.",
            Some(GST_BAD),
        )
        .tech(format!(
            "GStreamer WebRTC elements missing: {}.",
            missing.join(", ")
        ))
    });

    // The tray.
    rows.push(if probe.tray_host {
        check(
            "tray",
            "System tray",
            Level::Ok,
            "A tray is showing DeskLink. Closing the window keeps DeskLink running there.",
            None,
        )
    } else {
        let (detail, install) = match family {
            Family::Gnome => (
                "GNOME shows no tray without an extension. Closing the window keeps DeskLink \
                 running, and you reopen it from the Activities search or its launcher. \
                 Installing and enabling the AppIndicator extension adds a tray icon.",
                Some(APPINDICATOR),
            ),
            _ => (
                "No tray is showing DeskLink. DeskLink still works: closing the window keeps \
                 it running, and you reopen it from the launcher or by running `desklink` \
                 again.",
                None,
            ),
        };
        // The desktop's own step to get a tray (`desktop_env`).
        let detail = match family {
            Family::Gnome => detail.to_string(),
            _ => format!(
                "{} {detail}",
                crate::desktop_env::detect(&probe.desktop).tray_setup()
            ),
        };
        check("tray", "System tray", Level::Limited, detail, install)
            .tech("No StatusNotifierItem host registered with org.kde.StatusNotifierWatcher.")
    });

    // Notifications.
    rows.push(
        if probe.session_names.contains(NOTIFICATIONS) || has_portal("Notification") {
            check(
                "notifications",
                "Notifications",
                Level::Ok,
                "A notification service is running.",
                None,
            )
        } else {
            check(
                "notifications",
                "Notifications",
                Level::Limited,
                "Nothing on this desktop shows notifications, so DeskLink cannot show the \
                 phone's notifications or alerts here. Install a notification app (dunst, \
                 mako or swaync, or your desktop's own).",
                Some(NOTIFIER),
            )
            .tech("No org.freedesktop.Notifications server and no Notification portal.")
        },
    );

    // Sharing and controlling this computer.
    // Desktop to phone video is VP8 (an H.264 encoder is only an experiment).
    let encoder = has("vp8enc");
    let portal_complete = has_portal("ScreenCast") && has_portal("RemoteDesktop");
    let x11_direct = !wayland
        && x11_session(
            probe.session_type.as_deref(),
            probe.wayland_display,
            probe.x11_display,
        )
        && !portal_complete;
    rows.push(if !probe.session_bus && !x11_direct {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            "Needs DeskLink to run inside your desktop session (see above).",
            None,
        )
    } else if x11_direct {
        // Without a portal DeskLink captures and types through X11 itself.
        if has("ximagesrc") && encoder {
            check(
                "share",
                "Share this screen and let the phone control it",
                Level::Ok,
                "DeskLink shares this screen itself, as this desktop has no screen sharing \
                 of its own. You approve each phone at this computer.",
                None,
            )
            .tech("X11 directly: ximagesrc and XTest (no ScreenCast/RemoteDesktop portal).")
        } else if probe.flatpak {
            check(
                "share",
                "Share this screen and let the phone control it",
                Level::Missing,
                "This desktop has no screen sharing of its own, and the Flatpak version of \
                 DeskLink cannot share the screen without it. Install DeskLink from your \
                 distribution's packages instead.",
                None,
            )
            .tech("No ScreenCast portal on X11, and the Flatpak runtime has no ximagesrc.")
        } else {
            check(
                "share",
                "Share this screen and let the phone control it",
                Level::Missing,
                "Parts DeskLink needs to share this screen are not installed.",
                Some(GST_GOOD),
            )
            .tech("Needs GStreamer ximagesrc and vp8enc (gst-plugins-good).")
        }
    } else if !has_portal("ScreenCast") {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            format!(
                "This desktop's screen sharing service is not installed or not running. \
                 Install {}, then log out and back in.",
                portal_backend(family)
            ),
            None,
        )
        .tech("No xdg-desktop-portal ScreenCast interface answered.")
    } else if !has_portal("RemoteDesktop") {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            format!(
                "This desktop can show its screen to apps but cannot let them control it, and \
                 DeskLink needs both. GNOME and KDE Plasma can; {} cannot on every desktop.",
                portal_backend(family)
            ),
            None,
        )
        .tech("The portal has ScreenCast but no RemoteDesktop interface.")
    } else if !any(&["pipewiresrc"]) {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            "A part DeskLink needs to share this screen is not installed.",
            Some(GST_PIPEWIRE),
        )
        .tech("Needs GStreamer pipewiresrc (the PipeWire plugin).")
    } else if !encoder {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            "The video encoder DeskLink sends this screen with is not installed.",
            Some(GST_GOOD),
        )
        .tech("Needs GStreamer vp8enc (gst-plugins-good).")
    } else {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Ok,
            format!("{SHARE_THROUGH_DESKTOP} You approve each session at this computer."),
            None,
        )
        .tech("xdg-desktop-portal ScreenCast and RemoteDesktop, PipeWire video.")
    });

    // The phone's screen shown here.
    rows.push(
        if any(&["vp8dec", "avdec_h264", "openh264dec", "vah264dec"]) {
            check(
                "view",
                "Phone screen on this computer",
                Level::Ok,
                "The phone's screen can be shown here.",
                None,
            )
        } else {
            check(
                "view",
                "Phone screen on this computer",
                Level::Missing,
                "The phone's screen cannot be shown: the video decoders are not installed.",
                Some(GST_VIDEO),
            )
            .tech("No GStreamer VP8 or H.264 decoder (vp8dec, avdec_h264, openh264dec, vah264dec).")
        },
    );

    // Sound.
    rows.push(if !any(&["pipewiresink", "pipewiresrc"]) {
        check(
            "sound",
            "Sound with the phone",
            Level::Missing,
            "A sound part DeskLink needs is not installed.",
            Some(GST_PIPEWIRE),
        )
        .tech("Needs GStreamer pipewiresrc and pipewiresink.")
    } else if !has("opusenc") || !has("opusdec") {
        check(
            "sound",
            "Sound with the phone",
            Level::Missing,
            "The sound codec DeskLink uses is not installed.",
            Some(GST_BASE),
        )
        .tech("Needs GStreamer opusenc and opusdec (gst-plugins-base).")
    } else if !probe.pipewire_socket && probe.flatpak {
        check(
            "sound",
            "Sound with the phone",
            Level::Missing,
            "This Flatpak is not allowed to use the sound system. Allow it with \
             `flatpak override --user --filesystem=xdg-run/pipewire-0 derx06.desklink.com` and \
             restart DeskLink.",
            None,
        )
        .tech("No $XDG_RUNTIME_DIR/pipewire-0 in the sandbox.")
    } else if !probe.pipewire_socket {
        let pulse = if probe.pulse_socket {
            "This computer's sound system is the older PulseAudio. "
        } else {
            "No sound server is running. "
        };
        check(
            "sound",
            "Sound with the phone",
            Level::Missing,
            format!("{pulse}Sound with the phone needs PipeWire, its successor (other apps keep working)."),
            Some(PIPEWIRE),
        )
        .tech("No $XDG_RUNTIME_DIR/pipewire-0 socket.")
    } else {
        check(
            "sound",
            "Sound with the phone",
            Level::Ok,
            "The sound system is ready.",
            None,
        )
    });

    // A second display.
    rows.push(
        if wayland
            && has_portal("ScreenCast")
            && probe.screencast_sources.is_some_and(|s| s & 4 != 0)
            // Hyprland's VIRTUAL source shares a workspace, not a new monitor,
            // and Extend places the monitor through Mutter: GNOME and KDE only.
            && matches!(family, Family::Gnome | Family::Kde)
        {
            check(
                "extend",
                "Phone as a second display (Extend)",
                Level::Ok,
                "This desktop can add a second screen for the phone.",
                None,
            )
        } else {
            check(
                "extend",
                "Phone as a second display (Extend)",
                Level::Missing,
                if wayland {
                    "This desktop cannot add a second screen for DeskLink. GNOME and KDE Plasma \
                     can."
                } else {
                    "Needs a Wayland session of GNOME or KDE Plasma (chosen when you log in). X11 \
                     sessions cannot add a second screen."
                },
                None,
            )
            .tech("Needs the ScreenCast portal's virtual source (AvailableSourceTypes bit 4) on Mutter or KWin.")
        },
    );

    // Glide.
    rows.push(
        if wayland && has_portal("InputCapture") && probe.input_capture.is_some_and(|c| c & 3 == 3)
        {
            check(
                "glide",
                "Move the pointer to the phone (Glide)",
                Level::Ok,
                "This desktop can hand the mouse and keyboard to the phone.",
                None,
            )
        } else {
            check(
                "glide",
                "Move the pointer to the phone (Glide)",
                Level::Missing,
                "This desktop cannot hand the mouse and keyboard to another device. GNOME 45 \
                 or later and KDE Plasma 6.1 or later can, in a Wayland session.",
                None,
            )
            .tech("Needs the InputCapture portal with keyboard and pointer (SupportedCapabilities 3).")
        },
    );

    // Lock, unlock, resume.
    rows.push(if probe.system_names.contains(LOGIND) {
        check(
            "lock",
            "Lock and unlock from the phone",
            Level::Ok,
            "Your system's session service is running.",
            None,
        )
    } else {
        check(
            "lock",
            "Lock and unlock from the phone",
            Level::Limited,
            "Your system has no session service (logind or elogind). Locking uses the \
             desktop's own screen saver instead; unlocking with the phone and noticing when the \
             computer wakes need one.",
            None,
        )
        .tech("org.freedesktop.login1 is not on the system bus.")
    });

    // Unlocking with the phone: only where the lock screen is known to obey
    // logind's Unlock signal. Others may ignore it, so it is not promised.
    rows.push(match family {
        Family::Gnome | Family::Kde => check(
            "unlock",
            "Unlock with the phone",
            Level::Ok,
            "This desktop's lock screen accepts unlocking from the phone.",
            None,
        ),
        _ => check(
            "unlock",
            "Unlock with the phone",
            Level::Limited,
            "Only GNOME and KDE Plasma are known to accept an unlock request; this lock \
             screen may ignore it, in which case the phone will say the computer stayed \
             locked.",
            None,
        ),
    });

    // Start at login.
    rows.push(match family {
        Family::BareWm => check(
            "autostart",
            "Start at login",
            Level::Limited,
            "This session does not start apps at login by itself. Add `exec desklink \
             --gapplication-service` to your window manager's startup (i3 and sway config, \
             Hyprland `exec-once`, openbox autostart), or use dex.",
            None,
        ),
        Family::Wlroots => check(
            "autostart",
            "Start at login",
            Level::Limited,
            "Desktops of this kind do not start apps at login by themselves. Add \
             `exec desklink --gapplication-service` to the compositor's config (sway `exec`, \
             Hyprland `exec-once`), or run dex.",
            None,
        ),
        _ => check(
            "autostart",
            "Start at login",
            Level::Ok,
            "DeskLink can start when you log in.",
            None,
        ),
    });

    // Terminals.
    rows.push(if probe.flatpak || !probe.terminals.is_empty() {
        check(
            "terminal",
            "Shared terminal windows here",
            Level::Ok,
            if probe.flatpak {
                "Opened on the host when needed."
            } else {
                "A terminal program was found."
            },
            None,
        )
    } else {
        check(
            "terminal",
            "Shared terminal windows here",
            Level::Limited,
            "No terminal program was found. A phone can still use shared terminals, but \
             they will not open a window on this computer. Set $TERMINAL or install one \
             (xterm, xfce4-terminal, konsole, gnome-terminal, kitty, foot, alacritty).",
            None,
        )
    });

    // Where settings live.
    rows.push(if probe.config_writable {
        check(
            "storage",
            "Saving settings and pairings",
            Level::Ok,
            "The configuration folder is writable.",
            None,
        )
    } else {
        check(
            "storage",
            "Saving settings and pairings",
            Level::Missing,
            "DeskLink cannot write to its configuration folder (a read-only or full home \
             directory?), so pairings and settings will not be kept.",
            None,
        )
    });
    if !probe.runtime_dir {
        rows.push(
            check(
                "runtime",
                "Runtime folder",
                Level::Limited,
                "This session has no private folder for running apps, so shared terminals and \
             media art are off. A normal login sets one up.",
                None,
            )
            .tech("XDG_RUNTIME_DIR is not set."),
        );
    }
    rows
}

/// The report as plain text (the `--check` output and Copy Report), with the
/// install command for `distro`.
pub fn as_text(rows: &[Check], distro: Option<Distro>) -> String {
    let mut text = format!("DeskLink {}\n", crate::config::VERSION);
    for row in rows {
        let mark = match row.level {
            Level::Ok => "ok     ",
            Level::Limited => "limited",
            Level::Missing => "missing",
        };
        text.push_str(&format!("[{mark}] {}: {}\n", row.title, row.detail));
        if !row.tech.is_empty() {
            text.push_str(&format!("           ({})\n", row.tech));
        }
        if let Some(packages) = row.install.filter(|_| row.level != Level::Ok) {
            for line in packages.command(distro).lines() {
                text.push_str(&format!("           {line}\n"));
            }
        }
    }
    text
}

/// `desklink --check`: probes without opening a window, prints the report and
/// returns the exit status (1 when the phone link itself cannot work).
pub fn run_check_cli() -> i32 {
    let rows = evaluate(&Probe::real());
    use std::io::Write;
    // A closed pipe (`desklink --check | head`) is not a crash.
    let _ = std::io::stdout().write_all(as_text(&rows, distro()).as_bytes());
    i32::from(
        rows.iter()
            .any(|row| row.id == "link" && row.level == Level::Missing),
    )
}

/// The check with `id`, if the system has been probed.
fn find(id: &str) -> Option<Check> {
    report()?.iter().find(|check| check.id == id).cloned()
}

/// Why `id` cannot work here, in plain words, or `None` when it works (or the
/// probe has not finished: nothing is hidden on a guess).
pub fn reason(id: &str) -> Option<String> {
    find(id)
        .filter(|check| check.level == Level::Missing)
        .map(|check| check.detail)
}

/// Whether `id` works here; `true` until the probe has said otherwise.
pub fn works(id: &str) -> bool {
    find(id).is_none_or(|check| check.level == Level::Ok)
}

/// Any note worth showing beside a feature that works with a limit.
pub fn note(id: &str) -> Option<String> {
    find(id)
        .filter(|check| check.level != Level::Ok)
        .map(|check| check.detail)
}

// ---- the real system --------------------------------------------------------

static REPORT: RwLock<Option<Arc<Vec<Check>>>> = RwLock::new(None);
/// Counts finished probes, so a dialog can tell its own refresh is done.
static GENERATION: AtomicU64 = AtomicU64::new(0);
static PROBING: AtomicBool = AtomicBool::new(false);

/// The latest probe; `None` until the first has finished.
pub fn report() -> Option<Arc<Vec<Check>>> {
    REPORT.read().ok()?.clone()
}

/// How many probes have finished.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Acquire)
}

fn store(rows: Vec<Check>) {
    if let Ok(mut report) = REPORT.write() {
        *report = Some(Arc::new(rows));
    }
    GENERATION.fetch_add(1, Ordering::AcqRel);
}

/// Replaces the report (tests fake a machine this way). Hold
/// [`TEST_REPORT`] while it is set, and call [`clear_report_for_tests`] after.
#[cfg(test)]
pub fn set_report_for_tests(rows: Vec<Check>) {
    store(rows);
}

#[cfg(test)]
pub fn clear_report_for_tests() {
    if let Ok(mut report) = REPORT.write() {
        *report = None;
    }
}

/// Serialises the tests that fake the report.
#[cfg(test)]
pub static TEST_REPORT: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Probes again on a background thread; one at a time (a second request while
/// one runs is answered by that one). The probe talks to D-Bus and GStreamer's
/// plugin registry, which can take seconds on a first run.
pub fn refresh() {
    spawn_probe(false, std::time::Duration::ZERO);
}

/// The first probe at start-up, and another about five seconds later: on
/// sessions without systemd, PipeWire, the tray host and the portal may
/// still be starting when DeskLink is.
pub fn start() {
    spawn_probe(true, std::time::Duration::ZERO);
    spawn_probe(false, std::time::Duration::from_secs(5));
}

fn spawn_probe(log: bool, after: std::time::Duration) {
    let _ = std::thread::Builder::new()
        .name("desklink-platform".into())
        .spawn(move || {
            std::thread::sleep(after);
            if PROBING.swap(true, Ordering::AcqRel) {
                return;
            }
            let rows = evaluate(&Probe::real());
            if log {
                for row in rows.iter().filter(|row| row.level == Level::Missing) {
                    eprintln!("[DeskLink] {}: {} {}", row.title, row.detail, row.tech);
                }
            }
            store(rows);
            PROBING.store(false, Ordering::Release);
        });
}

/// The row ids whose loss is shown as a banner that stays until the person
/// dismisses it (everything else is a Settings note): nothing connects, or
/// nothing can be saved.
const STICKY: [&str; 2] = ["link", "storage"];

/// The standing problem to keep in front of the person, if any: its row id
/// and the words (with the install command for this distribution).
pub fn sticky_notice() -> Option<(&'static str, String)> {
    sticky_from(&report()?, distro())
}

fn sticky_from(rows: &[Check], distro: Option<Distro>) -> Option<(&'static str, String)> {
    STICKY.iter().find_map(|id| {
        let row = rows
            .iter()
            .find(|row| row.id == *id && row.level == Level::Missing)?;
        let mut words = row.detail.clone();
        if let Some(packages) = row.install {
            words.push_str(&format!(" Install: {}", packages.command(distro)));
        }
        Some((*id, words))
    })
}

/// Whether a D-Bus error name is a definite "no such thing" (the object
/// answered and does not have it), as opposed to a timeout or a service that
/// is still starting.
fn definite_negative(remote_error: &str) -> bool {
    matches!(
        remote_error,
        "org.freedesktop.DBus.Error.UnknownMethod"
            | "org.freedesktop.DBus.Error.UnknownInterface"
            | "org.freedesktop.DBus.Error.UnknownProperty"
            | "org.freedesktop.DBus.Error.UnknownObject"
            | "org.freedesktop.DBus.Error.InvalidArgs"
    )
}

/// Whether a failed call is a definite "not there" (false: a timeout, no
/// reply, or a service that is still starting).
fn definite(error: &glib::Error) -> bool {
    gio::DBusError::remote_error(error).is_some_and(|name| definite_negative(&name))
}

/// The names to believe present on a bus. `listed` and `activatable` are the
/// answers to ListNames and ListActivatableNames (`None` when the call
/// failed). A name missing from both complete lists is absent; if either list
/// is missing, a watched name not seen is assumed present (fail open: a slow
/// bus must not switch features off).
fn settle_names(
    listed: Option<BTreeSet<String>>,
    activatable: Option<BTreeSet<String>>,
    watched: &[&str],
) -> BTreeSet<String> {
    let complete = listed.is_some() && activatable.is_some();
    let mut all: BTreeSet<String> = listed.into_iter().chain(activatable).flatten().collect();
    if !complete {
        all.extend(watched.iter().map(|name| name.to_string()));
    }
    all
}

fn names(bus: &gio::DBusConnection, method: &str) -> Option<BTreeSet<String>> {
    bus.call_sync(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
        method,
        None,
        None,
        gio::DBusCallFlags::NONE,
        2_000,
        gio::Cancellable::NONE,
    )
    .ok()
    .and_then(|reply| reply.child_value(0).get::<Vec<String>>())
    .map(|names| names.into_iter().collect())
}

fn bus_names(bus: &gio::DBusConnection, watched: &[&str]) -> BTreeSet<String> {
    settle_names(
        names(bus, "ListNames"),
        names(bus, "ListActivatableNames"),
        watched,
    )
}

/// A portal property: `Ok(value)`, `Err(true)` when the portal definitely has
/// no such interface, `Err(false)` when it could not say in time.
fn portal_property(
    bus: &gio::DBusConnection,
    interface: &str,
    name: &str,
) -> Result<glib::Variant, bool> {
    bus.call_sync(
        Some(PORTAL),
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.DBus.Properties",
        "Get",
        Some(&(format!("org.freedesktop.portal.{interface}"), name).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        2_000,
        gio::Cancellable::NONE,
    )
    .map_err(|error| definite(&error))
    .and_then(|reply| reply.child_value(0).as_variant().ok_or(false))
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn socket(path: std::path::PathBuf) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.metadata()
        .is_ok_and(|meta| meta.file_type().is_socket())
}

/// Whether asking for the session bus can work without starting anything:
/// an address in the environment, or the socket systemd and elogind make.
/// Without either, GDBus would run `dbus-launch`, which a check must not do.
fn session_bus_reachable(address_set: bool, runtime_bus_socket: bool) -> bool {
    address_set || runtime_bus_socket
}

impl Probe {
    /// Reads the facts from this system. Never fails. A question that cannot
    /// be answered in time (a busy bus, a portal still starting) is answered
    /// in favour of the feature; only a definite "no" switches one off.
    pub fn real() -> Self {
        let mut probe = Probe {
            desktop: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default(),
            session_type: std::env::var("XDG_SESSION_TYPE").ok(),
            wayland_display: env_set("WAYLAND_DISPLAY"),
            x11_display: env_set("DISPLAY"),
            runtime_dir: env_set("XDG_RUNTIME_DIR"),
            flatpak: crate::device_links::pty::host::in_flatpak(),
            snap: env_set("SNAP"),
            appimage: env_set("APPIMAGE"),
            // The linked libraries' versions, readable before GTK is
            // initialised (`--check` runs without a display).
            toolkit: unsafe {
                format!(
                    "GTK {}.{}, libadwaita {}.{}.",
                    gtk::ffi::gtk_get_major_version(),
                    gtk::ffi::gtk_get_minor_version(),
                    adw::ffi::adw_get_major_version(),
                    adw::ffi::adw_get_minor_version()
                )
            },
            ..Probe::default()
        };
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
        let reachable = session_bus_reachable(
            env_set("DBUS_SESSION_BUS_ADDRESS"),
            runtime.as_ref().is_some_and(|dir| socket(dir.join("bus"))),
        );
        let session = reachable
            .then(|| gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok())
            .flatten();
        if let Some(bus) = session {
            probe.session_bus = true;
            probe.session_names = bus_names(&bus, &[PORTAL, NOTIFICATIONS, WATCHER]);
            // A watcher that cannot answer in time counts as hosted.
            probe.tray_host = probe.session_names.contains(WATCHER)
                && bus
                    .call_sync(
                        Some(WATCHER),
                        "/StatusNotifierWatcher",
                        "org.freedesktop.DBus.Properties",
                        "Get",
                        Some(&(WATCHER, "IsStatusNotifierHostRegistered").to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        2_000,
                        gio::Cancellable::NONE,
                    )
                    .map_or_else(
                        |error| !definite(&error),
                        |reply| {
                            reply
                                .child_value(0)
                                .as_variant()
                                .and_then(|value| value.get::<bool>())
                                .unwrap_or(true)
                        },
                    );
            if probe.session_names.contains(PORTAL) {
                for interface in PORTAL_INTERFACES {
                    // Present, or unable to say in time: only a definite
                    // "no such interface" leaves it out.
                    if !matches!(portal_property(&bus, interface, "version"), Err(true)) {
                        probe.portals.insert(interface.to_string());
                    }
                }
                probe.screencast_sources =
                    probe_mask(&bus, "ScreenCast", "AvailableSourceTypes", 7)
                        .filter(|_| probe.portals.contains("ScreenCast"));
                probe.input_capture = probe_mask(&bus, "InputCapture", "SupportedCapabilities", 3)
                    .filter(|_| probe.portals.contains("InputCapture"));
            }
        }
        if let Ok(bus) = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            probe.system_bus = true;
            probe.system_names = bus_names(&bus, &[LOGIND]);
        }
        if let Some(runtime) = runtime {
            probe.pipewire_socket = socket(runtime.join("pipewire-0"));
            probe.pulse_socket = socket(runtime.join("pulse/native"));
        }
        if gst::init().is_ok() {
            probe.elements = WATCHED_ELEMENTS
                .iter()
                .filter(|name| gst::ElementFactory::find(name).is_some())
                .map(|name| name.to_string())
                .collect();
        }
        probe.terminals = crate::device_links::pty::desk::installed_terminals();
        probe.config_writable = config_writable();
        probe
    }
}

/// A portal bit mask: its value, or `fallback` (everything on) when the portal
/// could not answer in time, or `None` on a definite "no such property".
fn probe_mask(
    bus: &gio::DBusConnection,
    interface: &str,
    property: &str,
    fallback: u32,
) -> Option<u32> {
    match portal_property(bus, interface, property) {
        Ok(value) => value.get::<u32>(),
        Err(true) => None,
        Err(false) => Some(fallback),
    }
}

/// Whether a file can be created in the configuration folder, or in the
/// nearest folder above it that exists (it is made on first use). Asked of the
/// kernel with `access(2)`, which also answers for a read-only file system;
/// nothing is created or written, so a check never touches the real settings.
fn config_writable() -> bool {
    writable_below(&crate::device_links::config::desklink_config_root())
}

fn writable_below(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(existing) = path.ancestors().find(|dir| dir.is_dir()) else {
        return false;
    };
    std::ffi::CString::new(existing.as_os_str().as_bytes())
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> BTreeSet<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    /// A fully equipped machine; each test removes what it is about.
    fn full() -> Probe {
        Probe {
            desktop: "GNOME".into(),
            session_type: Some("wayland".into()),
            wayland_display: true,
            runtime_dir: true,
            session_bus: true,
            system_bus: true,
            session_names: names(&[PORTAL, NOTIFICATIONS, WATCHER]),
            system_names: names(&[LOGIND]),
            tray_host: true,
            portals: names(&["ScreenCast", "RemoteDesktop", "InputCapture", "Background"]),
            screencast_sources: Some(7),
            input_capture: Some(3),
            elements: WATCHED_ELEMENTS.iter().map(|e| e.to_string()).collect(),
            pipewire_socket: true,
            terminals: names(&["xterm"]),
            config_writable: true,
            toolkit: "GTK 4.14, libadwaita 1.5.".into(),
            ..Probe::default()
        }
    }

    fn level(rows: &[Check], id: &str) -> Level {
        rows.iter().find(|row| row.id == id).unwrap().level
    }

    fn row<'a>(rows: &'a [Check], id: &str) -> &'a Check {
        rows.iter().find(|row| row.id == id).unwrap()
    }

    #[test]
    fn a_fully_equipped_gnome_wayland_machine_has_nothing_missing() {
        let rows = evaluate(&full());
        for row in &rows {
            assert_eq!(row.level, Level::Ok, "{}: {}", row.title, row.detail);
        }
    }

    #[test]
    fn desktop_names_from_every_desktop_are_recognised() {
        for (value, expected) in [
            ("GNOME", Family::Gnome),
            ("ubuntu:GNOME", Family::Gnome),
            ("KDE", Family::Kde),
            ("X-Cinnamon", Family::Classic),
            ("XFCE", Family::Classic),
            ("MATE", Family::Classic),
            ("Budgie:GNOME", Family::Classic),
            ("GNOME-Flashback:GNOME", Family::Classic),
            ("GNOME-Classic:GNOME", Family::Gnome),
            ("pop:GNOME", Family::Gnome),
            ("Unity:Unity7:ubuntu", Family::Classic),
            ("COSMIC", Family::Cosmic),
            ("sway", Family::Wlroots),
            ("Hyprland", Family::Wlroots),
            ("river", Family::Wlroots),
            ("LXQt", Family::Classic),
            ("", Family::BareWm),
            ("i3", Family::BareWm),
            (":", Family::BareWm),
        ] {
            assert_eq!(family(value), expected, "{value:?}");
        }
        assert_eq!(desktop_name("ubuntu:GNOME"), "GNOME");
        assert!(desktop_name("").contains("unnamed"));
    }

    #[test]
    fn a_window_manager_without_autostart_is_told_what_line_to_add() {
        for desktop in ["", "i3", "sway", "Hyprland", "openbox"] {
            let rows = evaluate(&Probe {
                desktop: desktop.into(),
                ..full()
            });
            let row = row(&rows, "autostart");
            assert_eq!(row.level, Level::Limited, "{desktop:?}");
            assert!(row.detail.contains("exec desklink --gapplication-service"));
        }
        for desktop in ["GNOME", "KDE", "XFCE", "X-Cinnamon", "MATE", "COSMIC"] {
            let rows = evaluate(&Probe {
                desktop: desktop.into(),
                ..full()
            });
            assert_eq!(level(&rows, "autostart"), Level::Ok, "{desktop:?}");
        }
    }

    #[test]
    fn no_tray_is_limited_and_says_how_to_reopen_the_window() {
        let rows = evaluate(&Probe {
            tray_host: false,
            ..full()
        });
        let tray = row(&rows, "tray");
        assert_eq!(tray.level, Level::Limited);
        assert!(tray.detail.contains("AppIndicator"));
        assert!(tray.install.is_some());
        let rows = evaluate(&Probe {
            tray_host: false,
            desktop: "sway".into(),
            ..full()
        });
        assert!(row(&rows, "tray")
            .detail
            .contains("running `desklink` again"));
        assert!(row(&rows, "tray").install.is_none());
    }

    #[test]
    fn no_notification_daemon_is_said_plainly() {
        let rows = evaluate(&Probe {
            session_names: names(&[PORTAL]),
            ..full()
        });
        let row = row(&rows, "notifications");
        assert_eq!(row.level, Level::Limited);
        assert!(row.install.unwrap().arch.contains("dunst"));
    }

    #[test]
    fn x11_without_a_portal_shares_through_x11_directly() {
        let probe = Probe {
            desktop: "".into(),
            session_type: None,
            wayland_display: false,
            x11_display: true,
            session_names: names(&[NOTIFICATIONS]),
            portals: BTreeSet::new(),
            screencast_sources: None,
            input_capture: None,
            ..full()
        };
        let rows = evaluate(&probe);
        assert_eq!(level(&rows, "share"), Level::Ok);
        assert!(row(&rows, "share").tech.contains("X11 directly"));
        // And needs the X11 capture element.
        let mut bare = probe.clone();
        bare.elements.remove("ximagesrc");
        assert_eq!(level(&evaluate(&bare), "share"), Level::Missing);
        // Extend and Glide are Wayland-only.
        assert_eq!(level(&rows, "extend"), Level::Missing);
        assert_eq!(level(&rows, "glide"), Level::Missing);
    }

    #[test]
    fn wayland_without_a_portal_names_the_backend_for_the_desktop() {
        for (desktop, backend) in [
            ("GNOME", "xdg-desktop-portal-gnome"),
            ("KDE", "xdg-desktop-portal-kde"),
            ("sway", "xdg-desktop-portal-wlr"),
            ("COSMIC", "xdg-desktop-portal-cosmic"),
        ] {
            let rows = evaluate(&Probe {
                desktop: desktop.into(),
                session_names: names(&[NOTIFICATIONS]),
                portals: BTreeSet::new(),
                ..full()
            });
            let share = row(&rows, "share");
            assert_eq!(share.level, Level::Missing, "{desktop}");
            assert!(
                share.detail.contains(backend),
                "{desktop}: {}",
                share.detail
            );
        }
    }

    #[test]
    fn x11_is_the_session_type_or_a_display_without_wayland() {
        for (kind, wayland, x11, expected) in [
            (Some("x11"), false, true, true),
            (Some("X11"), true, true, true),
            (Some("wayland"), false, true, false),
            // startx from a console login keeps logind's `tty`.
            (Some("tty"), false, true, true),
            (None, false, true, true),
            (Some(""), false, true, true),
            // A compositor started from a tty, with Xwayland.
            (Some("tty"), true, true, false),
            (None, false, false, false),
        ] {
            assert_eq!(
                x11_session(kind, wayland, x11),
                expected,
                "{kind:?} {wayland} {x11}"
            );
        }
    }

    #[test]
    fn gnome_shell_extras_are_for_gnome_shell_only() {
        for desktop in ["GNOME", "ubuntu:GNOME", "pop:GNOME", "GNOME-Classic:GNOME"] {
            assert!(is_gnome_shell(desktop), "{desktop}");
        }
        for desktop in [
            "Budgie:GNOME",
            "GNOME-Flashback:GNOME",
            "KDE",
            "XFCE",
            "sway",
            "",
        ] {
            assert!(!is_gnome_shell(desktop), "{desktop}");
        }
        // Budgie has its own tray applet: no AppIndicator advice there.
        let rows = evaluate(&Probe {
            desktop: "Budgie:GNOME".into(),
            tray_host: false,
            ..full()
        });
        assert!(!row(&rows, "tray").detail.contains("AppIndicator"));
        assert!(row(&rows, "tray").install.is_none());
    }

    /// What each desktop gets, from the facts it really offers: every feature
    /// either works or is said plainly not to (packaging/README.md, "Desktops").
    #[test]
    fn every_desktop_gets_each_feature_or_a_plain_reason() {
        use Level::{Limited as L, Missing as M, Ok as O};
        let wayland =
            |desktop: &str, portals: &[&str], sources: Option<u32>, capture: Option<u32>| Probe {
                desktop: desktop.into(),
                portals: names(portals),
                screencast_sources: sources,
                input_capture: capture,
                ..full()
            };
        let x11 = |desktop: &str, session: Option<&str>, portals: &[&str]| Probe {
            desktop: desktop.into(),
            session_type: session.map(str::to_string),
            wayland_display: false,
            x11_display: true,
            portals: names(portals),
            screencast_sources: None,
            input_capture: None,
            ..full()
        };
        let all = ["ScreenCast", "RemoteDesktop", "InputCapture", "Background"];
        let cases: [(&str, Probe, [Level; 5]); 11] = [
            // name, probe: share, extend, glide, unlock, autostart
            (
                "GNOME",
                wayland("GNOME", &all, Some(7), Some(3)),
                [O, O, O, O, O],
            ),
            (
                "Plasma 6.1+",
                wayland("KDE", &all, Some(7), Some(3)),
                [O, O, O, O, O],
            ),
            (
                "Plasma 6.0",
                wayland("KDE", &["ScreenCast", "RemoteDesktop"], Some(7), None),
                [O, O, M, O, O],
            ),
            (
                "Plasma on X11",
                x11("KDE", Some("x11"), &[]),
                [O, M, M, O, O],
            ),
            ("Xfce", x11("XFCE", Some("x11"), &[]), [O, M, M, L, O]),
            (
                "Cinnamon",
                x11("X-Cinnamon", Some("x11"), &[]),
                [O, M, M, L, O],
            ),
            ("MATE", x11("MATE", Some("x11"), &[]), [O, M, M, L, O]),
            (
                "sway",
                wayland("sway", &["ScreenCast"], Some(3), None),
                [M, M, M, L, L],
            ),
            (
                "Hyprland",
                wayland(
                    "Hyprland",
                    &["ScreenCast", "GlobalShortcuts"],
                    Some(7),
                    None,
                ),
                [M, M, M, L, L],
            ),
            (
                "COSMIC",
                wayland("COSMIC", &["ScreenCast"], Some(3), None),
                [M, M, M, L, O],
            ),
            (
                "i3 via startx",
                x11("i3", Some("tty"), &[]),
                [O, M, M, L, L],
            ),
        ];
        for (name, probe, expected) in cases {
            let rows = evaluate(&probe);
            let got: Vec<Level> = ["share", "extend", "glide", "unlock", "autostart"]
                .iter()
                .map(|id| level(&rows, id))
                .collect();
            assert_eq!(got, expected, "{name}");
            for row in rows.iter().filter(|row| row.level != Level::Ok) {
                assert!(row.detail.len() > 20, "{name}: {} says too little", row.id);
            }
        }
    }

    #[test]
    fn a_wlroots_portal_without_remote_desktop_cannot_control() {
        let rows = evaluate(&Probe {
            desktop: "sway".into(),
            portals: names(&["ScreenCast"]),
            ..full()
        });
        let share = row(&rows, "share");
        assert_eq!(share.level, Level::Missing);
        assert!(share.tech.contains("RemoteDesktop"));
    }

    #[test]
    fn a_session_type_of_x11_overrides_a_stray_wayland_display() {
        let rows = evaluate(&Probe {
            session_type: Some("X11".into()),
            x11_display: true,
            ..full()
        });
        assert!(row(&rows, "session").detail.contains("on X11"));
    }

    #[test]
    fn missing_webrtc_plugins_are_named_with_packages_for_each_family() {
        let mut probe = full();
        probe.elements.remove("webrtcbin");
        probe.elements.remove("nicesrc");
        let rows = evaluate(&probe);
        let link = row(&rows, "link");
        assert_eq!(link.level, Level::Missing);
        assert!(link.tech.contains("webrtcbin") && link.tech.contains("nicesrc"));
        assert!(
            !link.detail.contains("webrtcbin"),
            "plain words for the person"
        );
        let packages = link.install.unwrap();
        assert!(packages.debian.contains("gstreamer1.0-plugins-bad"));
        assert!(packages.fedora.contains("gstreamer1-plugins-bad"));
        assert!(packages.arch.contains("gst-plugins-bad"));
        assert!(packages.opensuse.contains("gstreamer-plugins-bad"));
    }

    #[test]
    fn sound_needs_pipewire_and_says_so_on_a_pulseaudio_only_machine() {
        let rows = evaluate(&Probe {
            pipewire_socket: false,
            pulse_socket: true,
            ..full()
        });
        let sound = row(&rows, "sound");
        assert_eq!(sound.level, Level::Missing);
        assert!(sound.detail.contains("older PulseAudio"));
        // A Flatpak sees the socket only with `xdg-run/pipewire-0`.
        let rows = evaluate(&Probe {
            pipewire_socket: false,
            flatpak: true,
            ..full()
        });
        assert_eq!(level(&rows, "sound"), Level::Missing);
        assert!(row(&rows, "sound").detail.contains("xdg-run/pipewire-0"));
        let rows = evaluate(&Probe {
            flatpak: true,
            ..full()
        });
        assert_eq!(level(&rows, "sound"), Level::Ok);
        let mut probe = full();
        probe.elements.remove("opusenc");
        assert_eq!(level(&evaluate(&probe), "sound"), Level::Missing);
    }

    #[test]
    fn without_logind_locking_still_has_a_fallback() {
        let rows = evaluate(&Probe {
            system_names: BTreeSet::new(),
            system_bus: false,
            ..full()
        });
        let lock = row(&rows, "lock");
        assert_eq!(lock.level, Level::Limited);
        assert!(lock.detail.contains("elogind"));
    }

    #[test]
    fn extend_is_offered_only_on_gnome_and_kde_even_if_a_portal_claims_virtual() {
        for (desktop, expected) in [
            ("GNOME", Level::Ok),
            ("KDE", Level::Ok),
            ("Hyprland", Level::Missing),
            ("sway", Level::Missing),
            ("COSMIC", Level::Missing),
        ] {
            let rows = evaluate(&Probe {
                desktop: desktop.into(),
                ..full()
            });
            assert_eq!(level(&rows, "extend"), expected, "{desktop}");
        }
    }

    #[test]
    fn extend_and_glide_follow_the_portal_not_the_desktop_name() {
        let rows = evaluate(&Probe {
            desktop: "sway".into(),
            screencast_sources: Some(3),
            input_capture: None,
            portals: names(&["ScreenCast", "RemoteDesktop"]),
            ..full()
        });
        assert_eq!(level(&rows, "extend"), Level::Missing);
        assert_eq!(level(&rows, "glide"), Level::Missing);
        let rows = evaluate(&Probe {
            desktop: "KDE".into(),
            ..full()
        });
        assert_eq!(level(&rows, "extend"), Level::Ok);
        assert_eq!(level(&rows, "glide"), Level::Ok);
    }

    #[test]
    fn no_session_bus_no_display_no_runtime_dir_and_a_read_only_home_are_each_said() {
        let rows = evaluate(&Probe {
            session_bus: false,
            session_names: BTreeSet::new(),
            portals: BTreeSet::new(),
            tray_host: false,
            runtime_dir: false,
            config_writable: false,
            wayland_display: false,
            ..full()
        });
        assert_eq!(level(&rows, "bus"), Level::Missing);
        assert!(row(&rows, "bus").tech.contains("dbus-run-session"));
        assert_eq!(level(&rows, "session"), Level::Missing);
        assert_eq!(level(&rows, "storage"), Level::Missing);
        assert_eq!(level(&rows, "runtime"), Level::Limited);
        assert_eq!(level(&rows, "share"), Level::Missing);
    }

    #[test]
    fn no_terminal_program_is_limited_except_in_a_flatpak() {
        let none = Probe {
            terminals: BTreeSet::new(),
            ..full()
        };
        assert_eq!(level(&evaluate(&none), "terminal"), Level::Limited);
        let flatpak = Probe {
            flatpak: true,
            ..none
        };
        assert_eq!(level(&evaluate(&flatpak), "terminal"), Level::Ok);
    }

    #[test]
    fn video_decoders_and_encoders_are_each_required_somewhere() {
        let mut probe = full();
        for decoder in ["vp8dec", "avdec_h264", "openh264dec", "vah264dec"] {
            probe.elements.remove(decoder);
        }
        let rows = evaluate(&probe);
        assert_eq!(level(&rows, "view"), Level::Missing);
        assert_eq!(level(&rows, "share"), Level::Ok, "encoders are separate");
    }

    #[test]
    fn os_release_maps_to_a_family_and_a_command() {
        let id = |text| distro_from_os_release(text);
        assert_eq!(id("ID=ubuntu\nID_LIKE=debian\n"), Some(Distro::Debian));
        assert_eq!(
            id("ID=\"linuxmint\"\nID_LIKE=\"ubuntu debian\""),
            Some(Distro::Debian)
        );
        assert_eq!(id("ID=fedora\n"), Some(Distro::Fedora));
        assert_eq!(
            id("ID=rocky\nID_LIKE=\"rhel centos fedora\""),
            Some(Distro::Fedora)
        );
        assert_eq!(id("ID=manjaro\nID_LIKE=arch"), Some(Distro::Arch));
        assert_eq!(id("ID=endeavouros\nID_LIKE=arch"), Some(Distro::Arch));
        assert_eq!(
            id("NAME=x\nID=\"opensuse-tumbleweed\"\nID_LIKE=\"opensuse suse\""),
            Some(Distro::Suse)
        );
        assert_eq!(id("ID=nixos"), Some(Distro::Nix));
        assert_eq!(id("ID=void"), None);
        assert_eq!(id(""), None);
        assert_eq!(
            GST_BAD.command(Some(Distro::Debian)),
            "sudo apt install gstreamer1.0-plugins-bad gstreamer1.0-nice"
        );
        assert_eq!(
            GST_BAD.command(Some(Distro::Arch)),
            "sudo pacman -S gst-plugins-bad libnice"
        );
        assert!(GST_BAD.command(Some(Distro::Nix)).contains("NixOS"));
        assert!(GST_BAD.command(None).contains("Fedora:"));
    }

    #[test]
    fn unlock_is_promised_only_where_the_lock_screen_is_known_to_obey() {
        for (desktop, expected) in [
            ("GNOME", Level::Ok),
            ("ubuntu:GNOME", Level::Ok),
            ("KDE", Level::Ok),
            ("XFCE", Level::Limited),
            ("sway", Level::Limited),
            ("", Level::Limited),
        ] {
            let rows = evaluate(&Probe {
                desktop: desktop.into(),
                ..full()
            });
            assert_eq!(level(&rows, "unlock"), expected, "{desktop:?}");
        }
    }

    #[test]
    fn a_flatpak_on_x11_without_a_portal_cannot_capture_and_says_so() {
        let mut probe = Probe {
            flatpak: true,
            desktop: "X-Cinnamon".into(),
            wayland_display: false,
            x11_display: true,
            session_type: Some("x11".into()),
            session_names: names(&[NOTIFICATIONS]),
            portals: BTreeSet::new(),
            ..full()
        };
        probe.elements.remove("ximagesrc");
        let rows = evaluate(&probe);
        let share = row(&rows, "share");
        assert_eq!(share.level, Level::Missing);
        assert!(share.detail.contains("Flatpak"));
        assert!(share.install.is_none());
    }

    #[test]
    fn the_text_report_lists_every_row_and_one_command() {
        let rows = evaluate(&Probe::default());
        let text = as_text(&rows, Some(Distro::Fedora));
        for row in &rows {
            assert!(text.contains(row.title), "{}", row.title);
        }
        assert!(text.contains("[missing] Phone link"));
        assert!(text.contains("sudo dnf install gstreamer1-plugins-bad-free"));
        assert!(!text.contains("sudo apt"));
        assert!(as_text(&rows, None).contains("Arch: gst-plugins-bad libnice"));
    }

    #[test]
    fn the_dialog_text_lists_every_family() {
        let lines = GST_BAD.lines();
        for family in ["Debian/Ubuntu:", "Fedora:", "Arch:", "openSUSE:"] {
            assert!(lines.contains(family));
        }
    }

    #[test]
    fn every_row_has_a_unique_id_and_plain_text() {
        let rows = evaluate(&Probe::default());
        let mut ids: Vec<_> = rows.iter().map(|row| row.id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), rows.len());
        for row in rows {
            assert!(!row.detail.is_empty() && !row.title.is_empty());
        }
    }

    /// The person reads plain words; component and service names stay in the
    /// technical note (the copied report, `--check` and the log).
    #[test]
    fn details_name_no_components_and_the_report_keeps_them() {
        let jargon = [
            "GStreamer",
            "WebRTC",
            "portal",
            "D-Bus",
            "StatusNotifier",
            "XDG_",
            "webrtcbin",
            "vp8",
            "VP8",
            "pipewiresrc",
            "logind's",
            "virtual monitor",
        ];
        let mut machines = vec![Probe::default(), full()];
        let mut bare = full();
        bare.elements.clear();
        bare.portals.clear();
        bare.tray_host = false;
        bare.session_names = names(&[PORTAL]);
        machines.push(bare);
        machines.push(Probe {
            desktop: "sway".into(),
            portals: names(&["ScreenCast"]),
            ..full()
        });
        machines.push(Probe {
            flatpak: true,
            pipewire_socket: false,
            ..full()
        });
        for probe in &machines {
            let rows = evaluate(probe);
            for row in &rows {
                // A package to install is named as the distribution names it.
                let words = row.detail.replace("xdg-desktop-portal", "");
                for word in jargon {
                    assert!(
                        !words.contains(word),
                        "{}: {:?} in {:?}",
                        row.id,
                        word,
                        row.detail
                    );
                }
            }
            let text = as_text(&rows, None);
            for row in rows.iter().filter(|row| !row.tech.is_empty()) {
                assert!(text.contains(&row.tech), "{}", row.id);
            }
        }
    }

    #[test]
    fn writability_is_asked_of_the_nearest_existing_folder_and_writes_nothing() {
        // As root everything is writable: the body runs as `nobody` there.
        if !crate::test_privileges::unprivileged(
            "platform::tests::writability_is_asked_of_the_nearest_existing_folder_and_writes_nothing",
        ) {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("desklink-writable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let below = dir.join("not").join("made").join("yet");
        assert!(writable_below(&below));
        assert!(!below.exists(), "nothing was created");
        // A folder the user may not write to, and one that is not there at all.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        assert!(!writable_below(&below));
        assert!(!writable_below(std::path::Path::new("/proc/self/nothing")));
        assert!(!writable_below(std::path::Path::new("")));
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_a_definite_no_from_d_bus_switches_a_portal_feature_off() {
        for no in [
            "org.freedesktop.DBus.Error.UnknownMethod",
            "org.freedesktop.DBus.Error.UnknownInterface",
            "org.freedesktop.DBus.Error.UnknownProperty",
            "org.freedesktop.DBus.Error.UnknownObject",
            "org.freedesktop.DBus.Error.InvalidArgs",
        ] {
            assert!(definite_negative(no), "{no}");
        }
        for maybe in [
            "org.freedesktop.DBus.Error.NoReply",
            "org.freedesktop.DBus.Error.Timeout",
            "org.freedesktop.DBus.Error.TimedOut",
            "org.freedesktop.DBus.Error.ServiceUnknown",
            "org.freedesktop.DBus.Error.Failed",
            "",
        ] {
            assert!(!definite_negative(maybe), "{maybe}");
        }
    }

    #[test]
    fn a_name_is_absent_only_when_both_bus_lists_were_read_and_lack_it() {
        let watched = [PORTAL, NOTIFICATIONS];
        // Both lists read: what is not there is not there.
        let complete = settle_names(Some(names(&[PORTAL])), Some(names(&["x"])), &watched);
        assert!(complete.contains(PORTAL) && !complete.contains(NOTIFICATIONS));
        // Activatable but not running counts as present.
        let activatable = settle_names(Some(names(&[])), Some(names(&[NOTIFICATIONS])), &watched);
        assert!(activatable.contains(NOTIFICATIONS));
        // One list missing (a timeout): every watched name is believed present.
        for (listed, activatable) in [
            (None, Some(names(&[]))),
            (Some(names(&[])), None),
            (None, None),
        ] {
            let all = settle_names(listed, activatable, &watched);
            assert!(all.contains(PORTAL) && all.contains(NOTIFICATIONS));
        }
    }

    #[test]
    fn the_session_bus_is_asked_for_only_where_that_starts_nothing() {
        assert!(session_bus_reachable(true, false));
        assert!(session_bus_reachable(false, true));
        assert!(!session_bus_reachable(false, false));
    }

    #[test]
    fn the_banner_is_for_the_two_losses_that_stop_everything() {
        let mut probe = full();
        assert_eq!(sticky_from(&evaluate(&probe), None), None);
        probe.config_writable = false;
        let (id, words) = sticky_from(&evaluate(&probe), None).unwrap();
        assert_eq!(id, "storage");
        assert!(words.contains("configuration folder"));
        probe.elements.remove("webrtcbin");
        let (id, words) = sticky_from(&evaluate(&probe), Some(Distro::Arch)).unwrap();
        assert_eq!(id, "link", "the phone link comes first");
        assert!(
            words.contains("sudo pacman -S gst-plugins-bad libnice"),
            "{words}"
        );
        // A limit (no tray, no notifier) is never a banner.
        let probe = Probe {
            tray_host: false,
            session_names: BTreeSet::new(),
            ..full()
        };
        assert_eq!(sticky_from(&evaluate(&probe), None), None);
    }

    #[test]
    fn a_later_probe_replaces_the_report_and_counts() {
        let _held = TEST_REPORT.lock().unwrap_or_else(|e| e.into_inner());
        let before = generation();
        set_report_for_tests(evaluate(&Probe::default()));
        assert!(generation() > before);
        assert!(report().is_some());
        assert!(!works("link"));
        set_report_for_tests(evaluate(&full()));
        assert!(works("link"));
        assert_eq!(reason("sound"), None);
        clear_report_for_tests();
        assert!(works("link"), "nothing is hidden before a probe");
    }

    #[test]
    fn the_share_row_needs_the_vp8_encoder_not_any_encoder() {
        let mut probe = full();
        probe.elements.remove("vp8enc");
        assert_eq!(level(&evaluate(&probe), "share"), Level::Missing);
        assert!(row(&evaluate(&probe), "share").tech.contains("vp8enc"));
        probe.elements.insert("x264enc".into());
        probe.elements.insert("vah264enc".into());
        assert_eq!(level(&evaluate(&probe), "share"), Level::Missing);
    }
}
