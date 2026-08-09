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
use std::cell::RefCell;
use std::collections::HashMap;

use crate::device_links::core::events::CoreEvent;
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::device_links::device::{DeviceNotification, DeviceStatus, DeviceView, VolumeSink};

#[derive(Debug, Clone)]
pub(crate) struct PhoneFileDialog {
    window: adw::Window,
    list: gtk::ListBox,
}

mod imp {
    use super::*;
    use std::collections::HashSet;

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/derx06/desklink/com/window.ui")]
    pub struct DeskLinkWindow {
        // Template widgets
        #[template_child]
        pub devices_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub transfers_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub error_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub add_device_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        #[template_child]
        pub nearby_count_label: TemplateChild<gtk::Label>,
        #[template_child]
        pub device_status_label: TemplateChild<gtk::Label>,
        pub daemon: RefCell<Option<DaemonHandle>>,
        pub notified_pair_requests: RefCell<HashSet<String>>,
        pub notified_remote_notifications: RefCell<HashSet<String>>,
        pub transfers: RefCell<HashMap<String, TransferProgress>>,
        pub(crate) phone_file_dialogs: RefCell<HashMap<String, PhoneFileDialog>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DeskLinkWindow {
        const NAME: &'static str = "DeskLinkWindow";
        type Type = super::DeskLinkWindow;
        type ParentType = adw::ApplicationWindow;

        fn class_init(klass: &mut Self::Class) {
            klass.bind_template();
        }

        fn instance_init(obj: &glib::subclass::InitializingObject<Self>) {
            obj.init_template();
        }
    }

    impl ObjectImpl for DeskLinkWindow {
        fn constructed(&self) {
            self.parent_constructed();

            let window = self.obj().downgrade();
            self.add_device_button.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    window.show_discovery_dialog();
                }
            });

            let window = self.obj().downgrade();
            self.obj().connect_close_request(move |_| {
                if let Some(window) = window.upgrade() {
                    window.close_phone_file_dialogs();
                }
                glib::Propagation::Proceed
            });
        }
    }
    impl WidgetImpl for DeskLinkWindow {}
    impl WindowImpl for DeskLinkWindow {}
    impl ApplicationWindowImpl for DeskLinkWindow {}
    impl AdwApplicationWindowImpl for DeskLinkWindow {}
}

#[derive(Debug, Clone)]
pub struct TransferProgress {
    state: String,
    bytes_done: u64,
    bytes_total: u64,
    can_resume: bool,
    error: Option<String>,
}

glib::wrapper! {
    pub struct DeskLinkWindow(ObjectSubclass<imp::DeskLinkWindow>)
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,        @implements gio::ActionGroup, gio::ActionMap;
}

impl DeskLinkWindow {
    pub fn new<P: IsA<gtk::Application>>(application: &P) -> Self {
        glib::Object::builder()
            .property("application", application)
            .build()
    }

    pub fn set_daemon(&self, daemon: DaemonHandle) {
        self.imp().daemon.replace(Some(daemon));
        self.refresh_devices();

        let Some(daemon) = self.imp().daemon.borrow().clone() else {
            return;
        };
        let receiver = daemon.subscribe_events();
        let weak = self.downgrade();
        glib::MainContext::default().spawn_local(async move {
            use futures::StreamExt;
            let mut receiver = receiver;
            while let Some(event) = receiver.next().await {
                if let Some(window) = weak.upgrade() {
                    window.handle_core_event(event);
                } else {
                    break;
                }
            }
        });
    }

    fn handle_core_event(&self, event: CoreEvent) {
        match event {
            CoreEvent::FeatureStateChanged {
                device_id,
                feature,
                state,
                details,
            } if feature == "phone-files" => {
                self.update_phone_file_dialog(&device_id, &state, &details);
            }
            CoreEvent::DeviceChanged { .. }
            | CoreEvent::PairingChanged { .. }
            | CoreEvent::FeatureStateChanged { .. }
            | CoreEvent::NotificationReceived { .. } => self.refresh_devices(),
            CoreEvent::ConnectionChanged {
                device_id,
                state: crate::device_links::core::device_session::DeviceConnectionState::Unreachable,
                ..
            } => {
                self.close_phone_file_dialog(&device_id);
                self.refresh_devices();
            }
            CoreEvent::ConnectionChanged { .. } => self.refresh_devices(),
            CoreEvent::TransferChanged {
                transfer_id,
                state,
                bytes_done,
                bytes_total,
                can_resume,
                error,
            } => {
                self.imp().transfers.borrow_mut().insert(
                    transfer_id,
                    TransferProgress {
                        state,
                        bytes_done,
                        bytes_total,
                        can_resume,
                        error,
                    },
                );
                self.refresh_transfer_rows();
            }
            CoreEvent::Error { message, .. } => {
                self.imp().error_banner.set_title(&message);
                self.imp().error_banner.set_revealed(true);
                self.imp()
                    .toast_overlay
                    .add_toast(adw::Toast::new(&message));
                self.refresh_devices();
            }
        }
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
            imp.toast_overlay.add_toast(adw::Toast::new(error));
            self.send_app_notification("desklink-error", "DeskLink", error);
        }

