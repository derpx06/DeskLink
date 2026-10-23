//! The Bucket page (F8b): one shared space per phone. Any file put in it —
//! dropped, pasted, sent from Files, saved into the folder — shows up on the
//! phone too. A thumbnail grid, newest first, each with its state and menu.
//! Files only for now: folders are refused, out loud.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gdk, gio, glib};

use super::{ellipsized, icon_button, icon_square, text, toast_from, DesklinkWindow};
use crate::device_links::device::DeviceView;
use crate::device_links::folders::{self, Item, ItemState, PairView, Phase, Side, BUCKET_ID};
use crate::device_links::transfer_log::LoggedFile;

thread_local! {
    /// Ctrl+V is installed on the window once.
    static PASTE_READY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Progress widgets on screen, updated by `bucket_progress`'s tick (at
    /// most four times a second) without rebuilding the page.
    static LIVE: std::cell::RefCell<Vec<Live>> = const { std::cell::RefCell::new(Vec::new()) };
    static LISTENING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Where the page is and what is selected (F8c). Selection changes touch
/// the widgets in place: the page is not rebuilt, so scrolling stays.
#[derive(Default)]
struct Browse {
    device: String,
    /// The folder shown: "" the top, `A`, `A/B`.
    current: String,
    selecting: bool,
    selected: BTreeSet<String>,
    /// For Shift-click ranges.
    anchor: Option<String>,
    /// The visible items, in order.
    order: Vec<String>,
    tiles: Vec<TileRef>,
    bar: Option<glib::WeakRef<gtk::Box>>,
    count: Option<glib::WeakRef<gtk::Label>>,
    toggle: Option<glib::WeakRef<gtk::ToggleButton>>,
}

struct TileRef {
    path: String,
    button: glib::WeakRef<gtk::Button>,
    check: glib::WeakRef<gtk::Box>,
}

thread_local! {
    static BROWSE: std::cell::RefCell<Browse> = std::cell::RefCell::default();
}

fn current() -> String {
    BROWSE.with(|browse| browse.borrow().current.clone())
}

/// Folder levels of the folder shown (0 at the top).
fn current_depth() -> usize {
    let current = current();
    if current.is_empty() {
        0
    } else {
        current.split('/').count()
    }
}

fn selecting() -> bool {
    BROWSE.with(|browse| browse.borrow().selecting)
}

fn selected_paths() -> Vec<String> {
    BROWSE.with(|browse| browse.borrow().selected.iter().cloned().collect())
}

/// Shows the folder `path` ("" the top); selection ends.
fn go_to(win: &DesklinkWindow, path: &str) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.current = path.to_string();
        browse.selecting = false;
        browse.selected.clear();
        browse.anchor = None;
    });
    win.refresh_soon();
}

fn set_selecting(on: bool) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = on;
        if !on {
            browse.selected.clear();
            browse.anchor = None;
        }
    });
    refresh_selection();
}

/// Ctrl-click: one more in or out of the selection.
fn toggle_item(path: &str) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = true;
        if !browse.selected.remove(path) {
            browse.selected.insert(path.to_string());
        }
        browse.anchor = Some(path.to_string());
    });
    refresh_selection();
}

/// Shift-click: from the last one clicked to this one.
fn range_to(path: &str) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = true;
        let at = |name: &str, order: &[String]| order.iter().position(|item| item == name);
        let from = browse
            .anchor
            .as_deref()
            .and_then(|anchor| at(anchor, &browse.order))
            .or_else(|| at(path, &browse.order));
        if let (Some(from), Some(to)) = (from, at(path, &browse.order)) {
            let (low, high) = (from.min(to), from.max(to));
            let range: Vec<String> = browse.order[low..=high].to_vec();
            browse.selected.extend(range);
        }
    });
    refresh_selection();
}

fn select_all() {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = true;
        let all = browse.order.clone();
        browse.selected = all.into_iter().collect();
    });
    refresh_selection();
}

/// Makes the widgets say what the selection is (outside the borrow:
/// setting the toggle runs its handler).
fn refresh_selection() {
    type Shown = (
        bool,
        Vec<(bool, glib::WeakRef<gtk::Button>, glib::WeakRef<gtk::Box>)>,
        Option<glib::WeakRef<gtk::Box>>,
        Option<glib::WeakRef<gtk::Label>>,
        Option<glib::WeakRef<gtk::ToggleButton>>,
        usize,
    );
    let (on, tiles, bar, count, toggle, selected): Shown = BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.tiles.retain(|tile| tile.button.upgrade().is_some());
        (
            browse.selecting,
            browse
                .tiles
                .iter()
                .map(|tile| {
                    (
                        browse.selected.contains(&tile.path),
                        tile.button.clone(),
                        tile.check.clone(),
                    )
                })
                .collect(),
            browse.bar.clone(),
            browse.count.clone(),
            browse.toggle.clone(),
            browse.selected.len(),
        )
    });
    for (chosen, button, check) in tiles {
        if let (Some(button), Some(check)) = (button.upgrade(), check.upgrade()) {
            if chosen {
                button.add_css_class("selected");
                check.add_css_class("on");
            } else {
                button.remove_css_class("selected");
                check.remove_css_class("on");
            }
            check.set_visible(on);
            button.update_state(&[gtk::accessible::State::Selected(if on {
                Some(chosen)
            } else {
                None
            })]);
        }
    }
    if let Some(bar) = bar.and_then(|bar| bar.upgrade()) {
        bar.set_visible(on);
    }
    if let Some(label) = count.and_then(|label| label.upgrade()) {
        label.set_label(&match selected {
            0 => "Select items".to_string(),
            1 => "1 selected".to_string(),
            count => format!("{count} selected"),
        });
    }
    if let Some(toggle) = toggle.and_then(|toggle| toggle.upgrade()) {
        if toggle.is_active() != on {
            toggle.set_active(on);
        }
    }
}

/// Debug builds: shots without clicking.
pub(super) fn debug_open(win: &DesklinkWindow, path: &str) {
    go_to(win, path);
}

pub(super) fn debug_select(paths: Vec<String>) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = true;
        browse.selected = paths.into_iter().collect();
    });
    refresh_selection();
}

/// A progress widget kept current.
enum Live {
    /// One file moving: "340 MB of 1.2 GB · 2 min left" and its bar.
    Tile {
        device: String,
        path: String,
        incoming: bool,
        line: glib::WeakRef<gtk::Label>,
        bar: glib::WeakRef<gtk::ProgressBar>,
    },
    /// A folder tile: everything moving inside it, added up; `idle` when
    /// nothing is.
    Folder {
        device: String,
        prefix: String,
        idle: String,
        line: glib::WeakRef<gtk::Label>,
        bar: glib::WeakRef<gtk::ProgressBar>,
    },
    /// The page header or the overview card: the sync's summary while one
    /// goes, `idle` otherwise.
    Summary {
        device: String,
        idle: String,
        line: glib::WeakRef<gtk::Label>,
        bar: glib::WeakRef<gtk::ProgressBar>,
        speed: Option<glib::WeakRef<gtk::Label>>,
    },
}

fn watch(live: Live) {
    update_live(&live);
    LIVE.with(|all| all.borrow_mut().push(live));
    if !LISTENING.replace(true) {
        crate::bucket_progress::on_tick(|| {
            LIVE.with(|all| all.borrow_mut().retain(update_live));
            true
        });
    }
}

fn set_text(label: &gtk::Label, text: &str) {
    if label.label() != text {
        label.set_label(text);
    }
}

fn set_bar(bar: &gtk::ProgressBar, fraction: f64, text: &str) {
    if (bar.fraction() - fraction).abs() > f64::EPSILON {
        bar.set_fraction(fraction);
    }
    bar.update_property(&[gtk::accessible::Property::ValueText(text)]);
}

