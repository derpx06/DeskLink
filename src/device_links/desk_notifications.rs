//! This computer's notifications on the phone (contract C1; opt-in, only the
//! apps the user picked in Preferences).
//!
//! The source works on any desktop: a monitor on the session bus (its own
//! connection, `BecomeMonitor`) sees every `org.freedesktop.Notifications`
//! `Notify` and every `org.gtk.Notifications` `AddNotification` (GNotification
//! and the portal, so Flatpak apps too), and their closing. Nothing is
//! installed and no desktop-specific hook is needed.
//!
//! Sent desk → phone as `desklink.notification` with ids `desk:<n>`, under
//! the Phase 1 snapshot rules: on every feature-ready `{snapshot: true, ids}`
//! then each body with `silent: true`; closed here → `isCancel`. The phone
//! swiping one away sends `desklink.notification.request {cancel: id}`, which
//! closes it here. DeskLink's own notifications (the phone's, mirrored) are
//! never forwarded.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use gtk::gio;
use gtk::glib::{self, prelude::*};

use super::packet::{NetworkPacket, PACKET_TYPE_NOTIFICATION};

const APP_ID: &str = "derx06.desklink.com";
const ID_PREFIX: &str = "desk:";
/// Notifications kept (the oldest goes first).
const KEPT: usize = 50;
const MAX_TITLE: usize = 256;
const MAX_TEXT: usize = 4096;

/// Who closes a notification here.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Origin {
    /// `org.freedesktop.Notifications`: the server's id, once its reply is
    /// seen (0 before).
    Fdo { sender: String, server_id: u32 },
    /// `org.gtk.Notifications`: the app and its own id.
    Gtk { app_id: String, id: String },
}

/// One of this computer's notifications.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeskNote {
    pub(crate) id: String,
    /// What the allowlist is keyed by: the app id or desktop entry, else the
    /// app's name.
    pub(crate) app_key: String,
    pub(crate) app_name: String,
    pub(crate) title: String,
    pub(crate) text: String,
    /// Unix milliseconds.
    pub(crate) time: i64,
    origin: Origin,
}

