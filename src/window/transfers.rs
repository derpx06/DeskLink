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

/// Often enough that a progress bar looks alive, rarely enough to cost nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

// Debug builds only: transfers injected by the `e2e` action, for screenshots.
#[cfg(debug_assertions)]
thread_local! {
    pub(crate) static FAKE_ROWS: RefCell<Vec<TransferRow>> = const { RefCell::new(Vec::new()) };
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
            ("fast:", "fast, over Wi-Fi"),
            ("secure:", "over the secure link"),
            ("internet:", "through the internet (slower)"),
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
                    reason: reason_words(reason),
                    retry: Some(std::path::PathBuf::from("/tmp").join(name)),
                })
            })
            .collect()
    })
}

/// "Sending to Pixel · 4.2 MB of 40 MB".
fn subtitle(row: &TransferRow, device: &str) -> String {
    if row.preparing {
        return format!("Preparing to send to {device}…");
    }
    if let Some(note) = crate::device_links::webrtc::transfer_board::note(&row.id) {
        return note;
    }
    if row.paused {
        return format!("Paused — resumes when {device} reconnects");
    }
    let direction = if row.incoming {
        format!("Receiving from {device}")
    } else {
        format!("Sending to {device}")
    };
    // The lane comes right after the direction: the line wraps on the card,
    // and it used to be the tail that an ellipsis cut off.
    let lane = crate::device_links::webrtc::transfer_board::lane_of(&row.id)
        .map(|lane| format!(" · {lane}"))
        .unwrap_or_default();
    format!(
        "{direction}{lane} · {} of {}",
        crate::bucket_progress::size_words(row.done),
        crate::bucket_progress::size_words(row.total)
    )
}

/// A rate and a time left are shown only while bytes are meant to be moving: a
/// row that waits (its note says why, or its device is away) keeps no stale
/// "3 MB/s · 5 s left" from before it stopped.
fn shows_rate(row: &TransferRow, held: bool) -> bool {
    !row.preparing && !row.paused && !held
}

/// How far along, 0..=1; `None` while there is nothing to measure against.
pub(super) fn fraction(row: &TransferRow) -> Option<f64> {
    if row.preparing || row.total == 0 {
        return None;
    }
    Some((row.done as f64 / row.total as f64).clamp(0.0, 1.0))
}

/// What a moving row is doing, for its pill. `held` is a note saying why it
/// waits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RowState {
    /// A file being copied and hashed.
    Preparing,
    /// Bytes are meant to be moving.
    Moving,
    /// The connection dropped; it resumes by itself.
    Paused,
    /// Waiting for something else (a slot, space): the note says what.
    Waiting,
}

fn row_state(row: &TransferRow, held: bool) -> RowState {
    if row.preparing {
        RowState::Preparing
    } else if row.paused {
        RowState::Paused
    } else if held {
        RowState::Waiting
    } else {
        RowState::Moving
    }
}

impl RowState {
    /// The pill's state and word; a moving row has none (its percent says it).
    fn pill(self) -> Option<(Status, &'static str)> {
        match self {
            Self::Moving => None,
            Self::Preparing => Some((Status::Idle, "Preparing")),
            Self::Paused => Some((Status::Idle, "Paused")),
            Self::Waiting => Some((Status::Warn, "Waiting")),
        }
    }
}

/// The kinds of file the chips tell apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    Image,
    Video,
    Audio,
    Pdf,
    Document,
    Archive,
    Other,
}

/// What a file is, from its name.
fn class_of(name: &str) -> Class {
    use crate::thumbnails::{kind_of, FileKind};
    match kind_of(std::path::Path::new(name)) {
        FileKind::Image => return Class::Image,
        FileKind::Video => return Class::Video,
        FileKind::Pdf => return Class::Pdf,
        FileKind::Other => {}
    }
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_lowercase)
        .unwrap_or_default();
    match extension.as_str() {
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" => Class::Archive,
        "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ods" | "csv" | "ppt"
        | "pptx" | "odp" | "epub" => Class::Document,
        "mp3" | "flac" | "ogg" | "opus" | "wav" | "m4a" | "aac" => Class::Audio,
        _ => Class::Other,
    }
}

/// The kit glyph for a file of this name.
pub(super) fn glyph_for(name: &str) -> &'static str {
    match class_of(name) {
        Class::Image => "desklink-image-symbolic",
        Class::Video => "desklink-movie-symbolic",
        Class::Audio => "desklink-music-note-symbolic",
        Class::Pdf => "desklink-picture-as-pdf-symbolic",
        Class::Archive => "desklink-folder-zip-symbolic",
        Class::Document | Class::Other => "desklink-description-symbolic",
    }
}

/// Which files the history shows: one choice, as the chips.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Filter {
    #[default]
    All,
    Received,
    Sent,
    Images,
    Documents,
    Archives,
}

impl Filter {
    const CHIPS: [Filter; 6] = [
        Self::All,
        Self::Received,
        Self::Sent,
        Self::Images,
        Self::Documents,
        Self::Archives,
    ];

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Received => "Received",
            Self::Sent => "Sent",
            Self::Images => "Images",
            Self::Documents => "Documents",
            Self::Archives => "Archives",
        }
    }

    fn matches(self, entry: &LoggedFile) -> bool {
        match self {
            Self::All => true,
            Self::Received => entry.incoming,
            Self::Sent => !entry.incoming,
            Self::Images => class_of(&entry.name) == Class::Image,
            // A PDF is a document.
            Self::Documents => matches!(class_of(&entry.name), Class::Document | Class::Pdf),
            Self::Archives => class_of(&entry.name) == Class::Archive,
        }
    }
}

