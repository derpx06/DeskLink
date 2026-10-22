//! The Preferences page, for the selected phone and this computer: Access,
//! Connection and Appearance, and Unpair. With no phone paired yet the
//! computer-wide part is offered as a dialog instead.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use super::{icon_square, DesklinkWindow};
use crate::device_links::config::AppSettings;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::DeviceView;

/// A switch that follows a setting a phone can also change, and how to read
/// it. Synced in place each refresh: rebuilding the page would move focus.
type LiveSwitch = (
    glib::WeakRef<adw::SwitchRow>,
    fn(&AppSettings, &str) -> bool,
);

thread_local! {
    static LIVE: std::cell::RefCell<Vec<LiveSwitch>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn follow(row: &adw::SwitchRow, read: fn(&AppSettings, &str) -> bool) {
    LIVE.with(|live| live.borrow_mut().push((row.downgrade(), read)));
}

/// Shows changes made from a phone (or anywhere) on the open switches.
fn sync_live(device_id: &str) {
    let settings = crate::device_links::config::app_settings();
    LIVE.with(|live| {
        live.borrow_mut().retain(|(row, read)| {
            let Some(row) = row.upgrade() else {
                return false;
            };
            let value = read(&settings, device_id);
            if row.is_active() != value {
                row.set_active(value);
            }
            true
        })
    });
}

pub(super) fn update(win: &DesklinkWindow, daemon: &DaemonHandle, device: &DeviceView) {
    sync_live(&device.id);
    let imp = win.imp();
    let approving = imp.approving.borrow().contains(&device.id);
    let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
    // Only what the switches cannot show by themselves: a switch the user
    // just flipped already says so, and redrawing would move focus.
    let settings = crate::device_links::config::app_settings();
    // Read only while Bluetooth call audio is on (BlueZ, about 1 ms).
    let bonded = settings
        .call_audio
        .then(crate::device_links::call_audio::bonded_phones);
    let key = serde_json::to_string(&(
        &device.id,
        &device.name,
        approving,
        approved,
        device.terminal_access,
        &settings.run_commands,
        settings.relay_enabled,
        top_bar_key(),
        settings.forward_notifications,
        &settings.forward_apps,
        crate::device_links::desk_notifications::seen_apps(),
        settings.call_audio,
        settings.desk_mic_to_phone,
        settings.call_audio_devices.get(&device.id),
        &bonded,
    ))
    .unwrap_or_default();
    if !win.page_changed("preferences", key) {
        return;
    }
    LIVE.with(|live| live.borrow_mut().clear());
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(22)
        .margin_top(8)
        .margin_bottom(28)
        .margin_start(28)
        .margin_end(28)
        .build();

    let access = adw::PreferencesGroup::builder().title("Access").build();
    access.add(&password_row(daemon, &settings));
    access.add(&remote_control_row(device, approving));
    access.add(&terminal_access_row(daemon, device));
    access.add(&unlock_nearby_row(daemon, &settings, device));
    access.add(&command_alerts_row(daemon, &settings, device));
    page.append(&access);

    let connection = adw::PreferencesGroup::builder().title("Connection").build();
    connection.add(&away_row(daemon, &settings));
    connection.add(&keep_awake_row(daemon, &settings));
    let notifications = adw::SwitchRow::builder()
        .title("Show phone notifications")
        .subtitle("Here and on your desktop, with replies")
        .active(settings.shows_notifications(&device.id))
        .build();
    notifications.add_prefix(&icon_square(
        "preferences-system-notifications-symbolic",
        30,
        "row-icon",
    ));
    let (sender, id) = (daemon.clone(), device.id.clone());
    notifications.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetShowNotifications(
            id.clone(),
            row.is_active(),
        ));
    });
    follow(&notifications, |settings, id| {
        settings.shows_notifications(id)
    });
    connection.add(&notifications);
    page.append(&connection);

    page.append(&calls_group(win, daemon, &settings, device, bonded));
    page.append(&forward_group(daemon, &settings));
    page.append(&sound_group(daemon, &settings));

    page.append(&run_commands_group(win, daemon, &settings, &device.name));

    let look = adw::PreferencesGroup::builder().title("Appearance").build();
    look.add(&appearance_row());
    look.add(&start_at_login_row(daemon, &settings));
    page.append(&look);
    page.append(&top_bar_group(daemon, &settings));

    page.append(&unpair_button(daemon, device));
    win.replace_body(
        "preferences",
        &adw::Clamp::builder().maximum_size(720).child(&page).build(),
    );
}

