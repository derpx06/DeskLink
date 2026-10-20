//! The Transfers page: a drop zone and Choose Files… to send to the phone,
//! what is moving right now with a way to stop it, and the files received
//! here in the last 30 days.
//!
//! What a moving row says is decided by [`subtitle`] and [`fraction`],
//! which have no GTK in them and are unit-tested. Rows are updated in place
//! four times a second, so a progress bar never flickers and a click on
//! Cancel never lands on a row that was just rebuilt.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use super::{ellipsized, icon_button, icon_square, text, DesklinkWindow};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::DeviceView;
use crate::device_links::webrtc::transfer_board::TransferRow;

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
    let direction = if row.incoming {
        format!("Receiving from {device}")
    } else {
        format!("Sending to {device}")
    };
    format!(
        "{direction} · {} of {}",
        glib::format_size(row.done),
        glib::format_size(row.total)
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
    let speed = format!("{}/s", glib::format_size(bytes_per_second as u64));
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
    row: gtk::ListBoxRow,
    subtitle: gtk::Label,
    bar: gtk::ProgressBar,
    /// When `done` was last sampled, and the smoothed rate since.
    sample: Cell<(std::time::Instant, u64)>,
    rate: Cell<f64>,
}

#[derive(Debug)]
pub(super) struct Page {
    pub(super) root: gtk::Widget,
    zone_title: gtk::Label,
    choose: gtk::Button,
    received: gtk::ListBox,
    received_key: RefCell<String>,
    /// The selected device: id and name.
    device: Rc<RefCell<(String, String)>>,
}

impl Page {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
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
        words.append(&text(
            "They land in Downloads on the phone",
            &["card-subtitle"],
        ));
        inner.append(&words);
        let choose = icon_button(
            "document-send-symbolic",
            "Choose Files…",
            &["suggested-action"],
        );
        choose.set_margin_start(16);
        inner.append(&choose);
        zone.append(&inner);

        let live = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .visible(false)
            .build();
        live.update_property(&[gtk::accessible::Property::Label("Moving now")]);

