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
use gtk::{gdk, gio, glib};

use crate::device_links::daemon::{DaemonCommand, DaemonEvent, DaemonHandle};
use crate::device_links::device::{DeviceNotification, DeviceStatus, DeviceView, VolumeSink};
use crate::device_links::features::FeatureRegistry;
use crate::device_links::packet::{
    PACKET_TYPE_CLIPBOARD, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PING, PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SFTP_REQUEST,
    PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_SYSTEMVOLUME_REQUEST,
};
use crate::device_links::webrtc::transfer_board::{Finished, Outcome};

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
        /// Set while `refresh_devices` is rebuilding the sidebar.
        ///
        /// `ListBox::select_row` emits `row-selected` synchronously, and that
        /// handler calls `refresh_devices` again. The nested call cleared and
        /// rebuilt the list, then returned into the outer loop, which carried
        /// on appending rows from its now-stale vector — leaving duplicated
        /// entries and rendering the detail pane twice.
        pub refreshing: std::cell::Cell<bool>,
        /// The transfers window is on screen.
        pub transfers_open: std::cell::Cell<bool>,
        /// The user closed it while transfers were still running; it stays
        /// closed until the next batch of transfers starts.
        pub transfers_dismissed: std::cell::Cell<bool>,
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

            // "Show in folder" on a received-file toast.
            self.obj()
                .add_action_entries([gio::ActionEntry::builder("show-download")
                    .parameter_type(Some(glib::VariantTy::STRING))
                    .activate(|window: &super::DesklinkWindow, _, parameter| {
                        if let Some(path) = parameter.and_then(|value| value.get::<String>()) {
                            gtk::FileLauncher::new(Some(&gio::File::for_path(path)))
                                .open_containing_folder(
                                    Some(window),
                                    gio::Cancellable::NONE,
                                    |result| {
                                        if let Err(error) = result {
                                            eprintln!("[UI] Could not show the download: {error}");
                                        }
                                    },
                                );
                        }
                    })
                    .build()]);

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
        // Debug builds only: lets `tools/device_e2e.sh` drive desktop-initiated
        // features over the window's exported D-Bus action group, e.g.
        // `gdbus call --session --dest derx06.desklink.com
        //   --object-path /derx06/desklink/com/window/1
        //   --method org.gtk.Actions.Activate e2e "[<'ping ID'>]" {}`
        #[cfg(debug_assertions)]
        {
            let e2e_daemon = daemon.clone();
            self.add_action_entries([gio::ActionEntry::builder("e2e")
                .parameter_type(Some(glib::VariantTy::STRING))
                .activate(move |win: &Self, _, parameter| {
                    let Some(line) = parameter.and_then(|value| value.get::<String>()) else {
                        return;
                    };
                    let mut parts = line.splitn(3, ' ');
                    let (verb, id) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                    let (id, arg) = (id.to_string(), parts.next().unwrap_or("").to_string());
                    if verb == "desk-terminal" {
                        let result = crate::device_links::pty::desk::new_desk_session();
                        debug_log!("[E2E] desk-terminal: {result:?}");
                        return;
                    }
                    if verb == "desktop-selftest" {
                        let _ = std::thread::Builder::new()
                            .name("desklink-share-selftest".into())
                            .spawn(move || {
                                let result =
                                    crate::device_links::webrtc::screen::desktop_share_selftest(
                                        &id,
                                    );
                                debug_log!("[E2E] desktop-selftest: {result:?}");
                            });
                        return;
                    }
                    // "desk-input ID {mousepad body}": as if the phone sent it.
                    if verb == "desk-input" {
                        let result =
                            crate::device_links::webrtc::debug_inject_desktop_input(&id, &arg);
                        debug_log!("[E2E] desk-input {arg}: {result:?}");
                        return;
                    }
                    if verb == "remote-window" {
                        crate::remote_control::show_remote_control_dialog(
                            e2e_daemon.clone(),
                            id,
                            win,
                        );
                        return;
                    }
                    let command = match verb {
                        "request-pair" => DaemonCommand::RequestPair(id),
                        // Accepts with the key the desktop is showing, as a
                        // user comparing codes would.
                        "accept-pair" => {
                            let key = e2e_daemon
                                .devices()
                                .into_iter()
                                .find(|device| device.id == id)
                                .and_then(|device| device.verification_key);
                            match key {
                                Some(key) => DaemonCommand::AcceptPair(id, Some(key)),
                                None => return,
                            }
                        }
                        "ping" => DaemonCommand::SendPing(id),
                        "find-phone" => DaemonCommand::SendFindPhone(id),
                        "lock-phone" => DaemonCommand::SendLockRequest(id, true),
                        "send-file" => DaemonCommand::SendFile(id, arg.into()),
                        "share-text" => DaemonCommand::SendShareText(id, arg),
                        "notifications" => DaemonCommand::RequestNotifications(id),
                        "browse-files" => DaemonCommand::SendSftpRequest(id),
                        "terminal-access" => DaemonCommand::SetTerminalAccess(id, arg == "on"),
                        "view-phone" => DaemonCommand::StartPhoneRemoteControl(id),
                        // "type TEXT" / "key N": the phone window's keyboard path.
                        "type" => {
                            let mut payload = serde_json::Map::new();
                            payload.insert("key".into(), arg.clone().into());
                            DaemonCommand::SendMousepadRequest(id, payload)
                        }
                        "key" => {
                            let mut payload = serde_json::Map::new();
                            payload.insert(
                                "specialKey".into(),
                                arg.parse::<i64>().unwrap_or(0).into(),
                            );
                            DaemonCommand::SendMousepadRequest(id, payload)
                        }
                        "stop-phone" => DaemonCommand::StopPhoneRemoteControl(id),
                        // "tap X Y" on the phone, through the real lease path.
                        "tap" => {
                            let mut parts = arg.split(' ').filter_map(|v| v.parse::<i64>().ok());
                            let (Some(x), Some(y)) = (parts.next(), parts.next()) else {
                                return;
                            };
                            let mut payload = serde_json::Map::new();
                            payload.insert("singleclick".into(), true.into());
                            payload.insert("x".into(), x.into());
                            payload.insert("y".into(), y.into());
                            DaemonCommand::SendMousepadRequest(id, payload)
                        }
                        _ => {
                            debug_log!("[E2E] unknown command: {line}");
                            return;
                        }
                    };
                    debug_log!("[E2E] {verb}");
                    e2e_daemon.send(command);
                })
                .build()]);
        }
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
        if imp.refreshing.get() {
            return;
        }
        let Some(daemon) = imp.daemon.borrow().clone() else {
            return;
        };
        imp.refreshing.set(true);
        // RAII rather than a set at the end: `refresh_devices` returns early
        // from several places, and any missed reset would wedge the sidebar
        // permanently.
        let _guard = RefreshGuard(&imp.refreshing);

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

        self.follow_transfers(&daemon);

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

        // Runs before the snapshot short-circuit below, and before either
        // empty-list early return. These effects are driven by `recent_devices`,
        // which the search filter does not touch — running them after the
        // snapshot check meant a pair request or a phone notification from a
        // device the user had filtered out was dropped entirely, and the whole
        // backlog then fired at once when the search box was cleared. It is
        // also the only place the error banner is ever hidden.
        self.mirror_device_side_effects(&recent_devices, errors.is_empty());

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

    /// Desktop-notification mirroring and error-banner state.
    ///
    /// Kept out of the list-rendering path deliberately: these depend on what
    /// the daemon reports, not on what the sidebar happens to be showing, so
    /// they must survive both the snapshot short-circuit and the search filter.
    fn mirror_device_side_effects(&self, recent_devices: &[DeviceView], errors_empty: bool) {
        let imp = self.imp();

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
            // The borrow is resolved to a bool before the notification call.
            // Holding a `RefMut` across `send_app_notification` risked a panic
            // if GIO ever pumped the main loop and re-entered this timeout.
            let is_new = imp
                .notified_pair_requests
                .borrow_mut()
                .insert(device.id.clone());
            if is_new {
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

        if errors_empty
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
                let is_new = imp
                    .notified_remote_notifications
                    .borrow_mut()
                    .insert(unique_id.clone());
                if is_new {
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

    /// Shows the transfers window while files are moving and tells the user how
    /// each one ended.
    fn follow_transfers(&self, daemon: &DaemonHandle) {
        let imp = self.imp();
        let finished = daemon.take_finished_transfers();
        if !finished.is_empty() {
            let names: std::collections::HashMap<String, String> = daemon
                .devices()
                .into_iter()
                .map(|device| (device.id, device.name))
                .collect();
            for done in &finished {
                let device = names
                    .get(&done.device_id)
                    .map_or("your device", String::as_str);
                self.announce_transfer(done, device);
            }
        }
        if daemon.transfer_rows().is_empty() {
            imp.transfers_dismissed.set(false);
        } else if !imp.transfers_open.get() && !imp.transfers_dismissed.get() {
            imp.transfers_open.set(true);
            let window = self.downgrade();
            crate::transfers::show(daemon.clone(), self.upcast_ref(), move || {
                if let Some(window) = window.upgrade() {
                    window.imp().transfers_open.set(false);
                    window.imp().transfers_dismissed.set(true);
                }
            });
        }
    }

    fn announce_transfer(&self, done: &Finished, device: &str) {
        let name = &done.name;
        let (message, notify) = match &done.outcome {
            Outcome::Received(_) => (format!("Received {name} from {device}"), true),
            Outcome::Sent => (format!("Sent {name} to {device}"), false),
            Outcome::Cancelled => (format!("Cancelled {name}"), false),
            Outcome::Failed(reason) => {
                let what = if done.incoming { "receive" } else { "send" };
                (format!("Could not {what} {name}: {reason}"), true)
            }
        };
        // Names and reasons come from the peer; toasts read markup by default.
        let toast = adw::Toast::new(&message);
        toast.set_use_markup(false);
        toast.set_timeout(if matches!(done.outcome, Outcome::Received(_)) {
            10
        } else {
            5
        });
        if let Outcome::Received(path) = &done.outcome {
            toast.set_button_label(Some("Show in folder"));
            toast.set_action_name(Some("win.show-download"));
            toast.set_action_target_value(Some(&path.to_string_lossy().to_string().to_variant()));
        }
        self.imp().toast_overlay.add_toast(toast);
        if notify {
            self.send_app_notification(
                &format!("desklink-transfer-{}", done.id),
                "DeskLink",
                &message,
            );
        }
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
        content.append(&qr_pairing_section(&daemon, &window));
        content.append(&scroll);
        toolbar.set_content(Some(&content));
        window.set_content(Some(&toolbar));

        refresh.connect_clicked({
            let daemon = daemon.clone();
            move |_| daemon.send(DaemonCommand::Discover)
        });

        let discovery_snapshot = std::rc::Rc::new(std::cell::RefCell::new(None));
        populate_discovery_list(&list, &daemon, &discovery_snapshot);
        let weak_list = list.downgrade();
        let weak_window = window.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            let Some(list) = weak_list.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if weak_window.upgrade().is_none() {
                return glib::ControlFlow::Break;
            }
            populate_discovery_list(&list, &daemon, &discovery_snapshot);
            glib::ControlFlow::Continue
        });

        window.present();
    }
}

