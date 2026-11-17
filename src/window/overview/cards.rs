//! Home's cards: the live strip, the columns, files, terminals and
//! notifications cards, and transfer progress.
//!
//! Sibling edges: none (the parent's helpers).

use super::*;

/// "Pixel 9 is using this computer · viewing your screen · since 14:02",
/// with End all, which asks first.
pub(super) fn live_strip(
    daemon: &DaemonHandle,
    device: &DeviceView,
    summary: &str,
    since: Option<&str>,
) -> gtk::Box {
    let headline = if device.desktop_extended {
        format!("{} is a second display", device.name)
    } else {
        format!("{} is using this computer", device.name)
    };
    let strip = banner(
        BannerKind::Live,
        "desklink-mobile-screen-share-symbolic",
        &headline,
        Some(&live_detail(summary, since)),
    );
    // A second display (F2) is placed with the desktop's display settings.
    if device.desktop_extended && gtk::glib::find_program_in_path("gnome-control-center").is_some()
    {
        let arrange = button(ButtonKind::Outline, "Arrange…", None);
        arrange.set_tooltip_text(Some("Place the phone's display beside this screen"));
        arrange.connect_clicked(|_| crate::window::guide::open_display_settings());
        strip.add_action(&arrange);
    }
    // Its speaker toggle (F1): this computer's sound to the phone too.
    if device.desktop_session != DesktopSession::None {
        let on = crate::window::sound::is_on(device, "desk-sound");
        let label = if on {
            "Stop sending this computer's sound"
        } else {
            "Play this computer's sound on the phone"
        };
        let sound = kit_icon_button(
            if on {
                "desklink-volume-up-symbolic"
            } else {
                "desklink-volume-mute-symbolic"
            },
            label,
        );
        crate::window::sound::toggle(daemon, device, "desk-sound", &sound);
        strip.add_action(&sound);
    }
    let end = button(ButtonKind::Danger, "End all", None);
    end.set_tooltip_text(Some("End the phone's sessions on this computer"));
    let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
    // Cutting the phone off loses nothing (the terminals keep running and
    // it can start again), so it happens at once, and the toast says so.
    end.connect_clicked(move |button| {
        daemon.send(DaemonCommand::EndPhoneSessions(Some(id.clone())));
        toast_from(
            button,
            &format!("Ended {name}'s sessions. The terminals keep running"),
        );
    });
    strip.add_action(&end);
    strip.root
}

/// "Viewing your screen · since 14:02": the summary with a capital.
pub(super) fn live_detail(summary: &str, since: Option<&str>) -> String {
    let mut detail = summary.to_string();
    if let Some(first) = detail.get(..1) {
        detail = first.to_uppercase() + &detail[1..];
    }
    if let Some(since) = since.filter(|since| !since.is_empty()) {
        detail.push_str(&format!(" · since {since}"));
    }
    detail
}

/// Reachable, the encrypted connection still being set up.
pub(super) fn reconnecting_row(device: &DeviceView) -> gtk::Box {
    banner(
        BannerKind::Info,
        "desklink-refresh-symbolic",
        &format!("Connecting to {}", device.name),
        Some("Setting up a secure connection. This takes a few seconds."),
    )
    .root
}

/// The Files card: the Bucket's summary and drop target, and the transfer
/// moving now, if one is.
pub(super) fn files_card(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let card = crate::window::bucket::overview_card(device);
    card.add_css_class("home-card-files");
    if let Some(progress) = transfer_progress(daemon, device) {
        card.append(&progress);
    }
    card
}

