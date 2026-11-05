//! The Notes page (F6): the user's notes, here and on the phone. A list
//! (search, pinned first) beside an editor that saves as you type, with
//! markdown-lite shown as you write it: headings, bold, italic, code, lists
//! and checkboxes.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use super::ui_kit::{self, ButtonKind};
use super::{clipboard, text, toast_from, DesklinkWindow};
use crate::device_links::notes::{self, Note};

/// Saved this long after the last keystroke.
const SAVE_AFTER: Duration = Duration::from_millis(800);

#[derive(Debug)]
pub(super) struct Page {
    pub(super) root: gtk::Widget,
    split: adw::NavigationSplitView,
    list: gtk::ListBox,
    search: gtk::SearchEntry,
    editor: Rc<Editor>,
    /// "Remove duplicate copies", shown while conflicted copies exist.
    tidy: gtk::Button,
    /// The notes generation the list shows.
    shown: Cell<u64>,
}

#[derive(Debug)]
struct Editor {
    title: gtk::Entry,
    body: gtk::TextView,
    pin: gtk::ToggleButton,
    delete: gtk::Button,
    /// "Saving…", "Saved": where the autosave stands.
    saved: gtk::Label,
    stack: gtk::Stack,
    /// The note open, and the `modified` it had when loaded or saved.
    open: RefCell<Option<(String, i64)>>,
    /// Typed and not saved yet.
    pending: RefCell<Option<glib::SourceId>>,
    /// Setting the text from a note, not the user typing.
    loading: Cell<bool>,
    /// The person changed something since the note was loaded or last saved. A
    /// save of an untouched editor must not run: it would adopt the stored version
    /// (a phone's edit that landed) as the editor's own without showing it.
    dirty: Cell<bool>,
}

impl Editor {
    fn load(&self, note: Option<&Note>) {
        self.cancel_save();
        self.dirty.set(false);
        self.saved.set_label("");
        self.loading.set(true);
        match note {
            Some(note) => {
                self.title.set_text(&note.title);
                self.body.buffer().set_text(&note.body);
                self.pin.set_active(note.pinned);
                self.open.replace(Some((note.id.clone(), note.modified)));
                self.stack.set_visible_child_name("editor");
            }
            None => {
                self.open.replace(None);
                self.stack.set_visible_child_name("none");
            }
        }
        style(&self.body.buffer());
        self.loading.set(false);
    }

    fn cancel_save(&self) {
        if let Some(source) = self.pending.borrow_mut().take() {
            source.remove();
        }
    }

    /// Saves now what is typed.
    fn save(&self) {
        self.cancel_save();
        if !self.dirty.get() {
            return;
        }
        let Some((id, loaded)) = self.open.borrow().clone() else {
            return;
        };
        let buffer = self.body.buffer();
        let body = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), false)
            .to_string();
        let title = self.title.text().to_string();
        let pinned = self.pin.is_active();
        match notes::edit_note_from(&id, Some(loaded), |note| {
            note.title = title;
            note.body = body;
            note.pinned = pinned;
        }) {
            // The version this save left, read in the same step as the save: one
            // read afterwards could be a phone's edit that landed in between.
            Ok(Some(modified)) => {
                self.dirty.set(false);
                self.open.replace(Some((id, modified)));
                self.saved.set_label("Saved");
            }
            // Nothing to save (the text is what is stored): the version stays as loaded.
            Ok(None) => {
                self.dirty.set(false);
                self.saved.set_label("Saved");
            }
            Err(error) => {
                // The text stays in the editor; nothing is thrown away.
                self.saved.set_label("Not saved");
                toast_from(&self.body, &error);
            }
        }
    }

    fn schedule_save(self: &Rc<Self>) {
        if self.loading.get() {
            return;
        }
        self.cancel_save();
        self.dirty.set(true);
        self.saved.set_label("Saving…");
        let editor = Rc::downgrade(self);
        let source = glib::timeout_add_local_once(SAVE_AFTER, move || {
            if let Some(editor) = editor.upgrade() {
                editor.pending.replace(None);
                editor.save();
            }
        });
        self.pending.replace(Some(source));
    }
}