#[derive(Default)]
struct State {
    notes: Vec<DeskNote>,
    next: u64,
    /// `Notify` calls waiting for their reply: (sender, serial) → desk id.
    pending: HashMap<(String, u32), String>,
    /// Apps seen since start, for Preferences: key → name.
    seen: BTreeMap<String, String>,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn with_state<R>(run: impl FnOnce(&mut State) -> R) -> Option<R> {
    STATE
        .lock()
        .ok()
        .map(|mut state| run(state.get_or_insert_with(State::default)))
}

/// Apps whose notifications appeared since DeskLink started, key → name.
pub(crate) fn seen_apps() -> BTreeMap<String, String> {
    with_state(|state| state.seen.clone()).unwrap_or_default()
}

/// Plain text: markup (`<b>`, `<a href>`) and entities removed, capped.
fn plain(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in text.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    let out = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&");
    out.trim().chars().take(max).collect()
}

fn now_ms() -> i64 {
    glib::real_time() / 1000
}

/// Whether a notification is DeskLink's own (the phone's, mirrored here, or
/// DeskLink's alerts): never sent to the phone.
fn is_ours(app_key: &str, app_name: &str) -> bool {
    app_key == APP_ID || app_name.eq_ignore_ascii_case("DeskLink")
}

/// The note for `Notify(app_name, replaces_id, icon, summary, body, actions,
/// hints, timeout)`; `None` for DeskLink's own, transient ones and empty ones.
fn from_notify(args: &glib::Variant, sender: &str) -> Option<(DeskNote, u32)> {
    let app_name = args.try_child_value(0)?.str()?.to_string();
    let replaces = args.try_child_value(1)?.get::<u32>()?;
    let title = plain(args.try_child_value(3)?.str()?, MAX_TITLE);
    let text = plain(args.try_child_value(4)?.str()?, MAX_TEXT);
    let hints = glib::VariantDict::new(vardict(args, 6).as_ref());
    if hints.lookup::<bool>("transient").ok().flatten() == Some(true) {
        return None;
    }
    let app_key = hints
        .lookup::<String>("desktop-entry")
        .ok()
        .flatten()
        .filter(|entry| !entry.is_empty())
        .unwrap_or_else(|| app_name.clone());
    if is_ours(&app_key, &app_name) || (title.is_empty() && text.is_empty()) {
        return None;
    }
    let app_name = if app_name.is_empty() {
        app_key.clone()
    } else {
        app_name
    };
    Some((
        DeskNote {
            id: String::new(),
            app_key,
            app_name,
            title,
            text,
            time: now_ms(),
            origin: Origin::Fdo {
                sender: sender.to_string(),
                server_id: replaces,
            },
        },
        replaces,
    ))
}

/// Child `index` when it is an `a{sv}` (anything else would panic in
/// `VariantDict`; a peer can send any type).
fn vardict(args: &glib::Variant, index: usize) -> Option<glib::Variant> {
    args.try_child_value(index)
        .filter(|value| value.type_() == glib::VariantTy::VARDICT)
}

/// An installed app's name, from its desktop file.
fn app_display_name(app_id: &str) -> Option<String> {
    use gio::prelude::AppInfoExt;
    gio::DesktopAppInfo::new(&format!("{app_id}.desktop")).map(|info| info.name().to_string())
}

/// The note for `AddNotification(app_id, id, notification)`.
fn from_gtk(args: &glib::Variant, name_of: impl Fn(&str) -> Option<String>) -> Option<DeskNote> {
    let app_id = args.try_child_value(0)?.str()?.to_string();
    let id = args.try_child_value(1)?.str()?.to_string();
    let body = glib::VariantDict::new(vardict(args, 2).as_ref());
    let text = |key: &str| {
        body.lookup::<String>(key)
            .ok()
            .flatten()
            .unwrap_or_default()
    };
    let (title, text) = (
        plain(&text("title"), MAX_TITLE),
        plain(&text("body"), MAX_TEXT),
    );
    if is_ours(&app_id, "") || (title.is_empty() && text.is_empty()) {
        return None;
    }
    Some(DeskNote {
        id: String::new(),
        app_name: name_of(&app_id).unwrap_or_else(|| app_id.clone()),
        app_key: app_id.clone(),
        title,
        text,
        time: now_ms(),
        origin: Origin::Gtk { app_id, id },
    })
}

/// Adds `note`, or replaces the one it updates (same origin); returns the
/// stored copy with its `desk:` id.
fn upsert(state: &mut State, mut note: DeskNote) -> DeskNote {
    state
        .seen
        .insert(note.app_key.clone(), note.app_name.clone());
    let same = |known: &DeskNote| match (&known.origin, &note.origin) {
        (Origin::Gtk { .. }, Origin::Gtk { .. }) => known.origin == note.origin,
        (Origin::Fdo { server_id: a, .. }, Origin::Fdo { server_id: b, .. }) => *a != 0 && a == b,
        _ => false,
    };
    if let Some(known) = state.notes.iter_mut().find(|known| same(known)) {
        note.id = known.id.clone();
        *known = note.clone();
        return note;
    }
    state.next += 1;
    note.id = format!("{ID_PREFIX}{}", state.next);
    state.notes.push(note.clone());
    if state.notes.len() > KEPT {
        state.notes.remove(0);
    }
    note
}

/// The body sent to the phone (C1).
pub(crate) fn body_packet(note: &DeskNote, silent: bool) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION);
    packet.set("id", note.id.as_str());
    packet.set("appName", note.app_name.as_str());
    packet.set("title", note.title.as_str());
    packet.set("text", note.text.as_str());
    packet.set("time", note.time);
    packet.set("isClearable", true);
    packet.set("silent", silent);
    packet
}

