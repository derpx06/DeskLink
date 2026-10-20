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
    use std::collections::{HashMap, HashSet};

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/derx06/desklink/com/window.ui")]
    pub struct DesklinkWindow {
        // Template widgets
        #[template_child]
        pub main_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub welcome_body: TemplateChild<gtk::Box>,
        #[template_child]
        pub split_view: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub content_page: TemplateChild<adw::NavigationPage>,
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
        pub refresh_button: TemplateChild<gtk::Button>,
        #[template_child]
        pub detail_body: TemplateChild<gtk::Box>,
        #[template_child]
        pub detail_scroll: TemplateChild<gtk::ScrolledWindow>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        pub daemon: RefCell<Option<DaemonHandle>>,
        /// The sidebar's rows in order: device id, what the row showed when it
        /// was drawn, and the row. Rows are updated in place so the one with
        /// keyboard focus keeps it.
        pub sidebar_rows: RefCell<Vec<(String, String, gtk::ListBoxRow)>>,
        /// What the device pane shows: the device id and everything it was
        /// drawn from. The pane is rebuilt only when this changes.
        pub detail_snapshot: RefCell<Option<(String, String)>>,
        /// Devices whose remote-control approval is waiting on the portal.
        pub approving: RefCell<HashSet<String>>,
        /// The daemon's own words for the last error, behind "Details".
        pub last_raw_error: RefCell<String>,
        /// When a lasting error (offline) was last reported.
        pub banner_since: std::cell::Cell<Option<std::time::Instant>>,
        /// The last error toast and when, so a repeating error toasts once.
        pub last_error_toast: RefCell<Option<(String, std::time::Instant)>>,
        pub selected_device_id: RefCell<Option<String>>,
        pub notified_pair_requests: RefCell<HashSet<String>>,
        pub notified_remote_notifications: RefCell<HashSet<String>>,
        /// The pairing dialog on screen for each incoming request, with the
        /// code it shows. A request whose code changes gets a fresh dialog, so
        /// Accept always confirms the code the user actually compared.
        pub pair_dialogs: RefCell<HashMap<String, (Option<String>, adw::AlertDialog)>>,
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
        /// Debug builds only: devices injected by the `e2e` action's
        /// `fake-device` verb, for screenshots without a phone.
        #[cfg(debug_assertions)]
        pub fake_devices: RefCell<Vec<DeviceView>>,
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
            // Typing into a dialog's entry must not land in the device search.
            self.obj()
                .connect_notify_local(Some("visible-dialog"), |window, _| {
                    let capture = window
                        .visible_dialog()
                        .is_none()
                        .then(|| window.clone().upcast::<gtk::Widget>());
                    window
                        .imp()
                        .search_bar
                        .set_key_capture_widget(capture.as_ref());
                });
            self.search_button
                .bind_property("active", &*self.search_bar, "search-mode-enabled")
                .bidirectional()
                .sync_create()
                .build();
            self.welcome_body.append(&pairing_steps());
            self.devices_list.set_placeholder(Some(
                &gtk::Label::builder()
                    .label("No matching devices")
                    .margin_top(24)
                    .margin_bottom(24)
                    .css_classes(["dim-label"])
                    .build(),
            ));

            self.obj().add_action_entries([
                // "Show in folder" on a received-file toast.
                gio::ActionEntry::builder("show-download")
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
                    .build(),
                // "Details" on an error toast: the daemon's own text.
                gio::ActionEntry::builder("error-details")
                    .parameter_type(Some(glib::VariantTy::STRING))
                    .activate(|window: &super::DesklinkWindow, _, parameter| {
                        if let Some(raw) = parameter.and_then(|value| value.get::<String>()) {
                            window.show_error_details(&raw);
                        }
                    })
                    .build(),
                gio::ActionEntry::builder("pair-new")
                    .activate(|window: &super::DesklinkWindow, _, _| window.show_discovery_dialog())
                    .build(),
                gio::ActionEntry::builder("toggle-search")
                    .activate(|window: &super::DesklinkWindow, _, _| {
                        let imp = window.imp();
                        if imp.main_stack.visible_child_name().as_deref() == Some("devices") {
                            imp.split_view.set_show_content(false);
                            imp.search_button.set_active(!imp.search_button.is_active());
                        }
                    })
                    .build(),
            ]);

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

            // Only a user's click navigates; the selection the refresh restores
            // must not push the device page over the list on a narrow window.
            let window = self.obj().downgrade();
            self.devices_list.connect_row_activated(move |_, _| {
                if let Some(window) = window.upgrade() {
                    window.imp().split_view.set_show_content(true);
                }
            });

            let window = self.obj().downgrade();
            self.error_banner.connect_button_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    let raw = window.imp().last_raw_error.borrow().clone();
                    window.show_error_details(&raw);
                }
            });

            let window = self.obj().downgrade();
            self.refresh_button.connect_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    window.refresh_selected_status();
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
        @extends gtk::Widget, gtk::Window, gtk::ApplicationWindow, adw::ApplicationWindow,
        @implements gio::ActionGroup, gio::ActionMap, gtk::Accessible, gtk::Buildable,
            gtk::ConstraintTarget, gtk::Native, gtk::Root, gtk::ShortcutManager;
}

impl DesklinkWindow {
    pub fn new<P: IsA<gtk::Application>>(application: &P) -> Self {
        glib::Object::builder()
            .property("application", application)
            .build()
    }

    /// Selects `device_id` and shows its page (from the top-bar extension).
    pub fn show_device(&self, device_id: &str) {
        self.imp()
            .selected_device_id
            .replace(Some(device_id.to_string()));
        self.refresh_devices();
        self.imp().split_view.set_show_content(true);
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
                    if debug_ui_verb(win, verb, &id, &arg) {
                        return;
                    }
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
            self.report_error(error);
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

        #[allow(unused_mut)]
        let mut devices = daemon.devices();
        #[cfg(debug_assertions)]
        devices.extend(imp.fake_devices.borrow().iter().cloned());
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
        self.mirror_device_side_effects(&daemon, &recent_devices, errors.is_empty());

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

        if recent_devices.is_empty() {
            imp.notified_pair_requests.borrow_mut().clear();
            imp.notified_remote_notifications.borrow_mut().clear();
            imp.main_stack.set_visible_child_name("welcome");
            imp.refresh_button.set_visible(false);
            self.sync_sidebar(&[]);
            imp.detail_snapshot.replace(None);
            return;
        }
        imp.main_stack.set_visible_child_name("devices");
        self.sync_sidebar(&filtered_devices);

        if filtered_devices.is_empty() {
            self.show_detail_empty(
                "system-search-symbolic",
                "No Matching Devices",
                "Try a different name, type or address.",
            );
            return;
        }
        self.render_selected_device(&daemon, &filtered_devices);
    }

    /// Brings the sidebar up to date without rebuilding rows that did not
    /// change, so a row with keyboard focus keeps it — arrow keys select
    /// rows, and a rebuild after each one dropped focus after one keypress.
    fn sync_sidebar(&self, devices: &[DeviceView]) {
        let imp = self.imp();
        let wanted: Vec<(String, String)> = devices
            .iter()
            .map(|device| (device.id.clone(), sidebar_key(device)))
            .collect();
        let same_devices = {
            let rows = imp.sidebar_rows.borrow();
            rows.len() == wanted.len()
                && rows
                    .iter()
                    .zip(&wanted)
                    .all(|((shown, _, _), (id, _))| shown == id)
        };
        if same_devices {
            let mut rows = imp.sidebar_rows.borrow_mut();
            for ((_, shown_key, row), ((_, key), device)) in
                rows.iter_mut().zip(wanted.into_iter().zip(devices))
            {
                if *shown_key != key {
                    fill_sidebar_row(row, device);
                    *shown_key = key;
                }
            }
        } else {
            // The row that had focus, by device, to give it back afterwards.
            let focused = gtk::prelude::RootExt::focus(self)
                .filter(|widget| widget.is_ancestor(&*imp.devices_list))
                .and_then(|widget| widget.ancestor(gtk::ListBoxRow::static_type()))
                .map(|row| row.widget_name().to_string());
            imp.devices_list.remove_all();
            let mut rows = imp.sidebar_rows.borrow_mut();
            rows.clear();
            for ((id, key), device) in wanted.into_iter().zip(devices) {
                let row = gtk::ListBoxRow::new();
                row.set_widget_name(&id);
                fill_sidebar_row(&row, device);
                imp.devices_list.append(&row);
                rows.push((id, key, row));
            }
            drop(rows);
            if let Some(row) = focused.and_then(|id| self.sidebar_row(&id)) {
                row.grab_focus();
            }
        }
        let selected = imp
            .selected_device_id
            .borrow()
            .clone()
            .and_then(|id| self.sidebar_row(&id));
        if selected.is_some() && imp.devices_list.selected_row() != selected {
            imp.devices_list.select_row(selected.as_ref());
        }
    }

    fn sidebar_row(&self, id: &str) -> Option<gtk::ListBoxRow> {
        self.imp()
            .sidebar_rows
            .borrow()
            .iter()
            .find(|(shown, _, _)| shown == id)
            .map(|(_, _, row)| row.clone())
    }

