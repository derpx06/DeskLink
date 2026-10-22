//! The Folders page (F8): folders kept the same here and on the phone, each
//! with its state and last sync, the phone's offers to pair a folder, and
//! conflicts to resolve.

use adw::prelude::*;
use gtk::{gio, glib};

use super::{icon_button, icon_square, text, DesklinkWindow};
use crate::device_links::device::DeviceView;
use crate::device_links::folders::{self, Conflict, PairView, Phase, Resolve};

pub(super) fn update(win: &DesklinkWindow, device: &DeviceView) {
    let key = format!(
        "{}\n{}\n{}\n{}\n{}",
        folders::generation(),
        device.id,
        device.name,
        device.feature_transport_ready,
        super::clock_time(glib::DateTime::now_local().ok())
    );
    if !win.page_changed("folders", key) {
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

    let head = gtk::Box::builder().spacing(12).build();
    head.set_widget_name("responsive");
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .build();
    words.append(&text("Two-way folder sync", &["heading"]));
    let explained = text(
        &format!(
            "Folders here and on {} kept the same while it is connected. Files deleted on \
             one side go to the trash on the other.",
            device.name
        ),
        &["text-dim", "body-small"],
    );
    explained.set_wrap(true);
    words.append(&explained);
    head.append(&words);
    let add = icon_button(
        "list-add-symbolic",
        "Add Folder…",
        &["suggested-action", "pill"],
    );
    if !device.feature_transport_ready {
        add.set_sensitive(false);
        add.set_tooltip_text(Some(&format!("Connect {} first", device.name)));
    }
    let id = device.id.clone();
    add.connect_clicked(move |button| {
        let id = id.clone();
        choose_folder(button, "Choose a Folder to Sync", move |root| {
            folders::add(&id, root)
        });
    });
    head.append(&add);
    page.append(&head);

    let offers: Vec<_> = folders::offers()
        .into_iter()
        .filter(|offer| offer.device == device.id)
        .collect();
    if !offers.is_empty() {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for offer in offers {
            // Words beside the buttons, or above them when narrow.
            let content = gtk::Box::builder()
                .spacing(12)
                .margin_top(10)
                .margin_bottom(10)
                .margin_start(12)
                .margin_end(12)
                .build();
            content.set_widget_name("responsive");
            let lead = gtk::Box::builder().spacing(12).hexpand(true).build();
            lead.append(&icon_square("folder-new-symbolic", 30, "row-icon"));
            let words = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .valign(gtk::Align::Center)
                .build();
            let title = text(
                &format!("{} wants to sync “{}”", device.name, offer.name),
                &["heading"],
            );
            title.set_wrap(true);
            words.append(&title);
            let detail = text(
                "Choose the folder on this computer it stays the same as",
                &["text-dim", "body-small"],
            );
            detail.set_wrap(true);
            words.append(&detail);
            lead.append(&words);
            content.append(&lead);
            let choose = gtk::Button::builder()
                .label("Choose Folder…")
                .valign(gtk::Align::Center)
                .css_classes(["suggested-action"])
                .build();
            let (device_id, offer_id) = (offer.device.clone(), offer.id.clone());
            choose.connect_clicked(move |button| {
                let (device_id, offer_id) = (device_id.clone(), offer_id.clone());
                choose_folder(button, "Choose the Folder to Sync", move |root| {
                    folders::accept(&device_id, &offer_id, root)
                });
            });
            let decline = super::soft_button("Decline");
            let (device_id, offer_id) = (offer.device.clone(), offer.id.clone());
            decline.connect_clicked(move |_| folders::decline(&device_id, &offer_id));
            let actions = gtk::Box::builder()
                .spacing(8)
                .halign(gtk::Align::End)
                .valign(gtk::Align::Center)
                .build();
            actions.append(&decline);
            actions.append(&choose);
            content.append(&actions);
            let row = gtk::ListBoxRow::builder()
                .activatable(false)
                .child(&content)
                .build();
            row.update_property(&[gtk::accessible::Property::Label(&format!(
                "{} wants to sync “{}”",
                device.name, offer.name
            ))]);
            list.append(&row);
        }
        page.append(&list);
    }

    let pairs: Vec<PairView> = folders::pairs()
        .into_iter()
        .filter(|view| view.pair.device == device.id)
        .collect();
    if pairs.is_empty() {
        let empty = text(
            "No folders yet. Add one, then choose where it goes on the phone.",
            &["text-faint", "body-small"],
        );
        empty.set_wrap(true);
        page.append(&empty);
    } else {
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for view in &pairs {
            list.append(&pair_row(view, &device.name));
        }
        page.append(&list);
    }
    win.replace_body(
        "folders",
        &adw::Clamp::builder().maximum_size(820).child(&page).build(),
    );
}

/// A folder chooser; `chosen` gets the folder.
fn choose_folder(
    widget: &impl IsA<gtk::Widget>,
    title: &str,
    chosen: impl Fn(std::path::PathBuf) + 'static,
) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let dialog = gtk::FileDialog::builder().title(title).modal(true).build();
    dialog.select_folder(Some(&root), gio::Cancellable::NONE, move |result| {
        if let Some(path) = result.ok().and_then(|folder| folder.path()) {
            chosen(path);
        }
    });
}