fn cancel_packet(id: &str) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION);
    packet.set("id", id);
    packet.set("isCancel", true);
    packet
}

fn snapshot_packet(ids: &[String]) -> NetworkPacket {
    let mut packet = NetworkPacket::new(PACKET_TYPE_NOTIFICATION);
    packet.set("snapshot", true);
    packet.set("ids", ids.to_vec());
    packet
}

/// Whether `note` goes to phones now: the feature on and its app allowed.
fn allowed(settings: &super::config::AppSettings, note: &DeskNote) -> bool {
    settings.forward_notifications && settings.forward_apps.contains(&note.app_key)
}

fn to_phones(packet: NetworkPacket) {
    super::desk_features::run_later(move || {
        for device_id in super::desk_features::ready_devices() {
            if super::features::peer_accepts(&device_id, PACKET_TYPE_NOTIFICATION) {
                super::desk_features::send(&device_id, &packet);
            }
        }
    });
}

/// What the phone should hold now: the snapshot, then every allowed body.
fn snapshot_for(settings: &super::config::AppSettings) -> Vec<NetworkPacket> {
    let notes: Vec<DeskNote> = with_state(|state| state.notes.clone())
        .unwrap_or_default()
        .into_iter()
        .filter(|note| allowed(settings, note))
        .collect();
    let ids: Vec<String> = notes.iter().map(|note| note.id.clone()).collect();
    std::iter::once(snapshot_packet(&ids))
        .chain(notes.iter().map(|note| body_packet(note, true)))
        .collect()
}

/// On every feature-ready, and after the switch or the allowlist changed.
pub(crate) fn send_snapshot(device_id: Option<&str>) {
    let settings = super::config::app_settings();
    if settings.forward_notifications {
        start();
    }
    let packets = snapshot_for(&settings);
    let device_id = device_id.map(str::to_string);
    super::desk_features::run_later(move || {
        let devices = match device_id {
            Some(id) => vec![id],
            None => super::desk_features::ready_devices(),
        };
        for device_id in devices {
            if !super::features::peer_accepts(&device_id, PACKET_TYPE_NOTIFICATION) {
                continue;
            }
            for packet in &packets {
                super::desk_features::send(&device_id, packet);
            }
        }
    });
}

fn added(note: Option<DeskNote>) {
    if let Some(note) = note.and_then(|note| with_state(|state| upsert(state, note))) {
        forward(&note);
    }
}

/// Sends a new or changed notification to the phones, if its app is allowed.
fn forward(note: &DeskNote) {
    let allowed = allowed(&super::config::app_settings(), note);
    // App and id only: the text stays out of the journal.
    debug_log!(
        "[Notifications] {} from {}: {}",
        note.id,
        note.app_key,
        if allowed {
            "sent to the phones"
        } else {
            "not chosen, kept here"
        }
    );
    if allowed {
        to_phones(body_packet(note, false));
    }
}

fn removed(matches: impl Fn(&Origin) -> bool) {
    let gone = with_state(|state| {
        let gone: Vec<DeskNote> = state
            .notes
            .iter()
            .filter(|note| matches(&note.origin))
            .cloned()
            .collect();
        state.notes.retain(|note| !matches(&note.origin));
        gone
    })
    .unwrap_or_default();
    let settings = super::config::app_settings();
    for note in &gone {
        debug_log!("[Notifications] {} closed here", note.id);
    }
    for note in gone.iter().filter(|note| allowed(&settings, note)) {
        to_phones(cancel_packet(&note.id));
    }
}

