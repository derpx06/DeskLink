//! The per-phone rows: the Bucket, unlock nearby, gallery, Do Not Disturb,
//! walk away, command alerts, keep the phone awake, terminal access, remote
//! control (and approving it), unpair, and Glide with its phone steps.
//!
//! Sibling edges: `search` (`OWN_VISIBILITY`). Outside: `config`, `glide`,
//! `folders`, `dnd`, `desk_features`, `webrtc`, `window` (confirm,
//! requirements).

use super::*;

/// "The Bucket": on or off for this phone.
pub(super) fn bucket_row(device: &DeviceView) -> adw::SwitchRow {
    let on = crate::device_links::folders::bucket_on(&device.id);
    let row = adw::SwitchRow::builder()
        .title(esc(format!("Bucket with {}", device.name)))
        .subtitle("One place for files, pictures, text and links on both devices")
        .active(on)
        .build();
    with_icon(&row, "folder-symbolic");
    let id = device.id.clone();
    row.connect_active_notify(move |row| {
        if row.is_active() {
            crate::device_links::folders::turn_on_bucket(&id);
        } else if crate::device_links::folders::bucket_on(&id) {
            crate::device_links::folders::remove(&id, crate::device_links::folders::BUCKET_ID);
        }
    });
    row
}

/// Unlock with the phone: off unless turned on here, and explained.
pub(super) fn unlock_nearby_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let explained = format!(
        "When this computer is locked, {phone} can unlock it after you approve on the \
         phone — only on the same network{}. Other phones cannot.",
        if RELAY_ENABLED {
            ", never through the relay"
        } else {
            ""
        }
    );
    let row = adw::SwitchRow::builder()
        .title(format!("Unlock with {phone} nearby"))
        .use_markup(false)
        .active(settings.unlocks_from(&device.id))
        .build();
    // Set after `use_markup(false)`: a subtitle given to the builder is read
    // as markup first, and a phone named `A & B` was a warning.
    row.set_subtitle(&explained);
    with_icon(&row, "changes-allow-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetUnlockNearby(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.unlocks_from(id));
    row
}

/// Photos and screenshots the phone sends on its own (W12): off unless turned
/// on here (the phone has its own switch too).
pub(super) fn gallery_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let explained = format!(
        "New screenshots and photos {phone} sends on its own appear on the Files page. They are \
         kept here for 30 days, up to 200 MB, and never in Downloads. {phone} has its own switch too."
    );
    let row = adw::SwitchRow::builder()
        .title(format!("Accept photos and screenshots from {phone}"))
        .use_markup(false)
        .active(settings.accepts_gallery_from(&device.id))
        .build();
    row.set_subtitle(&explained);
    with_icon(&row, "image-x-generic-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetGalleryAccept(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.accepts_gallery_from(id));
    row
}

/// Do Not Disturb shared with the phone (W14): off unless turned on here.
pub(super) fn dnd_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let supported = crate::device_links::dnd::desk_state().is_some();
    let mut explained = if supported {
        format!(
            "{phone}'s Do Not Disturb turns off banners here, and turning them off here asks \
             {phone} to do the same. Put back when you stop sharing or {phone} is gone for a minute."
        )
    } else {
        "Not available on this desktop (it needs GNOME's notification settings)".to_string()
    };
    if supported && settings.shares_dnd_with(&device.id) && device.dnd_can_set == Some(false) {
        explained.push_str(&format!(
            " {phone} can't be controlled from here until you allow Do Not Disturb access on it."
        ));
    }
    let row = adw::SwitchRow::builder()
        .title(format!("Share Do Not Disturb with {phone}"))
        .use_markup(false)
        .active(settings.shares_dnd_with(&device.id))
        .sensitive(supported)
        .build();
    row.set_subtitle(&explained);
    with_icon(&row, "notifications-disabled-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetDndShare(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.shares_dnd_with(id));
    row
}

