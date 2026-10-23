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

use super::{ellipsized, icon_button, icon_square, text, DesklinkWindow};
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
    #[allow(unused_mut)]
    let mut rows = daemon.transfer_rows();
    #[cfg(debug_assertions)]
    rows.extend(FAKE_ROWS.with(|fake| fake.borrow().clone()));
    rows
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
    format!(
        "{direction} · {} of {}",
        crate::bucket_progress::size_words(row.done),
        crate::bucket_progress::size_words(row.total)
    )
}

/// How far along, 0..=1; `None` while there is nothing to measure against.
pub(super) fn fraction(row: &TransferRow) -> Option<f64> {
    if row.preparing || row.total == 0 {
        return None;
    }
    Some((row.done as f64 / row.total as f64).clamp(0.0, 1.0))
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

/// A symbolic icon for the file's type, guessed from its name.
fn file_icon(name: &str) -> gio::Icon {
    let (content_type, _) = gio::content_type_guess(Some(name), &[]);
    gio::content_type_get_symbolic_icon(&content_type)
}

struct RowWidgets {
    row: gtk::Box,
    subtitle: gtk::Label,
    bar: gtk::ProgressBar,
    /// When `done` was last sampled, and the smoothed rate since.
    sample: Cell<(std::time::Instant, u64)>,
    rate: Cell<f64>,
}

#[derive(Debug)]
pub(super) struct Page {
    /// The transfers part of the Files page: the send zone, the transfers
    /// moving now and the history. Not scrolled: Files scrolls it with the
    /// Bucket above it.
    pub(super) content: gtk::Box,
    zone_title: gtk::Label,
    choose: gtk::Button,
    history: gtk::FlowBox,
    empty: gtk::Label,
    history_key: RefCell<String>,
    /// The presence check asked for last; a reply to an older one is
    /// dropped. With it, the entries and device name it was asked for.
    generation: Cell<u64>,
    pending: RefCell<(Vec<LoggedFile>, String)>,
    window: glib::WeakRef<DesklinkWindow>,
    /// The selected device: id and name.
    device: Rc<RefCell<(String, String)>>,
    /// Arms the progress timer (see [`follow`]).
    kick: Kick,
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

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        crate::thumbnails::set_delivery(deliver);
        WINDOW.with(|window| window.replace(win.downgrade()));
        let zone = gtk::Box::builder()
            .spacing(16)
            .height_request(120)
            .halign(gtk::Align::Fill)
            .css_classes(["drop-zone"])
            .build();
        let inner = gtk::Box::builder()
            .spacing(16)
            .hexpand(true)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build();
        inner.set_widget_name("responsive");
        inner.append(&icon_square("document-send-symbolic", 44, "feature-icon"));
        let words = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .valign(gtk::Align::Center)
            .build();
        let zone_title = text("", &["card-title"]);
        zone_title.set_wrap(true);
        words.append(&zone_title);
        let landing = text("They land in Downloads on the phone", &["card-subtitle"]);
        landing.set_wrap(true);
        words.append(&landing);
        inner.append(&words);
        let choose = icon_button(
            "document-send-symbolic",
            "Choose Files…",
            &["suggested-action"],
        );
        choose.set_margin_start(16);
        inner.append(&choose);
        zone.append(&inner);

        let moving = grid();
        moving.set_visible(false);
        moving.update_property(&[gtk::accessible::Property::Label("Moving now")]);

        let head = gtk::Box::builder().spacing(12).margin_top(6).build();
        let title = text("Recent transfers", &["card-title"]);
        title.set_hexpand(true);
        head.append(&title);
        let open = gtk::Button::builder()
            .label("Open Downloads")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        open.connect_clicked(|button| {
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
        head.append(&open);
        let history = grid();
        history.update_property(&[gtk::accessible::Property::Label("Recent transfers")]);
        let empty = text("", &["text-faint", "body-small"]);
        empty.set_wrap(true);

        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_start(16)
            .margin_end(16)
            .build();
        page.append(&zone);
        page.append(&head);
        page.append(&moving);
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
            kick.set(follow(&moving, daemon, Rc::clone(&device)));
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
                    kick.set(follow(&moving, daemon, device));
                }
            });
        }
        Self {
            content: page,
            zone_title,
            choose,
            history,
            empty,
            history_key: RefCell::default(),
            generation: Cell::new(0),
            pending: RefCell::default(),
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
        self.history.remove_all();
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
        self.zone_title
            .set_label(&format!("Drop files to send to {}", device.name));
        self.choose.set_sensitive(device.feature_transport_ready);
        self.choose
            .set_tooltip_text(super::unavailable_reason(device).as_deref());

        // The 50 newest are listed, and only they decide a redraw. Whether
        // each is still there is asked of the worker (a stat on a network
        // mount can hang), and the grid is drawn when it answers.
        let entries: Vec<_> = crate::device_links::transfer_log::recent()
            .into_iter()
            .filter(|entry| entry.device_id == device.id)
            .take(50)
            .collect();
        let minute = super::clock_time(glib::DateTime::now_local().ok());
        let key = serde_json::to_string(&(&entries, &device.name, minute)).unwrap_or_default();
        if *self.history_key.borrow() == key {
            return;
        }
        self.history_key.replace(key);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let paths = entries.iter().map(|entry| entry.path.clone()).collect();
        self.pending.replace((entries, device.name.clone()));
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
        let (entries, device) = self.pending.borrow().clone();
        if files.len() != entries.len() {
            return;
        }
        // Thumbnails kept in memory: only those still listed.
        let keys: HashSet<&String> = files.iter().filter_map(|file| file.key.as_ref()).collect();
        SHOWN.with(|shown| shown.borrow_mut().retain(|key, _| keys.contains(key)));
        self.history.remove_all();
        self.empty.set_visible(entries.is_empty());
        self.empty.set_label(&format!(
            "Nothing sent to or received from {device} in the last 30 days."
        ));
        // What the viewer pages through: the files still there.
        let viewable: Rc<Vec<LoggedFile>> = Rc::new(
            entries
                .iter()
                .zip(files)
                .filter(|(_, file)| file.exists)
                .map(|(entry, _)| entry.clone())
                .collect(),
        );
        for (entry, file) in entries.iter().zip(files) {
            let card: gtk::Widget = if file.exists {
                file_card(&win, entry, file.key.as_deref(), &device, &viewable).upcast()
            } else {
                missing_card(&win, entry, &device).upcast()
            };
            add_card(&self.history, &card);
        }
    }
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
                "go-down-symbolic"
            } else {
                "go-up-symbolic"
            })
            .pixel_size(12)
            .css_classes(["text-faint"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    line.append(&ellipsized(
        &if entry.incoming {
            format!("From {device}")
        } else {
            format!("Sent to {device}")
        },
        &["caption", "text-faint"],
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
    words.append(&ellipsized(&size_and_time(entry), &["caption", "text-dim"]));
    words.append(&direction(entry, device));
    // Where it went, so nobody has to look for it.
    if entry.incoming {
        words.append(&ellipsized(
            &format!("Saved in {}", super::saved_in(&entry.path)),
            &["caption", "text-dim"],
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

/// A typed icon for a file with no picture: a PDF, a document.
pub(super) fn type_icon(name: &str) -> gtk::Box {
    let icon = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    icon.append(
        &gtk::Image::builder()
            .gicon(&file_icon(name))
            .pixel_size(40)
            .css_classes(["text-dim"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("file")
        .to_uppercase();
    icon.append(&text(&extension, &["caption", "type-label"]));
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
        .css_classes(["skeleton"])
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
    badge.append(&gtk::Image::from_icon_name("media-playback-start-symbolic"));
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
            .icon_name("edit-find-symbolic")
            .pixel_size(28)
            .css_classes(["text-faint"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    gone.append(&text("File moved or deleted", &["caption", "text-dim"]));
    let remove = gtk::Button::builder()
        .label("Remove from List")
        .halign(gtk::Align::Center)
        .css_classes(["flat", "remove-button"])
        .build();
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
        let (weak, daemon, rows, device, running) = (
            weak.clone(),
            daemon.clone(),
            Rc::clone(&rows),
            Rc::clone(&device),
            Rc::clone(&running),
        );
        glib::timeout_add_local(POLL_INTERVAL, move || {
            let Some(grid) = weak.upgrade() else {
                running.set(false);
                return glib::ControlFlow::Break;
            };
            if !needs_timer(&grid, &daemon, &rows) {
                // One last pass clears cards whose transfer has ended.
                refresh(&grid, &daemon, &rows, &device.borrow().1);
                running.set(false);
                return glib::ControlFlow::Break;
            }
            refresh(&grid, &daemon, &rows, &device.borrow().1);
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
    device: &str,
) {
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
        if !row.preparing && !rate.is_empty() {
            text = format!("{text} · {rate}");
        }
        // Labels here are plain text, so peer-supplied names need no escaping.
        shown.subtitle.set_label(&text);
        shown.subtitle.set_tooltip_text(Some(&text));
        match fraction(row) {
            Some(fraction) => shown.bar.set_fraction(fraction),
            None => shown.bar.pulse(),
        }
    }
    grid.set_visible(!rows.is_empty());
}

/// A transfer moving now, as a card: its type, name, progress and Cancel.
fn new_row(daemon: &DaemonHandle, row: &TransferRow) -> RowWidgets {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .css_classes(["file-card", "moving"])
        .build();
    let (preview, _) = preview_area();
    preview.add_overlay(&type_icon(&row.name));
    // A file still being copied has no transfer to cancel yet.
    if !row.preparing {
        let cancel = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text("Cancel this transfer")
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_top(6)
            .margin_end(6)
            .css_classes(["circular", "osd"])
            .build();
        cancel.update_property(&[gtk::accessible::Property::Label(&format!(
            "Cancel sending {}",
            row.name
        ))]);
        let (daemon, device_id, transfer_id) =
            (daemon.clone(), row.device_id.clone(), row.id.clone());
        cancel.connect_clicked(move |button| {
            // Once is enough; the card goes when the daemon has done it.
            button.set_sensitive(false);
            daemon.send(DaemonCommand::CancelTransfer(
                device_id.clone(),
                transfer_id.clone(),
            ));
        });
        preview.add_overlay(&cancel);
    }
    card.append(&preview);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .css_classes(["file-card-words"])
        .build();
    let name = ellipsized(&row.name, &["file-name"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    name.set_max_width_chars(16);
    words.append(&name);
    let subtitle = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .max_width_chars(16)
        .css_classes(["caption", "text-dim", "numeric"])
        .build();
    words.append(&subtitle);
    let bar = gtk::ProgressBar::builder()
        .margin_top(4)
        .css_classes(["thin"])
        .build();
    bar.update_property(&[gtk::accessible::Property::Label(&row.name)]);
    words.append(&bar);
    card.append(&words);
    card.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}",
        row.name,
        if row.incoming { "receiving" } else { "sending" }
    ))]);
    RowWidgets {
        row: card,
        subtitle,
        bar,
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
    fn a_dropped_connection_reads_as_paused() {
        let mut paused = row(true, 1_000, 4_000);
        paused.paused = true;
        assert_eq!(
            subtitle(&paused, "Pixel"),
            "Paused — resumes when Pixel reconnects"
        );
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
}