/// Shows the latest numbers; `false` once the widgets are gone.
fn update_live(live: &Live) -> bool {
    match live {
        Live::Tile {
            device,
            path,
            incoming,
            line,
            bar,
        } => {
            let (Some(line), Some(bar)) = (line.upgrade(), bar.upgrade()) else {
                return false;
            };
            if !line.is_mapped() && line.label() != "" {
                return true;
            }
            let tile = crate::bucket_progress::tile(device, path, *incoming);
            let text = tile
                .as_ref()
                .map_or_else(|| "Waiting…".to_string(), |tile| tile.line());
            set_text(&line, &text);
            set_bar(&bar, tile.map_or(0.0, |tile| tile.fraction()), &text);
        }
        Live::Folder {
            device,
            prefix,
            idle,
            line,
            bar,
        } => {
            let (Some(line), Some(bar)) = (line.upgrade(), bar.upgrade()) else {
                return false;
            };
            match crate::bucket_progress::folder(device, prefix) {
                Some(tile) => {
                    let text = tile.line();
                    set_text(&line, &text);
                    set_bar(&bar, tile.fraction(), &text);
                    bar.set_visible(true);
                }
                None => {
                    set_text(&line, idle);
                    bar.set_visible(false);
                }
            }
        }
        Live::Summary {
            device,
            idle,
            line,
            bar,
            speed,
        } => {
            let (Some(line), Some(bar)) = (line.upgrade(), bar.upgrade()) else {
                return false;
            };
            if !line.is_mapped() && line.label() != "" {
                return true;
            }
            let speed = speed.as_ref().and_then(|speed| speed.upgrade());
            match crate::bucket_progress::summary(device) {
                Some(summary) => {
                    let text = summary.line();
                    set_text(&line, &text);
                    set_bar(&bar, summary.fraction(), &text);
                    bar.set_visible(true);
                    if let Some(speed) = speed {
                        let words = if summary.paused || summary.speed < 1.0 {
                            String::new()
                        } else {
                            crate::bucket_progress::speed_words(summary.speed)
                        };
                        speed.set_visible(!words.is_empty());
                        set_text(&speed, &words);
                    }
                }
                None => {
                    set_text(&line, idle);
                    bar.set_visible(false);
                    if let Some(speed) = speed {
                        speed.set_visible(false);
                    }
                }
            }
        }
    }
    true
}

/// A thin bar for screen readers too.
fn thin_bar(name: &str) -> gtk::ProgressBar {
    let bar = gtk::ProgressBar::builder()
        .css_classes(["thin"])
        .valign(gtk::Align::Center)
        .hexpand(true)
        .build();
    bar.update_property(&[gtk::accessible::Property::Label(name)]);
    bar
}

/// This phone's Bucket, if it is on (or being turned on).
pub(super) fn bucket_of(device: &str) -> Option<PairView> {
    folders::pairs()
        .into_iter()
        .find(|view| view.pair.device == device && view.pair.id == BUCKET_ID && view.pair.bucket)
}

pub(super) fn update(win: &DesklinkWindow, device: &DeviceView) {
    install_paste(win);
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        if browse.device != device.id {
            *browse = Browse {
                device: device.id.clone(),
                ..Browse::default()
            };
        }
    });
    let key = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        current(),
        folders::generation(),
        device.id,
        device.name,
        device.feature_transport_ready,
        super::clock_time(glib::DateTime::now_local().ok())
    );
    if !win.page_changed("bucket", key) {
        return;
    }
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(8)
        .margin_bottom(28)
        .margin_start(16)
        .margin_end(16)
        .build();
    let offer = folders::offers()
        .into_iter()
        .find(|offer| offer.device == device.id && offer.bucket);
    match bucket_of(&device.id) {
        Some(view) if view.phase == Phase::Offered => {
            page.append(&waiting_card(&view, device));
            page.append(&empty_zone(device));
            accept_drops(&page, &device.id, Some(""));
        }
        Some(view) => fill(win, &page, &view, device),
        None => page.append(&off_card(device, offer.is_some())),
    }
    win.replace_body(
        "bucket",
        &adw::Clamp::builder()
            .maximum_size(1000)
            .child(&page)
            .build(),
    );
}

/// Whether `device_id`'s phone has asked for a Bucket that is not on yet: the
/// question "Turn on the Bucket?" waits for an answer.
pub(super) fn asked(device_id: &str) -> bool {
    bucket_of(device_id).is_none()
        && folders::offers()
            .iter()
            .any(|offer| offer.device == device_id && offer.bucket)
}

/// Before the Bucket is on: what it is, one button.
fn off_card(device: &DeviceView, asked: bool) -> gtk::Box {
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .css_classes(["dl-card", "bucket-hero"])
        .build();
    let art = icon_square("folder-symbolic", 64, "bucket-art");
    art.set_halign(gtk::Align::Center);
    card.append(&art);
    let title = text(
        if asked {
            "Turn on the Bucket?"
        } else {
            "Your Bucket"
        },
        &["bucket-title"],
    );
    title.set_halign(gtk::Align::Center);
    card.append(&title);
    let body = text(
        &if asked {
            format!(
                "{} asked for a Bucket with this computer: put anything in it, here or there, \
                 and it shows up on both. Remove it from one and it goes from both.",
                device.name
            )
        } else {
            format!(
                "One place for anything you want on both this computer and {}: files, \
                 pictures, text and links. Put it in on either and it shows up on the other.",
                device.name
            )
        },
        &["text-dim"],
    );
    body.set_wrap(true);
    body.set_justify(gtk::Justification::Center);
    body.set_halign(gtk::Align::Center);
    body.set_max_width_chars(60);
    card.append(&body);
    let buttons = gtk::Box::builder()
        .spacing(10)
        .halign(gtk::Align::Center)
        .margin_top(4)
        .build();
    if asked {
        let later = super::soft_button("Not Now");
        let id = device.id.clone();
        later.connect_clicked(move |_| folders::decline(&id, BUCKET_ID));
        buttons.append(&later);
    }
    let on = icon_button(
        "folder-new-symbolic",
        "Turn On the Bucket",
        &["suggested-action", "pill"],
    );
    let id = device.id.clone();
    on.connect_clicked(move |_| folders::turn_on_bucket(&id));
    buttons.append(&on);
    card.append(&buttons);
    let place = text(
        "It is the folder DeskLink Bucket in your Home folder, also in the Files sidebar.",
        &["caption", "text-faint"],
    );
    place.set_wrap(true);
    place.set_justify(gtk::Justification::Center);
    place.set_halign(gtk::Align::Center);
    card.append(&place);
    let dropped = folders::dropped_pairs();
    if dropped > 0 {
        let note = text(
            &format!(
                "The Bucket replaces folder pairs. {} you set up before stopped syncing; \
                 their files are still where they were.",
                if dropped == 1 {
                    "The folder pair".to_string()
                } else {
                    format!("The {dropped} folder pairs")
                }
            ),
            &["caption", "text-dim"],
        );
        note.set_wrap(true);
        note.set_justify(gtk::Justification::Center);
        note.set_halign(gtk::Align::Center);
        card.append(&note);
    }
    card
}

/// Turned on here; the phone has to pick its side.
fn waiting_card(view: &PairView, device: &DeviceView) -> gtk::Box {
    let bar = gtk::Box::builder()
        .spacing(14)
        .css_classes(["state-banner", "tall"])
        .build();
    let spinner = gtk::Spinner::builder()
        .spinning(true)
        .valign(gtk::Align::Center)
        .build();
    bar.append(&spinner);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(
        &format!("Waiting for {}", device.name),
        &["heading"],
    ));
    let detail = text(
        "Open DeskLink on the phone and choose where its Bucket goes. You can add things here \
         already.",
        &["text-dim"],
    );
    detail.set_wrap(true);
    words.append(&detail);
    bar.append(&words);
    let cancel = super::soft_button("Cancel");
    let (id, pair) = (view.pair.device.clone(), view.pair.id.clone());
    cancel.connect_clicked(move |_| folders::remove(&id, &pair));
    bar.append(&cancel);
    bar
}