/// The empty state's title and words: nothing at all yet, or nothing under
/// this chip.
fn empty_words(filter: Filter, any: bool, device: &str) -> (String, String) {
    if filter == Filter::All || !any {
        (
            "No files yet".to_string(),
            format!(
                "Files you send to or receive from {device} show up here for 30 days. Drop files \
                 on this page, or press Send files."
            ),
        )
    } else {
        let none = match filter {
            Filter::Received => "No received files",
            Filter::Sent => "No sent files",
            Filter::Images => "No images",
            Filter::Documents => "No documents",
            Filter::Archives => "No archives",
            Filter::All => "No files yet",
        };
        (
            none.to_string(),
            "Nothing matches this filter in the last 30 days. Choose All to see everything."
                .to_string(),
        )
    }
}

/// Files in the order given, grouped under "Today", "Yesterday", a weekday
/// or a date; a day that comes again later in the list starts a new group.
fn day_groups<T>(
    items: impl IntoIterator<Item = (u64, T)>,
    now: &glib::DateTime,
) -> Vec<(String, Vec<T>)> {
    let mut groups: Vec<(String, Vec<T>)> = Vec::new();
    for (ts, item) in items {
        let day = glib::DateTime::from_unix_local(ts as i64)
            .ok()
            .map(|when| super::history::day_label(now, &when))
            .unwrap_or_default();
        match groups.last_mut() {
            Some((last, members)) if *last == day => members.push(item),
            _ => groups.push((day, vec![item])),
        }
    }
    groups
}

/// "3.1 MB/s · 12 s left"; empty until there is a rate to speak of.
fn rate_text(bytes_per_second: f64, remaining: u64) -> String {
    if bytes_per_second < 1.0 {
        return String::new();
    }
    let speed = format!(
        "{}/s",
        crate::bucket_progress::size_words(bytes_per_second as u64)
    );
    let seconds = (remaining as f64 / bytes_per_second).ceil() as u64;
    let left = match seconds {
        0 => return speed,
        1..=59 => format!("{seconds} s left"),
        60..=3599 => format!("{} min left", seconds.div_ceil(60)),
        _ => format!("{} h left", seconds.div_ceil(3600)),
    };
    format!("{speed} · {left}")
}

struct RowWidgets {
    row: gtk::Box,
    subtitle: gtk::Label,
    percent: gtk::Label,
    bar: gtk::ProgressBar,
    /// Paused, waiting or preparing, in a word (hidden while it moves).
    pill: gtk::Box,
    pill_state: Cell<Option<RowState>>,
    /// When `done` was last sampled, and the smoothed rate since.
    sample: Cell<(std::time::Instant, u64)>,
    rate: Cell<f64>,
}

/// A send or receive that ended badly: kept on the page until it is tried
/// again or dismissed, so the reason is not lost with a toast.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Failure {
    id: String,
    device_id: String,
    name: String,
    incoming: bool,
    reason: String,
    retry: Option<std::path::PathBuf>,
}

/// How many failures the page keeps.
const MAX_FAILURES: usize = 5;

/// The failures part of the page, and how far along the board's outcomes it
/// has read.
struct Failures {
    list: gtk::Box,
    kept: RefCell<Vec<Failure>>,
    seen: Cell<u64>,
    /// What is drawn now, so the list is rebuilt only when it changes.
    drawn: RefCell<Vec<Failure>>,
}

/// "Pixel 9 ran out of space." as a sentence: the first letter up, a full
/// stop; "Something went wrong." when nothing was said.
fn reason_words(reason: &str) -> String {
    let reason = reason.trim().trim_end_matches('.');
    let mut chars = reason.chars();
    match chars.next() {
        None => "Something went wrong.".to_string(),
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
    }
}

impl Failures {
    fn new() -> Rc<Self> {
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .visible(false)
            .build();
        list.update_property(&[gtk::accessible::Property::Label("Failed transfers")]);
        Rc::new(Self {
            list,
            kept: RefCell::default(),
            // Only what fails from now on; earlier outcomes were toasts.
            seen: Cell::new(crate::device_links::webrtc::transfer_board::ended_since(u64::MAX).1),
            drawn: RefCell::default(),
        })
    }

    /// Reads the outcomes since last time and redraws when the list changed.
    fn sync(self: &Rc<Self>, daemon: &DaemonHandle, device: &str) {
        use crate::device_links::webrtc::transfer_board::{ended_since, Outcome};
        let (news, seq) = ended_since(self.seen.get());
        self.seen.set(seq);
        {
            let mut kept = self.kept.borrow_mut();
            for done in news {
                if let Outcome::Failed(reason) = done.outcome {
                    kept.retain(|failure| failure.id != done.id);
                    kept.push(Failure {
                        id: done.id,
                        device_id: done.device_id,
                        name: done.name,
                        incoming: done.incoming,
                        reason: reason_words(&reason),
                        retry: done.retry,
                    });
                    if kept.len() > MAX_FAILURES {
                        kept.remove(0);
                    }
                }
            }
        }
        #[allow(unused_mut)]
        let mut shown: Vec<Failure> = self
            .kept
            .borrow()
            .iter()
            .filter(|failure| failure.device_id == device)
            .cloned()
            .collect();
        #[cfg(debug_assertions)]
        shown.extend(debug_failures(device));
        if *self.drawn.borrow() == shown {
            return;
        }
        while let Some(child) = self.list.first_child() {
            self.list.remove(&child);
        }
        for failure in shown.iter().rev() {
            self.list.append(&failed_row(self, daemon, failure));
        }
        self.list.set_visible(!shown.is_empty());
        self.drawn.replace(shown);
    }

    fn forget(&self, id: &str) {
        self.kept.borrow_mut().retain(|failure| failure.id != id);
        self.drawn.borrow_mut().clear();
    }
}

