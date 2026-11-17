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
    /// Where the cursor was before Home was rebuilt, until the person types
    /// or leaves the entry: whichever of the focus change and the restore
    /// comes last, the cursor ends up here (and nothing is selected).
    /// One for the process: DeskLink has one window, so one quick entry.
    static PENDING_CURSOR: std::cell::Cell<Option<i32>> = const { std::cell::Cell::new(None) };
    /// What is typed in the entry: kept when Home is redrawn (a note arriving
    /// from the phone) so typing is never lost. One draft for every phone:
    /// the entry is the computer's, not a phone's.
    static DRAFT: RefCell<String> = const { RefCell::new(String::new()) };
}

fn apply_pending_cursor(entry: &gtk::Entry) {
    if let Some(position) = PENDING_CURSOR.with(|pending| pending.get()) {
        entry.select_region(position, position);
    }
}

/// Gives the focus to `entry`, a rebuilt copy of the entry that had it with
/// its cursor at `position`: the cursor stays there however the focus-in
/// (which selects all) and this restore are ordered.
pub(in crate::window) fn restore_cursor(entry: &gtk::Entry, position: i32) {
    PENDING_CURSOR.with(|pending| pending.set(Some(position)));
    entry.grab_focus();
    apply_pending_cursor(entry);
    let entry = entry.clone();
    gtk::glib::idle_add_local_once(move || {
        apply_pending_cursor(&entry);
        // The focus did not land (the entry is not mapped, the page changed)
        // while the window is active: nothing late is coming, so no stale
        // position waits for a click or Tab hours from now.
        let in_entry = entry.root().is_some_and(|root| {
            gtk::prelude::RootExt::focus(&root).is_some_and(|focus| focus.is_ancestor(&entry))
        });
        let active = entry
            .root()
            .and_downcast::<gtk::Window>()
            .is_some_and(|window| window.is_active());
        if !in_entry && active {
            PENDING_CURSOR.with(|pending| pending.set(None));
        }
    });
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
    // A redraw gives the focus back with `grab_focus`, which selects all of
    // an entry's text: the next key would replace the draft. The cursor is
    // put back where it was (`restore_cursor`), whenever the focus change
    // is actually done: at once, or late (a window that was not active).
    // (A GtkEntry's focus is its inner GtkText's.)
    if let Some(inner) = entry.delegate() {
        inner.connect_notify_local(Some("has-focus"), {
            // Weak: the entry owns its text, which owns this closure.
            let entry = entry.downgrade();
            move |inner, _| {
                if inner.has_focus() {
                    if let Some(entry) = entry.upgrade() {
                        gtk::glib::idle_add_local_once(move || apply_pending_cursor(&entry));
                    }
                } else {
                    // Out of the entry: nothing is owed to it any more.
                    PENDING_CURSOR.with(|pending| pending.set(None));
                }
            }
        });
    }
    // The person's own keys end it: from the first one the cursor is theirs.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(|_, key, _, _| {
        use gtk::gdk::Key;
        // Shift, Ctrl, Alt and the like are not typing.
        let modifier = matches!(
            key,
            Key::Shift_L
                | Key::Shift_R
                | Key::Control_L
                | Key::Control_R
                | Key::Alt_L
                | Key::Alt_R
                | Key::Meta_L
                | Key::Meta_R
                | Key::Super_L
                | Key::Super_R
                | Key::ISO_Level3_Shift
                | Key::Caps_Lock
                | Key::Num_Lock
        );
        if !modifier {
            PENDING_CURSOR.with(|pending| pending.set(None));
        }
        gtk::glib::Propagation::Proceed
    });
    entry.add_controller(keys);
    // Off the screen (another page): no position is owed to it.
    entry.connect_unmap(|_| PENDING_CURSOR.with(|pending| pending.set(None)));
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
    card.add_css_class("home-card-notes");
    card
}
