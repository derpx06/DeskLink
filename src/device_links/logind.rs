//! This user's graphical session, from logind over D-Bus: its lock state,
//! and locking or unlocking it.
//!
//! DeskLink is usually D-Bus activated, so it runs in a `dbus-…service` cgroup
//! with no `XDG_SESSION_ID`. The session is therefore found the way logind
//! defines it for a user: `org.freedesktop.login1.User.Display` of our own
//! UID. Only that session is ever touched.

use gtk::gio;
use gtk::glib::{self, prelude::*};

const LOGIND: &str = "org.freedesktop.login1";
const PROPERTIES: &str = "org.freedesktop.DBus.Properties";
const SESSION: &str = "org.freedesktop.login1.Session";
/// Every call is bounded: logind wedges around suspend and restarts.
const TIMEOUT_MS: i32 = 2_000;

fn system_bus() -> Result<gio::DBusConnection, String> {
    gio::bus_get_sync(gio::BusType::System, gio::Cancellable::NONE)
        .map_err(|error| error.to_string())
}

fn call(
    bus: &gio::DBusConnection,
    path: &str,
    interface: &str,
    method: &str,
    arguments: Option<&glib::Variant>,
) -> Result<glib::Variant, String> {
    bus.call_sync(
        Some(LOGIND),
        path,
        interface,
        method,
        arguments,
        None,
        gio::DBusCallFlags::NONE,
        TIMEOUT_MS,
        gio::Cancellable::NONE,
    )
    .map_err(|error| error.to_string())
}

fn property(
    bus: &gio::DBusConnection,
    path: &str,
    interface: &str,
    name: &str,
) -> Result<glib::Variant, String> {
    call(
        bus,
        path,
        PROPERTIES,
        "Get",
        Some(&(interface, name).to_variant()),
    )?
    .child_value(0)
    .as_variant()
    .ok_or_else(|| format!("logind sent no {name}"))
}

/// The object path of our UID's display session.
fn display_session(bus: &gio::DBusConnection) -> Result<String, String> {
    let uid = unsafe { libc::getuid() };
    let user = call(
        bus,
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
        "GetUser",
        Some(&(uid,).to_variant()),
    )?
    .child_value(0);
    let user = user.str().ok_or("logind sent no user path")?.to_string();
    // `(so)`: the session id and its object path; "/" when there is none.
    let display = property(bus, &user, "org.freedesktop.login1.User", "Display")?;
    let path = display
        .try_child_value(1)
        .and_then(|path| path.str().map(str::to_string))
        .filter(|path| path != "/")
        .ok_or("This user has no graphical session")?;
    Ok(path)
}

/// Our display session's object path (for its PropertiesChanged).
pub(crate) fn display_session_path() -> Option<String> {
    display_session(&system_bus().ok()?).ok()
}

/// Whether our display session is locked; `None` when logind cannot say.
pub(crate) fn locked() -> Option<bool> {
    let bus = system_bus().ok()?;
    let session = display_session(&bus).ok()?;
    property(&bus, &session, SESSION, "LockedHint")
        .ok()?
        .get::<bool>()
}

/// Asks logind to lock our display session.
pub(crate) fn lock() -> Result<(), String> {
    let bus = system_bus()?;
    let session = display_session(&bus)?;
    call(&bus, &session, SESSION, "Lock", None).map(|_| ())
}

/// Asks logind to unlock our display session (allowed for its own user).
pub(crate) fn unlock() -> Result<(), String> {
    let bus = system_bus()?;
    let session = display_session(&bus)?;
    call(&bus, &session, SESSION, "Unlock", None).map(|_| ())
}
