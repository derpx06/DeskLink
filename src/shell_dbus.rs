//! `derx06.desklink.com.Shell1` at `/derx06/desklink/com/Shell`: what the
//! GNOME Shell extension (`gnome-shell-extension/`) and any other top-bar
//! client read and do.
//!
//! Exported on the application's own connection, so it lives exactly as long
//! as the `derx06.desklink.com` name. `Devices` is polled from the daemon once
//! a second and `PropertiesChanged` is emitted only when it changed. `Invoke`
//! carries no data in: files to send are picked in DeskLink's own dialog,
//! never accepted as paths over the bus.
//!
//! Also offers that extension, once, on GNOME without a tray host.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib::variant::ToVariant;
use gtk::glib::{self, Variant};

use crate::application::DesklinkApplication;
use crate::device_links::config::AppSettings;
use crate::device_links::daemon::DaemonCommand;
use crate::device_links::device::{DesktopSession, DeviceStatus, DeviceView};

pub const PATH: &str = "/derx06/desklink/com/Shell";
pub const INTERFACE: &str = "derx06.desklink.com.Shell1";
/// 2: `Desk`, and the `unlock-via-phone`, `lock-hand-off`, `keep-awake-*`
/// and `clipboard-*` actions.
/// 3: `Notifications`, `Transfers`, `Desk.pill`, `Devices[].lastSeen`, the
/// `lock`, `send-clipboard` and `reconnect` actions, and the notification and
/// transfer methods.
const VERSION: u32 = 3;
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

const XML: &str = r#"<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <property name="Desk" type="a{sv}" access="read"/>
    <property name="Notifications" type="aa{sv}" access="read"/>
    <property name="Transfers" type="aa{sv}" access="read"/>
    <method name="Invoke">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
    <method name="NotificationAction">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
    <method name="NotificationReply">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
      <arg name="text" type="s" direction="in"/>
    </method>
    <method name="NotificationDismiss">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="notification_id" type="s" direction="in"/>
    </method>
    <method name="OpenTransfer">
      <arg name="transfer_id" type="s" direction="in"/>
    </method>
    <method name="Hello">
      <arg name="version" type="u" direction="in"/>
      <arg name="capabilities" type="as" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Actions that need no device (`device_id` ""): `end-sessions` then means
/// every device.
const DEVICELESS: [&str; 9] = [
    "open",
    "reconnect",
    "pair-new",
    "end-sessions",
    "lock-hand-off",
    "keep-awake-on",
    "keep-awake-off",
    "clipboard-on",
    "clipboard-off",
];

/// This computer's switches, for the menu's tiles.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Desk {
    keep_awake: bool,
    clipboard: bool,
    /// A terminal is running and a phone is connected.
    hand_off: bool,
    /// Preferences → "Phone in the top bar": the extension's own pill, on
    /// top of the Quick Settings menu.
    pill: bool,
}

impl Desk {
    fn to_variant(&self) -> Variant {
        let dict = glib::VariantDict::new(None);
        dict.insert("keepAwake", self.keep_awake);
        dict.insert("clipboard", self.clipboard);
        dict.insert("handOff", self.hand_off);
        dict.insert("pill", self.pill);
        dict.end()
    }
}

fn desk(devices: &[ShellDevice]) -> Desk {
    let settings = crate::device_links::config::app_settings();
    Desk {
        keep_awake: settings.stay_awake,
        clipboard: settings.clipboard_sync,
        hand_off: devices.iter().any(|device| device.ready)
            && crate::terminal_previews::sessions()
                .iter()
                .any(|info| info.exit_status.is_none()),
        pill: settings.top_bar_pill,
    }
}

/// One device as clients see it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ShellDevice {
    id: String,
    name: String,
    kind: String,
    paired: bool,
    reachable: bool,
    ready: bool,
    battery: i32,
    charging: bool,
    path: String,
    screen: &'static str,
    terminals: u32,
    /// When its link was lost (Unix seconds), while it stays lost; 0 live.
    last_seen: u64,
    actions: Vec<&'static str>,
}