/// "Last synced 2 min ago".
fn synced_words(at: Option<i64>) -> Option<String> {
    let at = glib::DateTime::from_unix_local(at? / 1000).ok()?;
    let now = glib::DateTime::now_local().ok()?;
    let minutes = now.difference(&at).as_minutes();
    Some(match minutes {
        ..=0 => "Last synced just now".to_string(),
        1 => "Last synced 1 min ago".to_string(),
        2..=59 => format!("Last synced {minutes} min ago"),
        _ => format!(
            "Last synced {}",
            super::history::time_label(&now, &at).to_lowercase()
        ),
    })
}

fn fill(win: &DesklinkWindow, page: &gtk::Box, view: &PairView, device: &DeviceView) {
    // A folder that is gone (removed on the phone): back to the top.
    let here = current();
    if !here.is_empty() && !view.items.iter().any(|item| item.dir && item.path == here) {
        BROWSE.with(|browse| browse.borrow_mut().current.clear());
    }
    let here = current();
    // This folder's items, folders first.
    let mut level: Vec<Item> = view
        .items
        .iter()
        .filter(|item| folders::parent_of(&item.path) == here)
        .cloned()
        .collect();
    level.sort_by_key(|item| !item.dir);
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.order = level.iter().map(|item| item.path.clone()).collect();
        browse.tiles.clear();
        browse
            .selected
            .retain(|path| level.iter().any(|item| item.path == *path));
    });
    page.append(&header(win, view, device));
    page.append(&selection_bar(win, view, device));
    if let Some(banner) = problem_banner(view) {
        page.append(&banner);
    }
    if level.is_empty() && here.is_empty() {
        page.append(&empty_zone(device));
    } else if level.is_empty() {
        let hint = text(
            "This folder is empty. Drop files here to add them.",
            &["bucket-title"],
        );
        hint.set_margin_top(40);
        hint.set_margin_bottom(40);
        hint.set_halign(gtk::Align::Center);
        hint.set_wrap(true);
        hint.set_justify(gtk::Justification::Center);
        page.append(&hint);
    } else {
        page.append(&grid(win, &level, view, device));
        if let Some(note) = more_note(level.len()) {
            let note = text(&note, &["caption", "text-dim"]);
            note.set_wrap(true);
            note.set_halign(gtk::Align::Center);
            page.append(&note);
        }
        let hint = text(
            "Drop files or folders anywhere here, or press Ctrl+V to paste.",
            &["caption", "text-faint"],
        );
        hint.set_halign(gtk::Align::Center);
        hint.set_wrap(true);
        hint.set_justify(gtk::Justification::Center);
        page.append(&hint);
    }
    if !view.skipped.is_empty() {
        page.append(&not_synced(&view.skipped));
    }
    accept_drops(page, &device.id, None);
    rubber_band(page);
    refresh_selection();
}

/// Dragging over empty space selects what the rectangle touches.
fn rubber_band(page: &gtk::Box) {
    let drag = gtk::GestureDrag::new();
    let active = Rc::new(std::cell::Cell::new(false));
    {
        let (active, page) = (Rc::clone(&active), page.clone());
        drag.connect_drag_begin(move |_, x, y| {
            // Only from empty space: a tile or a button has its own click.
            let mut hit = page.pick(x, y, gtk::PickFlags::DEFAULT);
            let mut on_item = false;
            while let Some(widget) = hit {
                if widget.is::<gtk::Button>()
                    || widget.is::<gtk::MenuButton>()
                    || widget.is::<gtk::ToggleButton>()
                    || widget.is::<gtk::Entry>()
                {
                    on_item = true;
                    break;
                }
                hit = widget.parent();
            }
            active.set(!on_item);
        });
    }
    {
        let (active, page) = (Rc::clone(&active), page.clone());
        drag.connect_drag_update(move |gesture, dx, dy| {
            if !active.get() || (dx.abs() < 6.0 && dy.abs() < 6.0) {
                return;
            }
            let Some((x, y)) = gesture.start_point() else {
                return;
            };
            let band = gtk::graphene::Rect::new(
                x.min(x + dx) as f32,
                y.min(y + dy) as f32,
                dx.abs() as f32,
                dy.abs() as f32,
            );
            let ctrl = gesture
                .current_event_state()
                .contains(gdk::ModifierType::CONTROL_MASK);
            let hits: Vec<String> = BROWSE.with(|browse| {
                browse
                    .borrow()
                    .tiles
                    .iter()
                    .filter(|tile| {
                        tile.button
                            .upgrade()
                            .and_then(|button| button.compute_bounds(&page))
                            .is_some_and(|bounds| bounds.intersection(&band).is_some())
                    })
                    .map(|tile| tile.path.clone())
                    .collect()
            });
            BROWSE.with(|browse| {
                let mut browse = browse.borrow_mut();
                browse.selecting = true;
                if !ctrl {
                    browse.selected.clear();
                }
                browse.selected.extend(hits);
            });
            refresh_selection();
        });
    }
    page.add_controller(drag);
}

/// The bar shown while selecting: the count and the bulk actions.
fn selection_bar(win: &DesklinkWindow, view: &PairView, device: &DeviceView) -> gtk::Box {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .css_classes(["state-banner", "selection-bar"])
        .visible(false)
        .build();
    bar.update_property(&[gtk::accessible::Property::Label("Selection actions")]);
    let top = gtk::Box::builder().spacing(8).build();
    let count = text("Select items", &["heading"]);
    count.set_hexpand(true);
    count.set_xalign(0.0);
    top.append(&count);
    let done = menu_item("Done");
    done.connect_clicked(|_| set_selecting(false));
    top.append(&done);
    bar.append(&top);
    // The actions wrap on a narrow window.
    let actions = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(4)
        .column_spacing(4)
        .row_spacing(0)
        .homogeneous(false)
        .build();
    let add = |button: &gtk::Button| {
        actions.append(button);
    };
    let all = menu_item("Select All");
    all.connect_clicked(|_| select_all());
    add(&all);
    let move_to = menu_item("Move to Folder…");
    {
        let (id, view) = (device.id.clone(), view.clone());
        move_to.connect_clicked(move |button| {
            let paths = selected_paths();
            if !paths.is_empty() {
                move_dialog(button, &id, paths, &view);
            }
        });
    }
    add(&move_to);
    let copy = menu_item("Copy");
    {
        let root = view.pair.root.clone();
        copy.connect_clicked(move |button| {
            let files: Vec<gio::File> = selected_paths()
                .iter()
                .map(|path| gio::File::for_path(root.join(path)))
                .collect();
            if files.is_empty() {
                return;
            }
            let list = gdk::FileList::from_array(&files);
            button
                .clipboard()
                .set_content(Some(&gdk::ContentProvider::for_value(&list.to_value())))
                .ok();
            toast_from(button, "Copied");
        });
    }
    add(&copy);
    let remove = menu_item("Remove from Both…");
    remove.add_css_class("error");
    {
        let (id, phone) = (device.id.clone(), device.name.clone());
        remove.connect_clicked(move |button| {
            let paths = selected_paths();
            let count = paths.len();
            if count == 0 {
                return;
            }
            let id = id.clone();
            super::confirm(
                button,
                &match count {
                    1 => "Remove 1 item?".to_string(),
                    count => format!("Remove {count} items?"),
                },
                &format!("They go to the trash here and on {phone}."),
                "Remove",
                move || {
                    folders::remove_items(&id, paths.clone());
                    set_selecting(false);
                },
            );
        });
    }
    add(&remove);
    bar.append(&actions);
    let _ = win;
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.bar = Some(bar.downgrade());
        browse.count = Some(count.downgrade());
    });
    bar
}

