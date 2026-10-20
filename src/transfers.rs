//! The "File transfers" window: what is being sent or received right now, how
//! far along it is, and a way to stop it.
//!
//! What each row says is decided by [`subtitle`] and [`fraction`], which have no
//! GTK in them and are unit-tested. The rest only draws the daemon's table of
//! transfers, updating rows in place so a progress bar never flickers and a
//! click on Cancel never lands on a row that was just rebuilt.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::webrtc::transfer_board::TransferRow;

/// Often enough that a progress bar looks alive, rarely enough to cost nothing.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

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
fn fraction(row: &TransferRow) -> Option<f64> {
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

// Debug builds only: transfers injected by the `e2e` action, for screenshots.
#[cfg(debug_assertions)]
thread_local! {
    pub(crate) static FAKE_ROWS: RefCell<Vec<TransferRow>> = const { RefCell::new(Vec::new()) };
}

struct RowWidgets {
    row: gtk::ListBoxRow,
    subtitle: gtk::Label,
    bar: gtk::ProgressBar,
    /// When `done` was last sampled, and the smoothed rate since.
    sample: Cell<(std::time::Instant, u64)>,
    rate: Cell<f64>,
}

struct Ctx {
    daemon: DaemonHandle,
    list: gtk::ListBox,
    stack: gtk::Stack,
    rows: RefCell<HashMap<String, RowWidgets>>,
    closed: Cell<bool>,
}

/// Opens the window. `on_closed` runs when the user closes it.
pub fn show(daemon: DaemonHandle, parent: &gtk::Window, on_closed: impl Fn() + 'static) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .default_width(480)
        .default_height(400)
        .title("File Transfers")
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .valign(gtk::Align::Start)
        .css_classes(["boxed-list"])
        .build();
    let stack = gtk::Stack::new();
    stack.add_named(
        &adw::StatusPage::builder()
            .icon_name("folder-download-symbolic")
            .title("No Transfers")
            .description("Files you send or receive appear here while they move.")
            .css_classes(["compact"])
            .build(),
        Some("empty"),
    );
    stack.add_named(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(
                &adw::Clamp::builder()
                    .margin_top(12)
                    .margin_bottom(24)
                    .margin_start(12)
                    .margin_end(12)
                    .child(&list)
                    .build(),
            )
            .build(),
        Some("list"),
    );
    toolbar.set_content(Some(&stack));
    window.set_content(Some(&toolbar));

    let ctx = Rc::new(Ctx {
        daemon,
        list,
        stack,
        rows: RefCell::new(HashMap::new()),
        closed: Cell::new(false),
    });
    window.connect_close_request({
        let ctx = Rc::downgrade(&ctx);
        move |_| {
            if let Some(ctx) = ctx.upgrade() {
                ctx.closed.set(true);
            }
            on_closed();
            glib::Propagation::Proceed
        }
    });

    refresh(&ctx);
    // The timer is the one strong owner of the context and ends with the window.
    glib::timeout_add_local(POLL_INTERVAL, {
        let ctx = Rc::clone(&ctx);
        move || {
            if ctx.closed.get() {
                return glib::ControlFlow::Break;
            }
            refresh(&ctx);
            glib::ControlFlow::Continue
        }
    });
    window.present();
}

fn refresh(ctx: &Rc<Ctx>) {
    let names: HashMap<String, String> = ctx
        .daemon
        .devices()
        .into_iter()
        .map(|device| (device.id, device.name))
        .collect();
    #[allow(unused_mut)]
    let mut rows = ctx.daemon.transfer_rows();
    #[cfg(debug_assertions)]
    rows.extend(FAKE_ROWS.with(|fake| fake.borrow().clone()));
    let mut widgets = ctx.rows.borrow_mut();

    // Rows whose transfer has ended.
    widgets.retain(|id, shown| {
        let alive = rows.iter().any(|row| row.id == *id);
        if !alive {
            ctx.list.remove(&shown.row);
        }
        alive
    });

    for row in &rows {
        let device = names
            .get(&row.device_id)
            .map_or("your device", String::as_str);
        let shown = widgets.entry(row.id.clone()).or_insert_with(|| {
            let widgets = new_row(ctx, row);
            ctx.list.append(&widgets.row);
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
        let mut text = subtitle(row, device);
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
    ctx.stack
        .set_visible_child_name(if rows.is_empty() { "empty" } else { "list" });
}

fn new_row(ctx: &Rc<Ctx>, row: &TransferRow) -> RowWidgets {
    let content = gtk::Box::builder()
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    let icon = gtk::Image::builder()
        .gicon(&file_icon(&row.name))
        .pixel_size(32)
        .valign(gtk::Align::Center)
        .css_classes(["dim-label"])
        .build();
    content.append(&icon);

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(
        &gtk::Label::builder()
            .label(&row.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .css_classes(["heading"])
            .build(),
    );
    let subtitle = gtk::Label::builder()
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["caption", "dim-label", "numeric"])
        .build();
    text.append(&subtitle);
    let bar = gtk::ProgressBar::builder().margin_top(4).build();
    bar.update_property(&[gtk::accessible::Property::Label(&row.name)]);
    text.append(&bar);
    content.append(&text);

    // A file still being copied has no transfer to cancel yet.
    if !row.preparing {
        let cancel = gtk::Button::builder()
            .icon_name("process-stop-symbolic")
            .tooltip_text("Cancel this transfer")
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        cancel.update_property(&[gtk::accessible::Property::Label("Cancel this transfer")]);
        cancel.connect_clicked({
            let daemon = ctx.daemon.clone();
            let device_id = row.device_id.clone();
            let transfer_id = row.id.clone();
            move |button| {
                // Once is enough; the row disappears when the daemon has done it.
                button.set_sensitive(false);
                daemon.send(DaemonCommand::CancelTransfer(
                    device_id.clone(),
                    transfer_id.clone(),
                ));
            }
        });
        content.append(&cancel);
    }
    let widget = gtk::ListBoxRow::builder()
        .activatable(false)
        .child(&content)
        .css_classes(["transfer-row"])
        .build();
    RowWidgets {
        row: widget,
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
