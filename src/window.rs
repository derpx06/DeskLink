/* window.rs
 *
 * Copyright 2026 manas
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
 * GNU General Public License for more details.
 *
 * You should have received a copy of the GNU General Public License
 * along with this program.  If not, see <https://www.gnu.org/licenses/>.
 *
 * SPDX-License-Identifier: GPL-3.0-or-later
 */

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::device_links::daemon::{DaemonCommand, DaemonEvent, DaemonHandle};
use crate::device_links::device::{DeviceNotification, DeviceStatus, DeviceView, VolumeSink};
use crate::device_links::features::FeatureRegistry;
use crate::device_links::packet::{
    PACKET_TYPE_CLIPBOARD, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PING, PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SFTP_REQUEST,
    PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_SYSTEMVOLUME_REQUEST,
};

mod imp {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashSet;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/derx06/desklink/com/window.ui")]
    pub struct DesklinkWindow {
        // Template widgets
        #[template_child]
        pub devices_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub error_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub search_button: TemplateChild<gtk::ToggleButton>,
        #[template_child]
        pub search_bar: TemplateChild<gtk::SearchBar>,
        #[template_child]
        pub search_entry: TemplateChild<gtk::SearchEntry>,
        #[template_child]
        pub pair_new_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub settings_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub about_row: TemplateChild<adw::ActionRow>,
        #[template_child]
        pub detail_body: TemplateChild<gtk::Box>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        pub daemon: RefCell<Option<DaemonHandle>>,
        pub last_devices_snapshot: RefCell<Option<String>>,
        pub selected_device_id: RefCell<Option<String>>,
        pub notified_pair_requests: RefCell<HashSet<String>>,
        pub notified_remote_notifications: RefCell<HashSet<String>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DesklinkWindow {
        const NAME: &'static str = "DesklinkWindow";
        type Type = super::DesklinkWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for DesklinkWindow {
        fn constructed(&self) {
            self.parent_constructed();

            let window_widget = self.obj().clone().upcast::<gtk::Widget>();
            self.search_bar.set_key_capture_widget(Some(&window_widget));
            gtk::prelude::AccessibleExtManual::update_property(
                &*self.search_button,
                &[gtk::accessible::Property::Label("Search devices")],
            );
            self.search_button
                .bind_property("active", &*self.search_bar, "search-mode-enabled")
                .bidirectional()
                .sync_create()
                .build();

            let window = self.obj().downgrade();
            self.search_entry.connect_search_changed(move |_| {
                if let Some(window) = window.upgrade() {
                    window.refresh_devices();
                }
            });

            let window = self.obj().downgrade();
            self.devices_list.connect_row_selected(move |_, row| {
                let Some(window) = window.upgrade() else {
                    return;
                };
                let Some(row) = row else {
                    return;
                };
                let id = row.widget_name();
                if id.is_empty() {
                    return;
                }
                window
                    .imp()
                    .selected_device_id
                    .replace(Some(id.to_string()));
                window.refresh_devices();
            });

            let window = self.obj().downgrade();
            self.pair_new_row.connect_activated(move |_| {
                if let Some(window) = window.upgrade() {
                    window.show_discovery_dialog();
                }
            });

            let window = self.obj().downgrade();
            self.settings_row.connect_activated(move |_| {
                if let Some(window) = window.upgrade() {
                    let _ =
                        gtk::prelude::WidgetExt::activate_action(&window, "app.preferences", None);
                }
            });

            let window = self.obj().downgrade();
            self.about_row.connect_activated(move |_| {
                if let Some(window) = window.upgrade() {
                    let _ = gtk::prelude::WidgetExt::activate_action(&window, "app.about", None);
                }
            });
        }
    }
    impl WidgetImpl for DesklinkWindow {}
    impl WindowImpl for DesklinkWindow {}
    impl ApplicationWindowImpl for DesklinkWindow {}
    impl AdwApplicationWindowImpl for DesklinkWindow {}
}

glib::wrapper! {
    pub struct DesklinkWindow(ObjectSubclass<imp::DesklinkWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,        @implements gio::ActionGroup, gio::ActionMap;
}

impl DesklinkWindow {
    pub fn new<P: IsA<gtk::Application>>(application: &P) -> Self {
        glib::Object::builder()
            .property("application", application)
            .build()
    }

    pub fn set_daemon(&self, daemon: DaemonHandle) {
        self.imp().daemon.replace(Some(daemon));
        self.refresh_devices();

        let weak = self.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            if let Some(window) = weak.upgrade() {
                window.refresh_devices();
                glib::ControlFlow::Continue
            } else {
                glib::ControlFlow::Break
            }
        });
    }