    /// Runs `refresh_devices` once the current handler has returned, e.g.
    /// after a click whose own button the refresh may replace.
    fn refresh_soon(&self) {
        let window = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(window) = window.upgrade() {
                window.refresh_devices();
            }
        });
    }

    /// Desktop-notification mirroring, pairing dialogs and error-banner state.
    ///
    /// Kept out of the list-rendering path deliberately: these depend on what
    /// the daemon reports, not on what the sidebar happens to be showing, so
    /// they must survive both the snapshot short-circuit and the search filter.
    fn mirror_device_side_effects(
        &self,
        daemon: &DaemonHandle,
        recent_devices: &[DeviceView],
        errors_empty: bool,
    ) {
        let imp = self.imp();

        let current_pair_requests = recent_devices
            .iter()
            .filter(|device| matches!(device.status, DeviceStatus::PairRequestedByPeer))
            .map(|device| device.id.clone())
            .collect::<std::collections::HashSet<_>>();
        imp.notified_pair_requests
            .borrow_mut()
            .retain(|device_id| current_pair_requests.contains(device_id));
        // A request that ended (accepted on the phone, timed out) or whose code
        // changed takes its dialog with it.
        let stale: Vec<(String, adw::AlertDialog)> = {
            let mut dialogs = imp.pair_dialogs.borrow_mut();
            let ids: Vec<String> = dialogs
                .iter()
                .filter(|(id, (key, _))| {
                    !recent_devices.iter().any(|device| {
                        &device.id == *id
                            && matches!(device.status, DeviceStatus::PairRequestedByPeer)
                            && &device.verification_key == key
                    })
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| dialogs.remove(&id).map(|(_, dialog)| (id, dialog)))
                .collect()
        };
        // Requests still open whose code changed: shown again with the new code.
        let mut reopen = std::collections::HashSet::new();
        for (id, dialog) in stale {
            dialog.force_close();
            reopen.insert(id);
        }
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
                    .map(|key| format!("Verification code: {}", spaced_code(key)))
                    .unwrap_or_else(|| {
                        "Open DeskLink to accept or reject the request.".to_string()
                    });
                self.send_app_notification(
                    &format!("pair-request-{}", device.id),
                    &format!("Pairing request from {}", device.name),
                    &body,
                );
            }
            // A dismissed dialog stays dismissed until the code changes; the
            // device page still offers Accept and Reject.
            if is_new || reopen.contains(&device.id) {
                self.present_pair_dialog(daemon, device);
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

        // A lasting state (offline) keeps its banner a while after its last
        // report, so a once-a-cycle error does not make it flicker.
        let banner_settled = imp
            .banner_since
            .get()
            .is_none_or(|since| since.elapsed() >= std::time::Duration::from_secs(10));
        if errors_empty
            && banner_settled
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

    /// "Pair with Pixel 8?" with the code to compare, large and spaced.
    fn present_pair_dialog(&self, daemon: &DaemonHandle, device: &DeviceView) {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Pair with {}?", device.name)),
            Some("Make sure this code matches the one shown on your phone before you accept."),
        );
        dialog.add_responses(&[("reject", "_Reject"), ("accept", "_Accept")]);
        dialog.set_response_appearance("reject", adw::ResponseAppearance::Destructive);
        dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("accept"));
        // Escape only puts the dialog away; the request stays answerable on the
        // device page.
        dialog.set_close_response("later");
        if let Some(key) = &device.verification_key {
            dialog.set_extra_child(Some(&pairing_code_label(key)));
        }
        {
            let daemon = daemon.clone();
            let id = device.id.clone();
            let shown_key = device.verification_key.clone();
            let window = self.downgrade();
            dialog.connect_response(None, move |dialog, response| {
                match response {
                    "accept" => {
                        debug_log!("[UI] Accept clicked for {}", id);
                        daemon.send(DaemonCommand::AcceptPair(id.clone(), shown_key.clone()));
                    }
                    "reject" => {
                        eprintln!("[UI] Reject clicked for {}", id);
                        daemon.send(DaemonCommand::RejectPair(id.clone()));
                    }
                    _ => {}
                }
                if let Some(window) = window.upgrade() {
                    let mut dialogs = window.imp().pair_dialogs.borrow_mut();
                    if dialogs.get(&id).is_some_and(|(_, shown)| shown == dialog) {
                        dialogs.remove(&id);
                    }
                }
            });
        }
        if let Some((_, old)) = self.imp().pair_dialogs.borrow_mut().insert(
            device.id.clone(),
            (device.verification_key.clone(), dialog.clone()),
        ) {
            old.force_close();
        }
        dialog.present(Some(self));
    }

    fn show_detail_empty(&self, icon: &str, title: &str, description: &str) {
        let imp = self.imp();
        let shown = Some((String::new(), title.to_string()));
        if *imp.detail_snapshot.borrow() == shown {
            return;
        }
        imp.detail_snapshot.replace(shown);
        imp.content_page.set_title(title);
        imp.refresh_button.set_visible(false);
        let body = &imp.detail_body;
        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        body.append(
            &adw::StatusPage::builder()
                .icon_name(icon)
                .title(title)
                .description(description)
                .vexpand(true)
                .build(),
        );
    }

    fn render_selected_device(&self, daemon: &DaemonHandle, devices: &[DeviceView]) {
        let selected = self.imp().selected_device_id.borrow().clone();
        let Some(device) = selected.and_then(|id| devices.iter().find(|device| device.id == id))
        else {
            self.show_detail_empty(
                "phone-symbolic",
                "No Device Selected",
                "Select a device from the list.",
            );
            return;
        };
        let imp = self.imp();
        imp.content_page.set_title(&device.name);
        imp.refresh_button.set_visible(
            matches!(device.status, DeviceStatus::Paired)
                && (can_send_feature(device, PACKET_TYPE_MPRIS_REQUEST)
                    || can_send_feature(device, PACKET_TYPE_NOTIFICATION_REQUEST)),
        );
        // Everything the page is drawn from; the page is rebuilt only when
        // this changes, so a switch being toggled is not torn down under the
        // pointer every second.
        let approving = imp.approving.borrow().contains(&device.id);
        let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
        let snapshot = (
            device.id.clone(),
            serde_json::to_string(&(device, approving, approved)).unwrap_or_default(),
        );
        let previous = imp.detail_snapshot.replace(Some(snapshot.clone()));
        if previous.as_ref() == Some(&snapshot) {
            return;
        }
        let same_device = previous.is_some_and(|(id, _)| id == device.id);

        let body = imp.detail_body.get();
        let body_widget = body.clone().upcast::<gtk::Widget>();
        // Where focus and the scroll position were, to put them back.
        let focus_path = gtk::prelude::RootExt::focus(self)
            .filter(|_| same_device)
            .and_then(|focus| widget_path(&body_widget, &focus));
        let scroll = imp.detail_scroll.vadjustment();
        let offset = if same_device { scroll.value() } else { 0.0 };

        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        body.append(&device_page(daemon, device, approving));

        scroll.set_value(offset);
        let body = body.downgrade();
        glib::idle_add_local_once(move || {
            scroll.set_value(offset);
            let Some(body) = body.upgrade() else {
                return;
            };
            if let Some(widget) = focus_path.and_then(|path| widget_at(body.upcast_ref(), &path)) {
                widget.grab_focus();
            }
        });
    }

    /// The header's Refresh button: asks the selected phone for its media and
    /// notification state again.
    fn refresh_selected_status(&self) {
        let Some(daemon) = self.imp().daemon.borrow().clone() else {
            return;
        };
        let Some(id) = self.imp().selected_device_id.borrow().clone() else {
            return;
        };
        let Some(device) = daemon.devices().into_iter().find(|device| device.id == id) else {
            return;
        };
        if can_send_feature(&device, PACKET_TYPE_MPRIS_REQUEST) {
            daemon.send(DaemonCommand::RequestMprisStatus(id.clone(), None));
        }
        if can_send_feature(&device, PACKET_TYPE_NOTIFICATION_REQUEST) {
            daemon.send(DaemonCommand::RequestNotifications(id));
        }
        self.show_feature_toast("Refreshing…");
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
            toast.set_button_label(Some("Show in Folder"));
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

    /// Puts a daemon error in words a person can act on: a banner for a
    /// lasting state, a toast (once per half minute) for a passing one. The
    /// raw text stays one click away for bug reports.
    fn report_error(&self, raw: &str) {
        let imp = self.imp();
        let (message, lasting) = friendly_error(raw);
        imp.last_raw_error.replace(raw.to_string());
        if lasting {
            imp.banner_since.set(Some(std::time::Instant::now()));
            imp.error_banner.set_title(&message);
            imp.error_banner.set_button_label(Some("Details"));
            imp.error_banner.set_revealed(true);
            return;
        }
        let repeated = imp
            .last_error_toast
            .borrow()
            .as_ref()
            .is_some_and(|(shown, at)| {
                *shown == message && at.elapsed() < std::time::Duration::from_secs(30)
            });
        if repeated {
            return;
        }
        imp.last_error_toast
            .replace(Some((message.clone(), std::time::Instant::now())));
        let toast = adw::Toast::new(&message);
        toast.set_use_markup(false);
        toast.set_timeout(6);
        toast.set_button_label(Some("Details"));
        toast.set_action_name(Some("win.error-details"));
        toast.set_action_target_value(Some(&raw.to_variant()));
        imp.toast_overlay.add_toast(toast);
        self.send_app_notification("desklink-error", "DeskLink", &message);
    }

    fn show_error_details(&self, raw: &str) {
        if self.visible_dialog().is_some() {
            return;
        }
        let dialog = adw::AlertDialog::new(Some("Error Details"), Some(&friendly_error(raw).0));
        dialog.set_extra_child(Some(
            &gtk::Label::builder()
                .label(raw)
                .selectable(true)
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .css_classes(["monospace", "dim-label"])
                .build(),
        ));
        dialog.add_responses(&[("copy", "_Copy"), ("close", "_Close")]);
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        let (window, raw) = (self.downgrade(), raw.to_string());
        dialog.connect_response(Some("copy"), move |_, _| {
            if let Some(window) = window.upgrade() {
                gtk::prelude::WidgetExt::clipboard(&window).set_text(&raw);
                window.show_feature_toast("Copied");
            }
        });
        dialog.present(Some(self));
    }

    fn show_feature_toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_use_markup(false);
        toast.set_timeout(3);
        self.imp().toast_overlay.add_toast(toast);
    }

    /// "Pair New Device": a QR code to scan, and the phones found nearby.
    fn show_discovery_dialog(&self) {
        let Some(daemon) = self.imp().daemon.borrow().clone() else {
            return;
        };
        // Ctrl+N with a dialog already up would stack a second one on it.
        if self.visible_dialog().is_some() {
            return;
        }
        daemon.send(DaemonCommand::Discover);

        let dialog = adw::Dialog::builder()
            .title("Pair New Device")
            .content_width(460)
            .content_height(680)
            .build();
        let toolbar = adw::ToolbarView::new();
        let header = adw::HeaderBar::new();
        let refresh = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Search Again")
            .build();
        header.pack_start(&refresh);
        toolbar.add_top_bar(&header);

        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&qr_pairing_section(&daemon, &dialog));

        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let nearby = adw::PreferencesGroup::builder()
            .title("Nearby Devices")
            .description("Phones on this network with DeskLink open.")
            .header_suffix(
                &gtk::Spinner::builder()
                    .spinning(true)
                    .valign(gtk::Align::Center)
                    .build(),
            )
            .build();
        nearby.add(&list);
        content.append(&nearby);

        toolbar.set_content(Some(
            &gtk::ScrolledWindow::builder()
                .hscrollbar_policy(gtk::PolicyType::Never)
                .child(
                    &adw::Clamp::builder()
                        .maximum_size(400)
                        .child(&content)
                        .build(),
                )
                .build(),
        ));
        dialog.set_child(Some(&toolbar));

        refresh.connect_clicked({
            let daemon = daemon.clone();
            move |_| daemon.send(DaemonCommand::Discover)
        });

        let discovery_snapshot = std::rc::Rc::new(std::cell::RefCell::new(None));
        populate_discovery_list(&list, &daemon, &discovery_snapshot);
        let weak_list = list.downgrade();
        glib::timeout_add_seconds_local(1, move || {
            let Some(list) = weak_list.upgrade() else {
                return glib::ControlFlow::Break;
            };
            populate_discovery_list(&list, &daemon, &discovery_snapshot);
            glib::ControlFlow::Continue
        });

        dialog.present(Some(self));
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
    #[allow(unused_mut)]
    let mut devices: Vec<_> = daemon
        .devices()
        .into_iter()
        .filter(|device| !device.trusted)
        .collect();
    #[cfg(debug_assertions)]
    if let Some(window) = list.root().and_downcast::<DesklinkWindow>() {
        devices.extend(
            window
                .imp()
                .fake_devices
                .borrow()
                .iter()
                .filter(|device| !device.trusted)
                .cloned(),
        );
    }

    let snapshot = serde_json::to_string(&devices).unwrap_or_default();
    if last_snapshot.borrow().as_ref() == Some(&snapshot) {
        return;
    }
    last_snapshot.replace(Some(snapshot));

    list.remove_all();

    if devices.is_empty() {
        list.append(
            &adw::ActionRow::builder()
                .title("Looking for phones…")
                .subtitle("Keep both devices on the same Wi-Fi network.")
                .build(),
        );
        return;
    }

    for device in devices {
        list.append(&discovery_row(daemon, device));
    }
}

