//! The Bucket page (F8b): one shared space per phone. Any file put in it —
//! dropped, pasted, sent from Files, saved into the folder — shows up on the
//! phone too. A thumbnail grid, newest first, each with its state and menu.
//! Files only for now: folders are refused, out loud.
//!
//! Layout: this file builds the page (update, the off and waiting cards, the
//! filled page, rubber-band selection, the Home card); `browse` keeps where
//! the page is and what is selected, `live` the progress that updates in
//! place, `toolbar` the bars, header and banners, `tiles` the grid, and
//! `adding` choosing, dropping and pasting. No cycles between the parts.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use super::ui_kit::{self, ButtonKind, Status, Tone};
use super::{ellipsized, icon_square, text, toast_from, DesklinkWindow};
use crate::device_links::device::DeviceView;
use crate::device_links::folders::{self, Item, ItemState, PairView, Phase, Side, BUCKET_ID};
use crate::device_links::transfer_log::LoggedFile;

mod adding;
mod browse;
mod live;
mod tiles;
mod toolbar;
use adding::*;
use browse::*;
use live::*;
use tiles::*;
use toolbar::*;

// What the rest of the window uses (everything else stays inside).
#[cfg(debug_assertions)]
pub(super) use browse::{debug_open, debug_select};

/// This phone's Bucket, if it is on (or being turned on).
pub(super) fn bucket_of(device: &str) -> Option<PairView> {
    folders::pairs()
        .into_iter()
        .find(|view| view.pair.device == device && view.pair.id == BUCKET_ID && view.pair.bucket)
}

pub(super) fn update(win: &DesklinkWindow, device: &DeviceView) {
    install_paste(win);
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        if browse.device != device.id {
            *browse = Browse {
                device: device.id.clone(),
                ..Browse::default()
            };
        }
    });
    let photos = crate::device_links::config::app_settings().accepts_gallery_from(&device.id);
    let key = format!(
        "{}\n{}\n{:?}\n{}\n{}\n{}",
        current(),
        folders::generation(),
        (photos, crate::device_links::gallery::generation()),
        device.id,
        device.name,
        device.feature_transport_ready,
    );
    if !win.page_changed("bucket", key) {
        return;
    }
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(8)
        .margin_bottom(28)
        .margin_start(16)
        .margin_end(16)
        .build();
    // W12: the phone's own screenshots and photos, above the Bucket.
    if let Some(strip) = super::gallery::strip(device) {
        page.append(&strip);
    }
    let offer = folders::offers()
        .into_iter()
        .find(|offer| offer.device == device.id && offer.bucket);
    match bucket_of(&device.id) {
        Some(view) if view.phase == Phase::Offered => {
            page.append(&waiting_card(&view, device));
            page.append(&empty_zone(device));
            accept_drops(&page, &device.id, Some(""));
        }
        Some(view) => fill(win, &page, &view, device),
        None => page.append(&off_card(device, offer.is_some())),
    }
    let clamp = adw::Clamp::builder()
        .maximum_size(super::FILES_WIDTH)
        .child(&page)
        .build();
    // Wider on a big window, like the transfers under it (`apply_layout`).
    clamp.set_widget_name("files-clamp");
    win.replace_body("bucket", &clamp);
}

/// Whether `device_id`'s phone has asked for a Bucket that is not on yet: the
/// question "Turn on the Bucket?" waits for an answer.
pub(super) fn asked(device_id: &str) -> bool {
    bucket_of(device_id).is_none()
        && folders::offers()
            .iter()
            .any(|offer| offer.device == device_id && offer.bucket)
}

/// "A folder that is the same on this computer and Pixel 9": the Bucket's
/// one line (`UI-REDESIGN.md` 5.2).
fn one_liner(device: &str) -> String {
    format!("A folder that is the same on this computer and {device}.")
}

