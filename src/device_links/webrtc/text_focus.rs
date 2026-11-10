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

            // Focus that was already somewhere when control began sends no
            // event: a terminal focused before the phone took over never
            // raised the keyboard. Report where it is now, then follow events.
            let mut reported: Option<Reported> = None;
            // An element whose query timed out, asked again shortly.
            let mut retry: Option<(String, String, u32, std::time::Instant)> = None;
            let look = |sender: &str, path: &str, tries: u32, reported: &mut Option<Reported>| {
                let element = format!("{sender}{path}");
                match element_takes_typing(&a11y, sender, path) {
                    Some(editable) => {
                        let now = std::time::Instant::now();
                        if should_report(reported.as_ref(), &element, editable, now) {
                            on_change(editable);
                            *reported = Some(Reported {
                                element,
                                editable,
                                at: now,
                            });
                        }
                        None
                    }
                    None if tries < FOCUS_RETRIES => Some((
                        sender.to_string(),
                        path.to_string(),
                        tries + 1,
                        std::time::Instant::now() + FOCUS_RETRY_AFTER,
                    )),
                    None => None,
                }
            };
            if let Some((sender, path)) = current_focus(&a11y) {
                retry = look(&sender, &path, 0, &mut reported);
            }
            while !stop.load(Ordering::Acquire) {
                while context.pending() {
                    context.iteration(false);
                }
                // Only the latest focus matters; a newer one replaces a retry.
                if let Some((sender, path)) = events.try_iter().last() {
                    retry = look(&sender, &path, 0, &mut reported);
                } else if let Some((sender, path, tries, at)) = retry.take() {
                    if std::time::Instant::now() >= at {
                        retry = look(&sender, &path, tries, &mut reported);
                    } else {
                        retry = Some((sender, path, tries, at));
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

/// `ATSPI_STATE_ACTIVE` and `ATSPI_STATE_FOCUSED`, in `GetState`'s first word.
const STATE_ACTIVE: u32 = 1;
const STATE_FOCUSED: u32 = 12;
/// Upper bound on elements visited looking for the current focus, so a huge
/// tree (a long document, a big list) cannot stall the watcher.
const FOCUS_SEARCH_LIMIT: usize = 3_000;

/// The focused element of the active window, found by walking the
/// accessibility tree: AT-SPI has no call that returns it directly.
fn current_focus(a11y: &gio::DBusConnection) -> Option<(String, String)> {
    let root = (
        REGISTRY.to_string(),
        "/org/a11y/atspi/accessible/root".to_string(),
    );
    let windows: Vec<_> = children(a11y, &root.0, &root.1)
        .into_iter()
        .flat_map(|(app, path)| children(a11y, &app, &path))
        .filter(|(sender, path)| state_word(a11y, sender, path) & (1 << STATE_ACTIVE) != 0)
        .collect();
    let mut pending = windows;
    let mut visited = 0;
    while let Some((sender, path)) = pending.pop() {
        visited += 1;
        if visited > FOCUS_SEARCH_LIMIT {
            return None;
        }
        if state_word(a11y, &sender, &path) & (1 << STATE_FOCUSED) != 0 {
            return Some((sender, path));
        }
        pending.extend(children(a11y, &sender, &path));
    }
    None
}

fn children(a11y: &gio::DBusConnection, sender: &str, path: &str) -> Vec<(String, String)> {
    a11y.call_sync(
        Some(sender),
        path,
        "org.a11y.atspi.Accessible",
        "GetChildren",
        None,
        None,
        gio::DBusCallFlags::NONE,
        300,
        None::<&gio::Cancellable>,
    )
    .ok()
    .and_then(|reply| {
        reply
            .child_value(0)
            .get::<Vec<(String, glib::variant::ObjectPath)>>()
    })
    .map(|list| {
        list.into_iter()
            .map(|(sender, path)| (sender, path.as_str().to_string()))
            .collect()
    })
    .unwrap_or_default()
}

fn state_word(a11y: &gio::DBusConnection, sender: &str, path: &str) -> u32 {
    state_of(a11y, sender, path, 300).unwrap_or(0)
}

/// The last report sent to the phone: which element, what, when.
pub(crate) struct Reported {
    element: String,
    editable: bool,
    at: std::time::Instant,
}

/// A burst of focus events for one element (a terminal sends three or four)
/// is told once; after this, the same field focused again is told again.
const SAME_FOCUS_WINDOW: Duration = Duration::from_secs(1);
/// Times an element whose query timed out is asked again, and how soon. An
/// application busy drawing (a terminal printing fast) let `GetRole` time
/// out, which used to read as "takes no typing".
const FOCUS_RETRIES: u32 = 2;
const FOCUS_RETRY_AFTER: Duration = Duration::from_millis(250);

/// Whether a focus on `element` (taking typing or not) is told to the phone.
/// Every text field that takes focus is told, even when the last report was
/// also "typing": the phone may have put its keyboard away itself (Back),
/// and it must come back for the next field. Only a burst of events for the
/// same element is told once, and "no typing" only when it changes.
pub(crate) fn should_report(
    last: Option<&Reported>,
    element: &str,
    editable: bool,
    now: std::time::Instant,
) -> bool {
    let Some(last) = last else {
        return true;
    };
    if !editable {
        return last.editable;
    }
    !last.editable
        || last.element != element
        || now.saturating_duration_since(last.at) >= SAME_FOCUS_WINDOW
}

/// How long the focused element itself may take to answer.
const ELEMENT_TIMEOUT_MS: i32 = 1_000;

fn state_of(a11y: &gio::DBusConnection, sender: &str, path: &str, timeout_ms: i32) -> Option<u32> {
    a11y.call_sync(
        Some(sender),
        path,
        "org.a11y.atspi.Accessible",
        "GetState",
        None,
        None,
        gio::DBusCallFlags::NONE,
        timeout_ms,
        None::<&gio::Cancellable>,
    )
    .ok()
    .and_then(|reply| reply.child_value(0).get::<Vec<u32>>())
    .and_then(|words| words.first().copied())
}

/// Whether the element takes typing; `None` when it did not answer.
fn element_takes_typing(a11y: &gio::DBusConnection, sender: &str, path: &str) -> Option<bool> {
    let role = role(a11y, sender, path);
    let state = state_of(a11y, sender, path, ELEMENT_TIMEOUT_MS);
    let typing = role
        .zip(state)
        .map(|(role, state)| takes_typing(role, state));
    debug_log!("[DeskLink] Text focus: {sender}{path} role {role:?}, takes typing {typing:?}");
    typing
}

fn role(a11y: &gio::DBusConnection, sender: &str, path: &str) -> Option<u32> {
    a11y.call_sync(
        Some(sender),
        path,
        "org.a11y.atspi.Accessible",
        "GetRole",
        None,
        None,
        gio::DBusCallFlags::NONE,
        ELEMENT_TIMEOUT_MS,
        None::<&gio::Cancellable>,
    )
    .ok()
    .and_then(|reply| reply.child_value(0).get::<u32>())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_text_field_that_takes_focus_raises_the_keyboard_again() {
        let t0 = std::time::Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let typing = |element: &str, ms: u64| Reported {
            element: element.to_string(),
            editable: true,
            at: at(ms),
        };
        assert!(should_report(None, "app/a", true, t0), "first focus");
        assert!(should_report(None, "app/a", false, t0), "first focus");
        // The phone may have put its keyboard away (Back): the next field
        // still raises it.
        assert!(should_report(
            Some(&typing("app/a", 0)),
            "app/b",
            true,
            at(100)
        ));
        // A terminal's burst of focus events for one element is told once ...
        assert!(!should_report(
            Some(&typing("app/a", 0)),
            "app/a",
            true,
            at(100)
        ));
        // ... but the same field focused again later is told again.
        assert!(should_report(
            Some(&typing("app/a", 0)),
            "app/a",
            true,
            at(1_500)
        ));
        // Leaving typing is told once.
        assert!(should_report(
            Some(&typing("app/a", 0)),
            "app/c",
            false,
            at(100)
        ));
        let away = Reported {
            element: "app/c".to_string(),
            editable: false,
            at: at(100),
        };
        assert!(!should_report(Some(&away), "app/d", false, at(200)));
        assert!(should_report(Some(&away), "app/a", true, at(200)));
    }

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
