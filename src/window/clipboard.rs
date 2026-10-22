//! The Clipboard page: sharing on or off, and the last 20 items shared,
//! each with Copy. The history is held in memory only and never lists an
//! item a password manager marked as a secret.

use adw::prelude::*;

use super::overview::{clipboard_switch, preview_line};
use super::{card, ellipsized, icon_square, text, toast_from, DesklinkWindow};
use crate::device_links::device::DeviceView;

pub(super) fn update(win: &DesklinkWindow, device: &DeviceView) {
    let Some(daemon) = win.daemon() else {
        return;
    };
    let on = crate::device_links::config::app_settings().clipboard_sync;
    let items = crate::device_links::daemon::clipboard::history();
    let minute = super::clock_time(gtk::glib::DateTime::now_local().ok());
    let key = serde_json::to_string(&(
        on,
        crate::device_links::daemon::clipboard::password_detection(),
        &device.name,
        items
            .iter()
            .map(|item| (&item.text, &item.from, item.at))
            .collect::<Vec<_>>(),
        minute,
        crate::device_links::notes::generation(),
    ))
    .unwrap_or_default();
    if !win.page_changed("clipboard", key) {
        return;
    }

    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(8)
        .margin_bottom(28)
        .margin_start(28)
        .margin_end(28)
        .build();
    let top = card(0);
    top.set_orientation(gtk::Orientation::Horizontal);
    top.set_spacing(14);
    top.add_css_class("padded-card");
    top.append(&icon_square("edit-paste-symbolic", 36, "feature-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(
        &format!("Share clipboard with {}", device.name),
        &["card-title"],
    ));
    let note = text(
        match crate::device_links::daemon::clipboard::password_detection() {
            Some(true) => "Copy on one device, paste on the other. Passwords are never shared.",
            Some(false) => {
                "Copy on one device, paste on the other. Password detection isn't available in \
                 this session, so copied passwords are shared too."
            }
            None => "Copy on one device, paste on the other.",
        },
        &["card-subtitle"],
    );
    note.set_wrap(true);
    words.append(&note);
    top.append(&words);
    top.append(&clipboard_switch(&daemon, on));
    page.append(&top);

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list", "history-list"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Clipboard history")]);
    let heading = text("History", &["list-heading"]);
    let rows = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    rows.append(&heading);
    rows.append(&list);
    if items.is_empty() {
        let empty = text(
            if on {
                "Text you copy here or on the phone appears here."
            } else {
                "Sharing is off. Nothing is copied between the devices."
            },
            &["text-faint", "body-small", "padded-row"],
        );
        empty.set_wrap(true);
        list.append(
            &gtk::ListBoxRow::builder()
                .activatable(false)
                .child(&empty)
                .build(),
        );
    }
    let now = gtk::glib::DateTime::now_local().ok();
    for item in items {
        let row = gtk::Box::builder()
            .spacing(12)
            .css_classes(["padded-row"])
            .build();
        row.append(
            &gtk::Image::builder()
                .icon_name(if item.from.is_some() {
                    "phone-symbolic"
                } else {
                    "computer-symbolic"
                })
                .css_classes(["text-dim"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        let words = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        words.append(&ellipsized(
            &preview_line(&item.text),
            &["mono", "body-small"],
        ));
        let when = gtk::glib::DateTime::from_unix_local(item.at as i64)
            .ok()
            .zip(now.as_ref())
            .map(|(when, now)| super::history::time_label(now, &when))
            .unwrap_or_default();
        let from = item
            .from
            .clone()
            .unwrap_or_else(|| "this computer".to_string());
        words.append(&ellipsized(
            &format!("From {from} · {when}"),
            &["caption", "text-faint"],
        ));
        row.append(&words);
        let copy = super::soft_button("Copy");
        copy.add_css_class("small");
        copy.update_property(&[gtk::accessible::Property::Label(&format!(
            "Copy {}",
            preview_line(&item.text)
        ))]);
        let value = item.text.clone();
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&value);
            toast_from(button, "Copied");
        });
        // F6: keep it as a snippet, here and on the phone.
        let pin = gtk::Button::builder()
            .icon_name("view-pin-symbolic")
            .tooltip_text("Pin as a Snippet")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        pin.update_property(&[gtk::accessible::Property::Label(&format!(
            "Pin {} as a snippet",
            preview_line(&item.text)
        ))]);
        let value = item.text.clone();
        pin.connect_clicked(
            move |button| match crate::device_links::notes::add_snippet(&value) {
                Ok(()) => toast_from(button, "Pinned to Snippets"),
                Err(error) => toast_from(button, &error),
            },
        );
        row.append(&pin);
        row.append(&copy);
        list.append(
            &gtk::ListBoxRow::builder()
                .activatable(false)
                .child(&row)
                .build(),
        );
    }
    page.append(&snippets_section());
    page.append(&rows);
    win.replace_body(
        "clipboard",
        &adw::Clamp::builder().maximum_size(760).child(&page).build(),
    );
}

/// Snippets (F6): texts kept to paste again, synced with the phone. Copy on
/// click; remove with the cross.
pub(super) fn snippets_section() -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    section.append(&text("Snippets", &["list-heading"]));
    // Typed in, with or without a phone.
    let add = adw::EntryRow::builder()
        .title("Add a snippet")
        .show_apply_button(true)
        .build();
    add.connect_apply(
        |row| match crate::device_links::notes::add_snippet(&row.text()) {
            Ok(()) => {
                row.set_text("");
                toast_from(row, "Snippet added");
            }
            Err(error) => toast_from(row, &error),
        },
    );
    let adding = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    adding.append(&add);
    section.append(&adding);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list", "history-list"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Snippets")]);
    let snippets = crate::device_links::notes::snippets();
    let mut texts = Vec::new();
    if snippets.is_empty() {
        let empty = text(
            "Add text above, or pin it from the clipboard history, to keep it here and on \
             your phone.",
            &["text-faint", "body-small", "padded-row"],
        );
        empty.set_wrap(true);
        list.append(
            &gtk::ListBoxRow::builder()
                .activatable(false)
                .child(&empty)
                .build(),
        );
    }
    for snippet in snippets {
        let row = gtk::Box::builder()
            .spacing(12)
            .css_classes(["padded-row"])
            .build();
        row.append(
            &gtk::Image::builder()
                .icon_name("view-pin-symbolic")
                .css_classes(["text-dim"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        let words = ellipsized(&snippet.label(), &["mono", "body-small"]);
        words.set_hexpand(true);
        row.append(&words);
        let remove = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text("Remove Snippet")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        remove.update_property(&[gtk::accessible::Property::Label(&format!(
            "Remove snippet {}",
            snippet.label()
        ))]);
        let id = snippet.id.clone();
        remove.connect_clicked(move |button| {
            crate::device_links::notes::delete_snippet(&id);
            toast_from(button, "Snippet removed");
        });
        row.append(&remove);
        let item = gtk::ListBoxRow::builder()
            .activatable(true)
            .child(&row)
            .tooltip_text("Copy")
            .build();
        item.update_property(&[gtk::accessible::Property::Label(&format!(
            "Copy {}",
            snippet.label()
        ))]);
        texts.push(snippet.text.clone());
        list.append(&item);
    }
    list.connect_row_activated(move |_, row| {
        if let Some(value) = usize::try_from(row.index()).ok().and_then(|i| texts.get(i)) {
            row.clipboard().set_text(value);
            toast_from(row, "Copied");
        }
    });
    section.append(&list);
    section
}