/// Before the Bucket is on: what it is, one button.
fn off_card(device: &DeviceView, asked: bool) -> gtk::Box {
    let card = ui_kit::card(None);
    card.add_css_class("files-bucket-card");
    let head = gtk::Box::builder().spacing(12).build();
    head.append(&ui_kit::icon_square(
        "desklink-inventory-symbolic",
        Tone::Accent,
    ));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let title = super::wrapped(
        if asked {
            "Turn on the Bucket?"
        } else {
            "Bucket"
        },
        &["files-bucket-title"],
    );
    title.set_accessible_role(gtk::AccessibleRole::Heading);
    words.append(&title);
    words.append(&super::wrapped(
        &if asked {
            format!(
                "{} asked for a Bucket with this computer: put anything in it, here or there, \
                 and it shows up on both. Remove it from one and it goes from both.",
                device.name
            )
        } else {
            one_liner(&device.name)
        },
        &["dl-secondary"],
    ));
    words.append(&super::wrapped(
        "It is “DeskLink Bucket” in your Home folder and the Files sidebar.",
        &["dl-caption"],
    ));
    head.append(&words);
    card.append(&head);
    let buttons = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(false)
        .max_children_per_line(2)
        .column_spacing(8)
        .row_spacing(8)
        .halign(gtk::Align::End)
        .build();
    if asked {
        let later = ui_kit::button(ButtonKind::Text, "Not now", None);
        let id = device.id.clone();
        later.connect_clicked(move |_| folders::decline(&id, BUCKET_ID));
        super::transfers::add_card(&buttons, &later);
    }
    let on = ui_kit::button(
        ButtonKind::Primary,
        "Turn on the Bucket",
        Some("desklink-directory-sync-symbolic"),
    );
    let id = device.id.clone();
    on.connect_clicked(move |_| folders::turn_on_bucket(&id));
    super::transfers::add_card(&buttons, &on);
    card.append(&buttons);
    let dropped = folders::dropped_pairs();
    if dropped > 0 {
        card.append(&super::wrapped(
            &format!(
                "The Bucket replaces folder pairs. {} you set up before stopped syncing; \
                 their files are still where they were.",
                if dropped == 1 {
                    "The folder pair".to_string()
                } else {
                    format!("The {dropped} folder pairs")
                }
            ),
            &["dl-secondary"],
        ));
    }
    card
}

/// Turned on here; the phone has to pick its side.
fn waiting_card(view: &PairView, device: &DeviceView) -> gtk::Box {
    let bar = gtk::Box::builder()
        .spacing(14)
        .css_classes(["state-banner", "files-banner", "tall"])
        .build();
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .valign(gtk::Align::Center)
        .build();
    bar.append(&spinner);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(
        &format!("Waiting for {}", device.name),
        &["files-banner-title"],
    ));
    let detail = text(
        "Open DeskLink on the phone and choose where its Bucket goes. You can add things here \
         already.",
        &["dl-secondary"],
    );
    detail.set_wrap(true);
    words.append(&detail);
    bar.append(&words);
    let cancel = ui_kit::button(ButtonKind::Text, "Cancel", None);
    cancel.set_valign(gtk::Align::Center);
    let (id, pair) = (view.pair.device.clone(), view.pair.id.clone());
    cancel.connect_clicked(move |_| folders::remove(&id, &pair));
    bar.append(&cancel);
    bar
}

/// "Last synced 2 min ago".
fn synced_words(at: Option<i64>) -> Option<String> {
    let at = glib::DateTime::from_unix_local(at? / 1000).ok()?;
    let now = glib::DateTime::now_local().ok()?;
    let minutes = now.difference(&at).as_minutes();
    Some(match minutes {
        ..=0 => "Last synced just now".to_string(),
        1 => "Last synced 1 min ago".to_string(),
        2..=59 => format!("Last synced {minutes} min ago"),
        _ => format!(
            "Last synced {}",
            super::history::time_label(&now, &at).to_lowercase()
        ),
    })
}

