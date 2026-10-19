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
use gtk::glib;

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

struct RowWidgets {
    row: adw::ActionRow,
    bar: gtk::ProgressBar,
}

struct Ctx {
    daemon: DaemonHandle,
    list: gtk::ListBox,
    empty: gtk::Label,
    rows: RefCell<HashMap<String, RowWidgets>>,
    closed: Cell<bool>,
}

/// Opens the window. `on_closed` runs when the user closes it.
pub fn show(daemon: DaemonHandle, parent: &gtk::Window, on_closed: impl Fn() + 'static) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .default_width(480)
        .default_height(360)
        .title("File transfers")
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let empty = gtk::Label::builder()
        .label("No transfers in progress")
        .css_classes(["dim-label"])
        .margin_top(24)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    content.append(&empty);
    content.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .child(&list)
            .build(),
    );
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    let ctx = Rc::new(Ctx {
        daemon,
        list,
        empty,
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
    let rows = ctx.daemon.transfer_rows();
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
        // Device names come from the peer, and subtitles are read as markup.
        shown
            .row
            .set_subtitle(&glib::markup_escape_text(&subtitle(row, device)));
        match fraction(row) {
            Some(fraction) => shown.bar.set_fraction(fraction),
            None => shown.bar.pulse(),
        }
    }
    ctx.empty.set_visible(rows.is_empty());
}

fn new_row(ctx: &Rc<Ctx>, row: &TransferRow) -> RowWidgets {
    let bar = gtk::ProgressBar::builder()
        .valign(gtk::Align::Center)
        .width_request(140)
        .build();
    let widget = adw::ActionRow::builder()
        .title(glib::markup_escape_text(&row.name))
        .subtitle_lines(1)
        .build();
    widget.add_suffix(&bar);
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
        widget.add_suffix(&cancel);
    }
    RowWidgets { row: widget, bar }
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
}