/// The computer-wide preferences as a dialog, for when no phone is paired.
pub(crate) fn show_dialog(parent: &impl IsA<gtk::Widget>, daemon: &DaemonHandle) {
    let settings = crate::device_links::config::app_settings();
    let dialog = adw::PreferencesDialog::builder().content_width(640).build();
    let page = adw::PreferencesPage::new();
    let access = adw::PreferencesGroup::builder().title("Access").build();
    access.add(&password_row(daemon, &settings));
    page.add(&access);
    let connection = adw::PreferencesGroup::builder().title("Connection").build();
    connection.add(&away_row(daemon, &settings));
    connection.add(&keep_awake_row(daemon, &settings));
    page.add(&connection);
    let look = adw::PreferencesGroup::builder().title("Appearance").build();
    look.add(&appearance_row());
    look.add(&start_at_login_row(daemon, &settings));
    page.add(&look);
    page.add(&top_bar_group(daemon, &settings));
    dialog.add(&page);
    dialog.present(Some(parent));
}

/// Sound (F1 A3): whether a phone may hear this computer's microphone.
fn sound_group(daemon: &DaemonHandle, settings: &AppSettings) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Sound").build();
    let mic = adw::SwitchRow::builder()
        .title("Phones may hear this computer's microphone")
        .subtitle("Only when one asks; a coral bar shows while it listens")
        .active(settings.desk_mic_to_phone)
        .build();
    with_icon(&mic, "audio-input-microphone-symbolic");
    let sender = daemon.clone();
    mic.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetDeskMicToPhone(row.is_active()))
    });
    group.add(&mic);
    group
}

/// Notifications to Phone (C1): this computer's notifications on the
/// phones, off by default and only from the apps ticked here (the apps seen
/// since DeskLink started, and those already chosen).
fn forward_group(daemon: &DaemonHandle, settings: &AppSettings) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Notifications to Phone")
        .description("Only from the apps you choose. The phone's own are never sent back.")
        .build();
    let on = adw::SwitchRow::builder()
        .title("Send this computer's notifications")
        .active(settings.forward_notifications)
        .build();
    with_icon(&on, "preferences-system-notifications-symbolic");
    let sender = daemon.clone();
    on.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetForwardNotifications(row.is_active()))
    });
    group.add(&on);
    if !settings.forward_notifications {
        return group;
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
    group
}