/// Clears the re-entrancy flag however `refresh_devices` exits.
struct RefreshGuard<'a>(&'a std::cell::Cell<bool>);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// "How to pair" under the welcome page's button: three numbered steps.
fn pairing_steps() -> adw::Clamp {
    let group = adw::PreferencesGroup::builder()
        .title("How to Pair")
        .build();
    for (number, icon, hue, title, subtitle) in [
        (
            1,
            "phone-symbolic",
            "blue",
            "Install DeskLink on your phone",
            "Get the DeskLink app for Android",
        ),
        (
            2,
            "network-wireless-symbolic",
            "green",
            "Open it on the same Wi-Fi",
            "Both devices need to be on one network",
        ),
        (
            3,
            "camera-photo-symbolic",
            "purple",
            "Scan the QR code",
            "Choose Pair a Phone… here, then scan with the app",
        ),
    ] {
        let row = adw::ActionRow::builder()
            .title(format!("{number}. {title}"))
            .subtitle(subtitle)
            .build();
        row.add_prefix(&badge(icon, hue, true));
        group.add(&row);
    }
    adw::Clamp::builder()
        .maximum_size(440)
        .child(&group)
        .build()
}

/// A daemon error in plain words, and whether it is a lasting state (shown
/// as a banner) rather than a passing failure (a toast). Unknown errors keep
/// their text, minus the "(os error N)" tail.
fn friendly_error(raw: &str) -> (String, bool) {
    let lower = raw.to_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| lower.contains(needle));
    let (message, lasting) = if has(&[
        "(os error 101)",
        "network is unreachable",
        "(os error 113)",
        "no route to host",
    ]) {
        (
            "No network connection. DeskLink will reconnect when you're back online.",
            true,
        )
    } else if has(&["(os error 98)", "address already in use"]) {
        (
            "Another app is using DeskLink's network ports. Close other phone-link apps, \
             then restart DeskLink.",
            true,
        )
    } else if has(&["(os error 111)", "connection refused"]) {
        (
            "The device refused the connection. Make sure DeskLink is open on it.",
            false,
        )
    } else if has(&["(os error 110)", "timed out"]) {
        (
            "The connection timed out. DeskLink will keep trying.",
            false,
        )
    } else if has(&[
        "(os error 104)",
        "connection reset",
        "(os error 32)",
        "broken pipe",
    ]) {
        (
            "The connection was interrupted. DeskLink will reconnect.",
            false,
        )
    } else if has(&["(os error 13)", "(os error 1)", "permission denied"]) {
        (
            "DeskLink was not allowed to do that. Check its permissions.",
            false,
        )
    } else {
        let trimmed = match raw.rfind(" (os error ") {
            Some(at) if raw.ends_with(')') => &raw[..at],
            _ => raw,
        };
        return (trimmed.to_string(), false);
    };
    (message.to_string(), lasting)
}

fn device_type_icon(device_type: &str) -> &'static str {
    match device_type.to_lowercase().as_str() {
        t if t.contains("phone") || t.contains("smartphone") => "phone-symbolic",
        t if t.contains("tablet") => "input-tablet-symbolic",
        t if t.contains("tv") || t.contains("television") => "tv-symbolic",
        _ => "computer-symbolic",
    }
}

/// "phone" → "Phone".
fn device_type_label(device_type: &str) -> String {
    let mut chars = device_type.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

fn device_avatar(device: &DeviceView, size: i32) -> adw::Avatar {
    let avatar = adw::Avatar::new(size, Some(&device.name), false);
    avatar.set_icon_name(Some(device_type_icon(&device.device_type)));
    avatar
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
        DeviceStatus::PairRequestedByPeer => ("Wants to pair", "warning"),
        DeviceStatus::PairRequested => ("Waiting for the phone", "dim-label"),
        other => (other.label(), "dim-label"),
    }
}

/// A small dot coloured like the status, always beside its text.
fn status_dot(class: &str) -> gtk::Box {
    let dot = gtk::Box::builder()
        .valign(gtk::Align::Center)
        .css_classes(["status-dot"])
        .build();
    match class {
        "success" => dot.add_css_class("connected"),
        "warning" => dot.add_css_class("connecting"),
        "error" => dot.add_css_class("offline"),
        _ => {}
    }
    dot
}

/// How the connection reaches the phone, for the pill on the device page.
fn connection_path_presentation(path: Option<&str>) -> Option<(&'static str, &'static str)> {
    let path = path?;
    let text = crate::device_links::device::connection_path_label(path)?;
    let icon = match path {
        "lan" => "network-wireless-symbolic",
        "direct" => "web-browser-symbolic",
        _ => "network-server-symbolic",
    };
    Some((icon, text))
}

/// The themed battery icon closest to `charge`.
fn battery_icon(charge: i64, charging: bool) -> String {
    let level = (charge.clamp(0, 100) + 5) / 10 * 10;
    match (level, charging) {
        (100, true) => "battery-level-100-charged-symbolic".to_string(),
        (_, true) => format!("battery-level-{level}-charging-symbolic"),
        (_, false) => format!("battery-level-{level}-symbolic"),
    }
}

/// "482913" → "482 913": two groups read aloud and compare more easily.
fn spaced_code(code: &str) -> String {
    let digits: Vec<char> = code.chars().collect();
    if digits.len() == 6 {
        format!(
            "{} {}",
            digits[..3].iter().collect::<String>(),
            digits[3..].iter().collect::<String>()
        )
    } else {
        code.to_string()
    }
}

fn pairing_code_label(code: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(spaced_code(code))
        .halign(gtk::Align::Center)
        .css_classes(["pairing-code", "numeric", "accent"])
        .build();
    label.update_property(&[gtk::accessible::Property::Label(&format!(
        "Verification code {}",
        code.chars()
            .map(|digit| digit.to_string())
            .collect::<Vec<_>>()
            .join(" ")
    ))]);
    label
}

fn chip(icon: Option<&str>, text: &str, class: Option<&str>) -> gtk::Box {
    let chip = gtk::Box::builder().spacing(6).css_classes(["chip"]).build();
    if let Some(class) = class {
        chip.add_css_class(class);
    }
    if let Some(icon) = icon {
        chip.append(&gtk::Image::from_icon_name(icon));
    }
    chip.append(&gtk::Label::new(Some(text)));
    chip
}

/// What a sidebar row shows; the row is redrawn only when this changes.
fn sidebar_key(device: &DeviceView) -> String {
    serde_json::to_string(&(
        &device.name,
        &device.device_type,
        device_status_presentation(device),
        device.notifications.len(),
        device
            .battery_status
            .as_ref()
            .map(|battery| battery.current_charge),
    ))
    .unwrap_or_default()
}

/// The sidebar row's accessible name: what the eye gets from the row.
fn sidebar_label(name: &str, status: &str, unread: usize) -> String {
    match unread {
        0 => format!("{name}, {status}"),
        1 => format!("{name}, {status}, 1 notification"),
        count => format!("{name}, {status}, {count} notifications"),
    }
}

