//! The Overview page: the phone in a hero band with its quick actions, what
//! it is doing here, and a card for each feature under "On this computer"
//! and "On <phone>".

use adw::prelude::*;
use adw::subclass::prelude::*;

use super::{
    can_send_feature, card, clock_time, confirm, device_type_label, ellipsized, icon_button,
    icon_square, named_widgets, page_link, pick_and_send_files, session_summary, text, toast_from,
    unavailable_reason, DesklinkWindow, QuickAction,
};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DesktopSession, DeviceStatus, DeviceView};
use crate::device_links::packet::{
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST, PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_PING, PACKET_TYPE_SHARE_REQUEST,
};
use crate::device_links::webrtc::terminal_bridge::attached_devices;
use crate::device_links::webrtc::transfer_board::TransferRow;
use crate::link_glyph::{battery_ring, link_glyph, presence_dot, LinkState};

pub(super) fn update(win: &DesklinkWindow, daemon: &DaemonHandle, device: &DeviceView) {
    let imp = win.imp();
    let approving = imp.approving.borrow().contains(&device.id);
    let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
    let since = imp.session_since.borrow().get(&device.id).cloned();
    let settings = crate::device_links::config::app_settings();
    let clip = crate::device_links::daemon::clipboard::history()
        .first()
        .map(|item| item.text.clone());
    // Sessions come and go here; their output is refreshed in place.
    let terminals: Vec<_> = crate::terminal_previews::sessions()
        .into_iter()
        .map(|info| {
            let attached = attached_devices(&info.id);
            (info.id, info.title, info.exit_status, attached)
        })
        .collect();
    let transfers: Vec<String> = super::transfers::rows(daemon)
        .into_iter()
        .filter(|row| row.device_id == device.id || row.device_id.is_empty())
        .map(|row| row.id)
        .collect();
    let minute = clock_time(gtk::glib::DateTime::now_local().ok());
    // The round trip changes every reading; it is updated in place.
    let steady = DeviceView {
        rtt_ms: None,
        ..device.clone()
    };
    let key = serde_json::to_string(&(
        &steady,
        approving,
        approved,
        &since,
        &terminals,
        &transfers,
        settings.clipboard_sync,
        settings.shows_notifications(&device.id),
        &clip,
        &minute,
    ))
    .unwrap_or_default();
    if !win.page_changed("overview", key) {
        refresh_in_place(win, daemon);
        show_round_trip(win, device);
        return;
    }
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(20)
        .margin_top(10)
        .margin_bottom(28)
        .css_classes(["overview-page"])
        .build();
    if device.trusted {
        page.append(&hero(daemon, device, &minute));
        if let Some(bar) = now_playing_bar(daemon, device) {
            page.append(&bar);
        }
        if let Some(summary) = session_summary(device) {
            page.append(&live_banner(daemon, device, &summary, since.as_deref()));
        }
        match device.status {
            DeviceStatus::Unreachable => page.append(&offline_row(daemon, device)),
            DeviceStatus::Paired if !device.feature_transport_ready => {
                page.append(&reconnecting_row(device))
            }
            _ => {}
        }
        page.append(&columns(win, daemon, device, approving, approved, clip));
    } else if device.status == DeviceStatus::PairRequestedByPeer {
        page.append(&super::pairing::pair_request_view(daemon, device));
    } else {
        page.append(&super::pairing::pair_waiting_view(device));
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(940)
        .tightening_threshold(720)
        .child(&page)
        .build();
    win.replace_body("overview", &clamp);
    refresh_in_place(win, daemon);
    show_round_trip(win, device);
}

/// The hero's "· 12 ms", in place.
fn show_round_trip(win: &DesklinkWindow, device: &DeviceView) {
    let Some((_, body)) = win.imp().bodies.borrow().get("overview").cloned() else {
        return;
    };
    let rtt = LinkState::of(device).round_trip(device);
    for widget in named_widgets(body.upcast_ref(), "hero-rtt") {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            let words = rtt
                .as_deref()
                .map(|rtt| format!("· {rtt}"))
                .unwrap_or_default();
            if label.label() != words {
                label.set_label(&words);
            }
            label.set_visible(rtt.is_some());
        }
    }
}