fn shell_device(device: &DeviceView, settings: &AppSettings) -> ShellDevice {
    let battery = device.battery_status.as_ref();
    ShellDevice {
        id: device.id.clone(),
        name: device.name.clone(),
        kind: device.device_type.clone(),
        paired: device.trusted,
        reachable: !matches!(
            device.status,
            DeviceStatus::Discovered | DeviceStatus::Unreachable
        ),
        ready: device.feature_transport_ready,
        // Only a live reading: a stale one would read as current in the top bar.
        battery: battery
            .filter(|_| device.feature_transport_ready)
            .map_or(-1, |battery| battery.current_charge.clamp(0, 100) as i32),
        charging: device.feature_transport_ready
            && battery.is_some_and(|battery| battery.is_charging),
        path: device.connection_path.clone().unwrap_or_default(),
        screen: match device.desktop_session {
            DesktopSession::None => "",
            DesktopSession::Viewing => "view",
            DesktopSession::Controlling => "control",
        },
        terminals: device.terminal_sessions_attached as u32,
        last_seen: device.last_seen.unwrap_or(0),
        actions: actions(device, settings),
    }
}

/// What a client may offer for `device` right now.
fn actions(device: &DeviceView, settings: &AppSettings) -> Vec<&'static str> {
    let mut actions = vec!["open"];
    if device.feature_transport_ready {
        actions.extend([
            "ring",
            "lock",
            "send-files",
            "send-clipboard",
            "new-terminal",
        ]);
    }
    // The lock-screen button: only with "Unlock with … nearby" on and the
    // phone connected on this network.
    if crate::device_links::desk_features::could_unlock(device, settings) {
        actions.push("unlock-via-phone");
    }
    // Ending is local (stop sharing, detach terminals), so it does not wait
    // for the feature transport.
    if device.desktop_session != DesktopSession::None || device.terminal_sessions_attached > 0 {
        actions.push("end-sessions");
    }
    actions
}

impl ShellDevice {
    fn to_dict(&self) -> Variant {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", &self.id);
        dict.insert("name", &self.name);
        dict.insert("type", &self.kind);
        dict.insert("paired", self.paired);
        dict.insert("reachable", self.reachable);
        dict.insert("ready", self.ready);
        dict.insert("battery", self.battery);
        dict.insert("charging", self.charging);
        dict.insert("path", &self.path);
        dict.insert("screen", self.screen);
        dict.insert("terminals", self.terminals);
        dict.insert("lastSeen", self.last_seen);
        dict.insert("actions", &self.actions);
        dict.end()
    }
}

/// The phones' notifications, for the extension's message tray: only phones
/// whose notifications are shown here. `silent` ones were resent on a
/// reconnect and never banner; `ready` false means they are stale (no
/// actions).
fn notifications_variant(devices: &[DeviceView], settings: &AppSettings) -> Variant {
    let dicts = devices
        .iter()
        .filter(|device| device.trusted && settings.shows_notifications(&device.id))
        .flat_map(|device| {
            device.notifications.iter().map(move |notification| {
                let dict = glib::VariantDict::new(None);
                dict.insert("device", &device.id);
                dict.insert("deviceName", &device.name);
                dict.insert("id", &notification.id);
                dict.insert("app", &notification.app_name);
                dict.insert(
                    "icon",
                    crate::window::notifications::app_icon(&notification.app_name),
                );
                dict.insert("title", &notification.title);
                dict.insert(
                    "body",
                    if notification.text.is_empty() {
                        &notification.ticker
                    } else {
                        &notification.text
                    },
                );
                dict.insert("actions", &notification.actions);
                dict.insert("replyable", notification.request_reply_id.is_some());
                dict.insert("dismissable", notification.is_clearable);
                dict.insert("silent", notification.silent);
                dict.insert("ready", device.feature_transport_ready);
                dict.end()
            })
        });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

/// How many recent files the menu lists.
const RECENT_TRANSFERS: usize = 5;

/// The newest files sent or received; `id` is the path, which only
/// `OpenTransfer` reads back (and only for a file still on this list).
fn transfers_variant(files: &[crate::device_links::transfer_log::LoggedFile]) -> Variant {
    let dicts = files.iter().take(RECENT_TRANSFERS).map(|file| {
        let dict = glib::VariantDict::new(None);
        dict.insert("id", file.path.to_string_lossy().as_ref());
        dict.insert("name", &file.name);
        dict.insert("deviceName", &file.device_name);
        dict.insert("incoming", file.incoming);
        dict.insert("time", file.ts);
        dict.end()
    });
    Variant::array_from_iter_with_type(glib::VariantTy::VARDICT, dicts)
}

fn devices_variant(devices: &[ShellDevice]) -> Variant {
    Variant::array_from_iter_with_type(
        glib::VariantTy::VARDICT,
        devices.iter().map(ShellDevice::to_dict),
    )
}

/// The device (if any) `action` targets, once it is allowed now.
fn check<'a>(
    devices: &'a [ShellDevice],
    device_id: &str,
    action: &str,
) -> Result<Option<&'a ShellDevice>, String> {
    if device_id.is_empty() {
        return if DEVICELESS.contains(&action) {
            Ok(None)
        } else {
            Err(format!("\"{action}\" needs a device"))
        };
    }
    let device = devices
        .iter()
        .find(|device| device.id == device_id)
        .ok_or_else(|| format!("no device \"{device_id}\""))?;
    if device.actions.contains(&action) {
        Ok(Some(device))
    } else {
        Err(format!("\"{action}\" is not available for this device now"))
    }
}