/// Rebuilds the "Pair New Device" list, but only when its contents changed.
///
/// This used to tear down and rebuild every row once a second unconditionally.
/// A tick landing between mouse-down and mouse-up destroyed the "Request Pair"
/// button mid-click, so the press was silently lost and the user had to try
/// again with no feedback; scroll position and the spinner also reset every
/// second. `refresh_devices` already guards itself this way.
///
/// Paired devices are filtered out: they belong in the sidebar, and rendering
/// them here built a full feature card for each one, in a dialog whose entire
/// purpose is pairing something new.
fn populate_discovery_list(
    list: &gtk::ListBox,
    daemon: &DaemonHandle,
    last_snapshot: &std::cell::RefCell<Option<String>>,
) {
    let devices: Vec<_> = daemon
        .devices()
        .into_iter()
        .filter(|device| !device.trusted)
        .collect();

    let snapshot = serde_json::to_string(&devices).unwrap_or_default();
    if last_snapshot.borrow().as_ref() == Some(&snapshot) {
        return;
    }
    last_snapshot.replace(Some(snapshot));

    while let Some(child) = list.first_child() {
        list.remove(&child);
    }

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

/// Clears the re-entrancy flag however `refresh_devices` exits.
struct RefreshGuard<'a>(&'a std::cell::Cell<bool>);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
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

