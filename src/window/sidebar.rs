//! The sidebar: the selected device's card, which opens the device
//! switcher, the page list with its counts, and the footer.

use std::cell::RefCell;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::glib;

use super::ui_kit::{button, icon_square, status_dot, status_pill, ButtonKind, Tone};
use super::{device_type_icon, DesklinkWindow, MAIN_PAGES, PAGES};
use crate::device_links::device::DeviceView;
use crate::link_glyph::{presence_dot, Presentation};

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
                    .pixel_size(24)
                    .css_classes(["nav-icon"])
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
        switcher.update_property(&[gtk::accessible::Property::Label("Paired phones")]);
        let pair = button(
            ButtonKind::Text,
            "Pair another phone",
            Some("desklink-add-symbolic"),
        );
        pair.set_action_name(Some("win.pair-new"));
        pair.set_halign(gtk::Align::Start);
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

    /// The device card: the phone's name, and under it a status
    /// dot, the word ("Connected") and the short detail ("Wi-Fi · 9 ms").
    fn update_card(&self, win: &DesklinkWindow, device: &DeviceView) {
        let words = Presentation::of(device);
        let key = format!("{}\n{words:?}", device.name);
        if *self.card_key.borrow() == key {
            return;
        }
        self.card_key.replace(key);
        let content = gtk::Box::builder().spacing(12).build();
        let lines = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        lines.append(&super::ellipsized(&device.name, &["device-name"]));
        let status = gtk::Box::builder()
            .spacing(6)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build();
        status.append(&status_dot(words.status));
        status.append(
            &gtk::Label::builder()
                .label(words.word)
                .css_classes(["dl-label"])
                .build(),
        );
        if !words.short.is_empty() {
            status.append(&super::ellipsized(&words.short, &["dl-caption", "dl-mono"]));
        }
        lines.append(&status);
        content.append(&lines);
        content.append(
            &gtk::Image::builder()
                .icon_name("desklink-expand-more-symbolic")
                .pixel_size(20)
                .css_classes(["text-dim"])
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        let card = &win.imp().device_card;
        card.set_child(Some(&content));
        let label = format!("{}, {}. Switch phone", device.name, words.spoken());
        card.set_tooltip_text(Some("Switch phone"));
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
                    end.append(&super::text(&sessions.len().to_string(), &["count-badge"]));
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

    /// The switcher's rows: each paired phone, its status pill and detail.
    fn update_switcher(&self, devices: &[DeviceView], selected: &DeviceView) {
        let key = devices
            .iter()
            .map(|device| {
                format!(
                    "{}|{}|{}|{:?}",
                    device.id,
                    device.name,
                    device.device_type,
                    Presentation::of(device)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
            + &selected.id;
        if *self.switcher_key.borrow() == key {
            return;
        }
        self.switcher_key.replace(key);
        self.switcher.remove_all();
        for device in devices {
            let words = Presentation::of(device);
            let content = gtk::Box::builder()
                .spacing(12)
                .margin_top(6)
                .margin_bottom(6)
                .margin_start(6)
                .margin_end(6)
                .build();
            let current = device.id == selected.id;
            let tone = if current { Tone::Accent } else { Tone::Neutral };
            content.append(&icon_square(device_type_icon(&device.device_type), tone));
            let lines = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .hexpand(true)
                .valign(gtk::Align::Center)
                .build();
            lines.append(&super::ellipsized(&device.name, &["heading"]));
            let status = gtk::Box::builder().spacing(8).build();
            status.append(&status_pill(words.status, words.word));
            if !words.short.is_empty() {
                status.append(&super::ellipsized(&words.short, &["dl-caption", "dl-mono"]));
            }
            lines.append(&status);
            content.append(&lines);
            if current {
                content.append(
                    &gtk::Image::builder()
                        .icon_name("desklink-check-symbolic")
                        .pixel_size(20)
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
                "{}, {}{}",
                device.name,
                words.spoken(),
                if current { ", selected" } else { "" }
            ))]);
            self.switcher.append(&row);
        }
    }
}

/// "Drop to send to Pixel 9": files dragged anywhere over the window are
/// sent to the selected phone once it is connected. A target below (the Files
/// page, the Bucket card) takes its own drops first.
pub(super) fn install_window_drop(win: &DesklinkWindow) {
    let imp = win.imp();
    let target = |window: &glib::WeakRef<DesklinkWindow>| {
        let window = window.upgrade()?;
        let daemon = window.daemon()?;
        let id = window.selected_id()?;
        let device = window
            .known_devices(&daemon)
            .into_iter()
            .find(|known| known.id == id && known.trusted && known.feature_transport_ready)?;
        Some((window, daemon, device))
    };
    let weak = win.downgrade();
    let hover_weak = win.downgrade();
    super::accept_dropped_files_hover(
        &imp.split_view.get(),
        move || target(&weak).map(|(_, daemon, device)| (daemon, device.id)),
        move |over| {
            let Some(window) = hover_weak.upgrade() else {
                return;
            };
            let imp = window.imp();
            if over {
                let name = window
                    .selected_id()
                    .and_then(|id| {
                        let daemon = window.daemon()?;
                        window
                            .known_devices(&daemon)
                            .into_iter()
                            .find(|device| device.id == id)
                            .map(|device| device.name)
                    })
                    .unwrap_or_default();
                imp.drop_hint.set_label(&drop_words(&name));
                imp.split_view.add_css_class("drop-over");
            } else {
                imp.split_view.remove_css_class("drop-over");
            }
            imp.drop_hint.set_visible(over);
        },
    );
}

/// The words over a window files are dragged across.
pub(super) fn drop_words(name: &str) -> String {
    if name.is_empty() {
        "Drop to send".to_string()
    } else {
        format!("Drop to send to {name}")
    }
}