/// Draws `device` into `row`, replacing the row's content but not the row,
/// so keyboard focus and selection stay where they are.
fn fill_sidebar_row(row: &gtk::ListBoxRow, device: &DeviceView) {
    let (status, class) = device_status_presentation(device);
    let content = gtk::Box::builder()
        .spacing(12)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    // The avatar carries the reachability as a ringed dot on its corner.
    let avatar = gtk::Overlay::builder()
        .child(&device_avatar(device, 40))
        .valign(gtk::Align::Center)
        .build();
    let dot = status_dot(class);
    dot.add_css_class("ring");
    dot.set_halign(gtk::Align::End);
    dot.set_valign(gtk::Align::End);
    avatar.add_overlay(&dot);
    content.append(&avatar);

    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(
        &gtk::Label::builder()
            .label(&device.name)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build(),
    );
    text.append(
        &gtk::Label::builder()
            .label(status)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    content.append(&text);

    if let Some(battery) = device
        .battery_status
        .as_ref()
        .filter(|battery| battery.current_charge >= 0)
    {
        content.append(
            &gtk::Label::builder()
                .label(format!("{}%", battery.current_charge))
                .valign(gtk::Align::Center)
                .tooltip_text("Battery")
                .css_classes(["caption", "dim-label", "numeric"])
                .build(),
        );
    }

    let unread = device.notifications.len();
    if unread > 0 {
        content.append(
            &gtk::Label::builder()
                .label(unread.to_string())
                .valign(gtk::Align::Center)
                .css_classes(["sidebar-badge", "numeric"])
                .tooltip_text(format!("{unread} notifications"))
                .build(),
        );
    }

    row.set_child(Some(&content));
    row.update_property(&[gtk::accessible::Property::Label(&sidebar_label(
        &device.name,
        status,
        unread,
    ))]);
}

/// Child indexes from `root` down to `widget`, to find "the same widget" in
/// a rebuilt copy of the tree.
fn widget_path(root: &gtk::Widget, widget: &gtk::Widget) -> Option<Vec<usize>> {
    let mut path = Vec::new();
    let mut current = widget.clone();
    while current != *root {
        let parent = current.parent()?;
        let mut index = 0;
        let mut sibling = parent.first_child();
        while let Some(child) = sibling {
            if child == current {
                break;
            }
            index += 1;
            sibling = child.next_sibling();
        }
        path.push(index);
        current = parent;
    }
    path.reverse();
    Some(path)
}

fn widget_at(root: &gtk::Widget, path: &[usize]) -> Option<gtk::Widget> {
    let mut current = root.clone();
    for &index in path {
        let mut child = current.first_child();
        for _ in 0..index {
            child = child?.next_sibling();
        }
        current = child?;
    }
    Some(current)
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

/// The device pane: a hero header, then what fits the device's state.
fn device_page(daemon: &DaemonHandle, device: &DeviceView, approving: bool) -> gtk::Widget {
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(24)
        .margin_bottom(36)
        .margin_start(12)
        .margin_end(12)
        .build();
    page.append(&device_hero(device));

    match &device.status {
        DeviceStatus::Paired => {
            if let Some(line) = crate::application::session_line(device) {
                page.append(&session_group(daemon, device, &line));
            }
            page.append(&quick_actions(daemon, device));
            if can_send_feature(device, PACKET_TYPE_NOTIFICATION_REQUEST) {
                page.append(&notifications_group(daemon, device));
            }
            if let Some(media) = media_group(daemon, device) {
                page.append(&media);
            }
            page.append(&sharing_group(device));
            page.append(&terminal_access_section(daemon, device));
            page.append(&remote_control_section(device, approving));
            page.append(&details_group(device));
            page.append(&unpair_group(daemon, device));
        }
        DeviceStatus::Unreachable => {
            page.append(&unreachable_card(daemon, device));
            page.append(&terminal_access_section(daemon, device));
            page.append(&remote_control_section(device, approving));
            page.append(&details_group(device));
            page.append(&unpair_group(daemon, device));
        }
        DeviceStatus::PairRequestedByPeer => page.append(&pair_request_card(daemon, device)),
        DeviceStatus::PairRequested => page.append(&pair_waiting_card(device)),
        DeviceStatus::Discovered | DeviceStatus::Connected => {
            let group = adw::PreferencesGroup::new();
            group.add(&discovery_row(daemon, device.clone()));
            page.append(&group);
        }
    }

    adw::Clamp::builder()
        .maximum_size(720)
        .tightening_threshold(560)
        .child(&page)
        .build()
        .upcast()
}

/// Large avatar, name, and pills for status, connection path and battery.
fn device_hero(device: &DeviceView) -> gtk::Box {
    let hero = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(6)
        .css_classes(["device-hero"])
        .build();
    let avatar = device_avatar(device, 96);
    avatar.set_halign(gtk::Align::Center);
    avatar.set_margin_bottom(6);
    hero.append(&avatar);
    hero.append(
        &gtk::Label::builder()
            .label(&device.name)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["title-1"])
            .build(),
    );
    let mut about = device_type_label(&device.device_type);
    if !device.address.is_empty() && matches!(device.status, DeviceStatus::Paired) {
        about.push_str(" · ");
        about.push_str(&device.address);
    }
    hero.append(
        &gtk::Label::builder()
            .label(about)
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build(),
    );

    // A FlowBox, so the pills wrap onto a second line in a narrow window.
    let chips = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(6)
        .row_spacing(6)
        .max_children_per_line(3)
        .halign(gtk::Align::Center)
        .margin_top(6)
        .build();
    let (status, class) = device_status_presentation(device);
    let status_chip = gtk::Box::builder().spacing(6).css_classes(["chip"]).build();
    if class != "dim-label" {
        status_chip.add_css_class(class);
    }
    status_chip.append(&status_dot(class));
    status_chip.append(&gtk::Label::new(Some(status)));
    chips.append(&status_chip);
    if matches!(device.status, DeviceStatus::Paired) {
        if let Some((icon, text)) = connection_path_presentation(device.connection_path.as_deref())
        {
            chips.append(&chip(Some(icon), text, None));
        }
    }
    if let Some(battery) = device
        .battery_status
        .as_ref()
        .filter(|battery| battery.current_charge >= 0)
    {
        let text = if battery.is_charging {
            format!("{}% · Charging", battery.current_charge)
        } else {
            format!("{}%", battery.current_charge)
        };
        let class = (battery.current_charge <= 15 && !battery.is_charging).then_some("error");
        chips.append(&chip(
            Some(&battery_icon(battery.current_charge, battery.is_charging)),
            &text,
            class,
        ));
    }
    // The pills are labels, not controls: no keyboard stops on their wrappers,
    // and a pill keeps its own width when a narrow window wraps the line.
    let mut child = chips.first_child();
    while let Some(wrapper) = child {
        wrapper.set_focusable(false);
        if let Some(pill) = wrapper.first_child() {
            pill.set_halign(gtk::Align::Center);
        }
        child = wrapper.next_sibling();
    }
    hero.append(&chips);

    if let Some(error) = &device.last_error {
        hero.append(
            &gtk::Label::builder()
                .label(friendly_error(error).0)
                .tooltip_text(error)
                .wrap(true)
                .justify(gtk::Justification::Center)
                .margin_top(6)
                .css_classes(["caption", "error"])
                .build(),
        );
    }
    hero
}

/// "Pixel 8 — controlling the screen, 2 terminals", with a live dot and a
/// way to stop it, in an accent-tinted card that stands out from the rows.
fn session_group(daemon: &DaemonHandle, device: &DeviceView, line: &str) -> gtk::Box {
    let card = gtk::Box::builder()
        .spacing(14)
        .css_classes(["session-card"])
        .build();
    card.append(&badge("screen-shared-symbolic", "accent", false));
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    let live = gtk::Box::builder().spacing(6).build();
    live.append(
        &gtk::Box::builder()
            .valign(gtk::Align::Center)
            .css_classes(["live-dot"])
            .build(),
    );
    live.append(
        &gtk::Label::builder()
            .label("ACTIVE SESSION")
            .xalign(0.0)
            .css_classes(["session-label"])
            .build(),
    );
    text.append(&live);
    text.append(
        &gtk::Label::builder()
            .label(line)
            .xalign(0.0)
            .wrap(true)
            .css_classes(["heading"])
            .build(),
    );
    card.append(&text);
    let end = gtk::Button::builder()
        .label("End Sessions")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    {
        let daemon = daemon.clone();
        let id = device.id.clone();
        end.connect_clicked(move |button| {
            button.set_sensitive(false);
            daemon.send(DaemonCommand::EndPhoneSessions(Some(id.clone())));
            toast_from(button, "Ending sessions…");
        });
    }
    card.append(&end);
    card.update_property(&[gtk::accessible::Property::Label(&format!(
        "Active session: {line}"
    ))]);
    card
}

/// Shows a toast in the main window that holds `widget`.
fn toast_from(widget: &impl IsA<gtk::Widget>, message: &str) {
    if let Some(window) = widget.root().and_downcast::<DesklinkWindow>() {
        window.show_feature_toast(message);
    }
}

/// The grid of rounded action tiles: only actions both apps can carry out.
fn quick_actions(daemon: &DaemonHandle, device: &DeviceView) -> gtk::FlowBox {
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .min_children_per_line(1)
        .max_children_per_line(3)
        .column_spacing(12)
        .row_spacing(12)
        .build();
    let add = |tile: gtk::Button| {
        grid.append(&tile);
        // Tab goes straight to the button, not to a wrapper around it.
        if let Some(child) = tile.parent() {
            child.set_focusable(false);
        }
    };
    let id = device.id.clone();
    let name = device.name.clone();

    if can_send_feature(device, PACKET_TYPE_MOUSEPAD_REQUEST) {
        let tile = action_tile(
            "Phone Screen",
            "See and control your phone",
            "blue",
            "screen-shared-symbolic",
            "View and control the phone's screen",
            {
                let (daemon, id) = (daemon.clone(), id.clone());
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
        );
        // The phone's screen needs the encrypted connection to be up.
        if !device.feature_transport_ready {
            tile.set_sensitive(false);
            tile.set_tooltip_text(Some("Available once the phone has finished connecting"));
        }
        add(tile);
    }
    if can_send_feature(device, PACKET_TYPE_SHARE_REQUEST) {
        add(action_tile(
            "Send Files",
            "Pick files to send",
            "green",
            "document-send-symbolic",
            "Send files to the phone",
            {
                let (daemon, id) = (daemon.clone(), id.clone());
                move |btn| {
                    let Some(root) = btn.root().and_downcast::<gtk::Window>() else {
                        return;
                    };
                    let file_dialog = gtk::FileDialog::builder()
                        .title("Select Files to Send")
                        .build();
                    let daemon = daemon.clone();
                    let id = id.clone();
                    file_dialog.open_multiple(Some(&root), gio::Cancellable::NONE, move |result| {
                        let Ok(files) = result else {
                            return;
                        };
                        for file in files.iter::<gio::File>().flatten() {
                            if let Some(path) = file.path() {
                                debug_log!("[UI] Selected file to send: {:?}", path);
                                daemon.send(DaemonCommand::SendFile(id.clone(), path));
                            }
                        }
                    });
                }
            },
        ));
    }
    if device.feature_transport_ready || can_send_feature(device, PACKET_TYPE_SFTP_REQUEST) {
        add(action_tile(
            "Browse Files",
            "Open your phone's storage",
            "yellow",
            "folder-remote-symbolic",
            "Browse, download and manage the folders the phone shares",
            {
                let (daemon, id) = (daemon.clone(), id.clone());
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
                        show_file_browsing_dialog(browsing_device.clone(), btn);
                    }
                }
            },
        ));
    }
    if can_send_feature(device, PACKET_TYPE_SHARE_REQUEST) {
        add(action_tile(
            "Share Text",
            "Send text or a link",
            "purple",
            "insert-text-symbolic",
            "Send text or a link to the phone",
            {
                let (daemon, id, name) = (daemon.clone(), id.clone(), name.clone());
                move |btn| show_share_text_dialog(daemon.clone(), id.clone(), &name, btn)
            },
        ));
    }
    if can_send_feature(device, PACKET_TYPE_FINDMYPHONE_REQUEST) {
        add(action_tile(
            "Ring Phone",
            "Find a misplaced phone",
            "orange",
            "find-location-symbolic",
            "Make the phone ring, even when it is silenced",
            {
                let (daemon, id, name) = (daemon.clone(), id.clone(), name.clone());
                move |btn| {
                    daemon.send(DaemonCommand::SendFindPhone(id.clone()));
                    toast_from(btn, &format!("Ringing {name}…"));
                }
            },
        ));
    }
    if can_send_feature(device, PACKET_TYPE_LOCK_REQUEST) {
        add(action_tile(
            "Lock Phone",
            "Lock it right away",
            "brown",
            "system-lock-screen-symbolic",
            "Lock the phone's screen",
            {
                let (daemon, id, name) = (daemon.clone(), id.clone(), name.clone());
                move |btn| {
                    daemon.send(DaemonCommand::SendLockRequest(id.clone(), true));
                    toast_from(btn, &format!("Locking {name}"));
                }
            },
        ));
    }
    if can_send_feature(device, PACKET_TYPE_NOTIFICATION_REQUEST) {
        add(action_tile(
            "Notifications",
            "Mirror and reply",
            "red",
            "preferences-system-notifications-symbolic",
            "See the phone's notifications",
            {
                let (daemon, id) = (daemon.clone(), id.clone());
                let device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestNotifications(id.clone()));
                    show_notifications_dialog(daemon.clone(), device.clone(), btn);
                }
            },
        ));
    }
    // A terminal started here is shared: it opens in a window on this
    // computer and appears in the phone's session list.
    add(action_tile(
        "Terminal",
        "Shared shell, Claude Code, Codex",
        "slate",
        "utilities-terminal-symbolic",
        "Open a shared terminal here that also appears on the phone",
        |btn| {
            if let Err(error) = crate::device_links::pty::desk::new_desk_session() {
                eprintln!("[UI] Could not start a shared terminal: {error}");
                toast_from(btn, "Could not start a shared terminal");
            }
        },
    ));
    if can_send_feature(device, PACKET_TYPE_PING) {
        add(action_tile(
            "Ping",
            "Check the connection",
            "teal",
            "mail-send-symbolic",
            "Send a ping to test the connection",
            {
                let (daemon, id, name) = (daemon.clone(), id.clone(), name.clone());
                move |btn| {
                    daemon.send(DaemonCommand::SendPing(id.clone()));
                    toast_from(btn, &format!("Ping sent to {name}"));
                }
            },
        ));
    }
    if can_send_feature(device, PACKET_TYPE_MPRIS_REQUEST) {
        add(action_tile(
            "Media",
            "Play, pause and skip",
            "accent",
            "media-playback-start-symbolic",
            "Control music and video",
            {
                let (daemon, id) = (daemon.clone(), id.clone());
                let media_device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestMprisStatus(id.clone(), None));
                    show_media_control_dialog(
                        daemon.clone(),
                        id.clone(),
                        media_device.clone(),
                        btn,
                    );
                }
            },
        ));
    }
    if can_send_feature(device, PACKET_TYPE_SYSTEMVOLUME_REQUEST) {
        add(action_tile(
            "Volume",
            "Phone volume",
            "accent",
            "audio-volume-high-symbolic",
            "View and control the phone's volume",
            {
                let (daemon, id) = (daemon.clone(), id.clone());
                let device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestSystemVolume(id.clone()));
                    show_volume_dialog(daemon.clone(), device.clone(), btn);
                }
            },
        ));
    }
    if can_send_feature(device, PACKET_TYPE_RUNCOMMAND_REQUEST) {
        add(action_tile(
            "Commands",
            "Run the phone's commands",
            "slate",
            "system-run-symbolic",
            "Run commands the phone offers",
            {
                let (daemon, id) = (daemon.clone(), id.clone());
                let device = device.clone();
                move |btn| {
                    daemon.send(DaemonCommand::RequestRemoteCommands(id.clone()));
                    show_remote_commands_dialog(daemon.clone(), device.clone(), btn);
                }
            },
        ));
    }
    grid
}