fn current(app: &DesklinkApplication) -> Vec<ShellDevice> {
    let settings = crate::device_links::config::app_settings();
    app.daemon_handle()
        .map(|daemon| {
            daemon
                .devices()
                .iter()
                .map(|device| shell_device(device, &settings))
                .collect()
        })
        .unwrap_or_default()
}

/// Exports the interface on `connection` (from `dbus_register`, so it is
/// there before the application's name is) and starts the poll.
pub fn export(app: &DesklinkApplication, connection: &gio::DBusConnection) {
    let interface = gio::DBusNodeInfo::for_xml(XML)
        .ok()
        .and_then(|node| node.lookup_interface(INTERFACE));
    let Some(interface) = interface else {
        eprintln!("[Shell] Invalid interface description");
        return;
    };
    let properties = Rc::new(RefCell::new(property_values(app)));

    let weak = app.downgrade();
    let shown = Rc::clone(&properties);
    let registered = connection
        .register_object(PATH, &interface)
        .method_call(
            move |connection, sender, _, _, method, params, invocation| {
                let Some(app) = weak.upgrade() else {
                    return invocation.return_dbus_error(INVALID_ARGS, "DeskLink is quitting");
                };
                if method == "Hello" {
                    let (_, capabilities) = params.get::<(u32, Vec<String>)>().unwrap_or_default();
                    client_said_hello(&connection, sender.unwrap_or_default(), capabilities);
                    return invocation.return_value(None);
                }
                match call(&app, method, &params) {
                    Ok(()) => invocation.return_value(None),
                    Err(error) => invocation.return_dbus_error(INVALID_ARGS, &error),
                }
            },
        )
        .property(move |_, _, _, _, name| {
            shown
                .borrow()
                .get(name)
                .cloned()
                .unwrap_or_else(|| VERSION.to_variant())
        })
        .build();
    if let Err(error) = registered {
        eprintln!("[Shell] Could not export {PATH}: {error}");
        return;
    }

    track_extension(connection);

    let weak = app.downgrade();
    let connection = connection.clone();
    glib::timeout_add_seconds_local(1, move || {
        let Some(app) = weak.upgrade().filter(|app| !app.is_remote()) else {
            return glib::ControlFlow::Break;
        };
        let now = property_values(&app);
        let changed: std::collections::HashMap<String, Variant> = now
            .iter()
            .filter(|(name, value)| properties.borrow().get(*name) != Some(*value))
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect();
        properties.replace(now);
        if !changed.is_empty() {
            let changed = (INTERFACE, changed, Vec::<String>::new()).to_variant();
            let _ = connection.emit_signal(
                None,
                PATH,
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                Some(&changed),
            );
        }
        glib::ControlFlow::Continue
    });

    offer_extension(app);
}

/// Every property's current value, by name.
fn property_values(app: &DesklinkApplication) -> std::collections::HashMap<&'static str, Variant> {
    let settings = crate::device_links::config::app_settings();
    let views = app
        .daemon_handle()
        .map(|daemon| daemon.devices())
        .unwrap_or_default();
    let devices: Vec<ShellDevice> = views
        .iter()
        .map(|device| shell_device(device, &settings))
        .collect();
    std::collections::HashMap::from([
        ("Version", VERSION.to_variant()),
        ("Desk", desk(&devices).to_variant()),
        ("Devices", devices_variant(&devices)),
        ("Notifications", notifications_variant(&views, &settings)),
        (
            "Transfers",
            transfers_variant(&crate::device_links::transfer_log::recent()),
        ),
    ])
}