/// Markdown-lite, shown in place: the marks stay, the text takes the look.
fn style(buffer: &gtk::TextBuffer) {
    let table = buffer.tag_table();
    let tag = |name: &str, build: &dyn Fn(&gtk::TextTag)| {
        table.lookup(name).unwrap_or_else(|| {
            let tag = gtk::TextTag::new(Some(name));
            build(&tag);
            table.add(&tag);
            tag
        })
    };
    let h1 = tag("h1", &|tag| {
        tag.set_scale(1.6);
        tag.set_weight(700);
    });
    let h2 = tag("h2", &|tag| {
        tag.set_scale(1.3);
        tag.set_weight(700);
    });
    let bold = tag("bold", &|tag| tag.set_weight(700));
    let italic = tag("italic", &|tag| tag.set_style(gtk::pango::Style::Italic));
    let code = tag("code", &|tag| tag.set_family(Some("monospace")));
    let mark = tag("mark", &|tag| {
        tag.set_foreground_rgba(Some(&gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0)))
    });
    let done = tag("done", &|tag| tag.set_strikethrough(true));
    buffer.remove_all_tags(&buffer.start_iter(), &buffer.end_iter());
    let text = buffer
        .text(&buffer.start_iter(), &buffer.end_iter(), false)
        .to_string();
    for (number, content) in text.split('\n').enumerate() {
        let at = |byte: usize| buffer.iter_at_line_index(number as i32, byte as i32);
        for (span, tag) in spans(content) {
            let tag = match tag {
                Span::H1 => &h1,
                Span::H2 => &h2,
                Span::Bold => &bold,
                Span::Italic => &italic,
                Span::Code => &code,
                Span::Mark => &mark,
                Span::Done => &done,
            };
            if let (Some(start), Some(end)) = (at(span.0), at(span.1.min(content.len()))) {
                buffer.apply_tag(tag, &start, &end);
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Span {
    H1,
    H2,
    Bold,
    Italic,
    Code,
    /// A list or checkbox mark, dimmed.
    Mark,
    /// A checked item's text.
    Done,
}

/// The styled byte ranges of one line.
fn spans(line: &str) -> Vec<((usize, usize), Span)> {
    let mut found = Vec::new();
    if line.starts_with("## ") {
        return vec![((0, line.len()), Span::H2)];
    }
    if line.starts_with("# ") {
        return vec![((0, line.len()), Span::H1)];
    }
    let indent = line.len() - line.trim_start().len();
    let trimmed = &line[indent..];
    for (mark, checked) in [("- [x] ", true), ("- [X] ", true), ("- [ ] ", false)] {
        if trimmed.starts_with(mark) {
            found.push(((indent, indent + mark.len()), Span::Mark));
            if checked {
                found.push(((indent + mark.len(), line.len()), Span::Done));
            }
            return found;
        }
    }
    let number = trimmed.chars().take_while(char::is_ascii_digit).count();
    if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        found.push(((indent, indent + 2), Span::Mark));
    } else if number > 0 && trimmed[number..].starts_with(". ") {
        found.push(((indent, indent + number + 2), Span::Mark));
    }
    // Inline: `code`, **bold**, *italic*, in that order of precedence.
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &line[i..];
        let (open, kind) = if rest.starts_with('`') {
            ("`", Span::Code)
        } else if rest.starts_with("**") {
            ("**", Span::Bold)
        } else if rest.starts_with('*') && i >= found.last().map_or(0, |((_, end), _)| *end) {
            ("*", Span::Italic)
        } else {
            i += rest.chars().next().map_or(1, char::len_utf8);
            continue;
        };
        let inner = i + open.len();
        match line[inner..].find(open) {
            Some(length) if length > 0 => {
                let end = inner + length + open.len();
                found.push(((i, end), kind));
                i = end;
            }
            _ => i += open.len(),
        }
    }
    found
}

/// A line of a note as plain words for the list: its list, task and heading
/// marks and its bold, italic and code marks left out.
fn plain_words(line: &str) -> String {
    line.trim_start_matches(['#', '>', ' ', '-', '*'])
        .trim_start_matches("[ ] ")
        .trim_start_matches("[x] ")
        .replace("**", "")
        .replace(['*', '`'], "")
}

fn note_row(note: &Note) -> gtk::ListBoxRow {
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .build();
    let title = super::ellipsized(&note.label(), &["heading"]);
    title.set_xalign(0.0);
    words.append(&title);
    let preview: String = note
        .body
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .find(|line| line.trim_start_matches(['#', ' ']) != note.label())
        .map(plain_words)
        .unwrap_or_default();
    let detail = super::ellipsized(
        &if preview.is_empty() {
            modified_words(note.modified)
        } else {
            format!("{} · {preview}", modified_words(note.modified))
        },
        &["dl-secondary"],
    );
    detail.set_xalign(0.0);
    words.append(&detail);
    let line = gtk::Box::builder()
        .spacing(10)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    line.append(&words);
    if note.pinned {
        line.append(
            &gtk::Image::builder()
                .icon_name("desklink-keep-filled-symbolic")
                .valign(gtk::Align::Center)
                .tooltip_text("Pinned")
                .build(),
        );
    }
    let row = gtk::ListBoxRow::builder().child(&line).build();
    row.set_widget_name(&note.id);
    row.update_property(&[gtk::accessible::Property::Label(&note.label())]);
    row
}

/// Asks, with the counts, before deleting duplicate conflicted copies. A copy
/// with text found nowhere else is never deleted; it is only counted.
fn confirm_remove_duplicates(button: &gtk::Button) {
    let (removable, kept) = notes::duplicate_copies_preview();
    let Some(window) = button.root().and_downcast::<DesklinkWindow>() else {
        return;
    };
    if removable == 0 {
        window.show_feature_toast(&format!(
            "No duplicate copies. {kept} {} different text, kept.",
            if kept == 1 { "copy has" } else { "copies have" }
        ));
        return;
    }
    let dialog = adw::AlertDialog::new(
        Some("Remove duplicate copies?"),
        Some(&format!(
            "{removable} conflicted {} the same text as another note and will be deleted, here and on your phone.{}",
            if removable == 1 { "copy has" } else { "copies have" },
            if kept == 0 {
                String::new()
            } else {
                format!(" {kept} with different text will be kept.")
            }
        )),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("remove", "_Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let weak = window.downgrade();
    dialog.connect_response(Some("remove"), move |_, _| {
        let removed = notes::remove_duplicate_copies();
        if let Some(window) = weak.upgrade() {
            window.show_feature_toast(&format!("Removed {removed} duplicate copies"));
        }
    });
    dialog.present(Some(&window));
}

fn modified_words(modified: i64) -> String {
    let now = glib::DateTime::now_local().ok();
    glib::DateTime::from_unix_local(modified / 1000)
        .ok()
        .zip(now.as_ref())
        .map(|(when, now)| super::history::time_label(now, &when))
        .unwrap_or_default()
}

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        clipboard::ensure_css();
        let search = gtk::SearchEntry::builder()
            .placeholder_text("Search notes")
            .hexpand(true)
            .build();
        search.update_property(&[gtk::accessible::Property::Label("Search notes")]);
        let add = ui_kit::icon_button("desklink-add-symbolic", "New note (Ctrl+N)");
        add.update_property(&[gtk::accessible::Property::Label("New note")]);
        let top = gtk::Box::builder()
            .spacing(6)
            .margin_start(12)
            .margin_end(8)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        top.append(&search);
        top.append(&add);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::Single)
            .css_classes(["dl-note-list"])
            .build();
        list.update_property(&[gtk::accessible::Property::Label("Notes")]);
        // Collapsed (a narrow window) this list is all that shows: with no notes
        // it says so and how to start one, instead of an empty pane.
        let none = clipboard::empty_state(
            "desklink-edit-note-symbolic",
            "No notes yet",
            "Notes you write here also appear on your phone.",
        );
        let first = ui_kit::button(
            ButtonKind::Primary,
            "New note",
            Some("desklink-add-symbolic"),
        );
        first.set_halign(gtk::Align::Center);
        none.root.append(&first);
        list.set_placeholder(Some(&none.root));
        list.set_vexpand(true);
        let sidebar_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        sidebar_box.append(&top);
        let tidy = ui_kit::button(ButtonKind::Text, "Remove duplicate copies", None);
        tidy.set_tooltip_text(Some(
            "Deletes conflicted copies that say exactly what another note says",
        ));
        tidy.set_margin_start(12);
        tidy.set_margin_end(12);
        tidy.set_margin_bottom(6);
        tidy.set_visible(false);
        tidy.connect_clicked(confirm_remove_duplicates);
        sidebar_box.append(&tidy);
        sidebar_box.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&list)
                .build(),
        );

        let title = gtk::Entry::builder()
            .placeholder_text("Title")
            .hexpand(true)
            .css_classes(["flat", "note-title"])
            .build();
        title.update_property(&[gtk::accessible::Property::Label("Note title")]);
        let pin = gtk::ToggleButton::builder()
            .icon_name("desklink-keep-symbolic")
            .tooltip_text("Pin to the top")
            .valign(gtk::Align::Center)
            .css_classes(["dl-icon-btn"])
            .build();
        pin.update_property(&[gtk::accessible::Property::Label("Pin to the top")]);
        let delete = ui_kit::icon_button("desklink-delete-symbolic", "Delete note");
        delete.set_valign(gtk::Align::Center);
        // Back to the list, when only one of them fits.
        let back = ui_kit::icon_button("desklink-arrow-back-symbolic", "All notes");
        back.set_valign(gtk::Align::Center);
        let saved = gtk::Label::builder()
            .css_classes(["dl-saved"])
            .valign(gtk::Align::Center)
            .build();
        let head = gtk::Box::builder().spacing(6).build();
        head.append(&back);
        head.append(&title);
        head.append(&saved);
        head.append(&pin);
        head.append(&delete);
        let body = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(24)
            .left_margin(4)
            .right_margin(4)
            .vexpand(true)
            .css_classes(["note-body"])
            .build();
        body.update_property(&[gtk::accessible::Property::Label("Note")]);
        let editing = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(18)
            .margin_end(18)
            .margin_top(8)
            .build();
        editing.append(&head);
        editing.append(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .vexpand(true)
                .child(&body)
                .build(),
        );
        let hint = text(
            "Synced with your phone and kept here, encrypted, even if you unpair it.",
            &["dl-caption"],
        );
        hint.set_wrap(true);
        hint.set_margin_bottom(8);
        editing.append(&hint);
        let empty = clipboard::empty_state(
            "desklink-edit-note-symbolic",
            "No note selected",
            "Pick a note, or start a new one with +",
        );
        let stack = gtk::Stack::new();
        stack.add_named(&editing, Some("editor"));
        stack.add_named(&empty.root, Some("none"));
        stack.set_visible_child_name("none");

        let split = adw::NavigationSplitView::builder()
            .sidebar(&adw::NavigationPage::new(&sidebar_box, "Notes"))
            .content(&adw::NavigationPage::new(&stack, "Note"))
            .min_sidebar_width(220.0)
            .max_sidebar_width(320.0)
            .build();
        split.set_widget_name("notes-split");
        split
            .bind_property("collapsed", &back, "visible")
            .sync_create()
            .build();
        {
            let split = split.downgrade();
            back.connect_clicked(move |_| {
                if let Some(split) = split.upgrade() {
                    split.set_show_content(false);
                }
            });
        }

        let editor = Rc::new(Editor {
            title,
            body,
            pin,
            delete,
            saved,
            stack,
            open: RefCell::new(None),
            pending: RefCell::new(None),
            loading: Cell::new(false),
            dirty: Cell::new(false),
        });
        {
            let weak = Rc::downgrade(&editor);
            editor.title.connect_changed(move |_| {
                if let Some(editor) = weak.upgrade() {
                    editor.schedule_save();
                }
            });
            let weak = Rc::downgrade(&editor);
            editor.body.buffer().connect_changed(move |buffer| {
                if let Some(editor) = weak.upgrade() {
                    if !editor.loading.get() {
                        style(buffer);
                    }
                    editor.schedule_save();
                }
            });
            let weak = Rc::downgrade(&editor);
            editor.pin.connect_toggled(move |_| {
                if let Some(editor) = weak.upgrade() {
                    if !editor.loading.get() {
                        editor.dirty.set(true);
                        editor.save();
                    }
                }
            });
            let (weak, split_weak) = (Rc::downgrade(&editor), split.downgrade());
            editor.delete.connect_clicked(move |button| {
                let Some(editor) = weak.upgrade() else {
                    return;
                };
                if let Some((id, _)) = editor.open.borrow().clone() {
                    editor.cancel_save();
                    let _ = notes::edit_note(&id, |note| note.deleted = true);
                    toast_from(button, "Note deleted");
                }
                editor.load(None);
                if let Some(split) = split_weak.upgrade() {
                    split.set_show_content(false);
                }
            });
        }
        let terms = Rc::new(RefCell::new(String::new()));
        {
            let filter_terms = Rc::clone(&terms);
            list.set_filter_func(move |row| {
                let terms = filter_terms.borrow();
                if terms.is_empty() {
                    return true;
                }
                notes::note(row.widget_name().as_str()).is_some_and(|note| {
                    let haystack = format!("{} {}", note.title, note.body).to_lowercase();
                    terms.split_whitespace().all(|term| haystack.contains(term))
                })
            });
            let (list, terms) = (list.clone(), Rc::clone(&terms));
            search.connect_search_changed(move |entry| {
                terms.replace(entry.text().trim().to_lowercase());
                list.invalidate_filter();
            });
        }
        {
            let (editor, split) = (Rc::clone(&editor), split.clone());
            list.connect_row_activated(move |_, row| {
                editor.save();
                editor.load(notes::note(row.widget_name().as_str()).as_ref());
                split.set_show_content(true);
            });
        }
        let page = Self {
            root: split.clone().upcast(),
            split,
            list,
            search,
            editor,
            tidy,
            shown: Cell::new(u64::MAX),
        };
        for button in [add, first] {
            let win = win.downgrade();
            button.connect_clicked(move |_| {
                if let Some(win) = win.upgrade() {
                    win.new_note();
                }
            });
        }
        page
    }

    /// Saves what is typed but not yet saved (the window is closing).
    pub(super) fn flush(&self) {
        if self.editor.pending.borrow().is_some() {
            self.editor.save();
        }
    }

    /// Redraws the list when the notes changed (here, or from the phone),
    /// and the open note when the phone changed it and nothing is typed.
    pub(super) fn update(&self) {
        let generation = notes::generation();
        if self.shown.replace(generation) == generation {
            return;
        }
        let (removable, kept) = notes::duplicate_copies_preview();
        self.tidy.set_visible(removable + kept > 0);
        let open = self.editor.open.borrow().clone();
        // Rows only: `first_child` can be the list's placeholder, and removing
        // that is how the empty pane lost its "No Notes Yet" prompt.
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        for note in notes::notes() {
            let row = note_row(&note);
            self.list.append(&row);
            if open.as_ref().is_some_and(|(id, _)| *id == note.id) {
                self.list.select_row(Some(&row));
            }
        }
        if let Some((id, modified)) = open {
            let typing = self.editor.pending.borrow().is_some();
            match notes::note(&id) {
                // Deleted on the phone.
                None if !typing => self.editor.load(None),
                Some(note) if note.modified != modified && !typing => self.editor.load(Some(&note)),
                _ => {}
            }
        }
    }

    /// Opens the `index`th note listed (debug e2e).
    #[cfg(debug_assertions)]
    pub(super) fn open_nth(&self, index: i32) {
        if let Some(row) = self.list.row_at_index(index) {
            row.emit_activate();
        }
    }

    /// Starts a new note and opens it, the title ready to type.
    pub(super) fn new_note(&self) {
        self.editor.save();
        self.search.set_text("");
        let Some(id) = notes::new_note("") else {
            toast_from(&self.list, "Too many notes (2000 at most)");
            return;
        };
        self.update();
        self.editor.load(notes::note(&id).as_ref());
        self.split.set_show_content(true);
        self.editor.title.grab_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list shows a note's words, not its marks.
    #[test]
    fn a_notes_preview_in_the_list_has_no_markdown_marks() {
        assert_eq!(
            plain_words("Ship **F6** after the *real-phone* run."),
            "Ship F6 after the real-phone run."
        );
        assert_eq!(plain_words("- [x] contract"), "contract");
        assert_eq!(plain_words("- `cargo test` green"), "cargo test green");
        assert_eq!(plain_words("## Plan"), "Plan");
    }

    #[test]
    fn markdown_lite_marks_headings_lists_and_inline_styles() {
        assert_eq!(spans("# Title"), vec![((0, 7), Span::H1)]);
        assert_eq!(spans("## Sub"), vec![((0, 6), Span::H2)]);
        assert_eq!(spans("#nope"), vec![]);
        assert_eq!(
            spans("- [x] done"),
            vec![((0, 6), Span::Mark), ((6, 10), Span::Done)]
        );
        assert_eq!(spans("  - [ ] todo"), vec![((2, 8), Span::Mark)]);
        assert_eq!(spans("12. item"), vec![((0, 4), Span::Mark)]);
        assert_eq!(
            spans("- a **b** and *c* `d*e`"),
            vec![
                ((0, 2), Span::Mark),
                ((4, 9), Span::Bold),
                ((14, 17), Span::Italic),
                ((18, 23), Span::Code)
            ]
        );
        assert_eq!(spans("2 * 3 = 6"), vec![], "a lone star is not italic");
        assert_eq!(spans("é **ü**"), vec![((3, 9), Span::Bold)]);
    }
}

