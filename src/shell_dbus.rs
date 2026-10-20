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
use crate::device_links::daemon::DaemonCommand;
use crate::device_links::device::{DesktopSession, DeviceStatus, DeviceView};

pub const PATH: &str = "/derx06/desklink/com/Shell";
pub const INTERFACE: &str = "derx06.desklink.com.Shell1";
const VERSION: u32 = 1;
const INVALID_ARGS: &str = "org.freedesktop.DBus.Error.InvalidArgs";

const XML: &str = r#"<node>
  <interface name="derx06.desklink.com.Shell1">
    <property name="Version" type="u" access="read"/>
    <property name="Devices" type="aa{sv}" access="read"/>
    <method name="Invoke">
      <arg name="device_id" type="s" direction="in"/>
      <arg name="action" type="s" direction="in"/>
    </method>
  </interface>
</node>"#;

/// Actions that need no device (`device_id` ""): `end-sessions` then means
/// every device.
const DEVICELESS: [&str; 3] = ["open", "pair-new", "end-sessions"];

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
    actions: Vec<&'static str>,
}

fn shell_device(device: &DeviceView) -> ShellDevice {
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
        battery: battery.map_or(-1, |battery| battery.current_charge.clamp(0, 100) as i32),
        charging: battery.is_some_and(|battery| battery.is_charging),
        path: device.connection_path.clone().unwrap_or_default(),
        screen: match device.desktop_session {
            DesktopSession::None => "",
            DesktopSession::Viewing => "view",
            DesktopSession::Controlling => "control",
        },
        terminals: device.terminal_sessions_attached as u32,
        actions: actions(device),
    }
}

/// What a client may offer for `device` right now.
fn actions(device: &DeviceView) -> Vec<&'static str> {
    let mut actions = vec!["open"];
    if device.feature_transport_ready {
        actions.extend(["ring", "send-files", "browse-files", "new-terminal"]);
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
        dict.insert("actions", &self.actions);
        dict.end()
    }
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
    app.daemon_handle()
        .map(|daemon| daemon.devices().iter().map(shell_device).collect())
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
    let devices = Rc::new(RefCell::new(current(app)));

    let weak = app.downgrade();
    let shown = Rc::clone(&devices);
    let registered = connection
        .register_object(PATH, &interface)
        .method_call(move |_, _, _, _, method, params, invocation| {
            let Some(app) = weak.upgrade() else {
                return invocation.return_dbus_error(INVALID_ARGS, "DeskLink is quitting");
            };
            let (device_id, action) = params.get::<(String, String)>().unwrap_or_default();
            let devices = current(&app);
            match check(&devices, &device_id, &action) {
                Ok(device) if method == "Invoke" => {
                    perform(&app, device, &action);
                    invocation.return_value(None);
                }
                Ok(_) => invocation.return_dbus_error(INVALID_ARGS, "unknown method"),
                Err(error) => invocation.return_dbus_error(INVALID_ARGS, &error),
            }
        })
        .property(move |_, _, _, _, name| match name {
            "Version" => VERSION.to_variant(),
            _ => devices_variant(&shown.borrow()),
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
        let now = current(&app);
        if *devices.borrow() != now {
            let changed = (
                INTERFACE,
                std::collections::HashMap::from([("Devices".to_string(), devices_variant(&now))]),
                Vec::<String>::new(),
            )
                .to_variant();
            devices.replace(now);
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

fn perform(app: &DesklinkApplication, device: Option<&ShellDevice>, action: &str) {
    let id = device.map(|device| device.id.clone());
    let daemon = app.daemon_handle();
    match (action, id, daemon) {
        ("ring", Some(id), Some(daemon)) => daemon.send(DaemonCommand::SendFindPhone(id)),
        ("end-sessions", id, Some(daemon)) => daemon.send(DaemonCommand::EndPhoneSessions(id)),
        ("new-terminal", _, _) => {
            if let Err(error) = crate::device_links::pty::desk::new_desk_session() {
                eprintln!("[Shell] Could not start a terminal: {error}");
            }
        }
        ("send-files", Some(id), Some(daemon)) => {
            let window = main_window(app);
            let name = device
                .map(|device| device.name.as_str())
                .unwrap_or_default();
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
        ("browse-files", Some(id), Some(daemon)) => {
            if let (Some(window), Some(device)) = (main_window(app), device) {
                crate::phone_files::show(daemon, id, &device.name, &window);
            }
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

fn is_gnome(current_desktop: &str) -> bool {
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

/// Whether DeskLink's top-bar extension is running, so it already shows
/// sessions (kept current from `ExtensionStateChanged`, not polled).
pub fn extension_active() -> bool {
    EXTENSION_IS_ACTIVE.with(|active| active.get())
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

/// On GNOME with no tray host and our extension not running, offers it in
/// one notification ("Show" / "Not now") a little after start, until the user
/// says "Not now".
fn offer_extension(app: &DesklinkApplication) {
    app.add_action_entries([
        gio::ActionEntry::builder("shell-extension-show")
            .activate(|app: &DesklinkApplication, _, _| {
                let Some(connection) = app.dbus_connection() else {
                    return;
                };
                let retry = connection.clone();
                call_extensions(&connection, "GetExtensionInfo", move |reply| {
                    let installed = reply
                        .map(|reply| extension_info(&reply).contains("uuid"))
                        .unwrap_or(false);
                    let method = if installed {
                        "EnableExtension"
                    } else {
                        "InstallRemoteExtension"
                    };
                    call_extensions(&retry, method, move |reply| {
                        if let Err(error) = reply {
                            eprintln!("[Shell] {method} failed: {error}");
                        }
                    });
                });
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
            notification.add_button("Show", "app.shell-extension-show");
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
        });
        let mapped = shell_device(&device);
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
                "send-files",
                "browse-files",
                "new-terminal",
                "end-sessions"
            ]
        );

        let idle = shell_device(&phone());
        assert!(!idle.reachable && !idle.ready);
        assert_eq!(
            (idle.battery, idle.screen, idle.path.as_str()),
            (-1, "", "")
        );
    }

    #[test]
    fn actions_follow_readiness_and_sessions_and_invoke_checks_them() {
        let mut device = phone();
        assert_eq!(actions(&device), ["open"]);
        device.terminal_sessions_attached = 1;
        assert_eq!(actions(&device), ["open", "end-sessions"]);

        let devices = [shell_device(&device)];
        assert!(check(&devices, "phone1", "open").unwrap().is_some());
        assert!(check(&devices, "phone1", "end-sessions").is_ok());
        assert!(check(&devices, "phone1", "ring").is_err(), "not ready");
        assert!(check(&devices, "phone1", "explode").is_err());
        assert!(check(&devices, "nobody", "open").is_err());
        assert!(check(&devices, "", "pair-new").unwrap().is_none());
        assert!(check(&devices, "", "end-sessions").is_ok());
        assert!(check(&devices, "", "ring").is_err());
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
    fn gnome_is_found_in_a_desktop_list() {
        assert!(is_gnome("GNOME"));
        assert!(is_gnome("ubuntu:GNOME"));
        assert!(!is_gnome("KDE"));
        assert!(!is_gnome(""));
    }
}
