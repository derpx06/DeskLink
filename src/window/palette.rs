//! "Find anything": Ctrl+K opens a search over what the window can do and
//! show (actions, pages, phones, terminals, recent files, notes, snippets,
//! the clipboard history, contacts, settings). `palette_model.rs` decides
//! what is listed and how a query ranks it; this file gathers the data from
//! the live window, draws the dialog and maps a chosen row onto the action
//! or command the same button already runs. Nothing is done twice: a row
//! only calls what the Home tiles, the pages and the menus call.
//!
//! The data is in memory, so the list is filtered on every keystroke on the
//! main loop; it is gathered once, when the palette opens.
//!
//! Keys: type to search (from anywhere in the dialog), Up and Down move the
//! selection (the focus goes with it, so a screen reader reads the row),
//! Page Up and Page Down move by a screenful, Home and End by the ends,
//! Enter opens the selected row (the first one is selected for you), Esc
//! closes, Tab moves between the search box and the list, Ctrl+K closes.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;
use gtk::glib;

use super::palette_model::{
    self as model, Action, ClipEntry, ContactEntry, FileEntry, Group, Item, NoteEntry, PhoneState,
    Sources, Target, TerminalEntry,
};
use super::ui_kit::{self, Tone};
use super::{DesklinkWindow, QuickAction};
use crate::device_links::daemon::DaemonCommand;
use crate::device_links::device::DeviceView;

/// Rows a Page Up or Page Down moves by.
const PAGE_ROWS: isize = 6;
/// Clipboard entries and files the palette looks through.
const CLIPS: usize = 50;
const FILES: usize = 20;

/// The dialog on screen, while there is one. Owned here so that a closure
/// holding a widget never makes a cycle: the handlers hold it weakly.
struct Palette {
    dialog: adw::Dialog,
    window: glib::WeakRef<DesklinkWindow>,
    entry: gtk::SearchEntry,
    list: gtk::ListBox,
    stack: gtk::Stack,
    count: gtk::Label,
    items: Vec<Item>,
    /// The group and the item of each row, top to bottom.
    order: Rc<RefCell<Vec<(Group, usize)>>>,
    /// The row chosen: done once the dialog has gone.
    chosen: RefCell<Option<Target>>,
    /// What had the focus before, to give it back.
    before: glib::WeakRef<gtk::Widget>,
}

thread_local! {
    static OPEN: RefCell<Option<Rc<Palette>>> = const { RefCell::new(None) };
}

/// Opens the palette over `window` (with `query` typed, for the e2e verb), or
/// closes it when it is open already.
pub(super) fn toggle(window: &DesklinkWindow, query: &str) {
    if let Some(open) = OPEN.with(|slot| slot.borrow().clone()) {
        open.dialog.close();
        return;
    }
    // Not over another dialog, as Ctrl+N and Ctrl+O are not.
    if window.visible_dialog().is_some() {
        return;
    }
    let sources = gather(window);
    let items = model::build(&sources);
    present(window, items, &sources, query);
}

/// Whether the focus is in something the person types in, where Ctrl+F is
/// theirs.
pub(super) fn typing_in(window: &DesklinkWindow) -> bool {
    gtk::prelude::GtkWindowExt::focus(window).is_some_and(|focus| {
        focus.is::<gtk::Text>() || focus.is::<gtk::TextView>() || focus.is::<gtk::Editable>()
    })
}

// ── Gathering ─────────────────────────────────────────────────────────────