/// A tinted rounded square behind a symbolic icon; `hue` is one of the
/// badge colours in `style.css` (blue, green, yellow, orange, red, purple,
/// brown, teal, slate, accent).
pub(crate) fn badge(icon: &str, hue: &str, small: bool) -> gtk::Box {
    let badge = gtk::Box::builder()
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes(["badge", hue])
        .build();
    // Set, not just unset: the centred icon expands inside the badge, and
    // that must not make the badge itself take a row's spare width.
    badge.set_hexpand(false);
    badge.set_vexpand(false);
    if small {
        badge.add_css_class("small");
    }
    badge.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .pixel_size(if small { 16 } else { 22 })
            .hexpand(true)
            .vexpand(true)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .build(),
    );
    badge
}

/// A card button: a coloured icon badge, a bold title and a one-line hint.
fn action_tile(
    label: &str,
    hint: &str,
    hue: &str,
    icon: &str,
    tooltip: &str,
    action: impl Fn(&gtk::Button) + 'static,
) -> gtk::Button {
    let inner = gtk::Box::builder().spacing(14).build();
    inner.append(&badge(icon, hue, false));
    let text = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .hexpand(true)
        .valign(gtk::Align::Center)
        .build();
    text.append(
        &gtk::Label::builder()
            .label(label)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(12)
            .css_classes(["heading"])
            .build(),
    );
    // A short natural width lets the grid fit three cards to a line; a long
    // hint wraps onto a second line rather than being cut off.
    text.append(
        &gtk::Label::builder()
            .label(hint)
            .xalign(0.0)
            .wrap(true)
            .lines(2)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(12)
            .css_classes(["hint", "dim-label"])
            .build(),
    );
    inner.append(&text);
    let button = gtk::Button::builder()
        .child(&inner)
        .tooltip_text(tooltip)
        .css_classes(["card", "action-card"])
        .build();
    button.update_property(&[
        gtk::accessible::Property::Label(label),
        gtk::accessible::Property::Description(tooltip),
    ]);
    button.connect_clicked(move |b| action(b));
    button
}

/// The newest notifications inline, with a way to see them all.
fn notifications_group(daemon: &DaemonHandle, device: &DeviceView) -> adw::PreferencesGroup {
    const SHOWN: usize = 3;
    let group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .build();
    if device.notifications.len() > SHOWN {
        let all = gtk::Button::builder()
            .label(format!("Show All ({})", device.notifications.len()))
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .build();
        let (daemon, device) = (daemon.clone(), device.clone());
        all.connect_clicked(move |button| {
            show_notifications_dialog(daemon.clone(), device.clone(), button)
        });
        group.set_header_suffix(Some(&all));
    }
    if device.notifications.is_empty() {
        let row = adw::ActionRow::builder()
            .title("No notifications")
            .subtitle("Notifications from the phone appear here")
            .build();
        row.add_prefix(&badge(
            "preferences-system-notifications-symbolic",
            "red",
            true,
        ));
        group.add(&row);
    }
    for notification in device.notifications.iter().take(SHOWN) {
        group.add(&notification_row(daemon, &device.id, notification));
    }
    group
}

fn media_group(daemon: &DaemonHandle, device: &DeviceView) -> Option<adw::PreferencesGroup> {
    let media = device.media_status.as_ref()?;
    let group = adw::PreferencesGroup::builder()
        .title("Now Playing")
        .build();
    let details = [
        media.artist.as_str(),
        media.album.as_str(),
        media.player.as_str(),
    ]
    .into_iter()
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(" · ");
    let row = adw::ActionRow::builder()
        .title(if media.title.is_empty() {
            "Media session"
        } else {
            &media.title
        })
        .subtitle(if details.is_empty() {
            "Remote media status"
        } else {
            &details
        })
        .use_markup(false)
        .build();
    row.add_prefix(&badge("audio-x-generic-symbolic", "purple", true));
    row.add_suffix(
        &gtk::Image::builder()
            .icon_name(if media.is_playing {
                "media-playback-start-symbolic"
            } else {
                "media-playback-pause-symbolic"
            })
            .tooltip_text(if media.is_playing {
                "Playing"
            } else {
                "Paused"
            })
            .build(),
    );
    if can_send_feature(device, PACKET_TYPE_MPRIS_REQUEST) {
        row.set_activatable(true);
        let (daemon, device) = (daemon.clone(), device.clone());
        row.connect_activated(move |row| {
            daemon.send(DaemonCommand::RequestMprisStatus(device.id.clone(), None));
            show_media_control_dialog(daemon.clone(), device.id.clone(), device.clone(), row);
        });
    }
    group.add(&row);
    Some(group)
}

/// Where received files go, and clipboard sync.
fn sharing_group(device: &DeviceView) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Sharing").build();
    let downloads = adw::ActionRow::builder()
        .title("Received Files")
        .subtitle("Saved to your Downloads folder")
        .build();
    downloads.add_prefix(&badge("folder-download-symbolic", "green", true));
    let open = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .tooltip_text("Open Downloads")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    open.connect_clicked(|button| {
        let Some(dir) = glib::user_special_dir(glib::UserDirectory::Downloads) else {
            return;
        };
        gtk::FileLauncher::new(Some(&gio::File::for_path(dir))).launch(
            button.root().and_downcast_ref::<gtk::Window>(),
            gio::Cancellable::NONE,
            |result| {
                if let Err(error) = result {
                    eprintln!("[UI] Could not open Downloads: {error}");
                }
            },
        );
    });
    downloads.add_suffix(&open);
    group.add(&downloads);
    if can_send_feature(device, PACKET_TYPE_CLIPBOARD) {
        let clipboard = adw::ActionRow::builder()
            .title("Clipboard Sync")
            .subtitle("Copied text is shared automatically while connected")
            .build();
        clipboard.add_prefix(&badge("edit-paste-symbolic", "blue", true));
        clipboard.add_suffix(
            &gtk::Image::builder()
                .icon_name("object-select-symbolic")
                .tooltip_text("On")
                .css_classes(["success"])
                .build(),
        );
        group.add(&clipboard);
    }
    group
}

/// The consent switch for phone-driven terminals, and — while the phone is
/// using them — a visible indicator.
///
/// A terminal is a shell on this computer, so it is **off until the user turns
/// it on here**, per phone, and turning it off cuts the phone off at once. The
/// indicator is text with an icon, not colour alone, and both rows have
/// accessible names and descriptions.
fn terminal_access_section(daemon: &DaemonHandle, device: &DeviceView) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Terminal").build();

    const WARNING: &str = "Lets this phone run commands on this computer";
    // `active` is set before the handler is connected, so building the row
    // from persisted state does not itself send a command.
    let switch = adw::SwitchRow::builder()
        .title("Allow Terminal Access")
        .subtitle(WARNING)
        .active(device.terminal_access)
        .build();
    switch.add_prefix(&badge("utilities-terminal-symbolic", "slate", true));
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
    group.add(&switch);

    // A terminal started here is shared: it opens in a window on this
    // computer and appears in the phone's session list.
    let shared = adw::ActionRow::builder()
        .title("New Shared Terminal")
        .subtitle("Opens here and appears on this phone, live in both")
        .activatable(true)
        .build();
    shared.add_prefix(&badge("list-add-symbolic", "accent", true));
    shared.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    shared.connect_activated(|_| {
        if let Err(error) = crate::device_links::pty::desk::new_desk_session() {
            eprintln!("[UI] Could not start a shared terminal: {error}");
        }
    });
    group.add(&shared);

    if device.terminal_sessions_attached > 0 {
        let count = device.terminal_sessions_attached;
        let plural = if count == 1 { "session" } else { "sessions" };
        let indicator = adw::ActionRow::builder()
            .title(format!("Phone has terminal access ({count} {plural})"))
            .subtitle("This phone is connected to a terminal on this computer")
            .activatable(false)
            .build();
        indicator.add_prefix(&badge("dialog-warning-symbolic", "yellow", true));
        group.add(&indicator);
    }
    group
}

