//! The first run's words and rules, with no widgets: the glance, the tour
//! pages, the setup step's words, when to show it, navigation between steps,
//! and the setup rows built from the system check (pure, unit-tested).
//!
//! Sibling edges: none. Outside: `platform` (checks), `shell_dbus`
//! (extension status).

use super::*;

/// The very first screen: what DeskLink does and how to start, at a glance.
/// The phone's first screen says the same words.
pub(in crate::window) const GLANCE_KICKER: &str = "Welcome to DeskLink";
pub(in crate::window) const GLANCE_TITLE: &str = "Your phone and computer, together";
pub(in crate::window) const GLANCE_LINES: [(&str, &str); 3] = [
    (
        "desklink-desktop-windows-symbolic",
        "See and control one screen from the other",
    ),
    (
        "desklink-upload-file-symbolic",
        "Send files, and copy on one, paste on the other",
    ),
    (
        "desklink-notifications-symbolic",
        "Phone notifications and terminals on both",
    ),
];
pub(in crate::window) const GLANCE_STEPS_TITLE: &str = "Start in 3 steps";
pub(in crate::window) const GLANCE_STEPS: [&str; 3] = [
    "Install DeskLink on both",
    "Pair once with a code",
    "Done: they connect on their own",
];
pub(in crate::window) const GLANCE_FOOTER: &str =
    "No account. Only your paired devices can connect.";
pub(in crate::window) const GLANCE_START: &str = "Get started";

/// One page of the tour: the small line above the title, the title, the body.
pub(in crate::window) struct TourPage {
    pub kicker: &'static str,
    pub title: &'static str,
    pub body: &'static str,
}

/// The tour after the glance, in order: one idea, one scene, one sentence
/// each, in the phone's words.
pub(in crate::window) const TOUR: [TourPage; 3] = [
    TourPage {
        kicker: "Screens",
        title: "See it. Steer it.",
        body: "Watch one screen on the other, and use your phone as a touchpad and keyboard.",
    },
    TourPage {
        kicker: "Share",
        title: "Files and copies, both ways",
        body: "Send a file in a tap, and copy on one device to paste on the other.",
    },
    TourPage {
        kicker: "Terminals",
        title: "Terminals that follow you",
        body: "Start a terminal on one device and pick it up on the other.",
    },
];

/// The step after the tour: what this computer needs.
/// Step 0 is the glance, then the tour's pages, then this.
pub(in crate::window) const SETUP: usize = TOUR.len() + 1;
pub(in crate::window) const SETUP_KICKER: &str = "Set up";
pub(in crate::window) const SETUP_TITLE: &str = "Set up this computer";
pub(in crate::window) const SETUP_BODY: &str =
    "DeskLink checked what it needs on this computer. Fix what \
    you like now, or later in Settings. You can go on either way.";

/// The tour is for someone who has not paired and has not seen it; a paired
/// person never sees it unasked.
pub(in crate::window) fn show_on_start(seen: bool, any_paired: bool) -> bool {
    !seen && !any_paired
}

/// How many segments the progress shows: the glance, the tour, the setup step and, when
/// it ends in pairing, the pairing page.
pub(in crate::window) fn segment_count(for_pairing: bool) -> usize {
    SETUP + 1 + usize::from(for_pairing)
}

/// A move through the steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::window) enum Nav {
    Next,
    Back,
    Skip,
}

/// Where `nav` goes from `step`: another step, or `None` to leave (to the
/// pairing page, or back to the phones on a replay). Skip leaves the glance or
/// the tour for the setup step; from the setup step it leaves.
pub(in crate::window) fn go(step: usize, nav: Nav) -> Option<usize> {
    match nav {
        Nav::Next if step < SETUP => Some(step + 1),
        Nav::Back => Some(step.saturating_sub(1)),
        Nav::Skip if step < SETUP => Some(SETUP),
        Nav::Next | Nav::Skip => None,
    }
}

/// The keys of the onboarding: Enter and the forward arrow go on, the back
/// arrow goes back, Esc skips. `rtl` swaps the arrows.
pub(in crate::window) fn key_nav(key: gdk::Key, rtl: bool) -> Option<Nav> {
    let (forward, backward) = if rtl {
        (gdk::Key::Left, gdk::Key::Right)
    } else {
        (gdk::Key::Right, gdk::Key::Left)
    };
    match key {
        gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Page_Down => Some(Nav::Next),
        gdk::Key::Page_Up => Some(Nav::Back),
        gdk::Key::Escape => Some(Nav::Skip),
        key if key == forward => Some(Nav::Next),
        key if key == backward => Some(Nav::Back),
        _ => None,
    }
}

