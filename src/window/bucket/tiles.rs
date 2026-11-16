//! The Bucket grid: which items are drawn, each tile with its state, badge and
//! menu, renaming and opening an item.
//!
//! Sibling edges: `browse` (selection, tiles drawn), `live` (progress).

use super::*;

/// "Showing 300 of 1,204. The rest are in the folder on this computer." once
/// a level has more items than are drawn.
pub(super) fn more_note(count: usize) -> Option<String> {
    (count > MAX_TILES).then(|| {
        format!(
            "Showing {MAX_TILES} of {count}. The rest are in the Bucket folder on this computer; \
             put them in folders to see them here."
        )
    })
}

pub(super) fn grid(
    win: &DesklinkWindow,
    items: &[Item],
    view: &PairView,
    device: &DeviceView,
) -> gtk::FlowBox {
    let grid = crate::window::transfers::grid();
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
        crate::window::transfers::add_card(&grid, &tile(win, item, &root, device, &files, view));
    }
    grid
}

/// "From Pixel 9" / "From this computer".
pub(super) fn from_words(item: &Item, device: &DeviceView) -> String {
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
                .icon_name("desklink-folder-symbolic")
                .pixel_size(56)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .css_classes(["bucket-folder-icon"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
    } else if item.state == ItemState::Receiving {
        preview.add_overlay(&crate::window::transfers::type_icon(&item.path));
    } else {
        let kind = crate::thumbnails::kind_of(&path);
        let key = std::fs::metadata(&path).ok().and_then(|metadata| {
            Some(crate::thumbnails::cache_key(
                &path,
                metadata.modified().ok()?,
                metadata.len(),
            ))
        });
        crate::window::transfers::fill_preview(&preview, key.as_deref(), &path, &item.path, kind);
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
    check.append(&gtk::Image::from_icon_name("desklink-check-symbolic"));
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
            crate::bucket_progress::size_words(bytes)
        )
    } else {
        format!(
            "{} · {}",
            crate::bucket_progress::size_words(item.size),
            from_words(item, device)
        )
    };
    if item.dir {
        // Everything moving inside it, added up.
        let line = ellipsized(&meta, &["dl-secondary", "numeric"]);
        line.set_max_width_chars(18);
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
        // "340 MB of 1.2 GiB · 2 min left" and a bar, kept current.
        let line = ellipsized("Waiting…", &["dl-secondary", "numeric"]);
        line.set_max_width_chars(18);
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
        let line = ellipsized(&meta, &["dl-secondary"]);
        line.set_max_width_chars(18);
        words.append(&line);
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
            // While selecting a click only selects (the capture-phase click
            // below claims the press, but the button's own `clicked` still
            // fires on release): it never opens the card or enters a folder.
            if !opens_on_click(selecting()) {
                return;
            }
            if item.dir {
                go_to(&win, &item.path);
            } else if item.state != ItemState::Receiving {
                let index = files.iter().position(|file| file.path == path).unwrap_or(0);
                crate::window::viewer::show(&win, &files, index, &device_name);
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
            holder.append(&gtk::Image::from_icon_name("desklink-check-symbolic"));
        }
        ItemState::Sending => {
            holder.add_css_class("moving");
            holder.append(&gtk::Image::from_icon_name(
                "desklink-arrow-upward-symbolic",
            ));
        }
        ItemState::Receiving => {
            holder.add_css_class("moving");
            holder.append(&gtk::Image::from_icon_name(
                "desklink-arrow-downward-symbolic",
            ));
        }
        ItemState::Waiting => {
            holder.add_css_class("waiting");
            holder.append(&gtk::Image::from_icon_name("desklink-schedule-symbolic"));
        }
        ItemState::Conflict => {
            holder.add_css_class("conflict");
            holder.append(&gtk::Image::from_icon_name("desklink-warning-symbolic"));
        }
    }
    Some(holder.upcast())
}

/// The tile's "⋯": Open, Show in Folder, Copy, Rename, Remove from Both,
/// and for a conflict, its choices.
pub(super) fn item_menu(
    item: &Item,
    path: &Path,
    device: &DeviceView,
    view: &PairView,
) -> gtk::MenuButton {
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
        let popover = popover.downgrade();
        button.connect_clicked(move |button| {
            if let Some(popover) = popover.upgrade() {
                popover.popdown();
            }
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
            "Show in folder",
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
                ("Keep both".to_string(), folders::Resolve::Both),
                ("Keep this computer's".to_string(), folders::Resolve::Desk),
                (format!("Keep {}'s", device.name), folders::Resolve::Phone),
            ]
            .into_iter()
            .chain(
                folders::replace_anyway_offered(&device.id, &conflict.path).then(|| {
                    (
                        format!("Replace with {}'s anyway", device.name),
                        folders::Resolve::PhoneAnyway,
                    )
                }),
            ) {
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
        "Remove from both…",
        Box::new(move |button| {
            let (rel, id) = (rel.clone(), id.clone());
            crate::window::confirm(
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
    mark_danger(&remove);
    let button = gtk::MenuButton::builder()
        .icon_name("desklink-more-vert-symbolic")
        .popover(&popover)
        .halign(gtk::Align::End)
        .valign(gtk::Align::End)
        .margin_end(4)
        .margin_bottom(4)
        .css_classes(["dl-icon-btn", "bucket-menu"])
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
pub(super) fn rename_to(path: &Path, name: &str) -> Result<(), String> {
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
    // A name that differs from a neighbour's only in capital letters is one
    // name to the phone: one of the two would silently stop syncing.
    let own = path.file_name().map(|own| own.to_os_string());
    let wanted = folders::fold_name(name);
    let clashes = target.parent().is_some_and(|dir| {
        std::fs::read_dir(dir).is_ok_and(|entries| {
            entries.flatten().any(|entry| {
                Some(entry.file_name()) != own
                    && folders::fold_name(&entry.file_name().to_string_lossy()) == wanted
            })
        })
    });
    if clashes {
        return Err("A name that differs only in capital letters is already there".into());
    }
    std::fs::rename(path, &target).map_err(|_| "Could not rename it".to_string())
}

pub(super) fn launch(widget: &impl IsA<gtk::Widget>, path: &Path) {
    gtk::FileLauncher::new(Some(&gio::File::for_path(path))).launch(
        widget.root().and_downcast_ref::<gtk::Window>(),
        gio::Cancellable::NONE,
        |_| {},
    );
}

/// "~/DeskLink Bucket".
pub(super) fn shown_path(path: &Path) -> String {
    let home = glib::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}