/// Approve, while at the desk, the phone controlling this computer — so later
/// sessions from the phone start without anyone here to click Share.
///
/// Drawn from state only (the remembered grant and whether an approval is
/// under way), because the page is rebuilt whenever the device changes and a
/// label set by hand would be lost.
fn remote_control_section(device: &DeviceView, approving: bool) -> adw::PreferencesGroup {
    const APPROVED: &str =
        "Approved: this phone can view and control this computer without asking here, until DeskLink restarts";
    const NOT_APPROVED: &str =
        "Each session from this phone asks here first. Approve now so it can connect on its own";
    const WAITING: &str =
        "Choose the screen, turn on \u{201c}Allow Remote Interaction\u{201d}, then Share";
    const VIEW_ONLY: &str =
        "Shared without remote interaction, so not remembered. Try again with \u{201c}Allow Remote Interaction\u{201d} on";

    let group = adw::PreferencesGroup::builder()
        .title("Remote Control")
        .build();
    let approved = crate::device_links::config::remote_desktop_token(&device.id).is_some();
    let row = adw::ActionRow::builder()
        .title("Control This Computer From the Phone")
        .subtitle(match (approving, approved) {
            (true, _) => WAITING,
            (false, true) => APPROVED,
            (false, false) => NOT_APPROVED,
        })
        .subtitle_lines(3)
        .build();
    row.add_prefix(&badge("input-mouse-symbolic", "blue", true));
    let approve = gtk::Button::builder()
        .label(match (approving, approved) {
            (true, _) => "Choose the Screen…",
            (false, true) => "Approve Again",
            (false, false) => "Approve…",
        })
        .sensitive(!approving)
        .valign(gtk::Align::Center)
        .css_classes(["suggested-action"])
        .build();
    let forget = gtk::Button::builder()
        .label("Forget")
        .valign(gtk::Align::Center)
        .visible(approved && !approving)
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
        let id = device.id.clone();
        approve.connect_clicked(move |button| {
            let Some(window) = button.root().and_downcast::<DesklinkWindow>() else {
                return;
            };
            window.imp().approving.borrow_mut().insert(id.clone());
            window.refresh_soon();
            let (window, id) = (window.downgrade(), id.clone());
            glib::spawn_future_local(async move {
                let worker_id = id.clone();
                let outcome = gio::spawn_blocking(move || {
                    crate::device_links::webrtc::preauthorize_remote_control(&worker_id)
                })
                .await
                .unwrap_or_else(|_| Err("The approval was interrupted".to_string()));
                let Some(window) = window.upgrade() else {
                    return;
                };
                window.imp().approving.borrow_mut().remove(&id);
                match &outcome {
                    Ok(true) => {
                        window.show_feature_toast("Approved: this phone can now connect on its own")
                    }
                    Ok(false) => window.show_feature_toast(VIEW_ONLY),
                    Err(error) => {
                        eprintln!("[UI] Remote-control approval did not complete: {error}");
                        window.show_feature_toast("The approval did not complete");
                    }
                }
                window.refresh_soon();
            });
        });
    }
    {
        let id = device.id.clone();
        forget.connect_clicked(move |button| {
            crate::device_links::config::set_remote_desktop_token(&id, None);
            if let Some(window) = button.root().and_downcast::<DesklinkWindow>() {
                window.refresh_soon();
            }
        });
    }
    group.add(&row);
    group
}

fn details_group(device: &DeviceView) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Details").build();
    let connection = match device.status {
        DeviceStatus::Paired => connection_path_presentation(device.connection_path.as_deref())
            .map_or("Connecting…", |(_, text)| text),
        _ => "Not connected",
    };
    let address = if device.address.is_empty() {
        "Unknown".to_string()
    } else {
        device.address.clone()
    };
    for (title, value) in [
        ("Type", device_type_label(&device.device_type)),
        ("Connection", connection.to_string()),
        ("Address", address),
        ("Protocol", format!("DeskLink v{}", device.protocol_version)),
    ] {
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(value)
            .subtitle_selectable(true)
            .use_markup(false)
            .css_classes(["property"])
            .build();
        group.add(&row);
    }
    group
}

/// Unpairing, behind a confirmation, at the bottom of the page.
fn unpair_group(daemon: &DaemonHandle, device: &DeviceView) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::new();
    let unpair = adw::ActionRow::builder()
        .title("Unpair Device")
        .subtitle("Forget this device; it must be paired again to connect")
        .activatable(true)
        .build();
    unpair.add_prefix(&badge("user-trash-symbolic", "red", true));
    let button = gtk::Button::builder()
        .label("Unpair…")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    unpair.add_suffix(&button);
    unpair.set_activatable_widget(Some(&button));
    let (daemon, id, name) = (daemon.clone(), device.id.clone(), device.name.clone());
    button.connect_clicked(move |button| {
        let dialog = adw::AlertDialog::new(
            Some(&format!("Unpair {name}?")),
            Some("It will no longer be able to connect to this computer until you pair it again."),
        );
        dialog.add_responses(&[("cancel", "_Cancel"), ("unpair", "_Unpair")]);
        dialog.set_response_appearance("unpair", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let (daemon, id) = (daemon.clone(), id.clone());
        dialog.choose(button, gio::Cancellable::NONE, move |response| {
            if response == "unpair" {
                debug_log!("[UI] Unpair clicked for {}", id);
                daemon.send(DaemonCommand::Unpair(id.clone()));
            }
        });
    });
    group.add(&unpair);
    group
}

/// A paired phone that cannot be reached right now.
fn unreachable_card(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let card = card_box();
    card.append(
        &gtk::Image::builder()
            .icon_name("network-offline-symbolic")
            .pixel_size(48)
            .css_classes(["dim-label"])
            .build(),
    );
    card.append(
        &gtk::Label::builder()
            .label("Not Connected")
            .css_classes(["title-2"])
            .build(),
    );
    card.append(
        &gtk::Label::builder()
            .label(format!(
                "Open DeskLink on {} and keep both devices on the same network, or turn on \
                 Away From Home in Preferences.",
                device.name
            ))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build(),
    );
    let search = gtk::Button::builder()
        .label("Search Again")
        .halign(gtk::Align::Center)
        .margin_top(6)
        .css_classes(["pill"])
        .build();
    let daemon = daemon.clone();
    search.connect_clicked(move |button| {
        daemon.send(DaemonCommand::Discover);
        toast_from(button, "Searching the network…");
    });
    card.append(&search);
    card
}

/// The phone asked to pair: the code to compare, and Accept / Reject.
fn pair_request_card(daemon: &DaemonHandle, device: &DeviceView) -> gtk::Box {
    let card = card_box();
    card.append(
        &gtk::Label::builder()
            .label("Pairing Request")
            .css_classes(["title-2"])
            .build(),
    );
    card.append(
        &gtk::Label::builder()
            .label(format!(
                "{} wants to pair with this computer. Accept only if this code matches the one on the phone.",
                device.name
            ))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build(),
    );
    if let Some(key) = &device.verification_key {
        card.append(&pairing_code_label(key));
    }
    let buttons = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
        .homogeneous(true)
        .margin_top(6)
        .build();
    let reject = gtk::Button::builder()
        .label("Reject")
        .css_classes(["pill"])
        .build();
    let accept = gtk::Button::builder()
        .label("Accept")
        .css_classes(["pill", "suggested-action"])
        .build();
    {
        let daemon = daemon.clone();
        let id = device.id.clone();
        reject.connect_clicked(move |_| {
            eprintln!("[UI] Reject clicked for {}", id);
            daemon.send(DaemonCommand::RejectPair(id.clone()));
        });
    }
    {
        let daemon = daemon.clone();
        let id = device.id.clone();
        let shown_key = device.verification_key.clone();
        accept.connect_clicked(move |_| {
            debug_log!("[UI] Accept clicked for {}", id);
            daemon.send(DaemonCommand::AcceptPair(id.clone(), shown_key.clone()));
        });
    }
    buttons.append(&reject);
    buttons.append(&accept);
    card.append(&buttons);
    card
}

fn pair_waiting_card(device: &DeviceView) -> gtk::Box {
    let card = card_box();
    card.append(
        &gtk::Spinner::builder()
            .spinning(true)
            .width_request(32)
            .height_request(32)
            .build(),
    );
    card.append(
        &gtk::Label::builder()
            .label(format!("Waiting for {}", device.name))
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["title-2"])
            .build(),
    );
    card.append(
        &gtk::Label::builder()
            .label(if device.verification_key.is_some() {
                "Accept the request on the phone. It should show this code:"
            } else {
                "Accept the request on the phone."
            })
            .wrap(true)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build(),
    );
    if let Some(key) = &device.verification_key {
        card.append(&pairing_code_label(key));
    }
    card
}

fn card_box() -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .css_classes(["card", "padded-card"])
        .build()
}

/// A titled dialog over the main window, its content scrolling inside a clamp.
fn present_sheet(
    parent: &impl IsA<gtk::Widget>,
    title: &str,
    width: i32,
    content: &impl IsA<gtk::Widget>,
) -> adw::Dialog {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    let clamp = adw::Clamp::builder()
        .child(content)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    toolbar.set_content(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&clamp)
            .build(),
    ));
    let dialog = adw::Dialog::builder()
        .title(title)
        .content_width(width)
        .child(&toolbar)
        .build();
    dialog.present(Some(parent));
    dialog
}

fn empty_state(icon: &str, title: &str, description: &str) -> adw::StatusPage {
    adw::StatusPage::builder()
        .icon_name(icon)
        .title(title)
        .description(glib::markup_escape_text(description))
        .css_classes(["compact"])
        .build()
}

fn boxed_list() -> gtk::ListBox {
    gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build()
}

