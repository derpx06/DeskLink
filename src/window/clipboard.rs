//! The Clipboard history tab of Notes & Clipboard: the last 20 items shared,
//! each with Copy and Pin as Snippet; and the Snippets tab's list. The
//! history is held in memory only and never lists an item a password manager
//! marked as a secret. The sharing switch is in the page header
//! (`notes_clipboard`).

use adw::prelude::*;

use super::overview::preview_line;
use super::{ellipsized, text, toast_from};

/// What the history tab was drawn from: it is redrawn only when this changes.
pub(super) fn history_key(on: bool) -> String {
    serde_json::to_string(&(
        on,
        crate::device_links::daemon::clipboard::password_detection(),
        crate::device_links::daemon::clipboard::history()
            .iter()
            .map(|item| (item.text.clone(), item.from.clone(), item.at))
            .collect::<Vec<_>>(),
        super::clock_time(gtk::glib::DateTime::now_local().ok()),
    ))
    .unwrap_or_default()
}

/// The clipboard history: a line about passwords, then the list.
pub(super) fn history_box(on: bool) -> gtk::Box {
    let items = crate::device_links::daemon::clipboard::history();
    let rows = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .build();
    let note = text(
        match crate::device_links::daemon::clipboard::password_detection() {
            Some(true) => "Copy on one device, paste on the other. Passwords are never shared.",
            Some(false) => {
                "Copy on one device, paste on the other. Password detection isn't available in \
                 this session, so copied passwords are shared too."
            }
            None => "Copy on one device, paste on the other.",
        },
        &["text-dim", "body-small"],
    );
    note.set_wrap(true);
    note.set_xalign(0.0);
    rows.append(&note);
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list", "history-list"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Clipboard history")]);
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
    rows
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
