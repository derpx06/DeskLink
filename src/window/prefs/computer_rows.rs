//! The computer-wide rows: sound, forwarding, calls, clipboard, downloads,
//! history, the system check, logs, about, keep awake, start at login, the top
//! bar, appearance, the relay line and the session password.
//!
//! Sibling edges: none (the parent's icon and markup helpers). Outside:
//! `config`, `shell_dbus` (the extension), `session_lock`, `relay_link`,
//! `platform`, `window` (history, system check, toasts).

use super::*;

/// Sound (F1 A3): whether a phone may hear this computer's microphone.
pub(super) fn sound_group(daemon: &DaemonHandle, settings: &AppSettings) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Sound").build();
    let mic = adw::SwitchRow::builder()
        .title("Phones may hear this computer's microphone")
        .subtitle("Only when a phone asks. A coral bar shows while it listens.")
        .active(settings.desk_mic_to_phone)
        .build();
    with_icon(&mic, "audio-input-microphone-symbolic");
    let sender = daemon.clone();
    mic.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetDeskMicToPhone(row.is_active()))
    });
    // No PipeWire (or its GStreamer plugin): off, with the reason.
    if let Some(reason) = crate::platform::reason("sound") {
        mic.set_sensitive(false);
        mic.set_subtitle(&esc(reason));
        mic.set_subtitle_lines(0);
    }
    group.add(&mic);
    group
}

/// Notifications to Phone (C1): this computer's notifications on the
/// phones, off by default and only from the apps ticked here (the apps seen
/// since DeskLink started, and those already chosen).
pub(super) fn add_forward_rows(
    group: &adw::PreferencesGroup,
    daemon: &DaemonHandle,
    settings: &AppSettings,
) {
    if let Some(reason) = crate::device_links::desk_notifications::unavailable() {
        let row = adw::ActionRow::builder()
            .title("Send this computer's notifications to the phone")
            .subtitle(esc(reason))
            .build();
        with_icon(&row, "preferences-system-notifications-symbolic");
        group.add(&row);
        return;
    }
    let on = adw::SwitchRow::builder()
        .title("Send this computer's notifications to the phone")
        .subtitle("Only from the apps you choose. The phone's own are never sent back.")
        .active(settings.forward_notifications)
        .build();
    with_icon(&on, "preferences-system-notifications-symbolic");
    let sender = daemon.clone();
    on.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetForwardNotifications(row.is_active()))
    });
    group.add(&on);
    if !settings.forward_notifications {
        return;
    }
    let mut apps = crate::device_links::desk_notifications::seen_apps();
    for key in &settings.forward_apps {
        apps.entry(key.clone()).or_insert_with(|| key.clone());
    }
    if apps.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No notifications yet")
            .subtitle("Apps appear here once they show a notification")
            .build();
        group.add(&row);
    }
    for (key, name) in apps {
        let row = adw::SwitchRow::builder()
            .title(&name)
            .subtitle(if name == key { "" } else { key.as_str() })
            .use_markup(false)
            .active(settings.forward_apps.contains(&key))
            .build();
        let sender = daemon.clone();
        row.connect_active_notify(move |row| {
            sender.send(DaemonCommand::SetForwardApp(key.clone(), row.is_active()))
        });
        group.add(&row);
    }
}

