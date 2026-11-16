//! The GNOME Shell extension's state from the Shell (installed, enabled,
//! active), enabling it, turning it on or off in GNOME's list, logging out,
//! tracking it and offering it once.
//!
//! Sibling edges: `extension_files` (installing the files). Outside: gio
//! D-Bus to `org.gnome.Shell.Extensions`, `config`, `application`.

use super::*;

const SHELL_EXTENSIONS: &str = "org.gnome.Shell.Extensions";
const SHELL_EXTENSIONS_PATH: &str = "/org/gnome/Shell/Extensions";
/// `ExtensionState.ACTIVE` (called `ENABLED` before GNOME 46).
const EXTENSION_ACTIVE: f64 = 1.0;

fn call_extensions(
    connection: &gio::DBusConnection,
    method: &str,
    uuid: &str,
    done: impl FnOnce(Result<Variant, glib::Error>) + 'static,
) {
    connection.call(
        Some(SHELL_EXTENSIONS),
        SHELL_EXTENSIONS_PATH,
        SHELL_EXTENSIONS,
        method,
        Some(&(uuid,).to_variant()),
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
    glib::VariantDict::new(
        reply
            .try_child_value(0)
            .filter(|v| v.type_() == glib::VariantTy::VARDICT)
            .as_ref(),
    )
}

pub(super) fn is_active(info: &glib::VariantDict) -> bool {
    info.lookup::<f64>("state").ok().flatten() == Some(EXTENSION_ACTIVE)
}

/// The GNOME extensions that host StatusNotifier tray icons.
const APPINDICATOR_UUIDS: [&str; 2] = [
    "appindicatorsupport@rgcjonas.gmail.com",
    "ubuntu-appindicators@ubuntu.com",
];

thread_local! {
    pub(super) static EXTENSION_IS_ACTIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// GNOME Shell has loaded ours (it may still be off): `GetExtensionInfo`
    /// is not empty. A copy written this session is not loaded until the
    /// next login on Wayland.
    pub(super) static EXTENSION_KNOWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// An installed AppIndicator extension, and whether it is on.
    static APPINDICATOR: RefCell<Option<(String, bool)>> = const { RefCell::new(None) };
}

/// The top bar on this desktop, for Settings.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub enum ExtensionStatus {
    Active,
    /// Written for this user; GNOME loads it at the next login.
    NeedsLogin,
    Off,
}

pub fn extension_status() -> ExtensionStatus {
    status_of(
        extension_active(),
        EXTENSION_KNOWN.with(|known| known.get()),
        user_extension_dir().join("metadata.json").is_file(),
    )
}

/// A running menu whose files are gone (DeskLink's data was removed) is Off:
/// GNOME would drop it at the next log-in, and "Turn on" writes it back.
pub(super) fn status_of(active: bool, known: bool, installed: bool) -> ExtensionStatus {
    match (active, installed) {
        (true, true) => ExtensionStatus::Active,
        (false, true) if !known => ExtensionStatus::NeedsLogin,
        _ => ExtensionStatus::Off,
    }
}

/// An installed AppIndicator extension that is off (its uuid): turning it on
/// shows DeskLink's tray icon at once, no login needed.
pub fn appindicator_off() -> Option<String> {
    APPINDICATOR.with(|found| {
        found
            .borrow()
            .as_ref()
            .filter(|(_, on)| !on)
            .map(|(uuid, _)| uuid.clone())
    })
}

/// Turns on an installed extension by `uuid`, user extensions first (the
/// user's own click only).
pub fn enable_extension(uuid: &str, done: impl FnOnce(Result<(), String>) + 'static) {
    let settings = gio::Settings::new("org.gnome.shell");
    if settings.boolean("disable-user-extensions") {
        if let Err(error) = settings.set_boolean("disable-user-extensions", false) {
            return done(Err(error.to_string()));
        }
        gio::Settings::sync();
    }
    let Ok(connection) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return done(Err("No session bus".into()));
    };
    call_extensions(&connection, "EnableExtension", uuid, move |reply| {
        done(match reply {
            Ok(reply)
                if reply.try_child_value(0).and_then(|ok| ok.get::<bool>()) == Some(false) =>
            {
                Err("GNOME did not turn it on".into())
            }
            Ok(_) => Ok(()),
            Err(error) => Err(error.message().to_string()),
        })
    });
}

/// Asks GNOME to log out; GNOME itself asks the user to confirm.
pub fn log_out() {
    let Ok(connection) = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE) else {
        return;
    };
    connection.call(
        Some("org.gnome.SessionManager"),
        "/org/gnome/SessionManager",
        "org.gnome.SessionManager",
        "Logout",
        // 0: the normal logout, with GNOME's confirmation dialog.
        Some(&(0u32,).to_variant()),
        None,
        gio::DBusCallFlags::NONE,
        -1,
        gio::Cancellable::NONE,
        |reply| {
            if let Err(error) = reply {
                eprintln!("[Shell] Could not log out: {error}");
            }
        },
    );
}