    fn refresh_devices(&self) {
        let imp = self.imp();
        let Some(daemon) = imp.daemon.borrow().clone() else {
            return;
        };

        let errors = daemon.drain_errors();
        if let Some(error) = errors.last() {
            imp.error_banner.set_title(error);
            imp.error_banner.set_revealed(true);
            self.send_app_notification("desklink-error", "DeskLink", error);
        }

        for event in daemon.drain_events() {
            match event {
                DaemonEvent::PingReceived {
                    device_id,
                    device_name,
                    message,
                } => {
                    self.send_feature_notification(
                        &format!("desklink-ping-{device_id}-{}", glib::monotonic_time()),
                        &format!("Ping from {device_name}"),
                        message
                            .as_deref()
                            .unwrap_or("Your phone sent a Ping through DeskLink."),
                    );
                    self.show_feature_toast(&format!("Ping received from {device_name}"));
                }
                DaemonEvent::FindPhoneReceived {
                    device_id,
                    device_name,
                } => {
                    self.send_feature_notification(
                        &format!("desklink-find-phone-{device_id}-{}", glib::monotonic_time()),
                        &format!("Find Phone request from {device_name}"),
                        "Your phone asked DeskLink to ring this computer.",
                    );
                    self.show_feature_toast(&format!("Find Phone request from {device_name}"));
                }
            }
        }

        let devices = daemon.devices();
        let recent_devices: Vec<_> = devices
            .iter()
            .filter(|device| {
                device.trusted
                    || matches!(
                        device.status,
                        DeviceStatus::PairRequested | DeviceStatus::PairRequestedByPeer
                    )
            })
            .filter(|device| {
                matches!(
                    device.status,
                    DeviceStatus::Paired
                        | DeviceStatus::Unreachable
                        | DeviceStatus::PairRequested
                        | DeviceStatus::PairRequestedByPeer
                )
            })
            .cloned()
            .collect();

        let query = imp.search_entry.text().trim().to_lowercase();
        let filtered_devices: Vec<_> = recent_devices
            .iter()
            .filter(|device| {
                query.is_empty()
                    || device.name.to_lowercase().contains(&query)
                    || device.device_type.to_lowercase().contains(&query)
                    || device.address.to_lowercase().contains(&query)
                    || device.id.to_lowercase().contains(&query)
            })
            .cloned()
            .collect();

        let selected_id = imp.selected_device_id.borrow().clone();
        if selected_id
            .as_ref()
            .is_none_or(|id| filtered_devices.iter().all(|device| &device.id != id))
        {
            imp.selected_device_id
                .replace(filtered_devices.first().map(|device| device.id.clone()));
        }

        let snapshot = serde_json::to_string(&(
            &filtered_devices,
            &query,
            imp.selected_device_id.borrow().clone(),
        ))
        .unwrap_or_default();
        if imp.last_devices_snapshot.borrow().as_ref() == Some(&snapshot) {
            return;
        }
        imp.last_devices_snapshot.replace(Some(snapshot));

        while let Some(child) = imp.devices_list.first_child() {
            imp.devices_list.remove(&child);
        }

        if recent_devices.is_empty() {
            imp.notified_pair_requests.borrow_mut().clear();
            imp.notified_remote_notifications.borrow_mut().clear();
            imp.devices_list.append(
                &adw::ActionRow::builder()
                    .title("No devices yet")
                    .subtitle("Pair a device to get started.")
                    .build(),
            );
            self.show_detail_empty(
                "No device selected",
                "Pair a phone or computer to manage it from DeskLink.",
            );
            return;
        }

        if filtered_devices.is_empty() {
            imp.devices_list.append(
                &adw::ActionRow::builder()
                    .title("No matching devices")
                    .subtitle("Try a different name, type, or address.")
                    .build(),
            );
            self.show_detail_empty(
                "No matching device",
                "Clear the search to see your devices.",
            );
            return;
        }

        let current_pair_requests = recent_devices
            .iter()
            .filter(|device| matches!(device.status, DeviceStatus::PairRequestedByPeer))
            .map(|device| device.id.clone())
            .collect::<std::collections::HashSet<_>>();
        imp.notified_pair_requests
            .borrow_mut()
            .retain(|device_id| current_pair_requests.contains(device_id));
        for device in recent_devices
            .iter()
            .filter(|device| matches!(device.status, DeviceStatus::PairRequestedByPeer))
        {
            if imp
                .notified_pair_requests
                .borrow_mut()
                .insert(device.id.clone())
            {
                let body = device
                    .verification_key
                    .as_ref()
                    .map(|key| format!("Verification key: {key}"))
                    .unwrap_or_else(|| {
                        "Open DeskLink to accept or reject the request.".to_string()
                    });
                self.send_app_notification(
                    &format!("pair-request-{}", device.id),
                    &format!("Pairing request from {}", device.name),
                    &body,
                );
            }
        }

        let current_notifications = recent_devices
            .iter()
            .flat_map(|device| {
                device
                    .notifications
                    .iter()
                    .map(|notification| format!("{}:{}", device.id, notification.id))
                    .collect::<Vec<_>>()
            })
            .collect::<std::collections::HashSet<_>>();

        if errors.is_empty()
            && recent_devices
                .iter()
                .all(|device| device.last_error.is_none())
        {
            imp.error_banner.set_revealed(false);
        }

        imp.notified_remote_notifications
            .borrow_mut()
            .retain(|notification_id| current_notifications.contains(notification_id));
        for device in recent_devices.iter() {
            for notification in &device.notifications {
                let unique_id = format!("{}:{}", device.id, notification.id);
                if imp
                    .notified_remote_notifications
                    .borrow_mut()
                    .insert(unique_id.clone())
                {
                    let title = if notification.title.is_empty() {
                        format!("{} on {}", notification.app_name, device.name)
                    } else {
                        format!("{}: {}", notification.app_name, notification.title)
                    };
                    let body = if notification.text.is_empty() {
                        notification.ticker.as_str()
                    } else {
                        notification.text.as_str()
                    };
                    self.send_app_notification(
                        &format!("phone-notification-{unique_id}"),
                        &title,
                        body,
                    );
                }
            }
        }

        let selected_id = imp.selected_device_id.borrow().clone();
        for device in &filtered_devices {
            let row = device_sidebar_row(&daemon, device.clone());
            let is_selected = selected_id.as_deref() == Some(device.id.as_str());
            imp.devices_list.append(&row);
            if is_selected {
                imp.devices_list.select_row(Some(&row));
            }
        }
        self.render_selected_device(&daemon, &filtered_devices);
    }

    fn show_detail_empty(&self, title: &str, description: &str) {
        let body = &self.imp().detail_body;
        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        let page = adw::StatusPage::builder()
            .icon_name("computer-symbolic")
            .title(title)
            .description(description)
            .vexpand(true)
            .build();
        body.append(&page);
    }

