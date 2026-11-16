//! The cards of files received or sent: size and time, direction, words,
//! previews (thumbnails delivered by the thumbnail worker, typed icons), and
//! the card for a file that is gone.
//!
//! Sibling edges: `model` (file classes). Outside: `thumbnails`, `transfer_log`.

use super::*;

/// "2.4 MB · 12 min ago".
pub(in crate::window) fn size_and_time(entry: &LoggedFile) -> String {
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
        .map(|(when, now)| crate::window::history::time_label(&now, &when))
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
            &format!("Saved in {}", crate::window::saved_in(&entry.path)),
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
pub(in crate::window) fn type_icon(name: &str) -> gtk::Box {
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
    pub(super) static SHOWN: RefCell<HashMap<String, (gtk::gdk::Texture, Option<Duration>)>> =
        RefCell::default();
    /// Keys with no preview to be had: the typed icon, without asking again.
    static FAILED: RefCell<HashSet<String>> = RefCell::default();
    /// Cards waiting, by key. A key here has been asked for once already, so
    /// a redraw while it is being made never queues it twice.
    static WAITING: RefCell<HashMap<String, Vec<Waiter>>> = RefCell::default();
    pub(super) static WINDOW: RefCell<glib::WeakRef<DesklinkWindow>> = RefCell::default();
}

/// Lets go of every decoded preview (a window nobody is looking at does not
/// keep pictures; the next look decodes the few it shows, from the disk
/// cache).
pub(in crate::window) fn forget_pictures() {
    SHOWN.with(|shown| shown.borrow_mut().clear());
}

/// The thumbnail worker's replies, on the main thread.
pub(super) fn deliver(reply: crate::thumbnails::Reply) {
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
pub(in crate::window) fn fill_preview(
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
pub(super) fn file_card(
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
        format!(", saved in {}", crate::window::saved_in(&entry.path))
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
            crate::window::viewer::show(&window, &viewable, index, &device);
        }
    });
    button
}

/// A file that has been moved or deleted since: said so, and Remove from
/// List; nothing to open.
pub(super) fn missing_card(win: &DesklinkWindow, entry: &LoggedFile, device: &str) -> gtk::Box {
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
            .icon_name(missing_glyph(&entry.name))
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
