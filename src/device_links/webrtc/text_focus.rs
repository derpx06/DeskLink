//! Desktop text focus, read from the accessibility bus (AT-SPI), so the phone
//! controlling this desktop can raise its keyboard when a text input takes
//! focus — the way a phone's own text fields do — and drop it when one loses
//! focus.
//!
//! GNOME keeps accessibility events off until a client asks
//! (`org.a11y.Status.IsEnabled`); this turns it on while it watches and puts
//! the previous setting back when it stops. Applications already running when
//! it turns on may only start reporting after a restart (Chromium-based ones
//! read the setting at launch); GTK apps and terminals report at once.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;

use gtk::gio::prelude::*;
use gtk::{gio, glib};

const A11Y_BUS: &str = "org.a11y.Bus";
const A11Y_BUS_PATH: &str = "/org/a11y/bus";
const REGISTRY: &str = "org.a11y.atspi.Registry";
const REGISTRY_PATH: &str = "/org/a11y/atspi/registry";
const FOCUS_EVENT: &str = "object:state-changed:focused";
/// `ATSPI_STATE_EDITABLE`, in the first word of `GetState`'s bit set.
const STATE_EDITABLE: u32 = 7;

/// `AtspiRole` numbers of elements that take typing: password text, spin
/// button, terminal, text, editbar, entry, document text. Numbers, not
/// `GetRoleName`: GTK 4 names its text fields "text box", which no list of
/// AT-SPI names matched, so the phone keyboard never came up for them.
const TYPING_ROLES: [u32; 7] = [40, 52, 60, 61, 77, 79, 94];

/// Does an element with this role and state take typing? A terminal does,
/// though it reports itself as not "editable".
pub(crate) fn takes_typing(role: u32, state_low_word: u32) -> bool {
    state_low_word & (1 << STATE_EDITABLE) != 0 || TYPING_ROLES.contains(&role)
}

/// Watches until dropped.
pub(crate) struct TextFocusWatcher {
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<()>>,
}

impl TextFocusWatcher {
    /// Calls `on_change(editable)` whenever focus moves onto or off a text
    /// input. Failures (no accessibility bus) are logged, never fatal.
    pub(crate) fn start(on_change: impl Fn(bool) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("desklink-text-focus".into())
            .spawn(move || {
                if let Err(error) = watch(&flag, &on_change) {
                    eprintln!("[DeskLink] Desktop text-focus watching is unavailable: {error}");
                }
            })
            .ok();
        Self { stop, thread }
    }
}

