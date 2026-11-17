//! The Transfers page: a drop zone and Choose Files… to send to the phone,
//! then cards — what is moving right now, with a way to stop it, and the
//! files received or sent in the last 30 days, each with a preview (a
//! thumbnail, a video's poster frame, or a typed icon) that opens in the
//! viewer.
//!
//! What a moving card says is decided by [`subtitle`] and [`fraction`],
//! which have no GTK in them and are unit-tested. Moving cards are updated
//! in place four times a second, so a progress bar never flickers and a
//! click on Cancel never lands on a card that was just rebuilt. Thumbnails
//! are made on a worker thread ([`crate::thumbnails`]).
//!
//! Layout: this file holds the rows the page reads, the debug rows, the grid
//! helpers and the [`Page`]; `model` is what a card says (no widgets), `live` the
//! moving cards and their timer, `failures` the failed transfers, `cards` the
//! received and sent files with previews. No cycles.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use adw::subclass::prelude::*;

use super::ui_kit::{self, ButtonKind, Status, Tone};
use super::{ellipsized, text, DesklinkWindow};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::DeviceView;
use crate::device_links::transfer_log::LoggedFile;
use crate::device_links::webrtc::transfer_board::TransferRow;
use crate::thumbnails::FileKind;

mod cards;
mod failures;
mod live;
mod model;
use cards::*;
use failures::*;
use live::*;
use model::*;

// What the rest of the module's parent uses (everything else stays inside).
pub(super) use cards::{fill_preview, forget_pictures, size_and_time, type_icon};
pub(super) use model::{fraction, glyph_for, Filter};

/// Often enough that a progress bar looks alive, rarely enough to cost nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

// Debug builds only: transfers injected by the `e2e` action, for screenshots.
#[cfg(debug_assertions)]
thread_local! {
    pub(crate) static FAKE_ROWS: RefCell<Vec<TransferRow>> = const { RefCell::new(Vec::new()) };
}

/// Whether `row` is `device_id`'s (a row with no phone is every phone's).
/// The one predicate for every surface that shows transfers of the selected
/// phone: Home, the Files page, the sidebar count.
pub(super) fn row_belongs(row: &TransferRow, device_id: &str) -> bool {
    row.device_id == device_id || row.device_id.is_empty()
}

/// `rows` that belong to `device_id`.
pub(super) fn rows_for(device_id: &str, rows: Vec<TransferRow>) -> Vec<TransferRow> {
    rows.into_iter()
        .filter(|row| row_belongs(row, device_id))
        .collect()
}

/// The transfers moving now, injected ones included in debug builds.
pub(super) fn rows(daemon: &DaemonHandle) -> Vec<TransferRow> {
    // Fakes first: one that names a lane is put on the real board.
    #[cfg(debug_assertions)]
    let fakes = debug_rows();
    #[allow(unused_mut)]
    let mut rows = daemon.transfer_rows();
    #[cfg(debug_assertions)]
    rows.extend(fakes);
    rows
}

/// Debug builds, for screenshots: what a `fake-transfer` name can say.
/// `paused:NAME` is a transfer whose link dropped; `fast:NAME`, `secure:NAME`
/// and `internet:NAME` are on that lane (they join the real board, so the row
/// says the lane's words); `failed:NAME|reason` is not moving at all (see
/// [`debug_failures`]).
#[cfg(debug_assertions)]
fn debug_rows() -> Vec<TransferRow> {
    use crate::device_links::webrtc::transfer_board as board;
    let fakes = FAKE_ROWS.with(|fake| fake.borrow().clone());
    let mut shown = Vec::new();
    for mut row in fakes {
        if row.name.starts_with("failed:") {
            continue;
        }
        if let Some(name) = row.name.strip_prefix("paused:") {
            row.name = name.to_string();
            row.paused = true;
        }
        let lane = [
            (
                "fast:",
                crate::device_links::webrtc::transfer_manager::words::fast_lane(),
            ),
            (
                "secure:",
                crate::device_links::webrtc::transfer_manager::words::over_webrtc(),
            ),
            (
                "internet:",
                crate::device_links::webrtc::transfer_manager::words::through_a_relay(),
            ),
        ]
        .into_iter()
        .find(|(prefix, _)| row.name.starts_with(prefix));
        if let Some((prefix, words)) = lane {
            row.name = row.name[prefix.len()..].to_string();
            if board::lane_of(&row.id).is_none() {
                board::begin(row.clone());
                board::set_lane(&row.id, Some(words));
            }
            // The board lists it now.
            continue;
        }
        shown.push(row);
    }
    shown
}