    fn render_selected_device(&self, daemon: &DaemonHandle, devices: &[DeviceView]) {
        let Some(selected_id) = self.imp().selected_device_id.borrow().clone() else {
            self.show_detail_empty("No device selected", "Select a device from the sidebar.");
            return;
        };
        let Some(device) = devices.iter().find(|device| device.id == selected_id) else {
            self.show_detail_empty("No device selected", "Select a device from the sidebar.");
            return;
        };
        let body = &self.imp().detail_body;
        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        body.append(&device_row(daemon, device.clone()));
    }

    fn send_app_notification(&self, id: &str, title: &str, body: &str) {
        let Some(application) = self.application() else {
            return;
        };
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
        application.send_notification(Some(id), &notification);
    }

    fn send_feature_notification(&self, id: &str, title: &str, body: &str) {
        let Some(application) = self.application() else {
            return;
        };
        let notification = gio::Notification::new(title);
        notification.set_body(Some(body));
        notification.set_priority(gio::NotificationPriority::High);
        notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
        application.send_notification(Some(id), &notification);
    }

    fn show_feature_toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_timeout(4);
        self.imp().toast_overlay.add_toast(toast);
    }

    fn show_discovery_dialog(&self) {
        let Some(daemon) = self.imp().daemon.borrow().clone() else {
            return;
        };
        daemon.send(DaemonCommand::Discover);

        let window = adw::Window::builder()
            .transient_for(self)
            .modal(true)
            .default_width(520)
            .default_height(520)
            .build();

        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Refresh")
            .valign(gtk::Align::Center)
            .build();
        header.pack_end(&refresh);
        toolbar.add_top_bar(&header);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(18)
            .margin_bottom(24)
            .margin_start(24)
            .margin_end(24)
            .build();

        let heading = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        heading.append(
            &gtk::Label::builder()
                .label("Device Discovery")
                .xalign(0.0)
                .css_classes(["title-2"])
                .build(),
        );
        heading.append(
            &gtk::Label::builder()
                .label("Searching the local network for devices you can pair.")
                .xalign(0.0)
                .wrap(true)
                .css_classes(["body", "dim-label"])
                .build(),
        );

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .vexpand(true)
            .build();
        list.add_css_class("boxed-list");

        let scroll = gtk::ScrolledWindow::builder()
            .min_content_height(280)
            .vexpand(true)
            .child(&list)
            .build();

        content.append(&heading);
        content.append(&scroll);
        toolbar.set_content(Some(&content));
        window.set_content(Some(&toolbar));

        refresh.connect_clicked({
            let daemon = daemon.clone();
            move |_| daemon.send(DaemonCommand::Discover)
        });

        populate_discovery_list(&list, &daemon);
        let weak_list = list.downgrade();
        let weak_window = window.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            let Some(list) = weak_list.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if weak_window.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            populate_discovery_list(&list, &daemon);
            glib::ControlFlow::Continue
        });

        window.present();
    }
}

fn populate_discovery_list(list: &gtk::ListBox, daemon: &DaemonHandle) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

    let devices = daemon.devices();
    if devices.is_empty() {
        let row = adw::ActionRow::builder()
            .title("Searching for devices")
            .subtitle("Keep both devices on the same Wi-Fi network.")
            .build();
        row.add_suffix(&gtk::Spinner::builder().spinning(true).build());
        list.append(&row);
        return;
    }

    for device in devices {
        list.append(&device_row(daemon, device));
    }
}

fn device_type_icon(device_type: &str) -> &'static str {
    match device_type.to_lowercase().as_str() {
        t if t.contains("phone") || t.contains("smartphone") => "phone-symbolic",
        t if t.contains("tablet") => "input-tablet-symbolic",
        t if t.contains("tv") || t.contains("television") => "tv-symbolic",
        t if t.contains("desktop") => "computer-symbolic",
        t if t.contains("laptop") => "laptop-symbolic",
        _ => "device-symbolic",
    }
}

fn device_row(daemon: &DaemonHandle, device: DeviceView) -> gtk::Widget {
    match &device.status {
        DeviceStatus::Paired => paired_device_card(daemon, device),
        _ => unpaired_device_row(daemon, device).upcast(),
    }
}

fn device_sidebar_row(daemon: &DaemonHandle, device: DeviceView) -> adw::ActionRow {
    let status = match &device.status {
        DeviceStatus::Paired => "Connected",
        DeviceStatus::Unreachable => "Unavailable",
        _ => device.status.label(),
    };
    let row = adw::ActionRow::builder()
        .title(&device.name)
        .subtitle(format!("{} · {status}", device.device_type))
        .activatable(true)
        .build();
    row.set_widget_name(&device.id);
    row.set_use_markup(false);

    let icon = gtk::Image::builder()
        .icon_name(device_type_icon(&device.device_type))
        .pixel_size(24)
        .build();
    row.add_prefix(&icon);

    if device.trusted {
        let unpair = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove device")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();
        let daemon = daemon.clone();
        let id = device.id.clone();
        unpair.connect_clicked(move |_| {
            daemon.send(DaemonCommand::Unpair(id.clone()));
        });
        row.add_suffix(&unpair);
    }
    row
}

fn can_send_feature(device: &DeviceView, packet_type: &str) -> bool {
    FeatureRegistry::desktop()
        .outgoing_capabilities()
        .iter()
        .any(|capability| capability == packet_type)
        && device
            .incoming_capabilities
            .iter()
            .any(|capability| capability == packet_type)
}