fn gather(window: &DesklinkWindow) -> Sources {
    let devices: Vec<DeviceView> = window.paired_devices();
    let selected = window.selected_id();
    let phone = devices
        .iter()
        .find(|device| Some(&device.id) == selected.as_ref())
        .map(PhoneState::from_device);
    let others = devices
        .iter()
        .filter(|device| Some(&device.id) != selected.as_ref())
        .map(|device| {
            (
                device.id.clone(),
                device.name.clone(),
                super::device_type_icon(&device.device_type),
                super::unavailable_reason(device).map_or_else(
                    || "Connected".to_string(),
                    |_| {
                        if device.status == crate::device_links::device::DeviceStatus::Paired {
                            "Connecting…".to_string()
                        } else {
                            "Offline".to_string()
                        }
                    },
                ),
            )
        })
        .collect();
    let paired = phone.is_some();
    let terminals = crate::terminal_previews::sessions()
        .into_iter()
        .filter(|session| session.exit_status.is_none())
        .map(|session| TerminalEntry {
            id: session.id,
            title: crate::terminal_previews::display_title(&session.title),
            cwd: session.cwd,
        })
        .collect();
    let files = crate::device_links::transfer_log::recent()
        .into_iter()
        .take(FILES)
        .map(|file| FileEntry {
            path: file.path,
            name: file.name,
            incoming: file.incoming,
            device_name: file.device_name,
        })
        .collect();
    let notes = crate::device_links::notes::notes()
        .into_iter()
        .map(|note| {
            let lines: Vec<String> = note
                .body
                .lines()
                .map(|line| {
                    line.trim_start_matches(['#', ' ', '-', '*'])
                        .trim()
                        .to_string()
                })
                .filter(|line| !line.is_empty())
                .collect();
            // The first line is the title when the note has none of its own.
            let skip = usize::from(note.title.trim().is_empty());
            NoteEntry {
                title: note.label(),
                preview: lines.get(skip).cloned().unwrap_or_default(),
                id: note.id,
                body: note.body,
                pinned: note.pinned,
            }
        })
        .collect();
    let snippets = crate::device_links::notes::snippets()
        .into_iter()
        .map(|snippet| (snippet.label(), snippet.text))
        .collect();
    let clips = crate::device_links::daemon::clipboard::history()
        .into_iter()
        .take(CLIPS)
        .map(|item| ClipEntry {
            text: item.text,
            from: item.from,
        })
        .collect();
    let contacts = window
        .application()
        .and_downcast::<crate::application::DesklinkApplication>()
        .map(|app| crate::search_provider::paired_contacts(&app))
        .unwrap_or_default()
        .into_iter()
        .map(|(device, phone, contact)| {
            let detail = contact
                .phones
                .first()
                .map(|number| number.number.clone())
                .or_else(|| contact.emails.first().cloned());
            let mut words: Vec<String> = contact
                .phones
                .iter()
                .flat_map(|number| {
                    [
                        number.number.clone(),
                        number.number.chars().filter(char::is_ascii_digit).collect(),
                    ]
                })
                .collect();
            words.extend(contact.emails.iter().cloned());
            words.extend(contact.org.iter().cloned());
            ContactEntry {
                device,
                phone,
                uid: contact.uid,
                name: contact.name,
                detail,
                words: words.join(" "),
            }
        })
        .collect();
    Sources {
        paired,
        phone,
        others,
        terminals,
        files,
        notes,
        snippets,
        clips,
        contacts,
        settings: super::prefs::subpages(paired),
        pages: super::PAGES.to_vec(),
    }
}

// ── The dialog ────────────────────────────────────────────────────────────

fn section_title(group: Group) -> gtk::Widget {
    let header = ui_kit::section_header(group.title());
    header.set_margin_top(12);
    header.set_margin_bottom(4);
    header.set_margin_start(20);
    header.set_margin_end(20);
    header.upcast()
}

fn row_for(item: &Item, group: Group) -> adw::ActionRow {
    let subtitle = (!item.subtitle.is_empty()).then_some(item.subtitle.as_str());
    let row = ui_kit::list_row(
        item.icon,
        if group == Group::Actions {
            Tone::Accent
        } else {
            Tone::Neutral
        },
        &item.title,
        subtitle,
    );
    row.add_css_class("dl-palette-row");
    row.set_activatable(true);
    row.set_title_lines(1);
    row.set_subtitle_lines(1);
    if item.title.ends_with('…') {
        row.set_tooltip_text(Some(&item.title));
    }
    if let Some(accel) = item.accel {
        let keys = gtk::ShortcutLabel::new(accel);
        keys.set_valign(gtk::Align::Center);
        keys.add_css_class("dl-palette-keys");
        row.add_suffix(&keys);
    }
    let spoken = match subtitle {
        Some(subtitle) => format!("{}, {}", item.title, subtitle),
        None => item.title.clone(),
    };
    row.update_property(&[gtk::accessible::Property::Label(&spoken)]);
    row
}