/// A failed transfer: what, why, and Try again when the file is still known.
fn failed_row(failures: &Rc<Failures>, daemon: &DaemonHandle, failure: &Failure) -> gtk::Box {
    let row = gtk::Box::builder()
        .spacing(12)
        .css_classes(["files-row", "failed"])
        .build();
    row.append(&ui_kit::icon_square("desklink-error-symbolic", Tone::Error));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let verb = if failure.incoming { "receive" } else { "send" };
    let name = ellipsized(&failure.name, &["dl-mono", "files-name"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    words.append(&name);
    words.append(&super::wrapped(
        &format!("Could not {verb} it. {}", failure.reason),
        &["dl-secondary"],
    ));
    row.append(&words);
    if let (Some(path), false) = (&failure.retry, failure.device_id.is_empty()) {
        let again = ui_kit::button(ButtonKind::Tonal, "Try again", None);
        again.set_halign(gtk::Align::Start);
        again.set_margin_top(6);
        again.set_tooltip_text(Some("Send this file again"));
        again.update_property(&[gtk::accessible::Property::Label(&format!(
            "Try sending {} again",
            failure.name
        ))]);
        let (failures, daemon, device, path, id) = (
            Rc::clone(failures),
            daemon.clone(),
            failure.device_id.clone(),
            path.clone(),
            failure.id.clone(),
        );
        again.connect_clicked(move |_| {
            daemon.send(DaemonCommand::SendFile(device.clone(), path.clone()));
            failures.forget(&id);
        });
        words.append(&again);
    }
    let dismiss = ui_kit::icon_button("desklink-close-symbolic", "Dismiss");
    dismiss.set_valign(gtk::Align::Start);
    dismiss.update_property(&[gtk::accessible::Property::Label(&format!(
        "Dismiss the failed {verb} of {}",
        failure.name
    ))]);
    {
        let (failures, id) = (Rc::clone(failures), failure.id.clone());
        dismiss.connect_clicked(move |_| failures.forget(&id));
    }
    row.append(&dismiss);
    row.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}: could not {verb}. {}",
        failure.name, failure.reason
    ))]);
    row
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