/// Calls: answered here by default; with the experimental switch, also heard
/// and spoken here over Bluetooth once the user confirmed which bonded
/// device is this phone.
fn calls_group(
    win: &DesklinkWindow,
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
    bonded: Option<Option<Vec<crate::device_links::call_audio::BondedPhone>>>,
) -> adw::PreferencesGroup {
    use crate::device_links::call_audio;
    let group = adw::PreferencesGroup::builder()
        .title("Calls")
        .description(format!(
            "Answer, decline and end {}'s calls here. Turn on Calls in DeskLink on the phone.",
            device.name
        ))
        .build();
    let audio = adw::SwitchRow::builder()
        .title("Use this computer's speaker and microphone")
        .subtitle("Experimental · over Bluetooth")
        .active(settings.call_audio)
        .build();
    with_icon(&audio, "audio-headset-symbolic");
    let sender = daemon.clone();
    audio.connect_active_notify(move |row| {
        sender.send(DaemonCommand::SetCallAudio(row.is_active()))
    });
    group.add(&audio);
    let Some(bonded) = bonded else {
        return group;
    };
    let settings_button = |label: &str| {
        let button = gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .css_classes(["soft-button"])
            .build();
        let win = win.downgrade();
        button.connect_clicked(move |_| {
            if !call_audio::open_bluetooth_settings() {
                if let Some(win) = win.upgrade() {
                    win.show_feature_toast("Open your Bluetooth settings to pair the phone");
                }
            }
        });
        button
    };
    let Some(phones) = bonded else {
        let row = adw::ActionRow::builder()
            .title("Bluetooth is off or not available")
            .subtitle("Turn it on to hear calls here")
            .build();
        with_icon(&row, "bluetooth-disabled-symbolic");
        row.add_suffix(&settings_button("Bluetooth Settings"));
        group.add(&row);
        return group;
    };
    let confirmed = settings
        .call_audio_devices
        .get(&device.id)
        .and_then(|address| phones.iter().find(|phone| &phone.address == address));
    if let Some(phone) = confirmed {
        let row = adw::ActionRow::builder()
            .title(&phone.name)
            .subtitle(if phone.connected {
                "Connected over Bluetooth · calls can use this computer"
            } else {
                "Not connected"
            })
            .use_markup(false)
            .build();
        with_icon(&row, "bluetooth-active-symbolic");
        let (label, connect) = if phone.connected {
            ("Return Calls to Phone", false)
        } else {
            ("Use This Computer", true)
        };
        let button = gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .css_classes(["soft-button"])
            .build();
        let (path, weak) = (phone.path.clone(), win.downgrade());
        button.connect_clicked(move |button| {
            button.set_sensitive(false);
            let (button, weak) = (button.downgrade(), weak.clone());
            call_audio::use_for_calls(&path, connect, move |result| {
                if let Some(button) = button.upgrade() {
                    button.set_sensitive(true);
                }
                if let (Err(error), Some(win)) = (result, weak.upgrade()) {
                    win.show_feature_toast(&format!("Bluetooth: {error}"));
                }
            });
        });
        row.add_suffix(&button);
        let forget = gtk::Button::builder()
            .icon_name("edit-delete-symbolic")
            .tooltip_text("Not this device")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        forget.update_property(&[gtk::accessible::Property::Label("Not this device")]);
        let (sender, id) = (daemon.clone(), device.id.clone());
        forget.connect_clicked(move |_| {
            sender.send(DaemonCommand::SetCallAudioDevice(id.clone(), None));
        });
        row.add_suffix(&forget);
        group.add(&row);
    } else if let Some(phone) = call_audio::likely_match(&phones, &device.name) {
        let row = adw::ActionRow::builder()
            .title(format!("Is “{}” {}?", phone.name, device.name))
            .subtitle("Paired with this computer over Bluetooth")
            .use_markup(false)
            .build();
        with_icon(&row, "bluetooth-active-symbolic");
        let button = gtk::Button::builder()
            .label("Yes, Use It")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        let (sender, id, address) = (daemon.clone(), device.id.clone(), phone.address.clone());
        button.connect_clicked(move |_| {
            sender.send(DaemonCommand::SetCallAudioDevice(
                id.clone(),
                Some(address.clone()),
            ));
        });
        row.add_suffix(&button);
        group.add(&row);
    } else {
        let row = adw::ActionRow::builder()
            .title(format!("{} is not paired over Bluetooth", device.name))
            .subtitle("Pair it once in Bluetooth settings")
            .use_markup(false)
            .build();
        with_icon(&row, "bluetooth-active-symbolic");
        row.add_suffix(&settings_button("Pair over Bluetooth"));
        group.add(&row);
    }
    group
}

fn with_icon(row: &impl IsA<adw::ActionRow>, icon: &str) {
    row.add_prefix(&icon_square(icon, 30, "row-icon"));
}

fn keep_awake_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Keep awake while connected")
        .subtitle("So the phone can always reach it, lid closed included")
        .active(settings.stay_awake)
        .build();
    with_icon(&row, "weather-clear-night-symbolic");
    let daemon = daemon.clone();
    row.connect_active_notify(move |row| daemon.send(DaemonCommand::SetStayAwake(row.is_active())));
    follow(&row, |settings, _| settings.stay_awake);
    row
}