/// Debug builds: the `failed:NAME|reason` fakes, as failures of `device`.
#[cfg(debug_assertions)]
fn debug_failures(device: &str) -> Vec<Failure> {
    FAKE_ROWS.with(|fake| {
        fake.borrow()
            .iter()
            .filter_map(|row| {
                let (name, reason) = row.name.strip_prefix("failed:")?.split_once('|')?;
                Some(Failure {
                    id: row.id.clone(),
                    device_id: device.to_string(),
                    name: name.to_string(),
                    incoming: row.incoming,
                    reason: reason_words(&reason.replace('_', " ")),
                    retry: Some(std::path::PathBuf::from("/tmp").join(name)),
                })
            })
            .collect()
    })
}

/// What the page asked the presence worker for, and what it asked it with.
struct Pending {
    entries: Vec<LoggedFile>,
    device: String,
    filter: Filter,
    /// Whether the device has any entry at all (before the filter).
    any: bool,
}

#[derive(Debug)]
pub(super) struct Page {
    /// The transfers part of the Files page: Send files, what moves now, what
    /// failed and the history. Not scrolled: Files scrolls it with the Bucket
    /// above it.
    pub(super) content: gtk::Box,
    hint: gtk::Label,
    choose: gtk::Button,
    /// The day groups, each a heading and a grid of cards.
    history: gtk::Box,
    empty: gtk::Box,
    empty_title: gtk::Label,
    empty_body: gtk::Label,
    filter: Rc<Cell<Filter>>,
    history_key: RefCell<String>,
    /// The presence check asked for last; a reply to an older one is
    /// dropped. With it, the entries and device name it was asked for.
    generation: Cell<u64>,
    pending: RefCell<Pending>,
    window: glib::WeakRef<DesklinkWindow>,
    /// The selected device: id and name.
    device: Rc<RefCell<(String, String)>>,
    /// Arms the progress timer (see [`follow`]).
    kick: Kick,
}

impl std::fmt::Debug for Pending {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Pending")
    }
}

/// The function that starts the progress timer.
type Start = Rc<dyn Fn()>;

/// What starts the progress timer, once the daemon is known.
#[derive(Clone, Default)]
struct Kick(Rc<RefCell<Option<Start>>>);

impl std::fmt::Debug for Kick {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Kick")
    }
}

impl Kick {
    fn set(&self, start: Start) {
        self.0.replace(Some(start));
    }

    fn run(&self) {
        if let Some(start) = self.0.borrow().as_ref() {
            start();
        }
    }
}

/// Cards a line in the grids: one to five, as wide allows (eight on a big
/// window, `apply_layout`).
pub(super) fn grid() -> gtk::FlowBox {
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(5)
        .column_spacing(14)
        .row_spacing(14)
        .valign(gtk::Align::Start)
        .build();
    grid.set_widget_name("card-grid");
    grid
}

/// Adds `card` to `grid`; Tab and the arrow keys go card to card, not to
/// the FlowBox's wrapper around each.
pub(super) fn add_card(grid: &gtk::FlowBox, card: &impl IsA<gtk::Widget>) {
    grid.append(card);
    if let Some(child) = card.as_ref().parent() {
        child.set_focusable(false);
    }
}