/// Cards a line in the grids: one to five, as wide allows.
pub(super) fn grid() -> gtk::FlowBox {
    gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(5)
        .column_spacing(14)
        .row_spacing(14)
        .valign(gtk::Align::Start)
        .build()
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
            let Some(dir) = glib::user_special_dir(glib::UserDirectory::Downloads) else {
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

/// "2.4 MB · 12 min ago".
pub(super) fn size_and_time(entry: &LoggedFile) -> String {
    format!(
        "{} · {}",
        crate::bucket_progress::size_words(entry.size),
        when(entry)
    )
}

fn when(entry: &LoggedFile) -> String {
    glib::DateTime::from_unix_local(entry.ts as i64)
        .ok()
        .zip(glib::DateTime::now_local().ok())
        .map(|(when, now)| super::history::time_label(&now, &when))
        .unwrap_or_default()
}

/// "From Pixel 9" or "Sent to Pixel 9", with its arrow.
fn direction(entry: &LoggedFile, device: &str) -> gtk::Box {
    let line = gtk::Box::builder().spacing(6).build();
    line.append(
        &gtk::Image::builder()
            .icon_name(if entry.incoming {
                "desklink-arrow-downward-symbolic"
            } else {
                "desklink-arrow-upward-symbolic"
            })
            .pixel_size(12)
            .css_classes(["files-direction"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    line.append(&ellipsized(
        &if entry.incoming {
            format!("From {device}")
        } else {
            format!("Sent to {device}")
        },
        &["dl-caption"],
    ));
    line
}

/// A card's name, size and time, and direction, under its preview.
fn card_words(entry: &LoggedFile, device: &str) -> gtk::Box {
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["file-card-words"])
        .build();
    let name = ellipsized(&entry.name, &["file-name"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    name.set_max_width_chars(16);
    words.append(&name);
    words.append(&ellipsized(
        &size_and_time(entry),
        &["dl-secondary", "numeric"],
    ));
    words.append(&direction(entry, device));
    // Where it went, so nobody has to look for it.
    if entry.incoming {
        words.append(&ellipsized(
            &format!("Saved in {}", super::saved_in(&entry.path)),
            &["dl-secondary"],
        ));
    }
    words
}

/// The preview area, fixed in size; what it shows is laid over it so a big
/// picture cannot widen the card.
fn preview_area() -> (gtk::Overlay, gtk::Box) {
    let base = gtk::Box::builder()
        .width_request(176)
        .height_request(132)
        .css_classes(["file-preview"])
        .build();
    let overlay = gtk::Overlay::builder()
        .child(&base)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["file-preview-clip"])
        .build();
    (overlay, base)
}

/// A typed glyph for a file with no picture: a PDF, a document.
pub(super) fn type_icon(name: &str) -> gtk::Box {
    let icon = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    icon.append(
        &gtk::Image::builder()
            .icon_name(glyph_for(name))
            .pixel_size(40)
            .css_classes(["files-glyph"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("file")
        .to_uppercase();
    icon.append(&text(&extension, &["dl-caption", "type-label"]));
    icon
}

/// A card waiting for its thumbnail.
type Waiter = (
    glib::WeakRef<gtk::Overlay>,
    glib::WeakRef<gtk::Box>,
    String,
    FileKind,
);

thread_local! {
    /// Thumbnails made this run, by cache key: a page redraw shows them at
    /// once instead of loading them again. Pruned to the listed files.
    static SHOWN: RefCell<HashMap<String, (gtk::gdk::Texture, Option<Duration>)>> =
        RefCell::default();
    /// Keys with no preview to be had: the typed icon, without asking again.
    static FAILED: RefCell<HashSet<String>> = RefCell::default();
    /// Cards waiting, by key. A key here has been asked for once already, so
    /// a redraw while it is being made never queues it twice.
    static WAITING: RefCell<HashMap<String, Vec<Waiter>>> = RefCell::default();
    static WINDOW: RefCell<glib::WeakRef<DesklinkWindow>> = RefCell::default();
}

/// Lets go of every decoded preview (a window nobody is looking at does not
/// keep pictures; the next look decodes the few it shows, from the disk
/// cache).
pub(super) fn forget_pictures() {
    SHOWN.with(|shown| shown.borrow_mut().clear());
}

/// The thumbnail worker's replies, on the main thread.
fn deliver(reply: crate::thumbnails::Reply) {
    use crate::thumbnails::Reply;
    match reply {
        Reply::Presence { generation, files } => {
            if let Some(window) = WINDOW.with(|window| window.borrow().upgrade()) {
                if let Some(page) = window.imp().transfers.borrow().as_ref() {
                    page.draw(generation, &files);
                }
            }
        }
        Reply::Thumbnail {
            key,
            texture,
            duration,
        } => {
            SHOWN.with(|shown| {
                shown
                    .borrow_mut()
                    .insert(key.clone(), (texture.clone(), duration))
            });
            for (overlay, shimmer, _, kind) in waiters(&key) {
                if let Some(shimmer) = shimmer.upgrade() {
                    overlay.remove_overlay(&shimmer);
                }
                show_thumbnail(&overlay, &texture, duration, kind);
            }
        }
        Reply::Failed { key } => {
            FAILED.with(|failed| failed.borrow_mut().insert(key.clone()));
            for (overlay, shimmer, name, _) in waiters(&key) {
                if let Some(shimmer) = shimmer.upgrade() {
                    overlay.remove_overlay(&shimmer);
                }
                overlay.add_overlay(&type_icon(&name));
            }
        }
    }
}

/// The cards still on screen that waited for `key`.
fn waiters(key: &str) -> Vec<(gtk::Overlay, glib::WeakRef<gtk::Box>, String, FileKind)> {
    WAITING
        .with(|waiting| waiting.borrow_mut().remove(key))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(overlay, shimmer, name, kind)| {
            Some((overlay.upgrade()?, shimmer, name, kind))
        })
        .collect()
}

/// Shows a thumbnail in `overlay`: a shimmer now, the picture once the
/// worker has made it (or a typed icon if it cannot be).
pub(super) fn fill_preview(
    overlay: &gtk::Overlay,
    key: Option<&str>,
    path: &std::path::Path,
    name: &str,
    kind: FileKind,
) {
    let Some(key) =
        key.filter(|_| matches!(kind, FileKind::Image | FileKind::Video | FileKind::Pdf))
    else {
        overlay.add_overlay(&type_icon(name));
        return;
    };
    if let Some((texture, duration)) = SHOWN.with(|shown| shown.borrow().get(key).cloned()) {
        show_thumbnail(overlay, &texture, duration, kind);
        return;
    }
    if FAILED.with(|failed| failed.borrow().contains(key)) {
        overlay.add_overlay(&type_icon(name));
        return;
    }
    let shimmer = gtk::Box::builder()
        .hexpand(true)
        .vexpand(true)
        .css_classes(["skeleton", "dl-skeleton"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    overlay.add_overlay(&shimmer);
    let first = WAITING.with(|waiting| {
        let mut waiting = waiting.borrow_mut();
        let entry = waiting.entry(key.to_string()).or_default();
        entry.push((
            overlay.downgrade(),
            shimmer.downgrade(),
            name.to_string(),
            kind,
        ));
        entry.len() == 1
    });
    if first {
        crate::thumbnails::request(key, path, kind);
    }
}

/// The picture, and for a video a play badge and its length.
fn show_thumbnail(
    overlay: &gtk::Overlay,
    texture: &gtk::gdk::Texture,
    duration: Option<Duration>,
    kind: FileKind,
) {
    let picture = gtk::Picture::builder()
        .paintable(texture)
        .content_fit(gtk::ContentFit::Cover)
        .can_shrink(true)
        .hexpand(true)
        .vexpand(true)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    overlay.add_overlay(&picture);
    if kind != FileKind::Video {
        return;
    }
    let badge = gtk::Box::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["play-badge"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    badge.append(&gtk::Image::from_icon_name("desklink-play-arrow-symbolic"));
    overlay.add_overlay(&badge);
    if let Some(duration) = duration {
        let length = text(&clock(duration), &["duration-chip", "numeric"]);
        length.set_halign(gtk::Align::End);
        length.set_valign(gtk::Align::End);
        overlay.add_overlay(&length);
    }
}

/// "1:05", or "1:02:05" past the hour.
pub(super) fn clock(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match seconds / 3600 {
        0 => format!("{}:{:02}", seconds / 60, seconds % 60),
        hours => format!("{hours}:{:02}:{:02}", (seconds / 60) % 60, seconds % 60),
    }
}

/// A file still where it was: a button that opens it in the viewer.
fn file_card(
    win: &DesklinkWindow,
    entry: &LoggedFile,
    key: Option<&str>,
    device: &str,
    viewable: &Rc<Vec<LoggedFile>>,
) -> gtk::Button {
    let kind = crate::thumbnails::kind_of(&entry.path);
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let (preview, _) = preview_area();
    fill_preview(&preview, key, &entry.path, &entry.name, kind);
    card.append(&preview);
    card.append(&card_words(entry, device));
    let button = gtk::Button::builder()
        .child(&card)
        .css_classes(["file-card"])
        .tooltip_text(&entry.name)
        .build();
    let verb = if entry.incoming { "received" } else { "sent" };
    let place = if entry.incoming {
        format!(", saved in {}", super::saved_in(&entry.path))
    } else {
        String::new()
    };
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}, {verb} {}{place}",
        entry.name,
        crate::bucket_progress::size_words(entry.size),
        when(entry)
    ))]);
    let index = viewable
        .iter()
        .position(|file| file == entry)
        .unwrap_or_default();
    let (window, viewable) = (win.downgrade(), Rc::clone(viewable));
    let device = device.to_string();
    button.connect_clicked(move |_| {
        if let Some(window) = window.upgrade() {
            super::viewer::show(&window, &viewable, index, &device);
        }
    });
    button
}

/// A file that has been moved or deleted since: said so, and Remove from
/// List; nothing to open.
fn missing_card(win: &DesklinkWindow, entry: &LoggedFile, device: &str) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["file-card", "missing"])
        .build();
    let (preview, _) = preview_area();
    let gone = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    gone.append(
        &gtk::Image::builder()
            .icon_name("desklink-broken-image-symbolic")
            .pixel_size(28)
            .css_classes(["files-glyph"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    gone.append(&text("File moved or deleted", &["dl-secondary"]));
    let remove = ui_kit::button(ButtonKind::Text, "Remove from list", None);
    remove.set_halign(gtk::Align::Center);
    gone.append(&remove);
    preview.add_overlay(&gone);
    card.append(&preview);
    card.append(&card_words(entry, device));
    remove.update_property(&[gtk::accessible::Property::Label(&format!(
        "Remove {} from the list",
        entry.name
    ))]);
    let (window, gone) = (win.downgrade(), entry.clone());
    remove.connect_clicked(move |_| {
        crate::device_links::transfer_log::remove(&gone);
        if let Some(window) = window.upgrade() {
            if let Some(page) = window.imp().transfers.borrow().as_ref() {
                page.invalidate();
            }
            window.refresh_soon();
        }
    });
    card.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, moved or deleted",
        entry.name
    ))]);
    card
}

/// Keeps `grid` showing the transfers moving now: a quarter-second timer
/// that runs only while something moves or the page shows, and stops
/// otherwise. The returned function starts it again when it has stopped.
fn follow(
    grid: &gtk::FlowBox,
    daemon: DaemonHandle,
    device: Rc<RefCell<(String, String)>>,
    failures: Rc<Failures>,
) -> Rc<dyn Fn()> {
    let rows: Rc<RefCell<HashMap<String, RowWidgets>>> = Rc::default();
    let weak = grid.downgrade();
    let running = Rc::new(Cell::new(false));
    Rc::new(move || {
        if running.get() {
            return;
        }
        let Some(grid) = weak.upgrade() else {
            return;
        };
        if !needs_timer(&grid, &daemon, &rows) {
            return;
        }
        running.set(true);
        let (weak, daemon, rows, device, running, failures) = (
            weak.clone(),
            daemon.clone(),
            Rc::clone(&rows),
            Rc::clone(&device),
            Rc::clone(&running),
            Rc::clone(&failures),
        );
        glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(grid) = weak.upgrade() else {
                running.set(false);
                return glib::ControlFlow::Break;
            };
            if !needs_timer(&grid, &daemon, &rows) {
                // One last pass clears cards whose transfer has ended.
                refresh(&grid, &daemon, &rows, &device.borrow(), &failures);
                running.set(false);
                return glib::ControlFlow::Break;
            }
            refresh(&grid, &daemon, &rows, &device.borrow(), &failures);
            glib::ControlFlow::Continue
        });
    })
}