pub(super) fn track_extension(connection: &gio::DBusConnection) {
    if !is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()) {
        return;
    }
    let set = |active: bool| {
        EXTENSION_IS_ACTIVE.with(|cell| cell.set(active));
        EXTENSION_KNOWN.with(|cell| cell.set(true));
    };
    // Broadcast by the Shell (and the extensions service); any sender.
    connection.signal_subscribe(
        None,
        Some(SHELL_EXTENSIONS),
        Some("ExtensionStateChanged"),
        None,
        Some(EXTENSION_UUID),
        gio::DBusSignalFlags::NONE,
        move |_, _, _, _, _, params| {
            crate::guard("top-bar signal", (), || {
                set(is_active(&glib::VariantDict::new(
                    params
                        .try_child_value(1)
                        .filter(|v| v.type_() == glib::VariantTy::VARDICT)
                        .as_ref(),
                )))
            })
        },
    );
    call_extensions(
        connection,
        "GetExtensionInfo",
        EXTENSION_UUID,
        move |reply| {
            if let Ok(reply) = reply {
                let info = extension_info(&reply);
                EXTENSION_IS_ACTIVE.with(|cell| cell.set(is_active(&info)));
                EXTENSION_KNOWN.with(|cell| cell.set(info.lookup_value("uuid", None).is_some()));
            }
        },
    );
    // The tray hosts: which one is installed, and whether it is on.
    for uuid in APPINDICATOR_UUIDS {
        let note = move |info: &glib::VariantDict| {
            if info.lookup_value("uuid", None).is_some() {
                APPINDICATOR.with(|found| {
                    found.replace(Some((uuid.to_string(), is_active(info))));
                });
            }
        };
        connection.signal_subscribe(
            None,
            Some(SHELL_EXTENSIONS),
            Some("ExtensionStateChanged"),
            None,
            Some(uuid),
            gio::DBusSignalFlags::NONE,
            move |_, _, _, _, _, params| {
                crate::guard("top-bar signal", (), || {
                    note(&glib::VariantDict::new(
                        params
                            .try_child_value(1)
                            .filter(|v| v.type_() == glib::VariantTy::VARDICT)
                            .as_ref(),
                    ))
                })
            },
        );
        call_extensions(connection, "GetExtensionInfo", uuid, move |reply| {
            if let Ok(reply) = reply {
                note(&extension_info(&reply));
            }
        });
    }
}

/// Shown after turning the menu on, when GNOME has to load it first.
pub const RELOGIN_NOTE: &str =
    "GNOME shows a newly added top-bar menu after you log out and back in (on X11, restarting GNOME Shell is enough).";