/// Full detail view shown for a paired device — header plus DeskLink features.
fn paired_device_card(daemon: &DaemonHandle, device: DeviceView) -> gtk::Widget {
    let outer = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();

    // ── Device header ────────────────────────────────────────────────────────
    let header_card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .css_classes(["card"])
        .build();

    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(14)
        .margin_start(18)
        .margin_end(12)
        .margin_top(16)
        .margin_bottom(16)
        .build();

    // Device icon (large)
    let icon_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .valign(gtk::Align::Center)
        .build();
    let icon = gtk::Image::builder()
        .icon_name(device_type_icon(&device.device_type))
        .pixel_size(48)
        .valign(gtk::Align::Center)
        .build();
    icon_box.append(&icon);
    header.append(&icon_box);

    // Name + type + address
    let info = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(3)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();

    let name_label = gtk::Label::builder()
        .label(&device.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["title-3"])
        .build();
    name_label.set_use_markup(false);
    info.append(&name_label);

    info.append(
        &gtk::Label::builder()
            .label(format!("{} · {}", device.device_type, device.address))
            .xalign(0.0)
            .css_classes(["caption", "dim-label"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    header.append(&info);

    // Status pill + unpair button on right
    let right = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .valign(gtk::Align::Center)
        .build();

    let status_pill = gtk::Label::builder()
        .label("● Connected")
        .css_classes(["caption", "success"])
        .valign(gtk::Align::Center)
        .build();
    right.append(&status_pill);

    let unpair_btn = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .tooltip_text("Unpair this device")
        .valign(gtk::Align::Center)
        .css_classes(["flat", "circular"])
        .build();
    {
        let daemon = daemon.clone();
        let id = device.id.clone();
        unpair_btn.connect_clicked(move |_| {
            eprintln!("[UI] Unpair clicked for {}", id);
            daemon.send(DaemonCommand::Unpair(id.clone()));
        });
    }
    right.append(&unpair_btn);
    header.append(&right);

    header_card.append(&header);

    // Error bar (only if there is one)
    if let Some(error) = &device.last_error {
        header_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        header_card.append(
            &gtk::Label::builder()
                .label(format!("⚠  {error}"))
                .xalign(0.0)
                .wrap(true)
                .margin_start(18)
                .margin_end(18)
                .margin_top(8)
                .margin_bottom(10)
                .css_classes(["caption", "warning"])
                .build(),
        );
    }

    outer.append(&header_card);

    let summary_parts = device_summary_parts(&device);
    if !summary_parts.is_empty() {
        let summary = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(8)
            .margin_top(12)
            .margin_bottom(4)
            .build();
        for (icon, text) in summary_parts {
            summary.append(&summary_chip(icon, &text));
        }
        outer.append(&summary);
    }

    if let Some(media) = &device.media_status {
        let now_playing = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .margin_top(12)
            .margin_bottom(4)
            .css_classes(["card"])
            .build();
        now_playing.append(
            &gtk::Image::builder()
                .icon_name(if media.is_playing {
                    "media-playback-start-symbolic"
                } else {
                    "media-playback-pause-symbolic"
                })
                .pixel_size(24)
                .margin_start(14)
                .valign(gtk::Align::Center)
                .build(),
        );
        let media_text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .margin_top(10)
            .margin_bottom(10)
            .margin_end(14)
            .hexpand(true)
            .build();
        let title = if media.title.is_empty() {
            "Media session".to_string()
        } else {
            media.title.clone()
        };
        media_text.append(
            &gtk::Label::builder()
                .label(&title)
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["heading"])
                .build(),
        );
        let details = [
            media.artist.as_str(),
            media.album.as_str(),
            media.player.as_str(),
        ]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" · ");
        media_text.append(
            &gtk::Label::builder()
                .label(if details.is_empty() {
                    "Remote media status"
                } else {
                    &details
                })
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        now_playing.append(&media_text);
        outer.append(&now_playing);
    }

    // ── Plugin section label ─────────────────────────────────────────────────
    outer.append(
        &gtk::Label::builder()
            .label("Features")
            .xalign(0.0)
            .margin_top(20)
            .margin_bottom(8)
            .css_classes(["heading"])
            .build(),
    );

    // ── Plugin grid card ─────────────────────────────────────────────────────
    let grid_card = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .css_classes(["card"])
        .build();

    // Row 1 — Communication
    let row1 = plugin_row();
    if can_send_feature(&device, PACKET_TYPE_PING) {
        row1.append(&plugin_tile_active(
            "Ping",
            "dialog-ok-symbolic",
            "Test connection",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move |_btn| {
                    daemon.send(DaemonCommand::SendPing(id.clone()));
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_SHARE_REQUEST) {
        row1.append(&plugin_tile_active(
            "Send File",
            "document-send-symbolic",
            "Share files with your device",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move |btn| {
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        let file_dialog = gtk::FileDialog::builder()
                            .title("Select File to Send")
                            .build();

                        let daemon = daemon.clone();
                        let id = id.clone();
                        file_dialog.open(Some(&root), gio::Cancellable::NONE, move |result| {
                            if let Ok(file) = result {
                                if let Some(path) = file.path() {
                                    eprintln!("[UI] Selected file to send: {:?}", path);
                                    daemon.send(DaemonCommand::SendFile(id.clone(), path));
                                }
                            }
                        });
                    }
                }
            },
        ));
        row1.append(&plugin_tile_active(
            "Share Text",
            "insert-text-symbolic",
            "Send text or a URL to your device",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move |btn| {
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        show_share_text_dialog(daemon.clone(), id.clone(), &root);
                    }
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_CLIPBOARD) {
        row1.append(&plugin_tile_active("Clipboard", "edit-paste-symbolic", "Clipboard sync runs automatically while paired", {
            move |btn| {
                if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                    show_info_dialog(&root, "Clipboard Sync", "Clipboard synchronization runs automatically while the device is connected and paired.");
                }
            }
        }));
    }
    grid_card.append(&row1);

    grid_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // Row 2 — Control
    let row2 = plugin_row();
    if can_send_feature(&device, PACKET_TYPE_FINDMYPHONE_REQUEST) {
        row2.append(&plugin_tile_active(
            "Find Phone",
            "find-location-symbolic",
            "Make your phone ring",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move |_btn| {
                    daemon.send(DaemonCommand::SendFindPhone(id.clone()));
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_MOUSEPAD_REQUEST) {
        row2.append(&plugin_tile_active(
            "Remote Control",
            "input-mouse-symbolic",
            "Control phone mouse and keyboard",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move |btn| {
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        crate::remote_control::show_remote_control_dialog(
                            daemon.clone(),
                            id.clone(),
                            &root,
                        );
                    }
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_LOCK_REQUEST) {
        row2.append(&plugin_tile_active(
            "Lock Screen",
            "system-lock-screen-symbolic",
            "Lock your phone remotely",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move |_btn| {
                    daemon.send(DaemonCommand::SendLockRequest(id.clone(), true));
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_MPRIS_REQUEST) {
        row2.append(&plugin_tile_active(
            "Media Control",
            "media-playback-start-symbolic",
            "Control music & video",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                let media_device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestMprisStatus(id.clone(), None));
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        show_media_control_dialog(
                            daemon.clone(),
                            id.clone(),
                            media_device.clone(),
                            &root,
                        );
                    }
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_SFTP_REQUEST) {
        row2.append(&plugin_tile_active(
            "Browse Files",
            "folder-remote-symbolic",
            "Request phone storage access",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                let browsing_device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::SendSftpRequest(id.clone()));
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        show_file_browsing_dialog(browsing_device.clone(), &root);
                    }
                }
            },
        ));
    }
    grid_card.append(&row2);

    grid_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // Row 3 — v2 status and desktop integration
    let row3 = plugin_row();
    if can_send_feature(&device, PACKET_TYPE_NOTIFICATION_REQUEST) {
        row3.append(&plugin_tile_active(
            "Notifications",
            "preferences-system-notifications-symbolic",
            "View mirrored phone notifications",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                let device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestNotifications(id.clone()));
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        show_notifications_dialog(daemon.clone(), device.clone(), &root);
                    }
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_SYSTEMVOLUME_REQUEST) {
        row3.append(&plugin_tile_active(
            "Volume",
            "audio-volume-high-symbolic",
            "View and control remote volume",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                let device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestSystemVolume(id.clone()));
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        show_volume_dialog(daemon.clone(), device.clone(), &root);
                    }
                }
            },
        ));
    }
    if can_send_feature(&device, PACKET_TYPE_RUNCOMMAND_REQUEST) {
        row3.append(&plugin_tile_active(
            "Commands",
            "utilities-terminal-symbolic",
            "Run commands exposed by your device",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                let device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestRemoteCommands(id.clone()));
                    if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                        show_remote_commands_dialog(daemon.clone(), device.clone(), &root);
                    }
                }
            },
        ));
    }
    let can_refresh_media = can_send_feature(&device, PACKET_TYPE_MPRIS_REQUEST);
    let can_refresh_notifications = can_send_feature(&device, PACKET_TYPE_NOTIFICATION_REQUEST);
    row3.append(&plugin_tile_active(
        "Refresh Status",
        "view-refresh-symbolic",
        "Refresh available WebRTC feature state",
        {
            let daemon = daemon.clone();
            let id = device.id.clone();
            move |_btn| {
                if can_refresh_media {
                    daemon.send(DaemonCommand::RequestMprisStatus(id.clone(), None));
                }
                if can_refresh_notifications {
                    daemon.send(DaemonCommand::RequestNotifications(id.clone()));
                }
            }
        },
    ));
    grid_card.append(&row3);

    outer.append(&grid_card);
    outer.upcast()
}

