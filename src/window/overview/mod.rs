//! The Overview page: the phone in a hero band with its quick actions, what
//! it is doing here, and a card for each feature under "On this computer"
//! and "On <phone>"; last, every feature with where it is
//! ([`super::everything`]).
//!
//! Layout: this file builds Home (update, the clock, activities, the round
//! trip, refreshing in place, the Glide bar) and holds the wrap-row helpers;
//! `hero`, `tiles`, `links`, `media` and `cards` are its parts. No cycles.
//! May depend on: the `window` helpers and pages it links to, `ui_kit`,
//! `device_links` (`activity`, `config`, `daemon`, `device`, `features`,
//! `folders`, `glide`, `media_art`, `media_seek`, `packet`, `pty`, `webrtc`),
//! `terminal_previews`, `remote_control`, `link_glyph`.

use adw::prelude::*;
use adw::subclass::prelude::*;

use super::requirements::{blocker, Feature, Need, PhoneFacts};
use super::ui_kit::banner::{banner, BannerKind};
use super::ui_kit::{
    button, card as kit_card, icon_button as kit_icon_button, status_pill, tile, ButtonKind,
    TileKind, Tone,
};
use super::{
    can_send_feature, clock_time, device_type_label, ellipsized, named_widgets,
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

mod cards;
mod hero;
mod links;
mod media;
mod notes_card;
mod tiles;
use cards::*;
use hero::*;
use media::*;
use notes_card::notes_card;
use tiles::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use links::link_to_send;

// What the rest of the module's parent uses (everything else stays inside).
pub(super) use cards::preview_line;
pub(super) use links::show_send_link_dialog;
pub(super) use tiles::{hand_off, tile_columns};

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
        // The track's position moves in place, never by redrawing the page.
        media_status: device.media_status.clone().map(|media| {
            crate::device_links::device::MediaStatus {
                position: None,
                ..media
            }
        }),
        // Ongoing items are keyed by id below; their progress and the
        // clock offset of each snapshot change in place. Kept in the key,
        // every snapshot redrew Home under the pointer and ate the click.
        activity: None,
        ..device.clone()
    };
    // What is ongoing on the phone, by id: the card is redrawn when an item
    // comes or goes, and its live text changes in place.
    let activity_ids: Vec<String> = shown_activities(device, &settings)
        .into_iter()
        .map(|item| item.id.clone())
        .collect();
    let facts = super::requirements::Facts::now(Some(device));
    let needs = super::requirements::missing(&facts);
    // Every feature, at the end of the page: in the key by its states only.
    let everything = super::everything::rows(&super::everything::Facts::now(facts, device));
    let key = serde_json::to_string(&(
        super::requirements::key(&needs),
        super::everything::key(&everything),
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
        // The Notes card shows what is stored here (or sent by the phone).
        crate::device_links::notes::generation(),
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
        // Two columns on a big window (`home-split`, `apply_layout`): the
        // phone and what to do with it on the left, what needs a look and
        // what came in on the right; one column, in that reading order,
        // otherwise. "Everything DeskLink does" spans both, last.
        let left = column();
        let right = column();
        left.append(&hero(daemon, device, &minute));
        if let Some(card) = activity_card(device, &settings) {
            left.append(&card);
        }
        if let Some(bar) = now_playing_bar(daemon, device) {
            left.append(&bar);
        }
        // Away, nothing here can reach the phone: the hero says so once, with
        // Try again; the tiles (each "Can't reach"), the setup steps (they
        // need the phone; Settings lists them all the same) and the feature
        // list wait until it is back.
        let away = phone_away(device);
        // Can't reach is said once, in the hero, with its Try again.
        if device.status == DeviceStatus::Paired && !device.feature_transport_ready && !away {
            left.append(&reconnecting_row(device));
        }
        // The actions after the hero and what is going on now (activity, Now
        // playing, Connecting; UI-REDESIGN 3.11), before "Finish setting up":
        // that card, often several steps tall, hid them below the fold when it
        // came first.
        if !away {
            left.append(&tiles(daemon, device));
            if let Some(card) = super::requirements::card(daemon, &needs) {
                right.append(&card);
            }
        }
        // What came in sits under the actions: the two columns end near each
        // other instead of the left stopping after the tiles.
        left.append(&columns(daemon, device));
        right.append(&notes_card(win, device));
        right.append(&terminals_card(win, device));
        let split = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .homogeneous(false)
            .build();
        split.set_widget_name("home-split");
        split.append(&left);
        split.append(&right);
        page.append(&split);
        // Away, every feature would say "Can't reach" again (it is the one
        // thing to know, and the hero says it): the list waits too.
        if !away {
            page.append(&super::everything::section(daemon, device, &everything));
        }
    } else if device.status == DeviceStatus::PairRequestedByPeer {
        page.append(&super::pairing::pair_request_view(daemon, device));
    } else if device.status == DeviceStatus::PairRequested {
        page.append(&super::pairing::pair_waiting_view(daemon, device));
    } else {
        page.append(&super::pairing::pair_idle_view(daemon, device));
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(HOME_WIDTH)
        .tightening_threshold(800)
        .child(&page)
        .build();
    // Wider on a big window, for two columns (`apply_layout`).
    clamp.set_widget_name("home-clamp");
    win.replace_body("home", &clamp);
    refresh_in_place(win, daemon);
    show_round_trip(win, device);
    show_clock(win, &minute);
}

/// Home's width in one column, and in two on a big window (each column then
/// about as wide as the one column: the extra width is structure, not longer
/// lines).
pub(in crate::window) const HOME_WIDTH: i32 = 944;
pub(in crate::window) const HOME_WIDE_WIDTH: i32 = 1640;

/// A column of Home's cards.
fn column() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .hexpand(true)
        .valign(gtk::Align::Start)
        .build()
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

/// "charging", with the time to full when the phone knows it.
fn charging_words(remaining_ms: Option<i64>) -> String {
    match remaining_ms.map(|ms| (ms + 59_999) / 60_000) {
        Some(minutes) if minutes > 0 && minutes < 60 => {
            format!("charging, full in {minutes} min")
        }
        Some(minutes) if minutes >= 60 => {
            format!("charging, full in {} h {} min", minutes / 60, minutes % 60)
        }
        _ => "charging".to_string(),
    }
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
    for widget in named_widgets(&root, "media-seek:") {
        let id = widget
            .widget_name()
            .trim_start_matches("media-seek:")
            .to_string();
        let Some(media) = daemon
            .devices()
            .into_iter()
            .find(|device| device.id == id)
            .and_then(|device| device.media_status)
        else {
            continue;
        };
        let at = shown_position(&media, std::time::Instant::now());
        // Not under the finger.
        if let Some(scale) = widget
            .downcast_ref::<gtk::Scale>()
            .filter(|scale| !scale.has_css_class("seeking"))
        {
            if (scale.value() - at as f64).abs() > 500.0 {
                scale.set_value(at as f64);
            }
            for label in named_widgets(&root, &format!("media-pos:{id}")) {
                if let Some(label) = label.downcast_ref::<gtk::Label>() {
                    label.set_label(&clock(at));
                }
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
mod activity_tests;
#[cfg(test)]
mod tests;