/// Notes and Snippets with no phone to show pages for (they are the user's,
/// paired or not): its own view in the window, switched between the two.
#[derive(Debug)]
pub(super) struct Solo {
    pub(super) root: gtk::Widget,
    pub(super) notes: Page,
    stack: adw::ViewStack,
    snippets: gtk::Box,
    shown: Cell<u64>,
}

impl Solo {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        let notes = Page::new(win);
        let snippets = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(12)
            .margin_bottom(28)
            .margin_start(18)
            .margin_end(18)
            .build();
        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(
            &notes.root,
            Some("notes"),
            "Notes",
            "desklink-edit-note-symbolic",
        );
        stack.add_titled_with_icon(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(
                    &adw::Clamp::builder()
                        .maximum_size(760)
                        .child(&snippets)
                        .build(),
                )
                .build(),
            Some("snippets"),
            "Snippets",
            "desklink-keep-symbolic",
        );
        let back = gtk::Button::builder()
            .icon_name("go-previous-symbolic")
            .tooltip_text("Back")
            .build();
        back.update_property(&[gtk::accessible::Property::Label("Back")]);
        {
            let win = win.downgrade();
            back.connect_clicked(move |_| {
                if let Some(win) = win.upgrade() {
                    win.leave_solo();
                }
            });
        }
        // The switcher is in the header, or along the bottom when narrow
        // (`apply_layout`).
        let switcher = adw::ViewSwitcher::builder()
            .stack(&stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        switcher.set_widget_name("solo-switcher");
        let header = adw::HeaderBar::builder().title_widget(&switcher).build();
        header.pack_start(&back);
        let bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
        bar.set_widget_name("solo-bar");
        let view = adw::ToolbarView::new();
        view.add_top_bar(&header);
        view.add_bottom_bar(&bar);
        view.set_content(Some(&stack));
        Self {
            root: view.upcast(),
            notes,
            stack,
            snippets,
            shown: Cell::new(u64::MAX),
        }
    }

    pub(super) fn show(&self, which: &str) {
        self.stack.set_visible_child_name(which);
        self.update();
    }

    pub(super) fn update(&self) {
        self.notes.update();
        let generation = notes::generation();
        if self.shown.replace(generation) != generation {
            while let Some(child) = self.snippets.first_child() {
                self.snippets.remove(&child);
            }
            self.snippets.append(&super::clipboard::snippets_section());
        }
    }
}