/// A card: its title and a line under it on the left, an optional widget at
/// the end of the header, and `body` below. Dimmed while what it shows is
/// the last known (`stale`).
pub(super) fn feature_card(
    title: &str,
    subtitle: &str,
    end: Option<&gtk::Widget>,
    body: &impl IsA<gtk::Widget>,
    stale: bool,
) -> gtk::Box {
    let card = kit_card(None);
    card.add_css_class("feature-card");
    card.set_vexpand(false);
    if stale {
        card.add_css_class("stale");
    }
    let head = gtk::Box::builder().spacing(12).build();
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let heading = ellipsized(title, &["dl-card-title"]);
    heading.set_accessible_role(gtk::AccessibleRole::Heading);
    words.append(&heading);
    words.append(&ellipsized(subtitle, &["dl-secondary"]));
    head.append(&words);
    if let Some(end) = end {
        head.append(end);
    }
    card.append(&head);
    card.append(body);
    card
}

/// A text button that opens page `name`: "View all", "Open".
pub(super) fn page_button(label: &str, name: &str) -> gtk::Button {
    let button = button(ButtonKind::Text, label, None);
    button.set_valign(gtk::Align::Center);
    button.set_action_name(Some("win.show-page"));
    button.set_action_target_value(Some(&name.to_variant()));
    button
}

/// The Terminals card: how many are open, New terminal, and the newest one
/// as a strip to open it.
pub(super) fn terminals_card(win: &DesklinkWindow, device: &DeviceView) -> gtk::Box {
    let sessions = crate::terminal_previews::sessions();
    let shared = sessions
        .iter()
        .filter(|info| attached_devices(&info.id).contains(&device.id))
        .count();
    let words = terminals_words(sessions.len(), shared, &device.name);
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    if let Some(info) = sessions.first() {
        let strip = gtk::Box::builder().spacing(12).build();
        strip.append(&text(
            &crate::terminal_previews::display_title(&info.title),
            &["mono", "strip-title"],
        ));
        let last = ellipsized(" ", &["mono", "strip-line"]);
        last.set_hexpand(true);
        last.set_widget_name(&format!("strip-line:{}", info.id));
        strip.append(&last);
        if attached_devices(&info.id).contains(&device.id) {
            strip.append(&presence_dot(&format!("{} is attached", device.name)));
            strip.append(&text(&device.name, &["strip-phone"]));
        }
        let open = gtk::Button::builder()
            .child(&strip)
            .css_classes(["term-strip"])
            .build();
        open.update_property(&[gtk::accessible::Property::Label(&format!(
            "Open the {} terminal",
            crate::terminal_previews::display_title(&info.title)
        ))]);
        let (window, id) = (win.downgrade(), info.id.clone());
        open.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.show_page("terminals");
                if let Some(page) = window.imp().terminals.borrow().as_ref() {
                    page.open_detail(&window, &id);
                }
            }
        });
        body.append(&open);
    }
    let new = button(
        ButtonKind::Tonal,
        "New terminal",
        Some("desklink-add-symbolic"),
    );
    new.set_valign(gtk::Align::Center);
    new.set_action_name(Some("win.new-terminal"));
    new.set_action_target_value(Some(&"shell".to_variant()));
    let card = feature_card("Terminals", &words, Some(new.upcast_ref()), &body, false);
    if sessions.is_empty() {
        // Only the header and its button: nothing to show below.
        card.remove(&body);
    }
    card.add_css_class("home-card-terminals");
    card
}

/// "1 open", "2 open · 1 shared with Pixel 9".
pub(super) fn terminals_words(open: usize, shared: usize, phone: &str) -> String {
    let mut words = format!("{open} open");
    if shared > 0 {
        words.push_str(&format!(" · {shared} shared with {phone}"));
    }
    words
}