/// Calls: answered here; callers can be named from the phone's contacts.
pub(super) fn calls_group(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Calls")
        .description(glib::markup_escape_text(&format!(
            "Answer, decline and end {}'s calls here.",
            device.name
        )))
        .build();
    group.add(&pause_music_row(daemon, settings));
    for (title, subtitle, icon, on, send, read) in [
        (
            "Show calls on this computer",
            "Applies to every paired phone. A call bar, a notification with Answer and Decline, and missed calls. Off: calls only ring on the phone.",
            "call-start-symbolic",
            settings.calls_on_computer,
            DaemonCommand::SetCallsOnComputer as fn(bool) -> DaemonCommand,
            (|settings: &AppSettings, _: &str| settings.calls_on_computer) as fn(&AppSettings, &str) -> bool,
        ),
        (
            "Show caller names on the lock screen",
            "Off: while this computer is locked, a call says only \"Incoming call\".",
            "changes-prevent-symbolic",
            settings.caller_on_lock_screen,
            DaemonCommand::SetCallerOnLockScreen,
            |settings: &AppSettings, _: &str| settings.caller_on_lock_screen,
        ),
        (
            "Ring on this computer",
            "A soft sound here while a phone rings. Quiet in Do Not Disturb.",
            "audio-volume-low-symbolic",
            settings.ring_on_computer,
            DaemonCommand::SetRingOnComputer,
            |settings: &AppSettings, _: &str| settings.ring_on_computer,
        ),
    ] {
        let row = adw::SwitchRow::builder()
            .title(title)
            .subtitle(subtitle)
            .active(on)
            .build();
        with_icon(&row, icon);
        let daemon = daemon.clone();
        toggled(&row, move |row| daemon.send(send(row.is_active())));
        follow(&row, read);
        group.add(&row);
    }
    // F5 K4: names from the contacts the phone shares, off until asked.
    let names = adw::SwitchRow::builder()
        .title(esc(format!("Name callers from {}'s contacts", device.name)))
        .subtitle("Uses the contacts the phone shares with this computer")
        .active(settings.names_callers_from(&device.id))
        .build();
    with_icon(&names, "avatar-default-symbolic");
    let (sender, id) = (daemon.clone(), device.id.clone());
    names.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetContactCallerNames(
            id.clone(),
            row.is_active(),
        ))
    });
    group.add(&names);
    group
}

/// "Pause music during calls": this computer's players pause while a call
/// rings or runs on the phone, and play again after.
fn pause_music_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Pause music during calls")
        .subtitle(COMPUTER_WIDE_MUSIC_SUBTITLE)
        .active(settings.pause_media_on_call)
        .build();
    with_icon(&row, "media-playback-pause-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetPauseMediaOnCall(row.is_active()))
    });
    follow(&row, |settings, _| settings.pause_media_on_call);
    row
}

/// "Share the clipboard": the same switch as the Notes & Clipboard header.
/// A computer-wide switch on a per-phone page says so (J18): it was read as
/// belonging to the phone whose page it sits on.
pub(super) const COMPUTER_WIDE_CLIPBOARD_TITLE: &str =
    "Share this computer's clipboard with phones";
pub(super) const COMPUTER_WIDE_CLIPBOARD_SUBTITLE: &str =
    "Applies to every paired phone. Copy on one device, paste on the other. Passwords are never shared.";
pub(super) const COMPUTER_WIDE_MUSIC_SUBTITLE: &str =
    "Applies to every paired phone. This computer's players pause while a call rings or runs, and carry on after";

pub(super) fn clipboard_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title(COMPUTER_WIDE_CLIPBOARD_TITLE)
        .subtitle(COMPUTER_WIDE_CLIPBOARD_SUBTITLE)
        .active(settings.clipboard_sync)
        .build();
    with_icon(&row, "edit-paste-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetClipboardSync(row.is_active()))
    });
    follow(&row, |settings, _| settings.clipboard_sync);
    row
}

/// Where received files land, one click away.
pub(super) fn downloads_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Downloads folder")
        .subtitle("Files from your phone are saved in your Downloads folder. Click to open it.")
        .activatable(true)
        .build();
    with_icon(&row, "folder-download-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let Some(dir) = crate::device_links::transfer_log::downloads_dir() else {
            return;
        };
        gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(
            row.root().and_downcast_ref::<gtk::Window>(),
            gio::Cancellable::NONE,
            |result| {
                if let Err(error) = result {
                    eprintln!("[UI] Could not open Downloads: {error}");
                }
            },
        );
    });
    row
}