/// One method call from a client.
fn call(app: &DesklinkApplication, method: &str, params: &Variant) -> Result<(), String> {
    let daemon = app.daemon_handle().ok_or("DeskLink is starting")?;
    match method {
        "Invoke" => {
            let (device_id, action) = params.get::<(String, String)>().unwrap_or_default();
            let devices = current(app);
            let device = check(&devices, &device_id, &action)?;
            perform(app, device, &action);
            Ok(())
        }
        "NotificationAction" | "NotificationReply" | "NotificationDismiss" => {
            let (device_id, notification_id, argument) = match method {
                "NotificationDismiss" => {
                    let (device, id) = params.get::<(String, String)>().unwrap_or_default();
                    (device, id, String::new())
                }
                _ => params.get::<(String, String, String)>().unwrap_or_default(),
            };
            let device = daemon
                .devices()
                .into_iter()
                .find(|device| device.id == device_id && device.trusted)
                .ok_or("no such phone")?;
            if !device.feature_transport_ready {
                return Err(format!("{} is not connected", device.name));
            }
            let notification = device
                .notifications
                .iter()
                .find(|notification| notification.id == notification_id)
                .ok_or("no such notification")?;
            match method {
                "NotificationAction" if notification.actions.contains(&argument) => daemon.send(
                    DaemonCommand::TriggerNotificationAction(device_id, notification_id, argument),
                ),
                "NotificationReply" if !argument.trim().is_empty() => {
                    let reply_id = notification
                        .request_reply_id
                        .clone()
                        .ok_or("this notification takes no reply")?;
                    daemon.send(DaemonCommand::ReplyNotification(
                        device_id, reply_id, argument,
                    ))
                }
                "NotificationDismiss" => daemon.send(DaemonCommand::DismissNotification(
                    device_id,
                    notification_id,
                )),
                _ => return Err("not an action this notification offers".to_string()),
            }
            Ok(())
        }
        "OpenTransfer" => {
            let id = params.get::<(String,)>().unwrap_or_default().0;
            // Only a file on the recent list, never an arbitrary path.
            let file = crate::device_links::transfer_log::recent()
                .into_iter()
                .take(RECENT_TRANSFERS)
                .find(|file| file.path.to_string_lossy() == id)
                .ok_or("not a recent transfer")?;
            let uri = gio::File::for_path(&file.path).uri();
            gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE)
                .map_err(|error| error.to_string())
        }
        _ => Err("unknown method".to_string()),
    }
}

fn perform(app: &DesklinkApplication, device: Option<&ShellDevice>, action: &str) {
    let id = device.map(|device| device.id.clone());
    let daemon = app.daemon_handle();
    match (action, id, daemon) {
        ("ring", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendFindPhone(id)),
        ("lock", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendLockRequest(id, true)),
        ("send-clipboard", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendClipboard(id)),
        ("reconnect", _, Some(daemon)) => {
            daemon.send(DaemonCommand::NetworkChanged);
            daemon.send(DaemonCommand::Discover);
        }
        ("end-sessions", id, Some(daemon)) => daemon.send(DaemonCommand::EndPhoneSessions(id)),
        ("lock-hand-off", _, Some(daemon)) => daemon.send(DaemonCommand::LockAndHandOff),
        ("keep-awake-on" | "keep-awake-off", _, Some(daemon)) => {
            daemon.send(DaemonCommand::SetStayAwake(action == "keep-awake-on"))
        }
        ("clipboard-on" | "clipboard-off", _, Some(daemon)) => {
            daemon.send(DaemonCommand::SetClipboardSync(action == "clipboard-on"))
        }
        ("unlock-via-phone", Some(id), _) => {
            // Reads the lock state (loginctl): off the main loop. Every
            // check is made again there and when the phone answers.
            std::thread::spawn(move || {
                crate::device_links::desk_features::offer_unlock(&id, "button");
            });
        }
        ("new-terminal", _, _) => {
            if let Err(error) = crate::device_links::pty::desk::new_desk_session() {
                eprintln!("[Shell] Could not start a terminal: {error}");
            }
        }
        ("send-files", Some(id), Some(_)) => {
            let name = device
                .map(|device| device.name.as_str())
                .unwrap_or_default();
            pick_and_send_files(app, &id, name);
        }
        ("pair-new", _, _) => {
            if let Some(window) = main_window(app) {
                let _ = window.activate_action("win.pair-new", None);
            }
        }
        // "open"
        _ => {
            let window = main_window(app);
            if let (Some(window), Some(device)) =
                (window.and_downcast::<crate::DesklinkWindow>(), device)
            {
                window.show_device(&device.id);
            }
        }
    }
}