        while let Some(child) = imp.devices_list.first_child() {
            imp.devices_list.remove(&child);
        }

        let devices = daemon.devices();
        self.refresh_transfer_rows();
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

        if recent_devices.is_empty() {
            imp.notified_pair_requests.borrow_mut().clear();
            imp.notified_remote_notifications.borrow_mut().clear();
            imp.nearby_count_label.set_label("No devices yet");
            imp.device_status_label
                .set_label("· Pair a device to get started");

            let empty_page = adw::StatusPage::builder()
                .icon_name("network-wireless-symbolic")
                .title("No devices paired")
                .description(
                    "Keep your phone on the same Wi-Fi network, then add it from DeskLink.",
                )
                .vexpand(true)
                .build();

            let discover_btn = gtk::Button::builder()
                .label("Discover Devices")
                .css_classes(["suggested-action"])
                .halign(gtk::Align::Center)
                .build();
            {
                let daemon = daemon.clone();
                let window = self.downgrade();
                discover_btn.connect_clicked(move |_| {
                    daemon.send(DaemonCommand::Discover);
                    if let Some(w) = window.upgrade() {
                        w.show_discovery_dialog();
                    }
                });
            }
            empty_page.set_child(Some(&discover_btn));
            imp.devices_list.append(&empty_page);
            return;
        }

        let pending_count = recent_devices
            .iter()
            .filter(|device| {
                matches!(
                    device.status,
                    DeviceStatus::PairRequested | DeviceStatus::PairRequestedByPeer
                )
            })
            .count();
        imp.nearby_count_label
            .set_label(&recent_count_label(recent_devices.len(), pending_count));
        imp.device_status_label.set_label(if pending_count > 0 {
            "· Pairing request pending"
        } else {
            "· Ready"
        });

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