/// Whether the progress cards need the timer: the page shows, or a card or
/// a transfer is there.
fn needs_timer(
    grid: &gtk::FlowBox,
    daemon: &DaemonHandle,
    rows: &RefCell<HashMap<String, RowWidgets>>,
) -> bool {
    grid.parent().is_some_and(|page| page.is_mapped())
        || !rows.borrow().is_empty()
        || !super::transfers::rows(daemon).is_empty()
}

fn refresh(
    grid: &gtk::FlowBox,
    daemon: &DaemonHandle,
    widgets: &RefCell<HashMap<String, RowWidgets>>,
    selected: &(String, String),
    failures: &Rc<Failures>,
) {
    let device = selected.1.as_str();
    failures.sync(daemon, &selected.0);
    let rows = rows(daemon);
    let mut widgets = widgets.borrow_mut();
    // Cards whose transfer has ended.
    widgets.retain(|id, shown| {
        let alive = rows.iter().any(|row| row.id == *id);
        if !alive {
            if let Some(child) = shown.row.parent() {
                grid.remove(&child);
            }
        }
        alive
    });
    for row in &rows {
        let shown = widgets.entry(row.id.clone()).or_insert_with(|| {
            let widgets = new_row(daemon, row);
            add_card(grid, &widgets.row);
            widgets
        });
        // A smoothed rate, so the estimate does not jump on every poll.
        let (at, done) = shown.sample.get();
        let elapsed = at.elapsed().as_secs_f64();
        if elapsed >= 1.0 {
            let instant = row.done.saturating_sub(done) as f64 / elapsed;
            let rate = shown.rate.get();
            shown.rate.set(if rate == 0.0 {
                instant
            } else {
                rate * 0.7 + instant * 0.3
            });
            shown.sample.set((std::time::Instant::now(), row.done));
        }
        let mut text = subtitle(
            row,
            if device.is_empty() {
                "your device"
            } else {
                device
            },
        );
        let rate = rate_text(shown.rate.get(), row.total.saturating_sub(row.done));
        let held = crate::device_links::webrtc::transfer_board::note(&row.id).is_some();
        if shows_rate(row, held) && !rate.is_empty() {
            text = format!("{text} · {rate}");
        }
        // Labels here are plain text, so peer-supplied names need no escaping.
        shown.subtitle.set_label(&text);
        shown.subtitle.set_tooltip_text(Some(&text));
        match fraction(row) {
            Some(fraction) => {
                ui_kit::set_progress(&shown.bar, fraction);
                shown.percent.set_label(&ui_kit::percent_text(fraction));
            }
            None => {
                shown.bar.pulse();
                shown.percent.set_label("");
            }
        }
        let state = row_state(row, held);
        if shown.pill_state.replace(Some(state)) != Some(state) {
            match state.pill() {
                Some((status, word)) => {
                    ui_kit::update_pill(&shown.pill, status, word);
                    shown.pill.set_visible(true);
                }
                None => shown.pill.set_visible(false),
            }
        }
    }
    grid.set_visible(!rows.is_empty());
}