/// "Move to Folder…": pick the Bucket or one of its folders.
fn move_dialog(anchor: &gtk::Button, device: &str, paths: Vec<String>, view: &PairView) {
    let mut places = vec![String::new()];
    places.extend(
        view.items
            .iter()
            .filter(|item| item.dir)
            .map(|item| item.path.clone()),
    );
    places.sort();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["boxed-list"])
        .build();
    list.update_property(&[gtk::accessible::Property::Label("Folders")]);
    for place in &places {
        let name = if place.is_empty() {
            "Bucket".to_string()
        } else {
            place.replace('/', " › ")
        };
        list.append(
            &gtk::Label::builder()
                .label(&name)
                .xalign(0.0)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(10)
                .build(),
        );
    }
    if let Some(first) = list.row_at_index(0) {
        list.select_row(Some(&first));
    }
    let scroll = gtk::ScrolledWindow::builder()
        .child(&list)
        .min_content_height(120)
        .max_content_height(280)
        .propagate_natural_height(true)
        .build();
    let dialog = adw::AlertDialog::new(Some("Move to Folder"), None);
    dialog.set_extra_child(Some(&scroll));
    dialog.add_responses(&[("cancel", "_Cancel"), ("move", "_Move")]);
    dialog.set_response_appearance("move", adw::ResponseAppearance::Suggested);
    dialog.set_close_response("cancel");
    let device = device.to_string();
    dialog.choose(anchor, gio::Cancellable::NONE, move |response| {
        if response != "move" {
            return;
        }
        let index = list.selected_row().map_or(0, |row| row.index()) as usize;
        if let Some(dest) = places.get(index) {
            folders::move_items(&device, paths.clone(), dest);
            set_selecting(false);
        }
    });
}

