//! The cards of transfers moving now: their widgets, the quarter-second
//! timer that follows them only while something moves or the page shows, the
//! refresh, and the menu of a transfer waiting for a slot.
//!
//! Sibling edges: `model` (words, states), `failures` (failed rows).

use super::*;

pub(super) struct RowWidgets {
    pub(super) row: gtk::Box,
    pub(super) subtitle: gtk::Label,
    percent: gtk::Label,
    bar: gtk::ProgressBar,
    /// Paused, waiting or preparing, in a word (hidden while it moves).
    pill: gtk::Box,
    pill_state: Cell<Option<RowState>>,
    /// When `done` was last sampled, and the smoothed rate since.
    sample: Cell<(std::time::Instant, u64)>,
    rate: Cell<f64>,
}

/// Keeps `grid` showing the transfers moving now: a quarter-second timer
/// that runs only while something moves or the page shows, and stops
/// otherwise. The returned function starts it again when it has stopped.
pub(super) fn follow(
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
        || !crate::window::transfers::rows(daemon).is_empty()
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
pub(super) fn new_row(daemon: &DaemonHandle, row: &TransferRow) -> RowWidgets {
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
