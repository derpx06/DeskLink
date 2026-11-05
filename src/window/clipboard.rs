//! The Clipboard history tab of Notes & Clipboard: the last 20 items shared,
//! each with Copy and Pin as Snippet; and the Snippets tab's list. The
//! history is held in memory only and never lists an item a password manager
//! marked as a secret. The sharing switch is in the Clipboard tab's head
//! (`notes_clipboard`).
//!
//! Also the few widgets the pages of redesign wave C share: the stylesheet
//! (`wave-c.css`), the empty state and the tab chips.

use adw::prelude::*;

use super::overview::preview_line;
use super::toast_from;
use super::ui_kit::{self, Tone};

/// Loads `wave-c.css` once (above the kit, `ui-kit.css`). Every page of the
/// wave calls this when it is built.
pub(super) fn ensure_css() {
    thread_local! {
        static LOADED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    if LOADED.with(|loaded| loaded.replace(true)) {
        return;
    }
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("../wave-c.css"));
    gtk::style_context_add_provider_for_display(
        &display,
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
}

/// An empty state (`UI-REDESIGN.md` 2.8.15): a 72 px circle with a glyph, a
/// title, up to three lines; the caller appends at most one filled button.
#[derive(Debug, Clone)]
pub(super) struct EmptyState {
    pub(super) root: gtk::Box,
    pub(super) title: gtk::Label,
    pub(super) body: gtk::Label,
}

pub(super) fn empty_state(icon: &str, title: &str, body: &str) -> EmptyState {
    let root = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .margin_top(32)
        .margin_bottom(32)
        .margin_start(16)
        .margin_end(16)
        .build();
    let circle = gtk::CenterBox::builder()
        .halign(gtk::Align::Center)
        .css_classes(["dl-empty-circle"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .center_widget(
            &gtk::Image::builder()
                .icon_name(icon)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        )
        .build();
    root.append(&circle);
    let title = gtk::Label::builder()
        .label(title)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(32)
        .css_classes(["dl-empty-title"])
        .accessible_role(gtk::AccessibleRole::Heading)
        .build();
    title.update_property(&[gtk::accessible::Property::Level(2)]);
    let body = gtk::Label::builder()
        .label(body)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .max_width_chars(40)
        .css_classes(["dl-empty-body"])
        .build();
    root.append(&title);
    root.append(&body);
    EmptyState { root, title, body }
}

/// What the history tab was drawn from: it is redrawn only when this changes.
pub(super) fn history_key(on: bool) -> String {
    serde_json::to_string(&(
        on,
        crate::device_links::daemon::clipboard::password_detection(),
        crate::device_links::daemon::clipboard::clipboard_unchecked(),
        crate::device_links::daemon::clipboard::history()
            .iter()
            .map(|item| (item.text.clone(), item.from.clone(), item.at))
            .collect::<Vec<_>>(),
    ))
    .unwrap_or_default()
}

/// "From Pixel 9 · 2 min ago": a history row's second line.
fn history_subtitle(from: &str, at: u64, now: &gtk::glib::DateTime) -> String {
    let when = gtk::glib::DateTime::from_unix_local(at as i64)
        .map(|when| super::history::time_label(now, &when))
        .unwrap_or_default();
    format!("From {from} · {when}")
}

const AT_PREFIX: &str = "clip-at:";

/// Brings the "2 min ago" of the history rows under `root` up to date, in
/// place: the list is rebuilt only when what was copied changes, not each
/// minute.
pub(super) fn refresh_times(root: &gtk::Widget) {
    let Ok(now) = gtk::glib::DateTime::now_local() else {
        return;
    };
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if let (Some(row), Some(rest)) = (
            widget.downcast_ref::<adw::ActionRow>(),
            widget.widget_name().strip_prefix(AT_PREFIX),
        ) {
            if let Some((at, from)) = rest.split_once(':') {
                let words = history_subtitle(from, at.parse().unwrap_or(0), &now);
                let words = gtk::glib::markup_escape_text(&words);
                if row.subtitle().as_deref() != Some(words.as_str()) {
                    row.set_subtitle(&words);
                }
            }
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
}

/// The line under the sharing switch: what is shared and what is not.
pub(super) fn sharing_note() -> &'static str {
    match crate::device_links::daemon::clipboard::password_detection() {
        Some(true) => "Copy on one device, paste on the other. Passwords are never shared.",
        Some(false) => {
            "Copy on one device, paste on the other. Password detection isn't available in \
             this session, so copied passwords are shared too."
        }
        None => "Copy on one device, paste on the other.",
    }
}

/// A list of kit rows with a name for screen readers.
fn group(label: &str, rows: &[adw::ActionRow]) -> gtk::ListBox {
    let list = ui_kit::row_group(rows);
    list.update_property(&[gtk::accessible::Property::Label(label)]);
    list
}

/// A row that copies `value` when activated, and says so.
fn copy_on_activate(row: &adw::ActionRow, value: String) {
    row.set_activatable(true);
    row.add_css_class("dl-activatable");
    row.connect_activated(move |row| {
        row.clipboard().set_text(&value);
        toast_from(row, "Copied");
    });
}

/// The whole text as a tooltip, capped, for a row that shows a cut preview.
fn full_text_tip(row: &adw::ActionRow, text: &str) {
    let tip: String = text.chars().take(400).collect();
    row.set_tooltip_text(Some(&tip));
}

/// The clipboard history: a quiet line about what could not be checked, then
/// the list (the sharing switch and its words are above it, in the tab).
pub(super) fn history_box(on: bool) -> gtk::Box {
    let items = crate::device_links::daemon::clipboard::history();
    let rows = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    if crate::device_links::daemon::clipboard::clipboard_unchecked() {
        let quiet = gtk::Label::builder()
            .label("Clipboard not shared: couldn't check it.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["dl-secondary"])
            .build();
        rows.append(&quiet);
    }
    if items.is_empty() {
        let empty = empty_state(
            "desklink-content-paste-symbolic",
            "Nothing copied yet",
            if on {
                "Text you copy here or on the phone appears here."
            } else {
                "Sharing is off. Nothing is copied between the devices."
            },
        );
        rows.append(&empty.root);
        return rows;
    }
    let now = gtk::glib::DateTime::now_local().ok();
    let mut list_rows = Vec::new();
    for item in items {
        let from = item
            .from
            .clone()
            .unwrap_or_else(|| "this computer".to_string());
        let preview = preview_line(&item.text);
        let row = ui_kit::list_row(
            if item.from.is_some() {
                "desklink-device-phone-symbolic"
            } else {
                "desklink-device-desktop-symbolic"
            },
            Tone::Neutral,
            &preview,
            now.as_ref()
                .map(|now| history_subtitle(&from, item.at, now))
                .as_deref(),
        );
        row.set_widget_name(&format!("{AT_PREFIX}{}:{from}", item.at));
        row.set_title_lines(2);
        row.add_css_class("dl-mono-row");
        full_text_tip(&row, &item.text);
        copy_on_activate(&row, item.text.clone());

        // F6: keep it as a snippet, here and on the phone.
        let pin = ui_kit::icon_button("desklink-keep-symbolic", "Pin as a snippet");
        pin.update_property(&[gtk::accessible::Property::Label(&format!(
            "Pin {preview} as a snippet"
        ))]);
        let value = item.text.clone();
        pin.connect_clicked(
            move |button| match crate::device_links::notes::add_snippet(&value) {
                Ok(()) => toast_from(button, "Pinned to snippets"),
                Err(error) => toast_from(button, &error),
            },
        );
        let copy = ui_kit::icon_button("desklink-content-copy-symbolic", "Copy");
        copy.update_property(&[gtk::accessible::Property::Label(&format!("Copy {preview}"))]);
        let value = item.text.clone();
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&value);
            toast_from(button, "Copied");
        });
        for button in [&pin, &copy] {
            button.set_valign(gtk::Align::Center);
        }
        row.add_suffix(&pin);
        row.add_suffix(&copy);
        list_rows.push(row);
    }
    rows.append(&group("Clipboard history", &list_rows));
    rows
}

/// Snippets (F6): texts kept to paste again, synced with the phone. Copy on
/// click; remove with the bin.
pub(super) fn snippets_section() -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
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
        .css_classes(["dl-row-group"])
        .build();
    adding.append(&add);
    section.append(&adding);

    let snippets = crate::device_links::notes::snippets();
    if snippets.is_empty() {
        let empty = empty_state(
            "desklink-keep-symbolic",
            "No snippets yet",
            "Add text above, or pin it from the clipboard history, to keep it here and on \
             your phone.",
        );
        section.append(&empty.root);
        return section;
    }
    let mut list_rows = Vec::new();
    for snippet in snippets {
        let label = snippet.label();
        let row = ui_kit::list_row("desklink-keep-symbolic", Tone::Accent, &label, None);
        row.set_title_lines(2);
        row.add_css_class("dl-mono-row");
        full_text_tip(&row, &snippet.text);
        copy_on_activate(&row, snippet.text.clone());

        let copy = ui_kit::icon_button("desklink-content-copy-symbolic", "Copy");
        copy.update_property(&[gtk::accessible::Property::Label(&format!("Copy {label}"))]);
        let value = snippet.text.clone();
        copy.connect_clicked(move |button| {
            button.clipboard().set_text(&value);
            toast_from(button, "Copied");
        });
        let remove = ui_kit::icon_button("desklink-delete-symbolic", "Remove snippet");
        remove.update_property(&[gtk::accessible::Property::Label(&format!(
            "Remove snippet {label}"
        ))]);
        let id = snippet.id.clone();
        remove.connect_clicked(move |button| {
            crate::device_links::notes::delete_snippet(&id);
            toast_from(button, "Snippet removed");
        });
        for button in [&copy, &remove] {
            button.set_valign(gtk::Align::Center);
        }
        row.add_suffix(&copy);
        row.add_suffix(&remove);
        list_rows.push(row);
    }
    section.append(&group("Snippets", &list_rows));
    section
}