impl Drop for TextFocusWatcher {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Puts `IsEnabled` back as it was, however the watch ends.
struct RestoreAccessibility {
    session: gio::DBusConnection,
    enabled_before: bool,
}

impl Drop for RestoreAccessibility {
    fn drop(&mut self) {
        if !self.enabled_before {
            let _ = set_accessibility(&self.session, false);
        }
    }
}

fn accessibility_enabled(session: &gio::DBusConnection) -> Result<bool, String> {
    let reply = session
        .call_sync(
            Some(A11Y_BUS),
            A11Y_BUS_PATH,
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.a11y.Status", "IsEnabled").to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2_000,
            None::<&gio::Cancellable>,
        )
        .map_err(|error| error.to_string())?;
    Ok(reply
        .child_value(0)
        .as_variant()
        .and_then(|value| value.get::<bool>())
        .unwrap_or(false))
}

fn set_accessibility(session: &gio::DBusConnection, enabled: bool) -> Result<(), String> {
    session
        .call_sync(
            Some(A11Y_BUS),
            A11Y_BUS_PATH,
            "org.freedesktop.DBus.Properties",
            "Set",
            Some(&("org.a11y.Status", "IsEnabled", enabled.to_variant()).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            2_000,
            None::<&gio::Cancellable>,
        )
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn watch(stop: &AtomicBool, on_change: &dyn Fn(bool)) -> Result<(), String> {
    let session = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|error| error.to_string())?;
    let _restore = RestoreAccessibility {
        enabled_before: accessibility_enabled(&session)?,
        session: session.clone(),
    };
    set_accessibility(&session, true)?;
    let address = session
        .call_sync(
            Some(A11Y_BUS),
            A11Y_BUS_PATH,
            A11Y_BUS,
            "GetAddress",
            None,
            None,
            gio::DBusCallFlags::NONE,
            2_000,
            None::<&gio::Cancellable>,
        )
        .map_err(|error| error.to_string())?
        .child_value(0)
        .get::<String>()
        .ok_or("the accessibility bus has no address")?;

    let context = glib::MainContext::new();
    context
        .with_thread_default(|| -> Result<(), String> {
            let a11y = gio::DBusConnection::for_address_sync(
                &address,
                gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
                    | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
                None,
                None::<&gio::Cancellable>,
            )
            .map_err(|error| error.to_string())?;
            let registry = |method: &str| {
                a11y.call_sync(
                    Some(REGISTRY),
                    REGISTRY_PATH,
                    REGISTRY,
                    method,
                    Some(&(FOCUS_EVENT, Vec::<String>::new(), "").to_variant()),
                    None,
                    gio::DBusCallFlags::NONE,
                    2_000,
                    None::<&gio::Cancellable>,
                )
            };
            registry("RegisterEvent").map_err(|error| error.to_string())?;

            let (focused, events) = mpsc::channel::<(String, String)>();
            let subscription = a11y.signal_subscribe(
                None,
                Some("org.a11y.atspi.Event.Object"),
                Some("StateChanged"),
                None,
                Some("focused"),
                gio::DBusSignalFlags::NONE,
                move |_, sender, path, _, _, parameters| {
                    let gained = parameters.child_value(1).get::<i32>() == Some(1);
                    if gained {
                        let _ = focused.send((sender.to_string(), path.to_string()));
                    }
                },
            );

            let mut reported: Option<bool> = None;
            while !stop.load(Ordering::Acquire) {
                while context.pending() {
                    context.iteration(false);
                }
                // Only the latest focus matters.
                if let Some((sender, path)) = events.try_iter().last() {
                    let editable = element_takes_typing(&a11y, &sender, &path);
                    if reported != Some(editable) {
                        reported = Some(editable);
                        on_change(editable);
                    }
                }
                thread::sleep(Duration::from_millis(40));
            }
            a11y.signal_unsubscribe(subscription);
            let _ = registry("DeregisterEvent");
            Ok(())
        })
        .map_err(|error| error.to_string())?
}

fn element_takes_typing(a11y: &gio::DBusConnection, sender: &str, path: &str) -> bool {
    let call = |method: &str| {
        a11y.call_sync(
            Some(sender),
            path,
            "org.a11y.atspi.Accessible",
            method,
            None,
            None,
            gio::DBusCallFlags::NONE,
            300,
            None::<&gio::Cancellable>,
        )
        .ok()
    };
    let state = call("GetState")
        .and_then(|reply| reply.child_value(0).get::<Vec<u32>>())
        .and_then(|words| words.first().copied())
        .unwrap_or(0);
    let role = call("GetRole")
        .and_then(|reply| reply.child_value(0).get::<u32>())
        .unwrap_or(0);
    takes_typing(role, state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_inputs_and_terminals_take_typing_and_controls_do_not() {
        // Roles as GTK 4 apps report them (measured: Text Editor 61, Console 60).
        assert!(takes_typing(79, 0), "entry");
        assert!(takes_typing(61, 0), "a GTK 4 \"text box\"");
        assert!(
            takes_typing(60, 0),
            "a terminal is not 'editable' but takes typing"
        );
        assert!(
            takes_typing(95, 1 << STATE_EDITABLE),
            "a contenteditable page"
        );
        assert!(!takes_typing(43, 0), "push button");
        assert!(!takes_typing(55, 0), "Files' grid");
        assert!(!takes_typing(23, 1 << 3), "window frame");
    }
}
