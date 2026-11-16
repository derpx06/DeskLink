//! Which top-bar client said `Hello` and what it handles, and the check that
//! a caller is the Shell itself before any verb runs.
//!
//! Sibling edges: `extension` (whether it is active). Outside: gio D-Bus.

use super::*;

/// The top-bar client that said `Hello`: its bus name, what it handles, and
/// the watch that forgets it when it goes.
struct Client {
    capabilities: Vec<String>,
    /// Stops the watch (its id type is not nameable outside `gio`).
    unwatch: Box<dyn FnOnce()>,
}

thread_local! {
    static CLIENT: RefCell<Option<Client>> = const { RefCell::new(None) };
}

/// What a top-bar client may say it handles.
const KNOWN_CAPABILITIES: [&str; 3] = ["notifications", "quick-settings", "calls"];

/// Only what DeskLink knows, at most once each.
pub(super) fn vetted(capabilities: Vec<String>) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for capability in capabilities {
        if KNOWN_CAPABILITIES.contains(&capability.as_str()) && !kept.contains(&capability) {
            kept.push(capability);
        }
    }
    kept
}

/// Whether `sender` (a unique bus name) is the owner of `org.gnome.Shell`.
pub(super) fn is_the_shell(sender: &str, shell_owner: Option<&str>) -> bool {
    !sender.is_empty() && shell_owner == Some(sender)
}

/// Whether a method call came from the Shell. The one gate for every
/// top-bar verb; a test pins that no verb is reachable around it.
pub(super) fn is_the_shell_caller(connection: &gio::DBusConnection, sender: Option<&str>) -> bool {
    sender.is_some_and(|sender| hello_is_from_the_shell(connection, sender))
}

/// The Shell's bus name as last asked for, and when. A bus name is never reused,
/// so a cached owner that matches the caller is still right after any restart;
/// only a caller that does not match needs a fresh question.
#[derive(Default)]
pub(super) struct OwnerCache {
    asked: Option<std::time::Instant>,
    owner: Option<String>,
}

/// A fresh question about the owner is asked at most this often: the call blocks
/// the main loop (up to its 500 ms timeout), and any process of the same user may
/// call these verbs in a loop.
pub(super) const OWNER_ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

/// What the owner check came to.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Check {
    Shell,
    NotTheShell,
    /// Not asked: a question was asked within [`OWNER_ASK_EVERY`] (by anyone).
    /// Not an answer: a one-time `Hello` is tried again after the window.
    Throttled,
}

pub(super) fn check_the_shell(
    cache: &mut OwnerCache,
    now: std::time::Instant,
    sender: &str,
    ask: impl FnOnce() -> Option<String>,
) -> Check {
    if is_the_shell(sender, cache.owner.as_deref()) {
        return Check::Shell;
    }
    if cache
        .asked
        .is_some_and(|asked| now.duration_since(asked) < OWNER_ASK_EVERY)
    {
        return Check::Throttled;
    }
    cache.asked = Some(now);
    cache.owner = ask();
    if is_the_shell(sender, cache.owner.as_deref()) {
        Check::Shell
    } else {
        Check::NotTheShell
    }
}

#[cfg(test)]
pub(super) fn is_the_shell_cached(
    cache: &mut OwnerCache,
    now: std::time::Instant,
    sender: &str,
    ask: impl FnOnce() -> Option<String>,
) -> bool {
    check_the_shell(cache, now, sender, ask) == Check::Shell
}

pub(super) fn hello_is_from_the_shell(connection: &gio::DBusConnection, sender: &str) -> bool {
    check_hello_sender(connection, sender) == Check::Shell
}

thread_local! {
    static OWNER: std::cell::RefCell<OwnerCache> = std::cell::RefCell::new(OwnerCache::default());
}

/// The Shell's bus name, once it has been asked for and answered.
pub(super) fn known_shell_owner() -> Option<String> {
    OWNER.with(|cache| cache.borrow().owner.clone())
}

pub(super) fn check_hello_sender(connection: &gio::DBusConnection, sender: &str) -> Check {
    OWNER.with(|cache| {
        check_the_shell(
            &mut cache.borrow_mut(),
            std::time::Instant::now(),
            sender,
            || {
                connection
                    .call_sync(
                        Some("org.freedesktop.DBus"),
                        "/org/freedesktop/DBus",
                        "org.freedesktop.DBus",
                        "GetNameOwner",
                        Some(&("org.gnome.Shell",).to_variant()),
                        None,
                        gio::DBusCallFlags::NONE,
                        500,
                        gio::Cancellable::NONE,
                    )
                    .ok()
                    .and_then(|reply| reply.get::<(String,)>())
                    .map(|(owner,)| owner)
            },
        )
    })
}

/// A top-bar client (the extension, on enable) says what it handles. Kept
/// until its bus name goes away (Shell restart, extension disabled).
pub(super) fn client_said_hello(
    connection: &gio::DBusConnection,
    sender: &str,
    capabilities: Vec<String>,
) {
    let watch = gio::bus_watch_name_on_connection(
        connection,
        sender,
        gio::BusNameWatcherFlags::NONE,
        |_, _, _| {},
        |_, _| forget_client(),
    );
    let unwatch: Box<dyn FnOnce()> = Box::new(move || gio::bus_unwatch_name(watch));
    let previous = CLIENT.with(|client| {
        client.replace(Some(Client {
            capabilities,
            unwatch,
        }))
    });
    if let Some(previous) = previous {
        (previous.unwatch)();
    }
}

fn forget_client() {
    if let Some(previous) = CLIENT.with(|client| client.take()) {
        (previous.unwatch)();
    }
}

/// Whether a running top-bar client said it shows `capability` (e.g.
/// `notifications`). An older extension says nothing, so the app keeps
/// showing them itself until the new one is running.
pub fn client_handles(capability: &str) -> bool {
    let state = EXTENSION_KNOWN
        .with(|known| known.get())
        .then(|| EXTENSION_IS_ACTIVE.with(|active| active.get()));
    CLIENT.with(|client| {
        client
            .borrow()
            .as_ref()
            .is_some_and(|client| handles(&client.capabilities, capability, state))
    })
}

/// A client said it shows `capability`; but the extension may have been
/// turned off or crashed since (the bus name it said Hello from is the
/// Shell's, which stays), so a state known to be "not active" wins (I4).
pub(super) fn handles(
    capabilities: &[String],
    capability: &str,
    extension_active: Option<bool>,
) -> bool {
    extension_active != Some(false) && capabilities.iter().any(|known| known == capability)
}