/// The aggregate "N files waiting for <phone>" card's button: a list of
/// the files with a remove button each, and Cancel All.
fn waiting_menu(device_id: &str, name: &str) -> gtk::MenuButton {
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let popover = gtk::Popover::builder().child(&list).build();
    let id = device_id.to_string();
    let fill = {
        let (list, id, popover) = (list.clone(), id.clone(), popover.downgrade());
        move || {
            use crate::device_links::daemon::file_transfer as queue;
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            for path in queue::waiting_files(&id) {
                let label = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let line = gtk::Box::builder().spacing(8).build();
                let name = ellipsized(&label, &[]);
                name.set_hexpand(true);
                line.append(&name);
                let remove = gtk::Button::builder()
                    .icon_name("desklink-delete-symbolic")
                    .tooltip_text("Don't send this file")
                    .css_classes(["dl-icon-btn"])
                    .build();
                remove.update_property(&[gtk::accessible::Property::Label(&format!(
                    "Don't send {label}"
                ))]);
                let (id, path, popover) = (id.clone(), path.clone(), popover.clone());
                remove.connect_clicked(move |_| {
                    queue::remove_waiting_file(&id, &path);
                    if let Some(popover) = popover.upgrade() {
                        popover.popdown();
                    }
                });
                line.append(&remove);
                list.append(&line);
            }
            let cancel = ui_kit::button(ButtonKind::Danger, "Cancel all", None);
            let (id, popover) = (id.clone(), popover.clone());
            cancel.connect_clicked(move |_| {
                queue::clear_waiting(&id);
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
            });
            list.append(&cancel);
        }
    };
    fill();
    popover.connect_show({
        let fill = fill.clone();
        move |_| fill()
    });
    let button = gtk::MenuButton::builder()
        .icon_name("view-list-symbolic")
        .tooltip_text("Waiting files")
        .valign(gtk::Align::Center)
        .css_classes(["dl-icon-btn"])
        .popover(&popover)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{name}: choose which to send or cancel them all"
    ))]);
    button
}

