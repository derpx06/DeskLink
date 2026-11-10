//! The Overview page: the phone in a hero band with its quick actions, what
//! it is doing here, and a card for each feature under "On this computer"
//! and "On <phone>".

use adw::prelude::*;
use adw::subclass::prelude::*;

use super::requirements::{blocker, Feature, Need, PhoneFacts};
use super::ui_kit::banner::{banner, BannerKind};
use super::ui_kit::{
    button, card as kit_card, icon_button as kit_icon_button, icon_square as kit_icon_square,
    status_pill, tile, ButtonKind, TileKind, Tone,
};
use super::{
    can_send_feature, clock_time, confirm, device_type_label, ellipsized, named_widgets,
    pick_and_send_files, session_summary, text, toast_from, wrapped, DesklinkWindow, QuickAction,
};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DesktopSession, DeviceStatus, DeviceView};
use crate::device_links::packet::{
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST, PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_PING, PACKET_TYPE_SHARE_REQUEST,
};
use crate::device_links::webrtc::terminal_bridge::attached_devices;
use crate::device_links::webrtc::transfer_board::TransferRow;
use crate::link_glyph::{battery_ring, presence_dot, LinkState, Presentation};

pub(super) fn update(win: &DesklinkWindow, daemon: &DaemonHandle, device: &DeviceView) {
    let imp = win.imp();
    let since = imp.session_since.borrow().get(&device.id).cloned();
    let settings = crate::device_links::config::app_settings();
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
    // What is ongoing on the phone, by id: the card is redrawn when an item
    // comes or goes, and its live text changes in place.
    let activity_ids: Vec<String> = shown_activities(device, &settings)
        .into_iter()
        .map(|item| item.id.clone())
        .collect();
    let needs = super::requirements::missing(&super::requirements::Facts::now(Some(device)));
    let key = serde_json::to_string(&(
        super::requirements::key(&needs),
        crate::terminal_previews::sessions()
            .iter()
            .any(|info| info.exit_status.is_none()),
        &steady,
        &since,
        &activity_ids,
        &terminals,
        &transfers,
        settings.shows_notifications(&device.id),
        crate::device_links::folders::bucket_on(&device.id),
        gliding_onto(&device.id),
    ))
    .unwrap_or_default();
    if !win.page_changed("home", key) {
        refresh_in_place(win, daemon);
        show_activities(win, device, &settings);
        show_round_trip(win, device);
        show_clock(win, &minute);
        return;
    }
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(8)
        .margin_bottom(32)
        .css_classes(["overview-page"])
        .build();
    if device.trusted {
        if let Some(call) = super::calls::card(daemon, device, false) {
            page.append(&call);
        }
        for sound in super::sound::bars(daemon, device) {
            page.append(&sound);
        }
        if gliding_onto(&device.id) {
            page.append(&glide_bar(device));
        }
        if let Some(capture) = super::insert::bar(daemon, device) {
            page.append(&capture);
        }
        if let Some(summary) = session_summary(device) {
            page.append(&live_strip(daemon, device, &summary, since.as_deref()));
        }
        page.append(&hero(daemon, device, &minute));
        if let Some(card) = super::requirements::card(daemon, &needs) {
            page.append(&card);
        }
        if let Some(card) = activity_card(device, &settings) {
            page.append(&card);
        }
        if let Some(bar) = now_playing_bar(daemon, device) {
            page.append(&bar);
        }
        // Can't reach is said once, in the hero, with its Try again.
        if device.status == DeviceStatus::Paired && !device.feature_transport_ready {
            page.append(&reconnecting_row(device));
        }
        page.append(&tiles(daemon, device));
        page.append(&columns(daemon, device));
        page.append(&terminals_card(win, device));
    } else if device.status == DeviceStatus::PairRequestedByPeer {
        page.append(&super::pairing::pair_request_view(daemon, device));
    } else if device.status == DeviceStatus::PairRequested {
        page.append(&super::pairing::pair_waiting_view(daemon, device));
    } else {
        page.append(&super::pairing::pair_idle_view(daemon, device));
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(944)
        .tightening_threshold(800)
        .child(&page)
        .build();
    win.replace_body("home", &clamp);
    refresh_in_place(win, daemon);
    show_round_trip(win, device);
    show_clock(win, &minute);
}

/// The phone picture's clock, in place: the page is not rebuilt (and its
/// open menus closed) every minute for it.
fn show_clock(win: &DesklinkWindow, minute: &str) {
    let Some((_, body)) = win.imp().bodies.borrow().get("home").cloned() else {
        return;
    };
    for widget in named_widgets(body.upcast_ref(), "hero-clock") {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            if label.label() != minute {
                label.set_label(minute);
            }
        }
    }
}

/// The phone's ongoing items that show now (W13).
fn shown_activities<'a>(
    device: &'a DeviceView,
    settings: &crate::device_links::config::AppSettings,
) -> Vec<&'a crate::device_links::activity::Activity> {
    crate::device_links::activity::shown(
        device.activity.as_ref(),
        device.feature_transport_ready,
        settings.shows_notifications(&device.id),
        crate::device_links::activity::now_ms(),
    )
}