/// The script a Flatpak user saves for their file manager (the sandbox cannot
/// write the host's menus itself).
pub(super) const FLATPAK_SCRIPT: &str =
    "#!/bin/sh\nexec flatpak run derx06.desklink.com --send-files -- \"$@\"\n";

/// "Show Send to phone in the file manager": a switch; in a Flatpak (which
/// cannot write the host's menus) a row that shows the commands to copy.
pub(super) fn file_manager_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::PreferencesRow {
    if crate::device_links::pty::host::in_flatpak() {
        let row = adw::ActionRow::builder()
            .title("Show Send to phone in the file manager")
            .subtitle("This Flatpak can't add it to your file manager. Copy the commands to add it yourself.")
            .subtitle_lines(0)
            .build();
        with_icon(&row, "document-send-symbolic");
        let button = gtk::Button::builder()
            .label("Copy the commands…")
            .valign(gtk::Align::Center)
            .build();
        button.connect_clicked(|button| {
            let body = format!(
                "Run this to send files: flatpak run derx06.desklink.com --send-files -- FILES\n\nFor Files (Nautilus): save this as an executable file named \"Send to phone\" in ~/.local/share/nautilus/scripts (Nemo: ~/.local/share/nemo/scripts; Dolphin: a service menu running the same command):\n\n{FLATPAK_SCRIPT}"
            );
            let dialog = adw::AlertDialog::new(Some("Send to phone from a Flatpak"), Some(&body));
            dialog.add_responses(&[("close", "_Close"), ("copy", "_Copy the script")]);
            dialog.set_close_response("close");
            dialog.connect_response(Some("copy"), |_, _| {
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(FLATPAK_SCRIPT);
                }
            });
            dialog.present(Some(button));
        });
        row.add_suffix(&button);
        return row.upcast();
    }
    let row = adw::SwitchRow::builder()
        .title("Show Send to phone in the file manager")
        .subtitle("Adds Send to phone to the right-click menu of Files, Dolphin, Nemo and Thunar. In Files, restart it once (nautilus -q) after turning this on. Turning it off removes every entry. \"Open With\" entries follow this switch.")
        .subtitle_lines(0)
        .active(settings.file_manager_send)
        .build();
    with_icon(&row, "document-send-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetFileManagerSend(row.is_active()))
    });
    follow(&row, |settings, _| settings.file_manager_send);
    row.upcast()
}

/// "Connection history": the log of connections and sessions, in a dialog.
pub(super) fn history_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Connection history")
        .subtitle("Every connection and session from a phone, kept for 30 days")
        .activatable(true)
        .build();
    with_icon(&row, "document-open-recent-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let types = row
            .root()
            .and_downcast::<DesklinkWindow>()
            .and_then(|window| window.daemon().map(|daemon| (window, daemon)))
            .map(|(window, daemon)| {
                window
                    .known_devices(&daemon)
                    .into_iter()
                    .map(|known| (known.id, known.device_type))
                    .collect()
            })
            .unwrap_or_default();
        crate::window::history::show_dialog(row, &types, None);
    });
    row
}

/// "Logs": where crash reports are kept.
pub(super) fn system_check_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("System check")
        .subtitle("What works on this computer, and what to install for the rest")
        .activatable(true)
        .build();
    with_icon(&row, "preferences-system-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(crate::window::system_check::show_dialog);
    row
}

pub(super) fn logs_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Crash reports")
        .subtitle("Opens the folder DeskLink writes a report to if it ever crashes")
        .activatable(true)
        .build();
    with_icon(&row, "text-x-generic-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let Some(dir) = crate::crash_dir() else {
            return;
        };
        let _ = std::fs::create_dir_all(&dir);
        let (shown, widget) = (dir.display().to_string(), row.clone());
        gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(
            row.root().and_downcast_ref::<gtk::Window>(),
            gio::Cancellable::NONE,
            move |result| {
                if let Err(error) = result {
                    eprintln!("[UI] Could not open the crash folder: {error}");
                    // No file manager (a bare window manager) is not a silent no-op.
                    crate::window::toast_from(
                        &widget,
                        &format!("No file manager opened it. The reports are in {shown}"),
                    );
                }
            },
        );
    });
    row
}