fn fill(win: &DesklinkWindow, page: &gtk::Box, view: &PairView, device: &DeviceView) {
    // A folder that is gone (removed on the phone): back to the top.
    let here = current();
    if !here.is_empty() && !view.items.iter().any(|item| item.dir && item.path == here) {
        BROWSE.with(|browse| browse.borrow_mut().current.clear());
    }
    let here = current();
    // This folder's items, folders first.
    let mut level: Vec<Item> = view
        .items
        .iter()
        .filter(|item| folders::parent_of(&item.path) == here)
        .cloned()
        .collect();
    level.sort_by_key(|item| !item.dir);
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.show_level(level.iter().map(|item| item.path.as_str()));
    });
    page.append(&header(win, view, device));
    page.append(&selection_bar(win, view, device));
    if let Some(banner) = problem_banner(view) {
        page.append(&banner);
    }
    if level.is_empty() && here.is_empty() {
        page.append(&empty_zone(device));
    } else if level.is_empty() {
        let hint = text(
            "This folder is empty. Drop files here to add them.",
            &["files-bucket-title"],
        );
        hint.set_margin_top(40);
        hint.set_margin_bottom(40);
        hint.set_halign(gtk::Align::Center);
        hint.set_wrap(true);
        hint.set_justify(gtk::Justification::Center);
        page.append(&hint);
    } else {
        page.append(&grid(win, &level, view, device));
        if let Some(note) = more_note(level.len()) {
            let note = text(&note, &["dl-secondary"]);
            note.set_wrap(true);
            note.set_halign(gtk::Align::Center);
            page.append(&note);
        }
        let hint = text(
            "Drop files or folders anywhere here, or press Ctrl+V to paste.",
            &["dl-caption"],
        );
        hint.set_halign(gtk::Align::Center);
        hint.set_wrap(true);
        hint.set_justify(gtk::Justification::Center);
        page.append(&hint);
    }
    if !view.skipped.is_empty() {
        page.append(&not_synced(&view.skipped));
    }
    accept_drops(page, &device.id, None);
    rubber_band(page);
    refresh_selection();
}

/// Dragging over empty space selects what the rectangle touches.
fn rubber_band(page: &gtk::Box) {
    let drag = gtk::GestureDrag::new();
    let active = Rc::new(std::cell::Cell::new(false));
    {
        let (active, page) = (Rc::clone(&active), page.clone());
        drag.connect_drag_begin(move |_, x, y| {
            // Only from empty space: a tile or a button has its own click.
            let mut hit = page.pick(x, y, gtk::PickFlags::DEFAULT);
            let mut on_item = false;
            while let Some(widget) = hit {
                if widget.is::<gtk::Button>()
                    || widget.is::<gtk::MenuButton>()
                    || widget.is::<gtk::ToggleButton>()
                    || widget.is::<gtk::Entry>()
                {
                    on_item = true;
                    break;
                }
                hit = widget.parent();
            }
            active.set(!on_item);
        });
    }
    {
        let (active, page) = (Rc::clone(&active), page.clone());
        drag.connect_drag_update(move |gesture, dx, dy| {
            if !active.get() || (dx.abs() < 6.0 && dy.abs() < 6.0) {
                return;
            }
            let Some((x, y)) = gesture.start_point() else {
                return;
            };
            let band = gtk::graphene::Rect::new(
                x.min(x + dx) as f32,
                y.min(y + dy) as f32,
                dx.abs() as f32,
                dy.abs() as f32,
            );
            let ctrl = gesture
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            let hits: Vec<String> = BROWSE.with(|browse| {
                browse
                    .borrow()
                    .tiles
                    .iter()
                    .filter(|tile| {
                        tile.button
                            .upgrade()
                            .and_then(|button| button.compute_bounds(&page))
                            .is_some_and(|bounds| bounds.intersection(&band).is_some())
                    })
                    .map(|tile| tile.path.clone())
                    .collect()
            });
            BROWSE.with(|browse| {
                let mut browse = browse.borrow_mut();
                browse.selecting = true;
                if !ctrl {
                    browse.selected.clear();
                }
                browse.selected.extend(hits);
            });
            refresh_selection();
        });
    }
    page.add_controller(drag);
}