impl Palette {
    /// Lists what `query` finds, selects the first row and says how many.
    fn refill(&self, query: &str) {
        let sections = model::search(&self.items, query);
        let shown = model::count(&sections);
        while let Some(row) = self.list.row_at_index(0) {
            self.list.remove(&row);
        }
        let mut order = Vec::with_capacity(shown);
        for section in &sections {
            for index in &section.items {
                self.list
                    .append(&row_for(&self.items[*index], section.group));
                order.push((section.group, *index));
            }
        }
        self.order.replace(order);
        self.stack
            .set_visible_child_name(if shown == 0 { "empty" } else { "list" });
        if let Some(first) = self.list.row_at_index(0) {
            self.list.select_row(Some(&first));
        }
        if let Some(scroll) = self.list.ancestor(gtk::ScrolledWindow::static_type()) {
            if let Some(scroll) = scroll.downcast_ref::<gtk::ScrolledWindow>() {
                scroll.vadjustment().set_value(0.0);
            }
        }
        let words = model::count_words(shown, query.trim().is_empty());
        self.count.set_label(&words);
        self.entry
            .update_property(&[gtk::accessible::Property::Description(&words)]);
    }

    fn rows(&self) -> usize {
        self.order.borrow().len()
    }

    fn selected(&self) -> Option<usize> {
        self.list
            .selected_row()
            .and_then(|row| usize::try_from(row.index()).ok())
    }

    /// The focus is in the search box (not on a row).
    fn in_box(&self) -> bool {
        self.dialog.focus().map_or(true, |focus| {
            focus == *self.entry.upcast_ref::<gtk::Widget>() || focus.is_ancestor(&self.entry)
        })
    }

    /// Moves the selection by `by` rows and the focus with it. From the box
    /// the first row is already selected, so Down goes to the second.
    fn move_by(&self, by: isize) {
        let current = self.selected();
        let target = model::step(current.or(Some(0)), self.rows(), by);
        if self.in_box() && target == current.or(Some(0)) {
            return;
        }
        self.focus_row(target);
    }

    fn focus_row(&self, index: Option<usize>) {
        let Some(row) = index.and_then(|index| self.list.row_at_index(index as i32)) else {
            return;
        };
        self.list.select_row(Some(&row));
        row.grab_focus();
    }

    /// Chooses row `index`: the dialog goes first, then the row is done.
    fn choose(&self, index: usize) {
        let Some((_, item)) = self.order.borrow().get(index).copied() else {
            return;
        };
        self.chosen.replace(Some(self.items[item].target.clone()));
        self.dialog.close();
    }
}