        for device in recent_devices {
            imp.devices_list.append(&device_row(&daemon, device));
        }
    }

    fn close_phone_file_dialog(&self, device_id: &str) {
        if let Some(dialog) = self.imp().phone_file_dialogs.borrow_mut().remove(device_id) {
            dialog.window.close();
        }
    }

    fn close_phone_file_dialogs(&self) {
        let dialogs: Vec<_> = self.imp().phone_file_dialogs.borrow_mut().drain().collect();
        for (_, dialog) in dialogs {
            dialog.window.close();
        }
    }

    fn show_phone_file_dialog(&self, device_id: String, device_name: String) {
        if let Some(existing) = self.imp().phone_file_dialogs.borrow().get(&device_id) {
            existing.window.present();
            return;
        }
        let window = adw::Window::builder()
            .transient_for(self)
            .modal(true)
            .default_width(560)
            .default_height(520)
            .title(format!("Files on {device_name}"))
            .build();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let home = gtk::Button::builder()
            .icon_name("go-home-symbolic")
            .tooltip_text("Storage roots")
            .build();
        home.update_property(&[gtk::accessible::Property::Label("Show storage roots")]);
        if let Some(daemon) = self.imp().daemon.borrow().clone() {
            let id = device_id.clone();
            home.connect_clicked(move |_| daemon.send(DaemonCommand::SendSftpRequest(id.clone())));
        }
        header.pack_start(&home);
        toolbar.add_top_bar(&header);
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .margin_top(18)
            .margin_bottom(18)
            .margin_start(18)
            .margin_end(18)
            .build();
        list.append(
            &adw::ActionRow::builder()
                .title("Loading phone storage…")
                .subtitle("Waiting for the authenticated WebRTC file channel")
                .build(),
        );
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&list)
            .build();
        toolbar.set_content(Some(&scrolled));
        window.set_content(Some(&toolbar));

        let weak = self.downgrade();
        let closed_id = device_id.clone();
        window.connect_close_request(move |_| {
            if let Some(window) = weak.upgrade() {
                window
                    .imp()
                    .phone_file_dialogs
                    .borrow_mut()
                    .remove(&closed_id);
            }
            glib::Propagation::Proceed
        });
        self.imp().phone_file_dialogs.borrow_mut().insert(
            device_id,
            PhoneFileDialog {
                window: window.clone(),
                list,
            },
        );
        window.present();
    }

    fn update_phone_file_dialog(&self, device_id: &str, state: &str, details: &serde_json::Value) {
        let dialogs = self.imp().phone_file_dialogs.borrow();
        let Some(dialog) = dialogs.get(device_id) else {
            return;
        };
        while let Some(child) = dialog.list.first_child() {
            dialog.list.remove(&child);
        }
        if state == "Requesting" {
            dialog.list.append(
                &adw::ActionRow::builder()
                    .title("Loading…")
                    .subtitle("Requesting phone storage over WebRTC")
                    .build(),
            );
            return;
        }
        if let Some(error) = details.get("error").and_then(serde_json::Value::as_str) {
            dialog.list.append(
                &adw::ActionRow::builder()
                    .title("Could not browse phone files")
                    .subtitle(error)
                    .build(),
            );
            return;
        }
        let entries = details
            .get("result")
            .and_then(|result| result.get("entries"))
            .and_then(serde_json::Value::as_array);
        let Some(entries) = entries else {
            dialog.list.append(
                &adw::ActionRow::builder()
                    .title("No files available")
                    .subtitle("The phone returned no authorized storage entries")
                    .build(),
            );
            return;
        };
        if entries.is_empty() {
            dialog.list.append(
                &adw::ActionRow::builder()
                    .title("This folder is empty")
                    .subtitle("No accessible entries were returned")
                    .build(),
            );
        }
        for entry in entries {
            let Some(entry_id) = entry.get("entryId").and_then(serde_json::Value::as_str) else {
                continue;
            };
            let name = entry
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Unnamed");
            let is_directory = entry
                .get("directory")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false);
            let size = entry
                .get("size")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            let row = adw::ActionRow::builder()
                .title(name)
                .subtitle(if is_directory {
                    "Folder".to_string()
                } else {
                    format!("{size} bytes")
                })
                .activatable(is_directory)
                .build();
            row.set_use_markup(false);
            row.add_prefix(
                &gtk::Image::builder()
                    .icon_name(if is_directory {
                        "folder-symbolic"
                    } else {
                        "text-x-generic-symbolic"
                    })
                    .build(),
            );
            if is_directory {
                if let Some(daemon) = self.imp().daemon.borrow().clone() {
                    let id = device_id.to_string();
                    let entry_id = entry_id.to_string();
                    row.connect_activated(move |_| {
                        daemon.send(DaemonCommand::BrowsePhoneFiles(
                            id.clone(),
                            entry_id.clone(),
                        ));
                    });
                }
            } else if let Some(daemon) = self.imp().daemon.borrow().clone() {
                let id = device_id.to_string();
                let entry_id = entry_id.to_string();
                let download = gtk::Button::builder()
                    .icon_name("folder-download-symbolic")
                    .tooltip_text("Download file")
                    .valign(gtk::Align::Center)
                    .build();
                download
                    .update_property(&[gtk::accessible::Property::Label("Download phone file")]);
                download.connect_clicked(move |_| {
                    daemon.send(DaemonCommand::DownloadPhoneFile(
                        id.clone(),
                        entry_id.clone(),
                    ));
                });
                row.add_suffix(&download);
            }
            dialog.list.append(&row);
        }
    }

    fn refresh_transfer_rows(&self) {
        let imp = self.imp();
        while let Some(child) = imp.transfers_list.first_child() {
            imp.transfers_list.remove(&child);
        }
        let transfers = imp.transfers.borrow();
        if let Some(parent) = imp.transfers_list.parent() {
            parent.set_visible(!transfers.is_empty());
        }
        for (transfer_id, progress) in transfers.iter() {
            let bytes = if progress.bytes_total == 0 {
                "Waiting for payload".to_string()
            } else {
                format!("{} / {} bytes", progress.bytes_done, progress.bytes_total)
            };
            let subtitle = progress
                .error
                .clone()
                .unwrap_or_else(|| format!("{} · {}", progress.state, bytes));
            let row = adw::ActionRow::builder()
                .title(format!("Transfer {}", short_transfer_id(transfer_id)))
                .subtitle(subtitle)
                .activatable(false)
                .build();
            let progress_bar = gtk::ProgressBar::builder()
                .valign(gtk::Align::Center)
                .width_request(150)
                .show_text(true)
                .build();
            if progress.bytes_total > 0 {
                progress_bar.set_fraction(
                    (progress.bytes_done as f64 / progress.bytes_total as f64).clamp(0.0, 1.0),
                );
            }
            row.add_suffix(&progress_bar);
            if progress.state != "completed" && progress.state != "cancelled" {
                if let Some(daemon) = imp.daemon.borrow().clone() {
                    let transfer_id = transfer_id.clone();
                    let button = gtk::Button::builder()
                        .icon_name("process-stop-symbolic")
                        .tooltip_text(if progress.can_resume {
                            "Cancel transfer"
                        } else {
                            "Stop transfer"
                        })
                        .valign(gtk::Align::Center)
                        .build();
                    button.update_property(&[gtk::accessible::Property::Label("Cancel transfer")]);
                    button.connect_clicked(move |_| {
                        daemon.send(DaemonCommand::CancelTransfer(transfer_id.clone()));
                    });
                    row.add_suffix(&button);
                }
            }
            imp.transfers_list.append(&row);
        }
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
                .css_classes(["body", "dimmed"])
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
        let mut receiver = daemon.subscribe_events();
        glib::MainContext::default().spawn_local(async move {
            use futures::StreamExt;
            while let Some(event) = receiver.next().await {
                let Some(list) = weak_list.upgrade() else {
                    break;
                };
                if weak_window.upgrade().is_none() {
                    break;
                }
                if matches!(
                    event,
                    CoreEvent::DeviceChanged { .. }
                        | CoreEvent::ConnectionChanged { .. }
                        | CoreEvent::PairingChanged { .. }
                        | CoreEvent::Error { .. }
                ) {
                    populate_discovery_list(&list, &daemon);
                }
            }
        });

        window.present();
    }
}

