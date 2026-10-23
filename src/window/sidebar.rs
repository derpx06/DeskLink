//! The sidebar: the selected device's card, which opens the device
//! switcher, the page list with its counts, and the footer.

use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;

use super::{device_status_presentation, device_type_icon, DesklinkWindow, MAIN_PAGES, PAGES};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::link_glyph::{link_glyph, presence_dot, LinkState};

/// Where the last selected device is kept: `~/.config/desklink/last-device`,
/// beside the `appearance` file. Its own file, so the pairing records are
/// never rewritten for it.
fn last_device_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("desklink").join("last-device"))
}

pub(super) fn load_last_device() -> Option<String> {
    let text = std::fs::read_to_string(last_device_path()?).ok()?;
    let id = text.trim();
    (!id.is_empty()).then(|| id.to_string())
}

pub(super) fn save_last_device(device_id: &str) {
    let Some(path) = last_device_path() else {
        return;
    };
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(error) = std::fs::write(&path, device_id) {
        eprintln!("[UI] Could not remember the selected device: {error}");
    }
}

#[derive(Debug)]
pub(super) struct Sidebar {
    /// Each page's row and the box at its end that holds its count.
    rows: Vec<(String, gtk::ListBoxRow, gtk::Box)>,
    /// What each part was drawn from, to redraw only on change.
    card_key: RefCell<String>,
    rows_key: RefCell<String>,
    switcher_key: RefCell<String>,
    switcher: gtk::ListBox,
}

impl Sidebar {
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        let imp = win.imp();
        let mut rows = Vec::new();
        for (index, (name, title, icon)) in PAGES.iter().enumerate() {
            let content = gtk::Box::builder().spacing(12).build();
            content.append(
                &gtk::Image::builder()
                    .icon_name(*icon)
                    .accessible_role(gtk::AccessibleRole::Presentation)
                    .build(),
            );
            content.append(
                &gtk::Label::builder()
                    .label(*title)
                    .xalign(0.0)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .css_classes(["nav-label"])
                    .build(),
            );
            let end = gtk::Box::builder()
                .spacing(8)
                .valign(gtk::Align::Center)
                .build();
            content.append(&end);
            let row = gtk::ListBoxRow::builder().child(&content).build();
            row.set_widget_name(name);
            row.update_property(&[gtk::accessible::Property::Label(title)]);
            if index < MAIN_PAGES {
                imp.nav_list.append(&row);
            } else {
                imp.footer_list.append(&row);
            }
            rows.push((name.to_string(), row, end));
        }
        for list in [&imp.nav_list, &imp.footer_list] {
            let window = win.downgrade();
            list.connect_row_activated(move |_, row| {
                if let Some(window) = window.upgrade() {
                    window.show_page(&row.widget_name());
                }
            });
        }

