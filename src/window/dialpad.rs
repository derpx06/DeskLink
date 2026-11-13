//! The dial pad: a number field, the 12 keys, contact suggestions and the
//! numbers dialled from here lately. Call hands the number to the one
//! confirmation every dial goes through (emergency refusal, premium
//! warning, digits only), so nothing here places a call.

use std::rc::Rc;

use adw::prelude::*;

use super::ui_kit::{self, ButtonKind};
use super::DesklinkWindow;
use crate::device_links::contacts::Contact;

/// The longest number the field takes (`calls::dial_number` allows 24 digits).
const MAX_CHARS: usize = 32;
const KEYS: [&str; 12] = ["1", "2", "3", "4", "5", "6", "7", "8", "9", "*", "0", "#"];

/// `text` after pressing `key`: `*` and `#` are never added (the phone
/// would run them as codes), a `+` only first.
pub(crate) fn press(text: &str, key: &str) -> String {
    let allowed = match key {
        "+" => text.is_empty(),
        _ => key.len() == 1 && key.chars().all(|c| c.is_ascii_digit()),
    };
    if !allowed || text.chars().count() >= MAX_CHARS {
        return text.to_string();
    }
    format!("{text}{key}")
}

/// Contacts whose name or number holds `query`: up to `limit` (name, number).
pub(crate) fn suggestions(
    contacts: &[Contact],
    query: &str,
    limit: usize,
) -> Vec<(String, String)> {
    let query = query.trim().to_lowercase();
    let digits: String = query.chars().filter(char::is_ascii_digit).collect();
    if query.is_empty() {
        return Vec::new();
    }
    contacts
        .iter()
        .filter_map(|contact| {
            let by_name = contact.name.to_lowercase().contains(&query);
            contact
                .phones
                .iter()
                .find(|phone| {
                    by_name
                        || (!digits.is_empty()
                            && phone
                                .number
                                .chars()
                                .filter(char::is_ascii_digit)
                                .collect::<String>()
                                .contains(&digits))
                })
                .map(|phone| (contact.name.clone(), phone.number.clone()))
        })
        .take(limit)
        .collect()
}

/// The pad's widgets, for the dialog and for tests.
#[cfg_attr(not(test), allow(dead_code))] // keys and call are read by the tests
pub(crate) struct Pad {
    pub(crate) root: gtk::Box,
    pub(crate) entry: gtk::Entry,
    pub(crate) keys: Vec<gtk::Button>,
    pub(crate) call: gtk::Button,
}

/// Builds the pad. `on_call` gets the typed number when Call (or Enter) is
/// used; it is not called for an empty field.
pub(crate) fn pad(
    contacts: Vec<Contact>,
    recents: Vec<String>,
    on_call: impl Fn(String) + 'static,
    on_clear: impl Fn() + 'static,
) -> Pad {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let entry = gtk::Entry::builder()
        .placeholder_text("Number or contact name")
        .input_purpose(gtk::InputPurpose::Phone)
        .max_length(MAX_CHARS as i32)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Number or contact name")]);
    root.append(&entry);

    let suggested = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    root.append(&suggested);

    let on_call = Rc::new(on_call);
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(8)
        .halign(gtk::Align::Center)
        .build();
    let mut keys = Vec::new();
    for (index, key) in KEYS.iter().enumerate() {
        let button = gtk::Button::builder()
            .label(*key)
            .width_request(64)
            .height_request(48)
            .build();
        button.set_widget_name(&format!("key-{key}"));
        if matches!(*key, "*" | "#") {
            // Refused everywhere: shown for the familiar shape, never pressed.
            button.set_sensitive(false);
            button.set_tooltip_text(Some("Codes cannot be dialled from the computer"));
        } else {
            let entry = entry.clone();
            button.connect_clicked(move |_| {
                let text = press(&entry.text(), key);
                entry.set_text(&text);
                entry.set_position(-1);
            });
        }
        grid.attach(&button, (index % 3) as i32, (index / 3) as i32, 1, 1);
        keys.push(button);
    }
    root.append(&grid);

    let call = ui_kit::button(ButtonKind::Primary, "Call", Some("desklink-call-symbolic"));
    call.set_halign(gtk::Align::Center);
    call.set_sensitive(false);
    call.update_property(&[gtk::accessible::Property::Label(
        "Call this number on the phone",
    )]);
    root.append(&call);

    let fire = {
        let (entry, on_call) = (entry.clone(), on_call.clone());
        move || {
            let text = entry.text().to_string();
            if !text.trim().is_empty() {
                on_call(text)
            }
        }
    };
    {
        let fire = fire.clone();
        call.connect_clicked(move |_| fire());
    }
    entry.connect_activate(move |_| fire());

    // Suggestions follow what is typed; the Call button waits for text.
    {
        let (call, suggested, entry2) = (call.clone(), suggested.clone(), entry.clone());
        entry.connect_changed(move |entry| {
            let text = entry.text();
            call.set_sensitive(!text.trim().is_empty());
            while let Some(child) = suggested.first_child() {
                suggested.remove(&child);
            }
            for (name, number) in suggestions(&contacts, &text, 4) {
                let row = gtk::Button::builder()
                    .label(format!("{name} · {number}"))
                    .css_classes(["flat"])
                    .halign(gtk::Align::Start)
                    .build();
                let entry = entry2.clone();
                row.connect_clicked(move |_| {
                    entry.set_text(&number);
                    entry.set_position(-1);
                });
                suggested.append(&row);
            }
        });
    }

    if !recents.is_empty() {
        root.append(
            &gtk::Label::builder()
                .label("Recent")
                .xalign(0.0)
                .css_classes(["dl-secondary"])
                .build(),
        );
        for number in recents {
            let row = gtk::Button::builder()
                .label(format!("Call back {number}"))
                .css_classes(["flat"])
                .halign(gtk::Align::Start)
                .build();
            let on_call = on_call.clone();
            row.connect_clicked(move |_| on_call(number.clone()));
            root.append(&row);
        }
        let clear = gtk::Button::builder()
            .label("Clear recent")
            .css_classes(["flat"])
            .halign(gtk::Align::Start)
            .build();
        clear.connect_clicked(move |_| on_clear());
        root.append(&clear);
    }
    Pad {
        root,
        entry,
        keys,
        call,
    }
}