/// Asks for files in DeskLink's own dialog and sends them to `device_id`.
pub(crate) fn pick_and_send_files(app: &DesklinkApplication, device_id: &str, name: &str) {
    let Some(daemon) = app.daemon_handle() else {
        return;
    };
    let window = main_window(app);
    let id = device_id.to_string();
    gtk::FileDialog::builder()
        .title(format!("Send Files to {name}"))
        .modal(true)
        .build()
        .open_multiple(window.as_ref(), gio::Cancellable::NONE, move |files| {
            let Ok(files) = files else { return };
            for file in files.iter::<gio::File>().flatten() {
                if let Some(path) = file.path() {
                    daemon.send(DaemonCommand::SendFile(id.clone(), path));
                }
            }
        });
}

/// Presents the main window (creating it if needed) and returns it.
fn main_window(app: &DesklinkApplication) -> Option<gtk::Window> {
    app.activate();
    app.active_window()
}

const EXTENSION_UUID: &str = "desklink@derx06.github.io";
const SHELL_EXTENSIONS: &str = "org.gnome.Shell.Extensions";
const SHELL_EXTENSIONS_PATH: &str = "/org/gnome/Shell/Extensions";
/// `ExtensionState.ACTIVE` (called `ENABLED` before GNOME 46).
const EXTENSION_ACTIVE: f64 = 1.0;

pub(crate) fn is_gnome(current_desktop: &str) -> bool {
    current_desktop
        .split(':')
        .any(|desktop| desktop.eq_ignore_ascii_case("GNOME"))
}

fn call_extensions(
    connection: &gio::DBusConnection,
    method: &str,
    done: impl FnOnce(Result<Variant, glib::Error>) + 'static,
) {
    connection.call(
        Some(SHELL_EXTENSIONS),
        SHELL_EXTENSIONS_PATH,
        SHELL_EXTENSIONS,
        method,
        Some(&(EXTENSION_UUID,).to_variant()),
        None,
        // The Shell's D-Bus-activated extensions service, meant for apps.
        gio::DBusCallFlags::NONE,
        // Installing waits for the user to answer GNOME's own dialog.
        i32::MAX,
        gio::Cancellable::NONE,
        done,
    );
}

/// The `a{sv}` of `GetExtensionInfo`, empty when not installed.
fn extension_info(reply: &Variant) -> glib::VariantDict {
    glib::VariantDict::new(reply.try_child_value(0).as_ref())
}

fn is_active(info: &glib::VariantDict) -> bool {
    info.lookup::<f64>("state").ok().flatten() == Some(EXTENSION_ACTIVE)
}

thread_local! {
    static EXTENSION_IS_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

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

/// A top-bar client (the extension, on enable) says what it handles. Kept
/// until its bus name goes away (Shell restart, extension disabled).
fn client_said_hello(connection: &gio::DBusConnection, sender: &str, capabilities: Vec<String>) {
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
    CLIENT.with(|client| {
        client
            .borrow()
            .as_ref()
            .is_some_and(|client| client.capabilities.iter().any(|known| known == capability))
    })
}

/// Whether DeskLink's top-bar extension is running, so it already shows
/// sessions (kept current from `ExtensionStateChanged`, not polled).
pub fn extension_active() -> bool {
    EXTENSION_IS_ACTIVE.with(|active| active.get())
}

/// The extension, built into the app so any install can turn it on.
const EXTENSION_FILES: [(&str, &str); 4] = [
    (
        "metadata.json",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/metadata.json"),
    ),
    (
        "extension.js",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/extension.js"),
    ),
    (
        "notifications.js",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/notifications.js"),
    ),
    (
        "stylesheet.css",
        include_str!("../gnome-shell-extension/desklink@derx06.github.io/stylesheet.css"),
    ),
];

/// Whether this desktop can show the top-bar menu at all.
pub fn extension_possible() -> bool {
    is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
        && gio::SettingsSchemaSource::default()
            .and_then(|source| source.lookup("org.gnome.shell", true))
            .is_some()
}

/// Beside the installed extension: the digest of what DeskLink wrote, so a
/// copy the user changed is told apart from an older one of ours.
const INSTALL_STAMP: &str = ".desklink-installed";

/// SHA-256 over each extension file's name and contents (missing: empty).
fn files_digest<'a>(files: impl Iterator<Item = (&'a str, Option<&'a str>)>) -> String {
    let mut hasher = openssl::sha::Sha256::new();
    for (name, content) in files {
        hasher.update(name.as_bytes());
        hasher.update(&[0]);
        hasher.update(content.unwrap_or_default().as_bytes());
        hasher.update(&[0]);
    }
    hasher
        .finish()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Writes `path` whole or not at all.
fn write_atomically(path: &std::path::Path, content: &str) -> Result<(), String> {
    let temporary = path.with_extension("desklink-tmp");
    std::fs::write(&temporary, content).map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, path).map_err(|error| error.to_string())
}