fn device_summary_parts(device: &DeviceView) -> Vec<(&'static str, String)> {
    let mut parts = Vec::new();
    if let Some(battery) = &device.battery_status {
        let charging = if battery.is_charging { " charging" } else { "" };
        parts.push((
            "battery-good-symbolic",
            format!("{}%{}", battery.current_charge, charging),
        ));
    }
    if !device.notifications.is_empty() {
        parts.push((
            "preferences-system-notifications-symbolic",
            format!("{} notifications", device.notifications.len()),
        ));
    }
    if let Some(volume) = &device.volume_status {
        if let Some(sink) = volume
            .sinks
            .iter()
            .find(|sink| sink.enabled)
            .or(volume.sinks.first())
        {
            let muted = if sink.muted { " muted" } else { "" };
            parts.push((
                "audio-volume-high-symbolic",
                format!("{} {}{}", sink.description, sink.volume, muted),
            ));
        }
    }
    if !device.available_commands.is_empty() {
        parts.push((
            "utilities-terminal-symbolic",
            format!("{} commands", device.available_commands.len()),
        ));
    }
    if let Some(sftp) = &device.sftp_status {
        if sftp.error.is_some() {
            parts.push(("dialog-warning-symbolic", "File browsing error".to_string()));
        } else if sftp.port.is_some() {
            parts.push(("folder-remote-symbolic", "File browsing ready".to_string()));
        }
    }
    parts
}

fn summary_chip(icon: &str, text: &str) -> gtk::Box {
    let chip = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(10)
        .margin_end(10)
        .css_classes(["card"])
        .build();
    chip.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .pixel_size(16)
            .valign(gtk::Align::Center)
            .build(),
    );
    chip.append(
        &gtk::Label::builder()
            .label(text)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption"])
            .build(),
    );
    chip
}