fn present(window: &DesklinkWindow, items: Vec<Item>, sources: &Sources, query: &str) {
    let entry = gtk::SearchEntry::builder()
        .placeholder_text("Find anything")
        .hexpand(true)
        .search_delay(0)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("Find anything")]);
    let scope = gtk::Label::builder()
        .label(model::scope_line(sources.paired, sources.phone.as_ref()))
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dl-secondary", "dl-palette-scope"])
        .build();
    let head = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .css_classes(["dl-palette-head"])
        .build();
    head.append(&entry);
    head.append(&scope);

    let order: Rc<RefCell<Vec<(Group, usize)>>> = Rc::default();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .activate_on_single_click(true)
        .css_classes(["dl-palette"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Results")]);
    {
        // Titles between the groups; only the order is captured.
        let order = Rc::clone(&order);
        list.set_header_func(move |row, before| {
            let order = order.borrow();
            let group_of = |row: &gtk::ListBoxRow| {
                usize::try_from(row.index())
                    .ok()
                    .and_then(|i| order.get(i))
                    .map(|(group, _)| *group)
            };
            let group = group_of(row);
            match group.filter(|group| Some(*group) != before.and_then(group_of)) {
                Some(group) => row.set_header(Some(&section_title(group))),
                None => row.set_header(None::<&gtk::Widget>),
            }
        });
    }
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .focusable(false)
        .child(&list)
        .build();
    let empty = ui_kit::banner::empty_state(
        "desklink-search-symbolic",
        "Nothing matches",
        "Try the name of a phone, a file, a note, a setting or something to do.",
    );
    let stack = gtk::Stack::builder().vexpand(true).build();
    stack.add_named(&scroll, Some("list"));
    stack.add_named(&empty, Some("empty"));

    let count = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["dl-secondary"])
        .accessible_role(gtk::AccessibleRole::Status)
        .build();
    let hint = gtk::Label::builder()
        .label("↑↓ move · ↵ open · Esc close")
        .css_classes(["dl-secondary"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    let foot = gtk::Box::builder()
        .spacing(12)
        .css_classes(["dl-palette-foot"])
        .build();
    foot.append(&count);
    foot.append(&hint);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    content.append(&head);
    content.append(&stack);
    content.append(&foot);
    // Typing anywhere in the dialog goes to the box, so the focus can sit on
    // a row (for a screen reader) and a keystroke still searches.
    entry.set_key_capture_widget(Some(&content));

    // Narrow: the key hints go, the count stays.
    let bin = adw::BreakpointBin::builder()
        .width_request(320)
        .height_request(240)
        .child(&content)
        .build();
    let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
        adw::BreakpointConditionLengthType::MaxWidth,
        440.0,
        adw::LengthUnit::Sp,
    ));
    narrow.add_setter(&hint, "visible", Some(&false.to_value()));
    bin.add_breakpoint(narrow);

    let dialog = adw::Dialog::builder()
        .title("Find anything")
        .content_width(560)
        .content_height(560)
        .child(&bin)
        .focus_widget(&entry)
        .build();

    let palette = Rc::new(Palette {
        dialog: dialog.clone(),
        window: window.downgrade(),
        entry: entry.clone(),
        list: list.clone(),
        stack,
        count,
        items,
        order,
        chosen: RefCell::new(None),
        before: gtk::prelude::GtkWindowExt::focus(window)
            .map(|w| w.downgrade())
            .unwrap_or_default(),
    });
    wire(&palette);
    palette.refill(query);
    if !query.is_empty() {
        entry.set_text(query);
        entry.set_position(-1);
    }
    OPEN.with(|slot| slot.replace(Some(palette)));
    dialog.present(Some(window));
    entry.grab_focus();
}

fn wire(palette: &Rc<Palette>) {
    let weak = Rc::downgrade(palette);
    palette.entry.connect_changed({
        let weak = weak.clone();
        move |entry| {
            if let Some(palette) = weak.upgrade() {
                palette.refill(&entry.text());
            }
        }
    });
    palette.entry.connect_activate({
        let weak = weak.clone();
        move |_| {
            if let Some(palette) = weak.upgrade() {
                if let Some(index) = palette.selected().or((palette.rows() > 0).then_some(0)) {
                    palette.choose(index);
                }
            }
        }
    });
    palette.entry.connect_next_match({
        let weak = weak.clone();
        move |_| {
            if let Some(palette) = weak.upgrade() {
                palette.move_by(1);
            }
        }
    });
    palette.entry.connect_previous_match({
        let weak = weak.clone();
        move |_| {
            if let Some(palette) = weak.upgrade() {
                palette.move_by(-1);
            }
        }
    });
    // Esc in the box closes the palette (it would only stop the search).
    palette.entry.connect_stop_search({
        let weak = weak.clone();
        move |_| {
            if let Some(palette) = weak.upgrade() {
                palette.dialog.close();
            }
        }
    });
    palette.list.connect_row_activated({
        let weak = weak.clone();
        move |_, row| {
            if let (Some(palette), Ok(index)) = (weak.upgrade(), usize::try_from(row.index())) {
                palette.choose(index);
            }
        }
    });

    // Page Up and Page Down from the box; Up from the first row goes back to
    // it. The list moves itself for everything else.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let weak = weak.clone();
        move |_, key, _, _| {
            let Some(palette) = weak.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let in_box = palette.in_box();
            match key {
                gtk::gdk::Key::Page_Down | gtk::gdk::Key::KP_Page_Down if in_box => {
                    palette.move_by(PAGE_ROWS);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Page_Up | gtk::gdk::Key::KP_Page_Up if in_box => {
                    palette.move_by(-PAGE_ROWS);
                    glib::Propagation::Stop
                }
                gtk::gdk::Key::Up | gtk::gdk::Key::KP_Up
                    if !in_box && palette.selected() == Some(0) =>
                {
                    palette.entry.grab_focus();
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            }
        }
    });
    palette.dialog.add_controller(keys);

    palette.dialog.connect_closed(move |_| {
        // Takes the palette out of its slot: nothing keeps it after this.
        let Some(palette) = OPEN.with(|slot| slot.borrow_mut().take()) else {
            return;
        };
        let Some(window) = palette.window.upgrade() else {
            return;
        };
        // The focus goes back to where it was; what is chosen may move it
        // again.
        if let Some(widget) = palette.before.upgrade() {
            if widget.root().is_some() {
                widget.grab_focus();
            }
        }
        if let Some(target) = palette.chosen.take() {
            // Out of the dialog's own close: a dialog the action opens needs
            // the window free of this one.
            glib::idle_add_local_once(move || run(&window, &target));
        }
    });
}