fn recent_count_label(total: usize, pending: usize) -> String {
    match (total, pending) {
        (_, 1) => "1 pairing request".to_string(),
        (_, pending) if pending > 1 => format!("{pending} pairing requests"),
        (1, _) => "1 recent device".to_string(),
        _ => format!("{total} recent devices"),
    }
}

fn short_transfer_id(transfer_id: &str) -> &str {
    transfer_id.get(..8).unwrap_or(transfer_id)
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

/// Full card shown for a paired device — header + DeskLink feature grid.
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
            .css_classes(["caption", "dimmed"])
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
        let name = device.name.clone();
        unpair_btn.connect_clicked(move |button| {
            let Some(window) = button.root().and_downcast::<gtk::Window>() else {
                return;
            };
            let dialog = adw::AlertDialog::builder()
                .heading("Unpair device?")
                .body(format!("Remove the trusted pairing with {name}?"))
                .build();
            dialog.add_responses(&[("cancel", "Cancel"), ("unpair", "Unpair")]);
            dialog.set_close_response("cancel");
            dialog.set_default_response(Some("cancel"));
            dialog.set_response_appearance("unpair", adw::ResponseAppearance::Destructive);
            let daemon = daemon.clone();
            let id = id.clone();
            dialog.choose(&window, None::<&gio::Cancellable>, move |response| {
                if response == "unpair" {
                    daemon.send(DaemonCommand::Unpair(id));
                }
            });
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
                .label(format!("Warning: {error}"))
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
                .css_classes(["caption", "dimmed"])
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
    row1.append(&plugin_tile_active("Clipboard", "edit-paste-symbolic", "Clipboard sync runs automatically while paired", {
        move |btn| {
            if let Some(root) = btn.root().and_downcast::<gtk::Window>() {
                show_info_dialog(&root, "Clipboard Sync", "Clipboard synchronization runs automatically while the device is connected and paired.");
            }
        }
    }));
    grid_card.append(&row1);

    grid_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // Row 2 — Control
    let row2 = plugin_row();
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
    row2.append(&plugin_tile_active(
        "Browse Files",
        "folder-remote-symbolic",
        "Request phone storage access",
        {
            let daemon = daemon.clone();
            let id = device.id.clone();
            let device_name = device.name.clone();
            move |btn| {
                daemon.send(DaemonCommand::SendSftpRequest(id.clone()));
                if let Some(root) = btn
                    .ancestor(DeskLinkWindow::static_type())
                    .and_downcast::<DeskLinkWindow>()
                {
                    root.show_phone_file_dialog(id.clone(), device_name.clone());
                }
            }
        },
    ));
    grid_card.append(&row2);

    grid_card.append(&gtk::Separator::new(gtk::Orientation::Horizontal));

    // Row 3 — v2 status and desktop integration
    let row3 = plugin_row();
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
    row3.append(&plugin_tile_active(
        "Refresh Status",
        "view-refresh-symbolic",
        "Request media, volume, notifications, commands, and file browsing",
        {
            let daemon = daemon.clone();
            let id = device.id.clone();
            move |_btn| {
                daemon.send(DaemonCommand::RequestMprisStatus(id.clone(), None));
                daemon.send(DaemonCommand::RequestNotifications(id.clone()));
                daemon.send(DaemonCommand::RequestSystemVolume(id.clone()));
                daemon.send(DaemonCommand::RequestRemoteCommands(id.clone()));
                daemon.send(DaemonCommand::SendSftpRequest(id.clone()));
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
            .css_classes(["body", "dimmed"])
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
            .css_classes(["body", "dimmed"])
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
        .build();

    if device.notifications.is_empty() {
        list.append(
            &gtk::Label::builder()
                .label("No mirrored notifications")
                .margin_top(24)
                .margin_bottom(24)
                .css_classes(["dimmed"])
                .build(),
        );
    } else {
        for notification in &device.notifications {
            list.append(&notification_row(&daemon, &device.id, notification, parent));
        }
    }
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .vexpand(true)
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(260)
        .build();
    content.append(&scroller);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));
    let list_weak = list.downgrade();
    let window_weak = window.downgrade();
    let daemon_refresh = daemon.clone();
    let device_id_refresh = device.id.clone();
    let parent_refresh = parent.clone();
    let mut receiver = daemon.subscribe_events();
    glib::MainContext::default().spawn_local(async move {
        use futures::StreamExt;
        while let Some(event) = receiver.next().await {
            let relevant = match event {
                CoreEvent::NotificationReceived { device_id, .. }
                | CoreEvent::ConnectionChanged { device_id, .. }
                | CoreEvent::PairingChanged { device_id, .. }
                | CoreEvent::Error {
                    device_id: Some(device_id),
                    ..
                } => device_id == device_id_refresh,
                CoreEvent::DeviceChanged { device } => device.id == device_id_refresh,
                _ => false,
            };
            if !relevant {
                continue;
            }
            let Some(window) = window_weak.upgrade() else {
                break;
            };
            let Some(list) = list_weak.upgrade() else {
                break;
            };
            let Some(device) = daemon_refresh
                .devices()
                .into_iter()
                .find(|candidate| candidate.id == device_id_refresh)
            else {
                continue;
            };
            while let Some(child) = list.first_child() {
                list.remove(&child);
            }
            if device.notifications.is_empty() {
                list.append(
                    &gtk::Label::builder()
                        .label("No mirrored notifications")
                        .margin_top(24)
                        .margin_bottom(24)
                        .css_classes(["dimmed"])
                        .build(),
                );
            } else {
                for notification in &device.notifications {
                    list.append(&notification_row(
                        &daemon_refresh,
                        &device.id,
                        notification,
                        &parent_refresh,
                    ));
                }
            }
            window.queue_resize();
        }
    });
    window.present();
}

fn notification_row(
    daemon: &DaemonHandle,
    device_id: &str,
    notification: &DeviceNotification,
    parent: &gtk::Window,
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
    row.set_title_lines(2);
    row.set_subtitle_lines(4);
    row.set_activatable(false);

    if let Some(reply_id) = &notification.request_reply_id {
        let reply = command_button("Reply", "mail-reply-sender-symbolic", {
            let daemon = daemon.clone();
            let device_id = device_id.to_string();
            let reply_id = reply_id.clone();
            let parent = parent.clone();
            move || {
                let entry = gtk::Entry::builder()
                    .placeholder_text("Write a reply")
                    .activates_default(true)
                    .build();
                let dialog = adw::AlertDialog::builder()
                    .heading("Reply to notification")
                    .extra_child(&entry)
                    .build();
                dialog.add_responses(&[("cancel", "Cancel"), ("send", "Send")]);
                dialog.set_close_response("cancel");
                dialog.set_default_response(Some("send"));
                let daemon = daemon.clone();
                let device_id = device_id.clone();
                let reply_id = reply_id.clone();
                dialog.choose(&parent, None::<&gio::Cancellable>, move |response| {
                    if response == "send" {
                        let message = entry.text().trim().to_string();
                        if !message.is_empty() {
                            daemon.send(DaemonCommand::ReplyNotification(
                                device_id, reply_id, message,
                            ));
                        }
                    }
                });
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
                .css_classes(["dimmed"])
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
                .css_classes(["dimmed"])
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
        .css_classes(["dimmed"])
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
                    .css_classes(["caption", "dimmed"])
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