/// The phone dismissed `id`: closed here too, if it is still open.
pub(crate) fn close_from_phone(id: &str) {
    if !id.starts_with(ID_PREFIX) {
        return;
    }
    let Some(note) = with_state(|state| {
        let index = state.notes.iter().position(|note| note.id == id)?;
        Some(state.notes.remove(index))
    })
    .flatten() else {
        return;
    };
    debug_log!("[Notifications] {id} dismissed on the phone: closing it here");
    let Ok(bus) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    let (name, path, interface, method, args) = match &note.origin {
        Origin::Fdo { server_id: 0, .. } => return,
        Origin::Fdo { server_id, .. } => (
            "org.freedesktop.Notifications",
            "/org/freedesktop/Notifications",
            "org.freedesktop.Notifications",
            "CloseNotification",
            (*server_id,).to_variant(),
        ),
        Origin::Gtk { app_id, id } => (
            "org.gtk.Notifications",
            "/org/gtk/Notifications",
            "org.gtk.Notifications",
            "RemoveNotification",
            (app_id.as_str(), id.as_str()).to_variant(),
        ),
    };
    bus.call(
        Some(name),
        path,
        interface,
        method,
        Some(&args),
        None,
        gio::DBusCallFlags::NONE,
        3000,
        gio::Cancellable::NONE,
        |reply| {
            if let Err(error) = reply {
                debug_log!("[Notifications] Could not close it here: {error}");
            }
        },
    );
}

/// One message the monitor saw.
fn observe(message: &gio::DBusMessage) {
    let sender = message
        .sender()
        .map(|sender| sender.to_string())
        .unwrap_or_default();
    let Some(args) = message.body() else {
        return;
    };
    match (
        message.message_type(),
        message.interface().as_deref(),
        message.member().as_deref(),
    ) {
        (
            gio::DBusMessageType::MethodCall,
            Some("org.freedesktop.Notifications"),
            Some("Notify"),
        ) if to_server(message.destination().as_deref()) => {
            let Some((note, _)) = from_notify(&args, &sender) else {
                return;
            };
            let serial = message.serial();
            let note = with_state(|state| {
                let stored = upsert(state, note);
                state
                    .pending
                    .insert((sender.clone(), serial), stored.id.clone());
                if state.pending.len() > 200 {
                    state.pending.clear();
                }
                stored
            });
            if let Some(note) = note {
                forward(&note);
            }
        }
        (gio::DBusMessageType::Signal, Some("org.freedesktop.DBus"), Some("NameOwnerChanged")) => {
            let new_owner = args
                .try_child_value(2)
                .and_then(|owner| owner.str().map(str::to_string));
            let changed = new_owner.as_ref().is_some_and(|owner| {
                !owner.is_empty()
                    && SERVER
                        .lock()
                        .is_ok_and(|server| server.as_deref() != Some(owner.as_str()))
            });
            if changed {
                debug_log!("[Notifications] The notification server changed; watching it again");
                rebuild();
            }
        }
        (gio::DBusMessageType::MethodReturn, ..) => {
            // A `Notify` reply: the server's id for that notification.
            let Some(destination) = message.destination() else {
                return;
            };
            let key = (destination.to_string(), message.reply_serial());
            let Some(server_id) = args.try_child_value(0).and_then(|id| id.get::<u32>()) else {
                return;
            };
            with_state(|state| {
                if let Some(desk_id) = state.pending.remove(&key) {
                    if let Some(note) = state.notes.iter_mut().find(|note| note.id == desk_id) {
                        if let Origin::Fdo { server_id: id, .. } = &mut note.origin {
                            *id = server_id;
                        }
                    }
                }
            });
        }
        (
            gio::DBusMessageType::Signal,
            Some("org.freedesktop.Notifications"),
            Some("NotificationClosed"),
        ) => {
            if let Some(closed) = args.try_child_value(0).and_then(|id| id.get::<u32>()) {
                removed(
                    |origin| matches!(origin, Origin::Fdo { server_id, .. } if *server_id == closed),
                );
            }
        }
        (
            gio::DBusMessageType::MethodCall,
            Some("org.gtk.Notifications"),
            Some("AddNotification"),
        ) => {
            added(from_gtk(&args, app_display_name));
        }
        (
            gio::DBusMessageType::MethodCall,
            Some("org.gtk.Notifications"),
            Some("RemoveNotification"),
        ) => {
            let (Some(app), Some(id)) = (
                args.try_child_value(0)
                    .and_then(|v| v.str().map(str::to_string)),
                args.try_child_value(1)
                    .and_then(|v| v.str().map(str::to_string)),
            ) else {
                return;
            };
            removed(
                |origin| matches!(origin, Origin::Gtk { app_id, id: known } if *app_id == app && *known == id),
            );
        }
        _ => {}
    }
}

