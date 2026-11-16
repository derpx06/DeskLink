//! Building the settings groups for a page from the computer-wide and
//! per-phone rows, plus the first-run dialog without a phone.
//!
//! Sibling edges: `computer_rows`, `phone_rows`, `search`.

use super::*;

pub(super) fn group(title: &str) -> adw::PreferencesGroup {
    // A group's title is read as markup: "Privacy & security" is not valid
    // markup as it is, and the heading was not drawn at all.
    adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(title))
        .build()
}

pub(super) fn groups(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
    glide_status: &crate::device_links::glide::Status,
    approving: bool,
) -> Vec<adw::PreferencesGroup> {
    let connection = group(GROUPS[0]);
    connection.add(&keep_awake_row(daemon, settings));
    connection.add(&phone_keep_awake_row(daemon, settings, device));
    connection.add(&start_at_login_row(daemon, settings));
    connection.add(&computer_name_row());
    connection.add(&pair_row());

    let notifications = group(GROUPS[1]);
    notifications.set_description(Some(
        "Where phone notifications show, and which of this computer's go to the phone",
    ));
    let shown = show_notifications_row(daemon, settings, device);
    notifications.add(&shown);
    notifications.add(&command_alerts_row(daemon, settings, device));
    add_forward_rows(&notifications, daemon, settings);
    notifications.add(&dnd_row(daemon, settings, device));
    phone_step_rows(&notifications, device, &["notifications", "dnd"]);

    let clipboard = group(GROUPS[2]);
    clipboard.add(&clipboard_row(daemon, settings));
    phone_step_rows(&clipboard, device, &["clipboard"]);

    let calls = calls_group(daemon, settings, device);
    phone_step_rows(&calls, device, &["calls", "contacts"]);

    let sound = sound_group(daemon, settings);
    phone_step_rows(&sound, device, &["media"]);

    let control = group(GROUPS[5]);
    add_glide_rows(&control, daemon, settings, device, glide_status);
    control.add(&remote_control_row(device, approving));

    let files = group(GROUPS[6]);
    files.add(&bucket_row(device));
    files.add(&downloads_row());
    files.add(&gallery_row(daemon, settings, device));
    phone_step_rows(&files, device, &["photos"]);

    let privacy = group(GROUPS[7]);
    privacy.add(&password_row(daemon, settings));
    privacy.add(&terminal_access_row(daemon, device));
    privacy.add(&unlock_nearby_row(daemon, settings, device));
    privacy.add(&walk_away_row(daemon, settings, device));
    privacy.add(&unpair_row(daemon, device));

    let appearance = group(GROUPS[8]);
    appearance.add(&appearance_row());
    add_top_bar_rows(&appearance, daemon, settings);

    let advanced = group(GROUPS[9]);
    if let Some(row) = relay_status_row(RELAY_ENABLED) {
        advanced.add(&row);
    }
    advanced.add(&history_row());
    advanced.add(&system_check_row());
    advanced.add(&logs_row());
    advanced.add(&about_row());

    // The group titles are fixed (see GROUPS); calls and sound build theirs.
    calls.set_title(&glib::markup_escape_text(GROUPS[3]));
    sound.set_title(&glib::markup_escape_text(GROUPS[4]));
    vec![
        connection,
        notifications,
        clipboard,
        calls,
        sound,
        control,
        files,
        privacy,
        appearance,
        advanced,
    ]
}

/// "Show <phone>'s notifications here": in its group and among the quick switches.
pub(super) fn show_notifications_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let shown = adw::SwitchRow::builder()
        .title(esc(format!("Show {}'s notifications here", device.name)))
        .subtitle("See them on this computer and reply from here")
        .active(settings.shows_notifications(&device.id))
        .build();
    with_icon(&shown, "preferences-system-notifications-symbolic");
    let (sender, id) = (daemon.clone(), device.id.clone());
    toggled(&shown, move |row| {
        sender.send(DaemonCommand::SetShowNotifications(
            id.clone(),
            row.is_active(),
        ));
    });
    follow(&shown, |settings, id| settings.shows_notifications(id));
    shown
}

/// The name phones see for this computer: the system's own, changed there.
fn computer_name_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("This computer's name")
        .use_markup(false)
        .build();
    row.set_subtitle(&format!(
        "Phones see it as {}. To change it, rename the computer in your system settings (About), then restart DeskLink.",
        crate::device_links::glide::desk_name()
    ));
    with_icon(&row, "desklink-device-desktop-symbolic");
    row
}

/// "Pair another phone…": the same window as the menu's and Ctrl+N.
fn pair_row() -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Pair another phone…")
        .subtitle("Connect a phone or tablet by scanning a QR code")
        .activatable(true)
        .build();
    with_icon(&row, "desklink-add-symbolic");
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name("desklink-chevron-right-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    row.connect_activated(|row| {
        let _ = row.activate_action("win.pair-new", None);
    });
    row
}

/// The computer-wide settings as a dialog, for when no phone is paired.
pub(crate) fn show_dialog(parent: &impl IsA<gtk::Widget>, daemon: &DaemonHandle) {
    let settings = crate::device_links::config::app_settings();
    let dialog = adw::PreferencesDialog::builder()
        .title("Settings")
        .content_width(640)
        .build();
    let page = adw::PreferencesPage::new();
    let connection = group(GROUPS[0]);
    connection.add(&keep_awake_row(daemon, &settings));
    connection.add(&start_at_login_row(daemon, &settings));
    connection.add(&computer_name_row());
    connection.add(&pair_row());
    page.add(&connection);
    let privacy = group(GROUPS[7]);
    privacy.add(&password_row(daemon, &settings));
    page.add(&privacy);
    let appearance = group(GROUPS[8]);
    appearance.add(&appearance_row());
    add_top_bar_rows(&appearance, daemon, &settings);
    page.add(&appearance);
    let advanced = group(GROUPS[9]);
    if let Some(row) = relay_status_row(RELAY_ENABLED) {
        advanced.add(&row);
    }
    advanced.add(&history_row());
    advanced.add(&system_check_row());
    advanced.add(&logs_row());
    advanced.add(&about_row());
    page.add(&advanced);
    dialog.add(&page);
    dialog.present(Some(parent));
}