/// A page's chip row (Notes, Snippets, Clipboard), one chosen at a time.
/// `on_pick` gets the name of the chip chosen. Returns the row and the chips
/// by name, so code can move the choice ([`pick_chip`]).
pub(super) fn tab_chips(
    tabs: &[(&str, &str, &str)],
    selected: &str,
    on_pick: impl Fn(&str) + 'static,
) -> (gtk::Box, Vec<(String, gtk::ToggleButton)>) {
    let row = gtk::Box::builder()
        .spacing(8)
        .accessible_role(gtk::AccessibleRole::Group)
        .build();
    let on_pick = std::rc::Rc::new(on_pick);
    let mut chips: Vec<(String, gtk::ToggleButton)> = Vec::new();
    for (name, label, icon) in tabs {
        let chip = ui_kit::chip(label, Some(icon), *name == selected);
        if let Some((_, first)) = chips.first() {
            chip.set_group(Some(first));
        }
        let name = name.to_string();
        chip.connect_toggled({
            let (name, on_pick) = (name.clone(), on_pick.clone());
            move |chip| {
                if chip.is_active() {
                    on_pick(&name);
                }
            }
        });
        row.append(&chip);
        chips.push((name, chip));
    }
    (row, chips)
}

/// Moves the choice of a chip row made by [`tab_chips`].
pub(super) fn pick_chip(chips: &[(String, gtk::ToggleButton)], name: &str) {
    if let Some((_, chip)) = chips.iter().find(|(chip, _)| chip == name) {
        chip.set_active(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "2 h ago" moves on without the list being rebuilt. (Needs a display.)
    #[test]
    fn the_times_of_the_history_rows_are_refreshed_in_place() {
        crate::test_display::run(|| {
            let now = gtk::glib::DateTime::now_local().unwrap();
            let at = now.to_unix() as u64;
            let row = ui_kit::list_row(
                "desklink-device-phone-symbolic",
                Tone::Neutral,
                "git push",
                Some(&history_subtitle("Pixel 9", at - 3 * 3600, &now)),
            );
            row.set_widget_name(&format!("{AT_PREFIX}{}:Pixel 9", at - 3 * 3600));
            let holder = gtk::Box::new(gtk::Orientation::Vertical, 0);
            holder.append(&row);
            assert_eq!(row.subtitle().unwrap(), "From Pixel 9 · 3 h ago");
            // Later: the row's own time says so, nothing else changed.
            row.set_widget_name(&format!("{AT_PREFIX}{}:Pixel 9", at - 20));
            refresh_times(holder.upcast_ref());
            assert_eq!(row.subtitle().unwrap(), "From Pixel 9 · Just now");
        });
    }
}