fn show_share_text_dialog(daemon: DaemonHandle, device_id: String, parent: &gtk::Window) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(460)
        .default_height(180)
        .title("Share Text")
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_top(20)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();

    content.append(
        &gtk::Label::builder()
            .label("Send text or a URL to the paired device.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["body", "dim-label"])
            .build(),
    );

    let entry = gtk::Entry::builder()
        .placeholder_text("Text or https://example.com")
        .hexpand(true)
        .build();
    content.append(&entry);

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .halign(gtk::Align::End)
        .build();
    let cancel = gtk::Button::with_label("Cancel");
    let send = gtk::Button::builder()
        .label("Send")
        .css_classes(["suggested-action"])
        .build();
    buttons.append(&cancel);
    buttons.append(&send);
    content.append(&buttons);

    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    {
        let window = window.downgrade();
        cancel.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }

    {
        let window = window.downgrade();
        let entry = entry.downgrade();
        let daemon = daemon.clone();
        let device_id = device_id.clone();
        send.connect_clicked(move |_| {
            let Some(entry) = entry.upgrade() else {
                return;
            };
            let text = entry.text().trim().to_string();
            if !text.is_empty() {
                daemon.send(DaemonCommand::SendShareText(device_id.clone(), text));
            }
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }

    {
        let send = send.downgrade();
        entry.connect_activate(move |_| {
            if let Some(send) = send.upgrade() {
                send.emit_clicked();
            }
        });
    }

    window.present();
    entry.grab_focus();
}

fn show_media_control_dialog(
    daemon: DaemonHandle,
    device_id: String,
    device: DeviceView,
    parent: &gtk::Window,
) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .default_height(260)
        .title("Media Control")
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(14)
        .margin_top(20)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();

    content.append(
        &gtk::Label::builder()
            .label("Control the active media session on your paired device.")
            .xalign(0.0)
            .wrap(true)
            .css_classes(["body", "dim-label"])
            .build(),
    );

    let player_entry = gtk::Entry::builder()
        .placeholder_text("Player name (optional)")
        .primary_icon_name("multimedia-player-symbolic")
        .build();
    if let Some(media) = &device.media_status {
        if !media.player.is_empty() {
            player_entry.set_text(&media.player);
        } else if let Some(player) = media.player_list.first() {
            player_entry.set_text(player);
        }
    }
    content.append(&player_entry);

    let controls = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .homogeneous(true)
        .build();
    for (label, icon, action) in [
        ("Previous", "media-skip-backward-symbolic", "Previous"),
        ("Play/Pause", "media-playback-start-symbolic", "PlayPause"),
        ("Next", "media-skip-forward-symbolic", "Next"),
        ("Stop", "media-playback-stop-symbolic", "Stop"),
    ] {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(label)
            .build();
        let daemon = daemon.clone();
        let device_id = device_id.clone();
        let player_entry = player_entry.downgrade();
        button.connect_clicked(move |_| {
            let player = player_entry
                .upgrade()
                .map(|entry| entry.text().to_string())
                .unwrap_or_default();
            daemon.send(DaemonCommand::SendMprisAction(
                device_id.clone(),
                player,
                action.to_string(),
            ));
        });
        controls.append(&button);
    }
    content.append(&controls);

    let advanced = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .homogeneous(true)
        .build();
    for (label, icon, command) in [
        ("Refresh", "view-refresh-symbolic", "refresh"),
        ("Volume Down", "audio-volume-low-symbolic", "volume-down"),
        ("Volume Up", "audio-volume-high-symbolic", "volume-up"),
        (
            "Seek Forward",
            "media-seek-forward-symbolic",
            "seek-forward",
        ),
    ] {
        let button = gtk::Button::builder()
            .icon_name(icon)
            .tooltip_text(label)
            .build();
        let daemon = daemon.clone();
        let device_id = device_id.clone();
        let player_entry = player_entry.downgrade();
        let current_volume = device
            .media_status
            .as_ref()
            .and_then(|media| media.volume)
            .unwrap_or(50);
        button.connect_clicked(move |_| {
            let player = player_entry
                .upgrade()
                .map(|entry| entry.text().to_string())
                .unwrap_or_default();
            match command {
                "refresh" => daemon.send(DaemonCommand::RequestMprisStatus(
                    device_id.clone(),
                    if player.trim().is_empty() {
                        None
                    } else {
                        Some(player)
                    },
                )),
                "volume-down" => daemon.send(DaemonCommand::SendMprisSetVolume(
                    device_id.clone(),
                    player,
                    current_volume - 5,
                )),
                "volume-up" => daemon.send(DaemonCommand::SendMprisSetVolume(
                    device_id.clone(),
                    player,
                    current_volume + 5,
                )),
                "seek-forward" => daemon.send(DaemonCommand::SendMprisSeek(
                    device_id.clone(),
                    player,
                    10_000,
                )),
                _ => {}
            }
        });
        advanced.append(&button);
    }
    content.append(&advanced);

    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    window.present();
}

fn show_notifications_dialog(daemon: DaemonHandle, device: DeviceView, parent: &gtk::Window) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(560)
        .default_height(460)
        .title("Notifications")
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .vexpand(true)
        .build();

    if device.notifications.is_empty() {
        list.append(
            &gtk::Label::builder()
                .label("No mirrored notifications")
                .margin_top(24)
                .margin_bottom(24)
                .css_classes(["dim-label"])
                .build(),
        );
    } else {
        for notification in &device.notifications {
            list.append(&notification_row(&daemon, &device.id, notification));
        }
    }
    content.append(&list);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    window.present();
}

