//! Reading the facts from this system ([`Probe::real`]): the session bus and
//! its names, the portal's interfaces, sockets and variables, GStreamer
//! elements, other programs on the discovery port, and writable folders. The
//! only part of the check that touches the system.
//!
//! Sibling edges: none (the parent's types and bus names). Outside: gio
//! D-Bus, GStreamer's registry, `/proc`, `daemon::network`.

use super::*;

/// Whether a D-Bus error name is a definite "no such thing" (the object
/// answered and does not have it), as opposed to a timeout or a service that
/// is still starting.
pub(super) fn definite_negative(remote_error: &str) -> bool {
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
pub(super) fn settle_names(
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
pub(super) fn session_bus_reachable(address_set: bool, runtime_bus_socket: bool) -> bool {
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
        probe.discovery_sharers = discovery_sharers();
        probe
    }
}

/// DeskLink's UDP discovery port, also KDE Connect's and GSConnect's.
pub(super) const DISCOVERY_PORT: u16 = crate::device_links::daemon::network::UDP_PORT;

/// The programs other than DeskLink with an IPv4 UDP socket on the
/// discovery port. Both sides set `SO_REUSEADDR`, so the bind succeeds, and
/// the kernel then gives each unicast datagram (a phone's beacon to the
/// address it last reached this computer on) to one of the sockets only.
/// Broadcasts reach all of them, and a dual-stack IPv6 socket (GSConnect's,
/// KDE Connect's) loses IPv4 unicast to DeskLink's IPv4 one, so only IPv4
/// listeners are counted.
pub(super) fn discovery_sharers() -> Vec<String> {
    let table = std::fs::read_to_string("/proc/net/udp").unwrap_or_default();
    let own_exe = std::fs::read_link("/proc/self/exe").ok();
    holders(
        &udp4_inodes(&table, DISCOVERY_PORT),
        std::process::id(),
        own_exe.as_deref(),
    )
}

/// Whether a holder is this DeskLink: this process, or another process of
/// the same program (`desklink --check` asks while the app runs: the app's
/// own socket is not a rival, F45). A replaced binary reads "(deleted)".
pub(super) fn is_this_app(
    pid: u32,
    own_pid: u32,
    exe: Option<&std::path::Path>,
    own_exe: Option<&std::path::Path>,
) -> bool {
    let plain = |path: &std::path::Path| {
        let text = path.to_string_lossy();
        text.strip_suffix(" (deleted)").unwrap_or(&text).to_string()
    };
    pid == own_pid || matches!((exe, own_exe), (Some(a), Some(b)) if plain(a) == plain(b))
}

/// The inodes of the sockets bound to `port` in `/proc/net/udp` text.
pub(super) fn udp4_inodes(table: &str, port: u16) -> Vec<u64> {
    let suffix = format!(":{port:04X}");
    table
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if !fields.get(1)?.ends_with(&suffix) {
                return None;
            }
            fields.get(9)?.parse().ok().filter(|inode| *inode != 0)
        })
        .collect()
}

/// Who holds each socket in `inodes`, by name, leaving out this DeskLink
/// (`is_this_app`); a socket whose holder cannot be seen (another user's
/// process) is "Another program".
pub(super) fn holders(
    inodes: &[u64],
    own_pid: u32,
    own_exe: Option<&std::path::Path>,
) -> Vec<String> {
    let mut names = Vec::new();
    if inodes.is_empty() {
        return names;
    }
    let mut seen = Vec::new();
    for entry in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = std::fs::read_link(fd.path()) else {
                continue;
            };
            let Some(inode) = target
                .to_str()
                .and_then(|t| t.strip_prefix("socket:["))
                .and_then(|t| t.strip_suffix(']'))
                .and_then(|t| t.parse::<u64>().ok())
                .filter(|inode| inodes.contains(inode))
            else {
                continue;
            };
            seen.push(inode);
            let exe = std::fs::read_link(entry.path().join("exe")).ok();
            if !is_this_app(pid, own_pid, exe.as_deref(), own_exe) {
                let read = |file: &str| std::fs::read(entry.path().join(file)).unwrap_or_default();
                names.push(app_name(
                    String::from_utf8_lossy(&read("comm")).trim(),
                    &String::from_utf8_lossy(&read("cmdline")).replace('\0', " "),
                ));
            }
        }
    }
    if inodes.iter().any(|inode| !seen.contains(inode)) {
        names.push("Another program".to_string());
    }
    names.sort();
    names.dedup();
    names
}

/// A process as the person knows it: the KDE Connect family by its name,
/// anything else by its command name.
pub(super) fn app_name(comm: &str, cmdline: &str) -> String {
    let cmdline = cmdline.to_ascii_lowercase();
    if comm == "kdeconnectd" {
        "KDE Connect".into()
    } else if cmdline.contains("gsconnect") {
        "GSConnect".into()
    } else if cmdline.contains("zorin-connect") || cmdline.contains("zorin_connect") {
        "Zorin Connect".into()
    } else if comm == "valent" {
        "Valent".into()
    } else if comm.is_empty() {
        "Another program".into()
    } else {
        comm.to_string()
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

pub(super) fn writable_below(path: &std::path::Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let Some(existing) = path.ancestors().find(|dir| dir.is_dir()) else {
        return false;
    };
    std::ffi::CString::new(existing.as_os_str().as_bytes())
        .is_ok_and(|path| unsafe { libc::access(path.as_ptr(), libc::W_OK) } == 0)
}

/// The elements the probe asks GStreamer about.
pub(super) const WATCHED_ELEMENTS: [&str; 17] = [
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