/// " · charging", with the time to full when the phone knows it.
fn charging_words(remaining_ms: Option<i64>) -> String {
    match remaining_ms.map(|ms| (ms + 59_999) / 60_000) {
        Some(minutes) if minutes > 0 && minutes < 60 => {
            format!(" · charging, full in {minutes} min")
        }
        Some(minutes) if minutes >= 60 => {
            format!(
                " · charging, full in {} h {} min",
                minutes / 60,
                minutes % 60
            )
        }
        _ => " · charging".to_string(),
    }
}

/// The hero: the phone, its name, how it is connected, its battery, and
/// what can be done to it.
fn hero(daemon: &DaemonHandle, device: &DeviceView, time: &str) -> gtk::Box {
    let band = gtk::Box::builder()
        .spacing(26)
        .css_classes(["hero-band"])
        .build();
    band.set_widget_name("responsive");

    let phone = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(62)
        .height_request(118)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["phone-art"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    phone.set_hexpand(false);
    phone.append(
        &gtk::Box::builder()
            .halign(gtk::Align::Center)
            .css_classes(["phone-camera"])
            .build(),
    );
    phone.append(
        &gtk::Label::builder()
            .label(time)
            .margin_top(22)
            .css_classes(["phone-time"])
            .build(),
    );
    band.append(&phone);

    let info = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let name = text(&device.name, &["hero-name"]);
    name.set_wrap(true);
    name.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    info.append(&name);
    let state = LinkState::of(device);
    let link = gtk::Box::builder().spacing(10).build();
    link.append(&link_glyph(state, state.label(device)));
    link.append(&text(state.label(device), &["text-dim", "hero-meta"]));
    let rtt = text("", &["mono", "text-faint", "hero-meta"]);
    rtt.set_widget_name("hero-rtt");
    rtt.set_tooltip_text(Some("Round trip to the phone"));
    rtt.set_visible(false);
    link.append(&rtt);
    info.append(&link);
    if let Some(battery) = device
        .battery_status
        .as_ref()
        .filter(|battery| battery.current_charge >= 0)
    {
        let line = gtk::Box::builder().spacing(10).build();
        line.append(&battery_ring(battery.current_charge, battery.is_charging));
        let words = gtk::Box::builder().valign(gtk::Align::Center).build();
        words.append(&text(
            &format!("{}%", battery.current_charge),
            &["mono", "battery-percent"],
        ));
        if battery.is_charging {
            words.append(&text(
                &charging_words(battery.charge_time_remaining),
                &["text-dim", "caption-strong"],
            ));
        }
        line.append(&words);
        info.append(&line);
    }
    if let Some(error) = &device.last_error {
        let label = text(&super::friendly_error(error).0, &["caption", "error"]);
        label.set_wrap(true);
        label.set_tooltip_text(Some(error));
        info.append(&label);
    }
    band.append(&info);

    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .build();
    let reason = unavailable_reason(device);
    let buttons = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    if can_send_feature(device, PACKET_TYPE_MOUSEPAD_REQUEST) {
        let show = icon_button(
            "video-display-symbolic",
            &format!("Show {} Screen", device_type_label(&device.device_type)),
            &["soft-button"],
        );
        let (daemon, id) = (daemon.clone(), device.id.clone());
        show.connect_clicked(move |button| {
            if let Some(root) = button.root().and_downcast::<gtk::Window>() {
                crate::remote_control::show_remote_control_dialog(
                    daemon.clone(),
                    id.clone(),
                    &root,
                );
            }
        });
        disable_with(&show, reason.as_deref());
        buttons.append(&show);
    }
    if can_send_feature(device, PACKET_TYPE_SHARE_REQUEST) {
        let send = icon_button(
            "document-send-symbolic",
            "Send Files…",
            &["suggested-action"],
        );
        let (daemon, id) = (daemon.clone(), device.id.clone());
        send.connect_clicked(move |button| pick_and_send_files(&daemon, &id, button));
        disable_with(&send, reason.as_deref());
        buttons.append(&send);
    }
    actions.append(&buttons);

    let round = gtk::Box::builder()
        .spacing(2)
        .halign(gtk::Align::End)
        .build();
    let kind = device_type_label(&device.device_type);
    for (packet, icon, label, accessible, action) in [
        (
            PACKET_TYPE_FINDMYPHONE_REQUEST,
            "desklink-ring-symbolic",
            "Ring",
            format!("Ring {kind}"),
            Some(QuickAction::Ring),
        ),
        (
            PACKET_TYPE_LOCK_REQUEST,
            "system-lock-screen-symbolic",
            "Lock",
            format!("Lock {kind}"),
            Some(QuickAction::Lock),
        ),
        (
            PACKET_TYPE_PING,
            "network-transmit-receive-symbolic",
            "Ping",
            "Ping".to_string(),
            Some(QuickAction::Ping),
        ),
        (
            PACKET_TYPE_SHARE_REQUEST,
            "insert-link-symbolic",
            "Send link",
            format!("Send a link to {}", device.name),
            None,
        ),
    ] {
        if !can_send_feature(device, packet) {
            continue;
        }
        let button = round_action(icon, label, &accessible);
        match action {
            Some(action) => {
                button.connect_clicked(move |button| {
                    if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                        window.quick_action(action);
                    }
                });
            }
            None => {
                let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
                button.connect_clicked(move |button| {
                    show_send_link_dialog(daemon.clone(), id.clone(), &name, button)
                });
            }
        }
        disable_with(&button, reason.as_deref());
        round.append(&button);
    }
    actions.append(&round);
    band.append(&actions);
    band
}

