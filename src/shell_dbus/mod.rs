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
//!
//! Layout: this file holds the interface (path, XML, version), the export
//! with the sender gate, the properties and their change signals, and the
//! extension's identity. `model` is the pure device model with its actions
//! and switches, `variants` turns it into GLib variants, `methods` performs
//! the calls, `clients` knows who said `Hello`, `extension` drives the GNOME
//! extension and `extension_files` holds its embedded files. No cycles
//! between the parts. The extension's node tests read these files
//! (`gnome-shell-extension/tests/model.test.mjs`).

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

mod clients;
mod extension;
mod extension_files;
mod methods;
mod model;
mod variants;
use clients::*;
use extension::*;
use extension_files::*;
use methods::*;
use model::*;
use variants::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use clients::client_handles;
pub(crate) use extension::{
    appindicator_off, enable_extension, extension_active, extension_status, log_out,
    turn_off_extension, turn_on_extension, ExtensionStatus, RELOGIN_NOTE,
};
pub(crate) use extension_files::extension_possible;
pub(crate) use methods::pick_and_send_files;

pub const PATH: &str = "/derx06/desklink/com/Shell";
pub const INTERFACE: &str = "derx06.desklink.com.Shell1";
/// 2: `Desk`, and the `unlock-via-phone`, `lock-hand-off`, `keep-awake-*`
/// and `clipboard-*` actions.
/// 3: `Notifications`, `Transfers`, `Desk.pill`, `Devices[].lastSeen`, the
/// `lock`, `send-clipboard` and `reconnect` actions, and the notification and
/// transfer methods.
/// 4: `Devices[].call` and the `call-*` actions (a client that shows calls
/// says `calls` in `Hello`).
/// 5: `Devices[].media` with the `media-play-pause` / `media-next` actions,
/// and `Devices[].liveSince`.
/// 6: `Snippets` (id, label, text: the client copies the text itself) and
/// the device-less `new-note` action.
/// 7: `Devices[].bucket` (`line`, `detail`, `fraction`) while the Bucket
/// syncs, and the `open-bucket` action.
/// 8: the `insert-photo` action (a photo from the phone onto the clipboard).
/// 9: the complete remote. `Devices[].settings` (per-phone switches),
/// `.sound`, `.rttMs`, `.unread`, `.textCapture`, `.transfers`; more `Desk`
/// switches; the `SetSwitch` and `SendLink` methods; the `ping`, `send-link`,
/// `show-screen`, `media-prev`, `insert-*`, `open-*` and `unpair` actions.
/// The security-sensitive switches can only be turned OFF here
/// ([`SWITCHES`]); turning them on is the application's own page.
/// 10: `Devices[].media` gains `positionMs`, `lengthMs` (both as last reported;
/// the client moves the bar on while `playing`) and `seekReason` (why the bar
/// cannot seek, else empty); the `media-seek` action marks a seekable track and
/// the `SeekMedia(device, position_ms)` method seeks it.
const VERSION: u32 = 10;
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

const XML: &str = r#"<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <property name="Desk" type="a{sv}" access="read"/>
    <property name="Notifications" type="aa{sv}" access="read"/>
    <property name="Transfers" type="aa{sv}" access="read"/>
    <property name="Snippets" type="aa{sv}" access="read"/>
    <property name="Activities" type="aa{sv}" access="read"/>
    <method name="Invoke">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
    <method name="SeekMedia">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="position_ms" type="x" direction="in"/>
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
    <method name="SetSwitch">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="key" type="s" direction="in"/>
      <arg name="on" type="b" direction="in"/>
    </method>
    <method name="SendLink">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="link" type="s" direction="in"/>
    </method>
    <method name="Hello">
      <arg name="version" type="u" direction="in"/>
      <arg name="capabilities" type="as" direction="in"/>
    </method>
  </interface>