        let received_card = super::card(10);
        received_card.add_css_class("padded-card");
        let head = gtk::Box::builder().spacing(12).build();
        head.append(&icon_square("folder-download-symbolic", 32, "feature-icon"));
        let title = text("Received here", &["card-title"]);
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
        received_card.append(&head);
        let received = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["received-list"])
            .build();
        received.update_property(&[gtk::accessible::Property::Label("Received here")]);
        received_card.append(&received);

        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(8)
            .margin_bottom(28)
            .margin_start(28)
            .margin_end(28)
            .build();
        page.append(&zone);
        page.append(&live);
        page.append(&received_card);
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&page)
            .build();

        let device: Rc<RefCell<(String, String)>> = Rc::default();
        {
            let (window, device) = (win.downgrade(), Rc::clone(&device));
            choose.connect_clicked(move |button| {
                if let Some(daemon) = window.upgrade().and_then(|window| window.daemon()) {
                    super::pick_and_send_files(&daemon, &device.borrow().0, button);
                }
            });
        }
        if let Some(daemon) = win.daemon() {
            follow(&live, daemon, Rc::clone(&device));
        } else {
            // The daemon is handed over after construction.
            let (window, live, device) = (win.downgrade(), live.clone(), Rc::clone(&device));
            glib::idle_add_local_once(move || {
                if let Some(daemon) = window.upgrade().and_then(|window| window.daemon()) {
                    follow(&live, daemon, device);
                }
            });
        }
        {
            let (window, device) = (win.downgrade(), Rc::clone(&device));
            super::accept_dropped_files(&scroll, move || {
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

        Self {
            root: scroll.upcast(),
            zone_title,
            choose,
            received,
            received_key: RefCell::default(),
            device,
        }
    }

    pub(super) fn update(
        &self,
        _win: &DesklinkWindow,
        _daemon: &DaemonHandle,
        device: &DeviceView,
    ) {
        self.device
            .replace((device.id.clone(), device.name.clone()));
        self.zone_title
            .set_label(&format!("Drop files to send to {}", device.name));
        self.choose.set_sensitive(device.feature_transport_ready);
        self.choose
            .set_tooltip_text(super::unavailable_reason(device).as_deref());

        // The 50 newest are listed, and only they decide a redraw.
        let entries: Vec<_> = crate::device_links::transfer_log::recent()
            .into_iter()
            .filter(|entry| entry.device_id == device.id)
            .take(50)
            .collect();
        let minute = super::clock_time(glib::DateTime::now_local().ok());
        let key = serde_json::to_string(&(&entries, &device.name, minute)).unwrap_or_default();
        if *self.received_key.borrow() == key {
            return;
        }
        self.received_key.replace(key);
        self.received.remove_all();
        if entries.is_empty() {
            let none = text(
                &format!("Nothing received from {} in the last 30 days.", device.name),
                &["text-faint", "body-small"],
            );
            none.set_wrap(true);
            self.received.append(
                &gtk::ListBoxRow::builder()
                    .activatable(false)
                    .child(&none)
                    .build(),
            );
            return;
        }
        let now = glib::DateTime::now_local().ok();
        for entry in &entries {
            let line = gtk::Box::builder().spacing(10).build();
            line.append(
                &gtk::Image::builder()
                    .gicon(&file_icon(&entry.name))
                    .css_classes(["text-dim"])
                    .accessible_role(gtk::AccessibleRole::Presentation)
                    .build(),
            );
            let name = ellipsized(&entry.name, &["mono", "body-small"]);
            name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            name.set_hexpand(true);
            line.append(&name);
            let when = glib::DateTime::from_unix_local(entry.ts as i64)
                .ok()
                .zip(now.as_ref())
                .map(|(when, now)| super::history::time_label(now, &when))
                .unwrap_or_default();
            line.append(&text(
                &format!("{} · {when}", glib::format_size(entry.size)),
                &["caption", "text-dim"],
            ));
            let row = gtk::ListBoxRow::builder()
                .child(&line)
                .action_name("win.show-download")
                .action_target(&entry.path.to_string_lossy().to_string().to_variant())
                .tooltip_text("Show in Folder")
                .build();
            row.update_property(&[gtk::accessible::Property::Label(&format!(
                "{}, received {when}. Show in folder",
                entry.name
            ))]);
            self.received.append(&row);
        }
    }
}

/// Keeps `list` showing the transfers moving now while it exists.
fn follow(list: &gtk::ListBox, daemon: DaemonHandle, device: Rc<RefCell<(String, String)>>) {
    let rows: Rc<RefCell<HashMap<String, RowWidgets>>> = Rc::default();
    let weak = list.downgrade();
    glib::timeout_add_local(POLL_INTERVAL, move || {
        let Some(list) = weak.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if list.is_mapped() || !rows.borrow().is_empty() {
            refresh(&list, &daemon, &rows, &device.borrow().1);
        }
        glib::ControlFlow::Continue
    });
}

fn refresh(
    list: &gtk::ListBox,
    daemon: &DaemonHandle,
    widgets: &RefCell<HashMap<String, RowWidgets>>,
    device: &str,
) {
    let rows = rows(daemon);
    let mut widgets = widgets.borrow_mut();
    // Rows whose transfer has ended.
    widgets.retain(|id, shown| {
        let alive = rows.iter().any(|row| row.id == *id);
        if !alive {
            list.remove(&shown.row);
        }
        alive
    });
    for row in &rows {
        let shown = widgets.entry(row.id.clone()).or_insert_with(|| {
            let widgets = new_row(daemon, row);
            list.append(&widgets.row);
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
        match fraction(row) {
            Some(fraction) => shown.bar.set_fraction(fraction),
            None => shown.bar.pulse(),
        }
    }
    list.set_visible(!rows.is_empty());
}

fn new_row(daemon: &DaemonHandle, row: &TransferRow) -> RowWidgets {
    let content = gtk::Box::builder()
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(16)
        .margin_end(12)
        .build();
    content.append(
        &gtk::Image::builder()
            .gicon(&file_icon(&row.name))
            .pixel_size(24)
            .valign(gtk::Align::Center)
            .css_classes(["text-dim"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let name = ellipsized(&row.name, &["mono", "body-small"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    words.append(&name);
    let subtitle = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption", "text-dim", "numeric"])
        .build();
    words.append(&subtitle);
    let bar = gtk::ProgressBar::builder()
        .margin_top(4)
        .css_classes(["thin"])
        .build();
    bar.update_property(&[gtk::accessible::Property::Label(&row.name)]);
    words.append(&bar);
    content.append(&words);

    // A file still being copied has no transfer to cancel yet.
    if !row.preparing {
        let cancel = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text("Cancel this transfer")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        cancel.update_property(&[gtk::accessible::Property::Label("Cancel this transfer")]);
        let (daemon, device_id, transfer_id) =
            (daemon.clone(), row.device_id.clone(), row.id.clone());
        cancel.connect_clicked(move |button| {
            // Once is enough; the row disappears when the daemon has done it.
            button.set_sensitive(false);
            daemon.send(DaemonCommand::CancelTransfer(
                device_id.clone(),
                transfer_id.clone(),
            ));
        });
        content.append(&cancel);
    }
    RowWidgets {
        row: gtk::ListBoxRow::builder()
            .activatable(false)
            .child(&content)
            .build(),
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
    fn speed_and_time_left_read_naturally() {
        assert_eq!(rate_text(0.0, 1_000), "", "no estimate before any progress");
        let text = rate_text(1_000_000.0, 12_000_000);
        assert!(text.ends_with("/s · 12 s left"), "{text}");
        assert!(rate_text(1_000_000.0, 600_000_000).ends_with("10 min left"));
        assert!(rate_text(1_000.0, 10_000_000).ends_with("3 h left"));
        assert!(rate_text(1_000_000.0, 0).ends_with("/s"));
    }
}