/// Greys `button` out with the reason as its tooltip, when there is one.
fn disable_with(button: &gtk::Button, reason: Option<&str>) {
    if let Some(reason) = reason {
        button.set_sensitive(false);
        button.set_tooltip_text(Some(reason));
    }
}

/// A round icon with a caption under it: Ring, Lock, Ping, Send link.
fn round_action(icon: &str, label: &str, accessible: &str) -> gtk::Button {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .build();
    content.append(&super::icon_square(icon, 40, "round-icon"));
    content.append(
        &gtk::Label::builder()
            .label(label)
            .css_classes(["round-caption"])
            .build(),
    );
    let button = gtk::Button::builder()
        .child(&content)
        .width_request(64)
        .tooltip_text(accessible)
        .css_classes(["round-action"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(accessible)]);
    button
}

/// "Send a Link": a URL to open on the phone.
fn show_send_link_dialog(
    daemon: DaemonHandle,
    device_id: String,
    device_name: &str,
    parent: &impl IsA<gtk::Widget>,
) {
    let entry = adw::EntryRow::builder()
        .title("Link")
        .input_purpose(gtk::InputPurpose::Url)
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    let dialog = adw::AlertDialog::new(
        Some("Send a Link"),
        Some(&format!("It opens on {device_name}.")),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("send", "_Send")]);
    dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("send"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("send", false);
    dialog.set_extra_child(Some(&list));
    {
        let dialog = dialog.downgrade();
        entry.connect_changed(move |entry| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled("send", link_to_send(&entry.text()).is_some());
            }
        });
    }
    let toast_target = parent.as_ref().clone();
    dialog.choose(parent, gtk::gio::Cancellable::NONE, move |response| {
        if response != "send" {
            return;
        }
        if let Some(link) = link_to_send(&entry.text()) {
            daemon.send(DaemonCommand::SendShareText(device_id.clone(), link));
            toast_from(&toast_target, "Link sent");
        }
    });
}

/// What the user typed as a link to open: `https://` is added when no
/// scheme was given; anything with spaces is not a link.
fn link_to_send(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty() || typed.contains(char::is_whitespace) || !typed.contains('.') {
        return None;
    }
    Some(
        if typed.starts_with("http://") || typed.starts_with("https://") {
            typed.to_string()
        } else {
            format!("https://{typed}")
        },
    )
}

/// The phone's music while it plays: track, progress and controls.
fn now_playing_bar(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Box> {
    // Paused too: hiding the bar on Pause left no button to play again.
    let media = device
        .media_status
        .as_ref()
        .filter(|media| media.is_playing || !media.title.is_empty())?;
    let bar = gtk::Box::builder()
        .spacing(16)
        .css_classes(["dl-card", "now-playing"])
        .build();
    bar.append(&art(52));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(3)
        .width_request(200)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(
        if media.title.is_empty() {
            "Playing"
        } else {
            &media.title
        },
        &["heading"],
    ));
    let state = if media.is_playing {
        "playing"
    } else {
        "paused"
    };
    let source = if media.artist.is_empty() {
        format!("{state} on {}", device.name)
    } else {
        format!("{} · {state} on {}", media.artist, device.name)
    };
    words.append(&ellipsized(&source, &["caption", "text-dim"]));
    bar.append(&words);
    let progress = gtk::Box::builder()
        .spacing(10)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    if let (Some(position), Some(length)) = (media.position, media.length.filter(|l| *l > 0)) {
        let time = |ms: i64| format!("{}:{:02}", ms / 60_000, (ms / 1000) % 60);
        progress.append(&text(&time(position), &["mono", "caption", "text-faint"]));
        let level = gtk::LevelBar::builder()
            .min_value(0.0)
            .max_value(1.0)
            .value((position as f64 / length as f64).clamp(0.0, 1.0))
            .hexpand(true)
            .valign(gtk::Align::Center)
            .css_classes(["track"])
            .build();
        level.update_property(&[gtk::accessible::Property::Label("Track position")]);
        progress.append(&level);
        progress.append(&text(&time(length), &["mono", "caption", "text-faint"]));
    }
    bar.append(&progress);
    bar.append(&media_controls(
        daemon,
        device,
        &media.player,
        media.is_playing,
    ));
    Some(bar)
}