/// An empty state of the kit's shape: a 28 glyph in a 72 circle, a title,
/// up to three lines of words, centred, at most 320 wide.
pub(super) fn empty_state(
    icon: &str,
    title: &str,
    body: &str,
) -> (gtk::Box, gtk::Label, gtk::Label) {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .halign(gtk::Align::Center)
        .css_classes(["files-empty"])
        .build();
    let circle = gtk::CenterBox::builder()
        .halign(gtk::Align::Center)
        .css_classes(["files-empty-art"])
        .center_widget(
            &gtk::Image::builder()
                .icon_name(icon)
                .pixel_size(28)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        )
        .build();
    column.append(&circle);
    let title = gtk::Label::builder()
        .label(title)
        .wrap(true)
        .justify(gtk::Justification::Center)
        .css_classes(["files-empty-title"])
        .accessible_role(gtk::AccessibleRole::Heading)
        .build();
    let body = gtk::Label::builder()
        .label(body)
        .wrap(true)
        .max_width_chars(36)
        .justify(gtk::Justification::Center)
        .css_classes(["dl-secondary"])
        .build();
    // At most 320 wide, whatever the words.
    let clamp = adw::Clamp::builder()
        .maximum_size(320)
        .tightening_threshold(320)
        .build();
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    words.append(&title);
    words.append(&body);
    clamp.set_child(Some(&words));
    column.append(&clamp);
    (column, title, body)
}

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        crate::thumbnails::set_delivery(deliver);
        WINDOW.with(|window| window.replace(win.downgrade()));
        let filter = Rc::new(Cell::new(Filter::All));

        // Title, Send files and the page's one overflow.
        let head = gtk::Box::builder().spacing(8).build();
        let title = ui_kit::section_header("Sent and received");
        title.set_hexpand(true);
        title.set_valign(gtk::Align::Center);
        head.append(&title);
        let choose = ui_kit::button(
            ButtonKind::Primary,
            "Send files…",
            Some("desklink-upload-file-symbolic"),
        );
        choose.set_valign(gtk::Align::Center);
        head.append(&choose);
        head.append(&more_menu());
        let hint = super::wrapped("", &["dl-secondary"]);

        let failures = Failures::new();
        let moving = grid();
        moving.set_visible(false);
        moving.set_max_children_per_line(1);
        // One a line at any width: not one of the card grids.
        moving.set_widget_name("moving");
        moving.set_homogeneous(false);
        moving.set_row_spacing(8);
        moving.update_property(&[gtk::accessible::Property::Label("Moving now")]);

        // Filter chips: one choice, scrolling sideways when they do not fit.
        let chips = gtk::Box::builder().spacing(8).build();
        chips.update_property(&[gtk::accessible::Property::Label("Show")]);
        let mut first: Option<gtk::ToggleButton> = None;
        for choice in Filter::CHIPS {
            let chip = ui_kit::chip(choice.label(), None, choice == Filter::All);
            match &first {
                Some(first) => chip.set_group(Some(first)),
                None => first = Some(chip.clone()),
            }
            let (filter, window) = (Rc::clone(&filter), win.downgrade());
            chip.connect_toggled(move |chip| {
                if chip.is_active() && filter.replace(choice) != choice {
                    if let Some(window) = window.upgrade() {
                        window.refresh_soon();
                    }
                }
            });
            chips.append(&chip);
        }
        let chips = gtk::ScrolledWindow::builder()
            .child(&chips)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .build();

        let history = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .build();
        history.update_property(&[gtk::accessible::Property::Label("Recent transfers")]);
        let (empty, empty_title, empty_body) =
            empty_state("desklink-folder-symbolic", "No files yet", "");
        empty.set_margin_top(24);
        empty.set_margin_bottom(24);

        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_start(16)
            .margin_end(16)
            .build();
        page.append(&head);
        page.append(&hint);
        page.append(&failures.list);
        page.append(&moving);
        page.append(&chips);
        page.append(&history);
        page.append(&empty);

        let device: Rc<RefCell<(String, String)>> = Rc::default();
        {
            let (window, device) = (win.downgrade(), Rc::clone(&device));
            choose.connect_clicked(move |button| {
                if let Some(daemon) = window.upgrade().and_then(|window| window.daemon()) {
                    super::pick_and_send_files(&daemon, &device.borrow().0, button);
                }
            });
        }
        let kick = Kick::default();
        if let Some(daemon) = win.daemon() {
            kick.set(follow(
                &moving,
                daemon,
                Rc::clone(&device),
                Rc::clone(&failures),
            ));
        } else {
            // The daemon is handed over after construction.
            let (window, moving, device, kick) = (
                win.downgrade(),
                moving.clone(),
                Rc::clone(&device),
                kick.clone(),
            );
            glib::idle_add_local_once(move || {
                if let Some(daemon) = window.upgrade().and_then(|window| window.daemon()) {
                    kick.set(follow(&moving, daemon, device, failures));
                }
            });
        }
        Self {
            content: page,
            hint,
            choose,
            history,
            empty,
            empty_title,
            empty_body,
            filter,
            history_key: RefCell::default(),
            generation: Cell::new(0),
            pending: RefCell::new(Pending {
                entries: Vec::new(),
                device: String::new(),
                filter: Filter::All,
                any: false,
            }),
            window: win.downgrade(),
            device,
            kick,
        }
    }

    /// Starts following the transfers if something moves or the page shows
    /// (the window calls it on every tick; nothing runs between).
    pub(super) fn kick(&self) {
        self.kick.run();
    }

    /// Makes `widget` (the whole Files page) take dropped files to send to
    /// the selected phone, once it is connected. The Bucket section has its
    /// own drop target above this one, so drops over it go to the Bucket.
    pub(super) fn accept_drops_on(&self, widget: &impl IsA<gtk::Widget>) {
        let (window, device) = (self.window.clone(), Rc::clone(&self.device));
        super::accept_dropped_files(widget, move || {
            let window = window.upgrade()?;
            let daemon = window.daemon()?;
            let id = device.borrow().0.clone();
            let ready = window
                .known_devices(&daemon)
                .into_iter()
                .any(|known| known.id == id && known.feature_transport_ready);
            ready.then_some((daemon, id))
        });
    }

    /// Empties the history cards (and their pictures); the next update
    /// draws them again.
    pub(super) fn release_cards(&self) {
        while let Some(child) = self.history.first_child() {
            self.history.remove(&child);
        }
        self.invalidate();
    }

    /// Redraws the history on its next update, e.g. after Remove from List.
    pub(super) fn invalidate(&self) {
        self.history_key.replace(String::new());
    }

    pub(super) fn update(
        &self,
        _win: &DesklinkWindow,
        _daemon: &DaemonHandle,
        device: &DeviceView,
    ) {
        self.device
            .replace((device.id.clone(), device.name.clone()));
        self.kick();
        self.hint.set_label(&format!(
            "Or drop files here to send them to {}. They land in Downloads on the phone.",
            device.name
        ));
        self.choose.set_sensitive(device.feature_transport_ready);
        self.choose
            .set_tooltip_text(super::unavailable_reason(device).as_deref());

        // The 50 newest are listed, and only they decide a redraw. Whether
        // each is still there is asked of the worker (a stat on a network
        // mount can hang), and the grid is drawn when it answers.
        let filter = self.filter.get();
        let all: Vec<_> = crate::device_links::transfer_log::recent()
            .into_iter()
            .filter(|entry| entry.device_id == device.id)
            .collect();
        let any = !all.is_empty();
        let entries: Vec<_> = all
            .into_iter()
            .filter(|entry| filter.matches(entry))
            .take(50)
            .collect();
        let minute = super::clock_time(glib::DateTime::now_local().ok());
        let key = serde_json::to_string(&(&entries, &device.name, minute, filter.label(), any))
            .unwrap_or_default();
        if *self.history_key.borrow() == key {
            return;
        }
        self.history_key.replace(key);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let paths = entries.iter().map(|entry| entry.path.clone()).collect();
        self.pending.replace(Pending {
            entries,
            device: device.name.clone(),
            filter,
            any,
        });
        crate::thumbnails::check_presence(generation, paths);
    }

    /// Draws the history once the worker says which files are still there.
    fn draw(&self, generation: u64, files: &[crate::thumbnails::Presence]) {
        let Some(win) = self.window.upgrade() else {
            return;
        };
        if generation != self.generation.get() {
            return;
        }
        let (entries, device, filter, any) = {
            let pending = self.pending.borrow();
            (
                pending.entries.clone(),
                pending.device.clone(),
                pending.filter,
                pending.any,
            )
        };
        if files.len() != entries.len() {
            return;
        }
        // Thumbnails kept in memory: only those still listed.
        let keys: HashSet<&String> = files.iter().filter_map(|file| file.key.as_ref()).collect();
        SHOWN.with(|shown| shown.borrow_mut().retain(|key, _| keys.contains(key)));
        while let Some(child) = self.history.first_child() {
            self.history.remove(&child);
        }
        self.empty.set_visible(entries.is_empty());
        let (title, body) = empty_words(filter, any, &device);
        self.empty_title.set_label(&title);
        self.empty_body.set_label(&body);
        // What the viewer pages through: the files still there.
        let viewable: Rc<Vec<LoggedFile>> = Rc::new(
            entries
                .iter()
                .zip(files)
                .filter(|(_, file)| file.exists)
                .map(|(entry, _)| entry.clone())
                .collect(),
        );
        let Ok(now) = glib::DateTime::now_local() else {
            return;
        };
        let cards = entries.iter().zip(files);
        for (day, members) in day_groups(cards.map(|(entry, file)| (entry.ts, (entry, file))), &now)
        {
            let group = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .build();
            group.append(&ui_kit::section_header(&day));
            let cards = grid();
            cards.set_max_children_per_line(win.card_columns(5));
            cards.update_property(&[gtk::accessible::Property::Label(&day)]);
            for (entry, file) in members {
                let card: gtk::Widget = if file.exists {
                    file_card(&win, entry, file.key.as_deref(), &device, &viewable).upcast()
                } else {
                    missing_card(&win, entry, &device).upcast()
                };
                add_card(&cards, &card);
            }
            group.append(&cards);
            self.history.append(&group);
        }
    }
}

/// The page's overflow: Open Downloads.
fn more_menu() -> gtk::MenuButton {
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let popover = gtk::Popover::builder().child(&menu).build();
    let open = ui_kit::button(ButtonKind::Text, "Open Downloads", None);
    open.set_halign(gtk::Align::Start);
    {
        let popover = popover.downgrade();
        open.connect_clicked(move |button| {
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
            let Some(dir) = crate::device_links::transfer_log::downloads_dir() else {
                return;
            };
            gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(
                button.root().and_downcast_ref::<gtk::Window>(),
                gio::Cancellable::NONE,
                |result| {
                    if let Err(error) = result {
                        eprintln!("[UI] Could not open Downloads: {error}");
                    }
                },
            );
        });
    }
    menu.append(&open);
    let button = gtk::MenuButton::builder()
        .icon_name("desklink-more-vert-symbolic")
        .popover(&popover)
        .valign(gtk::Align::Center)
        .css_classes(["dl-icon-btn"])
        .tooltip_text("More")
        .build();
    button.update_property(&[gtk::accessible::Property::Label("More about files")]);
    button
}

#[cfg(test)]
mod tests;
