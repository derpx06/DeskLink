//! Home's Notes card: the newest notes from the Notes page, New note, and a
//! one-line entry that saves a note to the same store.
//!
//! Notes are desk data: the card reads them with the phone away too. Only the
//! quick-add entry writes, and only when the person presses Enter.

use std::cell::RefCell;

use super::cards::{feature_card, page_button};
use super::*;
use crate::device_links::notes;

/// How many notes the card lists.
const SHOWN: usize = 3;

thread_local! {
    /// What is typed in the entry: kept when Home is redrawn (a note arriving
    /// from the phone) so typing is never lost.
    static DRAFT: RefCell<String> = const { RefCell::new(String::new()) };
}

/// "No notes yet", "1 note", "5 notes".
pub(super) fn notes_words(count: usize) -> String {
    match count {
        0 => "No notes yet".to_string(),
        1 => "1 note".to_string(),
        n => format!("{n} notes"),
    }
}

/// Saves `text` as a new note (the same store and sync as the Notes page);
/// false when the store is full.
pub(super) fn quick_add(text: &str) -> bool {
    notes::new_note(text.trim()).is_some()
}

pub(super) fn notes_card(win: &DesklinkWindow, device: &DeviceView) -> gtk::Box {
    let all = notes::notes();
    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    if all.is_empty() {
        body.append(&wrapped(
            "Type one below, or start a new note.",
            &["dl-secondary"],
        ));
    }
    for note in all.iter().take(SHOWN) {
        let words = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .build();
        let title = ellipsized(&note.label(), &["heading"]);
        title.set_xalign(0.0);
        words.append(&title);
        let preview = crate::window::notes::preview_of(note);
        let age = crate::window::notes::modified_words(note.modified);
        let detail = ellipsized(
            &if preview.is_empty() {
                age
            } else {
                format!("{age} · {preview}")
            },
            &["dl-secondary"],
        );
        detail.set_xalign(0.0);
        words.append(&detail);
        let open = gtk::Button::builder()
            .child(&words)
            .css_classes(["home-note"])
            .build();
        open.update_property(&[gtk::accessible::Property::Label(&format!(
            "Open the note {}",
            note.label()
        ))]);
        let (window, id) = (win.downgrade(), note.id.clone());
        open.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.open_note(&id);
            }
        });
        body.append(&open);
    }
    let entry = gtk::Entry::builder()
        .placeholder_text("Type a note and press Enter")
        .hexpand(true)
        .build();
    entry.set_widget_name("home-note-entry");
    entry.update_property(&[gtk::accessible::Property::Label("Quick note")]);
    entry.set_text(&DRAFT.with(|draft| draft.borrow().clone()));
    entry.connect_changed(|entry| {
        DRAFT.with(|draft| *draft.borrow_mut() = entry.text().to_string())
    });
    entry.connect_activate(|entry| {
        let text = entry.text();
        if text.trim().is_empty() {
            return;
        }
        if quick_add(&text) {
            // The store changed: Home is redrawn from it, the entry empty.
            DRAFT.with(|draft| draft.borrow_mut().clear());
            entry.set_text("");
            toast_from(entry, "Note saved");
        } else {
            toast_from(entry, "Too many notes (2000 at most)");
        }
    });
    body.append(&entry);
    if phone_away(device) {
        body.append(&wrapped(
            &format!("Notes sync with {} when it is back.", device.name),
            &["dl-secondary"],
        ));
    }
    let new = button(ButtonKind::Tonal, "New note", Some("desklink-add-symbolic"));
    new.set_valign(gtk::Align::Center);
    let window = win.downgrade();
    new.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            window.new_note();
        }
    });
    let end = gtk::Box::builder().spacing(8).build();
    end.append(page_button("View all", "notes").upcast_ref::<gtk::Widget>());
    end.append(&new);
    let card = feature_card(
        "Notes",
        &notes_words(all.len()),
        Some(end.upcast_ref()),
        &body,
        false,
    );
    card.set_widget_name("home-notes");
    card
}