/// The notification server's unique name, once known.
static SERVER: Mutex<Option<String>> = Mutex::new(None);

/// Whether a `Notify` is an app's own call to the notification server, not
/// a proxy passing it on (GNOME's `org.gnome.Shell.Notifications` forwards
/// every one to the shell, which counted each notification twice).
fn to_server(destination: Option<&str>) -> bool {
    destination.is_some_and(|destination| {
        destination == "org.freedesktop.Notifications"
            || SERVER
                .lock()
                .is_ok_and(|server| server.as_deref() == Some(destination))
    })
}

/// The monitor rules: the two notification APIs, closings, and the
/// notification server's replies (for `Notify`'s id).
fn rules(server: Option<&str>) -> Vec<String> {
    let mut rules = vec![
        "type='method_call',interface='org.freedesktop.Notifications',member='Notify'".to_string(),
        "type='signal',interface='org.freedesktop.Notifications',member='NotificationClosed'"
            .to_string(),
        "type='method_call',interface='org.gtk.Notifications',member='AddNotification'".to_string(),
        "type='method_call',interface='org.gtk.Notifications',member='RemoveNotification'"
            .to_string(),
    ];
    // A new server (it restarted): the monitor is rebuilt for its replies.
    rules.push(
        "type='signal',sender='org.freedesktop.DBus',member='NameOwnerChanged',arg0='org.freedesktop.Notifications'"
            .to_string(),
    );
    if let Some(server) = server {
        rules.push(format!("type='method_return',sender='{server}'"));
    }
    rules
}

/// Starts the monitor once (on first use: only after the user turned the
/// feature on). It runs on its own connection, which can do nothing else.
pub(crate) fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(rebuild);
}

/// (Re)builds the monitor on its own thread: at start, and when the
/// notification server changes.
fn rebuild() {
    let _ = std::thread::Builder::new()
        .name("desklink-notify-monitor".into())
        .spawn(|| {
            if let Err(error) = monitor() {
                eprintln!("[Notifications] Cannot watch this computer's notifications: {error}");
            }
        });
}