/// "Not synced: folders" — folders put in the Bucket folder from outside.
fn not_synced(folders: &[String]) -> gtk::Box {
    let line = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    line.append(
        &gtk::Image::builder()
            .icon_name("folder-symbolic")
            .pixel_size(14)
            .css_classes(["text-faint"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let names = folders.join(", ");
    let label = text(
        &format!("Not synced ({}): {names}", folders.len()),
        &["caption", "text-faint"],
    );
    label.set_wrap(true);
    line.append(&label);
    line
}

/// The status line, storage, and the Add and folder buttons.
fn header(win: &DesklinkWindow, view: &PairView, device: &DeviceView) -> gtk::Box {
    let head = gtk::Box::builder().spacing(12).build();
    head.set_widget_name("responsive");
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&breadcrumb(win));
    let state = gtk::Box::builder().spacing(8).build();
    let connected = device.feature_transport_ready;
    state.append(
        &gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes(["dot", if connected { "mint" } else { "faint" }])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let mut parts = Vec::new();
    if connected {
        match &view.phase {
            Phase::Syncing(count) => parts.push(format!(
                "Syncing {count} {}",
                if *count == 1 { "item" } else { "items" }
            )),
            Phase::Scanning => parts.push("Checking…".into()),
            _ => {
                parts.push("Up to date".to_string());
                parts.extend(synced_words(view.last_sync));
            }
        }
    } else {
        parts.push(format!(
            "{} is away · changes sync when it is back",
            device.name
        ));
    }
    parts.push(glib::format_size(view.bytes).to_string());
    let idle = parts.join(" · ");
    let line = text(&idle, &["caption", "text-dim", "numeric"]);
    line.set_wrap(true);
    state.append(&line);
    words.append(&state);
    // While files move: a thin bar for the whole sync, and its speed.
    let progress = gtk::Box::builder().spacing(10).margin_top(2).build();
    let bar = thin_bar("Bucket sync progress");
    bar.set_visible(false);
    progress.append(&bar);
    let speed = text("", &["caption", "text-dim", "numeric"]);
    speed.set_visible(false);
    progress.append(&speed);
    words.append(&progress);
    watch(Live::Summary {
        device: device.id.clone(),
        idle,
        line: line.downgrade(),
        bar: bar.downgrade(),
        speed: Some(speed.downgrade()),
    });
    head.append(&words);

    let actions = gtk::Box::builder()
        .spacing(6)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::End)
        .build();
    actions.append(&add_button(&device.id));
    // F8c: a new folder (not inside a level-2 folder) and selection.
    if current_depth() < folders::MAX_BUCKET_DEPTH {
        let new = gtk::Button::builder()
            .icon_name("folder-new-symbolic")
            .tooltip_text("New Folder")
            .valign(gtk::Align::Center)
            .css_classes(["circular"])
            .build();
        new.update_property(&[gtk::accessible::Property::Label("New Folder")]);
        let id = device.id.clone();
        new.connect_clicked(move |_| {
            folders::new_folder(&id, &current());
        });
        actions.append(&new);
    }
    let select = gtk::ToggleButton::builder()
        .icon_name("selection-mode-symbolic")
        .tooltip_text("Select")
        .valign(gtk::Align::Center)
        .css_classes(["circular"])
        .active(selecting())
        .build();
    select.update_property(&[gtk::accessible::Property::Label("Select items")]);
    select.connect_toggled(|button| set_selecting(button.is_active()));
    BROWSE.with(|browse| browse.borrow_mut().toggle = Some(select.downgrade()));
    actions.append(&select);
    actions.append(&more_menu(view, device));
    head.append(&actions);
    head
}

/// "Bucket › Trips › 2026": every part but the last goes back there.
fn breadcrumb(win: &DesklinkWindow) -> gtk::Box {
    let here = current();
    let crumbs = gtk::Box::builder().spacing(4).build();
    crumbs.update_property(&[gtk::accessible::Property::Label(
        "Where you are in the Bucket",
    )]);
    if here.is_empty() {
        crumbs.append(&text("Bucket", &["bucket-heading"]));
        return crumbs;
    }
    let mut parts = vec![(String::new(), "Bucket".to_string())];
    let mut path = String::new();
    for segment in here.split('/') {
        if !path.is_empty() {
            path.push('/');
        }
        path.push_str(segment);
        parts.push((path.clone(), segment.to_string()));
    }
    let last = parts.len() - 1;
    for (index, (target, name)) in parts.into_iter().enumerate() {
        if index > 0 {
            crumbs.append(&text("›", &["bucket-heading", "text-faint"]));
        }
        if index == last {
            let label = text(&name, &["bucket-heading"]);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_max_width_chars(20);
            crumbs.append(&label);
        } else {
            let button = gtk::Button::builder()
                .label(&name)
                .css_classes(["flat", "bucket-crumb"])
                .build();
            button.update_property(&[gtk::accessible::Property::Label(&format!("Back to {name}"))]);
            let win = win.downgrade();
            button.connect_clicked(move |_| {
                if let Some(win) = win.upgrade() {
                    go_to(&win, &target);
                }
            });
            crumbs.append(&button);
        }
    }
    crumbs
}

/// "Add Files…".
fn add_button(device: &str) -> gtk::Button {
    let button = icon_button("list-add-symbolic", "Add Files…", &["suggested-action"]);
    let device = device.to_string();
    button.connect_clicked(move |button| choose(button, &device));
    button
}

/// The page's "⋯": turning the Bucket off.
fn more_menu(view: &PairView, device: &DeviceView) -> gtk::MenuButton {
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let popover = gtk::Popover::builder().child(&menu).build();
    let open_files = menu_item("Open in Files");
    {
        let (root, pop) = (view.pair.root.clone(), popover.clone());
        open_files.connect_clicked(move |button| {
            pop.popdown();
            launch(button, &root);
        });
    }
    menu.append(&open_files);
    let add_folder = menu_item("Add Folder…");
    {
        let (id, pop) = (device.id.clone(), popover.clone());
        add_folder.connect_clicked(move |button| {
            pop.popdown();
            choose_folder(button, &id);
        });
    }
    menu.append(&add_folder);
    let off = menu_item("Turn Off the Bucket…");
    off.add_css_class("error");
    let (id, name, root) = (
        device.id.clone(),
        device.name.clone(),
        view.pair.root.clone(),
    );
    let pop = popover.clone();
    off.connect_clicked(move |button| {
        pop.popdown();
        let id = id.clone();
        super::confirm(
            button,
            "Turn off the Bucket?",
            &format!(
                "Things stop syncing with {name}. Nothing is deleted: everything stays in {} \
                 and on the phone.",
                shown_path(&root)
            ),
            "Turn Off",
            move || folders::remove(&id, BUCKET_ID),
        );
    });
    menu.append(&off);
    let button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .popover(&popover)
        .valign(gtk::Align::Center)
        .css_classes(["circular", "flat"])
        .tooltip_text("More")
        .build();
    button.update_property(&[gtk::accessible::Property::Label("More Bucket options")]);
    button
}

fn menu_item(label: &str) -> gtk::Button {
    let button = gtk::Button::builder()
        .label(label)
        .css_classes(["flat"])
        .build();
    if let Some(label) = button.child().and_downcast::<gtk::Label>() {
        label.set_xalign(0.0);
    }
    button
}

/// What needs the user: a size warning, a held empty folder, a problem.
/// What the Bucket's size says: a warning from 8 GB, "full" at 10 GB, where
/// adding more is refused.
fn size_notice(bytes: u64) -> Option<(String, String)> {
    if bytes >= folders::BUCKET_FULL_BYTES {
        Some((
            "The Bucket is full (10 GB)".to_string(),
            "Nothing more can be added until you remove something.".to_string(),
        ))
    } else if bytes > folders::BUCKET_WARN_BYTES {
        Some((
            "The Bucket is getting big".to_string(),
            format!(
                "{} of 10 GB — the phone keeps a copy of everything in it.",
                glib::format_size(bytes)
            ),
        ))
    } else {
        None
    }
}

fn problem_banner(view: &PairView) -> Option<gtk::Box> {
    let (icon, headline, detail, confirm) = match &view.phase {
        Phase::EmptyHeld => (
            "dialog-warning-symbolic",
            "The Bucket folder is empty here".to_string(),
            "Is its drive connected? Nothing is deleted on the phone until you say so.".to_string(),
            true,
        ),
        Phase::Problem(problem) => (
            "dialog-warning-symbolic",
            "Something needs a look".to_string(),
            problem.clone(),
            false,
        ),
        Phase::TooMany => (
            "dialog-warning-symbolic",
            "The Bucket is full".to_string(),
            "Only the first 10,000 items sync.".to_string(),
            false,
        ),
        Phase::NotOnPhone => (
            "dialog-warning-symbolic",
            "Not set up on the phone".to_string(),
            "Turn on the Bucket in DeskLink on the phone.".to_string(),
            false,
        ),
        _ if size_notice(view.bytes).is_some() => {
            let (headline, detail) = size_notice(view.bytes).unwrap_or_default();
            ("drive-harddisk-symbolic", headline, detail, false)
        }
        _ => return None,
    };
    let bar = gtk::Box::builder()
        .spacing(14)
        .css_classes(["state-banner", "tall"])
        .build();
    bar.append(&icon_square(icon, 28, "state-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(&headline, &["heading"]));
    let detail_label = text(&detail, &["text-dim"]);
    detail_label.set_wrap(true);
    words.append(&detail_label);
    bar.append(&words);
    if confirm {
        let button = super::soft_button("It's Empty on Purpose");
        let id = view.pair.device.clone();
        button.connect_clicked(move |button| {
            let id = id.clone();
            super::confirm(
                button,
                "Empty the Bucket on the phone too?",
                "Everything in it is removed from the phone (to its trash).",
                "Empty It",
                move || folders::confirm_empty(&id, BUCKET_ID),
            );
        });
        bar.append(&button);
    }
    bar.update_property(&[gtk::accessible::Property::Label(&format!(
        "{headline}. {detail}"
    ))]);
    Some(bar)
}

/// Nothing here yet: a big drop zone.
fn empty_zone(device: &DeviceView) -> gtk::Box {
    let zone = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .height_request(300)
        .css_classes(["drop-zone", "bucket-drop"])
        .build();
    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .vexpand(true)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .margin_start(16)
        .margin_end(16)
        .build();
    let art = icon_square("folder-download-symbolic", 64, "bucket-art");
    art.set_halign(gtk::Align::Center);
    inner.append(&art);
    let title = text(
        &format!("Drop anything here. It shows up on {} too.", device.name),
        &["bucket-title"],
    );
    title.set_wrap(true);
    title.set_justify(gtk::Justification::Center);
    title.set_halign(gtk::Align::Center);
    inner.append(&title);
    {
        let ways = text(
            "Or paste with Ctrl+V, choose Add Files, use Files › Scripts › Add to DeskLink \
             Bucket, or save files straight into the DeskLink Bucket folder.",
            &["caption", "text-dim"],
        );
        ways.set_wrap(true);
        ways.set_justify(gtk::Justification::Center);
        ways.set_max_width_chars(56);
        ways.set_halign(gtk::Align::Center);
        inner.append(&ways);
    }
    zone.append(&inner);
    zone
}

/// Tiles: two a line at 360 px, more as the page allows.
/// Tiles drawn at once (the grid is built whole, so a big folder must not
/// cost a page of thousands of widgets); past it, a line says how many more
/// there are and where they are.
const MAX_TILES: usize = 300;

/// "Showing 300 of 1,204. The rest are in the folder on this computer." once
/// a level has more items than are drawn.
fn more_note(count: usize) -> Option<String> {
    (count > MAX_TILES).then(|| {
        format!(
            "Showing {MAX_TILES} of {count}. The rest are in the Bucket folder on this computer; \
             put them in folders to see them here."
        )
    })
}

fn grid(
    win: &DesklinkWindow,
    items: &[Item],
    view: &PairView,
    device: &DeviceView,
) -> gtk::FlowBox {
    let grid = super::transfers::grid();
    grid.set_column_spacing(12);
    grid.set_row_spacing(12);
    grid.set_max_children_per_line(6);
    grid.update_property(&[gtk::accessible::Property::Label("Bucket items")]);
    let root = view.pair.root.clone();
    // What the viewer can page through: this level's files.
    let files: Rc<Vec<LoggedFile>> = Rc::new(
        items
            .iter()
            .filter(|item| item.state != ItemState::Receiving && !item.dir)
            .map(|item| LoggedFile {
                ts: (item.mtime / 1000).max(0) as u64,
                device_id: device.id.clone(),
                device_name: device.name.clone(),
                name: item.path.clone(),
                path: root.join(&item.path),
                size: item.size,
                incoming: item.from == Side::Phone,
            })
            .collect(),
    );
    for item in items.iter().take(MAX_TILES) {
        super::transfers::add_card(&grid, &tile(win, item, &root, device, &files, view));
    }
    grid
}

/// "From Pixel 9" / "From this computer".
fn from_words(item: &Item, device: &DeviceView) -> String {
    match item.from {
        Side::Phone => format!("From {}", device.name),
        Side::Desk => "From this computer".to_string(),
    }
}

/// The last part of a path.
fn leaf(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// One item: its picture, name, size and origin, its state, its menu. A
/// folder shows what it holds and opens on a click.
fn tile(
    win: &DesklinkWindow,
    item: &Item,
    root: &Path,
    device: &DeviceView,
    files: &Rc<Vec<LoggedFile>>,
    view: &PairView,
) -> gtk::Overlay {
    let path = root.join(&item.path);
    let item = &{
        let mut shown = item.clone();
        if shown.dir {
            // The badge rolls up from the children.
            let below = format!("{}/", item.path);
            shown.state = folders::rollup(
                view.items
                    .iter()
                    .filter(|other| other.path.starts_with(&below))
                    .map(|other| other.state),
            );
        }
        shown
    };
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    let base = gtk::Box::builder()
        .width_request(150)
        .height_request(112)
        .css_classes(["file-preview"])
        .build();
    let preview = gtk::Overlay::builder()
        .child(&base)
        .overflow(gtk::Overflow::Hidden)
        .css_classes(["file-preview-clip"])
        .build();
    if item.dir {
        preview.add_overlay(
            &gtk::Image::builder()
                .icon_name("folder-symbolic")
                .pixel_size(56)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .css_classes(["text-dim", "bucket-folder-icon"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
    } else if item.state == ItemState::Receiving {
        preview.add_overlay(&super::transfers::type_icon(&item.path));
    } else {
        let kind = crate::thumbnails::kind_of(&path);
        let key = std::fs::metadata(&path).ok().and_then(|metadata| {
            Some(crate::thumbnails::cache_key(
                &path,
                metadata.modified().ok()?,
                metadata.len(),
            ))
        });
        super::transfers::fill_preview(&preview, key.as_deref(), &path, &item.path, kind);
    }
    if let Some(badge) = badge(item, device) {
        preview.add_overlay(&badge);
    }
    // The selection mark, top left, while selecting.
    let check = gtk::Box::builder()
        .halign(gtk::Align::Start)
        .valign(gtk::Align::Start)
        .margin_top(8)
        .margin_start(8)
        .css_classes(["bucket-badge", "bucket-check"])
        .visible(false)
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    check.append(&gtk::Image::from_icon_name("object-select-symbolic"));
    preview.add_overlay(&check);
    card.append(&preview);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["file-card-words", "bucket-words"])
        .build();
    let name = ellipsized(leaf(&item.path), &["file-name"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    name.set_max_width_chars(14);
    words.append(&name);
    let meta = if item.dir {
        let (count, bytes) = folders::totals(&view.items, &item.path);
        format!(
            "{} · {}",
            match count {
                1 => "1 item".to_string(),
                count => format!("{count} items"),
            },
            glib::format_size(bytes)
        )
    } else {
        format!(
            "{} · {}",
            glib::format_size(item.size),
            from_words(item, device)
        )
    };
    if item.dir {
        // Everything moving inside it, added up.
        let line = ellipsized(&meta, &["caption", "text-dim", "numeric"]);
        words.append(&line);
        let bar = thin_bar(&format!("Progress of {}", leaf(&item.path)));
        bar.set_margin_top(4);
        bar.set_visible(false);
        words.append(&bar);
        watch(Live::Folder {
            device: device.id.clone(),
            prefix: item.path.clone(),
            idle: meta.clone(),
            line: line.downgrade(),
            bar: bar.downgrade(),
        });
    } else if matches!(item.state, ItemState::Sending | ItemState::Receiving) {
        // "340 MB of 1.2 GB · 2 min left" and a bar, kept current.
        let line = ellipsized("Waiting…", &["caption", "text-dim", "numeric"]);
        words.append(&line);
        let bar = thin_bar(&format!("Progress of {}", leaf(&item.path)));
        bar.set_margin_top(4);
        words.append(&bar);
        watch(Live::Tile {
            device: device.id.clone(),
            path: item.path.clone(),
            incoming: item.state == ItemState::Receiving,
            line: line.downgrade(),
            bar: bar.downgrade(),
        });
    } else {
        words.append(&ellipsized(&meta, &["caption", "text-dim"]));
    }
    card.append(&words);
    let button = gtk::Button::builder()
        .child(&card)
        .css_classes(["file-card"])
        .tooltip_text(&item.path)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}{}, {}",
        leaf(&item.path),
        if item.dir { "folder, " } else { "" },
        meta,
        state_words(item, device)
    ))]);
    BROWSE.with(|browse| {
        browse.borrow_mut().tiles.push(TileRef {
            path: item.path.clone(),
            button: button.downgrade(),
            check: check.downgrade(),
        });
    });
    {
        let (item, win) = (item.clone(), win.downgrade());
        let (files, device_name, path) = (Rc::clone(files), device.name.clone(), path.clone());
        button.connect_clicked(move |_| {
            let Some(win) = win.upgrade() else {
                return;
            };
            if item.dir {
                go_to(&win, &item.path);
            } else if item.state != ItemState::Receiving {
                let index = files.iter().position(|file| file.path == path).unwrap_or(0);
                super::viewer::show(&win, &files, index, &device_name);
            }
        });
    }
    // Ctrl-click and Shift-click select; so does any click while selecting.
    {
        let click = gtk::GestureClick::builder()
            .button(1)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let rel = item.path.clone();
        click.connect_pressed(move |gesture, _, _, _| {
            let state = gesture.current_event_state();
            if state.contains(gdk::ModifierType::SHIFT_MASK) {
                range_to(&rel);
            } else if state.contains(gdk::ModifierType::CONTROL_MASK) || selecting() {
                toggle_item(&rel);
            } else {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
        });
        button.add_controller(click);
    }
    let overlay = gtk::Overlay::builder().child(&button).build();
    if item.state != ItemState::Receiving {
        let menu = item_menu(item, &path, device, view);
        overlay.add_overlay(&menu);
        // Right-click opens the same menu.
        let click = gtk::GestureClick::builder().button(3).build();
        let weak = menu.downgrade();
        click.connect_pressed(move |_, _, _, _| {
            if let Some(menu) = weak.upgrade() {
                menu.popup();
            }
        });
        button.add_controller(click);
    }
    overlay
}

fn state_words(item: &Item, device: &DeviceView) -> String {
    match item.state {
        ItemState::Synced => format!("on {} too", device.name),
        ItemState::Sending => format!("going to {}", device.name),
        ItemState::Receiving => format!("coming from {}", device.name),
        ItemState::Waiting => format!("waiting for {}", device.name),
        ItemState::Conflict => "changed on both, needs a choice".to_string(),
    }
}

/// The state, top right on the picture.
fn badge(item: &Item, device: &DeviceView) -> Option<gtk::Widget> {
    let holder = gtk::Box::builder()
        .halign(gtk::Align::End)
        .valign(gtk::Align::Start)
        .margin_top(8)
        .margin_end(8)
        .css_classes(["bucket-badge"])
        .tooltip_text(state_words(item, device))
        .accessible_role(gtk::AccessibleRole::Presentation)
        .build();
    match item.state {
        ItemState::Synced => {
            holder.add_css_class("synced");
            holder.append(&gtk::Image::from_icon_name("object-select-symbolic"));
        }
        ItemState::Sending => {
            holder.add_css_class("moving");
            holder.append(&gtk::Image::from_icon_name("go-up-symbolic"));
        }
        ItemState::Receiving => {
            holder.add_css_class("moving");
            holder.append(&gtk::Image::from_icon_name("go-down-symbolic"));
        }
        ItemState::Waiting => {
            holder.add_css_class("waiting");
            holder.append(&gtk::Image::from_icon_name("alarm-symbolic"));
        }
        ItemState::Conflict => {
            holder.add_css_class("conflict");
            holder.append(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        }
    }
    Some(holder.upcast())
}

/// The tile's "⋯": Open, Show in Folder, Copy, Rename, Remove from Both,
/// and for a conflict, its choices.
fn item_menu(item: &Item, path: &Path, device: &DeviceView, view: &PairView) -> gtk::MenuButton {
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let popover = gtk::Popover::builder().child(&menu).build();
    let add = |label: &str, run: Box<dyn Fn(&gtk::Button)>| {
        let button = menu_item(label);
        let popover = popover.clone();
        button.connect_clicked(move |button| {
            popover.popdown();
            run(button);
        });
        menu.append(&button);
        button
    };
    let path = path.to_path_buf();
    {
        let path = path.clone();
        add("Open", Box::new(move |button| launch(button, &path)));
    }
    {
        let path = path.clone();
        add(
            "Show in Folder",
            Box::new(move |button| {
                gtk::FileLauncher::new(Some(&gio::File::for_path(&path))).open_containing_folder(
                    button.root().and_downcast_ref::<gtk::Window>(),
                    gio::Cancellable::NONE,
                    |_| {},
                );
            }),
        );
    }
    {
        let path = path.clone();
        add(
            "Copy",
            Box::new(move |button| {
                let list = gdk::FileList::from_array(&[gio::File::for_path(&path)]);
                button
                    .clipboard()
                    .set_content(Some(&gdk::ContentProvider::for_value(&list.to_value())))
                    .ok();
                toast_from(button, "Copied");
            }),
        );
    }
    {
        let path = path.clone();
        add("Rename…", Box::new(move |button| rename(button, &path)));
    }
    if item.state == ItemState::Conflict {
        let conflict = view
            .conflicts
            .iter()
            .find(|conflict| conflict.path == item.path || conflict.copy == item.path)
            .cloned();
        if let Some(conflict) = conflict {
            for (label, how) in [
                ("Keep Both".to_string(), folders::Resolve::Both),
                ("Keep This Computer's".to_string(), folders::Resolve::Desk),
                (format!("Keep {}'s", device.name), folders::Resolve::Phone),
            ] {
                let (id, conflict) = (device.id.clone(), conflict.clone());
                add(
                    &label,
                    Box::new(move |_| {
                        folders::resolve(&id, BUCKET_ID, &conflict.path, how);
                    }),
                );
            }
        }
    }
    let (name, phone, rel, id) = (
        leaf(&item.path).to_string(),
        device.name.clone(),
        item.path.clone(),
        device.id.clone(),
    );
    let folder = item.dir;
    let remove = add(
        "Remove from Both…",
        Box::new(move |button| {
            let (rel, id) = (rel.clone(), id.clone());
            super::confirm(
                button,
                &format!("Remove “{name}”?"),
                &if folder {
                    format!(
                        "The folder and everything in it goes to the trash here and on {phone}."
                    )
                } else {
                    format!("It goes to the trash here and on {phone}.")
                },
                "Remove",
                move || {
                    folders::remove_items(&id, vec![rel.clone()]);
                },
            );
        }),
    );
    remove.add_css_class("error");
    let button = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .popover(&popover)
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .margin_end(6)
        .margin_bottom(8)
        .css_classes(["circular", "flat", "bucket-menu"])
        .tooltip_text("More")
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "More for {}",
        &item.path
    ))]);
    button
}

/// Asks for a new name; renamed in place (the phone gets a move, no copy).
fn rename(button: &gtk::Button, path: &Path) {
    let current = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    let entry = gtk::Entry::builder()
        .text(&current)
        .activates_default(true)
        .build();
    entry.update_property(&[gtk::accessible::Property::Label("New name")]);
    let dialog = adw::AlertDialog::new(Some("Rename"), None);
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", "_Cancel"), ("rename", "_Rename")]);
    dialog.set_response_appearance("rename", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("rename"));
    dialog.set_close_response("cancel");
    let path = path.to_path_buf();
    let anchor = button.clone();
    let field = entry.clone();
    dialog.choose(button, gio::Cancellable::NONE, move |response| {
        if response != "rename" {
            return;
        }
        let name = field.text().trim().to_string();
        match rename_to(&path, &name) {
            Ok(()) => {}
            Err(error) => toast_from(&anchor, &error),
        }
    });
    entry.grab_focus();
}

/// Renames `path` to `name` beside it; refuses an unsafe or taken name.
fn rename_to(path: &Path, name: &str) -> Result<(), String> {
    // What the scan would drop (and so delete on the phone) is refused here.
    if !folders::name_ok(name) || name.starts_with('.') {
        return Err("That name cannot be used".into());
    }
    let target = path.with_file_name(name);
    if target == path {
        return Ok(());
    }
    if target.exists() {
        return Err("Something with that name is already there".into());
    }
    std::fs::rename(path, &target).map_err(|_| "Could not rename it".to_string())
}

fn launch(widget: &impl IsA<gtk::Widget>, path: &Path) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
        widget.root().and_downcast_ref::<gtk::Window>(),
        gio::Cancellable::NONE,
        |_| {},
    );
}

