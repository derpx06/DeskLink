//! "Stay awake": keeping this computer reachable from the phone.
//!
//! A sleeping computer answers nothing, and on a laptop closing the lid is the
//! most common way to put it to sleep. logind's `block` inhibitor on
//! `sleep:idle:handle-lid-switch` stops all three: suspend requests, idle
//! sleep, and logind's own lid-switch action. It lasts exactly as long as we
//! hold the file descriptor logind hands back, so releasing it is a `drop`,
//! and a crash can never leave the machine unable to sleep.

use std::os::fd::{FromRawFd, OwnedFd};

use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib::variant::ToVariant;

const WHAT: &str = "sleep:idle:handle-lid-switch";
const WHY: &str = "Staying reachable from your phone";

/// Holds the inhibitor while it is wanted.
#[derive(Default)]
pub(super) struct StayAwake {
    held: Option<OwnedFd>,
    /// The last failure, kept so a refusal is reported once rather than every
    /// second, and retried only when the wish changes.
    failed: bool,
}

impl StayAwake {
    /// Makes the inhibitor match `wanted`. Returns an error only on the first
    /// failure after the wish became true.
    pub(super) fn apply(&mut self, wanted: bool) -> Result<(), String> {
        if !wanted {
            self.held = None;
            self.failed = false;
            return Ok(());
        }
        if self.held.is_some() || self.failed {
            return Ok(());
        }
        match inhibit() {
            Ok(fd) => {
                self.held = Some(fd);
                Ok(())
            }
            Err(error) => {
                self.failed = true;
                Err(format!("Could not keep this computer awake: {error}"))
            }
        }
    }
}

fn inhibit() -> Result<OwnedFd, String> {
    let bus = gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>)
        .map_err(|error| error.to_string())?;
    let (reply, fds) = bus
        .call_with_unix_fd_list_sync(
            Some("org.freedesktop.login1"),
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            "Inhibit",
            Some(&(WHAT, "DeskLink", WHY, "block").to_variant()),
            Some(gtk::glib::VariantTy::new("(h)").map_err(|error| error.to_string())?),
            gio::DBusCallFlags::NONE,
            5000,
            None::<&gio::UnixFDList>,
            None::<&gio::Cancellable>,
        )
        .map_err(|error| error.to_string())?;
    let index = reply
        .try_child_value(0)
        .and_then(|handle| handle.get::<gtk::glib::variant::Handle>())
        .ok_or("logind answered without a handle")?;
    let fd = fds
        .ok_or("logind answered without a file descriptor")?
        .get(index.0)
        .map_err(|error| error.to_string())?;
    // SAFETY: `UnixFDList::get` returns a fresh duplicate owned by the caller.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
