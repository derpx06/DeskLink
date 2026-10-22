//! The Bucket page (F8b): one shared space per phone. Any file put in it —
//! dropped, pasted, sent from Files, saved into the folder — shows up on the
//! phone too. A thumbnail grid, newest first, each with its state and menu.
//! Files only for now: folders are refused, out loud.

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
}

/// This phone's Bucket, if it is on (or being turned on).
pub(super) fn bucket_of(device: &str) -> Option<PairView> {
    folders::pairs()
        .into_iter()
        .find(|view| view.pair.device == device && view.pair.id == BUCKET_ID && view.pair.bucket)
}

pub(super) fn update(win: &DesklinkWindow, device: &DeviceView) {
    install_paste(win);
    let key = format!(
        "{}\n{}\n{}\n{}\n{}",
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
            accept_drops(&page, &device.id);
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
    page.append(&header(view, device));
    if let Some(banner) = problem_banner(view) {
        page.append(&banner);
    }
    if view.items.is_empty() {
        page.append(&empty_zone(device));
    } else {
        page.append(&grid(win, &view.items, view, device));
        let hint = text(
            "Drop files anywhere here, or press Ctrl+V to paste.",
            &["caption", "text-faint"],
        );
        hint.set_halign(gtk::Align::Center);
        page.append(&hint);
    }
    if !view.skipped.is_empty() {
        page.append(&not_synced(&view.skipped));
    }
    accept_drops(page, &device.id);
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
        &format!("Not synced: folders ({names}). The Bucket holds files only for now."),
        &["caption", "text-faint"],
    );
    label.set_wrap(true);
    line.append(&label);
    line
}

/// The status line, storage, and the Add and folder buttons.
fn header(view: &PairView, device: &DeviceView) -> gtk::Box {
    let head = gtk::Box::builder().spacing(12).build();
    head.set_widget_name("responsive");
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&text("Bucket", &["bucket-heading"]));
    let state = gtk::Box::builder().spacing(8).build();
    let connected = device.feature_transport_ready;
    state.append(
        &gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes(["dot", if connected { "mint" } else { "faint" }])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let mut parts = vec![if connected {
        format!("With {}", device.name)
    } else {
        format!("{} is away · changes sync when it is back", device.name)
    }];
    if connected {
        match &view.phase {
            Phase::Syncing(count) => parts.push(format!(
                "Syncing {count} {}",
                if *count == 1 { "item" } else { "items" }
            )),
            Phase::Scanning => parts.push("Checking…".into()),
            _ => parts.extend(synced_words(view.last_sync)),
        }
    }
    parts.push(glib::format_size(view.bytes).to_string());
    let line = text(&parts.join(" · "), &["caption", "text-dim"]);
    line.set_wrap(true);
    state.append(&line);
    words.append(&state);
    head.append(&words);

    let actions = gtk::Box::builder()
        .spacing(8)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::End)
        .build();
    actions.append(&add_button(&device.id));
    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Open in Files")
        .valign(gtk::Align::Center)
        .css_classes(["circular"])
        .build();
    open.update_property(&[gtk::accessible::Property::Label("Open the Bucket in Files")]);
    let root = view.pair.root.clone();
    open.connect_clicked(move |button| launch(button, &root));
    actions.append(&open);
    actions.append(&more_menu(view, device));
    head.append(&actions);
    head
}