/// Writes the extension into `dir` (`~/.local/share/gnome-shell/extensions/<uuid>`)
/// when missing or an older copy of ours. Whether anything was written. A
/// copy someone changed is left alone, with an error saying so.
fn install_extension_files(dir: &std::path::Path) -> Result<bool, String> {
    let present: Vec<(&str, Option<String>)> = EXTENSION_FILES
        .iter()
        .map(|(name, _)| (*name, std::fs::read_to_string(dir.join(name)).ok()))
        .collect();
    if present
        .iter()
        .zip(EXTENSION_FILES)
        .all(|((_, have), (_, ours))| have.as_deref() == Some(ours))
    {
        return Ok(false);
    }
    if present.iter().any(|(_, have)| have.is_some()) {
        let current = files_digest(present.iter().map(|(name, have)| (*name, have.as_deref())));
        let stamp = std::fs::read_to_string(dir.join(INSTALL_STAMP)).ok();
        if stamp.as_deref().map(str::trim) != Some(current.as_str()) {
            return Err(format!(
                "A changed copy of the DeskLink extension is in {}; it was left as it is",
                dir.display()
            ));
        }
    }
    std::fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    for (name, content) in EXTENSION_FILES {
        write_atomically(&dir.join(name), content)?;
    }
    write_atomically(
        &dir.join(INSTALL_STAMP),
        &files_digest(
            EXTENSION_FILES
                .iter()
                .map(|(name, content)| (*name, Some(*content))),
        ),
    )?;
    Ok(true)
}

/// `enabled-extensions` with ours added, once.
fn with_extension(enabled: Vec<String>) -> Vec<String> {
    let mut enabled = enabled;
    if !enabled.iter().any(|uuid| uuid == EXTENSION_UUID) {
        enabled.push(EXTENSION_UUID.to_string());
    }
    enabled
}

/// "Turn on top-bar menu", only ever from the user's click: installs the
/// built-in extension for this user (when missing or outdated), turns user
/// extensions on and enables it. `Ok(true)` when the Shell must be restarted
/// (on Wayland: log out and back in) before it shows; activation is seen by
/// [`track_extension`].
pub fn turn_on_extension() -> Result<bool, String> {
    if !extension_possible() {
        return Err("The top-bar menu needs GNOME".into());
    }
    let dir = glib::user_data_dir()
        .join("gnome-shell")
        .join("extensions")
        .join(EXTENSION_UUID);
    let written = install_extension_files(&dir)?;
    let settings = gio::Settings::new("org.gnome.shell");
    settings
        .set_boolean("disable-user-extensions", false)
        .map_err(|error| error.to_string())?;
    let strings = |key: &str| -> Vec<String> {
        settings
            .strv(key)
            .iter()
            .map(|uuid| uuid.to_string())
            .collect()
    };
    settings
        .set_strv(
            "enabled-extensions",
            with_extension(strings("enabled-extensions")),
        )
        .map_err(|error| error.to_string())?;
    let disabled: Vec<String> = strings("disabled-extensions")
        .into_iter()
        .filter(|uuid| uuid != EXTENSION_UUID)
        .collect();
    settings
        .set_strv("disabled-extensions", disabled)
        .map_err(|error| error.to_string())?;
    gio::Settings::sync();
    Ok(written && !extension_active())
}

fn track_extension(connection: &gio::DBusConnection) {
    if !is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()) {
        return;
    }
    let set = |active: bool| EXTENSION_IS_ACTIVE.with(|cell| cell.set(active));
    // Broadcast by the Shell (and the extensions service); any sender.
    connection.signal_subscribe(
        None,
        Some(SHELL_EXTENSIONS),
        Some("ExtensionStateChanged"),
        None,
        Some(EXTENSION_UUID),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            set(is_active(&glib::VariantDict::new(
                params.try_child_value(1).as_ref(),
            )))
        },
    );
    call_extensions(connection, "GetExtensionInfo", move |reply| {
        if let Ok(reply) = reply {
            set(is_active(&extension_info(&reply)));
        }
    });
}