/// Walk-away lock: off unless turned on here, and explained.
pub(super) fn walk_away_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let phone = &device.name;
    let explained = format!(
        "Locks this computer if nothing reaches {phone} for a minute (three if its screen \
         was off), after a ten second warning you can cancel. Not while this computer has no \
         network, and it never unlocks."
    );
    let row = adw::SwitchRow::builder()
        .title(format!("Lock when I walk away with {phone}"))
        .use_markup(false)
        .active(settings.locks_when_away(&device.id))
        .build();
    row.set_subtitle(&explained);
    with_icon(&row, "system-lock-screen-symbolic");
    row.update_property(&[gtk::accessible::Property::Description(&explained)]);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetWalkAwayLock(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.locks_when_away(id));
    row
}

pub(super) fn command_alerts_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Command alerts")
        .use_markup(false)
        .active(settings.alerts_commands_to(&device.id))
        .build();
    row.set_subtitle(&format!(
        "Tell {} when a terminal command that ran over 10 seconds finishes",
        device.name
    ));
    with_icon(&row, "alarm-symbolic");
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetCommandAlerts(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.alerts_commands_to(id));
    row
}

/// Whether this phone may switch keep-awake from its side: off, with the
/// reason in the subtitle.
pub(super) fn phone_keep_awake_row(
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder()
        .title("Let this phone keep the computer awake")
        .use_markup(false)
        .active(settings.lets_keep_awake(&device.id))
        .build();
    row.set_subtitle(&format!(
        "{} may turn \"keep awake\" on, lid closed included. A laptop kept awake in a bag gets hot and flattens its battery.",
        device.name
    ));
    with_icon(&row, "weather-clear-night-symbolic");
    let (daemon, id) = (daemon.clone(), device.id.clone());
    toggled(&row, move |row| {
        daemon.send(DaemonCommand::SetKeepAwakeFrom(id.clone(), row.is_active()))
    });
    follow(&row, |settings, id| settings.lets_keep_awake(id));
    row
}