/// At most 200 characters of one line, so a huge copy cannot stretch a row.
pub(in crate::window) fn preview_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// The Notifications card: the newest two, each with Reply when the phone
/// takes one; or why there is nothing.
pub(super) fn notifications_card(device: &DeviceView) -> gtk::Box {
    let shown = crate::device_links::config::app_settings().shows_notifications(&device.id);
    let mut words = match (shown, device.notifications.len()) {
        (false, _) => "Hidden on this computer".to_string(),
        (true, 0) => "None".to_string(),
        (true, count) => format!("{count} new"),
    };
    if let Some(seen) = crate::window::last_seen_words(device).filter(|_| shown) {
        words.push_str(&format!(" · {seen}"));
    }
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    if shown && !device.notifications.is_empty() {
        for newest in device.notifications.iter().take(2) {
            // The app's square at the top, then the heading, two lines of
            // the words and Reply under them (never centred beside them).
            let row = gtk::Box::builder().spacing(12).build();
            row.append(&crate::window::notifications::app_square(newest));
            let words = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .hexpand(true)
                .build();
            words.append(&crate::window::notifications::heading_line(newest));
            words.append(&crate::window::notifications::body_label(newest, 2));
            row.append(&words);
            // Away, Reply could not be sent: it is left out, not greyed.
            if newest.request_reply_id.is_some() && !phone_away(device) {
                let reply = button(ButtonKind::Tonal, "Reply", Some("desklink-reply-symbolic"));
                reply.set_halign(gtk::Align::End);
                reply.set_margin_top(6);
                reply.add_css_class("dl-note-action");
                // Replies go to the phone: only while it is connected.
                reply.set_sensitive(device.feature_transport_ready);
                let id = newest.id.clone();
                reply.connect_clicked(move |button| {
                    if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                        window.show_page("notifications");
                        if let Some(page) = window.imp().notifications.borrow().as_ref() {
                            page.open_reply(&id);
                        }
                    }
                });
                words.append(&reply);
            }
            body.append(&row);
        }
    } else if shown {
        body.append(&text(
            &format!("{}'s notifications show up here.", device.name),
            &["dl-secondary"],
        ));
    } else {
        body.append(&wrapped(
            &format!("Notifications from {} are turned off.", device.name),
            &["dl-secondary"],
        ));
        let turn_on = button(ButtonKind::Tonal, "Turn on", None);
        turn_on.set_halign(gtk::Align::Start);
        turn_on.set_action_name(Some("win.show-page"));
        turn_on.set_action_target_value(Some(&"settings".to_variant()));
        body.append(&turn_on);
    }
    let card = feature_card(
        "Notifications",
        &words,
        Some(page_button("View all", "notifications").upcast_ref()),
        &body,
        shown && !device.feature_transport_ready,
    );
    card.add_css_class("home-card-notifications");
    card
}

fn transfer_progress(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Box> {
    let row = crate::window::transfers::rows(daemon)
        .into_iter()
        .find(|row| row.device_id == device.id || row.device_id.is_empty())?;
    let progress = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(4)
        .build();
    let line = gtk::Box::builder().spacing(8).build();
    line.append(
        &gtk::Image::builder()
            .icon_name(if row.incoming {
                "desklink-arrow-downward-symbolic"
            } else {
                "desklink-arrow-upward-symbolic"
            })
            .css_classes(["text-dim"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    // The middle goes, so the kind of file at the end stays.
    let name = ellipsized(&row.name, &["mono"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    name.set_hexpand(true);
    line.append(&name);
    let label = text(&progress_words(&row), &["caption", "text-dim"]);
    label.set_widget_name("overview-transfer-label");
    line.append(&label);
    progress.append(&line);
    let bar = crate::window::ui_kit::progress(0.0);
    bar.set_widget_name("overview-transfer-bar");
    bar.update_property(&[gtk::accessible::Property::Label(&row.name)]);
    if let Some(fraction) = crate::window::transfers::fraction(&row) {
        bar.set_fraction(fraction);
    }
    progress.append(&bar);
    Some(progress)
}

/// "42% · sending", or "Preparing…".
pub(super) fn progress_words(row: &TransferRow) -> String {
    match crate::window::transfers::fraction(row) {
        Some(fraction) => format!(
            "{}% · {}",
            (fraction * 100.0).round() as u32,
            if row.incoming { "receiving" } else { "sending" }
        ),
        None => "Preparing…".to_string(),
    }
}