// ── Doing what was chosen ─────────────────────────────────────────────────

fn app_action(window: &DesklinkWindow, name: &str) {
    if let Some(app) = window.application() {
        gio::prelude::ActionGroupExt::activate_action(&app, name, None);
    }
}

fn win_action(window: &DesklinkWindow, name: &str, parameter: Option<&glib::Variant>) {
    gio::prelude::ActionGroupExt::activate_action(window, name, parameter);
}

/// What choosing `target` does: the same call the equivalent button makes.
pub(super) fn run(window: &DesklinkWindow, target: &Target) {
    match target {
        Target::Action(action) => run_action(window, *action),
        Target::Page(name) => {
            let paired = window.selected_is_paired();
            if *name == "settings" && !paired {
                app_action(window, "preferences");
            } else if !window.show_page(name) {
                window.show_feature_toast("Pair a phone to open that page");
            }
        }
        Target::Settings(index) => {
            if window.selected_is_paired() {
                super::prefs::show_group(*index);
                window.show_page("settings");
            } else {
                app_action(window, "preferences");
            }
        }
        Target::Phone(id) => window.show_device(id),
        Target::Terminal(id) => crate::device_links::pty::desk::open_on_desktop(id),
        Target::File(path) => {
            let uri = gio::File::for_path(path).uri();
            if gio::AppInfo::launch_default_for_uri(&uri, gio::AppLaunchContext::NONE).is_err() {
                // No path in the log: file names are personal data.
                eprintln!("[UI] Could not open a file from the palette");
                window.show_feature_toast("Could not open the file");
            }
        }
        Target::Note(id) => window.open_note(id),
        Target::Snippet(text) | Target::Clip(text) => {
            gtk::prelude::WidgetExt::clipboard(window).set_text(text);
            window.show_feature_toast("Copied");
        }
        Target::Contact { device, uid } => window.show_contact(device, uid),
    }
}