/// "Happening on <phone>": its timers, downloads and navigation, each with
/// its live countdown or progress. `None` when nothing is ongoing.
fn activity_card(
    device: &DeviceView,
    settings: &crate::device_links::config::AppSettings,
) -> Option<gtk::Box> {
    let items = shown_activities(device, settings);
    if items.is_empty() {
        return None;
    }
    let card = kit_card(Some(&format!("Happening on {}", device.name)));
    for (index, item) in items.into_iter().enumerate() {
        let row = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(3)
            .build();
        let head = gtk::Box::builder().spacing(12).build();
        let title = if item.title.is_empty() {
            &item.app_label
        } else {
            &item.title
        };
        let name = ellipsized(title, &["body"]);
        name.set_hexpand(true);
        head.append(&name);
        let live = text("", &["numeric", "text-dim"]);
        live.set_widget_name(&format!("activity-live:{index}"));
        head.append(&live);
        row.append(&head);
        if !item.app_label.is_empty() || !item.text.is_empty() {
            let detail = if item.text.is_empty() {
                item.app_label.clone()
            } else {
                format!("{} · {}", item.app_label, item.text)
            };
            row.append(&ellipsized(&detail, &["caption", "text-dim"]));
        }
        if item.progress >= 0 {
            let bar = super::ui_kit::progress(0.0);
            bar.set_widget_name(&format!("activity-bar:{index}"));
            row.append(&bar);
        }
        row.update_property(&[gtk::accessible::Property::Label(&format!(
            "{}, {}",
            title, item.app_label
        ))]);
        card.append(&row);
    }
    Some(card)
}

/// The activity card's countdowns and bars, in place (every tick).
fn show_activities(
    win: &DesklinkWindow,
    device: &DeviceView,
    settings: &crate::device_links::config::AppSettings,
) {
    let Some((_, body)) = win.imp().bodies.borrow().get("home").cloned() else {
        return;
    };
    paint_activities(
        body.upcast_ref(),
        device,
        settings,
        crate::device_links::activity::now_ms(),
    );
}

/// [`show_activities`] on any root, at the clock `now_ms`.
fn paint_activities(
    root: &gtk::Widget,
    device: &DeviceView,
    settings: &crate::device_links::config::AppSettings,
    now_ms: i64,
) {
    let Some(snapshot) = device.activity.as_ref() else {
        return;
    };
    let items = crate::device_links::activity::shown(
        Some(snapshot),
        device.feature_transport_ready,
        settings.shows_notifications(&device.id),
        now_ms,
    );
    for (index, item) in items.into_iter().enumerate() {
        for widget in named_widgets(root, &format!("activity-live:{index}")) {
            if let Some(label) = widget.downcast_ref::<gtk::Label>() {
                let words = item.live_text(snapshot, now_ms).unwrap_or_default();
                if label.label() != words {
                    label.set_label(&words);
                }
            }
        }
        for widget in named_widgets(root, &format!("activity-bar:{index}")) {
            if let Some(bar) = widget.downcast_ref::<gtk::ProgressBar>() {
                bar.set_fraction(f64::from(item.progress.clamp(0, 100)) / 100.0);
            }
        }
    }
}