/// How a device's reachability should read, and the CSS class that goes with it.
///
/// Both the sidebar row and the detail pane call this. The detail pane used to
/// hardcode a green "● Connected" literal, so a device that was unreachable, or
/// stuck mid-handover, showed as fully connected in the pane while the sidebar
/// row beside it correctly said otherwise. Deriving both from one function is
/// what keeps them from disagreeing again.
fn device_status_presentation(device: &DeviceView) -> (&'static str, &'static str) {
    match &device.status {
        // Reachable over the bootstrap link but still negotiating the encrypted
        // feature transport. Features work, so this is not a warning state, but
        // calling it plain "Connected" hid a stuck handover completely.
        DeviceStatus::Paired if !device.feature_transport_ready => ("Connecting", "warning"),
        DeviceStatus::Paired => ("Connected", "success"),
        DeviceStatus::Unreachable => ("Unavailable", "error"),
        other => (other.label(), "dim-label"),
    }
}

fn device_sidebar_row(daemon: &DaemonHandle, device: DeviceView) -> adw::ActionRow {
    let (status, _) = device_status_presentation(&device);
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

    let (status_text, status_class) = device_status_presentation(&device);
    let status_pill = gtk::Label::builder()
        .label(format!("● {status_text}"))
        .css_classes(["caption", status_class])
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
            debug_log!("[UI] Unpair clicked for {}", id);
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
                            .title("Select Files to Send")
                            .build();

                        let daemon = daemon.clone();
                        let id = id.clone();
                        file_dialog.open_multiple(
                            Some(&root),
                            gio::Cancellable::NONE,
                            move |result| {
                                let Ok(files) = result else {
                                    return;
                                };
                                for file in files.iter::<gio::File>().flatten() {
                                    if let Some(path) = file.path() {
                                        debug_log!("[UI] Selected file to send: {:?}", path);
                                        daemon.send(DaemonCommand::SendFile(id.clone(), path));
                                    }
                                }
                            },
                        );
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
    if device.feature_transport_ready || can_send_feature(&device, PACKET_TYPE_SFTP_REQUEST) {
        row2.append(&plugin_tile_active(
            "Browse Files",
            "folder-remote-symbolic",
            "Browse, download and manage the folders your phone shares",
            {
                let daemon = daemon.clone();
                let id = device.id.clone();
                let browsing_device = device.clone();
                move |btn| {
                    let Some(root) = btn.root().and_downcast::<gtk::Window>() else {
                        return;
                    };
                    if browsing_device.feature_transport_ready {
                        crate::phone_files::show(
                            daemon.clone(),
                            id.clone(),
                            &browsing_device.name,
                            &root,
                        );
                    } else {
                        // Until the WebRTC handover finishes only the legacy
                        // SFTP request is available.
                        daemon.send(DaemonCommand::SendSftpRequest(id.clone()));
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
    outer.append(&terminal_access_section(daemon, &device));
    outer.append(&remote_control_section(&device));
    outer.upcast()
}

/// The consent switch for phone-driven terminals, and — while the phone is
/// using them — a visible indicator.
///
/// A terminal is a shell on this computer, so it is **off until the user turns
/// it on here**, per phone, and turning it off cuts the phone off at once. The
/// indicator is text with an icon, not colour alone, and both rows have
/// accessible names and descriptions.
fn terminal_access_section(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(0)
        .build();
    section.append(
        &gtk::Label::builder()
            .label("Terminal")
            .xalign(0.0)
            .margin_top(20)
            .margin_bottom(8)
            .css_classes(["heading"])
            .build(),
    );

    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();

    const WARNING: &str = "Lets this phone run commands on this computer";
    // `active` is set before the handler is connected, so building the row
    // from persisted state does not itself send a command.
    let switch = adw::SwitchRow::builder()
        .title("Allow terminal access from this phone")
        .subtitle(WARNING)
        .active(device.terminal_access)
        .build();
    gtk::prelude::AccessibleExtManual::update_property(
        &switch,
        &[gtk::accessible::Property::Description(WARNING)],
    );
    {
        let daemon = daemon.clone();
        let id = device.id.clone();
        switch.connect_active_notify(move |row| {
            debug_log!("[UI] Terminal access for {id} set to {}", row.is_active());
            daemon.send(DaemonCommand::SetTerminalAccess(
                id.clone(),
                row.is_active(),
            ));
        });
    }
    list.append(&switch);

    // A terminal started here is shared: it opens in a window on this
    // computer and appears in the phone's session list.
    let shared = adw::ActionRow::builder()
        .title("New shared terminal")
        .subtitle("Opens here and appears on this phone, live in both")
        .activatable(true)
        .build();
    shared.add_suffix(
        &gtk::Image::builder()
            .icon_name("utilities-terminal-symbolic")
            .valign(gtk::Align::Center)
            .build(),
    );
    shared.connect_activated(|_| {
        if let Err(error) = crate::device_links::pty::desk::new_desk_session() {
            eprintln!("[UI] Could not start a shared terminal: {error}");
        }
    });
    list.append(&shared);

    if device.terminal_sessions_attached > 0 {
        let count = device.terminal_sessions_attached;
        let plural = if count == 1 { "session" } else { "sessions" };
        let indicator = adw::ActionRow::builder()
            .title(format!("Phone has terminal access ({count} {plural})"))
            .subtitle("This phone is connected to a terminal on this computer")
            .activatable(false)
            .build();
        indicator.add_prefix(
            &gtk::Image::builder()
                .icon_name("utilities-terminal-symbolic")
                .valign(gtk::Align::Center)
                .build(),
        );
        list.append(&indicator);
    }

    section.append(&list);
    section
}

/// Approve, while at the desk, the phone controlling this computer — so later
/// sessions from the phone start without anyone here to click Share.
fn remote_control_section(device: &DeviceView) -> gtk::Box {
    const APPROVED: &str =
        "Approved: this phone can view and control this computer without asking here, until DeskLink restarts";
    const NOT_APPROVED: &str =
        "Each session from this phone asks here first. Approve now so it can connect on its own";
    const WAITING: &str =
        "Choose the screen, turn on \u{201c}Allow Remote Interaction\u{201d}, then Share";
    const VIEW_ONLY: &str =
        "Shared without remote interaction, so not remembered. Try again with \u{201c}Allow Remote Interaction\u{201d} on";

    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    section.append(
        &gtk::Label::builder()
            .label("Remote control")
            .xalign(0.0)
            .margin_top(20)
            .margin_bottom(8)
            .css_classes(["heading"])
            .build(),
    );
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
    let row = adw::ActionRow::builder()
        .title("Control this computer from this phone")
        .subtitle(if approved { APPROVED } else { NOT_APPROVED })
        .subtitle_lines(3)
        .build();
    let approve = gtk::Button::builder()
        .label(if approved {
            "Approve again"
        } else {
            "Approve…"
        })
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    let forget = gtk::Button::builder()
        .label("Forget")
        .valign(gtk::Align::Center)
        .visible(approved)
        .build();
    gtk::prelude::AccessibleExtManual::update_property(
        &forget,
        &[gtk::accessible::Property::Description(
            "Stop this phone controlling the computer without asking",
        )],
    );
    row.add_suffix(&forget);
    row.add_suffix(&approve);
    {
        let (row, forget) = (row.downgrade(), forget.downgrade());
        let id = device.id.clone();
        approve.connect_clicked(move |button| {
            button.set_sensitive(false);
            if let Some(row) = row.upgrade() {
                row.set_subtitle(WAITING);
            }
            let (row, forget, button, id) =
                (row.clone(), forget.clone(), button.downgrade(), id.clone());
            glib::spawn_future_local(async move {
                let outcome = gio::spawn_blocking(move || {
                    crate::device_links::webrtc::preauthorize_remote_control(&id)
                })
                .await
                .unwrap_or_else(|_| Err("The approval was interrupted".to_string()));
                let granted = matches!(outcome, Ok(true));
                if let Some(row) = row.upgrade() {
                    row.set_subtitle(match &outcome {
                        Ok(true) => APPROVED,
                        Ok(false) => VIEW_ONLY,
                        Err(_) => NOT_APPROVED,
                    });
                }
                if let Err(error) = outcome {
                    eprintln!("[UI] Remote-control approval did not complete: {error}");
                }
                if let Some(forget) = forget.upgrade() {
                    forget.set_visible(granted);
                }
                if let Some(button) = button.upgrade() {
                    button.set_sensitive(true);
                    button.set_label(if granted {
                        "Approve again"
                    } else {
                        "Approve…"
                    });
                }
            });
        });
    }
    {
        let (row, approve) = (row.downgrade(), approve.downgrade());
        let id = device.id.clone();
        forget.connect_clicked(move |button| {
            crate::device_links::config::set_remote_desktop_token(&id, None);
            button.set_visible(false);
            if let Some(row) = row.upgrade() {
                row.set_subtitle(NOT_APPROVED);
            }
            if let Some(approve) = approve.upgrade() {
                approve.set_label("Approve…");
            }
        });
    }
    list.append(&row);
    section.append(&list);
    section
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
    match device.connection_path.as_deref() {
        Some("lan") => subtitle_parts.push("Local network".to_string()),
        Some("direct") => subtitle_parts.push("Direct over the internet".to_string()),
        Some("turn") => subtitle_parts.push("Through a TURN relay".to_string()),
        _ => {}
    }
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
                let shown_key = device.verification_key.clone();
                move || {
                    debug_log!("[UI] Accept clicked for {}", id);
                    daemon.send(DaemonCommand::AcceptPair(id.clone(), shown_key.clone()));
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

/// "Scan to pair": shows a live QR code from `pairing_qr`. A scanned code is
/// consumed, so a fresh one appears for the next phone; closing the dialog
/// withdraws it so no code outlives the window that showed it.
fn qr_pairing_section(daemon: &DaemonHandle, window: &adw::Window) -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .halign(gtk::Align::Center)
        .build();
    let picture = gtk::Picture::builder()
        .width_request(220)
        .height_request(220)
        .can_shrink(false)
        .build();
    gtk::prelude::AccessibleExtManual::update_property(
        &picture,
        &[gtk::accessible::Property::Label("Pairing QR code")],
    );
    section.append(&picture);
    section.append(
        &gtk::Label::builder()
            .label("Scan this with the DeskLink app to pair. No code to compare.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build(),
    );

    daemon.send(DaemonCommand::StartQrPairing);
    let shown = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    // Set on close, so a tick racing the close can never start a code that no
    // window shows (it would stay live for 5 minutes).
    let closed = std::rc::Rc::new(std::cell::Cell::new(false));
    let tick_closed = std::rc::Rc::clone(&closed);
    let weak_picture = picture.downgrade();
    let poll_daemon = daemon.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
        let Some(picture) = weak_picture.upgrade() else {
            return glib::ControlFlow::Break;
        };
        if tick_closed.get() {
            return glib::ControlFlow::Break;
        }
        match crate::device_links::pairing_qr::current_uri() {
            Some(uri) if shown.borrow().as_deref() != Some(uri.as_str()) => {
                if let Some(texture) = qr_texture(&uri) {
                    picture.set_paintable(Some(&texture));
                }
                *shown.borrow_mut() = Some(uri);
            }
            Some(_) => {}
            None => {
                // Used or expired: offer a new one.
                if shown.borrow_mut().take().is_some() {
                    poll_daemon.send(DaemonCommand::StartQrPairing);
                }
            }
        }
        glib::ControlFlow::Continue
    });
    window.connect_close_request({
        let daemon = daemon.clone();
        move |_| {
            closed.set(true);
            daemon.send(DaemonCommand::CancelQrPairing);
            glib::Propagation::Proceed
        }
    });
    section
}

/// Rasterises a QR code: 6 px per module and the standard 4-module quiet zone.
fn qr_texture(uri: &str) -> Option<gdk::MemoryTexture> {
    const SCALE: usize = 6;
    const QUIET: usize = 4;
    let (width, dark) = crate::device_links::pairing_qr::modules(uri).ok()?;
    let side = (width + 2 * QUIET) * SCALE;
    let mut pixels = vec![255u8; side * side * 3];
    for (index, is_dark) in dark.iter().enumerate() {
        if !is_dark {
            continue;
        }
        let (row, column) = (index / width + QUIET, index % width + QUIET);
        for y in row * SCALE..(row + 1) * SCALE {
            let start = (y * side + column * SCALE) * 3;
            pixels[start..start + SCALE * 3].fill(0);
        }
    }
    Some(gdk::MemoryTexture::new(
        side as i32,
        side as i32,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(pixels),
        side * 3,
    ))
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
            instance_id: None,
        };
        let device = DeviceView::from_info(&info, "127.0.0.1".to_string(), true);

        assert!(can_send_feature(&device, PACKET_TYPE_PING));
        assert!(!can_send_feature(&device, PACKET_TYPE_MOUSEPAD_REQUEST));
    }

    /// The detail pane used to render a hardcoded green "● Connected" for every
    /// paired device, so an unreachable phone read as connected right next to a
    /// sidebar row that said otherwise. Both now derive from this function.
    #[test]
    fn status_presentation_never_reports_an_unreachable_device_as_connected() {
        let info = DeviceInfo {
            id: "0123456789abcdef0123456789abcdef".to_string(),
            name: "Phone".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        let mut device = DeviceView::from_info(&info, "127.0.0.1".to_string(), true);

        device.status = DeviceStatus::Unreachable;
        assert_eq!(
            device_status_presentation(&device),
            ("Unavailable", "error")
        );

        // Paired but the WebRTC handover has not finished.
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = false;
        assert_eq!(
            device_status_presentation(&device),
            ("Connecting", "warning")
        );

        device.feature_transport_ready = true;
        assert_eq!(
            device_status_presentation(&device),
            ("Connected", "success")
        );
    }
}