fn notification_row(
    daemon: &DaemonHandle,
    device_id: &str,
    notification: &DeviceNotification,
) -> adw::ActionRow {
    let title = if notification.title.is_empty() {
        notification.app_name.clone()
    } else {
        format!("{} · {}", notification.app_name, notification.title)
    };
    let subtitle = if notification.text.is_empty() {
        notification.ticker.clone()
    } else {
        notification.text.clone()
    };
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    row.set_use_markup(false);
    row.set_activatable(false);

    if let Some(reply_id) = &notification.request_reply_id {
        let reply = command_button("Reply", "mail-reply-sender-symbolic", {
            let daemon = daemon.clone();
            let device_id = device_id.to_string();
            let reply_id = reply_id.clone();
            move || {
                daemon.send(DaemonCommand::ReplyNotification(
                    device_id.clone(),
                    reply_id.clone(),
                    "Acknowledged".to_string(),
                ));
            }
        });
        row.add_suffix(&reply);
    }
    for action in notification.actions.iter().take(2) {
        let button = command_button(action, "system-run-symbolic", {
            let daemon = daemon.clone();
            let device_id = device_id.to_string();
            let key = notification.id.clone();
            let action = action.clone();
            move || {
                daemon.send(DaemonCommand::TriggerNotificationAction(
                    device_id.clone(),
                    key.clone(),
                    action.clone(),
                ));
            }
        });
        row.add_suffix(&button);
    }
    if notification.is_clearable {
        let dismiss = command_button("Dismiss", "window-close-symbolic", {
            let daemon = daemon.clone();
            let device_id = device_id.to_string();
            let notification_id = notification.id.clone();
            move || {
                daemon.send(DaemonCommand::DismissNotification(
                    device_id.clone(),
                    notification_id.clone(),
                ));
            }
        });
        row.add_suffix(&dismiss);
    }
    row
}

fn show_volume_dialog(daemon: DaemonHandle, device: DeviceView, parent: &gtk::Window) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(360)
        .title("Volume")
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .css_classes(["boxed-list"])
        .build();
    let sinks = device
        .volume_status
        .as_ref()
        .map(|status| status.sinks.as_slice())
        .unwrap_or(&[]);
    if sinks.is_empty() {
        list.append(
            &gtk::Label::builder()
                .label("No remote audio devices reported yet")
                .margin_top(24)
                .margin_bottom(24)
                .css_classes(["dim-label"])
                .build(),
        );
    } else {
        for sink in sinks {
            list.append(&volume_sink_row(&daemon, &device.id, sink));
        }
    }
    toolbar.set_content(Some(&list));
    window.set_content(Some(&toolbar));
    window.present();
}

fn volume_sink_row(daemon: &DaemonHandle, device_id: &str, sink: &VolumeSink) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&sink.description)
        .subtitle(format!(
            "{} · volume {}{}",
            sink.name,
            sink.volume,
            if sink.muted { " · muted" } else { "" }
        ))
        .build();
    row.set_use_markup(false);
    row.set_activatable(false);
    for (label, icon, delta) in [
        ("Lower Volume", "list-remove-symbolic", -5),
        ("Raise Volume", "list-add-symbolic", 5),
    ] {
        let button = command_button(label, icon, {
            let daemon = daemon.clone();
            let device_id = device_id.to_string();
            let name = sink.name.clone();
            let next = (sink.volume + delta).max(0);
            move || {
                daemon.send(DaemonCommand::SetSystemVolume(
                    device_id.clone(),
                    name.clone(),
                    Some(next),
                    None,
                ));
            }
        });
        row.add_suffix(&button);
    }
    let mute = command_button(
        if sink.muted { "Unmute" } else { "Mute" },
        if sink.muted {
            "audio-volume-muted-symbolic"
        } else {
            "audio-volume-high-symbolic"
        },
        {
            let daemon = daemon.clone();
            let device_id = device_id.to_string();
            let name = sink.name.clone();
            let muted = !sink.muted;
            move || {
                daemon.send(DaemonCommand::SetSystemVolume(
                    device_id.clone(),
                    name.clone(),
                    None,
                    Some(muted),
                ));
            }
        },
    );
    row.add_suffix(&mute);
    row
}

fn show_remote_commands_dialog(daemon: DaemonHandle, device: DeviceView, parent: &gtk::Window) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(400)
        .title("Remote Commands")
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .css_classes(["boxed-list"])
        .build();
    if device.available_commands.is_empty() {
        list.append(
            &gtk::Label::builder()
                .label("No remote commands reported yet")
                .margin_top(24)
                .margin_bottom(24)
                .css_classes(["dim-label"])
                .build(),
        );
    } else {
        for command in &device.available_commands {
            let row = adw::ActionRow::builder()
                .title(&command.name)
                .subtitle(
                    command
                        .command
                        .as_deref()
                        .unwrap_or("Advertised remote command"),
                )
                .build();
            row.set_use_markup(false);
            row.set_activatable(false);
            let run = command_button("Run", "system-run-symbolic", {
                let daemon = daemon.clone();
                let device_id = device.id.clone();
                let key = command.key.clone();
                move || {
                    daemon.send(DaemonCommand::ExecuteRemoteCommand(
                        device_id.clone(),
                        key.clone(),
                    ))
                }
            });
            row.add_suffix(&run);
            list.append(&row);
        }
    }
    toolbar.set_content(Some(&list));
    window.set_content(Some(&toolbar));
    window.present();
}

fn show_file_browsing_dialog(device: DeviceView, parent: &gtk::Window) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(520)
        .default_height(340)
        .title("Browse Files")
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .margin_top(18)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .css_classes(["boxed-list"])
        .build();
    if let Some(sftp) = &device.sftp_status {
        if let Some(error) = &sftp.error {
            list.append(
                &adw::ActionRow::builder()
                    .title("SFTP Error")
                    .subtitle(error)
                    .build(),
            );
        } else {
            list.append(
                &adw::ActionRow::builder()
                    .title("Connection")
                    .subtitle(format!(
                        "user {} · port {} · path {}",
                        sftp.user.as_deref().unwrap_or("unknown"),
                        sftp.port
                            .map(|port| port.to_string())
                            .unwrap_or_else(|| "unknown".to_string()),
                        sftp.path.as_deref().unwrap_or("/")
                    ))
                    .build(),
            );
            for (path, name) in &sftp.directories {
                list.append(&adw::ActionRow::builder().title(name).subtitle(path).build());
            }
        }
    } else {
        list.append(
            &gtk::Label::builder()
                .label("File browsing request sent. Details appear after the device replies.")
                .wrap(true)
                .margin_top(24)
                .margin_bottom(24)
                .margin_start(16)
                .margin_end(16)
                .css_classes(["dim-label"])
                .build(),
        );
    }
    toolbar.set_content(Some(&list));
    window.set_content(Some(&toolbar));
    window.present();
}