/// "About": straight to the About dialog, also over the Settings dialog
/// (where the `app.about` action would wait for that dialog to close).
pub(super) fn about_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("About")
        .subtitle("Version, what's new, report a problem, credits")
        .activatable(true)
        .build();
    with_icon(&row, "desklink-info-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let daemon = row
            .root()
            .and_downcast::<DesklinkWindow>()
            .and_then(|window| window.daemon());
        crate::about::present(row, daemon.as_ref());
    });
    row
}

pub(super) fn keep_awake_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Keep this computer awake while a phone is connected")
        .subtitle("Stops this computer going to sleep, even with the lid closed, while a paired phone is connected. It sleeps again when the phone goes away.")
        .active(settings.stay_awake)
        .build();
    with_icon(&row, "weather-clear-night-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetStayAwake(row.is_active()))
    });
    follow(&row, |settings, _| settings.stay_awake);
    row
}

pub(super) fn start_at_login_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Start at login")
        .subtitle(crate::platform::note("autostart").map_or_else(
            || glib::GString::from("Opens DeskLink in the background when you log in, so your phone can reach this computer"),
            esc,
        ))
        .subtitle_lines(0)
        .active(settings.start_at_login.unwrap_or(true))
        .build();
    with_icon(&row, "system-shutdown-symbolic");
    let daemon = daemon.clone();
    row.connect_active_notify(move |row| {
        daemon.send(DaemonCommand::SetStartAtLogin(row.is_active()))
    });
    row
}

/// The extension's own top-bar item, besides its Quick Settings entry.
fn top_bar_pill_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Phone in the top bar")
        .subtitle("Also show the phone as its own item, not only in Quick Settings")
        .active(settings.top_bar_pill)
        .build();
    with_icon(&row, "phone-symbolic");
    let daemon = daemon.clone();
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetTopBarPill(row.is_active()))
    });
    follow(&row, |settings, _| settings.top_bar_pill);
    row
}

fn tray_hosted() -> bool {
    gio::Application::default()
        .and_downcast::<crate::application::DesklinkApplication>()
        .is_some_and(|app| app.tray_hosted())
}

/// What the Top Bar group shows, to redraw it when that changes.
pub(super) fn top_bar_key() -> String {
    format!(
        "{} {:?} {:?}",
        tray_hosted(),
        crate::shell_dbus::extension_status(),
        crate::shell_dbus::appindicator_off()
    )
}

/// "Log out now…" (GNOME asks to confirm).
fn log_out_button() -> gtk::Button {
    let button = gtk::Button::builder()
        .label("Log out now…")
        .valign(gtk::Align::Center)
        .build();
    button.connect_clicked(|_| crate::shell_dbus::log_out());
    button
}

