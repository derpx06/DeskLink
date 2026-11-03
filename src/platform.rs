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
use std::sync::OnceLock;

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
    pub detail: String,
    pub install: Option<Packages>,
}

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
    if has(&["gnome", "gnome-classic", "gnome-flashback"]) {
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
    }
}

const GST_BAD: Packages = Packages {
    debian: "gstreamer1.0-plugins-bad gstreamer1.0-nice",
    fedora: "gstreamer1-plugins-bad-free libnice-gstreamer1",
    arch: "gst-plugins-bad libnice",
    opensuse: "gstreamer-plugins-bad gstreamer-plugins-libnice",
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
    fedora: "gstreamer1-plugins-good gstreamer1-libav",
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
    rows.push(check(
        "session",
        "Desktop session",
        if wayland || probe.x11_display {
            Level::Ok
        } else {
            Level::Missing
        },
        format!(
            "{} on {session}{packaging}. {}",
            desktop_name(&probe.desktop),
            probe.toolkit
        ),
        None,
    ));

    // The bus.
    rows.push(if probe.session_bus {
        check("bus", "D-Bus session bus", Level::Ok, "Found.", None)
    } else {
        check(
            "bus",
            "D-Bus session bus",
            Level::Missing,
            "There is no session bus, so the tray, notifications, screen sharing and locking \
             cannot work. Start DeskLink from your desktop session, or run it as \
             `dbus-run-session desklink`.",
            None,
        )
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
            "Phone link (WebRTC)",
            Level::Ok,
            "GStreamer's WebRTC plugins are installed.",
            None,
        )
    } else {
        check(
            "link",
            "Phone link (WebRTC)",
            Level::Missing,
            format!(
                "Phones cannot connect without GStreamer's WebRTC plugins. Missing: {}.",
                missing.join(", ")
            ),
            Some(GST_BAD),
        )
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
                "No tray was found on this desktop (it needs a StatusNotifierItem host, such \
                 as the tray in Plasma, Xfce, MATE, Cinnamon, Waybar or swaybar). DeskLink \
                 still works: closing the window keeps it running, and you reopen it from \
                 the launcher or by running `desklink` again.",
                None,
            ),
        };
        check("tray", "System tray", Level::Limited, detail, install)
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
                "No notification service is running, so DeskLink cannot show phone \
                 notifications or alerts here. Install one (dunst, mako or swaync, or your \
                 desktop's own).",
                Some(NOTIFIER),
            )
        },
    );

    // Sharing and controlling this computer.
    let encoder = any(&["vp8enc", "x264enc", "openh264enc", "vah264enc"]);
    let portal_complete = has_portal("ScreenCast") && has_portal("RemoteDesktop");
    let x11_direct = !wayland && probe.x11_display && !portal_complete;
    rows.push(if !probe.session_bus && !x11_direct {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            "Needs the D-Bus session bus.",
            None,
        )
    } else if x11_direct {
        // Without a portal DeskLink captures and types through X11 itself.
        if has("ximagesrc") && encoder {
            check(
                "share",
                "Share this screen and let the phone control it",
                Level::Ok,
                "Through X11 directly (this session has no screen-sharing portal). You \
                 approve each session at this computer.",
                None,
            )
        } else if probe.flatpak {
            check(
                "share",
                "Share this screen and let the phone control it",
                Level::Missing,
                "This session has no screen-sharing portal, and the Flatpak runtime has no X11 \
                 screen capture. Install DeskLink from your distribution's package instead, or \
                 use a desktop whose portal offers screen sharing.",
                None,
            )
        } else {
            check(
                "share",
                "Share this screen and let the phone control it",
                Level::Missing,
                "Needs GStreamer's X11 screen capture and a video encoder (VP8 or H.264).",
                Some(GST_GOOD),
            )
        }
    } else if !has_portal("ScreenCast") {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            format!(
                "Wayland needs a desktop portal for screen sharing, and none answered. \
                 Install {} and restart your session.",
                portal_backend(family)
            ),
            None,
        )
    } else if !has_portal("RemoteDesktop") {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            format!(
                "This desktop's portal can show the screen but has no remote-control \
                 interface (RemoteDesktop), and DeskLink needs both. {} does not offer it \
                 on every compositor; GNOME and KDE Plasma do.",
                portal_backend(family)
            ),
            None,
        )
    } else if !any(&["pipewiresrc"]) {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            "Wayland screen sharing reads PipeWire video through GStreamer's pipewire plugin.",
            Some(GST_PIPEWIRE),
        )
    } else if !encoder {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Missing,
            "No video encoder found (VP8 is in gst-plugins-good; H.264 in gst-plugins-ugly \
             or gst-libav).",
            Some(GST_GOOD),
        )
    } else {
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Ok,
            "Through the desktop portal. You approve each session at this computer.",
            None,
        )
    });

    // The phone's screen shown here.
    rows.push(
        if any(&["vp8dec", "avdec_h264", "openh264dec", "vah264dec"]) {
            check(
                "view",
                "Phone screen on this computer",
                Level::Ok,
                "A video decoder is installed.",
                None,
            )
        } else {
            check(
                "view",
                "Phone screen on this computer",
                Level::Missing,
                "No VP8 or H.264 decoder found, so the phone's screen cannot be shown.",
                Some(GST_VIDEO),
            )
        },
    );

    // Sound.
    rows.push(if !any(&["pipewiresink", "pipewiresrc"]) {
        check(
            "sound",
            "Sound with the phone",
            Level::Missing,
            "Needs GStreamer's PipeWire plugin.",
            Some(GST_PIPEWIRE),
        )
    } else if !has("opusenc") || !has("opusdec") {
        check(
            "sound",
            "Sound with the phone",
            Level::Missing,
            "Needs the Opus codec.",
            Some(GST_BASE),
        )
    } else if !probe.pipewire_socket && !probe.flatpak {
        let pulse = if probe.pulse_socket {
            "This computer runs PulseAudio alone. "
        } else {
            "No sound server answered. "
        };
        check(
            "sound",
            "Sound with the phone",
            Level::Missing,
            format!("{pulse}Sound with the phone needs PipeWire (with its PulseAudio layer, so other apps keep working)."),
            Some(PIPEWIRE),
        )
    } else {
        check(
            "sound",
            "Sound with the phone",
            Level::Ok,
            "PipeWire is running.",
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
                "The portal can add a virtual monitor.",
                None,
            )
        } else {
            check(
                "extend",
                "Phone as a second display (Extend)",
                Level::Missing,
                if wayland {
                    "This desktop cannot add a virtual monitor for DeskLink (GNOME can; other \
                     compositors either lack the portal source or only share a workspace)."
                } else {
                    "Needs a Wayland session whose portal can add a virtual monitor (GNOME \
                     can). X11 sessions cannot."
                },
                None,
            )
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
                "The portal can capture keyboard and pointer.",
                None,
            )
        } else {
            check(
                "glide",
                "Move the pointer to the phone (Glide)",
                Level::Missing,
                "Needs a Wayland session whose portal offers input capture (GNOME 45 or later, \
                 or KDE Plasma 6.1 or later).",
                None,
            )
        },
    );

    // Lock, unlock, resume.
    rows.push(if probe.system_names.contains(LOGIND) {
        check(
            "lock",
            "Lock and unlock from the phone",
            Level::Ok,
            "logind (or elogind) is running.",
            None,
        )
    } else {
        check(
            "lock",
            "Lock and unlock from the phone",
            Level::Limited,
            "logind or elogind is not running. Locking falls back to the desktop's own \
             screensaver service; unlocking with the phone and noticing the computer waking \
             from sleep need logind or elogind.",
            None,
        )
    });

    // Unlocking with the phone: only where the lock screen is known to obey
    // logind's Unlock signal. Others may ignore it, so it is not promised.
    rows.push(match family {
        Family::Gnome | Family::Kde => check(
            "unlock",
            "Unlock with the phone",
            Level::Ok,
            "This desktop's lock screen accepts unlock requests from logind.",
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
            "This session does not run XDG autostart entries. Add `exec desklink \
             --gapplication-service` to your window manager's startup (i3 and sway config, \
             Hyprland `exec-once`, openbox autostart), or use dex.",
            None,
        ),
        Family::Wlroots => check(
            "autostart",
            "Start at login",
            Level::Limited,
            "Most compositors of this kind do not run XDG autostart entries. Add \
             `exec desklink --gapplication-service` to the compositor's config (sway `exec`, \
             Hyprland `exec-once`), or run dex.",
            None,
        ),
        _ => check(
            "autostart",
            "Start at login",
            Level::Ok,
            if probe.flatpak {
                "Through the Background portal."
            } else {
                "Through an XDG autostart entry."
            },
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
        rows.push(check(
            "runtime",
            "Runtime folder",
            Level::Limited,
            "XDG_RUNTIME_DIR is not set, so shared terminals and media art are off. A \
             normal login session sets it.",
            None,
        ));
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
    print!("{}", as_text(&rows, distro()));
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

static REPORT: OnceLock<Vec<Check>> = OnceLock::new();

/// The result of the start-up probe; `None` until it has finished.
pub fn report() -> Option<&'static [Check]> {
    REPORT.get().map(Vec::as_slice)
}

/// Probes the system on a background thread (the probe talks to D-Bus and
/// GStreamer's plugin registry, which can take seconds on a first run).
pub fn start() {
    let _ = std::thread::Builder::new()
        .name("desklink-platform".into())
        .spawn(|| {
            let probe = Probe::real();
            let rows = evaluate(&probe);
            for row in rows.iter().filter(|row| row.level == Level::Missing) {
                eprintln!("[DeskLink] {}: {}", row.title, row.detail);
            }
            let _ = REPORT.set(rows);
        });
}

fn names(bus: &gio::DBusConnection, method: &str) -> BTreeSet<String> {
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
    .unwrap_or_default()
    .into_iter()
    .collect()
}

fn bus_names(bus: &gio::DBusConnection) -> BTreeSet<String> {
    let mut all = names(bus, "ListNames");
    all.extend(names(bus, "ListActivatableNames"));
    all
}

fn portal_property(
    bus: &gio::DBusConnection,
    interface: &str,
    name: &str,
) -> Option<glib::Variant> {
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
    .ok()
    .and_then(|reply| reply.child_value(0).as_variant())
}

fn env_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|value| !value.is_empty())
}