/// Shown after turning the menu on, when GNOME has to load it first.
pub const RELOGIN_NOTE: &str =
    "GNOME shows a newly added top-bar menu after you log out and back in (on X11, restarting GNOME Shell is enough).";

/// On GNOME with no tray host and our extension not running, offers it in
/// one notification ("Show" / "Not now") a little after start, until the user
/// says "Not now".
fn offer_extension(app: &DesklinkApplication) {
    app.add_action_entries([
        // The notification's "Turn On": the user's click.
        gio::ActionEntry::builder("shell-extension-show")
            .activate(|app: &DesklinkApplication, _, _| {
                let notification = match turn_on_extension() {
                    Ok(false) => return,
                    Ok(true) => {
                        let notification = gio::Notification::new("Top-bar menu turned on");
                        notification.set_body(Some(RELOGIN_NOTE));
                        notification
                    }
                    Err(error) => {
                        eprintln!("[Shell] Could not turn on the top-bar menu: {error}");
                        let notification =
                            gio::Notification::new("Could not turn on the top-bar menu");
                        notification.set_body(Some(&error));
                        notification
                    }
                };
                app.send_notification(Some("shell-extension"), &notification);
            })
            .build(),
        gio::ActionEntry::builder("shell-extension-dismiss")
            .activate(|app: &DesklinkApplication, _, _| {
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(DaemonCommand::SetShellExtensionPromptDismissed(true));
                }
            })
            .build(),
    ]);

    let weak = app.downgrade();
    // Late enough for the tray to have found any host.
    glib::timeout_add_seconds_local_once(10, move || {
        let Some(app) = weak.upgrade() else { return };
        let wanted = !app.is_remote()
            && is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
            && !crate::device_links::config::app_settings().shell_extension_prompt_dismissed
            && !app.tray_hosted();
        let Some(connection) = app.dbus_connection().filter(|_| wanted) else {
            return;
        };
        // Fails when the Shell's extension service is absent: no offer then.
        call_extensions(&connection, "GetExtensionInfo", move |reply| {
            let Ok(reply) = reply else { return };
            if is_active(&extension_info(&reply)) {
                return;
            }
            let notification = gio::Notification::new("Show DeskLink in the top bar");
            notification.set_body(Some(
                "See your phone's battery and connection, and act on it, from the top bar.",
            ));
            notification.add_button("Turn On", "app.shell-extension-show");
            notification.add_button("Not now", "app.shell-extension-dismiss");
            app.send_notification(Some("shell-extension"), &notification);
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device::BatteryStatus;
    use crate::device_links::device_info::DeviceInfo;

    /// Unlock with the phone on for `phone1` only.
    fn on() -> AppSettings {
        let mut settings = AppSettings::default();
        settings.unlock_devices.insert("phone1".into());
        settings
    }

    fn phone() -> DeviceView {
        let info = DeviceInfo {
            id: "phone1".into(),
            name: "Pixel".into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        DeviceView::from_info(&info, "10.0.0.2".into(), true)
    }

    #[test]
    fn a_device_maps_to_the_documented_dict() {
        let mut device = phone();
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.connection_path = Some("lan".into());
        device.desktop_session = DesktopSession::Controlling;
        device.terminal_sessions_attached = 2;
        device.battery_status = Some(BatteryStatus {
            current_charge: 81,
            is_charging: true,
            battery_quantity: None,
            threshold_event: None,
            charge_time_remaining: None,
        });
        let mapped = shell_device(&device, &AppSettings::default());
        assert!(mapped.paired && mapped.reachable && mapped.ready && mapped.charging);
        assert_eq!(
            (mapped.battery, mapped.screen, mapped.terminals),
            (81, "control", 2)
        );
        assert_eq!(mapped.path, "lan");

        let list = devices_variant(&[mapped]);
        assert_eq!(list.type_().as_str(), "aa{sv}");
        let dict = glib::VariantDict::new(Some(&list.child_value(0)));
        assert_eq!(
            dict.lookup::<String>("id").unwrap().as_deref(),
            Some("phone1")
        );
        assert_eq!(
            dict.lookup::<String>("type").unwrap().as_deref(),
            Some("phone")
        );
        assert_eq!(dict.lookup::<i32>("battery").unwrap(), Some(81));
        assert_eq!(dict.lookup::<u32>("terminals").unwrap(), Some(2));
        assert_eq!(
            dict.lookup::<Vec<String>>("actions").unwrap().unwrap(),
            [
                "open",
                "ring",
                "lock",
                "send-files",
                "send-clipboard",
                "new-terminal",
                "end-sessions"
            ]
        );

        let idle = shell_device(&phone(), &AppSettings::default());
        assert!(!idle.reachable && !idle.ready);
        assert_eq!(
            (idle.battery, idle.screen, idle.path.as_str()),
            (-1, "", "")
        );
    }

    #[test]
    fn actions_follow_readiness_and_sessions_and_invoke_checks_them() {
        let mut device = phone();
        assert_eq!(actions(&device, &on()), ["open"]);
        device.terminal_sessions_attached = 1;
        assert_eq!(actions(&device, &on()), ["open", "end-sessions"]);

        let devices = [shell_device(&device, &on())];
        assert!(check(&devices, "phone1", "open").unwrap().is_some());
        assert!(check(&devices, "phone1", "end-sessions").is_ok());
        assert!(check(&devices, "phone1", "ring").is_err(), "not ready");
        assert!(check(&devices, "phone1", "explode").is_err());
        assert!(check(&devices, "nobody", "open").is_err());
        assert!(check(&devices, "", "pair-new").unwrap().is_none());
        assert!(check(&devices, "", "end-sessions").is_ok());
        assert!(check(&devices, "", "ring").is_err());
        assert!(check(&devices, "", "lock-hand-off").is_ok());
        assert!(
            check(&devices, "phone1", "unlock-via-phone").is_err(),
            "not connected"
        );
    }

    #[test]
    fn the_unlock_button_needs_the_setting_and_a_phone_on_this_network() {
        let mut device = phone();
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.connection_path = Some("lan".into());
        assert!(actions(&device, &on()).contains(&"unlock-via-phone"));
        assert!(
            !actions(&device, &AppSettings::default()).contains(&"unlock-via-phone"),
            "setting off"
        );
        let mut other = AppSettings::default();
        other.unlock_devices.insert("another-phone".into());
        assert!(
            !actions(&device, &other).contains(&"unlock-via-phone"),
            "on for another phone only"
        );
        for path in ["direct", "turn", "relay"] {
            device.connection_path = Some(path.into());
            assert!(
                !actions(&device, &on()).contains(&"unlock-via-phone"),
                "{path}"
            );
        }
    }

    #[test]
    fn only_state_1_counts_as_an_active_extension() {
        let info = |state: f64| {
            let dict = glib::VariantDict::new(None);
            dict.insert("state", state);
            dict
        };
        assert!(is_active(&info(1.0)));
        assert!(!is_active(&info(2.0)));
        assert!(!is_active(&info(6.0)));
        assert!(!is_active(&glib::VariantDict::new(None)));
    }

    #[test]
    fn the_extension_is_written_once_and_enabled_once() {
        let dir = std::env::temp_dir().join(format!("desklink-ext-{}", uuid::Uuid::new_v4()));
        assert!(install_extension_files(&dir).unwrap(), "missing: written");
        assert!(!install_extension_files(&dir).unwrap(), "same: left alone");
        // An older copy of ours (its stamp matches what is there): replaced.
        std::fs::write(dir.join("extension.js"), "old").unwrap();
        let older = files_digest(EXTENSION_FILES.iter().map(|(name, content)| {
            (
                *name,
                Some(if *name == "extension.js" {
                    "old"
                } else {
                    *content
                }),
            )
        }));
        std::fs::write(dir.join(INSTALL_STAMP), older).unwrap();
        assert!(
            install_extension_files(&dir).unwrap(),
            "outdated: rewritten"
        );
        // A copy someone edited: kept, and the caller is told.
        std::fs::write(dir.join("extension.js"), "// my tweaks").unwrap();
        assert!(install_extension_files(&dir).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.join("extension.js")).unwrap(),
            "// my tweaks"
        );
        let metadata = std::fs::read_to_string(dir.join("metadata.json")).unwrap();
        assert!(metadata.contains(EXTENSION_UUID) && metadata.contains("unlock-dialog"));
        let _ = std::fs::remove_dir_all(&dir);

        let enabled = with_extension(vec!["other@x".into()]);
        assert_eq!(enabled, ["other@x", EXTENSION_UUID]);
        assert_eq!(with_extension(enabled.clone()), enabled);
    }

    #[test]
    fn gnome_is_found_in_a_desktop_list() {
        assert!(is_gnome("GNOME"));
        assert!(is_gnome("ubuntu:GNOME"));
        assert!(!is_gnome("KDE"));
        assert!(!is_gnome(""));
    }
}