/// The hero's "· 12 ms", in place.
fn show_round_trip(win: &DesklinkWindow, device: &DeviceView) {
    let Some((_, body)) = win.imp().bodies.borrow().get("home").cloned() else {
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
/// the two things most people came to do: send files, send the clipboard.
fn hero(daemon: &DaemonHandle, device: &DeviceView, time: &str) -> gtk::Box {
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
        let words = gtk::Box::builder().valign(gtk::Align::Center).build();
        words.append(&percent);
        if let Some(seen) = super::last_seen_words(device) {
            words.append(&text(
                &format!(" · {seen}"),
                &["text-dim", "caption-strong"],
            ));
        } else if battery.is_charging && live {
            words.append(&text(
                &charging_words(battery.charge_time_remaining),
                &["text-dim", "caption-strong"],
            ));
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
        let (words, details) = super::hero_error_words(error);
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
        match blocker(Feature::Send(PACKET_TYPE_SHARE_REQUEST), &phone, true) {
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
        match blocker(Feature::Send(clipboard_type), &phone, true) {
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
fn hero_name(name: &str) -> gtk::Label {
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
    super::tooltip_when_cut(&label);
    label
}

/// Where the tile row gets fewer columns (its width in px, the window sets
/// them as it is resized, `apply_tile_columns`): below 840 it has three, below
/// 560 two, so there is never an orphan tile on a line of its own.
const TILE_STEPS: [(i32, u32); 2] = [(840, 3), (560, 2)];

/// How many tiles a row of `width` px holds.
pub(super) fn tile_columns(width: i32) -> u32 {
    let mut columns = 6;
    for (below, fewer) in TILE_STEPS {
        if width < below {
            columns = fewer;
        }
    }
    columns
}

/// A control that cannot be used now: it looks disabled but stays a focus
/// stop, its hint (a tile's) is the short reason, its tooltip the need, and a
/// click says why with the fix. Nothing it would send is wired.
fn unavailable(control: &gtk::Button, need: &Need) {
    control.add_css_class("dl-unavailable");
    control.set_tooltip_text(Some(&need.title));
    if let Some(hint) = named_widgets(control.upcast_ref(), "")
        .into_iter()
        .find(|w| w.has_css_class("dl-tile-hint"))
        .and_downcast::<gtk::Label>()
    {
        hint.set_label(super::requirements::short(need));
    }
    control.update_property(&[gtk::accessible::Property::Description(&format!(
        "Unavailable. {}",
        need.title
    ))]);
    let need = need.clone();
    control.connect_clicked(move |control| super::requirements::explain(control, &need));
}

/// The six quick actions: Phone screen, Insert, Sound, Ring phone, Hand off
/// and More. A row of six, three by two, or two by three, by the page's width.
fn tiles(daemon: &DaemonHandle, device: &DeviceView) -> gtk::FlowBox {
    let kind = device_type_label(&device.device_type);
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(2)
        .max_children_per_line(tile_columns(1000))
        .column_spacing(12)
        .row_spacing(12)
        .build();
    flow.set_widget_name("tile-row");
    let add = |tile: &gtk::Button| {
        tile.set_hexpand(true);
        flow.append(tile);
        if let Some(cell) = tile.parent() {
            cell.set_focusable(false);
        }
    };
    let open_terminal = crate::terminal_previews::sessions()
        .iter()
        .any(|info| info.exit_status.is_none());
    let phone = PhoneFacts::from_device(device);
    let blocked = |feature| blocker(feature, &phone, open_terminal);
    if desktop_sends(PACKET_TYPE_MOUSEPAD_REQUEST) {
        let need = blocked(Feature::Send(PACKET_TYPE_MOUSEPAD_REQUEST));
        let screen = tile(
            "desklink-mobile-screen-share-symbolic",
            &format!("{kind} screen"),
            Some("See and control it"),
            TileKind::Normal,
        );
        screen.set_tooltip_text(Some(&format!("Show {kind} Screen")));
        match &need {
            Some(need) => unavailable(&screen, need),
            None => {
                let (screen_daemon, id) = (daemon.clone(), device.id.clone());
                screen.connect_clicked(move |button| {
                    if let Some(root) = button.root().and_downcast::<gtk::Window>() {
                        crate::remote_control::show_remote_control_dialog(
                            screen_daemon.clone(),
                            id.clone(),
                            &root,
                        );
                    }
                });
            }
        }
        add(&screen);

        let insert = tile(
            "desklink-photo-camera-symbolic",
            "Insert",
            Some("Photo, scan, text"),
            TileKind::Normal,
        );
        insert.set_tooltip_text(Some(&format!(
            "Insert from {}: a photo, a scan, text or a QR code",
            device.name
        )));
        match &need {
            Some(need) => unavailable(&insert, need),
            None => super::insert::attach_menu(daemon, device, &insert),
        }
        add(&insert);

        // Sound both ways (F1): its two switches in a popover.
        let on = device.sound.is_some();
        let sound = tile(
            "desklink-volume-up-symbolic",
            "Sound",
            Some(if on { "On" } else { "Phone to here" }),
            if on { TileKind::On } else { TileKind::Normal },
        );
        sound.set_tooltip_text(Some(&format!("Sound with {}", device.name)));
        match &need {
            Some(need) => unavailable(&sound, need),
            None => super::sound::attach_menu(daemon, device, &sound),
        }
        add(&sound);
    }
    if desktop_sends(PACKET_TYPE_FINDMYPHONE_REQUEST) {
        let ring = tile(
            "desklink-ring-volume-symbolic",
            &format!("Ring {}", kind.to_lowercase()),
            Some("Find it"),
            TileKind::Normal,
        );
        ring.set_tooltip_text(Some(&format!("Ring {kind}")));
        match blocked(Feature::Send(PACKET_TYPE_FINDMYPHONE_REQUEST)) {
            Some(need) => unavailable(&ring, &need),
            None => {
                ring.connect_clicked(|button| {
                    if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                        window.quick_action(QuickAction::Ring);
                    }
                });
            }
        }
        add(&ring);
    }
    // Walk away: lock here, carry on from the phone, in the newest terminal.
    let hand_off = tile(
        "desklink-lock-symbolic",
        "Hand off",
        Some("Continue on phone"),
        TileKind::Normal,
    );
    hand_off.set_tooltip_text(Some(&format!(
        "Lock this computer and open the newest terminal on {}",
        device.name
    )));
    match blocked(Feature::HandOff) {
        Some(need) => unavailable(&hand_off, &need),
        // Bound only when it can run: a bound action's state would make the
        // tile live whatever it says.
        None => hand_off.set_action_name(Some("app.lock-hand-off")),
    }
    add(&hand_off);
    if let Some(more) = more_tile(daemon, device) {
        add(&more);
    }

    flow
}

/// "More actions for Pixel 9: Lock phone, Send a nudge": what is in it.
fn more_label(name: &str, titles: &[&str]) -> String {
    format!("More actions for {name}: {}", titles.join(", "))
}

/// "More": Lock phone, Send a nudge, Send a link. A popover of rows, each an
/// icon square, a title and a hint. `None` when the phone takes none.
fn more_tile(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Button> {
    type Run = Box<dyn Fn(&gtk::Widget)>;
    let kind = device_type_label(&device.device_type);
    let phone = PhoneFacts::from_device(device);
    // Each row: icon, title, hint, what it needs, what it does.
    let mut items: Vec<(&str, String, &str, Option<Need>, Run)> = Vec::new();
    let need = |packet| blocker(Feature::Send(packet), &phone, true);
    if desktop_sends(PACKET_TYPE_LOCK_REQUEST) {
        items.push((
            "desklink-lock-symbolic",
            format!("Lock {}", kind.to_lowercase()),
            "Lock its screen",
            need(PACKET_TYPE_LOCK_REQUEST),
            Box::new(|anchor: &gtk::Widget| {
                if let Some(window) = anchor.root().and_downcast::<DesklinkWindow>() {
                    window.quick_action(QuickAction::Lock);
                }
            }),
        ));
    }
    if desktop_sends(PACKET_TYPE_PING) {
        items.push((
            "desklink-vibration-symbolic",
            "Send a nudge".to_string(),
            "A quiet notification",
            need(PACKET_TYPE_PING),
            Box::new(|anchor: &gtk::Widget| {
                if let Some(window) = anchor.root().and_downcast::<DesklinkWindow>() {
                    window.quick_action(QuickAction::Ping);
                }
            }),
        ));
    }
    if desktop_sends(PACKET_TYPE_SHARE_REQUEST) {
        let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
        items.push((
            "desklink-link-symbolic",
            "Send a link…".to_string(),
            "Opens on the phone",
            need(PACKET_TYPE_SHARE_REQUEST),
            Box::new(move |anchor: &gtk::Widget| {
                show_send_link_dialog(daemon.clone(), id.clone(), &name, anchor)
            }),
        ));
    }
    if items.is_empty() {
        return None;
    }
    let more = tile(
        "desklink-more-vert-symbolic",
        "More",
        Some("Lock, nudge, link"),
        TileKind::Normal,
    );
    let titles: Vec<&str> = items
        .iter()
        .map(|(_, title, _, _, _)| title.as_str())
        .collect();
    more.update_property(&[gtk::accessible::Property::Label(&more_label(
        &device.name,
        &titles,
    ))]);
    // Not reachable: every row would say the same, so the tile says it.
    if let Some(reach) = need(PACKET_TYPE_PING).filter(|need| need.id == "reach") {
        unavailable(&more, &reach);
        return Some(more);
    }
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["dl-row-group"])
        .build();
    let runs: Vec<Result<Run, Need>> = items
        .into_iter()
        .map(|(icon, title, hint, need, run)| {
            let row = super::ui_kit::list_row(
                icon,
                Tone::Neutral,
                &title,
                Some(need.as_ref().map_or(hint, |need| need.title.as_str())),
            );
            row.set_activatable(true);
            if need.is_some() {
                // Listed, looks unavailable; choosing it says why.
                row.add_css_class("dl-unavailable");
            }
            list.append(&row);
            need.map_or(Ok(run), Err)
        })
        .collect();
    let popover = gtk::Popover::builder()
        .child(&list)
        .css_classes(["more-popover"])
        .build();
    popover.set_parent(&more);
    more.connect_destroy({
        let popover = popover.clone();
        move |_| popover.unparent()
    });
    let weak = popover.downgrade();
    // Weakly: the tile owns the popover, which owns this closure.
    let anchor = more.downgrade();
    list.connect_row_activated(move |_, row| {
        if let Some(popover) = weak.upgrade() {
            popover.popdown();
        }
        let (Some(anchor), Some(run)) = (
            anchor.upgrade(),
            usize::try_from(row.index()).ok().and_then(|i| runs.get(i)),
        ) else {
            return;
        };
        match run {
            Ok(run) => run(anchor.upcast_ref()),
            Err(need) => super::requirements::explain(&anchor, need),
        }
    });
    let shown = popover.downgrade();
    more.connect_clicked(move |_| {
        if let Some(popover) = shown.upgrade() {
            popover.popup();
        }
    });
    Some(more)
}

/// Whether this build of DeskLink sends `packet` at all (a feature it has).
fn desktop_sends(packet: &str) -> bool {
    crate::device_links::features::FeatureRegistry::desktop()
        .outgoing_capabilities()
        .iter()
        .any(|capability| capability == packet)
}

/// A row of controls that wraps when the page is too narrow for it. Its
/// cells are not focus stops: Tab goes from control to control.
pub(super) fn wrap_row(min: u32, max: u32) -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .min_children_per_line(min)
        .max_children_per_line(max)
        .column_spacing(2)
        .halign(gtk::Align::End)
        .valign(gtk::Align::Center)
        .build()
}

pub(super) fn add_wrapped(row: &gtk::FlowBox, widget: &impl IsA<gtk::Widget>) {
    row.append(widget);
    if let Some(cell) = widget.as_ref().parent() {
        cell.set_focusable(false);
    }
}

/// "Send a link": a URL to open on the phone.
pub(super) fn show_send_link_dialog(
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

/// Endings that name a file, not a site (`README.md`, `photo.jpg`), though many
/// are also country domains: a bare word with one of them is not a link.
const FILE_ENDINGS: [&str; 44] = [
    "md", "txt", "pdf", "doc", "docx", "odt", "xls", "xlsx", "ods", "csv", "ppt", "pptx", "rtf",
    "png", "jpg", "jpeg", "gif", "svg", "webp", "bmp", "heic", "mp3", "mp4", "mkv", "wav", "flac",
    "zip", "tar", "gz", "xz", "7z", "rs", "py", "js", "ts", "kt", "java", "sh", "json", "toml",
    "yml", "yaml", "log", "apk",
];

/// A host name: dot-separated labels of letters, digits and hyphens. A bare
/// word must look like a site (two labels, an alphabetic ending of two or
/// more letters, not a file ending); after `http(s)://` any such host does.
fn is_host(host: &str, bare: bool) -> bool {
    let labels: Vec<&str> = host.split('.').collect();
    let label_ok = |label: &&str| {
        !label.is_empty()
            && label.len() <= 63
            && !label.starts_with('-')
            && !label.ends_with('-')
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
    };
    if !labels.iter().all(label_ok) {
        return false;
    }
    if !bare {
        return true;
    }
    let ending = labels
        .last()
        .copied()
        .unwrap_or_default()
        .to_ascii_lowercase();
    labels.len() >= 2
        && ending.len() >= 2
        && ending.bytes().all(|b| b.is_ascii_alphabetic())
        && !FILE_ENDINGS.contains(&ending.as_str())
}

/// What the user typed as a link to open: an `http://` or `https://` address,
/// or a bare `host.tld` (with an optional port and path) that gets `https://`.
/// A file name (`README.md`), another scheme (`file:///…`, `javascript:`,
/// `mailto:`), a name with spaces, or a host with `user@` is not a link.
pub(crate) fn link_to_send(typed: &str) -> Option<String> {
    let typed = typed.trim();
    if typed.is_empty()
        || typed.len() > 2000
        || typed.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    let lower = typed.to_ascii_lowercase();
    let (rest, bare) = if lower.starts_with("https://") {
        (&typed[8..], false)
    } else if lower.starts_with("http://") {
        (&typed[7..], false)
    } else if typed.contains("://") {
        return None;
    } else {
        (typed, true)
    };
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    };
    if port.is_some_and(|port| {
        port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit())
    }) || !is_host(host, bare)
    {
        return None;
    }
    Some(if bare {
        format!("https://{typed}")
    } else {
        typed.to_string()
    })
}

/// The phone's music while it plays: track, progress and controls.
fn now_playing_bar(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Box> {
    // Not while the link is down: what the phone last said is stale.
    if !device.feature_transport_ready {
        return None;
    }
    // Paused too: hiding the bar on Pause left no button to play again.
    let media = device
        .media_status
        .as_ref()
        .filter(|media| media.is_playing || !media.title.is_empty())?;
    let bar = gtk::Box::builder()
        .spacing(16)
        .css_classes(["dl-card", "dl-card-new", "now-playing"])
        .build();
    bar.append(&art(52, media.album_art.as_ref()));
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
/// The cover, or the gradient square when there is none. The pixels were
/// decoded on the daemon thread; wrapping them is a copy-free texture.
fn art(size: i32, cover: Option<&crate::device_links::media_art::PhoneArt>) -> gtk::Box {
    let art = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .valign(gtk::Align::Center)
        .css_classes(["album-art"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .overflow(gtk::Overflow::Hidden)
        .build();
    art.set_hexpand(false);
    if let Some(cover) = cover {
        let format = if cover.has_alpha {
            gtk::gdk::MemoryFormat::R8g8b8a8
        } else {
            gtk::gdk::MemoryFormat::R8g8b8
        };
        let texture = gtk::gdk::MemoryTexture::new(
            cover.width,
            cover.height,
            format,
            &cover.pixels,
            cover.stride,
        );
        let picture = gtk::Picture::builder()
            .paintable(&texture)
            .content_fit(gtk::ContentFit::Cover)
            .hexpand(true)
            .vexpand(true)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        art.append(&picture);
    }
    art
}

/// "Pixel 9 is using this computer · viewing your screen · since 14:02",
/// with End all, which asks first.
fn live_strip(
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
    if device.desktop_extended {
        if let Some(settings) = gtk::glib::find_program_in_path("gnome-control-center") {
            let arrange = button(ButtonKind::Outline, "Arrange…", None);
            arrange.set_tooltip_text(Some("Place the phone's display beside this screen"));
            arrange.connect_clicked(move |_| {
                let _ = std::process::Command::new(&settings).arg("display").spawn();
            });
            strip.add_action(&arrange);
        }
    }
    // Its speaker toggle (F1): this computer's sound to the phone too.
    if device.desktop_session != DesktopSession::None {
        let on = super::sound::is_on(device, "desk-sound");
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
        super::sound::toggle(daemon, device, "desk-sound", &sound);
        strip.add_action(&sound);
    }
    let end = button(ButtonKind::Danger, "End all", None);
    end.set_tooltip_text(Some("End the phone's sessions on this computer"));
    let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
    end.connect_clicked(move |button| {
        let (daemon, id, target) = (daemon.clone(), id.clone(), button.clone());
        confirm(
            button,
            &format!("End sessions with {name}?"),
            "The phone stops using the screen and its terminals here. The terminals keep running.",
            "_End sessions",
            move || {
                daemon.send(DaemonCommand::EndPhoneSessions(Some(id.clone())));
                toast_from(&target, "Sessions ended");
            },
        );
    });
    strip.add_action(&end);
    strip.root
}

/// "Viewing your screen · since 14:02": the summary with a capital.
fn live_detail(summary: &str, since: Option<&str>) -> String {
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
fn reconnecting_row(device: &DeviceView) -> gtk::Box {
    banner(
        BannerKind::Info,
        "desklink-refresh-symbolic",
        &format!("Connecting to {}", device.name),
        Some("Setting up a secure connection. This takes a few seconds."),
    )
    .root
}

/// Notifications and Files side by side, or stacked when narrow.
fn columns(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let columns = gtk::Box::builder()
        .spacing(16)
        .homogeneous(true)
        .valign(gtk::Align::Start)
        .build();
    columns.set_widget_name("responsive");
    columns.append(&notifications_card(device));
    columns.append(&files_card(daemon, device));
    columns
}

/// The Files card: the Bucket's summary and drop target, and the transfer
/// moving now, if one is.
fn files_card(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let card = super::bucket::overview_card(device);
    if let Some(progress) = transfer_progress(daemon, device) {
        card.append(&progress);
    }
    card
}

/// A card: its title and a line under it on the left, an optional widget at
/// the end of the header, and `body` below. Dimmed while what it shows is
/// the last known (`stale`).
fn feature_card(
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
fn page_button(label: &str, name: &str) -> gtk::Button {
    let button = button(ButtonKind::Text, label, None);
    button.set_valign(gtk::Align::Center);
    button.set_action_name(Some("win.show-page"));
    button.set_action_target_value(Some(&name.to_variant()));
    button
}

/// The Terminals card: how many are open, New terminal, and the newest one
/// as a strip to open it.
fn terminals_card(win: &DesklinkWindow, device: &DeviceView) -> gtk::Box {
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
    card
}

/// "1 open", "2 open · 1 shared with Pixel 9".
fn terminals_words(open: usize, shared: usize, phone: &str) -> String {
    let mut words = format!("{open} open");
    if shared > 0 {
        words.push_str(&format!(" · {shared} shared with {phone}"));
    }
    words
}

/// At most 200 characters of one line, so a huge copy cannot stretch a row.
pub(super) fn preview_line(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    match line.char_indices().nth(200) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// The Notifications card: the newest two, each with Reply when the phone
/// takes one; or why there is nothing.
fn notifications_card(device: &DeviceView) -> gtk::Box {
    let shown = crate::device_links::config::app_settings().shows_notifications(&device.id);
    let mut words = match (shown, device.notifications.len()) {
        (false, _) => "Hidden on this computer".to_string(),
        (true, 0) => "None".to_string(),
        (true, count) => format!("{count} new"),
    };
    if let Some(seen) = super::last_seen_words(device).filter(|_| shown) {
        words.push_str(&format!(" · {seen}"));
    }
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    if shown && !device.notifications.is_empty() {
        for newest in device.notifications.iter().take(2) {
            let row = gtk::Box::builder().spacing(12).build();
            row.append(&kit_icon_square(
                super::notifications::app_icon(&newest.app_name),
                Tone::Accent,
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
                &["dl-secondary"],
            ));
            row.append(&words);
            if newest.request_reply_id.is_some() {
                let reply = button(ButtonKind::Tonal, "Reply", Some("desklink-reply-symbolic"));
                reply.set_valign(gtk::Align::Center);
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
                row.append(&reply);
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
    feature_card(
        "Notifications",
        &words,
        Some(page_button("View all", "notifications").upcast_ref()),
        &body,
        shown && !device.feature_transport_ready,
    )
}

fn transfer_progress(daemon: &DaemonHandle, device: &DeviceView) -> Option<gtk::Box> {
    let row = super::transfers::rows(daemon)
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
    let bar = super::ui_kit::progress(0.0);
    bar.set_widget_name("overview-transfer-bar");
    bar.update_property(&[gtk::accessible::Property::Label(&row.name)]);
    if let Some(fraction) = super::transfers::fraction(&row) {
        bar.set_fraction(fraction);
    }
    progress.append(&bar);
    Some(progress)
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
    let Some((_, body)) = win.imp().bodies.borrow().get("home").cloned() else {
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

/// Whether this computer's pointer is on `device_id` now (glide, F4).
fn gliding_onto(device_id: &str) -> bool {
    crate::device_links::glide::status().gliding.as_deref() == Some(device_id)
}

/// The coral in-use bar while gliding. No End button: every click goes to
/// the phone then; Esc+Super is the way back.
fn glide_bar(device: &DeviceView) -> gtk::Box {
    banner(
        BannerKind::Live,
        "desklink-mouse-symbolic",
        &format!("Controlling {}", device.name),
        Some("This mouse and keyboard are on the phone. Esc+Super to return."),
    )
    .root
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

    /// The same vectors as `tests/model.test.mjs` (`looksLikeLink`).
    #[test]
    fn only_web_addresses_and_site_names_become_links() {
        for (typed, sent) in [
            ("https://example.org/a?b=c", "https://example.org/a?b=c"),
            ("http://example.org", "http://example.org"),
            ("example.org", "https://example.org"),
            (
                "www.example.com/path?x=1",
                "https://www.example.com/path?x=1",
            ),
            ("sub.example.co.uk", "https://sub.example.co.uk"),
            ("example.org:8080/x", "https://example.org:8080/x"),
            ("HTTPS://Example.org", "HTTPS://Example.org"),
            ("http://localhost:3000", "http://localhost:3000"),
        ] {
            assert_eq!(link_to_send(typed).as_deref(), Some(sent), "{typed}");
        }
        for typed in [
            "README.md",
            "notes.txt",
            "photo.jpg",
            "foo.md/bar",
            "archive.tar.gz",
            "file:///etc/passwd",
            "ftp://example.org",
            "javascript:alert.x",
            "mailto:a@b.org",
            "data:text/plain,hi",
            "https://",
            "http:///x",
            "https://exa mple.org",
            "hello",
            "1.2.3",
            "192.168.1.1",
            "a..b.org",
            "-a.org",
            "example.org:abc",
            "example.org:",
            "https://good.com@evil.org",
            "[::1]:80",
        ] {
            assert_eq!(link_to_send(typed), None, "{typed}");
        }
    }

    #[test]
    fn the_more_tile_names_what_is_in_it() {
        assert_eq!(
            more_label("Pixel 9", &["Lock phone", "Send a link…"]),
            "More actions for Pixel 9: Lock phone, Send a link…"
        );
    }

    /// Six tiles in a row, then three by two, then two by three: never one
    /// alone on a line.
    #[test]
    fn the_tile_row_never_leaves_an_orphan() {
        assert_eq!(tile_columns(1000), 6);
        assert_eq!(tile_columns(840), 6);
        assert_eq!(tile_columns(839), 3);
        assert_eq!(tile_columns(560), 3);
        assert_eq!(tile_columns(559), 2);
        assert_eq!(tile_columns(280), 2);
        // 6 tiles divide into rows of 6, 3 and 2.
        for width in [1000, 700, 300] {
            assert_eq!(6 % tile_columns(width), 0, "{width}");
        }
    }

    #[test]
    fn the_live_strip_and_the_card_words_are_sentences() {
        assert_eq!(
            live_detail("viewing your screen", None),
            "Viewing your screen"
        );
        assert_eq!(
            live_detail("in a terminal, viewing your screen", Some("14:02")),
            "In a terminal, viewing your screen · since 14:02"
        );
        assert_eq!(
            live_detail("a second display", Some("")),
            "A second display"
        );
        assert_eq!(terminals_words(0, 0, "Pixel"), "0 open");
        assert_eq!(
            terminals_words(2, 1, "Pixel"),
            "2 open · 1 shared with Pixel"
        );
    }

    /// A tile that cannot be used says why in a word, with the same words as
    /// the status pill.
    #[test]
    fn a_tile_that_cannot_be_used_says_why() {
        let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        let reason = |device: &DeviceView| {
            blocker(
                Feature::Send(PACKET_TYPE_PING),
                &PhoneFacts::from_device(device),
                true,
            )
            .map(|need| super::super::requirements::short(&need))
        };
        device.incoming_capabilities = vec![PACKET_TYPE_PING.into()];
        assert_eq!(reason(&device), None);
        device.feature_transport_ready = false;
        assert_eq!(reason(&device), Some("Connecting…"));
        device.status = DeviceStatus::Unreachable;
        assert_eq!(reason(&device), Some("Can't reach"));
        assert_eq!(
            Presentation::of(&device).word,
            reason(&device).unwrap(),
            "the pill and the tile agree"
        );
    }

    /// The hero's name is two lines at most, then an ellipsis, and a heading
    /// for screen readers. (Needs a display.)
    #[test]
    fn a_long_name_is_two_lines_then_cut() {
        crate::test_display::run(|| {
            let name = hero_name(&"Aman's Pixel 9 Pro XL Fold ".repeat(6));
            assert_eq!(name.lines(), 2);
            assert_eq!(name.ellipsize(), gtk::pango::EllipsizeMode::End);
            assert!(name.wraps() && name.has_css_class("dl-hero"));
            assert_eq!(name.accessible_role(), gtk::AccessibleRole::Heading);
        });
    }

    /// Every label under `root`, and the widgets carrying `class`.
    fn words_and(root: &gtk::Widget, class: &str) -> (Vec<String>, Vec<gtk::Widget>) {
        let all = named_widgets(root, "");
        let words = all
            .iter()
            .filter_map(|w| w.downcast_ref::<gtk::Label>())
            .map(|label| label.label().to_string())
            .collect();
        let marked = all.into_iter().filter(|w| w.has_css_class(class)).collect();
        (words, marked)
    }

    /// Away for hours: "Can't reach" is said once (the pill and one line), with
    /// one Try again and no stale "dropped while it was being set up"; the
    /// hero's two buttons are unavailable like the tiles, the battery reads as
    /// old, and a tablet is drawn as one. (Needs a display.)
    #[test]
    fn an_away_tablet_says_so_once_and_nothing_reads_as_live() {
        crate::test_display::run(|| {
            let info =
                crate::device_links::device_info::DeviceInfo::local("t".into(), "emulator".into());
            let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
            device.device_type = "tablet".into();
            device.status = DeviceStatus::Unreachable;
            device.last_seen = Some(1_700_000_000);
            device.battery_status = Some(crate::device_links::device::BatteryStatus {
                current_charge: 100,
                is_charging: false,
                battery_quantity: None,
                threshold_event: None,
                charge_time_remaining: None,
            });
            let (daemon, _commands) = crate::device_links::daemon::DaemonHandle::detached();
            let once = |device: &DeviceView| {
                let band = hero(&daemon, device, "17:07");
                let root = band.upcast_ref::<gtk::Widget>().clone();
                let (words, _) = words_and(&root, "");
                let reach = words.iter().filter(|w| w.contains("Can't reach")).count();
                let retries = named_widgets(&root, "hero-reconnect").len();
                (root, words, reach, retries)
            };

            let (root, words, reach, retries) = once(&device);
            assert_eq!((reach, retries), (1, 1), "{words:?}");
            assert!(!words.iter().any(|w| w.contains("being set up")));
            let actions = named_widgets(&root, "hero-actions");
            let (_, dimmed) = words_and(&actions[0], "dl-unavailable");
            assert_eq!(dimmed.len(), 2, "Send files and Send clipboard");
            let (_, stale) = words_and(&root, "dl-stale");
            assert_eq!(stale.len(), 2, "the ring and its percent");
            let (_, art) = words_and(&root, "phone-art");
            assert!(art[0].has_css_class("tablet"));
            assert!(art[0].width_request() > art[0].height_request());

            // The ladder's own "Can't reach … · Try again" adds nothing more.
            device.last_error = Some(crate::device_links::webrtc::coordinator::unavailable_words(
                "emulator",
            ));
            let (_, words, reach, retries) = once(&device);
            assert_eq!((reach, retries), (1, 1), "{words:?}");

            // Live: no Try again, nothing dimmed, and a phone is a phone.
            device.last_error = None;
            device.device_type = "phone".into();
            device.status = DeviceStatus::Paired;
            device.feature_transport_ready = true;
            device.last_seen = None;
            device.incoming_capabilities = vec![
                PACKET_TYPE_SHARE_REQUEST.into(),
                crate::device_links::packet::PACKET_TYPE_CLIPBOARD.into(),
            ];
            let (root, _, reach, retries) = once(&device);
            assert_eq!((reach, retries), (0, 0));
            assert!(words_and(&root, "dl-unavailable").1.is_empty());
            assert!(words_and(&root, "dl-stale").1.is_empty());
            let (_, art) = words_and(&root, "phone-art");
            assert!(!art[0].has_css_class("tablet"));
        });
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

#[cfg(test)]
mod activity_tests {
    use super::*;
    use crate::device_links::activity::snapshot_from;

    fn device_with(items: serde_json::Value, at: i64, now: i64) -> DeviceView {
        let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.feature_transport_ready = true;
        let mut body = serde_json::Map::new();
        body.insert("items".into(), items);
        body.insert("at".into(), serde_json::json!(at));
        device.activity = Some(snapshot_from(&body, now));
        device
    }

    fn labels(root: &gtk::Widget, prefix: &str) -> Vec<String> {
        let mut found: Vec<_> = named_widgets(root, prefix)
            .into_iter()
            .filter_map(|w| w.downcast::<gtk::Label>().ok())
            .map(|l| (l.widget_name().to_string(), l.label().to_string()))
            .collect();
        found.sort();
        found.into_iter().map(|(_, label)| label).collect()
    }

    /// The banners that say what is wrong with the connection wrap onto more
    /// lines, never end in an ellipsis: a long phone name must not hide the
    /// reason or the way out. (Needs a display.)
    #[test]
    fn the_connection_banners_wrap_instead_of_cutting_their_words() {
        crate::test_display::run(|| {
            let info = crate::device_links::device_info::DeviceInfo::local(
                "p".into(),
                "A phone with a name that goes on and on and on".into(),
            );
            let device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
            let banner = reconnecting_row(&device);
            let labels = named_widgets(banner.upcast_ref(), "")
                .into_iter()
                .filter_map(|w| w.downcast::<gtk::Label>().ok())
                .filter(|label| label.label().chars().count() > 12)
                .collect::<Vec<_>>();
            assert!(!labels.is_empty());
            for label in labels {
                assert!(label.wraps(), "{} wraps", label.label());
                assert_eq!(
                    label.ellipsize(),
                    gtk::pango::EllipsizeMode::None,
                    "{} is not cut",
                    label.label()
                );
            }
        });
    }

    /// The card names the phone, shows each item, and its countdown and bar
    /// move in place; nothing ongoing, or a hidden phone, draws no card.
    /// (Needs a display.)
    #[test]
    fn the_activity_card_shows_items_and_counts_down_in_place() {
        crate::test_display::run(|| {
            let at = crate::device_links::activity::now_ms();
            let now = at;
            let device = device_with(
                serde_json::json!([
                    {"id": "t", "appLabel": "Clock", "title": "Pasta", "kind": "timer",
                     "chronometerBase": at + 272_000, "countdown": true},
                    {"id": "d", "appLabel": "Files", "title": "Download", "kind": "progress",
                     "progress": 40}
                ]),
                at,
                now,
            );
            let settings = crate::device_links::config::AppSettings::default();
            let card = activity_card(&device, &settings).expect("a card");
            let root = card.clone().upcast::<gtk::Widget>();
            paint_activities(&root, &device, &settings, now);
            assert_eq!(labels(&root, "activity-live:"), ["4:32 left", "40%"]);
            paint_activities(&root, &device, &settings, now + 10_000);
            assert_eq!(labels(&root, "activity-live:")[0], "4:22 left", "in place");
            let bars = named_widgets(&root, "activity-bar:");
            assert_eq!(bars.len(), 1);
            let bar = bars[0].downcast_ref::<gtk::ProgressBar>().unwrap();
            assert!((bar.fraction() - 0.4).abs() < 1e-9);

            let mut hidden = settings.clone();
            hidden.hidden_notifications.insert("p".into());
            assert!(activity_card(&device, &hidden).is_none());
            let mut offline = device.clone();
            offline.feature_transport_ready = false;
            assert!(activity_card(&offline, &settings).is_none());
            let empty = device_with(serde_json::json!([]), at, now);
            assert!(activity_card(&empty, &settings).is_none());
        });
    }
}
