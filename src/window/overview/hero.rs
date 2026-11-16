//! Home's hero: the phone's name, state, battery and the words under it.
//!
//! Sibling edges: `tiles` (`desktop_sends`, `unavailable`).

use super::*;

/// The hero: the phone, its name, how it is connected, its battery, and
/// the two things most people came to do: send files, send the clipboard.
pub(super) fn hero(daemon: &DaemonHandle, device: &DeviceView, time: &str) -> gtk::Box {
    let band = gtk::Box::builder()
        .spacing(24)
        .css_classes(["hero-band"])
        .build();
    band.set_widget_name("responsive");
    let away = device.status == DeviceStatus::Unreachable;

    // A tablet is drawn on its side, wider than tall; anything else as a phone.
    let tablet = device_type_label(&device.device_type) == "Tablet";
    let (width, height) = if tablet { (112, 80) } else { (56, 96) };
    let phone = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .width_request(width)
        .height_request(height)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["phone-art"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    phone.set_hexpand(false);
    if tablet {
        phone.add_css_class("tablet");
    }
    phone.append(
        &gtk::Box::builder()
            .halign(gtk::Align::Center)
            .css_classes(["phone-camera"])
            .build(),
    );
    phone.append(
        &gtk::Label::builder()
            .label(time)
            .margin_top(if tablet { 14 } else { 18 })
            .css_classes(["phone-time"])
            .name("hero-clock")
            .build(),
    );
    band.append(&phone);

    let info = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    info.append(&hero_name(&device.name));
    // The pill and its detail. The round trip changes every reading and is
    // set in place (`show_round_trip`), so it is not part of the detail.
    let steady = DeviceView {
        rtt_ms: None,
        ..device.clone()
    };
    let words = Presentation::of(&steady);
    let link = gtk::Box::builder().spacing(10).build();
    link.append(&status_pill(words.status, words.word));
    let detail = text(&words.detail, &["dl-secondary"]);
    detail.set_wrap(true);
    detail.set_valign(gtk::Align::Center);
    link.append(&detail);
    let rtt = text("", &["dl-mono", "dl-caption"]);
    rtt.set_widget_name("hero-rtt");
    rtt.set_tooltip_text(Some("Round trip to the phone"));
    rtt.set_valign(gtk::Align::Center);
    rtt.set_visible(false);
    link.append(&rtt);
    info.append(&link);
    if let Some(battery) = device
        .battery_status
        .as_ref()
        .filter(|battery| battery.current_charge >= 0)
    {
        let line = gtk::Box::builder().spacing(10).build();
        // Charging is news only while it is live.
        let live = device.feature_transport_ready;
        let ring = battery_ring(battery.current_charge, battery.is_charging && live);
        let percent = text(
            &format!("{}%", battery.current_charge),
            &["mono", "battery-percent"],
        );
        // Not live: the last reading, dimmed so it does not pass for now.
        if !live {
            for stale in [ring.upcast_ref::<gtk::Widget>(), percent.upcast_ref()] {
                stale.add_css_class("dl-stale");
                stale.set_tooltip_text(Some("The last battery level DeskLink heard"));
            }
        }
        line.append(&ring);
        // The percent, the dot and the words are three labels, not one string
        // with a leading " · ": right to left, that string's spaces went to
        // the far end and the line read "25 min82%".
        let words = gtk::Box::builder()
            .spacing(4)
            .valign(gtk::Align::Center)
            .build();
        words.append(&percent);
        let after = match crate::window::last_seen_words(device) {
            Some(seen) => Some(seen),
            None if battery.is_charging && live => {
                Some(charging_words(battery.charge_time_remaining))
            }
            None => None,
        };
        if let Some(after) = after {
            words.append(&text("·", &["text-dim", "caption-strong"]));
            words.append(&text(&after, &["text-dim", "caption-strong"]));
        }
        line.append(&words);
        info.append(&line);
    }
    let gave_up = device
        .last_error
        .as_deref()
        .is_some_and(crate::device_links::webrtc::coordinator::is_unavailable);
    // "Can't reach <phone>" from the ladder only repeats the pill when the
    // phone is away; what failed while connecting is still said.
    if let Some(error) = device.last_error.as_ref().filter(|_| !(away && gave_up)) {
        let (words, details) = crate::window::hero_error_words(error);
        let label = text(&words, &["dl-secondary", "error"]);
        label.set_wrap(true);
        info.append(&label);
        // The system's own words, one click away (selectable, for a bug report).
        if let Some(details) = details {
            let expander = gtk::Expander::builder().label("Details").build();
            let raw = text(&details, &["caption", "text-dim"]);
            raw.set_wrap(true);
            raw.set_selectable(true);
            raw.set_xalign(0.0);
            expander.set_child(Some(&raw));
            info.append(&expander);
        }
    }
    // Away, or the ladder gave up: the one "Try again" on the page. Every
    // address gets a fresh chance and this computer announces itself.
    if away || gave_up {
        let retry = button(
            ButtonKind::Tonal,
            "Try again",
            Some("desklink-refresh-symbolic"),
        );
        retry.set_widget_name("hero-reconnect");
        retry.set_halign(gtk::Align::Start);
        let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
        retry.connect_clicked(move |button| {
            daemon.send(DaemonCommand::Reconnect(id.clone()));
            daemon.send(DaemonCommand::Discover);
            toast_from(button, &format!("Looking for {name}…"));
        });
        info.append(&retry);
    }
    band.append(&info);

    // One primary and one tonal button, stacked; under the name when narrow.
    let actions = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .build();
    actions.set_widget_name("hero-actions");
    let phone = PhoneFacts::from_device(device);
    if desktop_sends(PACKET_TYPE_SHARE_REQUEST) {
        let send = button(
            ButtonKind::Primary,
            "Send files…",
            Some("desklink-upload-file-symbolic"),
        );
        send.set_tooltip_text(Some(&format!("Send files to {} (Ctrl+O)", device.name)));
        match blocker(Feature::Send(PACKET_TYPE_SHARE_REQUEST), &phone) {
            Some(need) => unavailable(&send, &need),
            None => {
                let (daemon, id) = (daemon.clone(), device.id.clone());
                send.connect_clicked(move |button| pick_and_send_files(&daemon, &id, button));
            }
        }
        actions.append(&send);
    }
    let clipboard_type = crate::device_links::packet::PACKET_TYPE_CLIPBOARD;
    if desktop_sends(clipboard_type) {
        let clipboard = button(
            ButtonKind::Tonal,
            "Send clipboard",
            Some("desklink-content-paste-symbolic"),
        );
        match blocker(Feature::Send(clipboard_type), &phone) {
            Some(need) => unavailable(&clipboard, &need),
            None => {
                clipboard.connect_clicked(move |button| {
                    if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                        window.quick_action(QuickAction::Clipboard);
                    }
                });
            }
        }
        actions.append(&clipboard);
    }
    band.append(&actions);
    band
}

/// The phone's name, at most two lines, then an ellipsis with the whole name
/// as its tooltip and its accessible label.
pub(super) fn hero_name(name: &str) -> gtk::Label {
    let label = text(name, &["dl-hero"]);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    label.set_lines(2);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label.set_accessible_role(gtk::AccessibleRole::Heading);
    label.update_property(&[
        gtk::accessible::Property::Label(name),
        gtk::accessible::Property::Level(1),
    ]);
    crate::window::tooltip_when_cut(&label);
    label
}