fn show_share_text_dialog(
    daemon: DaemonHandle,
    device_id: String,
    device_name: &str,
    parent: &impl IsA<gtk::Widget>,
) {
    let entry = adw::EntryRow::builder()
        .title("Text or link")
        .activates_default(true)
        .build();
    let list = boxed_list();
    list.append(&entry);

    let dialog = adw::AlertDialog::new(
        Some("Share Text"),
        Some(&format!(
            "Send text or a link to {device_name}. Links open on the phone."
        )),
    );
    dialog.add_responses(&[("cancel", "_Cancel"), ("send", "_Send")]);
    dialog.set_response_appearance("send", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("send"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("send", false);
    dialog.set_extra_child(Some(&list));
    {
        let dialog = dialog.downgrade();
        entry.connect_changed(move |entry| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.set_response_enabled("send", !entry.text().trim().is_empty());
            }
        });
    }
    let toast_target = parent.as_ref().clone();
    dialog.choose(parent, gio::Cancellable::NONE, move |response| {
        let text = entry.text().trim().to_string();
        if response == "send" && !text.is_empty() {
            daemon.send(DaemonCommand::SendShareText(device_id.clone(), text));
            toast_from(&toast_target, "Text sent");
        }
    });
}

fn show_media_control_dialog(
    daemon: DaemonHandle,
    device_id: String,
    device: DeviceView,
    parent: &impl IsA<gtk::Widget>,
) {
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .build();

    if let Some(media) = &device.media_status {
        let now = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .build();
        now.append(
            &gtk::Label::builder()
                .label(if media.title.is_empty() {
                    "Nothing playing"
                } else {
                    &media.title
                })
                .wrap(true)
                .justify(gtk::Justification::Center)
                .css_classes(["title-3"])
                .build(),
        );
        if !media.artist.is_empty() {
            now.append(
                &gtk::Label::builder()
                    .label(&media.artist)
                    .css_classes(["dim-label"])
                    .build(),
            );
        }
        content.append(&now);
    }

    let player_entry = adw::EntryRow::builder().title("Player (optional)").build();
    if let Some(media) = &device.media_status {
        if !media.player.is_empty() {
            player_entry.set_text(&media.player);
        } else if let Some(player) = media.player_list.first() {
            player_entry.set_text(player);
        }
    }

    let controls = gtk::Box::builder()
        .spacing(12)
        .halign(gtk::Align::Center)
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
            .width_request(48)
            .height_request(48)
            .css_classes(["circular"])
            .build();
        if action == "PlayPause" {
            button.add_css_class("suggested-action");
        }
        button.update_property(&[gtk::accessible::Property::Label(label)]);
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
        .spacing(6)
        .halign(gtk::Align::Center)
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
            .css_classes(["flat", "circular"])
            .build();
        button.update_property(&[gtk::accessible::Property::Label(label)]);
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

    let list = boxed_list();
    list.append(&player_entry);
    content.append(&list);

    present_sheet(parent, "Media", 400, &content);
}