/// Opens the dial pad over `win` for the selected phone's contacts.
pub(super) fn show(win: &DesklinkWindow) {
    let contacts = win
        .selected_id()
        .map(|id| crate::device_links::contacts::list_for_search(&id))
        .unwrap_or_default();
    let dialog = adw::Dialog::builder()
        .title("Dial")
        .content_width(340)
        .build();
    let (weak_dialog, weak_win) = (dialog.downgrade(), win.downgrade());
    let (clear_dialog,) = (dialog.downgrade(),);
    let pad = pad(
        contacts,
        crate::device_links::dial_recents::load(),
        move |number| {
            if let Some(dialog) = weak_dialog.upgrade() {
                dialog.close();
            }
            if let Some(win) = weak_win.upgrade() {
                super::confirm_dial(&win, &number, None);
            }
        },
        move || {
            crate::device_links::dial_recents::clear();
            if let Some(dialog) = clear_dialog.upgrade() {
                dialog.close();
            }
        },
    );
    dialog.set_child(Some(&pad.root));
    dialog.set_focus(Some(&pad.entry));
    dialog.present(Some(win));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::contacts::Phone;
    use std::cell::RefCell;

    fn contact(name: &str, number: &str) -> Contact {
        Contact {
            uid: name.into(),
            name: name.into(),
            phones: vec![Phone {
                kind: String::new(),
                number: number.into(),
            }],
            emails: vec![],
            org: None,
            note: None,
            photo: None,
        }
    }

    #[test]
    fn keys_add_digits_and_never_codes() {
        assert_eq!(press("12", "3"), "123");
        assert_eq!(press("12", "*"), "12");
        assert_eq!(press("12", "#"), "12");
        assert_eq!(press("", "+"), "+");
        assert_eq!(press("1", "+"), "1");
        assert_eq!(press(&"1".repeat(MAX_CHARS), "2").len(), MAX_CHARS);
    }

    #[test]
    fn suggestions_match_names_and_digits() {
        let book = [
            contact("Alice Ng", "+1 555 0100"),
            contact("Bob", "555-0199"),
        ];
        assert_eq!(
            suggestions(&book, "ali", 4),
            [("Alice Ng".into(), "+1 555 0100".into())]
        );
        assert_eq!(suggestions(&book, "0199", 4).len(), 1);
        assert_eq!(suggestions(&book, "555", 1).len(), 1);
        assert!(suggestions(&book, "", 4).is_empty());
        assert!(suggestions(&book, "zed", 4).is_empty());
    }

    #[test]
    fn the_pad_types_calls_and_keeps_codes_out() {
        crate::test_display::run(|| {
            let called = Rc::new(RefCell::new(Vec::<String>::new()));
            let seen = called.clone();
            let pad = pad(
                vec![contact("Alice", "555 0100")],
                vec!["5550123".into()],
                move |n| seen.borrow_mut().push(n),
                || {},
            );
            assert_eq!(pad.keys.len(), 12);
            assert!(!pad.call.is_sensitive(), "nothing typed");
            for key in &pad.keys {
                if key.is_sensitive() {
                    key.emit_clicked();
                }
            }
            let typed = pad.entry.text().to_string();
            assert_eq!(typed, "1234567890", "digits only");
            let by_label = |label: &str| {
                pad.keys
                    .iter()
                    .find(|k| k.label().as_deref() == Some(label))
                    .unwrap()
                    .clone()
            };
            assert!(!by_label("*").is_sensitive() && !by_label("#").is_sensitive());
            assert!(pad.call.is_sensitive());
            pad.call.emit_clicked();
            assert_eq!(called.borrow().as_slice(), ["1234567890"]);
            // A recent number calls back through the same path.
            let mut child = pad.root.first_child();
            while let Some(widget) = child {
                if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                    if button.label().is_some_and(|l| l.starts_with("Call back")) {
                        button.emit_clicked();
                    }
                }
                child = widget.next_sibling();
            }
            assert_eq!(called.borrow().last().map(String::as_str), Some("5550123"));
        });
    }
}