/// "~/DeskLink Bucket".
fn shown_path(path: &Path) -> String {
    let home = glib::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

/// Puts `paths` (files and folders) in the folder shown of `device`'s
/// Bucket. What was left out is told when the copy is done.
fn add_paths(device: &str, paths: Vec<PathBuf>, into: &str) -> bool {
    !paths.is_empty() && folders::import(device, paths, into)
}

/// "Add Files…".
fn choose(widget: &impl IsA<gtk::Widget>, device: &str) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let device = device.to_string();
    let dialog = gtk::FileDialog::builder()
        .title("Add Files to the Bucket")
        .modal(true)
        .build();
    dialog.open_multiple(Some(&root), gio::Cancellable::NONE, move |result| {
        if let Ok(files) = result {
            let paths: Vec<PathBuf> = files
                .iter::<gio::File>()
                .flatten()
                .filter_map(|file| file.path())
                .collect();
            add_paths(&device, paths, &current());
        }
    });
}

/// "Add Folder…".
fn choose_folder(widget: &impl IsA<gtk::Widget>, device: &str) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let device = device.to_string();
    let dialog = gtk::FileDialog::builder()
        .title("Add a Folder to the Bucket")
        .modal(true)
        .build();
    dialog.select_multiple_folders(Some(&root), gio::Cancellable::NONE, move |result| {
        if let Ok(files) = result {
            let paths: Vec<PathBuf> = files
                .iter::<gio::File>()
                .flatten()
                .filter_map(|file| file.path())
                .collect();
            add_paths(&device, paths, &current());
        }
    });
}