/// Top Bar: whether DeskLink shows there, and the one step that fixes it.
/// The tray icon is the freedesktop way (KDE, most desktops; GNOME with the
/// AppIndicator extension); GNOME also has DeskLink's own extension.
pub(super) fn add_top_bar_rows(
    group: &adw::PreferencesGroup,
    daemon: &DaemonHandle,
    settings: &AppSettings,
) {
    use crate::shell_dbus::ExtensionStatus;
    let gnome = crate::shell_dbus::extension_possible();

    let tray = adw::ActionRow::builder().title("Tray icon").build();
    with_icon(&tray, "phone-symbolic");
    let indicator_off = crate::shell_dbus::appindicator_off();
    tray.set_subtitle(&if tray_hosted() {
        "Shown".to_string()
    } else if indicator_off.is_some() {
        "Not shown — the AppIndicator extension is installed but off".to_string()
    } else if gnome {
        "Not available — GNOME shows tray icons only with the AppIndicator extension".to_string()
    } else {
        // Each desktop's own step (Plasma's System Tray, Waybar's tray module…).
        format!("Not shown — {}", crate::desktop_env::current().tray_setup())
    });
    if let Some(uuid) = indicator_off.filter(|_| !tray_hosted()) {
        let button = gtk::Button::builder()
            .label("Turn on")
            .valign(gtk::Align::Center)
            .build();
        button.update_property(&[gtk::accessible::Property::Label(
            "Turn on the AppIndicator extension",
        )]);
        button.connect_clicked(move |button| {
            button.set_sensitive(false);
            let weak = button.downgrade();
            crate::shell_dbus::enable_extension(&uuid, move |result| {
                let Some(button) = weak.upgrade() else { return };
                button.set_sensitive(true);
                if let Err(error) = result {
                    crate::window::toast_from(
                        &button,
                        &format!("Could not turn it on{}", crate::window::because(&error)),
                    );
                }
            });
        });
        tray.add_suffix(&button);
    }
    group.add(&tray);

    if !gnome {
        return;
    }
    let menu = adw::ActionRow::builder().title("GNOME extension").build();
    with_icon(&menu, "view-app-grid-symbolic");
    match crate::shell_dbus::extension_status() {
        ExtensionStatus::Active => {
            menu.set_subtitle(
                "Active — your phone, live sessions and quick actions in the top bar",
            );
            let off = gtk::Button::builder()
                .label("Turn off")
                .valign(gtk::Align::Center)
                .build();
            off.update_property(&[gtk::accessible::Property::Label(
                "Turn off the top-bar menu",
            )]);
            off.connect_clicked(|button| match crate::shell_dbus::turn_off_extension() {
                Ok(()) => {
                    button.set_sensitive(false);
                    crate::window::toast_from(button, "Top-bar menu turned off");
                }
                Err(error) => crate::window::toast_from(
                    button,
                    &format!("Could not turn it off{}", crate::window::because(&error)),
                ),
            });
            menu.add_suffix(&off);
        }
        ExtensionStatus::NeedsLogin => {
            menu.set_subtitle("Installed — it appears after you log in again");
            menu.add_suffix(&log_out_button());
        }
        ExtensionStatus::Off => {
            menu.set_subtitle(
                "Off — your phone, live sessions and quick actions in the top bar, and Unlock on the lock screen. Turning it on also allows GNOME user extensions (a setting for all of them); turning it off disables only this one.",
            );
            let button = gtk::Button::builder()
                .label("Turn on")
                .valign(gtk::Align::Center)
                .build();
            button.update_property(&[gtk::accessible::Property::Label("Turn on the top-bar menu")]);
            button.connect_clicked(|button| match crate::shell_dbus::turn_on_extension() {
                Ok(true) => {
                    let dialog = adw::AlertDialog::new(
                        Some("Top bar menu installed"),
                        Some(crate::shell_dbus::RELOGIN_NOTE),
                    );
                    dialog.add_response("later", "_Later");
                    dialog.add_response("log-out", "_Log out now…");
                    dialog.set_close_response("later");
                    dialog.connect_response(Some("log-out"), |_, _| crate::shell_dbus::log_out());
                    dialog.present(Some(button));
                }
                Ok(false) => crate::window::toast_from(button, "Top-bar menu turned on"),
                Err(error) => {
                    eprintln!("[UI] Could not turn on the top-bar menu: {error}");
                    let dialog = adw::AlertDialog::new(
                        Some("Could not turn on the top bar menu"),
                        Some(&error),
                    );
                    dialog.add_response("ok", "OK");
                    dialog.present(Some(button));
                }
            });
            menu.add_suffix(&button);
        }
    }
    group.add(&menu);
    group.add(&top_bar_pill_row(daemon, settings));
}

