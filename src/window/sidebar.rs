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
    /// The words that move with the round trip, set in place: the card's
    /// detail, and each switcher row's (device id, row, detail).
    switcher_details: RefCell<Vec<(String, gtk::ListBoxRow, gtk::Label)>>,
    /// What the rail was last applied for (rail, card, rows): the tree is
    /// walked only when it changes.
    rail_key: RefCell<String>,
    /// The phone card's words for a screen reader, which the rail also uses
    /// as its tooltip.
    card_label: RefCell<String>,
    /// Rows whose tooltip the rail set (so leaving it clears only those).
    rail_tips: RefCell<Vec<String>>,
    /// Whether the header showed its title before the rail hid it (a crowded
    /// window-button layout hides it on its own).
    title_before: std::cell::Cell<Option<bool>>,
}

/// What the device card is drawn from: not the round trip.
fn card_key(device: &DeviceView) -> String {
    format!("{}\n{:?}", device.name, Presentation::steady(device))
}

/// What the switcher is drawn from: not the round trips.
fn switcher_key(devices: &[DeviceView], selected: &DeviceView) -> String {
    devices
        .iter()
        .map(|device| {
            format!(
                "{}|{}|{}|{:?}",
                device.id,
                device.name,
                device.device_type,
                Presentation::steady(device)
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
        + &selected.id
}

/// What a switcher row says to a screen reader.
fn row_label(device: &DeviceView, words: &Presentation, current: bool) -> String {
    format!(
        "{}, {}{}",
        device.name,
        words.spoken(),
        if current { ", selected" } else { "" }
    )
}

/// Every label under `widget`.
fn labels_in(widget: &gtk::Widget) -> Vec<gtk::Label> {
    let mut out = Vec::new();
    let mut stack = vec![widget.clone()];
    while let Some(next) = stack.pop() {
        if let Some(label) = next.downcast_ref::<gtk::Label>() {
            out.push(label.clone());
        }
        let mut child = next.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            stack.push(c);
        }
    }
    out
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
        // A letter typed on the page lists or the search field starts "Find
        // anything" with it.
        for widget in [
            imp.nav_list.upcast_ref::<gtk::Widget>(),
            imp.footer_list.upcast_ref(),
            imp.sidebar_search.upcast_ref(),
        ] {
            let keys = gtk::EventControllerKey::new();
            let window = win.downgrade();
            keys.connect_key_pressed(move |_, key, _, state| {
                let letter = key.to_unicode().filter(|c| c.is_alphanumeric());
                let plain = !state.intersects(
                    gtk::gdk::ModifierType::CONTROL_MASK
                        | gtk::gdk::ModifierType::ALT_MASK
                        | gtk::gdk::ModifierType::SUPER_MASK,
                );
                match (letter, window.upgrade()) {
                    (Some(letter), Some(window)) if plain && !super::palette::is_open() => {
                        super::palette::toggle(&window, &letter.to_string());
                        glib::Propagation::Stop
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
            widget.add_controller(keys);
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
            switcher_details: RefCell::default(),
            rail_key: RefCell::default(),
            card_label: RefCell::default(),
            rail_tips: RefCell::default(),
            title_before: std::cell::Cell::new(None),
        }
    }

    /// The sidebar as a rail (icons, counts and the phone's dot) or as the
    /// full list: every name is hidden in a rail and said in the row's
    /// tooltip instead (a row greyed for a reason keeps its own tooltip).
    pub(super) fn set_rail(&self, win: &DesklinkWindow, rail: bool) {
        let imp = win.imp();
        let key = format!(
            "{rail}|{}|{}",
            self.card_key.borrow(),
            self.rows_key.borrow()
        );
        if *self.rail_key.borrow() == key {
            return;
        }
        self.rail_key.replace(key);
        for widget in [
            imp.nav_list.upcast_ref::<gtk::Widget>(),
            imp.footer_list.upcast_ref(),
        ] {
            if rail {
                widget.add_css_class("rail");
            } else {
                widget.remove_css_class("rail");
            }
        }
        // The header: its title and the two buttons that would keep the
        // sidebar wider than the rail (the phone card pairs, the search field
        // below finds).
        if rail {
            if self.title_before.get().is_none() {
                self.title_before
                    .set(Some(imp.sidebar_header.shows_title()));
            }
            imp.sidebar_header.set_show_title(false);
        } else if let Some(before) = self.title_before.take() {
            imp.sidebar_header.set_show_title(before);
        }
        for button in super::named_widgets(imp.sidebar_header.upcast_ref(), "rail-hide") {
            button.set_visible(!rail);
        }
        let mut tips = self.rail_tips.borrow_mut();
        for (page, row, _) in &self.rows {
            let title = PAGES
                .iter()
                .find(|(name, _, _)| name == page)
                .map_or("", |(_, title, _)| *title);
            for label in labels_in(row.upcast_ref()) {
                if label.has_css_class("nav-label") {
                    label.set_visible(!rail);
                }
            }
            // The icon and its count centred in the rail.
            if let Some(content) = row.child() {
                content.set_halign(if rail {
                    gtk::Align::Center
                } else {
                    gtk::Align::Fill
                });
            }
            if rail && row.tooltip_text().is_none() {
                row.set_tooltip_text(Some(title));
                tips.push(page.clone());
            } else if !rail && tips.contains(page) {
                row.set_tooltip_text(None);
            }
        }
        if !rail {
            tips.clear();
        }
        // The phone's card and the search: their words go, their glyphs stay;
        // the card's tooltip says who it is.
        for button in [
            imp.device_card.upcast_ref::<gtk::Widget>(),
            imp.sidebar_search.upcast_ref(),
        ] {
            for label in labels_in(button) {
                label.set_visible(!rail);
            }
        }
        let tip = if rail {
            self.card_label.borrow().clone()
        } else {
            "Switch phone".to_string()
        };
        imp.device_card.set_tooltip_text(Some(&tip));
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
        // Not while the keyboard is in the list: arrowing to a row selects it,
        // and a refresh must not pull the selection back under the user.
        let focus = gtk::prelude::RootExt::focus(win);
        let in_list = self.rows.iter().any(|(_, row, _)| {
            row.parent().is_some_and(|list| {
                focus
                    .as_ref()
                    .is_some_and(|focus| *focus == list || focus.is_ancestor(&list))
            })
        });
        if !in_list {
            self.select(&page);
        }
        self.update_card(win, device);
        // Files moving to or from this phone now, for the Files row's count.
        // A row with no device id (a debug build's fake transfer) counts for
        // every phone, as on Home's Files card.
        let moving = win.daemon().map_or(0, |daemon| {
            super::transfers::rows(&daemon)
                .iter()
                .filter(|row| row.device_id == device.id || row.device_id.is_empty())
                .count()
        });
        self.update_rows(device, moving);
        // The redrawn card and rows keep the rail's look.
        self.set_rail(win, win.imp().rail.get());
        self.update_switcher(devices, device);
        self.update_live_text(win, devices, device);
    }

    /// The round trip in the card and the switcher, in place: nothing is
    /// rebuilt for it, so an open switcher keeps its focus and its clicks.
    fn update_live_text(&self, win: &DesklinkWindow, devices: &[DeviceView], device: &DeviceView) {
        let words = Presentation::of(device);
        win.imp()
            .device_card
            .update_property(&[gtk::accessible::Property::Label(&format!(
                "{}, {}. Switch phone",
                device.name,
                words.spoken()
            ))]);
        for (id, row, label) in self.switcher_details.borrow().iter() {
            let Some(known) = devices.iter().find(|known| &known.id == id) else {
                continue;
            };
            let words = Presentation::of(known);
            if label.text() != words.short {
                label.set_text(&words.short);
            }
            row.update_property(&[gtk::accessible::Property::Label(&row_label(
                known,
                &words,
                known.id == device.id,
            ))]);
        }
    }

    /// The device card: the phone's name, and under it a status
    /// dot, the word ("Connected") and the short detail ("Wi-Fi · 9 ms").
    fn update_card(&self, win: &DesklinkWindow, device: &DeviceView) {
        let words = Presentation::of(device);
        let key = card_key(device);
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
        self.card_label.replace(label.clone());
    }

    /// The counts at the end of the page rows.
    fn update_rows(&self, device: &DeviceView, moving: usize) {
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
            "{}\n{shared}\n{unread}\n{}\n{bucket_asked}\n{moving}\n{}",
            sessions.len(),
            device.trusted,
            device.name
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
            let open = device.trusted || name == "home";
            row.set_sensitive(open);
            // Greyed, never silent: the pointer and a screen reader hear why.
            let why = (!open).then(|| format!("Opens once {} is paired", device.name));
            row.set_tooltip_text(why.as_deref());
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
                "files" if moving > 0 => {
                    end.append(&super::text(&moving.to_string(), &["count-badge"]));
                    label.push_str(&format!(", {moving} moving"));
                    if bucket_asked {
                        label.push_str(", a question is waiting");
                    }
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
            if let Some(why) = &why {
                label.push_str(&format!(", {why}"));
            }
            row.update_property(&[gtk::accessible::Property::Label(&label)]);
        }
    }

    /// The switcher's rows: each paired phone, its status pill and detail.
    fn update_switcher(&self, devices: &[DeviceView], selected: &DeviceView) {
        let key = switcher_key(devices, selected);
        if *self.switcher_key.borrow() == key {
            return;
        }
        self.switcher_key.replace(key);
        self.switcher.remove_all();
        let mut details = Vec::new();
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
            let name = super::ellipsized(&device.name, &["heading"]);
            name.set_max_width_chars(28);
            lines.append(&name);
            let status = gtk::Box::builder().spacing(8).build();
            status.append(&status_pill(words.status, words.word));
            let detail = super::ellipsized(&words.short, &["dl-caption"]);
            status.append(&detail);
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
            row.update_property(&[gtk::accessible::Property::Label(&row_label(
                device, &words, current,
            ))]);
            self.switcher.append(&row);
            details.push((device.id.clone(), row, detail));
        }
        self.switcher_details.replace(details);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device::DeviceStatus;
    use crate::device_links::device_info::DeviceInfo;

    fn phone() -> DeviceView {
        let info = DeviceInfo::local("p".into(), "Pixel 9".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = true;
        device.connection_path = Some("lan".into());
        device.rtt_ms = Some(9);
        device
    }

    /// A heartbeat changes the round trip every few seconds: the card and
    /// the switcher are not rebuilt for it (a rebuild drops a click and the
    /// focus), but they are for a change of words.
    #[test]
    fn the_card_and_the_switcher_are_not_redrawn_for_the_round_trip() {
        let mut device = phone();
        let (card, switcher) = (
            card_key(&device),
            switcher_key(std::slice::from_ref(&device), &device),
        );
        device.rtt_ms = Some(87);
        assert_eq!(card_key(&device), card);
        assert_eq!(
            switcher_key(std::slice::from_ref(&device), &device),
            switcher
        );
        device.rtt_ms = None;
        assert_eq!(card_key(&device), card);

        device.feature_transport_ready = false;
        assert_ne!(card_key(&device), card, "Connecting is news");
        device.feature_transport_ready = true;
        device.name = "Pixel 10".into();
        assert_ne!(card_key(&device), card);
        assert_ne!(
            switcher_key(std::slice::from_ref(&device), &device),
            switcher
        );
    }

    /// The round trip is in the words a screen reader gets, and in the label
    /// that is set in place.
    #[test]
    fn the_round_trip_is_in_the_live_words() {
        let mut device = phone();
        assert_eq!(Presentation::of(&device).short, "Wi-Fi · 9 ms");
        device.rtt_ms = Some(41);
        assert_eq!(
            row_label(&device, &Presentation::of(&device), true),
            "Pixel 9, Connected over Wi-Fi · 41 ms, selected"
        );
    }
}