/// Files dropped anywhere on `target` go in the Bucket.
/// Where a drop goes: the explicit folder of the target, else the one shown.
fn drop_folder(explicit: Option<&str>, shown: &str) -> String {
    explicit.unwrap_or(shown).to_string()
}

/// `into`: the folder they go in; `None` the one shown at the time of the
/// drop, `Some("")` the top (the Overview card, whatever the Bucket page
/// last showed).
pub(super) fn accept_drops(target: &impl IsA<gtk::Widget>, device: &str, into: Option<&str>) {
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let device = device.to_string();
    let into = into.map(str::to_string);
    let shown = target.clone().upcast::<gtk::Widget>();
    drop.connect_enter(move |_, _, _| {
        shown.add_css_class("bucket-drop-hover");
        gdk::DragAction::COPY
    });
    let shown = target.clone().upcast::<gtk::Widget>();
    drop.connect_leave(move |_| shown.remove_css_class("bucket-drop-hover"));
    let shown = target.clone().upcast::<gtk::Widget>();
    drop.connect_drop(move |_, value, _, _| {
        shown.remove_css_class("bucket-drop-hover");
        let Ok(list) = value.get::<gdk::FileList>() else {
            return false;
        };
        let paths: Vec<PathBuf> = list.files().iter().filter_map(|file| file.path()).collect();
        add_paths(&device, paths, &drop_folder(into.as_deref(), &current()))
    });
    target.as_ref().add_controller(drop);
}

/// Ctrl+V on the Bucket page: files, a picture, a link or text. Installed
/// on the window once; it passes when another page is shown, and a focused
/// text field gets its own Ctrl+V first.
fn install_paste(win: &DesklinkWindow) {
    if PASTE_READY.with(|ready| ready.replace(true)) {
        return;
    }
    let controller = gtk::ShortcutController::new();
    controller.set_scope(gtk::ShortcutScope::Managed);
    let action = gtk::CallbackAction::new(|widget, _| {
        let Some(win) = widget.downcast_ref::<DesklinkWindow>() else {
            return glib::Propagation::Proceed;
        };
        if win.imp().page.borrow().as_str() != "files" {
            return glib::Propagation::Proceed;
        }
        let Some(device) = win.selected_id().filter(|id| bucket_of(id).is_some()) else {
            return glib::Propagation::Proceed;
        };
        paste(win, &device);
        glib::Propagation::Stop
    });
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>v"),
        Some(action),
    ));
    // Ctrl+A selects everything shown; Escape leaves selecting.
    let on_bucket = |widget: &gtk::Widget| {
        widget
            .downcast_ref::<DesklinkWindow>()
            .is_some_and(|win| win.imp().page.borrow().as_str() == "files")
    };
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("<Control>a"),
        Some(gtk::CallbackAction::new(move |widget, _| {
            if on_bucket(widget) {
                select_all();
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        })),
    ));
    controller.add_shortcut(gtk::Shortcut::new(
        gtk::ShortcutTrigger::parse_string("Escape"),
        Some(gtk::CallbackAction::new(move |widget, _| {
            if on_bucket(widget) && selecting() {
                set_selecting(false);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        })),
    ));
    win.add_controller(controller);
}