/// A transfer moving now, as a row: its type, name and percent, a progress
/// bar, what it is doing in words, and Cancel.
fn new_row(daemon: &DaemonHandle, row: &TransferRow) -> RowWidgets {
    let card = gtk::Box::builder()
        .spacing(12)
        .css_classes(["files-row", "moving"])
        .build();
    card.append(&ui_kit::icon_square(glyph_for(&row.name), Tone::Accent));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let top = gtk::Box::builder().spacing(12).build();
    let name = ellipsized(&row.name, &["dl-mono", "files-name"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    name.set_hexpand(true);
    top.append(&name);
    let percent = gtk::Label::builder()
        .xalign(1.0)
        .css_classes(["dl-progress-text"])
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    top.append(&percent);
    words.append(&top);
    let bar = ui_kit::progress(0.0);
    bar.update_property(&[gtk::accessible::Property::Label(&row.name)]);
    words.append(&bar);
    // Wraps rather than ellipsizes: direction, lane, progress and rate do not
    // fit one line, and the lane ("fast, over Wi-Fi") was the part cut off.
    let subtitle = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .css_classes(["dl-secondary", "numeric"])
        .build();
    words.append(&subtitle);
    let pill = ui_kit::status_pill(Status::Idle, "Paused");
    pill.set_visible(false);
    words.append(&pill);
    card.append(&words);
    if crate::device_links::daemon::file_transfer::is_waiting_row(&row.id) {
        card.append(&waiting_menu(&row.device_id, &row.name));
    }
    // A file still being copied has no transfer to cancel yet.
    if !row.preparing {
        let cancel = ui_kit::icon_button("desklink-close-symbolic", "Cancel this transfer");
        cancel.set_valign(gtk::Align::Center);
        cancel.update_property(&[gtk::accessible::Property::Label(&format!(
            "Cancel {} {}",
            if row.incoming { "receiving" } else { "sending" },
            row.name
        ))]);
        let (daemon, device_id, transfer_id) =
            (daemon.clone(), row.device_id.clone(), row.id.clone());
        cancel.connect_clicked(move |button| {
            // Once is enough; the row goes when the daemon has done it. (A
            // second activation, however it arrives, asks for nothing.)
            if !button.is_sensitive() {
                return;
            }
            button.set_sensitive(false);
            daemon.send(DaemonCommand::CancelTransfer(
                device_id.clone(),
                transfer_id.clone(),
            ));
        });
        card.append(&cancel);
    }
    card.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}",
        row.name,
        if row.incoming { "receiving" } else { "sending" }
    ))]);
    RowWidgets {
        row: card,
        subtitle,
        percent,
        bar,
        pill,
        pill_state: Cell::new(None),
        sample: Cell::new((std::time::Instant::now(), row.done)),
        rate: Cell::new(0.0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(incoming: bool, done: u64, total: u64) -> TransferRow {
        TransferRow {
            id: "t".into(),
            device_id: "phone".into(),
            name: "photo.jpg".into(),
            incoming,
            done,
            total,
            preparing: false,
            paused: false,
        }
    }

    #[test]
    fn a_row_says_which_way_the_file_goes_and_how_much_has_moved() {
        let sending = subtitle(&row(false, 1_000, 4_000), "Pixel");
        assert!(sending.starts_with("Sending to Pixel · "), "{sending}");
        assert!(sending.contains(" of "), "{sending}");
        let receiving = subtitle(&row(true, 0, 4_000), "Pixel");
        assert!(
            receiving.starts_with("Receiving from Pixel · "),
            "{receiving}"
        );
    }

    #[test]
    fn no_rate_is_shown_for_a_row_that_is_waiting() {
        let moving = row(true, 1_000, 4_000);
        assert!(shows_rate(&moving, false));
        assert!(!shows_rate(&moving, true), "a note says why it waits");
        let mut paused = row(true, 1_000, 4_000);
        paused.paused = true;
        assert!(!shows_rate(&paused, false));
        let mut preparing = row(false, 0, 0);
        preparing.preparing = true;
        assert!(!shows_rate(&preparing, false));
    }

    #[test]
    fn a_dropped_connection_reads_as_paused() {
        let mut paused = row(true, 1_000, 4_000);
        paused.paused = true;
        assert_eq!(
            subtitle(&paused, "Pixel"),
            "Paused — resumes when Pixel reconnects"
        );
    }

    /// The lane is what the row is for telling; it sits right after the
    /// direction and the label wraps instead of cutting the tail with an
    /// ellipsis (a 176 px card; down to a 360 px window it is the same card).
    /// (Skipped without a display.)
    #[test]
    fn a_rows_lane_is_said_first_and_never_ellipsized() {
        crate::test_display::run(|| {
            let (daemon, _commands) = DaemonHandle::detached();
            let mut moving = row(false, 113 << 20, 300 << 20);
            moving.id = "lane-row".into();
            use crate::device_links::webrtc::transfer_board as board;
            assert!(board::begin(moving.clone()));
            board::set_lane("lane-row", Some("fast, over Wi-Fi"));
            let text = subtitle(&moving, "emulator");
            assert!(
                text.starts_with("Sending to emulator · fast, over Wi-Fi · "),
                "{text}"
            );
            let shown = new_row(&daemon, &moving);
            assert!(shown.subtitle.wraps());
            assert_eq!(shown.subtitle.ellipsize(), gtk::pango::EllipsizeMode::None);
            board::discard("lane-row");
        });
    }

    /// A paused row (the link dropped mid-file) still has its Cancel button:
    /// one click sends one `CancelTransfer` and the button then rests.
    /// (Skipped without a display.)
    #[test]
    fn a_paused_rows_cancel_button_asks_the_daemon_once() {
        crate::test_display::run(|| {
            use crate::device_links::daemon::DaemonCommand;
            let (daemon, commands) = DaemonHandle::detached();
            let mut paused = row(true, 1_000, 4_000);
            paused.paused = true;
            let shown = new_row(&daemon, &paused);
            fn find(widget: &gtk::Widget) -> Option<gtk::Button> {
                if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                    if button.tooltip_text().as_deref() == Some("Cancel this transfer") {
                        return Some(button.clone());
                    }
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    if let Some(found) = find(&next) {
                        return Some(found);
                    }
                    child = next.next_sibling();
                }
                None
            }
            let cancel = find(shown.row.upcast_ref()).expect("a paused row can be cancelled");
            assert!(cancel.is_sensitive());
            cancel.emit_clicked();
            assert!(matches!(
                commands.try_recv(),
                Ok(DaemonCommand::CancelTransfer(device, id)) if device == "phone" && id == "t"
            ));
            assert!(!cancel.is_sensitive(), "once is enough");
            // `emit_clicked` ignores sensitivity, as a keyboard activation can.
            cancel.emit_clicked();
            assert!(commands.try_recv().is_err(), "no second request");
        });
    }

    #[test]
    fn a_file_being_copied_has_no_size_to_measure() {
        let mut copying = row(false, 0, 0);
        copying.preparing = true;
        assert_eq!(subtitle(&copying, "Pixel"), "Preparing to send to Pixel…");
        assert_eq!(fraction(&copying), None);
    }

    #[test]
    fn the_bar_is_the_share_done_and_never_leaves_its_range() {
        assert_eq!(fraction(&row(false, 0, 200)), Some(0.0));
        assert_eq!(fraction(&row(false, 50, 200)), Some(0.25));
        assert_eq!(fraction(&row(false, 999, 200)), Some(1.0));
        assert_eq!(
            fraction(&row(false, 5, 0)),
            None,
            "an empty file has nothing to measure"
        );
        // Over 2 GB still measures.
        assert_eq!(fraction(&row(false, 3 << 30, 6 << 30)), Some(0.5));
    }

    #[test]
    fn a_video_length_reads_like_a_player() {
        assert_eq!(clock(Duration::from_secs(5)), "0:05");
        assert_eq!(clock(Duration::from_secs(65)), "1:05");
        assert_eq!(clock(Duration::from_secs(3725)), "1:02:05");
    }

    #[test]
    fn speed_and_time_left_read_naturally() {
        assert_eq!(rate_text(0.0, 1_000), "", "no estimate before any progress");
        let text = rate_text(1_000_000.0, 12_000_000);
        assert!(text.ends_with("/s · 12 s left"), "{text}");
        assert!(rate_text(1_000_000.0, 600_000_000).ends_with("10 min left"));
        assert!(rate_text(1_000.0, 10_000_000).ends_with("3 h left"));
        assert!(rate_text(1_000_000.0, 0).ends_with("/s"));
    }

    fn logged(name: &str, incoming: bool, ts: u64) -> LoggedFile {
        LoggedFile {
            ts,
            device_id: "phone".into(),
            device_name: "Pixel".into(),
            name: name.into(),
            path: std::path::PathBuf::from("/tmp").join(name),
            size: 10,
            incoming,
        }
    }

    #[test]
    fn the_chips_filter_by_direction_and_by_kind() {
        let photo = logged("IMG_1.JPG", true, 1);
        let report = logged("report.pdf", false, 2);
        let backup = logged("backup.tar.gz", true, 3);
        let clip = logged("clip.mp4", true, 4);
        let notes = logged("notes.txt", false, 5);
        let all = [&photo, &report, &backup, &clip, &notes];
        let shown = |filter: Filter| -> Vec<&str> {
            all.iter()
                .filter(|entry| filter.matches(entry))
                .map(|entry| entry.name.as_str())
                .collect()
        };
        assert_eq!(shown(Filter::All).len(), 5);
        assert_eq!(
            shown(Filter::Received),
            ["IMG_1.JPG", "backup.tar.gz", "clip.mp4"]
        );
        assert_eq!(shown(Filter::Sent), ["report.pdf", "notes.txt"]);
        assert_eq!(shown(Filter::Images), ["IMG_1.JPG"]);
        assert_eq!(shown(Filter::Documents), ["report.pdf", "notes.txt"]);
        assert_eq!(shown(Filter::Archives), ["backup.tar.gz"]);
        // The chips are the six, in this order, All first.
        let labels: Vec<_> = Filter::CHIPS.iter().map(|filter| filter.label()).collect();
        assert_eq!(
            labels,
            ["All", "Received", "Sent", "Images", "Documents", "Archives"]
        );
    }

    #[test]
    fn a_file_has_a_glyph_by_its_kind() {
        assert_eq!(glyph_for("a.JPG"), "desklink-image-symbolic");
        assert_eq!(glyph_for("a.mkv"), "desklink-movie-symbolic");
        assert_eq!(glyph_for("a.pdf"), "desklink-picture-as-pdf-symbolic");
        assert_eq!(glyph_for("a.zip"), "desklink-folder-zip-symbolic");
        assert_eq!(glyph_for("a.mp3"), "desklink-music-note-symbolic");
        assert_eq!(glyph_for("a.docx"), "desklink-description-symbolic");
        assert_eq!(glyph_for("noextension"), "desklink-description-symbolic");
    }

    #[test]
    fn files_are_grouped_by_the_day_they_came() {
        let at = |day: i32, hour: i32| {
            glib::DateTime::from_local(2026, 10, day, hour, 0, 0.0)
                .unwrap()
                .to_unix() as u64
        };
        let now = glib::DateTime::from_local(2026, 10, 20, 15, 0, 0.0).unwrap();
        // Newest first, as the log gives them.
        let groups = day_groups(
            [
                (at(20, 14), "a"),
                (at(20, 9), "b"),
                (at(19, 22), "c"),
                (at(16, 12), "d"),
                (at(1, 12), "e"),
                (at(1, 8), "f"),
            ],
            &now,
        );
        let shown: Vec<(&str, &[&str])> = groups
            .iter()
            .map(|(day, members)| (day.as_str(), members.as_slice()))
            .collect();
        assert_eq!(
            shown,
            [
                ("Today", &["a", "b"][..]),
                ("Yesterday", &["c"][..]),
                ("Friday", &["d"][..]),
                ("1 October", &["e", "f"][..]),
            ]
        );
        assert!(day_groups(Vec::<(u64, ())>::new(), &now).is_empty());
    }

    #[test]
    fn a_row_says_what_it_is_waiting_for_in_a_word() {
        let moving = row(false, 100, 400);
        assert_eq!(row_state(&moving, false), RowState::Moving);
        assert_eq!(
            RowState::Moving.pill(),
            None,
            "a moving row has its percent"
        );
        assert_eq!(row_state(&moving, true), RowState::Waiting);
        assert_eq!(RowState::Waiting.pill(), Some((Status::Warn, "Waiting")));
        let mut paused = row(true, 100, 400);
        paused.paused = true;
        assert_eq!(row_state(&paused, true), RowState::Paused);
        assert_eq!(RowState::Paused.pill(), Some((Status::Idle, "Paused")));
        let mut copying = row(false, 0, 0);
        copying.preparing = true;
        assert_eq!(row_state(&copying, false), RowState::Preparing);
    }

    #[test]
    fn a_failure_reads_as_a_sentence_and_an_empty_page_says_why() {
        assert_eq!(
            reason_words("pixel 9 ran out of space"),
            "Pixel 9 ran out of space."
        );
        assert_eq!(reason_words("The file changed. "), "The file changed.");
        assert_eq!(reason_words(""), "Something went wrong.");
        let (title, body) = empty_words(Filter::All, false, "Pixel 9");
        assert_eq!(title, "No files yet");
        assert!(body.contains("Pixel 9"), "{body}");
        // A chip with nothing under it is not "no files yet".
        let (title, _) = empty_words(Filter::Archives, true, "Pixel 9");
        assert_eq!(title, "No archives");
        let (title, _) = empty_words(Filter::Archives, false, "Pixel 9");
        assert_eq!(title, "No files yet");
    }

    /// The lane words of CLAUDE.md stay verbatim on the row, whichever the
    /// lane: nothing here rewrites them.
    #[test]
    fn the_lane_words_are_kept_as_the_board_says_them() {
        use crate::device_links::webrtc::transfer_board as board;
        for (id, lane) in [
            ("lane-a", "fast, over Wi-Fi"),
            ("lane-b", "over the secure link"),
            ("lane-c", "through the internet (slower)"),
        ] {
            let mut moving = row(false, 1 << 20, 8 << 20);
            moving.id = id.into();
            assert!(board::begin(moving.clone()));
            board::set_lane(id, Some(lane));
            let text = subtitle(&moving, "Pixel");
            assert!(text.contains(&format!(" · {lane} · ")), "{text}");
            board::discard(id);
        }
    }
}