/// System, Light or Dark.
/// A row with the menu as its suffix rather than an `AdwComboRow`, whose
/// value label ellipsizes first: at large text it read "S…". Here the
/// title and subtitle wrap and the choice keeps its full width.
pub(super) fn appearance_row() -> adw::ActionRow {
    use crate::application::{appearance_scheme, load_appearance, save_appearance};
    let choices = ["system", "light", "dark"];
    let row = adw::ActionRow::builder()
        .title("Style")
        .subtitle("Light, dark, or the same as your system")
        .build();
    row.add_prefix(&icon_square(
        "preferences-desktop-appearance-symbolic",
        30,
        "row-icon",
    ));
    let menu = gtk::DropDown::from_strings(&["System", "Light", "Dark"]);
    menu.set_valign(gtk::Align::Center);
    menu.set_selected(
        choices
            .iter()
            .position(|choice| *choice == load_appearance())
            .unwrap_or(0) as u32,
    );
    menu.update_property(&[gtk::accessible::Property::Label("Style")]);
    menu.connect_selected_notify(move |menu| {
        let choice = choices
            .get(menu.selected() as usize)
            .copied()
            .unwrap_or("system");
        save_appearance(choice);
        adw::StyleManager::default().set_color_scheme(appearance_scheme(choice));
    });
    row.add_suffix(&menu);
    row.set_activatable_widget(Some(&menu));
    row
}