fn paste(win: &DesklinkWindow, device: &str) {
    let clipboard = win.clipboard();
    let formats = clipboard.formats();
    let device = device.to_string();
    let anchor = win.clone();
    let here = current();
    if formats.contains_type(gdk::FileList::static_type()) {
        clipboard.read_value_async(
            gdk::FileList::static_type(),
            glib::Priority::DEFAULT,
            gio::Cancellable::NONE,
            move |value| {
                let paths: Vec<PathBuf> = value
                    .ok()
                    .and_then(|value| value.get::<gdk::FileList>().ok())
                    .map(|list| list.files().iter().filter_map(|file| file.path()).collect())
                    .unwrap_or_default();
                add_paths(&device, paths, &here);
            },
        );
    } else if formats.contains_type(gdk::Texture::static_type()) {
        clipboard.read_texture_async(gio::Cancellable::NONE, move |texture| {
            let Some(texture) = texture.ok().flatten() else {
                return;
            };
            // Encoding a big picture takes a while: not on this thread.
            let stamp = glib::DateTime::now_local()
                .ok()
                .and_then(|now| now.format("%Y-%m-%d %H.%M.%S").ok())
                .map(|stamp| stamp.to_string())
                .unwrap_or_default();
            let (device, here) = (device.clone(), here.clone());
            std::thread::spawn(move || {
                let png = texture.save_to_png_bytes();
                let _ = folders::import_bytes(
                    &device,
                    &format!("Picture {stamp}.png"),
                    png.to_vec(),
                    &here,
                );
            });
            anchor.show_feature_toast("Pasted into the Bucket");
        });
    } else {
        clipboard.read_text_async(gio::Cancellable::NONE, move |text| {
            let Some(text) = text.ok().flatten().filter(|text| !text.trim().is_empty()) else {
                anchor.show_feature_toast("Nothing to paste");
                return;
            };
            let Ok(now) = glib::DateTime::now_local() else {
                return;
            };
            let (name, bytes) = folders::pasted_text(&text, &now);
            match folders::import_bytes(&device, &name, bytes, &here) {
                Ok(()) => anchor.show_feature_toast("Pasted into the Bucket"),
                Err(error) => anchor.show_feature_toast(&error),
            }
        });
    }
}

/// The Overview's Bucket card: the last 3 items and a drop target.
pub(super) fn overview_card(device: &DeviceView) -> gtk::Box {
    let view = bucket_of(&device.id).filter(|view| view.phase != Phase::Offered);
    let card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .css_classes(["dl-card", "feature-card"])
        .build();
    let head = gtk::Box::builder().spacing(14).build();
    head.append(&icon_square("folder-symbolic", 36, "feature-icon"));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized("Files", &["card-title"]));
    let subtitle = match &view {
        Some(view) => format!(
            "{} · {}",
            match view.files {
                1 => "1 item".to_string(),
                count => format!("{count} items"),
            },
            glib::format_size(view.bytes)
        ),
        None if asked(&device.id) => format!("{} asked for a Bucket. Turn it on?", device.name),
        None => format!("Off · share anything with {}", device.name),
    };
    let line = ellipsized(&subtitle, &["card-subtitle", "numeric"]);
    words.append(&line);
    head.append(&words);
    let bar = thin_bar("Bucket sync progress");
    bar.set_visible(false);
    watch(Live::Summary {
        device: device.id.clone(),
        idle: subtitle,
        line: line.downgrade(),
        bar: bar.downgrade(),
        speed: None,
    });
    // The phone asked: the question is answered right here.
    let asking = view.is_none() && asked(&device.id);
    if asking {
        let later = super::soft_button("Not Now");
        let id = device.id.clone();
        later.connect_clicked(move |_| folders::decline(&id, BUCKET_ID));
        head.append(&later);
        let on = super::soft_button("Turn On");
        on.add_css_class("suggested-action");
        let id = device.id.clone();
        on.connect_clicked(move |_| folders::turn_on_bucket(&id));
        head.append(&on);
    } else {
        head.append(&super::page_link(
            if view.is_some() { "Open" } else { "Set Up" },
            "files",
        ));
    }
    card.append(&head);
    card.append(&bar);
    let Some(view) = view else {
        return card;
    };
    let mut recent: Vec<&Item> = view
        .items
        .iter()
        .filter(|item| !item.path.contains('/'))
        .collect();
    recent.sort_by_key(|item| std::cmp::Reverse(item.mtime));
    let list = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["bucket-recent"])
        .build();
    if recent.is_empty() {
        let hint = text(
            "Drop files here to put them in the Bucket",
            &["caption", "text-dim"],
        );
        hint.set_halign(gtk::Align::Center);
        hint.set_margin_top(10);
        hint.set_margin_bottom(10);
        list.append(&hint);
    }
    for item in recent.into_iter().take(3) {
        let row = gtk::Box::builder().spacing(10).build();
        row.append(
            &gtk::Image::builder()
                .gicon(&gio::content_type_get_symbolic_icon(
                    &gio::content_type_guess(Some(&item.path), &[]).0,
                ))
                .pixel_size(16)
                .css_classes(["text-dim"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        let name = ellipsized(&item.path, &[]);
        name.set_hexpand(true);
        row.append(&name);
        row.append(&ellipsized(
            &from_words(item, device),
            &["caption", "text-faint"],
        ));
        list.append(&row);
    }
    card.append(&list);
    accept_drops(&card, &device.id, Some(""));
    card
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_big_folder_says_how_much_is_not_drawn() {
        use super::*;
        assert!(more_note(MAX_TILES).is_none());
        let note = more_note(MAX_TILES + 904).unwrap();
        assert!(note.contains("300 of 1204"), "{note}");
    }

    #[test]
    fn the_size_warns_from_8_gb_and_says_full_at_10() {
        use super::*;
        assert!(size_notice(folders::BUCKET_WARN_BYTES).is_none());
        let (headline, detail) = size_notice(folders::BUCKET_WARN_BYTES + 1).unwrap();
        assert!(headline.contains("getting big") && detail.contains("of 10 GB"));
        let (headline, _) = size_notice(folders::BUCKET_FULL_BYTES).unwrap();
        assert!(headline.contains("full (10 GB)"));
    }

    use super::*;

    /// The phone asked for a Bucket: the question shows on the Overview card
    /// and as a badge on the sidebar's Bucket entry until it is answered.
    #[test]
    fn a_bucket_the_phone_asked_for_is_a_question_until_it_is_on() {
        use crate::device_links::folders::{Offer, Pair, PairView, Phase};
        let device = "asked-bucket-phone";
        assert!(!asked(device), "nothing asked");
        folders::debug_show(
            Vec::new(),
            vec![Offer {
                device: device.into(),
                id: BUCKET_ID.into(),
                name: "DeskLink Bucket".into(),
                bucket: true,
            }],
        );
        assert!(asked(device));
        assert!(!asked("another-phone"));
        // Turned on: no question any more.
        folders::debug_show(
            vec![PairView {
                pair: Pair {
                    id: BUCKET_ID.into(),
                    name: "DeskLink Bucket".into(),
                    root: std::env::temp_dir(),
                    device: device.into(),
                    include_hidden: false,
                    ignore: Vec::new(),
                    bucket: true,
                },
                phase: Phase::UpToDate,
                files: 0,
                last_sync: None,
                conflicts: Vec::new(),
                items: Vec::new(),
                bytes: 0,
                skipped: Vec::new(),
            }],
            Vec::new(),
        );
        assert!(!asked(device));
    }

    #[test]
    fn the_overview_card_drops_at_the_top_whatever_folder_the_page_showed() {
        assert_eq!(drop_folder(Some(""), "Trips/Rome"), "");
        assert_eq!(drop_folder(None, "Trips/Rome"), "Trips/Rome");
        assert_eq!(drop_folder(Some("A"), "Trips"), "A");
    }

    #[test]
    fn renaming_refuses_unsafe_and_taken_names() {
        let dir = std::env::temp_dir().join(format!("desklink-rename-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(dir.join("taken.txt"), b"y").unwrap();
        for bad in [
            "", "../a.txt", "x/y", ".hidden", "a\0b", "a:b", "a\\b", "a\u{7}b", "trail.", "trail ",
        ] {
            assert!(rename_to(&file, bad).is_err(), "{bad:?}");
        }
        assert!(rename_to(&file, "taken.txt").is_err());
        rename_to(&file, "b.txt").unwrap();
        assert!(dir.join("b.txt").exists() && !file.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