/// Previous, play/pause and next for the phone's player.
fn media_controls(
    daemon: &DaemonHandle,
    device: &DeviceView,
    player: &str,
    playing: bool,
) -> gtk::Box {
    let controls = gtk::Box::builder()
        .spacing(4)
        .valign(gtk::Align::Center)
        .build();
    let can = can_send_feature(device, PACKET_TYPE_MPRIS_REQUEST) && device.feature_transport_ready;
    for (icon, label, action, class) in [
        (
            "media-skip-backward-symbolic",
            "Previous",
            "Previous",
            "flat",
        ),
        (
            if playing {
                "media-playback-pause-symbolic"
            } else {
                "media-playback-start-symbolic"
            },
            if playing { "Pause" } else { "Play" },
            "PlayPause",
            "play-button",
        ),
        ("media-skip-forward-symbolic", "Next", "Next", "flat"),
    ] {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(label)
            .valign(gtk::Align::Center)
            .sensitive(can)
            .css_classes(["circular", class])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(label)]);
        let (daemon, id, player) = (daemon.clone(), device.id.clone(), player.to_string());
        button.connect_clicked(move |_| {
            daemon.send(DaemonCommand::SendMprisAction(
                id.clone(),
                player.clone(),
                action.to_string(),
            ))
        });
        controls.append(&button);
    }
    controls
}