/// The main button's words on `step`.
pub(in crate::window) fn next_label(step: usize, for_pairing: bool) -> &'static str {
    match step {
        0 => GLANCE_START,
        step if step < SETUP => "Next",
        _ if for_pairing => "Continue",
        _ => "Done",
    }
}

// ---- Set up this computer: the pure part ------------------------------------

/// What a setup row's button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::window) enum Fix {
    None,
    /// Start at login, with its current state.
    Switch(bool),
    /// Shows a notification, so the person sees they work.
    TestNotification,
    /// Installs and turns on DeskLink's top-bar extension.
    TurnOnExtension,
    /// Asks GNOME to log out (GNOME asks to confirm).
    LogOut,
    /// Copies an install command.
    Copy(String),
}

/// One row of the setup step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::window) struct SetupRow {
    pub id: &'static str,
    pub icon: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub status_words: &'static str,
    pub detail: String,
    pub fix: Fix,
}

fn status_of(level: Level) -> (Status, &'static str) {
    match level {
        Level::Ok => (Status::Ok, "Ready"),
        Level::Limited => (Status::Warn, "Needs a step"),
        Level::Missing => (Status::Err, "Not available"),
    }
}

/// The facts the setup step is drawn from.
pub(in crate::window) struct SetupFacts<'a> {
    pub checks: &'a [Check],
    pub start_at_login: bool,
    /// DeskLink's top-bar extension; `None` where GNOME is not running.
    pub extension: Option<ExtensionStatus>,
    pub gnome: bool,
    pub distro: Option<Distro>,
}