fn show_notifications_dialog(
    daemon: DaemonHandle,
    device: DeviceView,
    parent: &impl IsA<gtk::Widget>,
) {
    let content: gtk::Widget = if device.notifications.is_empty() {
        empty_state(
            "preferences-system-notifications-symbolic",
            "No Notifications",
            &format!("Notifications from {} appear here.", device.name),
        )
        .upcast()
    } else {
        let list = boxed_list();
        for notification in &device.notifications {
            list.append(&notification_row(&daemon, &device.id, notification));
        }
        list.upcast()
    };
    let dialog = present_sheet(parent, "Notifications", 560, &content);
    dialog.set_content_height(560);
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
        .subtitle_lines(2)
        .build();
    row.set_use_markup(false);
    row.set_activatable(false);
    let avatar = adw::Avatar::new(32, Some(&notification.app_name), true);
    avatar.set_tooltip_text(Some(&notification.app_name));
    row.add_prefix(&avatar);

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
        let button = gtk::Button::builder()
            .label(action)
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        let daemon = daemon.clone();
        let device_id = device_id.to_string();
        let key = notification.id.clone();
        let action = action.clone();
        button.connect_clicked(move |_| {
            daemon.send(DaemonCommand::TriggerNotificationAction(
                device_id.clone(),
                key.clone(),
                action.clone(),
            ));
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

fn show_volume_dialog(daemon: DaemonHandle, device: DeviceView, parent: &impl IsA<gtk::Widget>) {
    let sinks = device
        .volume_status
        .as_ref()
        .map(|status| status.sinks.as_slice())
        .unwrap_or(&[]);
    let content: gtk::Widget = if sinks.is_empty() {
        empty_state(
            "audio-volume-high-symbolic",
            "No Audio Devices",
            "The phone has not reported any audio devices yet.",
        )
        .upcast()
    } else {
        let list = boxed_list();
        for sink in sinks {
            list.append(&volume_sink_row(&daemon, &device.id, sink));
        }
        list.upcast()
    };
    present_sheet(parent, "Volume", 520, &content);
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

fn show_remote_commands_dialog(
    daemon: DaemonHandle,
    device: DeviceView,
    parent: &impl IsA<gtk::Widget>,
) {
    let content: gtk::Widget = if device.available_commands.is_empty() {
        empty_state(
            "system-run-symbolic",
            "No Commands",
            "The phone has not offered any commands yet.",
        )
        .upcast()
    } else {
        let list = boxed_list();
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
            let run = command_button("Run", "media-playback-start-symbolic", {
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
        list.upcast()
    };
    present_sheet(parent, "Commands", 520, &content);
}

fn show_file_browsing_dialog(device: DeviceView, parent: &impl IsA<gtk::Widget>) {
    let content: gtk::Widget = match &device.sftp_status {
        Some(sftp) if sftp.error.is_some() => empty_state(
            "dialog-warning-symbolic",
            "Could Not Browse Files",
            sftp.error.as_deref().unwrap_or_default(),
        )
        .upcast(),
        Some(sftp) => {
            let list = boxed_list();
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
                    .use_markup(false)
                    .build(),
            );
            for (path, name) in &sftp.directories {
                let row = adw::ActionRow::builder()
                    .title(name)
                    .subtitle(path)
                    .use_markup(false)
                    .build();
                row.add_prefix(&gtk::Image::from_icon_name("folder-symbolic"));
                list.append(&row);
            }
            list.upcast()
        }
        None => empty_state(
            "folder-remote-symbolic",
            "Waiting for the Phone",
            "File browsing was requested. Details appear when the phone replies.",
        )
        .upcast(),
    };
    present_sheet(parent, "Browse Files", 520, &content);
}

/// A device in the "Pair New Device" list: discovered, or mid-pairing.
fn discovery_row(daemon: &DaemonHandle, device: DeviceView) -> adw::ActionRow {
    let mut subtitle_parts: Vec<String> = vec![device_type_label(&device.device_type)];
    if !matches!(device.status, DeviceStatus::Discovered) {
        subtitle_parts.push(device_status_presentation(&device).0.to_string());
    }
    if let Some((_, path)) = connection_path_presentation(device.connection_path.as_deref()) {
        subtitle_parts.push(path.to_string());
    }
    if let Some(error) = &device.last_error {
        subtitle_parts.push(friendly_error(error).0);
    }
    let row = adw::ActionRow::builder()
        .title(&device.name)
        .subtitle(subtitle_parts.join(" · "))
        .build();
    row.set_use_markup(false);
    row.set_activatable(false);
    row.add_prefix(&device_avatar(&device, 32));

    match &device.status {
        DeviceStatus::Discovered | DeviceStatus::Connected | DeviceStatus::Unreachable => {
            let pair = gtk::Button::builder()
                .label("Pair")
                .valign(gtk::Align::Center)
                .css_classes(["suggested-action"])
                .build();
            let daemon = daemon.clone();
            let id = device.id.clone();
            pair.connect_clicked(move |_| daemon.send(DaemonCommand::RequestPair(id.clone())));
            row.add_suffix(&pair);
        }
        DeviceStatus::PairRequested => {
            if let Some(key) = &device.verification_key {
                row.add_suffix(
                    &gtk::Label::builder()
                        .label(spaced_code(key))
                        .valign(gtk::Align::Center)
                        .css_classes(["heading", "numeric", "accent"])
                        .build(),
                );
            }
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
                        .label(spaced_code(key))
                        .valign(gtk::Align::Center)
                        .css_classes(["heading", "numeric", "accent"])
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
        DeviceStatus::Paired => {}
    }

    row
}

/// A round icon button whose tooltip is also its accessible name.
fn command_button(label: &str, icon_name: &str, action: impl Fn() + 'static) -> gtk::Button {
    let button = gtk::Button::builder()
        .icon_name(icon_name)
        .tooltip_text(label)
        .valign(gtk::Align::Center)
        .css_classes(["circular", "flat"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button.connect_clicked(move |_| action());
    button
}

/// "Scan to pair": shows a live QR code from `pairing_qr`. A scanned code is
/// consumed, so a fresh one appears for the next phone; closing the dialog
/// withdraws it so no code outlives the dialog that showed it.
fn qr_pairing_section(daemon: &DaemonHandle, dialog: &adw::Dialog) -> gtk::Box {
    let section = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .halign(gtk::Align::Center)
        .build();
    let picture = gtk::Picture::builder()
        .width_request(196)
        .height_request(196)
        .can_shrink(true)
        .halign(gtk::Align::Center)
        .css_classes(["qr-frame"])
        .build();
    gtk::prelude::AccessibleExtManual::update_property(
        &picture,
        &[gtk::accessible::Property::Label("Pairing QR code")],
    );
    section.append(&picture);
    section.append(
        &gtk::Label::builder()
            .label("Scan to Pair")
            .margin_top(6)
            .css_classes(["title-3"])
            .build(),
    );
    section.append(
        &gtk::Label::builder()
            .label("Open DeskLink on your phone and scan this code. There is nothing to compare.")
            .wrap(true)
            .max_width_chars(36)
            .justify(gtk::Justification::Center)
            .css_classes(["dim-label"])
            .build(),
    );

    daemon.send(DaemonCommand::StartQrPairing);
    let shown = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    // Set on close, so a tick racing the close can never start a code that no
    // dialog shows (it would stay live for 5 minutes).
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
    let daemon = daemon.clone();
    dialog.connect_closed(move |_| {
        closed.set(true);
        daemon.send(DaemonCommand::CancelQrPairing);
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

/// Debug builds only: the `e2e` verbs that drive the UI itself, for
/// screenshots without a phone. Returns whether `verb` was one of them.
///
/// - `screenshot PATH`: the main window to PATH, every other visible window
///   beside it as `<stem>-<title>.png`.
/// - `fake-device KIND NAME`: shows a made-up device (`paired`, `unreachable`,
///   `pair-request`, `requested`, `discovered`); `fake-clear` removes them.
/// - `window-size W H`, `color-scheme dark|light|default`, `open-pair`,
///   `nav content|sidebar`, `open-files ID`, `fake-transfer NAME DONE/TOTAL`,
///   `open-transfers`
///   (a leading `-` on DONE makes it incoming; TOTAL 0 is still preparing).
#[cfg(debug_assertions)]
fn debug_ui_verb(win: &DesklinkWindow, verb: &str, id: &str, arg: &str) -> bool {
    match verb {
        "screenshot" => debug_screenshot(win, id),
        "fake-device" => {
            win.imp()
                .fake_devices
                .borrow_mut()
                .push(debug_fake_device(id, arg));
            win.imp().detail_snapshot.replace(None);
            win.refresh_devices();
        }
        "fake-clear" => {
            win.imp().fake_devices.borrow_mut().clear();
            win.imp().detail_snapshot.replace(None);
            win.refresh_devices();
        }
        "window-size" => {
            if let (Ok(width), Ok(height)) = (id.parse(), arg.parse()) {
                win.unmaximize();
                win.set_default_size(width, height);
            }
        }
        "color-scheme" => adw::StyleManager::default().set_color_scheme(match id {
            "dark" => adw::ColorScheme::ForceDark,
            "light" => adw::ColorScheme::ForceLight,
            _ => adw::ColorScheme::Default,
        }),
        "open-pair" => win.show_discovery_dialog(),
        "open-files" => crate::phone_files::show(
            win.imp().daemon.borrow().clone().expect("daemon is set"),
            id.to_string(),
            "Pixel 8",
            win.upcast_ref(),
        ),
        // "fake-transfer NAME DONE/TOTAL": a transfer row, then its window.
        "fake-transfer" => {
            let (done, total) = arg.split_once('/').unwrap_or(("0", "0"));
            crate::transfers::FAKE_ROWS.with(|rows| {
                rows.borrow_mut()
                    .push(crate::device_links::webrtc::transfer_board::TransferRow {
                        id: format!("fake-{}", glib::monotonic_time()),
                        device_id: String::new(),
                        name: id.to_string(),
                        incoming: done.starts_with('-'),
                        done: done.trim_start_matches('-').parse().unwrap_or(0),
                        total: total.parse().unwrap_or(0),
                        preparing: total == "0",
                    })
            });
        }
        "open-transfers" => crate::transfers::show(
            win.imp().daemon.borrow().clone().expect("daemon is set"),
            win.upcast_ref(),
            || {},
        ),
        // Focus checks: put focus on the page's first switch, change the
        // fake devices so the page is rebuilt, and report where focus is.
        "focus-switch" => {
            let mut stack = vec![win.imp().detail_body.get().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if widget.is::<adw::SwitchRow>() {
                    widget.grab_focus();
                    break;
                }
                let mut child = widget.last_child();
                while let Some(next) = child {
                    child = next.prev_sibling();
                    stack.push(next);
                }
            }
        }
        "fake-bump" => {
            for device in win.imp().fake_devices.borrow_mut().iter_mut() {
                if let Some(battery) = device.battery_status.as_mut() {
                    battery.current_charge -= 1;
                }
                device.notifications.truncate(1);
            }
            win.refresh_devices();
        }
        "focus-info" => {
            let focus = gtk::prelude::RootExt::focus(win);
            debug_log!(
                "[E2E] focus: {:?} {:?}",
                focus.as_ref().map(|widget| widget.type_().name()),
                focus.as_ref().map(|widget| widget.widget_name())
            );
        }
        // "nav content|sidebar": which pane a collapsed window shows.
        "nav" => win.imp().split_view.set_show_content(id == "content"),
        _ => return false,
    }
    true
}

#[cfg(debug_assertions)]
fn debug_screenshot(main: &DesklinkWindow, path: &str) {
    let base = std::path::Path::new(path);
    let stem = base
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    for window in gtk::Window::list_toplevels() {
        let Ok(window) = window.downcast::<gtk::Window>() else {
            continue;
        };
        if !window.is_visible() {
            continue;
        }
        let target = if window == *main.upcast_ref::<gtk::Window>() {
            base.to_path_buf()
        } else {
            let title: String = window
                .title()
                .unwrap_or_default()
                .chars()
                .map(|c| if c.is_alphanumeric() { c } else { '-' })
                .collect();
            base.with_file_name(format!("{stem}-{}.png", title.to_lowercase()))
        };
        let Some(renderer) = window.renderer() else {
            continue;
        };
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(
            &snapshot,
            f64::from(window.width()),
            f64::from(window.height()),
        );
        let Some(node) = snapshot.to_node() else {
            continue;
        };
        let texture = renderer.render_texture(node, None);
        match texture.save_to_png(&target) {
            Ok(()) => debug_log!("[E2E] screenshot {}", target.display()),
            Err(error) => debug_log!("[E2E] screenshot failed: {error}"),
        }
    }
}

#[cfg(debug_assertions)]
fn debug_fake_device(kind: &str, name: &str) -> DeviceView {
    use crate::device_links::device::{BatteryStatus, DesktopSession, MediaStatus};
    use crate::device_links::device_info::DeviceInfo;
    let info = DeviceInfo {
        id: format!("fake{:028x}", glib::monotonic_time()),
        name: if name.is_empty() { "Pixel 8" } else { name }.replace('_', " "),
        device_type: "phone".to_string(),
        protocol_version: 9,
        incoming_capabilities: FeatureRegistry::desktop().outgoing_capabilities().to_vec(),
        outgoing_capabilities: FeatureRegistry::desktop().incoming_capabilities().to_vec(),
        instance_id: None,
    };
    let mut device = DeviceView::from_info(&info, "192.168.1.42".to_string(), true);
    match kind {
        "unreachable" => device.status = DeviceStatus::Unreachable,
        "pair-request" => {
            device.trusted = false;
            device.status = DeviceStatus::PairRequestedByPeer;
            device.verification_key = Some("482913".to_string());
        }
        "requested" => {
            device.trusted = false;
            device.status = DeviceStatus::PairRequested;
            device.verification_key = Some("482913".to_string());
        }
        "discovered" => {
            device.trusted = false;
            device.status = DeviceStatus::Discovered;
        }
        _ => {
            device.status = DeviceStatus::Paired;
            device.feature_transport_ready = true;
            device.connection_path =
                Some(if kind == "paired-turn" { "turn" } else { "lan" }.into());
            device.terminal_access = true;
            device.terminal_sessions_attached = 2;
            device.desktop_session = DesktopSession::Controlling;
            device.battery_status = Some(BatteryStatus {
                current_charge: 82,
                is_charging: true,
                battery_quantity: None,
                threshold_event: None,
            });
            device.media_status = Some(MediaStatus {
                player: "Spotify".into(),
                player_list: vec!["Spotify".into()],
                title: "Midnight City".into(),
                artist: "M83".into(),
                album: "Hurry Up, We're Dreaming".into(),
                is_playing: true,
                volume: Some(60),
                position: None,
                length: None,
                can_seek: false,
                album_art_url: None,
            });
            device.notifications = vec![
                DeviceNotification {
                    id: "1".into(),
                    app_name: "Messages".into(),
                    title: "Alex".into(),
                    text: "Are we still on for dinner tonight?".into(),
                    ticker: String::new(),
                    is_clearable: true,
                    request_reply_id: Some("r1".into()),
                    actions: vec![],
                },
                DeviceNotification {
                    id: "2".into(),
                    app_name: "Calendar".into(),
                    title: "Stand-up in 10 minutes".into(),
                    text: "Room 4 · Video call".into(),
                    ticker: String::new(),
                    is_clearable: true,
                    request_reply_id: None,
                    actions: vec!["Snooze".into()],
                },
            ];
        }
    }
    device
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

    #[test]
    fn the_pairing_code_reads_as_two_groups_of_three() {
        assert_eq!(spaced_code("482913"), "482 913");
        assert_eq!(spaced_code("012345"), "012 345");
        // Anything else is shown as it came.
        assert_eq!(spaced_code("12345"), "12345");
    }

    #[test]
    fn the_battery_icon_is_the_nearest_themed_level() {
        assert_eq!(battery_icon(82, false), "battery-level-80-symbolic");
        assert_eq!(battery_icon(86, true), "battery-level-90-charging-symbolic");
        assert_eq!(battery_icon(3, false), "battery-level-0-symbolic");
        assert_eq!(
            battery_icon(100, true),
            "battery-level-100-charged-symbolic"
        );
        assert_eq!(battery_icon(140, false), "battery-level-100-symbolic");
    }

    #[test]
    fn every_connection_path_has_words_and_unknown_ones_have_none() {
        assert_eq!(
            connection_path_presentation(Some("lan")).map(|(_, text)| text),
            Some("Local network")
        );
        assert_eq!(
            connection_path_presentation(Some("direct")).map(|(_, text)| text),
            Some("Over the internet")
        );
        assert_eq!(
            connection_path_presentation(Some("turn")).map(|(_, text)| text),
            Some("Through a relay server")
        );
        assert_eq!(connection_path_presentation(Some("carrier-pigeon")), None);
        assert_eq!(connection_path_presentation(None), None);
    }

    #[test]
    fn the_sidebar_row_names_its_unread_count() {
        assert_eq!(
            sidebar_label("Pixel 8", "Connected", 0),
            "Pixel 8, Connected"
        );
        assert_eq!(
            sidebar_label("Pixel 8", "Connected", 1),
            "Pixel 8, Connected, 1 notification"
        );
        assert_eq!(
            sidebar_label("Pixel 8", "Connected", 3),
            "Pixel 8, Connected, 3 notifications"
        );
    }

    #[test]
    fn daemon_errors_read_as_plain_words() {
        let (offline, lasting) = friendly_error("Network is unreachable (os error 101)");
        assert!(offline.starts_with("No network connection"), "{offline}");
        assert!(lasting, "being offline is a state, shown as a banner");

        let (refused, lasting) =
            friendly_error("connect 10.0.0.2:1716: Connection refused (os error 111)");
        assert!(refused.starts_with("The device refused"), "{refused}");
        assert!(!lasting);
        assert!(friendly_error("operation timed out")
            .0
            .contains("timed out"));
        assert!(friendly_error("Permission denied (os error 13)")
            .0
            .contains("not allowed"));
        // Error 1 is not error 101 or 13.
        assert!(friendly_error("Operation not permitted (os error 1)")
            .0
            .contains("not allowed"));

        // Anything else keeps its words but loses the errno tail.
        assert_eq!(
            friendly_error("Pairing failed (os error 71)"),
            ("Pairing failed".to_string(), false)
        );
        assert_eq!(friendly_error("Pairing failed").0, "Pairing failed");
    }
}