/// The Overview's Bucket card: the last 3 items and a drop target.
pub(super) fn overview_card(device: &DeviceView) -> gtk::Box {
    let view = bucket_of(&device.id).filter(|view| view.phase != Phase::Offered);
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .css_classes(["dl-card", "feature-card"])
        .build();
    let head = gtk::Box::builder().spacing(14).build();
    head.append(&icon_square("folder-symbolic", 36, "feature-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized("Files", &["card-title"]));
    let subtitle = match &view {
        Some(view) => format!(
            "{} · {}",
            match view.files {
                1 => "1 item".to_string(),
                count => format!("{count} items"),
            },
            crate::bucket_progress::size_words(view.bytes)
        ),
        None if asked(&device.id) => format!("{} asked for a Bucket. Turn it on?", device.name),
        None => format!("Off · share anything with {}", device.name),
    };
    let line = ellipsized(&subtitle, &["card-subtitle", "numeric"]);
    words.append(&line);
    head.append(&words);
    let bar = thin_bar("Bucket sync progress");
    bar.set_visible(false);
    watch(Live::Summary {
        device: device.id.clone(),
        idle: subtitle,
        idle_now: None,
        line: line.downgrade(),
        bar: bar.downgrade(),
        speed: None,
    });
    // The phone asked: the question is answered right here.
    let asking = view.is_none() && asked(&device.id);
    if asking {
        let later = ui_kit::button(ButtonKind::Text, "Not now", None);
        later.set_valign(gtk::Align::Center);
        let id = device.id.clone();
        later.connect_clicked(move |_| folders::decline(&id, BUCKET_ID));
        head.append(&later);
        let on = ui_kit::button(ButtonKind::Primary, "Turn on", None);
        on.set_valign(gtk::Align::Center);
        let id = device.id.clone();
        on.connect_clicked(move |_| folders::turn_on_bucket(&id));
        head.append(&on);
    } else {
        head.append(&super::page_link(
            if view.is_some() { "Open" } else { "Set up" },
            "files",
        ));
    }
    card.append(&head);
    card.append(&bar);
    let Some(view) = view else {
        return card;
    };
    let mut recent: Vec<&Item> = view
        .items
        .iter()
        .filter(|item| !item.path.contains('/'))
        .collect();
    recent.sort_by_key(|item| std::cmp::Reverse(item.mtime));
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["bucket-recent"])
        .build();
    if recent.is_empty() {
        let hint = text(
            "Drop files here to put them in the Bucket",
            &["caption", "text-dim"],
        );
        hint.set_halign(gtk::Align::Center);
        hint.set_margin_top(10);
        hint.set_margin_bottom(10);
        list.append(&hint);
    }
    for item in recent.into_iter().take(3) {
        let row = gtk::Box::builder().spacing(10).build();
        row.append(
            &gtk::Image::builder()
                .gicon(&gio::content_type_get_symbolic_icon(
                    &gio::content_type_guess(Some(&item.path), &[]).0,
                ))
                .pixel_size(16)
                .css_classes(["text-dim"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        let name = ellipsized(&item.path, &[]);
        name.set_hexpand(true);
        row.append(&name);
        row.append(&ellipsized(
            &from_words(item, device),
            &["caption", "text-faint"],
        ));
        list.append(&row);
    }
    card.append(&list);
    accept_drops(&card, &device.id, Some(""));
    card
}

fn menu_item(label: &str) -> gtk::Button {
    let button = ui_kit::button(ButtonKind::Text, label, None);
    if let Some(label) = button.child().and_downcast::<gtk::Label>() {
        label.set_xalign(0.0);
    }
    button
}

/// A menu item that removes or turns something off: the error colours.
fn mark_danger(button: &gtk::Button) {
    button.remove_css_class("dl-btn-text");
    button.add_css_class("dl-btn-danger");
}

#[cfg(test)]
mod tests;