/// "Add Files…".
fn add_button(device: &str) -> gtk::Button {
    let button = icon_button(
        "list-add-symbolic",
        "Add Files…",
        &["suggested-action", "pill"],
    );
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
        _ if view.bytes > folders::BUCKET_WARN_BYTES => (
            "drive-harddisk-symbolic",
            "The Bucket is getting big".to_string(),
            format!(
                "{} — the phone keeps a copy of everything in it.",
                glib::format_size(view.bytes)
            ),
            false,
        ),
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
            .filter(|item| item.state != ItemState::Receiving)
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
    /// Tiles drawn; past it, the rest are in Files.
    const MAX_TILES: usize = 300;
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

/// One item: its picture, name, size and origin, its state, its menu.
fn tile(
    win: &DesklinkWindow,
    item: &Item,
    root: &Path,
    device: &DeviceView,
    files: &Rc<Vec<LoggedFile>>,
    view: &PairView,
) -> gtk::Overlay {
    let path = root.join(&item.path);
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
    if item.state == ItemState::Receiving {
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
    card.append(&preview);
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .css_classes(["file-card-words", "bucket-words"])
        .build();
    let name = ellipsized(&item.path, &["file-name"]);
    name.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    name.set_max_width_chars(14);
    words.append(&name);
    let meta = format!(
        "{} · {}",
        glib::format_size(item.size),
        from_words(item, device)
    );
    words.append(&ellipsized(&meta, &["caption", "text-dim"]));
    card.append(&words);
    let button = gtk::Button::builder()
        .child(&card)
        .css_classes(["file-card"])
        .tooltip_text(&item.path)
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&format!(
        "{}, {}, {}",
        &item.path,
        meta,
        state_words(item, device)
    ))]);
    {
        let (item, device_id, win) = (item.clone(), device.id.clone(), win.downgrade());
        let (files, device_name, path) = (Rc::clone(files), device.name.clone(), path.clone());
        let _ = device_id;
        button.connect_clicked(move |_| {
            if item.state != ItemState::Receiving {
                if let Some(win) = win.upgrade() {
                    let index = files.iter().position(|file| file.path == path).unwrap_or(0);
                    super::viewer::show(&win, &files, index, &device_name);
                }
            }
        });
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
        ItemState::Sending | ItemState::Receiving => {
            holder.add_css_class("moving");
            holder.append(&gtk::Spinner::builder().spinning(true).build());
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
    let (name, phone) = (item.path.clone(), device.name.clone());
    let remove = add(
        "Remove from Both…",
        Box::new(move |button| {
            let path = path.clone();
            super::confirm(
                button,
                &format!("Remove “{name}”?"),
                &format!("It goes to the trash here and on {phone}."),
                "Remove",
                move || trash(&path),
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

/// To the trash as one item (a folder whole); the scan syncs the removal.
fn trash(path: &Path) {
    gio::File::for_path(path).trash_async(
        glib::Priority::DEFAULT,
        gio::Cancellable::NONE,
        |result| {
            if result.is_err() {
                eprintln!("[Bucket] Could not move an item to the trash");
            }
        },
    );
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
    if crate::device_links::webrtc::file_protocol::validate_relpath(name).is_err()
        || name.contains('/')
        || name.starts_with('.')
    {
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

/// Puts the files among `paths` in `device`'s Bucket and says so; a folder
/// among them is refused out loud (files only for now).
fn add_paths(anchor: &impl IsA<gtk::Widget>, device: &str, paths: Vec<PathBuf>) -> bool {
    let (files, folder) = folders::split_files(paths);
    let count = files.len();
    let added = count > 0 && folders::import(device, files);
    let message = match (added, folder) {
        (_, true) => folders::FOLDERS_NOT_YET.to_string(),
        (true, false) if count == 1 => "Added to the Bucket".to_string(),
        (true, false) => format!("Added {count} files to the Bucket"),
        (false, false) => return false,
    };
    toast_from(anchor, &message);
    added
}

/// "Add Files…".
fn choose(widget: &impl IsA<gtk::Widget>, device: &str) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let device = device.to_string();
    let anchor = widget.as_ref().clone();
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
            add_paths(&anchor, &device, paths);
        }
    });
}

/// Files dropped anywhere on `target` go in the Bucket.
pub(super) fn accept_drops(target: &impl IsA<gtk::Widget>, device: &str) {
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    let device = device.to_string();
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
        add_paths(&shown, &device, paths)
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
        if win.imp().page.borrow().as_str() != "bucket" {
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
    win.add_controller(controller);
}

fn paste(win: &DesklinkWindow, device: &str) {
    let clipboard = win.clipboard();
    let formats = clipboard.formats();
    let device = device.to_string();
    let anchor = win.clone();
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
                add_paths(&anchor, &device, paths);
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
            let device = device.clone();
            std::thread::spawn(move || {
                let png = texture.save_to_png_bytes();
                let _ =
                    folders::import_bytes(&device, &format!("Picture {stamp}.png"), png.to_vec());
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
            match folders::import_bytes(&device, &name, bytes) {
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
    words.append(&ellipsized("Bucket", &["card-title"]));
    let subtitle = match &view {
        Some(view) => format!(
            "{} · {}",
            match view.files {
                1 => "1 item".to_string(),
                count => format!("{count} items"),
            },
            glib::format_size(view.bytes)
        ),
        None => format!("Off · share anything with {}", device.name),
    };
    words.append(&ellipsized(&subtitle, &["card-subtitle"]));
    head.append(&words);
    head.append(&super::page_link(
        if view.is_some() { "Open" } else { "Set Up" },
        "bucket",
    ));
    card.append(&head);
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
    accept_drops(&card, &device.id);
    card
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renaming_refuses_unsafe_and_taken_names() {
        let dir = std::env::temp_dir().join(format!("desklink-rename-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        std::fs::write(dir.join("taken.txt"), b"y").unwrap();
        for bad in ["", "../a.txt", "x/y", ".hidden", "a\0b"] {
            assert!(rename_to(&file, bad).is_err(), "{bad:?}");
        }
        assert!(rename_to(&file, "taken.txt").is_err());
        rename_to(&file, "b.txt").unwrap();
        assert!(dir.join("b.txt").exists() && !file.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