/// The rows of "Set up this computer", in order: start at login,
/// notifications, the top bar (GNOME) or the tray, screen sharing, Glide, pairing keys,
/// video and sound support. Only what [`crate::platform`] found is said.
pub(in crate::window) fn setup_rows(facts: &SetupFacts) -> Vec<SetupRow> {
    let find = |id: &str| facts.checks.iter().find(|check| check.id == id);
    let install = |check: &Check| {
        check
            .install
            .filter(|_| check.level != Level::Ok)
            .map(|packages| packages.command(facts.distro))
    };
    let with_install = |check: &Check| match install(check) {
        Some(_) => format!("{}\n\n{}", check.detail, crate::window::INSTALL_HINT),
        None => check.detail.clone(),
    };
    let copy_or_none = |check: &Check| install(check).map_or(Fix::None, Fix::Copy);
    let mut rows = Vec::new();

    // Start at login.
    let autostart = find("autostart");
    let (status, status_words, detail) = match autostart {
        Some(check) if check.level != Level::Ok => {
            (Status::Warn, "Needs a step", check.detail.clone())
        }
        _ if facts.start_at_login => (
            Status::Ok,
            "On",
            "DeskLink starts in the background when you log in, so your phone can reach this \
             computer after a restart."
                .to_string(),
        ),
        _ => (
            Status::Idle,
            "Off",
            "Your phone can reach this computer only while DeskLink is open.".to_string(),
        ),
    };
    rows.push(SetupRow {
        id: "autostart",
        icon: "desklink-restart-alt-symbolic",
        title: "Start at login",
        status,
        status_words,
        detail,
        fix: Fix::Switch(facts.start_at_login),
    });

    // Notifications.
    if let Some(check) = find("notifications") {
        let (status, status_words) = status_of(check.level);
        rows.push(SetupRow {
            id: "notifications",
            icon: "desklink-notifications-symbolic",
            title: "Notifications",
            status,
            status_words,
            detail: if check.level == Level::Ok {
                "Your phone's notifications and DeskLink's alerts show here.".to_string()
            } else {
                with_install(check)
            },
            fix: if check.level == Level::Ok {
                Fix::TestNotification
            } else {
                copy_or_none(check)
            },
        });
    }

    // The top bar (GNOME only).
    if let Some(extension) = &facts.extension {
        let (status, status_words, detail, fix) = match extension {
            ExtensionStatus::Active => (
                Status::Ok,
                "On",
                "Your phone's battery, connection and quick actions are in the top bar.",
                Fix::None,
            ),
            ExtensionStatus::NeedsLogin => (
                Status::Warn,
                "Needs a step",
                "Installed. GNOME shows it after you log out and back in.",
                Fix::LogOut,
            ),
            ExtensionStatus::Off => (
                Status::Idle,
                "Off",
                "Shows your phone's battery, connection and quick actions in the top bar. \
                 Turning it on also allows GNOME user extensions, and GNOME shows it after you \
                 log out and back in. Nothing changes until you click.",
                Fix::TurnOnExtension,
            ),
        };
        rows.push(SetupRow {
            id: "extension",
            icon: "desklink-devices-symbolic",
            title: "Top bar menu",
            status,
            status_words,
            detail: detail.to_string(),
            fix,
        });
    }

    // The tray (every other desktop): shown, or this desktop's own step.
    if let Some(check) = find("tray").filter(|_| !facts.gnome) {
        let shown = check.level == Level::Ok;
        rows.push(SetupRow {
            id: "tray",
            icon: "desklink-devices-symbolic",
            title: "Tray icon",
            status: if shown { Status::Ok } else { Status::Idle },
            status_words: if shown { "On" } else { "Not shown" },
            detail: if shown {
                "Your phone's battery, connection and quick actions are in the tray.".to_string()
            } else {
                check.detail.clone()
            },
            fix: Fix::None,
        });
    }

    // Screen sharing.
    if let Some(check) = find("share") {
        let (status, status_words) = status_of(check.level);
        let through_portal = check
            .detail
            .starts_with(crate::platform::SHARE_THROUGH_DESKTOP);
        let detail = match check.level {
            Level::Ok if through_portal && facts.gnome => {
                "The first time your phone asks to see or control this screen, GNOME shows a \
                 dialog here. Allow it, with Allow Remote Interaction turned on so the phone can \
                 control it. You are not asked again until DeskLink restarts."
                    .to_string()
            }
            Level::Ok if through_portal => {
                "The first time your phone asks to see or control this screen, your desktop \
                 shows a dialog here to allow it."
                    .to_string()
            }
            _ => with_install(check),
        };
        rows.push(SetupRow {
            id: "share",
            icon: "desklink-desktop-windows-symbolic",
            title: "Screen sharing",
            status,
            status_words,
            detail,
            fix: copy_or_none(check),
        });
    }

    // Glide.
    if let Some(check) = find("glide") {
        let (status, status_words) = status_of(check.level);
        let dialog = if facts.gnome {
            "GNOME's Capture Input dialog"
        } else {
            "a dialog to allow input capture"
        };
        rows.push(SetupRow {
            id: "glide",
            icon: "desklink-mouse-symbolic",
            title: "Glide",
            status,
            status_words,
            detail: if check.level == Level::Ok {
                format!(
                    "Move your mouse across to the phone, off an edge of this screen. Turn it on \
                     in Settings after pairing; it then shows {dialog}, and once you allow it \
                     you are asked again only after DeskLink restarts or the screen locks."
                )
            } else {
                format!("Move your mouse across to the phone. {}", check.detail)
            },
            fix: Fix::None,
        });
    }

    // Where the pairing keys live (no keyring is used).
    if let Some(check) = find("storage") {
        let (status, status_words) = status_of(check.level);
        rows.push(SetupRow {
            id: "keys",
            icon: "desklink-encrypted-symbolic",
            title: "Pairing keys",
            status,
            status_words,
            detail: if check.level == Level::Ok {
                "Kept in DeskLink's own folder, readable only by you. No keyring is needed."
                    .to_string()
            } else {
                check.detail.clone()
            },
            fix: Fix::None,
        });
    }

    // GStreamer: the connection, the phone's screen, sound.
    let parts: Vec<&Check> = ["link", "view", "sound"]
        .iter()
        .filter_map(|id| find(id))
        .collect();
    if !parts.is_empty() {
        let worst = parts
            .iter()
            .map(|check| check.level)
            .max_by_key(|level| match level {
                Level::Ok => 0,
                Level::Limited => 1,
                Level::Missing => 2,
            })
            .unwrap_or(Level::Ok);
        let (status, status_words) = status_of(worst);
        let mut commands: Vec<String> = Vec::new();
        for command in parts.iter().filter_map(|check| install(check)) {
            if !commands.contains(&command) {
                commands.push(command);
            }
        }
        let mut detail = if worst == Level::Ok {
            "Ready: this computer has what DeskLink needs for the connection, the phone's \
             screen and its sound."
                .to_string()
        } else {
            parts
                .iter()
                .filter(|check| check.level != Level::Ok)
                .map(|check| check.detail.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        if !commands.is_empty() {
            detail.push_str(&format!("\n\n{}", crate::window::INSTALL_HINT));
        }
        rows.push(SetupRow {
            id: "plugins",
            icon: "desklink-movie-symbolic",
            title: "Video and sound",
            status,
            status_words,
            detail,
            fix: if commands.is_empty() {
                Fix::None
            } else {
                Fix::Copy(commands.join("\n"))
            },
        });
    }
    rows
}