</node>"#;

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
    let announced = Rc::clone(&properties);
    let registered = connection
        .register_object(PATH, &interface)
        .method_call(
            move |connection, sender, _, _, method, params, invocation| {
                crate::guard("top-bar call", (), || {
                    let Some(app) = weak.upgrade() else {
                        return invocation.return_dbus_error(INVALID_ARGS, "DeskLink is quitting");
                    };
                    if method == "Hello" {
                        let (_, capabilities) =
                            params.get::<(u32, Vec<String>)>().unwrap_or_default();
                        let sender = sender.unwrap_or_default();
                        // Only the Shell itself (the extension runs inside it)
                        // may say it shows the alerts: anyone else claiming so
                        // would mute DeskLink's own call and notification alerts.
                        let check = check_hello_sender(&connection, sender);
                        if check == Check::Shell {
                            client_said_hello(&connection, sender, vetted(capabilities));
                            announce_to(&connection, sender, &announced.borrow());
                        } else if check == Check::Throttled {
                            // Someone else's question spent the window; the
                            // Shell says Hello once, so it is tried again
                            // just after it, not refused.
                            let (connection, sender) = (connection.clone(), sender.to_string());
                            let capabilities = vetted(capabilities);
                            let announced = Rc::clone(&announced);
                            gtk::glib::timeout_add_local_once(
                                OWNER_ASK_EVERY + std::time::Duration::from_millis(100),
                                move || {
                                    if hello_is_from_the_shell(&connection, &sender) {
                                        client_said_hello(&connection, &sender, capabilities);
                                        announce_to(&connection, &sender, &announced.borrow());
                                    }
                                },
                            );
                        } else {
                            static TOLD: std::sync::Once = std::sync::Once::new();
                            TOLD.call_once(|| {
                                eprintln!(
                                    "[Shell] Ignored a Hello from a client that is not GNOME Shell"
                                );
                            });
                        }
                        return invocation.return_value(None);
                    }
                    // Every verb has an outward effect (ring or lock the phone,
                    // send the clipboard, change what is shared): only the
                    // Shell, which the extension runs inside, may ask.
                    if !is_the_shell_caller(&connection, sender) {
                        return invocation.return_dbus_error(
                            "org.freedesktop.DBus.Error.AccessDenied",
                            "Only GNOME Shell may use the top-bar interface",
                        );
                    }
                    match call(&app, method, &params) {
                        Ok(()) => invocation.return_value(None),
                        Err(error) => invocation.return_dbus_error(INVALID_ARGS, &error),
                    }
                })
            },
        )
        .property(move |connection, sender, _, _, name| {
            // Phone names, call numbers, notifications, snippets and what is
            // allowed per phone are for the Shell only: anyone else on the
            // session bus reads empty values (the version is not private).
            let from_shell = name == "Version" || is_the_shell_caller(&connection, sender);
            visible_property(name, &shown.borrow(), from_shell)
        })
        .build();
    if let Err(error) = registered {
        eprintln!("[Shell] Could not export {PATH}: {error}");
        return;
    }

    track_extension(connection);

    let weak = app.downgrade();
    let connection = connection.clone();
    crate::ticker::every_tick(move || {
        crate::guard("top-bar tick", glib::ControlFlow::Continue, || {
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
            // Sent to the Shell alone: a broadcast would hand the same values
            // to any process that listens on the session bus.
            if !changed.is_empty() {
                if let Some(owner) = known_shell_owner() {
                    emit_changed(&connection, &owner, changed);
                }
            }
            glib::ControlFlow::Continue
        })
    });

    offer_extension(app);
}

/// The empty value of a property, of its declared type.
fn empty_property(name: &str) -> Variant {
    match name {
        "Desk" => glib::VariantDict::new(None).end(),
        _ => Variant::array_from_iter_with_type(
            glib::VariantTy::VARDICT,
            std::iter::empty::<Variant>(),
        ),
    }
}

/// What a reader of property `name` gets: the real value for the Shell, an
/// empty one of the same type for anyone else.
fn visible_property(
    name: &str,
    values: &std::collections::HashMap<&'static str, Variant>,
    from_shell: bool,
) -> Variant {
    match values.get(name) {
        Some(value) if from_shell || name == "Version" => value.clone(),
        Some(_) => empty_property(name),
        None => VERSION.to_variant(),
    }
}

/// `PropertiesChanged` to one bus name (the Shell's), never a broadcast.
fn emit_changed(
    connection: &gio::DBusConnection,
    destination: &str,
    changed: std::collections::HashMap<String, Variant>,
) {
    let changed = (INTERFACE, changed, Vec::<String>::new()).to_variant();
    let _ = connection.emit_signal(
        Some(destination),
        PATH,
        "org.freedesktop.DBus.Properties",
        "PropertiesChanged",
        Some(&changed),
    );
}

/// Every current value, to a client that just proved it is the Shell (its
/// first read may have come before the owner was known).
fn announce_to(
    connection: &gio::DBusConnection,
    shell: &str,
    values: &std::collections::HashMap<&'static str, Variant>,
) {
    emit_changed(
        connection,
        shell,
        values
            .iter()
            .map(|(name, value)| (name.to_string(), value.clone()))
            .collect(),
    );
}

/// Every property's current value, by name.
fn property_values(app: &DesklinkApplication) -> std::collections::HashMap<&'static str, Variant> {
    let settings = crate::device_links::config::app_settings();
    let views = app
        .daemon_handle()
        .map(|daemon| daemon.devices())
        .unwrap_or_default();
    let mut devices: Vec<ShellDevice> = views
        .iter()
        .map(|device| shell_device(device, &settings))
        .collect();
    stamp_live(&mut devices);
    std::collections::HashMap::from([
        ("Version", VERSION.to_variant()),
        ("Desk", desk(&devices).to_variant()),
        ("Devices", devices_variant(&devices)),
        ("Notifications", notifications_variant(&views, &settings)),
        (
            "Transfers",
            transfers_variant(&crate::device_links::transfer_log::recent()),
        ),
        ("Snippets", snippets_variant()),
        (
            "Activities",
            activities_variant(&views, &settings, crate::device_links::activity::now_ms()),
        ),
    ])
}

pub(super) const EXTENSION_UUID: &str = "desklink@derx06.github.io";

pub(super) fn user_extension_dir() -> std::path::PathBuf {
    glib::user_data_dir()
        .join("gnome-shell")
        .join("extensions")
        .join(EXTENSION_UUID)
}

pub(crate) fn is_gnome(current_desktop: &str) -> bool {
    crate::platform::is_gnome_shell(current_desktop)
}

#[cfg(test)]
mod tests;