/// On GNOME with no tray host and our extension not running, offers it in
/// one notification ("Show" / "Not now") a little after start, until the user
/// says "Not now".
pub(super) fn offer_extension(app: &DesklinkApplication) {
    app.add_action_entries([
        // The notification's "Turn On": the user's click.
        gio::ActionEntry::builder("shell-extension-show")
            .activate(|app: &DesklinkApplication, _, _| {
                let notification = match turn_on_extension() {
                    Ok(false) => return,
                    Ok(true) => {
                        let notification = gio::Notification::new("Top-bar menu turned on");
                        notification.set_body(Some(RELOGIN_NOTE));
                        notification.add_button("Log Out Now", "app.log-out");
                        notification
                    }
                    Err(error) => {
                        eprintln!("[Shell] Could not turn on the top-bar menu: {error}");
                        let notification = crate::application::desk_notification(
                            "Could not turn on the top-bar menu",
                        );
                        notification.set_body(Some(&error));
                        notification
                    }
                };
                app.send_notification(Some("shell-extension"), &notification);
            })
            .build(),
        // "Turn On" for an installed AppIndicator extension: the tray icon
        // shows at once.
        gio::ActionEntry::builder("tray-host-on")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(|app: &DesklinkApplication, _, uuid| {
                let Some(uuid) = uuid.and_then(|uuid| uuid.get::<String>()) else {
                    return;
                };
                if !APPINDICATOR_UUIDS.contains(&uuid.as_str()) {
                    return;
                }
                let app = app.clone();
                enable_extension(&uuid, move |result| {
                    app.withdraw_notification("shell-extension");
                    if let Err(error) = result {
                        let notification = crate::application::desk_notification(
                            "Could not show DeskLink in the top bar",
                        );
                        notification.set_body(Some(&error));
                        app.send_notification(Some("shell-extension"), &notification);
                    }
                });
            })
            .build(),
        gio::ActionEntry::builder("log-out")
            .activate(|_: &DesklinkApplication, _, _| log_out())
            .build(),
        gio::ActionEntry::builder("shell-extension-dismiss")
            .activate(|app: &DesklinkApplication, _, _| {
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(DaemonCommand::SetShellExtensionPromptDismissed(true));
                }
            })
            .build(),
    ]);

    // An installed copy of ours from an older DeskLink: brought up to date
    // (it was turned on before, so this is no new choice), and the user told
    // when it shows. Once registration has finished: during `dbus_register`
    // the application is not registered yet, so `is_remote` and
    // `send_notification` only assert.
    let weak = app.downgrade();
    glib::idle_add_local_once(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.is_remote()
            || !is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
            || !user_extension_dir().join("metadata.json").is_file()
        {
            return;
        }
        match install_extension_files(&user_extension_dir()) {
            Ok(true) => {
                let notification =
                    crate::application::desk_notification("DeskLink's top-bar menu was updated");
                notification.set_body(Some(RELOGIN_NOTE));
                notification.add_button("Log Out Now", "app.log-out");
                app.send_notification(Some("shell-extension"), &notification);
            }
            Ok(false) => {}
            Err(error) => eprintln!("[Shell] {error}"),
        }
    });

    let weak = app.downgrade();
    // Late enough for the tray to have found any host.
    glib::timeout_add_seconds_local_once(10, move || {
        let Some(app) = weak.upgrade() else { return };
        let wanted = !app.is_remote()
            && !crate::device_links::pty::host::in_flatpak()
            && is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
            && !crate::device_links::config::app_settings().shell_extension_prompt_dismissed
            && !app.tray_hosted();
        let Some(connection) = app.dbus_connection().filter(|_| wanted) else {
            return;
        };
        // An AppIndicator extension that is only off: the one-click fix
        // (not needed while our own extension shows DeskLink).
        if let Some(uuid) = appindicator_off().filter(|_| !extension_active()) {
            let notification =
                crate::application::desk_notification("Show DeskLink in the top bar");
            notification.set_body(Some(
                "Turn on the AppIndicator extension that is already installed to see DeskLink's icon in the top bar.",
            ));
            notification.add_button_with_target_value(
                "Turn On",
                "app.tray-host-on",
                Some(&uuid.to_variant()),
            );
            notification.add_button("Not now", "app.shell-extension-dismiss");
            app.send_notification(Some("shell-extension"), &notification);
            return;
        }
        // Fails when the Shell's extension service is absent: no offer then.
        call_extensions(
            &connection,
            "GetExtensionInfo",
            EXTENSION_UUID,
            move |reply| {
                let Ok(reply) = reply else { return };
                if is_active(&extension_info(&reply)) {
                    return;
                }
                let notification =
                    crate::application::desk_notification("Show DeskLink in the top bar");
                notification.set_body(Some(
                    "See your phone's battery and connection, and act on it, from the top bar.",
                ));
                notification.add_button("Turn On", "app.shell-extension-show");
                notification.add_button("Not now", "app.shell-extension-dismiss");
                app.send_notification(Some("shell-extension"), &notification);
            },
        );
    });
}

/// Whether DeskLink's top-bar extension is running, so it already shows
/// sessions (kept current from `ExtensionStateChanged`, not polled).
pub fn extension_active() -> bool {
    EXTENSION_IS_ACTIVE.with(|active| active.get())
}

/// `enabled-extensions` with ours added, once.
pub(super) fn with_extension(enabled: Vec<String>) -> Vec<String> {
    let mut enabled = enabled;
    if !enabled.iter().any(|uuid| uuid == EXTENSION_UUID) {
        enabled.push(EXTENSION_UUID.to_string());
    }
    enabled
}

/// `enabled-extensions` without ours; every other extension stays.
pub(super) fn without_extension(enabled: Vec<String>) -> Vec<String> {
    enabled
        .into_iter()
        .filter(|uuid| uuid != EXTENSION_UUID)
        .collect()
}

/// "Turn off": disables only DeskLink's extension. The system-wide
/// `disable-user-extensions` switch that "Turn On" may have flipped is left
/// as it is (other extensions may depend on it now).
pub fn turn_off_extension() -> Result<(), String> {
    let settings = gio::Settings::new("org.gnome.shell");
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
            without_extension(strings("enabled-extensions")),
        )
        .map_err(|error| error.to_string())?;
    let mut disabled = strings("disabled-extensions");
    if !disabled.iter().any(|uuid| uuid == EXTENSION_UUID) {
        disabled.push(EXTENSION_UUID.to_string());
    }
    settings
        .set_strv("disabled-extensions", disabled)
        .map_err(|error| error.to_string())?;
    gio::Settings::sync();
    Ok(())
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
    let written = install_extension_files(&user_extension_dir())?;
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
