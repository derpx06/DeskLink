//! The Bucket page's chrome: the selection bar and move dialog, the header with
//! its breadcrumb, add button and menu, size and problem banners, the empty zone.
//!
//! Sibling edges: `browse` (selection), `live` (progress), `tiles` (open and
//! show a path), `adding` (the add button).

use super::*;

/// The bar shown while selecting: the count and the bulk actions.
pub(super) fn selection_bar(
    win: &DesklinkWindow,
    view: &PairView,
    device: &DeviceView,
) -> gtk::Box {
    let bar = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .css_classes(["state-banner", "files-banner", "selection-bar"])
        .visible(false)
        .build();
    bar.update_property(&[gtk::accessible::Property::Label("Selection actions")]);
    let top = gtk::Box::builder().spacing(8).build();
    let count = text("Select items", &["files-banner-title"]);
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
    // With more items than are drawn it says so, and reaches only those drawn.
    let hidden = BROWSE.with(|browse| browse.borrow().hidden);
    let all = menu_item(&if hidden > 0 {
        format!("Select all {MAX_TILES} shown")
    } else {
        "Select all".to_string()
    });
    all.connect_clicked(|_| select_all());
    add(&all);
    let move_to = menu_item("Move to folder…");
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
    let remove = menu_item("Remove from both…");
    mark_danger(&remove);
    {
        let (id, phone) = (device.id.clone(), device.name.clone());
        remove.connect_clicked(move |button| {
            let paths = selected_paths();
            let count = paths.len();
            if count == 0 {
                return;
            }
            let id = id.clone();
            crate::window::confirm(
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
pub(super) fn not_synced(folders: &[String]) -> gtk::Box {
    let line = gtk::Box::builder()
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    line.append(
        &gtk::Image::builder()
            .icon_name("desklink-folder-symbolic")
            .pixel_size(16)
            .css_classes(["bucket-folder-icon"])
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let names = folders.join(", ");
    let label = text(
        &format!("Not synced ({}): {names}", folders.len()),
        &["dl-caption"],
    );
    label.set_wrap(true);
    line.append(&label);
    line
}

/// The status line, storage, and the Add and folder buttons.
pub(super) fn header(win: &DesklinkWindow, view: &PairView, device: &DeviceView) -> gtk::Box {
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
    state.append(&ui_kit::status_dot(if connected {
        Status::Ok
    } else {
        Status::Idle
    }));
    let mut parts = Vec::new();
    let mut ages = false;
    if connected {
        match &view.phase {
            Phase::Syncing(count) => parts.push(format!(
                "Syncing {count} {}",
                if *count == 1 { "item" } else { "items" }
            )),
            Phase::Scanning => parts.push("Checking…".into()),
            _ => {
                parts.push("Up to date".to_string());
                ages = true;
            }
        }
    } else {
        parts.push(format!(
            "{} is away · changes sync when it is back",
            device.name
        ));
    }
    let size = crate::bucket_progress::size_words(view.bytes).to_string();
    // "Last synced 2 min ago" changes by itself: worked out again on each
    // tick, so the page is not rebuilt (and its menus closed) every minute.
    let last_sync = view.last_sync;
    let idle_now: std::rc::Rc<dyn Fn() -> String> = {
        let (parts, size) = (parts.clone(), size.clone());
        std::rc::Rc::new(move || {
            let mut words = parts.clone();
            if ages {
                words.extend(synced_words(last_sync));
            }
            words.push(size.clone());
            words.join(" · ")
        })
    };
    let idle = idle_now();
    let line = text(&idle, &["dl-secondary", "numeric"]);
    line.set_wrap(true);
    state.append(&line);
    words.append(&state);
    // While files move: a thin bar for the whole sync, and its speed.
    let progress = gtk::Box::builder().spacing(10).margin_top(2).build();
    let bar = thin_bar("Bucket sync progress");
    bar.set_visible(false);
    progress.append(&bar);
    let speed = text("", &["dl-secondary", "numeric"]);
    speed.set_visible(false);
    progress.append(&speed);
    words.append(&progress);
    watch(Live::Summary {
        device: device.id.clone(),
        idle,
        idle_now: Some(idle_now),
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
        let new = ui_kit::icon_button("folder-new-symbolic", "New folder");
        new.set_valign(gtk::Align::Center);
        let id = device.id.clone();
        new.connect_clicked(move |_| {
            folders::new_folder(&id, &current());
        });
        actions.append(&new);
    }
    let select = gtk::ToggleButton::builder()
        .icon_name("desklink-check-circle-symbolic")
        .tooltip_text("Select")
        .valign(gtk::Align::Center)
        .css_classes(["dl-icon-btn"])
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
        let root = text("Bucket", &["bucket-heading"]);
        root.set_accessible_role(gtk::AccessibleRole::Heading);
        crumbs.append(&root);
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
            crumbs.append(&text("›", &["dl-section"]));
        }
        if index == last {
            let label = text(&name, &["dl-mono", "bucket-last-crumb"]);
            label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
            label.set_max_width_chars(20);
            crumbs.append(&label);
        } else {
            let button = gtk::Button::builder()
                .label(&name)
                .css_classes(["flat", "bucket-crumb", "dl-mono"])
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

/// "Add files…".
fn add_button(device: &str) -> gtk::Button {
    let button = ui_kit::button(
        ButtonKind::Tonal,
        "Add files…",
        Some("desklink-add-symbolic"),
    );
    button.set_valign(gtk::Align::Center);
    let device = device.to_string();
    button.connect_clicked(move |button| choose(button, &device));
    button
}

/// The page's "⋯": turning the Bucket off.
pub(super) fn more_menu(view: &PairView, device: &DeviceView) -> gtk::MenuButton {
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
        let (root, pop) = (view.pair.root.clone(), popover.downgrade());
        open_files.connect_clicked(move |button| {
            if let Some(pop) = pop.upgrade() {
                pop.popdown();
            }
            launch(button, &root);
        });
    }
    menu.append(&open_files);
    let add_folder = menu_item("Add folder…");
    {
        let (id, pop) = (device.id.clone(), popover.downgrade());
        add_folder.connect_clicked(move |button| {
            if let Some(pop) = pop.upgrade() {
                pop.popdown();
            }
            choose_folder(button, &id);
        });
    }
    menu.append(&add_folder);
    let off = menu_item("Turn off the Bucket…");
    mark_danger(&off);
    let (id, name, root) = (
        device.id.clone(),
        device.name.clone(),
        view.pair.root.clone(),
    );
    let pop = popover.downgrade();
    off.connect_clicked(move |button| {
        if let Some(pop) = pop.upgrade() {
            pop.popdown();
        }
        let id = id.clone();
        crate::window::confirm(
            button,
            "Turn off the Bucket?",
            &format!(
                "Things stop syncing with {name}. Nothing is deleted: everything stays in {} \
                 and on the phone.",
                shown_path(&root)
            ),
            "Turn off",
            move || folders::remove(&id, BUCKET_ID),
        );
    });
    menu.append(&off);
    let button = gtk::MenuButton::builder()
        .icon_name("desklink-more-vert-symbolic")
        .popover(&popover)
        .valign(gtk::Align::Center)
        .css_classes(["dl-icon-btn"])
        .tooltip_text("More")
        .build();
    button.update_property(&[gtk::accessible::Property::Label("More Bucket options")]);
    button
}

/// What needs the user: a size warning, a held empty folder, a problem.
/// What the Bucket's size says: a warning from 8 GiB, "full" at 10 GiB, where
/// adding more is refused.
pub(super) fn size_notice(bytes: u64) -> Option<(String, String)> {
    if bytes >= folders::BUCKET_FULL_BYTES {
        Some((
            "The Bucket is full (10 GiB)".to_string(),
            "Nothing more can be added until you remove something.".to_string(),
        ))
    } else if bytes > folders::BUCKET_WARN_BYTES {
        Some((
            "The Bucket is getting big".to_string(),
            format!(
                "{} of 10 GiB — the phone keeps a copy of everything in it.",
                crate::bucket_progress::size_words(bytes)
            ),
        ))
    } else {
        None
    }
}

pub(super) fn problem_banner(view: &PairView) -> Option<gtk::Box> {
    let (icon, headline, detail, confirm) = match &view.phase {
        Phase::EmptyHeld => (
            "desklink-warning-symbolic",
            "The Bucket folder is empty here".to_string(),
            "Is its drive connected? Nothing is deleted on the phone until you say so.".to_string(),
            true,
        ),
        Phase::Problem(problem) => (
            "desklink-warning-symbolic",
            "Something needs a look".to_string(),
            problem.clone(),
            false,
        ),
        Phase::TooMany => (
            "desklink-warning-symbolic",
            "The Bucket is full".to_string(),
            "Only the first 10,000 items sync.".to_string(),
            false,
        ),
        Phase::NotOnPhone => (
            "desklink-warning-symbolic",
            "Not set up on the phone".to_string(),
            "Turn on the Bucket in DeskLink on the phone.".to_string(),
            false,
        ),
        _ if size_notice(view.bytes).is_some() => {
            let (headline, detail) = size_notice(view.bytes).unwrap_or_default();
            ("desklink-inventory-symbolic", headline, detail, false)
        }
        _ => return None,
    };
    let bar = gtk::Box::builder()
        .spacing(14)
        .css_classes(["state-banner", "files-banner", "warn", "tall"])
        .build();
    bar.append(&ui_kit::icon_square(icon, Tone::Warn));
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    words.append(&ellipsized(&headline, &["files-banner-title"]));
    let detail_label = text(&detail, &["dl-secondary"]);
    detail_label.set_wrap(true);
    words.append(&detail_label);
    bar.append(&words);
    if matches!(&view.phase, Phase::Problem(problem) if problem == folders::FOLDER_MISSING) {
        let create = ui_kit::button(ButtonKind::Outline, "Create folder", None);
        create.set_valign(gtk::Align::Center);
        let root = view.pair.root.clone();
        create.connect_clicked(move |button| {
            if let Err(error) = folders::recreate_root(&root) {
                crate::window::toast_from(
                    button,
                    &format!(
                        "Could not create the folder{}",
                        crate::window::because(&error.to_string())
                    ),
                );
            }
        });
        bar.append(&create);
        let off = ui_kit::button(ButtonKind::Text, "Turn off", None);
        off.set_valign(gtk::Align::Center);
        let id = view.pair.device.clone();
        off.connect_clicked(move |button| {
            let id = id.clone();
            crate::window::confirm(
                button,
                "Turn off the Bucket?",
                "The phone keeps its copy; nothing is deleted on either side.",
                "Turn off",
                move || folders::remove(&id, BUCKET_ID),
            );
        });
        bar.append(&off);
    }
    if confirm {
        let button = ui_kit::button(ButtonKind::Outline, "It's empty on purpose", None);
        button.set_valign(gtk::Align::Center);
        let id = view.pair.device.clone();
        button.connect_clicked(move |button| {
            let id = id.clone();
            crate::window::confirm(
                button,
                "Empty the Bucket on the phone too?",
                "Everything in it is removed from the phone (to its trash).",
                "Empty it",
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

/// Nothing here yet: the empty state, as the drop zone.
pub(super) fn empty_zone(device: &DeviceView) -> gtk::Box {
    let zone = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(10)
        .height_request(240)
        .css_classes(["drop-zone", "bucket-drop"])
        .build();
    let (inner, _, _) = crate::window::transfers::empty_state(
        "desklink-inventory-symbolic",
        "Nothing here yet",
        &format!(
            "Drop files here from either device. They show up on {} too.",
            device.name
        ),
    );
    inner.set_vexpand(true);
    inner.set_valign(gtk::Align::Center);
    let ways = text(
        "Or paste with Ctrl+V, choose Add files, use Files › Scripts › Add to DeskLink \
         Bucket, or save files straight into the DeskLink Bucket folder.",
        &["dl-caption"],
    );
    ways.set_wrap(true);
    ways.set_justify(gtk::Justification::Center);
    ways.set_max_width_chars(56);
    ways.set_halign(gtk::Align::Center);
    inner.append(&ways);
    zone.append(&inner);
    zone
}