        let switcher = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["switcher-list"])
            .build();
        switcher.update_property(&[gtk::accessible::Property::Label("Paired devices")]);
        let pair = super::icon_button("list-add-symbolic", "Pair New Device…", &["flat"]);
        pair.set_action_name(Some("win.pair-new"));
        if let Some(content) = pair.child() {
            content.set_halign(gtk::Align::Start);
        }
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .width_request(280)
            .build();
        content.append(&switcher);
        content.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        content.append(&pair);
        let popover = gtk::Popover::builder()
            .child(&content)
            .has_arrow(false)
            .css_classes(["device-switcher"])
            .build();
        {
            let popover = popover.downgrade();
            pair.connect_clicked(move |_| {
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
            });
        }
        {
            let window = win.downgrade();
            let popover = popover.downgrade();
            switcher.connect_row_activated(move |_, row| {
                if let Some(popover) = popover.upgrade() {
                    popover.popdown();
                }
                if let Some(window) = window.upgrade() {
                    window.select_device(&row.widget_name());
                }
            });
        }
        imp.device_card.set_popover(Some(&popover));

        Self {
            rows,
            card_key: RefCell::default(),
            rows_key: RefCell::default(),
            switcher_key: RefCell::default(),
            switcher,
        }
    }

    /// Selects page `name`'s row, and no row in the other list.
    pub(super) fn select(&self, name: &str) {
        for (page, row, _) in &self.rows {
            let Some(list) = row.parent().and_downcast::<gtk::ListBox>() else {
                continue;
            };
            if page == name {
                if list.selected_row().as_ref() != Some(row) {
                    list.select_row(Some(row));
                }
            } else if list.selected_row().as_ref() == Some(row) {
                list.unselect_row(row);
            }
        }
    }

    pub(super) fn update(&self, win: &DesklinkWindow, devices: &[DeviceView], device: &DeviceView) {
        let page = win.imp().page.borrow().clone();
        self.select(&page);
        self.update_card(win, device);
        self.update_rows(device);
        self.update_switcher(devices, device);
    }

    /// The device card: link glyph, name, "Connected · Wi-Fi".
    fn update_card(&self, win: &DesklinkWindow, device: &DeviceView) {
        let state = LinkState::of(device);
        let short = if device.trusted {
            match state.round_trip(device) {
                // The glyph already says connected: room for the time.
                Some(rtt) => format!(
                    "{} · {rtt}",
                    state.short_label(device).trim_start_matches("Connected · ")
                ),
                None => state.short_label(device).to_string(),
            }
        } else {
            device_status_presentation(device).0.to_string()
        };
        let key = format!("{}\n{short}\n{state:?}", device.name);
        if *self.card_key.borrow() == key {
            return;
        }
        self.card_key.replace(key);
        let content = gtk::Box::builder().spacing(12).build();
        content.append(&link_glyph(state, state.label(device)));
        let words = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        words.append(&super::ellipsized(&device.name, &["device-name"]));
        words.append(&super::ellipsized(&short, &["caption", "text-dim"]));
        content.append(&words);
        content.append(
            &gtk::Image::builder()
                .icon_name("pan-down-symbolic")
                .css_classes(["text-dim"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        let card = &win.imp().device_card;
        card.set_child(Some(&content));
        let label = format!("{}, {short}. Switch device", device.name);
        card.set_tooltip_text(Some("Switch device"));
        card.update_property(&[gtk::accessible::Property::Label(&label)]);
    }

    /// The counts at the end of the page rows.
    fn update_rows(&self, device: &DeviceView) {
        let sessions = crate::terminal_previews::sessions();
        let shared = sessions.iter().any(|info| {
            !crate::device_links::webrtc::terminal_bridge::attached_devices(&info.id).is_empty()
        });
        let shown = crate::device_links::config::app_settings().shows_notifications(&device.id);
        // No badge for a stale list (the link is down).
        let unread = if shown && device.feature_transport_ready {
            device.notifications.len()
        } else {
            0
        };
        // The phone asked for a Bucket that is not on yet.
        let bucket_asked = device.trusted && super::bucket::asked(&device.id);
        let key = format!(
            "{}\n{shared}\n{unread}\n{}\n{bucket_asked}",
            sessions.len(),
            device.trusted
        );
        if *self.rows_key.borrow() == key {
            return;
        }
        self.rows_key.replace(key);
        for (name, row, end) in &self.rows {
            while let Some(child) = end.first_child() {
                end.remove(&child);
            }
            // A device still pairing has only its overview.
            row.set_sensitive(device.trusted || name == "home");
            let title = PAGES
                .iter()
                .find(|(page, _, _)| page == name)
                .map_or("", |(_, title, _)| *title);
            let mut label = title.to_string();
            match name.as_str() {
                "terminals" if !sessions.is_empty() => {
                    if shared {
                        end.append(&presence_dot("A phone is attached"));
                        label.push_str(", a phone is attached");
                    }
                    end.append(&super::text(
                        &sessions.len().to_string(),
                        &["mono", "nav-count"],
                    ));
                    label.push_str(&format!(", {} open", sessions.len()));
                }
                "files" if bucket_asked => {
                    end.append(&super::text("1", &["count-badge"]));
                    label.push_str(", a question is waiting");
                }
                "notifications" if unread > 0 => {
                    end.append(&super::text(&unread.to_string(), &["count-badge"]));
                    label.push_str(&format!(", {unread} new"));
                }
                _ => {}
            }
            row.update_property(&[gtk::accessible::Property::Label(&label)]);
        }
    }

    /// The switcher's rows: each paired device and its state.
    fn update_switcher(&self, devices: &[DeviceView], selected: &DeviceView) {
        let key = serde_json::to_string(
            &devices
                .iter()
                .map(|device| {
                    (
                        &device.id,
                        &device.name,
                        &device.device_type,
                        LinkState::of(device).short_label(device),
                        device_status_presentation(device).0,
                    )
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default()
            + &selected.id;
        if *self.switcher_key.borrow() == key {
            return;
        }
        self.switcher_key.replace(key);
        self.switcher.remove_all();
        for device in devices {
            let state = if device.trusted {
                LinkState::of(device).short_label(device)
            } else {
                device_status_presentation(device).0
            };
            let content = gtk::Box::builder()
                .spacing(12)
                .margin_top(8)
                .margin_bottom(8)
                .margin_start(6)
                .margin_end(6)
                .build();
            let offline = matches!(device.status, DeviceStatus::Unreachable);
            content.append(
                &gtk::Image::builder()
                    .icon_name(device_type_icon(&device.device_type))
                    .css_classes(if offline { vec!["text-dim"] } else { vec![] })
                    .accessible_role(gtk::AccessibleRole::Presentation)
                    .build(),
            );
            let words = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(1)
                .hexpand(true)
                .build();
            words.append(&super::ellipsized(&device.name, &["heading"]));
            words.append(&super::ellipsized(state, &["caption", "text-dim"]));
            content.append(&words);
            let current = device.id == selected.id;
            if current {
                content.append(
                    &gtk::Image::builder()
                        .icon_name("object-select-symbolic")
                        .css_classes(["accent"])
                        .accessible_role(gtk::AccessibleRole::Presentation)
                        .build(),
                );
            }
            let row = gtk::ListBoxRow::builder().child(&content).build();
            row.set_widget_name(&device.id);
            if current {
                row.add_css_class("current");
            }
            row.update_property(&[gtk::accessible::Property::Label(&format!(
                "{}, {state}{}",
                device.name,
                if current { ", selected" } else { "" }
            ))]);
            self.switcher.append(&row);
        }
    }
}