fn socket(path: std::path::PathBuf) -> bool {
    use std::os::unix::fs::FileTypeExt;
    path.metadata()
        .is_ok_and(|meta| meta.file_type().is_socket())
}

impl Probe {
    /// Reads the facts from this system. Never fails: what cannot be read is
    /// reported absent.
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
        if let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) {
            probe.session_bus = true;
            probe.session_names = bus_names(&bus);
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
                    .ok()
                    .and_then(|reply| reply.child_value(0).as_variant())
                    .and_then(|value| value.get::<bool>())
                    .unwrap_or(false);
            if probe.session_names.contains(PORTAL) {
                for interface in PORTAL_INTERFACES {
                    if portal_property(&bus, interface, "version").is_some() {
                        probe.portals.insert(interface.to_string());
                    }
                }
                probe.screencast_sources =
                    portal_property(&bus, "ScreenCast", "AvailableSourceTypes")
                        .and_then(|value| value.get::<u32>());
                probe.input_capture =
                    portal_property(&bus, "InputCapture", "SupportedCapabilities")
                        .and_then(|value| value.get::<u32>());
            }
        }
        if let Ok(bus) = gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE) {
            probe.system_bus = true;
            probe.system_names = bus_names(&bus);
        }
        if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from) {
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
            ("GNOME-Flashback:GNOME", Family::Gnome),
            ("KDE", Family::Kde),
            ("X-Cinnamon", Family::Classic),
            ("XFCE", Family::Classic),
            ("MATE", Family::Classic),
            ("Budgie:GNOME", Family::Gnome),
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
        assert!(row(&rows, "share").detail.contains("X11 directly"));
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
    fn a_wlroots_portal_without_remote_desktop_cannot_control() {
        let rows = evaluate(&Probe {
            desktop: "sway".into(),
            portals: names(&["ScreenCast"]),
            ..full()
        });
        let share = row(&rows, "share");
        assert_eq!(share.level, Level::Missing);
        assert!(share.detail.contains("RemoteDesktop"));
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
        assert!(link.detail.contains("webrtcbin") && link.detail.contains("nicesrc"));
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
        assert!(sound.detail.contains("PulseAudio alone"));
        // Inside a Flatpak the socket is not visible the same way: not judged.
        let rows = evaluate(&Probe {
            pipewire_socket: false,
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
        assert!(row(&rows, "bus").detail.contains("dbus-run-session"));
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
        assert!(text.contains("[missing] Phone link (WebRTC)"));
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

    #[test]
    fn writability_is_asked_of_the_nearest_existing_folder_and_writes_nothing() {
        let dir = std::env::temp_dir().join(format!("desklink-writable-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let below = dir.join("not").join("made").join("yet");
        assert!(writable_below(&below));
        assert!(!below.exists(), "nothing was created");
        // /proc is never writable for a normal user, and a missing root is not.
        assert!(!writable_below(std::path::Path::new("/proc/self/nothing")));
        assert!(!writable_below(std::path::Path::new("")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn reasons_only_exist_for_missing_features_once_probed() {
        // Nothing probed in a unit test: no reason, so nothing is hidden.
        assert_eq!(reason("sound"), None);
        assert_eq!(note("tray"), None);
    }
}