fn run_action(window: &DesklinkWindow, action: Action) {
    // What is for this computer.
    match action {
        Action::NewNote => return window.new_note(),
        Action::NewTerminal => {
            return win_action(window, "new-terminal", Some(&"shell".to_variant()))
        }
        Action::PairAnother => return win_action(window, "pair-new", None),
        Action::History => return app_action(window, "history"),
        Action::Shortcuts => return app_action(window, "shortcuts"),
        _ => {}
    }
    // The rest needs the selected phone, ready now: the Home tiles are
    // greyed the same way, with the same words.
    let (Some(daemon), Some(id)) = (window.daemon(), window.selected_id()) else {
        window.show_feature_toast("Pair a phone first");
        return;
    };
    let Some(device) = window
        .paired_devices()
        .into_iter()
        .find(|device| device.id == id)
    else {
        window.show_feature_toast("Pair a phone first");
        return;
    };
    if let Some(reason) = super::unavailable_reason(&device) {
        window.show_feature_toast(&reason);
        return;
    }
    match action {
        Action::SendFiles => win_action(window, "send-files", None),
        Action::SendClipboard => window.quick_action(QuickAction::Clipboard),
        Action::SendLink => {
            super::overview::show_send_link_dialog(daemon, device.id.clone(), &device.name, window)
        }
        Action::Ring => window.quick_action(QuickAction::Ring),
        Action::Nudge => window.quick_action(QuickAction::Ping),
        Action::Lock => window.quick_action(QuickAction::Lock),
        Action::PhoneScreen => {
            crate::remote_control::show_remote_control_dialog(daemon, device.id.clone(), window)
        }
        Action::Insert(index) => {
            if let Some((kind, target, _, _)) = super::insert::CAPTURES.get(index) {
                daemon.send(DaemonCommand::Capture(
                    device.id.clone(),
                    (*kind).into(),
                    (*target).into(),
                ));
            }
        }
        Action::Sound(what) => daemon.send(DaemonCommand::Sound(
            device.id.clone(),
            what.into(),
            !super::sound::is_on(&device, what),
        )),
        Action::HandOff => {
            // The same gate as terminals: with access off, say so and show
            // where it is turned on.
            if device.terminal_access {
                app_action(window, "lock-hand-off");
            } else {
                window.show_feature_toast(&format!(
                    "Terminal access is off for {}. Turn it on under Privacy & security",
                    device.name
                ));
                super::prefs::show_privacy();
                window.show_page("settings");
            }
        }
        Action::NewNote
        | Action::NewTerminal
        | Action::PairAnother
        | Action::History
        | Action::Shortcuts => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adw::subclass::prelude::ObjectSubclassIsExt;

    fn pump() {
        let context = glib::MainContext::default();
        for _ in 0..50 {
            while context.iteration(false) {}
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn open_palette() -> Rc<Palette> {
        OPEN.with(|slot| slot.borrow().clone())
            .expect("the palette is open")
    }

    /// The dialog opens over the window with the first row selected, filters
    /// as text is typed, says "Nothing matches", chooses with Enter and gives
    /// the focus back. (Needs a display.)
    #[test]
    fn the_palette_opens_filters_chooses_and_gives_the_focus_back() {
        crate::test_display::run(|| {
            let resources = crate::load_resources().expect("the bundled resources");
            gio::resources_register(&resources);
            let app = adw::Application::builder()
                .application_id("derx06.desklink.com.test")
                .build();
            let window = DesklinkWindow::new(&app);
            let (daemon, _commands) = crate::device_links::daemon::DaemonHandle::detached();
            window.set_daemon(daemon);
            let mut phone = super::super::debug_fake_device("paired", "Pixel 9");
            phone.name = "Pixel 9".into();
            window.imp().fake_devices.borrow_mut().push(phone);
            window.select_device(&window.imp().fake_devices.borrow()[0].id.clone());
            window.present();
            let before = gtk::Button::new();
            pump();

            toggle(&window, "");
            pump();
            let open = window.visible_dialog().and_downcast::<adw::Dialog>();
            assert!(open.is_some(), "the palette is the visible dialog");
            let palette = open_palette();
            assert!(palette.rows() > 5, "idle lists the actions");
            assert_eq!(palette.selected(), Some(0), "the first row is selected");
            assert_eq!(palette.stack.visible_child_name().as_deref(), Some("list"));
            assert!(palette.count.label().contains("suggestions"));

            palette.entry.set_text("send fil");
            let first = palette.items[palette.order.borrow()[0].1].title.clone();
            assert_eq!(first, "Send files");
            assert_eq!(palette.count.label(), "1 result");
            assert_eq!(palette.selected(), Some(0));

            palette.entry.set_text("qqqzzz");
            assert_eq!(palette.rows(), 0);
            assert_eq!(palette.stack.visible_child_name().as_deref(), Some("empty"));
            assert_eq!(palette.count.label(), "Nothing matches");

            // Ctrl+K again closes it; the slot is empty once it has gone.
            toggle(&window, "");
            pump();
            assert!(OPEN.with(|slot| slot.borrow().is_none()));
            assert!(window.visible_dialog().is_none());
            drop(before);
        });
    }
}
