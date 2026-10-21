//! "Stay awake": keeping this computer reachable from the phone.
//!
//! A sleeping computer answers nothing, and on a laptop closing the lid is the
//! most common way to put it to sleep. logind's `block` inhibitor on
//! `sleep:idle:handle-lid-switch` stops all three: suspend requests, idle
//! sleep, and logind's own lid-switch action. It lasts exactly as long as we
//! hold the file descriptor logind hands back, so releasing it is a `drop`,
//! and a crash can never leave the machine unable to sleep.

use std::os::fd::{FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

use gtk::gio;
use gtk::gio::prelude::*;
use gtk::glib::variant::ToVariant;

const WHAT: &str = "sleep:idle:handle-lid-switch";
const WHY: &str = "Staying reachable from your phone";

/// How long to wait before asking logind again after a refusal.
const RETRY_AFTER: Duration = Duration::from_secs(10);

/// Holds the inhibitor while it is wanted.
#[derive(Default)]
pub(super) struct StayAwake {
    held: Option<OwnedFd>,
    /// When to ask again after logind refused. A refusal is not final: asking
    /// while the machine is already going to sleep fails with
    /// `OperationInProgress`, and giving up then left it unprotected after
    /// it woke.
    retry_at: Option<Instant>,
    /// A refusal is reported once, not every retry.
    reported: bool,
}

impl StayAwake {
    /// Makes the inhibitor match `wanted`. Returns an error only for the first
    /// lasting refusal after the wish became true.
    pub(super) fn apply(&mut self, wanted: bool) -> Result<(), String> {
        self.apply_with(wanted, Instant::now(), inhibit)
    }

    fn apply_with(
        &mut self,
        wanted: bool,
        now: Instant,
        acquire: impl FnOnce() -> Result<OwnedFd, String>,
    ) -> Result<(), String> {
        if !wanted {
            *self = Self::default();
            return Ok(());
        }
        if self.held.is_some() || self.retry_at.is_some_and(|at| now < at) {
            return Ok(());
        }
        match acquire() {
            Ok(fd) => {
                *self = Self {
                    held: Some(fd),
                    ..Self::default()
                };
                Ok(())
            }
            Err(error) => {
                self.retry_at = Some(now + RETRY_AFTER);
                // Suspend already under way: the next try after waking works.
                if error.contains("OperationInProgress") || self.reported {
                    return Ok(());
                }
                self.reported = true;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn fd() -> Result<OwnedFd, String> {
        Ok(std::fs::File::open("/dev/null").unwrap().into())
    }

    #[test]
    fn a_refusal_is_retried_and_reported_once() {
        let mut awake = StayAwake::default();
        let start = Instant::now();
        let refused = || Err("org.freedesktop.DBus.Error.AccessDenied".to_string());

        assert!(awake.apply_with(true, start, refused).is_err());
        assert!(awake
            .apply_with(true, start, || panic!("asked again too soon"))
            .is_ok());
        assert!(awake.apply_with(true, start + RETRY_AFTER, refused).is_ok());
        assert!(awake.apply_with(true, start + RETRY_AFTER * 2, fd).is_ok());
        assert!(awake.held.is_some());
    }

    #[test]
    fn asking_during_a_suspend_is_silent_and_retried() {
        let mut awake = StayAwake::default();
        let start = Instant::now();
        let busy = || Err("GDBus.Error:org.freedesktop.login1.OperationInProgress".to_string());

        assert!(awake.apply_with(true, start, busy).is_ok());
        assert!(awake.apply_with(true, start + RETRY_AFTER, fd).is_ok());
        assert!(awake.held.is_some());
        assert!(awake.apply_with(false, start + RETRY_AFTER, fd).is_ok());
        assert!(awake.held.is_none());
    }
}
