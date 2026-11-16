//! Owning and watching a D-Bus name on a connection without aborting when
//! the connection closes.
//!
//! gio-rs 0.20's `bus_own_name_on_connection` and
//! `bus_watch_name_on_connection` unwrap the connection their closures are
//! given, but GLib passes NULL to "name lost" and "name vanished" once the
//! connection is gone (the session ends, the bus restarts). That unwrap is a
//! panic inside a GLib closure, which cannot unwind: the whole app aborts
//! with a core dump (seen every time a test desk's private bus shut down).
//! These go to the same GLib calls with closures that read the connection
//! as `Option`.
//!
//! May depend on: gio, glib.

use gtk::gio;
use gtk::glib;
use gtk::glib::translate::{IntoGlib, ToGlibPtr};

/// A name owned or watched with [`own_name`] / [`watch_name`]; `release`
/// gives it up.
#[derive(Debug)]
pub(crate) struct NameHandle {
    id: u32,
    watch: bool,
}

impl NameHandle {
    pub(crate) fn release(self) {
        // SAFETY: `id` came from the matching g_bus_*_with_closures call and
        // is released once (self is consumed).
        unsafe {
            if self.watch {
                gio::ffi::g_bus_unwatch_name(self.id);
            } else {
                gio::ffi::g_bus_unown_name(self.id);
            }
        }
    }
}

/// A closure for GLib's (connection, name, …) callbacks that never unwraps
/// the connection: `f` gets the connection if there still is one, and the
/// name.
fn closure(f: impl Fn(Option<gio::DBusConnection>, &str) + 'static) -> glib::Closure {
    glib::Closure::new_local(move |args| {
        let connection = args
            .first()
            .and_then(|value| value.get::<Option<gio::DBusConnection>>().ok())
            .flatten();
        let name = args
            .get(1)
            .and_then(|value| value.get::<&str>().ok())
            .unwrap_or_default();
        f(connection, name);
        None
    })
}

/// Owns `name` on `connection`; `lost` runs when it could not be owned or
/// was lost (also when the connection closed: then without a connection).
pub(crate) fn own_name(
    connection: &gio::DBusConnection,
    name: &str,
    lost: impl Fn(&str) + 'static,
) -> NameHandle {
    // SAFETY: valid connection and name pointers for the call; the closures
    // are referenced by GLib for as long as it needs them.
    let id = unsafe {
        gio::ffi::g_bus_own_name_on_connection_with_closures(
            connection.to_glib_none().0,
            name.to_glib_none().0,
            gio::BusNameOwnerFlags::NONE.into_glib(),
            closure(|_, _| {}).to_glib_none().0,
            closure(move |_, name| lost(name)).to_glib_none().0,
        )
    };
    NameHandle { id, watch: false }
}

/// Watches `name` on `connection`: `appeared` with the connection when it
/// has an owner, `vanished` when it has none (also when the connection
/// closed).
pub(crate) fn watch_name(
    connection: &gio::DBusConnection,
    name: &str,
    appeared: impl Fn(&gio::DBusConnection) + 'static,
    vanished: impl Fn() + 'static,
) -> NameHandle {
    // SAFETY: as in `own_name`.
    let id = unsafe {
        gio::ffi::g_bus_watch_name_on_connection_with_closures(
            connection.to_glib_none().0,
            name.to_glib_none().0,
            gio::BusNameWatcherFlags::NONE.into_glib(),
            closure(move |connection, _| {
                if let Some(connection) = connection {
                    appeared(&connection);
                }
            })
            .to_glib_none()
            .0,
            closure(move |_, _| vanished()).to_glib_none().0,
        )
    };
    NameHandle { id, watch: true }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gtk::prelude::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// GLib's "name lost" after the connection closed: a NULL connection.
    /// gio-rs unwrapped it and the app aborted; this closure takes it.
    #[test]
    fn a_lost_name_with_no_connection_is_not_a_panic() {
        let seen = Rc::new(RefCell::new(Vec::new()));
        let record = seen.clone();
        let lost = closure(move |connection, name| {
            record
                .borrow_mut()
                .push((connection.is_some(), name.to_string()));
        });
        // SAFETY: the closure takes the two values GLib passes, by type.
        unsafe {
            lost.invoke_with_values(
                glib::Type::UNIT,
                &[
                    None::<gio::DBusConnection>.to_value(),
                    "derx06.desklink.test".to_value(),
                ],
            );
        }
        assert_eq!(
            *seen.borrow(),
            [(false, "derx06.desklink.test".to_string())]
        );
    }
}