/// Album art stand-in: a soft gradient tile (the phone sends no artwork).
fn art(size: i32) -> gtk::Box {
    let art = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .valign(gtk::Align::Center)
        .css_classes(["album-art"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    art.set_hexpand(false);
    art
}

/// "Pixel 9 is using this computer · in a terminal, viewing your screen",
/// with End All, which asks first.
fn live_banner(
    daemon: &DaemonHandle,
    device: &DeviceView,
    summary: &str,
    since: Option<&str>,
) -> gtk::Box {
    let banner = gtk::Box::builder()
        .spacing(14)
        .css_classes(["live-banner"])
        .build();
    banner.append(&icon_square("screen-shared-symbolic", 28, "live-icon"));
    let words = gtk::Box::builder()
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let headline = format!("{} is using this computer", device.name);
    let mut detail = format!(" · {summary}");
    if let Some(since) = since.filter(|since| !since.is_empty()) {
        detail.push_str(&format!(" · since {since}"));
    }
    words.append(&text(&headline, &["heading"]));
    let detail_label = ellipsized(&detail, &["text-dim"]);
    detail_label.set_hexpand(true);
    words.append(&detail_label);
    banner.append(&words);
    banner.update_property(&[gtk::accessible::Property::Label(&format!(
        "{headline}{detail}"
    ))]);
    let end = super::soft_button("End All");
    end.set_css_classes(&["coral-outline"]);
    end.set_tooltip_text(Some("End the phone's sessions on this computer"));
    let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
    end.connect_clicked(move |button| {
        let (daemon, id, target) = (daemon.clone(), id.clone(), button.clone());
        confirm(
            button,
            &format!("End Sessions with {name}?"),
            "The phone stops using the screen and its terminals here. The terminals keep running.",
            "_End Sessions",
            move || {
                daemon.send(DaemonCommand::EndPhoneSessions(Some(id.clone())));
                toast_from(&target, "Sessions ended");
            },
        );
    });
    banner.append(&end);
    banner
}

/// "Pixel 9 is offline · your terminals keep running here", Try Again.
fn offline_row(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let row = gtk::Box::builder()
        .spacing(14)
        .css_classes(["state-banner"])
        .build();
    row.append(&icon_square("network-offline-symbolic", 28, "state-icon"));
    let words = gtk::Box::builder()
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&text(&format!("{} is offline", device.name), &["heading"]));
    let detail = ellipsized(" · your terminals keep running here", &["text-dim"]);
    detail.set_hexpand(true);
    words.append(&detail);
    row.append(&words);
    let retry = super::soft_button("Try Again");
    retry.set_css_classes(&["raised-button"]);
    let daemon = daemon.clone();
    retry.connect_clicked(move |button| {
        daemon.send(DaemonCommand::Discover);
        toast_from(button, "Searching the network…");
    });
    row.append(&retry);
    row
}

/// Reachable, the encrypted connection still being set up.
fn reconnecting_row(device: &DeviceView) -> gtk::Box {
    let row = gtk::Box::builder()
        .spacing(14)
        .css_classes(["state-banner", "tall"])
        .build();
    let dots = gtk::Box::builder()
        .spacing(4)
        .valign(gtk::Align::Center)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    for delay in ["one", "two", "three"] {
        dots.append(
            &gtk::Box::builder()
                .valign(gtk::Align::Center)
                .css_classes(["dot", "pulse", delay])
                .build(),
        );
    }
    row.append(&dots);
    row.append(&text(
        &format!("Reconnecting to {}", device.name),
        &["heading"],
    ));
    let detail = ellipsized("Setting up the encrypted connection", &["text-dim"]);
    detail.set_hexpand(true);
    row.append(&detail);
    row
}

/// "On this computer" and "On Pixel 9", side by side, or stacked when narrow.
fn columns(
    win: &DesklinkWindow,
    daemon: &DaemonHandle,
    device: &DeviceView,
    approving: bool,
    approved: bool,
    clip: Option<String>,
) -> gtk::Box {
    let columns = gtk::Box::builder()
        .spacing(18)
        .homogeneous(true)
        .valign(gtk::Align::Start)
        .build();
    columns.set_widget_name("responsive");
    let left = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .build();
    left.append(&column_heading("On this computer", "computer-marker"));
    left.append(&terminals_card(win, device));
    left.append(&screen_card(device, approving, approved));
    left.append(&clipboard_card(daemon, clip));
    let right = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .build();
    right.append(&column_heading(
        &format!("On {}", device.name),
        "phone-marker",
    ));
    right.append(&notifications_card(device));
    right.append(&transfer_card(daemon, device));
    if let Some(media) = device
        .media_status
        .as_ref()
        .filter(|media| !media.is_playing)
    {
        let bottom = gtk::Box::builder().spacing(14).build();
        bottom.append(&art(44));
        let words = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        words.append(&ellipsized(&media.title, &["heading"]));
        words.append(&ellipsized(&media.artist, &["caption", "text-dim"]));
        bottom.append(&words);
        bottom.append(&media_controls(daemon, device, &media.player, false));
        right.append(&feature_card(
            "audio-x-generic-symbolic",
            "Now Playing",
            &text("Paused", &["text-dim"]),
            None,
            &bottom,
        ));
    }
    columns.append(&left);
    columns.append(&right);
    columns
}

fn column_heading(label: &str, marker: &str) -> gtk::Box {
    let heading = gtk::Box::builder()
        .spacing(10)
        .margin_start(4)
        .margin_end(4)
        .build();
    heading.append(
        &gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes([marker])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    heading.append(&ellipsized(label, &["column-heading"]));
    heading
}

/// A 150 px card: icon, title, subtitle and an optional end widget on top,
/// `bottom` at the bottom.
fn feature_card(
    icon: &str,
    title: &str,
    subtitle: &impl IsA<gtk::Widget>,
    end: Option<&gtk::Widget>,
    bottom: &impl IsA<gtk::Widget>,
) -> gtk::Box {
    let card = card(0);
    card.add_css_class("feature-card");
    card.set_height_request(150);
    // Not the spacer's expansion: cards keep their height side by side.
    card.set_vexpand(false);
    let head = gtk::Box::builder().spacing(14).build();
    head.append(&icon_square(icon, 36, "feature-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(title, &["card-title"]));
    words.append(subtitle);
    head.append(&words);
    if let Some(end) = end {
        head.append(end);
    }
    card.append(&head);
    card.append(&gtk::Box::builder().vexpand(true).build());
    card.append(bottom);
    card
}

fn subtitle(label: &str) -> gtk::Label {
    ellipsized(label, &["card-subtitle"])
}

fn terminals_card(win: &DesklinkWindow, device: &DeviceView) -> gtk::Box {
    let sessions = crate::terminal_previews::sessions();
    let shared = sessions
        .iter()
        .filter(|info| attached_devices(&info.id).contains(&device.id))
        .count();
    let mut words = format!("{} open", sessions.len());
    if shared > 0 {
        words.push_str(&format!(" · {shared} shared with {}", device.name));
    }
    let bottom: gtk::Widget = match sessions.first() {
        Some(info) => {
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
            let button = gtk::Button::builder()
                .child(&strip)
                .css_classes(["term-strip"])
                .build();
            button.update_property(&[gtk::accessible::Property::Label(&format!(
                "Open the {} terminal",
                crate::terminal_previews::display_title(&info.title)
            ))]);
            let (window, id) = (win.downgrade(), info.id.clone());
            button.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    window.show_page("terminals");
                    if let Some(page) = window.imp().terminals.borrow().as_ref() {
                        page.open_detail(&window, &id);
                    }
                }
            });
            if device.feature_transport_ready && info.exit_status.is_none() {
                // Walk away: lock here, carry on from the phone.
                let both = gtk::Box::builder()
                    .orientation(gtk::Orientation::Vertical)
                    .spacing(10)
                    .build();
                both.append(&button);
                let hand_off = icon_button(
                    "system-lock-screen-symbolic",
                    "Lock & Hand Off",
                    &["soft-button"],
                );
                hand_off.set_halign(gtk::Align::Start);
                hand_off.set_tooltip_text(Some(&format!(
                    "Lock this computer and open the newest terminal on {}",
                    device.name
                )));
                hand_off.set_action_name(Some("app.lock-hand-off"));
                both.append(&hand_off);
                both.upcast()
            } else {
                button.upcast()
            }
        }
        None => {
            let new = icon_button("list-add-symbolic", "New Terminal", &["dashed-button"]);
            new.set_action_name(Some("win.new-terminal"));
            new.set_action_target_value(Some(&"shell".to_variant()));
            new.upcast()
        }
    };
    feature_card(
        "utilities-terminal-symbolic",
        "Terminals",
        &subtitle(&words),
        Some(page_link("View All", "terminals").upcast_ref()),
        &bottom,
    )
}

fn screen_card(device: &DeviceView, approving: bool, approved: bool) -> gtk::Box {
    let words = match device.desktop_session {
        DesktopSession::None => "Not shared".to_string(),
        DesktopSession::Viewing => format!("{} is viewing · view only", device.name),
        DesktopSession::Controlling => format!("{} is viewing and controlling", device.name),
    };
    let bottom: gtk::Widget = if device.desktop_session == DesktopSession::None {
        let label = text(
            "Open Desk screen on the phone to view or control this computer.",
            &["card-note"],
        );
        label.set_wrap(true);
        label.upcast()
    } else {
        let row = gtk::Box::builder().spacing(10).build();
        let chip = gtk::Box::builder()
            .spacing(10)
            .css_classes(["switch-chip"])
            .build();
        chip.append(&gtk::Label::new(Some("Allow Control")));
        let switch = gtk::Switch::builder()
            .active(approved)
            .sensitive(!approving)
            .valign(gtk::Align::Center)
            .build();
        switch.update_property(&[gtk::accessible::Property::Label("Allow Control")]);
        let id = device.id.clone();
        switch.connect_state_set(move |switch, on| {
            if let Some(window) = switch.root().and_downcast::<DesklinkWindow>() {
                if on {
                    super::prefs::approve_remote_control(&window, &id);
                } else {
                    crate::device_links::config::set_remote_desktop_token(&id, None);
                    window.refresh_soon();
                }
            }
            gtk::glib::Propagation::Proceed
        });
        chip.append(&switch);
        row.append(&chip);
        let stop = super::soft_button("Stop Sharing");
        stop.set_css_classes(&["coral-outline"]);
        let id = device.id.clone();
        stop.connect_clicked(move |button| {
            crate::device_links::webrtc::coordinator::end_screen_session(&id);
            toast_from(button, "Stopped sharing the screen");
        });
        row.append(&stop);
        row.upcast()
    };
    feature_card(
        "screen-shared-symbolic",
        "Your screen",
        &subtitle(&words),
        None,
        &bottom,
    )
}

fn clipboard_card(daemon: &DaemonHandle, clip: Option<String>) -> gtk::Box {
    let on = crate::device_links::config::app_settings().clipboard_sync;
    let state = gtk::Box::builder().spacing(6).build();
    state.append(
        &gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes(["dot", if on { "mint" } else { "faint" }])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    state.append(&subtitle(if on {
        "In sync both ways"
    } else {
        "Off · not shared"
    }));
    let switch = clipboard_switch(daemon, on);
    let last = clip
        .as_deref()
        .map(|text| text.lines().next().unwrap_or_default())
        .unwrap_or("Copied text appears here");
    let item = gtk::Button::builder()
        .child(&ellipsized(&preview_line(last), &["mono"]))
        .action_name("win.show-page")
        .action_target(&"clipboard".to_variant())
        .css_classes(if on {
            vec!["clip-box"]
        } else {
            vec!["clip-box", "off"]
        })
        .build();
    item.update_property(&[gtk::accessible::Property::Label("Clipboard history")]);
    feature_card(
        "edit-paste-symbolic",
        "Clipboard",
        &state,
        Some(switch.upcast_ref()),
        &item,
    )
}

/// At most 200 characters of one line, so a huge copy cannot stretch a row.
pub(super) fn preview_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// The clipboard on/off switch.
pub(super) fn clipboard_switch(daemon: &DaemonHandle, on: bool) -> gtk::Switch {
    let switch = gtk::Switch::builder()
        .active(on)
        .valign(gtk::Align::Center)
        .build();
    switch.update_property(&[gtk::accessible::Property::Label("Share the clipboard")]);
    let daemon = daemon.clone();
    switch.connect_active_notify(move |switch| {
        daemon.send(DaemonCommand::SetClipboardSync(switch.is_active()));
        if let Some(window) = switch.root().and_downcast::<DesklinkWindow>() {
            window.refresh_soon();
        }
    });
    switch
}

fn notifications_card(device: &DeviceView) -> gtk::Box {
    let shown = crate::device_links::config::app_settings().shows_notifications(&device.id);
    let words = match (shown, device.notifications.len()) {
        (false, _) => "Hidden on this computer".to_string(),
        (true, 0) => "None".to_string(),
        (true, count) => format!("{count} new"),
    };
    let bottom: gtk::Widget = match device.notifications.first().filter(|_| shown) {
        Some(newest) => {
            let row = gtk::Box::builder().spacing(12).build();
            row.append(&icon_square(
                super::notifications::app_icon(&newest.app_name),
                36,
                "avatar-circle",
            ));
            let words = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(2)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            words.append(&super::notifications::heading_line(newest));
            words.append(&ellipsized(
                super::notifications::body(newest),
                &["text-dim", "body-small"],
            ));
            row.append(&words);
            if newest.request_reply_id.is_some() {
                let reply = icon_button("mail-reply-sender-symbolic", "Reply", &["soft-button"]);
                let id = newest.id.clone();
                reply.connect_clicked(move |button| {
                    if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                        window.show_page("notifications");
                        if let Some(page) = window.imp().notifications.borrow().as_ref() {
                            page.open_reply(&id);
                        }
                    }
                });
                row.append(&reply);
            }
            row.upcast()
        }
        None => text(
            if shown {
                "You're all caught up."
            } else {
                "Turn them on in Preferences."
            },
            &["card-note"],
        )
        .upcast(),
    };
    feature_card(
        "preferences-system-notifications-symbolic",
        "Notifications",
        &subtitle(&words),
        Some(page_link("View All", "notifications").upcast_ref()),
        &bottom,
    )
}

fn transfer_card(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let current = super::transfers::rows(daemon)
        .into_iter()
        .find(|row| row.device_id == device.id || row.device_id.is_empty());
    let bottom: gtk::Widget = match current {
        Some(row) => {
            let progress = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .build();
            let line = gtk::Box::builder().spacing(8).build();
            line.append(
                &gtk::Image::builder()
                    .icon_name(if row.incoming {
                        "go-down-symbolic"
                    } else {
                        "go-up-symbolic"
                    })
                    .css_classes(["text-dim"])
                    .accessible_role(gtk::AccessibleRole::Presentation)
                    .build(),
            );
            let name = ellipsized(&row.name, &["mono"]);
            name.set_hexpand(true);
            line.append(&name);
            let label = text(&progress_words(&row), &["caption", "text-dim"]);
            label.set_widget_name("overview-transfer-label");
            line.append(&label);
            progress.append(&line);
            let bar = gtk::ProgressBar::builder().css_classes(["thin"]).build();
            bar.set_widget_name("overview-transfer-bar");
            bar.update_property(&[gtk::accessible::Property::Label(&row.name)]);
            if let Some(fraction) = super::transfers::fraction(&row) {
                bar.set_fraction(fraction);
            }
            progress.append(&bar);
            progress.upcast()
        }
        None => {
            let drop = icon_button(
                "document-send-symbolic",
                "Drop files here to send",
                &["dashed-button", "quiet"],
            );
            let (daemon, id) = (daemon.clone(), device.id.clone());
            drop.connect_clicked(move |button| pick_and_send_files(&daemon, &id, button));
            drop.set_sensitive(device.feature_transport_ready);
            drop.upcast()
        }
    };
    let card = feature_card(
        "network-transmit-receive-symbolic",
        "File transfer",
        &subtitle("Send and receive, both ways"),
        Some(page_link("History", "transfers").upcast_ref()),
        &bottom,
    );
    if device.feature_transport_ready {
        let (daemon, id) = (daemon.clone(), device.id.clone());
        super::accept_dropped_files(&card, move || Some((daemon.clone(), id.clone())));
    }
    card
}

/// "42% · sending", or "Preparing…".
fn progress_words(row: &TransferRow) -> String {
    match super::transfers::fraction(row) {
        Some(fraction) => format!(
            "{}% · {}",
            (fraction * 100.0).round() as u32,
            if row.incoming { "receiving" } else { "sending" }
        ),
        None => "Preparing…".to_string(),
    }
}

/// Puts the latest terminal line and transfer progress in place, without
/// redrawing the page.
fn refresh_in_place(win: &DesklinkWindow, daemon: &DaemonHandle) {
    let Some((_, body)) = win.imp().bodies.borrow().get("overview").cloned() else {
        return;
    };
    let root = body.upcast::<gtk::Widget>();
    for widget in named_widgets(&root, "strip-line:") {
        let id = widget
            .widget_name()
            .trim_start_matches("strip-line:")
            .to_string();
        let Some(info) = crate::terminal_previews::sessions()
            .into_iter()
            .find(|info| info.id == id)
        else {
            continue;
        };
        let (lines, _) = crate::terminal_previews::preview(&info);
        let last = lines.last().cloned().unwrap_or_default();
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            if label.label() != last {
                label.set_label(&last);
            }
        }
    }
    let row = super::transfers::rows(daemon).into_iter().next();
    for widget in named_widgets(&root, "overview-transfer-") {
        let Some(row) = row.as_ref() else {
            continue;
        };
        if let Some(bar) = widget.downcast_ref::<gtk::ProgressBar>() {
            match super::transfers::fraction(row) {
                Some(fraction) => bar.set_fraction(fraction),
                None => bar.pulse(),
            }
        } else if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            label.set_label(&progress_words(row));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charging_says_when_the_phone_will_be_full() {
        assert_eq!(charging_words(None), " · charging");
        assert_eq!(charging_words(Some(-1)), " · charging");
        assert_eq!(charging_words(Some(0)), " · charging");
        assert_eq!(
            charging_words(Some(25 * 60_000)),
            " · charging, full in 25 min"
        );
        assert_eq!(charging_words(Some(30_000)), " · charging, full in 1 min");
        assert_eq!(
            charging_words(Some(95 * 60_000)),
            " · charging, full in 1 h 35 min"
        );
    }

    #[test]
    fn a_typed_link_gets_a_scheme_and_text_is_not_a_link() {
        assert_eq!(
            link_to_send("gnome.org/news").as_deref(),
            Some("https://gnome.org/news")
        );
        assert_eq!(
            link_to_send(" http://10.0.0.2:8080/x ").as_deref(),
            Some("http://10.0.0.2:8080/x")
        );
        assert_eq!(link_to_send("not a link"), None);
        assert_eq!(link_to_send("localhost"), None);
        assert_eq!(link_to_send(""), None);
    }

    #[test]
    fn a_long_copied_line_is_cut_short() {
        let long = "x".repeat(500);
        let shown = preview_line(&format!("{long}\nsecond"));
        assert_eq!(shown.chars().count(), 201);
        assert!(shown.ends_with('…'));
        assert_eq!(preview_line("short\nmore"), "short");
    }
}
