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
//!
//! Layout: this file holds the types ([`Check`], [`Probe`], packages), the
//! well-known bus names, the report store and the API the app calls; `rules`
//! is the pure [`evaluate`], `probe` reads the facts. No cycles.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use gtk::gio;
use gtk::glib::{self, prelude::*};

mod probe;
mod rules;
#[cfg(test)]
use probe::*;
#[cfg(test)]
use rules::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use rules::{evaluate, is_gnome_shell, x11_session};

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
/// Read once: Home asks on the GTK thread every tick, and it does not change
/// while DeskLink runs.
pub fn distro() -> Option<Distro> {
    static DISTRO: std::sync::OnceLock<Option<Distro>> = std::sync::OnceLock::new();
    *DISTRO.get_or_init(|| {
        ["/etc/os-release", "/usr/lib/os-release"]
            .iter()
            .find_map(|path| std::fs::read_to_string(path).ok())
            .and_then(|text| distro_from_os_release(&text))
    })
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
    /// Other programs listening for phones on DeskLink's IPv4 discovery port
    /// ([`discovery_sharers`]).
    pub discovery_sharers: Vec<String>,
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

/// Debug builds only (the screenshot harness): a machine missing what the
/// phone link needs, so the standing-problem banner shows.
#[cfg(debug_assertions)]
pub fn fake_missing_link() {
    store(evaluate(&Probe {
        config_writable: true,
        ..Probe::default()
    }));
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
const STICKY: [&str; 3] = ["link", "storage", "discovery"];

/// The standing problem to keep in front of the person, if any: its row id
/// its plain words, and the install command for this distribution apart (the
/// banner's "Copy command"), never in the words.
pub fn sticky_notice() -> Option<(&'static str, String, Option<String>)> {
    sticky_from(&report()?, distro())
}

fn sticky_from(
    rows: &[Check],
    distro: Option<Distro>,
) -> Option<(&'static str, String, Option<String>)> {
    STICKY.iter().find_map(|id| {
        let row = rows
            .iter()
            // Another program taking the phone's messages is only a limit,
            // but one the person would never find: it is shown too.
            .find(|row| {
                row.id == *id
                    && (row.level == Level::Missing
                        || (row.id == "discovery" && row.level == Level::Limited))
            })?;
        let command = row.install.map(|packages| packages.command(distro));
        Some((*id, row.detail.clone(), command))
    })
}

/// "A", "A and B", "A, B and C".
fn and_list(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

const NOTIFICATIONS: &str = "org.freedesktop.Notifications";

const WATCHER: &str = "org.kde.StatusNotifierWatcher";

const PORTAL: &str = "org.freedesktop.portal.Desktop";

pub(super) const LOGIND: &str = "org.freedesktop.login1";

/// DeskLink's UDP discovery port, also KDE Connect's and GSConnect's.
const DISCOVERY_PORT: u16 = crate::device_links::daemon::network::UDP_PORT;

#[cfg(test)]
mod tests;