fn show_info_dialog(parent: &gtk::Window, title: &str, message: &str) {
    let window = adw::Window::builder()
        .transient_for(parent)
        .modal(true)
        .default_width(420)
        .default_height(160)
        .title(title)
        .build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .margin_top(20)
        .margin_bottom(20)
        .margin_start(20)
        .margin_end(20)
        .build();
    content.append(
        &gtk::Label::builder()
            .label(message)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["body"])
            .build(),
    );
    let close = gtk::Button::builder()
        .label("Close")
        .halign(gtk::Align::End)
        .css_classes(["suggested-action"])
        .build();
    {
        let window = window.downgrade();
        close.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.close();
            }
        });
    }
    content.append(&close);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    window.present();
}

/// A horizontal row inside the plugin grid.
fn plugin_row() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .homogeneous(true)
        .build()
}

/// Plugin tile — active, wired-up action.
fn plugin_tile_active(
    label: &str,
    icon: &str,
    tooltip: &str,
    action: impl Fn(&gtk::Button) + 'static,
) -> gtk::Button {
    let btn = gtk::Button::builder()
        .tooltip_text(tooltip)
        .css_classes(["flat"])
        .build();

    let inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_start(8)
        .margin_end(8)
        .margin_top(16)
        .margin_bottom(16)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();

    inner.append(&gtk::Image::builder().icon_name(icon).pixel_size(28).build());
    inner.append(
        &gtk::Label::builder()
            .label(label)
            .halign(gtk::Align::Center)
            .css_classes(["caption"])
            .build(),
    );

    btn.set_child(Some(&inner));
    btn.connect_clicked(move |b| action(b));
    btn
}

/// Compact row for non-paired device states (discovered, pairing, rejected, etc.)
fn unpaired_device_row(daemon: &DaemonHandle, device: DeviceView) -> adw::ActionRow {
    let mut subtitle_parts: Vec<String> = vec![
        device.device_type.clone(),
        device.status.label().to_string(),
    ];
    if let Some(key) = &device.verification_key {
        subtitle_parts.push(format!("Key: {key}"));
    }
    if let Some(error) = &device.last_error {
        subtitle_parts.push(error.clone());
    }
    let subtitle = subtitle_parts.join(" · ");

    let row = adw::ActionRow::builder()
        .title(&device.name)
        .subtitle(&subtitle)
        .build();
    row.set_use_markup(false);
    row.set_activatable(false);

    let icon = gtk::Image::builder()
        .icon_name(device_type_icon(&device.device_type))
        .pixel_size(32)
        .valign(gtk::Align::Center)
        .css_classes(["dim-label"])
        .build();
    row.add_prefix(&icon);

    match &device.status {
        DeviceStatus::Discovered | DeviceStatus::Connected | DeviceStatus::Unreachable => {
            let btn = command_button("Request Pair", "network-transmit-receive-symbolic", {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move || daemon.send(DaemonCommand::RequestPair(id.clone()))
            });
            row.add_suffix(&btn);
        }
        DeviceStatus::PairRequested => {
            row.add_suffix(
                &gtk::Label::builder()
                    .label("Waiting…")
                    .valign(gtk::Align::Center)
                    .css_classes(["caption", "dim-label"])
                    .build(),
            );
            row.add_suffix(
                &gtk::Spinner::builder()
                    .spinning(true)
                    .valign(gtk::Align::Center)
                    .build(),
            );
        }
        DeviceStatus::PairRequestedByPeer => {
            if let Some(key) = &device.verification_key {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(format!("Key: {key}"))
                        .valign(gtk::Align::Center)
                        .css_classes(["caption", "monospace", "accent"])
                        .build(),
                );
            }
            let accept = command_button("Accept", "object-select-symbolic", {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move || {
                    eprintln!("[UI] Accept clicked for {}", id);
                    daemon.send(DaemonCommand::AcceptPair(id.clone()));
                }
            });
            accept.add_css_class("suggested-action");
            row.add_suffix(&accept);

            let reject = command_button("Reject", "window-close-symbolic", {
                let daemon = daemon.clone();
                let id = device.id.clone();
                move || {
                    eprintln!("[UI] Reject clicked for {}", id);
                    daemon.send(DaemonCommand::RejectPair(id.clone()));
                }
            });
            reject.add_css_class("destructive-action");
            row.add_suffix(&reject);
        }
        DeviceStatus::Paired => unreachable!("handled by paired_device_card"),
    }

    row
}

fn command_button(label: &str, icon_name: &str, action: impl Fn() + 'static) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(label)
        .valign(gtk::Align::Center)
        .css_classes(["circular"])
        .build();
    button.connect_clicked(move |_| action());
    button
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::device_info::DeviceInfo;

    #[test]
    fn feature_tiles_require_both_a_local_sender_and_peer_receiver() {
        let info = DeviceInfo {
            id: "0123456789abcdef0123456789abcdef".to_string(),
            name: "Phone".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 9,
            incoming_capabilities: vec![PACKET_TYPE_PING.to_string()],
            outgoing_capabilities: Vec::new(),
        };
        let device = DeviceView::from_info(&info, "127.0.0.1".to_string(), true);

        assert!(can_send_feature(&device, PACKET_TYPE_PING));
        assert!(!can_send_feature(&device, PACKET_TYPE_MOUSEPAD_REQUEST));
    }
}
