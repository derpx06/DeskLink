//! The system check's rules: what each feature needs, by desktop family, and
//! [`evaluate`], a pure function of a [`Probe`] (the facts) to the rows.
//! Tests fake any desktop through it.
//!
//! Sibling edges: none (the parent's types and bus names).

use super::*;

// ---- the desktop families --------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Family {
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
pub(super) fn family(current_desktop: &str) -> Family {
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
pub(super) fn desktop_name(current_desktop: &str) -> String {
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

pub(super) const GST_BAD: Packages = Packages {
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

    // Finding the phone: another IPv4 listener on the discovery port takes
    // unicast beacons (the kernel gives each to one socket only).
    rows.push(if probe.discovery_sharers.is_empty() {
        check(
            "discovery",
            "Finding your phone",
            Level::Ok,
            "Only DeskLink listens for phones on this computer.",
            None,
        )
    } else {
        let names = and_list(&probe.discovery_sharers);
        let (listen, it) = if probe.discovery_sharers.len() == 1 {
            ("listens", "it")
        } else {
            ("listen", "them")
        };
        check(
            "discovery",
            "Finding your phone",
            Level::Limited,
            format!(
                "{names} also {listen} for phones on this computer and can take the \
                 messages your phone sends DeskLink, so your phone can take minutes to \
                 reconnect. Turn {names} off (quit {it} and stop {it} starting at login, or \
                 uninstall {it}), then restart DeskLink."
            ),
            None,
        )
        .tech(format!(
            "Another IPv4 UDP socket is bound to port {DISCOVERY_PORT} (SO_REUSEADDR): each \
             unicast beacon reaches only one of them."
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

    // The clipboard is read through X11 (XWayland on Wayland): Wayland lets
    // only the focused window read it, and DeskLink is usually in the
    // background (`daemon/clipboard.rs`).
    rows.push(if probe.x11_display || !wayland {
        check(
            "clipboard",
            "Shared clipboard",
            Level::Ok,
            "What you copy here can reach the phone.",
            None,
        )
    } else {
        check(
            "clipboard",
            "Shared clipboard",
            Level::Limited,
            "This session runs without X11 support (Xwayland), so what you copy here does not \
             reach the phone; the phone's copies still arrive. Turn Xwayland on in your \
             compositor's settings (on niri, run xwayland-satellite) and log in again.",
            None,
        )
        .tech("No DISPLAY: the clipboard watch and password-manager hints need X11 or Xwayland.")
    });

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
            "The video part DeskLink sends this screen with is not installed.",
            Some(GST_GOOD),
        )
        .tech("Needs GStreamer vp8enc (gst-plugins-good).")
    } else if !has_portal("RemoteDesktop") {
        // wlroots portals (xdg-desktop-portal-wlr, -hyprland): the phone
        // watches, view only.
        check(
            "share",
            "Share this screen and let the phone control it",
            Level::Limited,
            format!(
                "{SHARE_THROUGH_DESKTOP} The phone can watch this screen but not control it: \
                 this desktop lets apps show its screen, not control it. GNOME and KDE Plasma \
                 can."
            ),
            None,
        )
        .tech("The portal has ScreenCast but no RemoteDesktop interface: view only.")
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
                "The phone's screen cannot be shown: the video parts it needs are not installed.",
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
            "The sound part DeskLink uses is not installed.",
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
                if wayland && family == Family::Kde {
                    "This version of KDE Plasma's screen sharing cannot add a second screen. \
                     A newer xdg-desktop-portal-kde can."
                } else if wayland && family == Family::Gnome {
                    "This version of GNOME's screen sharing cannot add a second screen. A newer \
                     xdg-desktop-portal-gnome can."
                } else if wayland {
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