pub(super) fn terminal_access_row(daemon: &DaemonHandle, device: &DeviceView) -> adw::SwitchRow {
    let warning = format!("Lets {} open terminals on this computer", device.name);
    // `active` is set before the handler is connected, so building the row
    // from persisted state does not itself send a command.
    let row = adw::SwitchRow::builder()
        .title("Terminal access")
        .use_markup(false)
        .active(device.terminal_access)
        .build();
    row.set_subtitle(&warning);
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

/// Approve, while at the desk, the phone controlling this computer — so
/// later sessions from the phone start without anyone here to click Share.
pub(in crate::window) fn approve_remote_control(window: &DesklinkWindow, id: &str) {
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
        // The phone's "Ask on computer" for control follows the grant.
        crate::device_links::desk_features::push_settings();
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
pub(super) fn remote_control_row(device: &DeviceView, approving: bool) -> adw::ActionRow {
    const APPROVED: &str = "Approved until DeskLink restarts";
    const NOT_APPROVED: &str = "This computer asks you to approve each time";
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
        .build();
    row.add_prefix(&icon_square("input-mouse-symbolic", 30, "row-icon"));
    let approve = gtk::Button::builder()
        .label(match (approving, approved) {
            (true, _) => "Choose the screen…",
            (false, true) => "Approve again…",
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
    // A desktop that cannot share its screen, or shares it view only (no
    // control to approve), says why instead of a button that opens nothing.
    if let Some(reason) = crate::platform::note("share") {
        approve.set_sensitive(false);
        row.set_subtitle(&esc(reason));
        row.set_subtitle_lines(0);
    }
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
pub(super) fn unpair_row(daemon: &DaemonHandle, device: &DeviceView) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(esc(format!("Unpair {}…", device.name)))
        .subtitle("It can connect again only after pairing again")
        .activatable(true)
        .css_classes(["error"])
        .build();
    let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
    row.connect_activated(move |row| {
        let (daemon, id) = (daemon.clone(), id.clone());
        crate::window::confirm(
            row,
            &format!("Unpair {name}?"),
            "It can connect again only after pairing again.",
            "_Unpair",
            move || {
                debug_log!("[UI] Unpair clicked for {}", id);
                daemon.send(DaemonCommand::Unpair(id.clone()));
            },
        );
    });
    row
}

/// The words under the glide switch: what it does, or (until the phone says
/// it allows glide) what to do on the phone first. Glide stays off until then.
pub(super) fn glide_explained(
    phone: &str,
    allowed: Option<bool>,
    locked: bool,
    computer: &str,
) -> String {
    if locked {
        return format!(
            "{}. It works again as soon as {phone} is unlocked.",
            crate::device_links::glide::unlock_words(phone)
        );
    }
    if allowed == Some(true) {
        format!(
            "Move the pointer past the edge of this screen to use {phone} with this mouse and \
             keyboard. Esc+Super brings it back. GNOME asks you to allow this once each time \
             DeskLink starts and again after the screen locks; it cannot remember the answer."
        )
    } else {
        format!("{phone} needs to allow “Let {computer} control this phone” first.")
    }
}

/// Each feature in `keys` (`asks::TO_PHONE`) that is off on the phone: what
/// is off and the one button that asks the phone to turn it on (a request
/// there; the person allows it), or where that ask stands.
pub(super) fn phone_step_rows(group: &adw::PreferencesGroup, device: &DeviceView, keys: &[&str]) {
    let facts = crate::window::requirements::PhoneFacts::from_device(device);
    for key in keys {
        let Some(need) = crate::window::requirements::phone_step(&facts, key) else {
            continue;
        };
        let row = adw::ActionRow::builder()
            .title(esc(&need.title))
            .subtitle(esc(&need.detail))
            .build();
        row.add_prefix(&icon_square(
            "desklink-device-phone-symbolic",
            30,
            "row-icon",
        ));
        if let Some(label) = need.fix.label() {
            let ask = gtk::Button::builder()
                .label(label)
                .valign(gtk::Align::Center)
                .build();
            ask.update_property(&[gtk::accessible::Property::Description(&need.title)]);
            ask.connect_clicked(move |button| {
                crate::window::requirements::run(button, &need);
                button.set_sensitive(false);
            });
            row.add_suffix(&ask);
        }
        group.add(&row);
    }
}

/// Glide (F4): the pointer glides onto this phone past an edge of this
/// screen. A switch, and where the phone sits (a small arrangement: this
/// screen with a phone slot on each side). Hidden, with the reason, where
/// the desktop cannot capture input.
pub(super) fn add_glide_rows(
    group: &adw::PreferencesGroup,
    daemon: &DaemonHandle,
    settings: &AppSettings,
    device: &DeviceView,
    status: &crate::device_links::glide::Status,
) {
    use crate::device_links::glide::{Availability, Edge};
    match status.availability {
        Availability::Checking => return,
        Availability::Unavailable(reason) => {
            let row = adw::ActionRow::builder()
                .title(esc(format!("Glide onto {}", device.name)))
                .subtitle(esc(reason))
                .build();
            with_icon(&row, "input-mouse-symbolic");
            row.update_property(&[gtk::accessible::Property::Description(reason)]);
            group.add(&row);
            return;
        }
        Availability::Available => {}
    }
    let phone = device.name.clone();
    let current = settings
        .glide
        .as_ref()
        .filter(|glide| glide.device == device.id)
        .map(|glide| glide.edge);
    let explained = match status.ready.filter(|edge| current == Some(*edge)) {
        Some(edge) => format!(
            "Ready: move the pointer off the {} edge of this screen to use {phone}. Esc+Super \
             brings it back.",
            match edge {
                Edge::Left => "left",
                Edge::Right => "right",
                Edge::Top => "top",
                Edge::Bottom => "bottom",
            }
        ),
        None => glide_explained(
            &phone,
            crate::device_links::glide::phone_allows(&device.id),
            crate::device_links::glide::phone_locked(&device.id),
            &crate::device_links::glide::desk_name(),
        ),
    };
    let switch = adw::SwitchRow::builder()
        .title(esc(format!("Glide onto {phone}")))
        .subtitle(esc(&explained))
        .active(current.is_some())
        .build();
    with_icon(&switch, "input-mouse-symbolic");
    switch.update_property(&[gtk::accessible::Property::Description(&explained)]);
    follow(&switch, |settings, id| {
        settings
            .glide
            .as_ref()
            .is_some_and(|glide| glide.device == id)
    });
    group.add(&switch);

    // The arrangement: a phone slot on each side of this screen.
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(8)
        .halign(gtk::Align::Center)
        .margin_top(14)
        .margin_bottom(14)
        .build();
    let screen = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .width_request(132)
        .height_request(80)
        .valign(gtk::Align::Center)
        .css_classes(["glide-screen"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    let monitor = gtk::Image::builder()
        .icon_name("desklink-device-desktop-symbolic")
        .pixel_size(24)
        .vexpand(true)
        .valign(gtk::Align::End)
        .build();
    screen.append(&monitor);
    let label = gtk::Label::builder()
        .label("This screen")
        .vexpand(true)
        .valign(gtk::Align::Start)
        .css_classes(["caption", "dim-label"])
        .build();
    screen.append(&label);
    grid.attach(&screen, 1, 1, 1, 1);
    let mut first: Option<gtk::ToggleButton> = None;
    let mut slots = Vec::new();
    for edge in Edge::ALL {
        let words = format!("{phone} is {} this screen", edge.words());
        let button = gtk::ToggleButton::builder()
            .icon_name("desklink-device-phone-symbolic")
            .tooltip_text(&words)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["circular", "glide-slot"])
            .active(current.unwrap_or(Edge::Right) == edge)
            .build();
        button.update_property(&[gtk::accessible::Property::Label(&words)]);
        match first.as_ref() {
            Some(group) => button.set_group(Some(group)),
            None => first = Some(button.clone()),
        }
        let (row, column) = match edge {
            Edge::Left => (1, 0),
            Edge::Right => (1, 2),
            Edge::Top => (0, 1),
            Edge::Bottom => (2, 1),
        };
        grid.attach(&button, column, row, 1, 1);
        slots.push((edge, button.clone()));
        let (daemon, id, switch) = (daemon.clone(), device.id.clone(), switch.downgrade());
        button.connect_toggled(move |button| {
            if button.is_active() && switch.upgrade().is_some_and(|switch| switch.is_active()) {
                daemon.send(DaemonCommand::SetGlide(id.clone(), Some(edge)));
            }
        });
    }
    let arrangement = adw::PreferencesRow::builder()
        .activatable(false)
        .child(&grid)
        .visible(current.is_some())
        .build();
    arrangement.update_property(&[gtk::accessible::Property::Label(&format!(
        "Where {phone} sits"
    ))]);
    // Its visibility follows the glide switch, never the search's.
    arrangement.set_widget_name(OWN_VISIBILITY);
    group.add(&arrangement);
    let (daemon, id) = (daemon.clone(), device.id.clone());
    switch.connect_active_notify(move |switch| {
        arrangement.set_visible(switch.is_active());
        if SYNCING.with(|syncing| syncing.get()) {
            return;
        }
        // The slot shown as chosen (right by default).
        let edge = switch.is_active().then(|| {
            slots
                .iter()
                .find(|(_, button)| button.is_active())
                .map_or(Edge::Right, |(edge, _)| *edge)
        });
        daemon.send(DaemonCommand::SetGlide(id.clone(), edge));
    });
    // On here, off on the phone: said, with Ask on phone where the phone
    // takes it. (A locked phone is not "off": the row above says to unlock
    // it.)
    if current.is_some() && !crate::device_links::glide::phone_locked(&device.id) {
        phone_step_rows(group, device, &["glide"]);
    }
    if status.asking {
        let row = adw::ActionRow::builder()
            .title("Waiting for your OK on this computer")
            .subtitle("Approve “Capture Input” in the window that just opened.")
            .build();
        row.add_prefix(&icon_square("dialog-information-symbolic", 30, "row-icon"));
        group.add(&row);
    } else if let Some(problem) = status.problem.as_deref() {
        let row = adw::ActionRow::builder()
            .title(if status.can_ask_again {
                "Glide is paused"
            } else {
                "Glide is not working"
            })
            .subtitle(esc(problem))
            .build();
        row.add_prefix(&icon_square("dialog-warning-symbolic", 30, "row-icon"));
        if status.can_ask_again {
            let ask = gtk::Button::builder()
                .label("Turn on again")
                .valign(gtk::Align::Center)
                .build();
            ask.connect_clicked(|_| crate::device_links::glide::ask_again());
            row.add_suffix(&ask);
        }
        group.add(&row);
    }
}