/// Unlock with the phone: off unless turned on here, and explained.
fn unlock_nearby_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let explained = format!(
        "When this computer is locked, {phone} can unlock it after you approve on the \
         phone — only on the same network, never through the relay. Other phones cannot."
    );
    let row = adw::SwitchRow::builder()
        .title(format!("Unlock with {phone} nearby"))
        .subtitle(&explained)
        .use_markup(false)
        .active(settings.unlocks_from(&device.id))
        .build();
    with_icon(&row, "changes-allow-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    row.connect_active_notify(move |row| {
        daemon.send(DaemonCommand::SetUnlockNearby(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.unlocks_from(id));
    row
}

fn command_alerts_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Command alerts")
        .subtitle(format!(
            "Tell {} when a terminal command that ran over 10 seconds finishes",
            device.name
        ))
        .use_markup(false)
        .active(settings.alerts_commands_to(&device.id))
        .build();
    with_icon(&row, "alarm-symbolic");
    let (daemon, id) = (daemon.clone(), device.id.clone());
    row.connect_active_notify(move |row| {
        daemon.send(DaemonCommand::SetCommandAlerts(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.alerts_commands_to(id));
    row
}

/// The commands the phone can run here, with Add, Edit and Remove.
fn run_commands_group(
    win: &DesklinkWindow,
    daemon: &DaemonHandle,
    settings: &AppSettings,
    phone: &str,
) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Run Commands")
        .description(format!(
            "Commands {phone} can start here with one tap. Each runs in a new terminal you can watch."
        ))
        .build();
    let commands = settings.run_commands.clone();
    for (index, command) in commands.iter().enumerate() {
        let row = adw::ActionRow::builder()
            .title(&command.name)
            .subtitle(&command.command)
            .use_markup(false)
            .subtitle_lines(1)
            .build();
        with_icon(&row, "system-run-symbolic");
        let edit = gtk::Button::builder()
            .icon_name("document-edit-symbolic")
            .tooltip_text("Edit")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        edit.update_property(&[gtk::accessible::Property::Label(&format!(
            "Edit {}",
            command.name
        ))]);
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        remove.update_property(&[gtk::accessible::Property::Label(&format!(
            "Remove {}",
            command.name
        ))]);
        row.add_suffix(&edit);
        row.add_suffix(&remove);
        let (list, sender, window) = (commands.clone(), daemon.clone(), win.downgrade());
        edit.connect_clicked(move |_| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let (existing, list, sender) = (list[index].clone(), list.clone(), sender.clone());
            edit_command(&window, Some(&existing), move |name, line| {
                let mut list = list.clone();
                list[index].name = name;
                list[index].command = line;
                sender.send(DaemonCommand::SetRunCommands(list));
            });
        });
        let (list, sender) = (commands.clone(), daemon.clone());
        remove.connect_clicked(move |_| {
            let mut list = list.clone();
            list.remove(index);
            sender.send(DaemonCommand::SetRunCommands(list));
        });
        group.add(&row);
    }
    let add = adw::ActionRow::builder()
        .title("Add Command")
        .activatable(true)
        .build();
    with_icon(&add, "list-add-symbolic");
    let (sender, window) = (daemon.clone(), win.downgrade());
    add.connect_activated(move |_| {
        let Some(window) = window.upgrade() else {
            return;
        };
        let (list, sender) = (commands.clone(), sender.clone());
        edit_command(&window, None, move |name, line| {
            let mut list = list.clone();
            list.push(crate::device_links::config::RunCommand {
                id: uuid::Uuid::new_v4().simple().to_string(),
                name,
                command: line,
            });
            sender.send(DaemonCommand::SetRunCommands(list));
        });
    });
    group.add(&add);
    group
}

/// Asks for a command's name and command line.
fn edit_command(
    parent: &impl IsA<gtk::Widget>,
    existing: Option<&crate::device_links::config::RunCommand>,
    done: impl Fn(String, String) + 'static,
) {
    let name = adw::EntryRow::builder()
        .title("Name")
        .text(existing.map_or("", |command| command.name.as_str()))
        .build();
    let line = adw::EntryRow::builder()
        .title("Command")
        .text(existing.map_or("", |command| command.command.as_str()))
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&name);
    list.append(&line);
    let dialog = adw::AlertDialog::new(
        Some(if existing.is_some() {
            "Edit Command"
        } else {
            "Add Command"
        }),
        Some("It runs in your login shell, in a new terminal."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", "Save")]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_extra_child(Some(&list));
    let check = {
        let (dialog, name, line) = (dialog.downgrade(), name.downgrade(), line.downgrade());
        move || {
            if let (Some(dialog), Some(name), Some(line)) =
                (dialog.upgrade(), name.upgrade(), line.upgrade())
            {
                dialog.set_response_enabled(
                    "ok",
                    !name.text().trim().is_empty() && !line.text().trim().is_empty(),
                );
            }
        }
    };
    check();
    name.connect_changed({
        let check = check.clone();
        move |_| check()
    });
    line.connect_changed(move |_| check());
    dialog.choose(parent, gio::Cancellable::NONE, move |response| {
        if response == "ok" {
            done(
                name.text().trim().to_string(),
                line.text().trim().to_string(),
            );
        }
    });
}

fn start_at_login_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Start at Login")
        .subtitle("Keeps this computer reachable after a restart")
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
    row.connect_active_notify(move |row| {
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
fn top_bar_key() -> String {
    format!(
        "{} {:?} {:?}",
        tray_hosted(),
        crate::shell_dbus::extension_status(),
        crate::shell_dbus::appindicator_off()
    )
}

/// "Log Out Now…" (GNOME asks to confirm).
fn log_out_button() -> gtk::Button {
    let button = gtk::Button::builder()
        .label("Log Out Now…")
        .valign(gtk::Align::Center)
        .build();
    button.connect_clicked(|_| crate::shell_dbus::log_out());
    button
}

/// Top Bar: whether DeskLink shows there, and the one step that fixes it.
/// The tray icon is the freedesktop way (KDE, most desktops; GNOME with the
/// AppIndicator extension); GNOME also has DeskLink's own extension.
fn top_bar_group(daemon: &DaemonHandle, settings: &AppSettings) -> adw::PreferencesGroup {
    use crate::shell_dbus::ExtensionStatus;
    let group = adw::PreferencesGroup::builder().title("Top Bar").build();
    let gnome = crate::shell_dbus::extension_possible();

    let tray = adw::ActionRow::builder().title("Tray icon").build();
    with_icon(&tray, "phone-symbolic");
    let indicator_off = crate::shell_dbus::appindicator_off();
    tray.set_subtitle(if tray_hosted() {
        "Shown"
    } else if indicator_off.is_some() {
        "Not shown — the AppIndicator extension is installed but off"
    } else if gnome {
        "Not available — GNOME shows tray icons only with the AppIndicator extension"
    } else {
        "Not available on this desktop"
    });
    if let Some(uuid) = indicator_off.filter(|_| !tray_hosted()) {
        let button = gtk::Button::builder()
            .label("Turn On")
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
                    super::toast_from(&button, &format!("Could not turn it on: {error}"));
                }
            });
        });
        tray.add_suffix(&button);
    }
    group.add(&tray);

    if !gnome {
        return group;
    }
    let menu = adw::ActionRow::builder().title("GNOME extension").build();
    with_icon(&menu, "view-app-grid-symbolic");
    match crate::shell_dbus::extension_status() {
        ExtensionStatus::Active => {
            menu.set_subtitle("Active — your phone, live sessions and quick actions in the top bar")
        }
        ExtensionStatus::NeedsLogin => {
            menu.set_subtitle("Installed — it appears after you log in again");
            menu.add_suffix(&log_out_button());
        }
        ExtensionStatus::Off => {
            menu.set_subtitle(
                "Off — your phone, live sessions and quick actions in the top bar, and Unlock on the lock screen",
            );
            let button = gtk::Button::builder()
                .label("Turn On")
                .valign(gtk::Align::Center)
                .build();
            button.update_property(&[gtk::accessible::Property::Label("Turn on the top-bar menu")]);
            button.connect_clicked(|button| match crate::shell_dbus::turn_on_extension() {
                Ok(true) => {
                    let dialog = adw::AlertDialog::new(
                        Some("Top-Bar Menu Installed"),
                        Some(crate::shell_dbus::RELOGIN_NOTE),
                    );
                    dialog.add_response("later", "_Later");
                    dialog.add_response("log-out", "_Log Out Now…");
                    dialog.set_close_response("later");
                    dialog.connect_response(Some("log-out"), |_, _| crate::shell_dbus::log_out());
                    dialog.present(Some(button));
                }
                Ok(false) => super::toast_from(button, "Top-bar menu turned on"),
                Err(error) => {
                    eprintln!("[UI] Could not turn on the top-bar menu: {error}");
                    let dialog = adw::AlertDialog::new(
                        Some("Could Not Turn On the Top-Bar Menu"),
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
    group
}

/// System, Light or Dark.
fn appearance_row() -> adw::ComboRow {
    use crate::application::{appearance_scheme, load_appearance, save_appearance};
    let choices = ["system", "light", "dark"];
    let row = adw::ComboRow::builder()
        .title("Style")
        .model(&gtk::StringList::new(&["System", "Light", "Dark"]))
        .selected(
            choices
                .iter()
                .position(|choice| *choice == load_appearance())
                .unwrap_or(0) as u32,
        )
        .build();
    row.add_prefix(&icon_square(
        "preferences-desktop-appearance-symbolic",
        30,
        "row-icon",
    ));
    row.connect_selected_notify(move |row| {
        let choice = choices
            .get(row.selected() as usize)
            .copied()
            .unwrap_or("system");
        save_appearance(choice);
        adw::StyleManager::default().set_color_scheme(appearance_scheme(choice));
    });
    row
}

fn terminal_access_row(daemon: &DaemonHandle, device: &DeviceView) -> adw::SwitchRow {
    let warning = format!("Lets {} open terminals and run commands here", device.name);
    // `active` is set before the handler is connected, so building the row
    // from persisted state does not itself send a command.
    let row = adw::SwitchRow::builder()
        .title("Terminal access")
        .subtitle(&warning)
        .use_markup(false)
        .active(device.terminal_access)
        .build();
    with_icon(&row, "utilities-terminal-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&warning)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    row.connect_active_notify(move |row| {
        debug_log!("[UI] Terminal access for {id} set to {}", row.is_active());
        daemon.send(DaemonCommand::SetTerminalAccess(
            id.clone(),
            row.is_active(),
        ));
    });
    row
}

/// "Reach this computer from anywhere": the relay switch, and its full
/// settings (server, TURN, state) behind the row.
fn away_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::ActionRow {
    // No relay set up: say so, and the switch waits for an address.
    let configured = settings.relay_configured();
    let row = adw::ActionRow::builder()
        .title("Reach this computer from anywhere")
        .subtitle(if configured {
            format!(
                "{} · end-to-end encrypted",
                relay_host(settings.relay_url())
            )
        } else {
            NO_RELAY.to_string()
        })
        .subtitle_lines(2)
        .use_markup(false)
        .activatable(true)
        .build();
    if configured {
        row.add_css_class("mono-subtitle");
    }
    row.add_prefix(&icon_square("desklink-globe-symbolic", 30, "row-icon"));
    let switch = gtk::Switch::builder()
        .active(settings.relay_wanted())
        .sensitive(configured)
        .valign(gtk::Align::Center)
        .build();
    if !configured {
        switch.set_tooltip_text(Some("Add a relay server first"));
    }
    switch.update_property(&[gtk::accessible::Property::Label("Connect away from home")]);
    {
        let daemon = daemon.clone();
        switch.connect_active_notify(move |switch| {
            let url = crate::device_links::config::app_settings().relay_url;
            daemon.send(DaemonCommand::SetRelay {
                enabled: switch.is_active(),
                url,
            });
        });
    }
    row.add_suffix(&switch);
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("go-next-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let daemon = daemon.clone();
    let shown = switch.downgrade();
    row.connect_activated(move |row| {
        let content = adw::PreferencesPage::new();
        content.add(&away_from_home_group(
            &daemon,
            &crate::device_links::config::app_settings(),
        ));
        let dialog = adw::Dialog::builder()
            .title("Away From Home")
            .content_width(560)
            .content_height(520)
            .child(&crate::application::with_header(&content))
            .build();
        // The page catches up with what was set in the dialog.
        let (shown, page) = (shown.clone(), row.downgrade());
        dialog.connect_closed(move |_| {
            let settings = crate::device_links::config::app_settings();
            if let Some(switch) = shown.upgrade() {
                switch.set_sensitive(settings.relay_configured());
                if switch.is_active() != settings.relay_wanted() {
                    switch.set_active(settings.relay_wanted());
                }
            }
            if let Some(window) = page
                .upgrade()
                .and_then(|row| row.root())
                .and_downcast::<DesklinkWindow>()
            {
                window.imp().snapshots.borrow_mut().remove("preferences");
                window.refresh_soon();
            }
        });
        dialog.present(Some(row));
    });
    row
}

/// What the relay row says while no relay server is set up.
const NO_RELAY: &str = "No relay server set up · add one to reach this computer away from home";

/// "relay.desklink.app" from "wss://relay.desklink.app/v1".
fn relay_host(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split(['/', '?']).next().unwrap_or(rest)
}

/// Relay on/off, its address, TURN, and the relay's current state.
fn away_from_home_group(daemon: &DaemonHandle, settings: &AppSettings) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Away From Home")
        .description(
            "When your phone is on another network, it meets this computer through a \
             DeskLink relay server. What they exchange is end-to-end encrypted.",
        )
        .build();
    let built_in = !crate::device_links::config::DEFAULT_RELAY_URL.is_empty();
    let enabled = adw::SwitchRow::builder()
        .title("Connect Away From Home")
        .active(settings.relay_wanted())
        .sensitive(settings.relay_configured())
        .build();
    let url = adw::EntryRow::builder()
        .title(if built_in {
            "Server (leave empty for the DeskLink server)"
        } else {
            "Relay server address (wss://…)"
        })
        .text(settings.relay_url.as_str())
        .show_apply_button(true)
        .build();
    let turn = adw::SwitchRow::builder()
        .sensitive(settings.relay_configured())
        .title("Relay Media When Needed")
        .subtitle(
            "Use a TURN server when no direct path exists. Off means direct connections only.",
        )
        .active(settings.use_turn)
        .build();
    let status = adw::ActionRow::builder().title("Status").build();
    let status_label = gtk::Label::builder().css_classes(["dim-label"]).build();
    status.add_suffix(&status_label);

    // Weak: the closure is connected on these very rows.
    let send_relay = {
        let daemon = daemon.clone();
        let enabled = enabled.downgrade();
        let url = url.downgrade();
        move || {
            let (Some(enabled), Some(url)) = (enabled.upgrade(), url.upgrade()) else {
                return;
            };
            daemon.send(DaemonCommand::SetRelay {
                enabled: enabled.is_active(),
                url: url.text().trim().to_string(),
            })
        }
    };
    enabled.connect_active_notify({
        let send_relay = send_relay.clone();
        move |_| send_relay()
    });
    // An address makes the switch usable; entering one turns it on.
    {
        let enabled = enabled.downgrade();
        let turn = turn.downgrade();
        url.connect_changed(move |url| {
            let usable = built_in || !url.text().trim().is_empty();
            if let Some(turn) = turn.upgrade() {
                turn.set_sensitive(usable);
            }
            if let Some(enabled) = enabled.upgrade() {
                enabled.set_sensitive(usable);
                // No address left: off, and saved so (with the empty address).
                if !usable && enabled.is_active() {
                    enabled.set_active(false);
                }
            }
        });
    }
    {
        let enabled = enabled.downgrade();
        url.connect_apply(move |url| match enabled.upgrade() {
            Some(enabled) if !enabled.is_active() && !url.text().trim().is_empty() => {
                enabled.set_active(true)
            }
            _ => send_relay(),
        });
    }
    let daemon = daemon.clone();
    turn.connect_active_notify(move |row| daemon.send(DaemonCommand::SetUseTurn(row.is_active())));

    let update = move |label: &gtk::Label| {
        use crate::device_links::relay_link::client::{status, RelayStatus};
        label.set_label(&match status() {
            RelayStatus::Off if !crate::device_links::config::app_settings().relay_configured() => {
                "No relay server set up".to_string()
            }
            RelayStatus::Off => "Off".to_string(),
            RelayStatus::Connecting => "Connecting…".to_string(),
            RelayStatus::Online => "Ready".to_string(),
            RelayStatus::Unavailable(reason) => format!("Unavailable: {reason}"),
            RelayStatus::UpdateRequired => "Update DeskLink to connect away from home".to_string(),
        });
    };
    update(&status_label);
    let weak = status_label.downgrade();
    glib::timeout_add_seconds_local(1, move || match weak.upgrade() {
        Some(label) => {
            update(&label);
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });

    group.add(&enabled);
    group.add(&url);
    group.add(&turn);
    group.add(&status);
    group
}

fn password_subtitle(set: bool) -> &'static str {
    if set {
        "Phones give this computer's session password once per connection"
    } else {
        "Pairing alone is enough"
    }
}

/// "Ask before terminal and screen access": the session password, on or
/// off, and Change… while it is on.
fn password_row(daemon: &DaemonHandle, settings: &AppSettings) -> adw::ActionRow {
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
        Some("Session Password"),
        Some("Your phone asks for this once per connection before it can view or control the screen or open a terminal."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", "Set Password")]);
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

/// Approve, while at the desk, the phone controlling this computer — so
/// later sessions from the phone start without anyone here to click Share.
pub(super) fn approve_remote_control(window: &DesklinkWindow, id: &str) {
    const VIEW_ONLY: &str =
        "Shared without remote interaction, so not remembered. Try again with \u{201c}Allow Remote Interaction\u{201d} on.";
    window.imp().approving.borrow_mut().insert(id.to_string());
    window.refresh_soon();
    let (window, id) = (window.downgrade(), id.to_string());
    glib::spawn_future_local(async move {
        let worker_id = id.clone();
        let outcome = gio::spawn_blocking(move || {
            crate::device_links::webrtc::preauthorize_remote_control(&worker_id)
        })
        .await
        .unwrap_or_else(|_| Err("The approval was interrupted".to_string()));
        let Some(window) = window.upgrade() else {
            return;
        };
        window.imp().approving.borrow_mut().remove(&id);
        match &outcome {
            Ok(true) => window.show_feature_toast("Approved"),
            Ok(false) => window.show_feature_toast(VIEW_ONLY),
            Err(error) => {
                eprintln!("[UI] Remote-control approval did not complete: {error}");
                window.show_feature_toast("The approval did not complete");
            }
        }
        window.refresh_soon();
    });
}

/// "Let the phone control mouse and keyboard": approve it here once, so the
/// phone's later sessions need no one at the desk.
///
/// Drawn from state only (the remembered grant and whether an approval is
/// under way), because the page is redrawn when either changes and a label
/// set by hand would be lost.
fn remote_control_row(device: &DeviceView, approving: bool) -> adw::ActionRow {
    const APPROVED: &str = "Approved until DeskLink restarts";
    const NOT_APPROVED: &str = "The desktop asks you to approve each time";
    const WAITING: &str =
        "Choose the screen, turn on \u{201c}Allow Remote Interaction\u{201d}, then Share";

    let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
    let row = adw::ActionRow::builder()
        .title("Let the phone control mouse and keyboard")
        .subtitle(match (approving, approved) {
            (true, _) => WAITING,
            (false, true) => APPROVED,
            (false, false) => NOT_APPROVED,
        })
        .subtitle_lines(2)
        .build();
    row.add_prefix(&icon_square("input-mouse-symbolic", 30, "row-icon"));
    let approve = gtk::Button::builder()
        .label(match (approving, approved) {
            (true, _) => "Choose the Screen…",
            (false, true) => "Approve Again…",
            (false, false) => "Approve…",
        })
        .sensitive(!approving)
        .valign(gtk::Align::Center)
        .build();
    let forget = gtk::Button::builder()
        .label("Forget")
        .valign(gtk::Align::Center)
        .visible(approved && !approving)
        .css_classes(["flat"])
        .build();
    forget.update_property(&[gtk::accessible::Property::Description(
        "Stop this phone controlling the computer without asking",
    )]);
    row.add_suffix(&forget);
    row.add_suffix(&approve);
    let id = device.id.clone();
    approve.connect_clicked(move |button| {
        if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
            approve_remote_control(&window, &id);
        }
    });
    let id = device.id.clone();
    forget.connect_clicked(move |button| {
        crate::device_links::config::set_remote_desktop_token(&id, None);
        if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
            window.refresh_soon();
        }
    });
    row
}

/// "Unpair Pixel 9…", red, after asking.
fn unpair_button(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Button {
    let button = gtk::Button::builder()
        .child(&super::text(&format!("Unpair {}…", device.name), &[]))
        .css_classes(["danger-soft", "unpair"])
        .build();
    let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
    button.connect_clicked(move |button| {
        let (daemon, id) = (daemon.clone(), id.clone());
        super::confirm(
            button,
            &format!("Unpair {name}?"),
            "It can connect again only after pairing again.",
            "_Unpair",
            move || {
                debug_log!("[UI] Unpair clicked for {}", id);
                daemon.send(DaemonCommand::Unpair(id.clone()));
            },
        );
    });
    button
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_relay_reads_as_its_host() {
        assert_eq!(
            relay_host("wss://relay.desklink.app/v1"),
            "relay.desklink.app"
        );
        assert_eq!(relay_host("wss://10.0.0.2:8443?x=1"), "10.0.0.2:8443");
        assert_eq!(relay_host("relay.example"), "relay.example");
    }
}