/// With the relay compiled in (`relay_enabled`, i.e. `RELAY_ENABLED`): one
/// read-only line, where the relay is and how it is doing. Never a switch,
/// an address field or a TURN choice: the constant is the only control, and
/// the address lives in config.json. `None` with the relay compiled out.
pub(super) fn relay_status_row(relay_enabled: bool) -> Option<adw::ActionRow> {
    if !relay_enabled {
        return None;
    }
    let row = adw::ActionRow::builder()
        .title("Away from home")
        .use_markup(false)
        .activatable(false)
        .build();
    row.add_prefix(&icon_square("desklink-globe-symbolic", 30, "row-icon"));
    let update = |row: &adw::ActionRow| {
        use crate::device_links::relay_link::client::{status, RelayStatus};
        let settings = crate::device_links::config::app_settings();
        let state = match status() {
            _ if !settings.relay_configured() => "no relay server built in".to_string(),
            RelayStatus::Off => "waiting for a paired phone".to_string(),
            RelayStatus::Connecting => "connecting…".to_string(),
            RelayStatus::Online => "ready · end-to-end encrypted".to_string(),
            RelayStatus::Unavailable(reason) => format!("unavailable: {reason}"),
            RelayStatus::UpdateRequired => "update DeskLink to connect away from home".to_string(),
        };
        row.set_subtitle(&match relay_host(settings.relay_url()) {
            "" => state,
            host => format!("{host} · {state}"),
        });
    };
    update(&row);
    let weak = row.downgrade();
    crate::ticker::every_tick(move || match weak.upgrade() {
        Some(row) => {
            update(&row);
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    Some(row)
}

/// "relay.desklink.app" from "wss://relay.desklink.app/v1".
pub(super) fn relay_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

fn password_subtitle(set: bool) -> &'static str {
    if set {
        "On: a phone enters this computer's password once per connection before it opens a terminal or sees the screen"
    } else {
        "Off: pairing alone lets a phone open a terminal or see the screen"
    }
}

/// "Ask before terminal and screen access": the session password, on or
/// off, and Change… while it is on.
pub(super) fn password_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::ActionRow {
    let set = settings.session_password.is_some();
    let row = adw::ActionRow::builder()
        .title("Ask before terminal and screen access")
        .subtitle(password_subtitle(set))
        .build();
    row.add_prefix(&icon_square("auth-fingerprint-symbolic", 30, "row-icon"));
    let change = gtk::Button::builder()
        .label("Change…")
        .valign(gtk::Align::Center)
        .visible(set)
        .css_classes(["flat"])
        .build();
    let switch = gtk::Switch::builder()
        .active(set)
        .valign(gtk::Align::Center)
        .build();
    switch.update_property(&[gtk::accessible::Property::Label(
        "Ask for a password before terminal and screen access",
    )]);
    row.add_suffix(&change);
    row.add_suffix(&switch);
    row.set_activatable_widget(Some(&switch));

    // Weak references: the controls live inside the row they update.
    let (shown_row, shown_change) = (row.downgrade(), change.downgrade());
    let show = move |set: bool| {
        if let Some(row) = shown_row.upgrade() {
            row.set_subtitle(password_subtitle(set));
        }
        if let Some(change) = shown_change.upgrade() {
            change.set_visible(set);
        }
    };
    let show = std::rc::Rc::new(show);
    {
        let (daemon, show) = (daemon.clone(), show.clone());
        change.connect_clicked(move |button| {
            let (daemon, show) = (daemon.clone(), show.clone());
            ask_for_password(button, move |password| {
                if let Some(password) = password {
                    daemon.send(DaemonCommand::SetSessionPassword(Some(password)));
                    show(true);
                }
            });
        });
    }
    let daemon = daemon.clone();
    switch.connect_state_set(move |switch, on| {
        if !on {
            // Also reached when a cancelled dialog puts the switch back.
            if crate::device_links::config::app_settings()
                .session_password
                .is_some()
            {
                daemon.send(DaemonCommand::SetSessionPassword(None));
            }
            show(false);
            return glib::Propagation::Proceed;
        }
        // On only once a password is chosen; cancelling leaves it off.
        let (daemon, show, shown) = (daemon.clone(), show.clone(), switch.downgrade());
        ask_for_password(switch, move |password| {
            let Some(switch) = shown.upgrade() else {
                return;
            };
            match password {
                Some(password) => {
                    daemon.send(DaemonCommand::SetSessionPassword(Some(password)));
                    switch.set_state(true);
                    show(true);
                }
                None => {
                    switch.set_state(false);
                    switch.set_active(false);
                }
            }
        });
        glib::Propagation::Stop
    });
    row
}

/// Asks for a new session password twice; `done` gets it when both match,
/// or `None` when the dialog is cancelled.
fn ask_for_password(parent: &impl IsA<gtk::Widget>, done: impl Fn(Option<String>) + 'static) {
    let entry = adw::PasswordEntryRow::builder()
        .title("New password")
        .build();
    let confirm = adw::PasswordEntryRow::builder()
        .title("Confirm password")
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    list.append(&confirm);

    let dialog = adw::AlertDialog::new(
        Some("Session password"),
        Some("Your phone asks for this once per connection before it can view or control the screen or open a terminal."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", "Set password")]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("ok", false);
    dialog.set_extra_child(Some(&list));

    // Weak: `check` is connected on the entries and enables the dialog.
    let check = {
        let dialog = dialog.downgrade();
        let entry = entry.downgrade();
        let confirm = confirm.downgrade();
        move || {
            let (Some(dialog), Some(entry), Some(confirm)) =
                (dialog.upgrade(), entry.upgrade(), confirm.upgrade())
            else {
                return;
            };
            let password = entry.text();
            let usable = !password.is_empty()
                && password.len() <= crate::device_links::session_lock::MAX_PASSWORD_BYTES
                && password == confirm.text();
            dialog.set_response_enabled("ok", usable);
        }
    };
    entry.connect_changed({
        let check = check.clone();
        move |_| check()
    });
    confirm.connect_changed(move |_| check());

    dialog.choose(parent, gio::Cancellable::NONE, move |response| {
        done((response == "ok").then(|| entry.text().to_string()));
    });
}