fn monitor() -> Result<(), String> {
    let address = gio::dbus_address_get_for_bus_sync(gio::BusType::Session, gio::Cancellable::NONE)
        .map_err(|error| error.to_string())?;
    // Who serves notifications now (started if it is not running yet), for
    // its replies.
    let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok();
    if let Some(bus) = &bus {
        let _ = bus.call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus",
            "StartServiceByName",
            Some(&("org.freedesktop.Notifications", 0u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
        );
    }
    let server = bus
        .and_then(|bus| {
            bus.call_sync(
                Some("org.freedesktop.DBus"),
                "/org/freedesktop/DBus",
                "org.freedesktop.DBus",
                "GetNameOwner",
                Some(&("org.freedesktop.Notifications",).to_variant()),
                None,
                gio::DBusCallFlags::NONE,
                2000,
                gio::Cancellable::NONE,
            )
            .ok()
        })
        .and_then(|reply| reply.child_value(0).str().map(str::to_string));
    if let Ok(mut known) = SERVER.lock() {
        known.clone_from(&server);
    }
    let connection = gio::DBusConnection::for_address_sync(
        &address,
        gio::DBusConnectionFlags::AUTHENTICATION_CLIENT
            | gio::DBusConnectionFlags::MESSAGE_BUS_CONNECTION,
        None::<&gio::DBusAuthObserver>,
        gio::Cancellable::NONE,
    )
    .map_err(|error| error.to_string())?;
    // Every message is ours to read and none may be answered: a monitor that
    // sends anything is disconnected. So the filter takes them all, except
    // the bus's own answer to `BecomeMonitor`.
    connection.add_filter(|_, message, incoming| {
        // Our own `BecomeMonitor` going out (filters see both directions).
        if !incoming {
            return Some(message.clone());
        }
        let from_bus = message.sender().as_deref() == Some("org.freedesktop.DBus")
            && matches!(
                message.message_type(),
                gio::DBusMessageType::MethodReturn | gio::DBusMessageType::Error
            );
        if from_bus {
            return Some(message.clone());
        }
        observe(message);
        None
    });
    connection
        .call_sync(
            Some("org.freedesktop.DBus"),
            "/org/freedesktop/DBus",
            "org.freedesktop.DBus.Monitoring",
            "BecomeMonitor",
            Some(&(rules(server.as_deref()), 0u32).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            5000,
            gio::Cancellable::NONE,
        )
        .map_err(|error| error.to_string())?;
    debug_log!("[Notifications] Watching this computer's notifications");
    // The connection works on GDBus's own thread; it only has to live, until
    // a newer monitor replaces it.
    static MONITOR: Mutex<Option<gio::DBusConnection>> = Mutex::new(None);
    if let Ok(mut monitor) = MONITOR.lock() {
        if let Some(old) = monitor.replace(connection) {
            old.close(gio::Cancellable::NONE, |_| {});
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notify(
        app: &str,
        replaces: u32,
        summary: &str,
        body: &str,
        hints: glib::VariantDict,
    ) -> glib::Variant {
        glib::Variant::tuple_from_iter([
            app.to_variant(),
            replaces.to_variant(),
            "".to_variant(),
            summary.to_variant(),
            body.to_variant(),
            Vec::<String>::new().to_variant(),
            hints.end(),
            (-1i32).to_variant(),
        ])
    }

    #[test]
    fn a_notify_call_becomes_a_note_and_ours_are_left_out() {
        let hints = glib::VariantDict::new(None);
        hints.insert("desktop-entry", "org.gnome.Calendar");
        let (note, replaces) = from_notify(
            &notify("Calendar", 0, "Standup", "<b>Room 4</b> &amp; Meet", hints),
            ":1.9",
        )
        .unwrap();
        assert_eq!(replaces, 0);
        assert_eq!(note.app_key, "org.gnome.Calendar");
        assert_eq!(note.app_name, "Calendar");
        assert_eq!(note.title, "Standup");
        assert_eq!(note.text, "Room 4 & Meet", "markup and entities removed");

        assert!(from_notify(
            &notify("DeskLink", 0, "x", "y", glib::VariantDict::new(None)),
            ":1"
        )
        .is_none());
        let transient = glib::VariantDict::new(None);
        transient.insert("transient", true);
        assert!(from_notify(&notify("Volume", 0, "x", "", transient), ":1").is_none());
        assert!(from_notify(
            &notify("App", 0, "", "  ", glib::VariantDict::new(None)),
            ":1"
        )
        .is_none());
    }

    #[test]
    fn gtk_notifications_are_read_and_desklinks_own_never() {
        let body = glib::VariantDict::new(None);
        body.insert("title", "Mail");
        body.insert("body", "2 new");
        let args = glib::Variant::tuple_from_iter([
            "org.gnome.Geary".to_variant(),
            "n1".to_variant(),
            body.end(),
        ]);
        let note = from_gtk(&args, |_| Some("Geary".into())).unwrap();
        assert_eq!(
            (note.app_key.as_str(), note.app_name.as_str()),
            ("org.gnome.Geary", "Geary")
        );
        assert_eq!(
            note.origin,
            Origin::Gtk {
                app_id: "org.gnome.Geary".into(),
                id: "n1".into()
            }
        );
        let body = glib::VariantDict::new(None);
        body.insert("title", "Mara");
        let ours = glib::Variant::tuple_from_iter([
            APP_ID.to_variant(),
            "phone-1".to_variant(),
            body.end(),
        ]);
        assert!(
            from_gtk(&ours, |_| None).is_none(),
            "the phone's own, mirrored here"
        );
    }

    #[test]
    fn updates_keep_their_id_and_new_ones_get_the_next() {
        let mut state = State::default();
        let note = |origin: Origin, title: &str| DeskNote {
            id: String::new(),
            app_key: "a".into(),
            app_name: "A".into(),
            title: title.into(),
            text: String::new(),
            time: 0,
            origin,
        };
        let gtk = || Origin::Gtk {
            app_id: "a".into(),
            id: "1".into(),
        };
        assert_eq!(upsert(&mut state, note(gtk(), "one")).id, "desk:1");
        assert_eq!(
            upsert(&mut state, note(gtk(), "one again")).id,
            "desk:1",
            "replaced"
        );
        let fresh = Origin::Fdo {
            sender: ":1".into(),
            server_id: 0,
        };
        assert_eq!(upsert(&mut state, note(fresh.clone(), "two")).id, "desk:2");
        assert_eq!(
            upsert(&mut state, note(fresh, "three")).id,
            "desk:3",
            "id 0 is never a match"
        );
        assert_eq!(state.notes.len(), 3);
        assert_eq!(state.seen.get("a").map(String::as_str), Some("A"));
        for n in 0..KEPT {
            upsert(
                &mut state,
                note(
                    Origin::Gtk {
                        app_id: "b".into(),
                        id: n.to_string(),
                    },
                    "x",
                ),
            );
        }
        assert_eq!(state.notes.len(), KEPT, "capped");
    }

    #[test]
    fn only_allowed_apps_go_out_and_only_while_on() {
        let mut settings = super::super::config::AppSettings::default();
        let note = DeskNote {
            id: "desk:1".into(),
            app_key: "org.gnome.Calendar".into(),
            app_name: "Calendar".into(),
            title: "t".into(),
            text: String::new(),
            time: 0,
            origin: Origin::Gtk {
                app_id: "x".into(),
                id: "1".into(),
            },
        };
        assert!(!allowed(&settings, &note), "off by default");
        settings.forward_notifications = true;
        assert!(!allowed(&settings, &note), "an app must be picked");
        settings.forward_apps.insert("org.gnome.Calendar".into());
        assert!(allowed(&settings, &note));
        settings.forward_notifications = false;
        assert!(!allowed(&settings, &note));

        let body = body_packet(&note, true);
        assert_eq!(body.get_str("id"), Some("desk:1"));
        assert_eq!(body.get_bool("silent"), Some(true));
        assert_eq!(body.get_bool("isClearable"), Some(true));
        let snapshot = snapshot_packet(&["desk:1".into()]);
        assert_eq!(snapshot.get_bool("snapshot"), Some(true));
        assert_eq!(cancel_packet("desk:1").get_bool("isCancel"), Some(true));
    }

    #[test]
    fn a_proxys_forwarded_notify_is_not_counted_again() {
        assert!(to_server(Some("org.freedesktop.Notifications")));
        assert!(
            !to_server(Some("org.gnome.Shell")),
            "the proxy passing it on"
        );
        assert!(!to_server(None));
    }

    #[test]
    fn the_monitor_also_reads_the_servers_replies_when_it_is_known() {
        assert_eq!(rules(None).len(), 5);
        assert!(rules(Some(":1.20")).contains(&"type='method_return',sender=':1.20'".to_string()));
    }
}