/// "~/Sync/Camera" for a path under home.
fn shown_path(path: &std::path::Path) -> String {
    let home = glib::home_dir();
    match path.strip_prefix(&home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn phase_words(view: &PairView, phone: &str) -> String {
    match &view.phase {
        Phase::Waiting => format!("Waiting for {phone}"),
        Phase::NotOnPhone => format!("Not set up on {phone}"),
        Phase::Offered => format!("Waiting for {phone} to choose a folder"),
        Phase::Scanning => "Checking for changes…".into(),
        Phase::Syncing(1) => "Syncing 1 file".into(),
        Phase::Syncing(count) => format!("Syncing {count} files"),
        Phase::TooMany => "Too many files: only the first 20,000 sync".into(),
        Phase::Problem(problem) => problem.clone(),
        Phase::UpToDate => match view.last_sync {
            Some(at) => format!(
                "Up to date · synced {}",
                glib::DateTime::from_unix_local(at / 1000)
                    .ok()
                    .and_then(|time| time.format("%b %e, %H:%M").ok())
                    .map(|time| time.to_string())
                    .unwrap_or_default()
            ),
            None => "Up to date".into(),
        },
    }
}

fn pair_row(view: &PairView, phone: &str) -> adw::ExpanderRow {
    let files = match view.files {
        0 => String::new(),
        1 => " · 1 file".into(),
        count => format!(" · {count} files"),
    };
    let conflicts = match view.conflicts.len() {
        0 => String::new(),
        1 => " · 1 conflict".into(),
        count => format!(" · {count} conflicts"),
    };
    let row = adw::ExpanderRow::builder()
        .title(&view.pair.name)
        .subtitle(format!(
            "{} ↔ {phone}{files}\n{}{conflicts}",
            shown_path(&view.pair.root),
            phase_words(view, phone)
        ))
        .use_markup(false)
        .subtitle_lines(3)
        .enable_expansion(!view.conflicts.is_empty())
        .show_enable_switch(false)
        .build();
    row.set_widget_name("pair-row");
    let icon = match view.phase {
        Phase::Problem(_) | Phase::NotOnPhone | Phase::TooMany => "dialog-warning-symbolic",
        Phase::Syncing(_) | Phase::Scanning => "emblem-synchronizing-symbolic",
        _ => "folder-symbolic",
    };
    row.add_prefix(&icon_square(icon, 30, "row-icon"));

    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Open Folder")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    open.update_property(&[gtk::accessible::Property::Label(&format!(
        "Open {}",
        view.pair.name
    ))]);
    let root = view.pair.root.clone();
    open.connect_clicked(move |button| {
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&root)));
        launcher.launch(
            button.root().and_downcast_ref::<gtk::Window>(),
            gio::Cancellable::NONE,
            |_| {},
        );
    });
    row.add_suffix(&open);
    let remove = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Stop Syncing")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    remove.update_property(&[gtk::accessible::Property::Label(&format!(
        "Stop syncing {}",
        view.pair.name
    ))]);
    let (id, name) = (view.pair.id.clone(), view.pair.name.clone());
    let phone_name = phone.to_string();
    remove.connect_clicked(move |button| {
        let id = id.clone();
        super::confirm(
            button,
            "Stop syncing this folder?",
            &format!("“{name}” stops syncing with {phone_name}. The files stay on both."),
            "Stop Syncing",
            move || folders::remove(&id),
        );
    });
    row.add_suffix(&remove);

    for conflict in &view.conflicts {
        row.add_row(&conflict_row(&view.pair.id, conflict, phone));
    }
    row
}

fn conflict_row(pair: &str, conflict: &Conflict, phone: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&conflict.path)
        .subtitle(format!("{phone}’s version: {}", conflict.copy))
        .use_markup(false)
        .subtitle_lines(2)
        .build();
    row.add_prefix(
        &gtk::Image::builder()
            .icon_name("dialog-warning-symbolic")
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    let popover = gtk::Popover::builder().child(&menu).build();
    for (words, how) in [
        ("Keep Both".to_string(), Resolve::Both),
        ("Keep This Computer’s".to_string(), Resolve::Desk),
        (format!("Keep {phone}’s"), Resolve::Phone),
    ] {
        let button = gtk::Button::builder()
            .label(words.as_str())
            .css_classes(["flat"])
            .build();
        if let Some(label) = button.child().and_downcast::<gtk::Label>() {
            label.set_xalign(0.0);
        }
        let (pair, path, popover) = (pair.to_string(), conflict.path.clone(), popover.clone());
        button.connect_clicked(move |_| {
            popover.popdown();
            folders::resolve(&pair, &path, how);
        });
        menu.append(&button);
    }
    let resolve = gtk::MenuButton::builder()
        .label("Resolve")
        .popover(&popover)
        .valign(gtk::Align::Center)
        .build();
    resolve.update_property(&[gtk::accessible::Property::Label(&format!(
        "Resolve the conflict in {}",
        conflict.path
    ))]);
    row.add_suffix(&resolve);
    row
}
