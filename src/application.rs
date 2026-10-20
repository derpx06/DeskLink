/* application.rs
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
use gettextrs::gettext;
use gtk::{gio, glib};

use crate::config::VERSION;
use crate::device_links::daemon::DaemonHandle;
use crate::DesklinkWindow;

mod imp {
    use super::*;
    use std::cell::RefCell;

    #[derive(Default)]
    pub struct DesklinkApplication {
        pub daemon: RefCell<Option<DaemonHandle>>,
        /// Keeps DeskLink running with no window open: the phone stays
        /// connected and reachable until the user picks Quit.
        pub hold: RefCell<Option<gio::ApplicationHoldGuard>>,
        pub tray: RefCell<Option<crate::tray::Tray>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for DesklinkApplication {
        const NAME: &'static str = "DesklinkApplication";
        type Type = super::DesklinkApplication;
        type ParentType = adw::Application;
    }

    impl ObjectImpl for DesklinkApplication {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            self.daemon.replace(Some(DaemonHandle::start()));
            // The desk side of shared terminals (it leaves a running
            // instance's socket alone if this launch is a second one).
            crate::device_links::pty::desk::start_server();
            crate::device_links::connection_log::prune();
            obj.report_previous_crash();
            obj.setup_gactions();
            self.hold.replace(Some(obj.hold()));
            obj.start_session_indicator();
            obj.watch_network();
            obj.set_accels_for_action("app.quit", &["<control>q"]);
            obj.set_accels_for_action("app.shortcuts", &["<control>question"]);
            obj.set_accels_for_action("app.preferences", &["<control>comma"]);
            obj.set_accels_for_action("app.history", &["<control>h"]);
            obj.set_accels_for_action("win.pair-new", &["<control>n"]);
            obj.set_accels_for_action("win.toggle-search", &["<control>f"]);
            obj.set_accels_for_action("window.close", &["<control>w"]);
        }
    }

    impl ApplicationImpl for DesklinkApplication {
        // We connect to the activate callback to create a window when the application
        // has been launched. Additionally, this callback notifies us when the user
        // tries to launch a "second instance" of the application. When they try
        // to do that, we'll just present any existing window.
        fn activate(&self) {
            let application = self.obj();
            let window = application.main_window();

            // Ask the window manager/compositor to present the window
            window.present();
        }

        // Before the name is owned, so top-bar clients find it with the name.
        fn dbus_register(
            &self,
            connection: &gio::DBusConnection,
            object_path: &str,
        ) -> Result<(), glib::Error> {
            self.parent_dbus_register(connection, object_path)?;
            crate::shell_dbus::export(&self.obj(), connection);
            Ok(())
        }
    }

    impl GtkApplicationImpl for DesklinkApplication {}
    impl AdwApplicationImpl for DesklinkApplication {}
}

glib::wrapper! {
    pub struct DesklinkApplication(ObjectSubclass<imp::DesklinkApplication>)
        @extends gio::Application, gtk::Application, adw::Application,
        @implements gio::ActionGroup, gio::ActionMap;
}

impl DesklinkApplication {
    pub fn new(application_id: &str, flags: &gio::ApplicationFlags) -> Self {
        glib::Object::builder()
            .property("application-id", application_id)
            .property("flags", flags)
            .property("resource-base-path", "/derx06/desklink/com")
            .build()
    }

    fn setup_gactions(&self) {
        let quit_action = gio::ActionEntry::builder("quit")
            .activate(move |app: &Self, _, _| app.quit())
            .build();
        let about_action = gio::ActionEntry::builder("about")
            .activate(move |app: &Self, _, _| app.show_about())
            .build();
        let preferences_action = gio::ActionEntry::builder("preferences")
            .activate(move |app: &Self, _, _| app.show_preferences())
            .build();
        let shortcuts_action = gio::ActionEntry::builder("shortcuts")
            .activate(move |app: &Self, _, _| app.show_shortcuts())
            .build();
        // Target: a device id, or "" for every device. Used by the session
        // notification's button and the tray menu.
        let end_sessions_action = gio::ActionEntry::builder("end-sessions")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                let id = target
                    .and_then(|target| target.get::<String>())
                    .filter(|id| !id.is_empty());
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(crate::device_links::daemon::DaemonCommand::EndPhoneSessions(id));
                }
            })
            .build();
        let history_action = gio::ActionEntry::builder("history")
            .activate(move |app: &Self, _, _| app.show_history())
            .build();
        self.add_action_entries([
            quit_action,
            about_action,
            preferences_action,
            shortcuts_action,
            end_sessions_action,
            history_action,
        ]);
    }

    pub(crate) fn daemon_handle(&self) -> Option<DaemonHandle> {
        self.imp().daemon.borrow().clone()
    }

    /// Whether a tray host is showing DeskLink's StatusNotifierItem.
    pub(crate) fn tray_hosted(&self) -> bool {
        self.imp()
            .tray
            .borrow()
            .as_ref()
            .is_some_and(|tray| tray.hosted())
    }

    /// The main window, never one of the Phone Screen, Phone Files or File
    /// Transfers windows that `active_window()` can return.
    fn main_window(&self) -> DesklinkWindow {
        if let Some(window) = self
            .windows()
            .into_iter()
            .find_map(|window| window.downcast::<DesklinkWindow>().ok())
        {
            return window;
        }
        let window = DesklinkWindow::new(self);
        if let Some(daemon) = self.daemon_handle() {
            window.set_daemon(daemon);
        }
        // Closing hides: the window's poll keeps delivering pings,
        // find-my-phone and errors, and reopening is instant.
        window.set_hide_on_close(true);
        window
    }

    /// The main window, unless a dialog is already up on it: accelerators
    /// must not stack a second Preferences (or History, …) over the first.
    fn window_for_dialog(&self) -> Option<DesklinkWindow> {
        let window = self.main_window();
        window.present();
        window.visible_dialog().is_none().then_some(window)
    }

    /// Shows what phones are doing here — in the top bar when a tray host
    /// exists, otherwise as a notification when a session starts — once a
    /// second, from the daemon's device view.
    fn start_session_indicator(&self) {
        let app = self.downgrade();
        let actions = crate::tray::TrayActions {
            open: Box::new({
                let app = app.clone();
                move || {
                    if let Some(app) = app.upgrade() {
                        app.activate();
                    }
                }
            }),
            end_sessions: Box::new({
                let app = app.clone();
                move || {
                    if let Some(app) = app.upgrade() {
                        app.activate_action("end-sessions", Some(&"".to_variant()));
                    }
                }
            }),
            quit: Box::new({
                let app = app.clone();
                move || {
                    if let Some(app) = app.upgrade() {
                        app.quit();
                    }
                }
            }),
        };
        match crate::tray::Tray::new(actions) {
            Ok(tray) => {
                self.imp().tray.replace(Some(tray));
            }
            Err(error) => eprintln!("[DeskLink] No top-bar indicator: {error}"),
        }

        let mut notified = std::collections::HashSet::<String>::new();
        glib::timeout_add_seconds_local(1, move || {
            let Some(app) = app.upgrade() else {
                return glib::ControlFlow::Break;
            };
            let Some(daemon) = app.daemon_handle() else {
                return glib::ControlFlow::Continue;
            };
            let active: Vec<_> = daemon
                .devices()
                .into_iter()
                .filter_map(|device| session_line(&device).map(|line| (device, line)))
                .collect();
            let hosted = app
                .imp()
                .tray
                .borrow()
                .as_ref()
                .is_some_and(|tray| tray.hosted())
                || crate::shell_dbus::extension_active();
            if !hosted {
                for (device, line) in &active {
                    if notified.insert(device.id.clone()) {
                        let notification = gio::Notification::new(line);
                        notification.set_body(Some("Open DeskLink to see more."));
                        notification.add_button_with_target_value(
                            "End Session",
                            "app.end-sessions",
                            Some(&device.id.to_variant()),
                        );
                        app.send_notification(
                            Some(&format!("session-{}", device.id)),
                            &notification,
                        );
                    }
                }
            }
            notified.retain(|id| active.iter().any(|(device, _)| &device.id == id));
            if let Some(tray) = app.imp().tray.borrow().as_ref() {
                tray.update(active.into_iter().map(|(_, line)| line).collect());
            }
            glib::ControlFlow::Continue
        });
    }

    /// Relay on/off, its address, TURN, and the relay's current state.
    fn away_from_home_group(
        &self,
        settings: &crate::device_links::config::AppSettings,
    ) -> adw::PreferencesGroup {
        use crate::device_links::daemon::DaemonCommand;
        let group = adw::PreferencesGroup::builder()
            .title("Away From Home")
            .description(
                "When your phone is on another network, it meets this computer through the \
                 DeskLink server. What they exchange is end-to-end encrypted.",
            )
            .build();
        let enabled = adw::SwitchRow::builder()
            .title("Connect Away From Home")
            .active(settings.relay_enabled)
            .build();
        enabled.add_prefix(&crate::window::badge(
            "network-server-symbolic",
            "blue",
            true,
        ));
        let url = adw::EntryRow::builder()
            .title("Server (leave empty for the DeskLink server)")
            .text(settings.relay_url.as_str())
            .show_apply_button(true)
            .build();
        let turn = adw::SwitchRow::builder()
            .title("Relay Media When Needed")
            .subtitle(
                "Use a TURN server when no direct path exists. Off means direct connections only.",
            )
            .active(settings.use_turn)
            .build();
        turn.add_prefix(&crate::window::badge(
            "network-transmit-receive-symbolic",
            "purple",
            true,
        ));
        let status = adw::ActionRow::builder().title("Status").build();
        status.add_prefix(&crate::window::badge(
            "network-workgroup-symbolic",
            "slate",
            true,
        ));
        let status_label = gtk::Label::builder().css_classes(["dim-label"]).build();
        status.add_suffix(&status_label);

        if let Some(daemon) = self.daemon_handle() {
            // Weak: the closure is connected on these very rows.
            let send_relay = {
                let daemon = daemon.clone();
                let enabled = enabled.downgrade();
                let url = url.downgrade();
                move || {
                    let (Some(enabled), Some(url)) = (enabled.upgrade(), url.upgrade()) else {
                        return;
                    };
                    daemon.send(DaemonCommand::SetRelay {
                        enabled: enabled.is_active(),
                        url: url.text().trim().to_string(),
                    })
                }
            };
            enabled.connect_active_notify({
                let send_relay = send_relay.clone();
                move |_| send_relay()
            });
            url.connect_apply(move |_| send_relay());
            turn.connect_active_notify(move |row| {
                daemon.send(DaemonCommand::SetUseTurn(row.is_active()))
            });
        }

        let update = move |label: &gtk::Label| {
            use crate::device_links::relay_link::client::{status, RelayStatus};
            label.set_label(&match status() {
                RelayStatus::Off => "Off".to_string(),
                RelayStatus::Connecting => "Connecting…".to_string(),
                RelayStatus::Online => "Ready".to_string(),
                RelayStatus::Unavailable(reason) => format!("Unavailable: {reason}"),
                RelayStatus::UpdateRequired => {
                    "Update DeskLink to connect away from home".to_string()
                }
            });
        };
        update(&status_label);
        let weak = status_label.downgrade();
        glib::timeout_add_seconds_local(1, move || match weak.upgrade() {
            Some(label) => {
                update(&label);
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });

        group.add(&enabled);
        group.add(&url);
        group.add(&turn);
        group.add(&status);
        group
    }

    /// Reconnects the relay whenever the network changes (debounced: one
    /// switch fires several signals).
    fn watch_network(&self) {
        let pending: std::rc::Rc<std::cell::Cell<Option<glib::SourceId>>> = Default::default();
        let app = self.downgrade();
        gio::NetworkMonitor::default().connect_network_changed(move |_, _| {
            if let Some(source) = pending.take() {
                source.remove();
            }
            let app = app.clone();
            let pending_inner = pending.clone();
            pending.set(Some(glib::timeout_add_seconds_local_once(2, move || {
                pending_inner.set(None);
                if let Some(daemon) = app.upgrade().and_then(|app| app.daemon_handle()) {
                    daemon.send(crate::device_links::daemon::DaemonCommand::NetworkChanged);
                }
            })));
        });
    }

    /// Mentions a crash report written since the last start, once.
    fn report_previous_crash(&self) {
        let Some(dir) = crate::crash_dir() else {
            return;
        };
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return;
        };
        let seen = dir.join("crash-reported");
        let reported_up_to = std::fs::metadata(&seen)
            .and_then(|meta| meta.modified())
            .ok();
        let newest = entries
            .flatten()
            .filter(|entry| entry.file_name().to_string_lossy().starts_with("crash-1"))
            .filter_map(|entry| entry.metadata().ok()?.modified().ok())
            .max();
        let Some(newest) = newest else {
            return;
        };
        if reported_up_to.is_some_and(|reported| reported >= newest) {
            return;
        }
        let _ = std::fs::write(&seen, b"");
        let notification = gio::Notification::new("DeskLink stopped unexpectedly");
        notification.set_body(Some(&format!(
            "A crash report was saved in {}. Attaching it to a bug report helps fix this.",
            dir.display()
        )));
        self.send_notification(Some("crash-report"), &notification);
    }

    fn show_history(&self) {
        let Some(parent) = self.window_for_dialog() else {
            return;
        };
        let dialog = adw::Dialog::builder()
            .title("Connection History")
            .content_width(520)
            .content_height(620)
            .child(&with_header(&history_content()))
            .build();
        dialog.present(Some(&parent));
    }

    fn show_about(&self) {
        let Some(window) = self.window_for_dialog() else {
            return;
        };
        let about = adw::AboutDialog::builder()
            .application_name("DeskLink")
            .application_icon("derx06.desklink.com")
            .developer_name("manas")
            .version(VERSION)
            .developers(vec!["manas"])
            // Translators: Replace "translator-credits" with your name/username, and optionally an email or URL.
            .translator_credits(gettext("translator-credits"))
            .copyright("© 2026 manas")
            .build();

        about.present(Some(&window));
    }

    fn show_preferences(&self) {
        use crate::device_links::daemon::DaemonCommand;
        let Some(parent) = self.window_for_dialog() else {
            return;
        };
        let dialog = adw::PreferencesDialog::builder()
            .search_enabled(true)
            .content_width(720)
            .build();
        let settings = crate::device_links::config::app_settings();

        // ── General ─────────────────────────────────────────────────────────
        let general = adw::PreferencesPage::builder()
            .title("General")
            .icon_name("preferences-system-symbolic")
            .build();

        let startup_group = adw::PreferencesGroup::builder().title("Background").build();
        let start_at_login = adw::SwitchRow::builder()
            .title("Start at Login")
            .subtitle("Run in the background so your phone can reach this computer after a restart")
            .active(settings.start_at_login.unwrap_or(true))
            .build();
        start_at_login.add_prefix(&crate::window::badge(
            "system-reboot-symbolic",
            "green",
            true,
        ));
        if let Some(daemon) = self.daemon_handle() {
            start_at_login.connect_active_notify(move |row| {
                daemon.send(DaemonCommand::SetStartAtLogin(row.is_active()));
            });
        }
        startup_group.add(&start_at_login);
        general.add(&startup_group);

        let network_group = adw::PreferencesGroup::builder()
            .title("Connection")
            .description("DeskLink Protocol v9 over the local network, on ports 1716-1764.")
            .build();

        let discovery_row = adw::ActionRow::builder()
            .title("Device Discovery")
            .subtitle("Broadcast identity packets and listen for nearby DeskLink devices.")
            .build();

        discovery_row.add_prefix(&crate::window::badge(
            "network-wireless-symbolic",
            "blue",
            true,
        ));
        let discover = gtk::Button::builder()
            .label("Discover Now")
            .valign(gtk::Align::Center)
            .build();
        if let Some(daemon) = self.daemon_handle() {
            discover.connect_clicked(move |button| {
                daemon.send(DaemonCommand::Discover);
                if let Some(dialog) = button
                    .ancestor(adw::PreferencesDialog::static_type())
                    .and_downcast::<adw::PreferencesDialog>()
                {
                    dialog.add_toast(adw::Toast::new("Searching the network…"));
                }
            });
        }
        discovery_row.add_suffix(&discover);
        network_group.add(&discovery_row);

        let clipboard_row = adw::ActionRow::builder()
            .title("Clipboard Sync")
            .subtitle("Enabled automatically for paired devices while DeskLink is running.")
            .build();

        clipboard_row.add_prefix(&crate::window::badge("edit-paste-symbolic", "blue", true));
        clipboard_row.add_suffix(
            &gtk::Image::builder()
                .icon_name("object-select-symbolic")
                .tooltip_text("On")
                .css_classes(["success"])
                .build(),
        );
        network_group.add(&clipboard_row);

        let downloads_row = adw::ActionRow::builder()
            .title("Received Files")
            .subtitle("Incoming files are saved to your Downloads folder with automatic renaming.")
            .build();

        downloads_row.add_prefix(&crate::window::badge(
            "folder-download-symbolic",
            "green",
            true,
        ));
        network_group.add(&downloads_row);
        general.add(&network_group);
        dialog.add(&general);

        // ── Remote Access ───────────────────────────────────────────────────
        let access = adw::PreferencesPage::builder()
            .title("Remote Access")
            .icon_name("preferences-desktop-remote-desktop-symbolic")
            .build();
        let access_group = adw::PreferencesGroup::builder()
            .title("Remote Access")
            .description("What your paired phones may do with this computer.")
            .build();
        let stay_awake = adw::SwitchRow::builder()
            .title("Stay Awake for Your Phone")
            .subtitle("Don't sleep, even with the lid closed, while a phone is paired.")
            .active(settings.stay_awake)
            .build();
        stay_awake.add_prefix(&crate::window::badge(
            "weather-clear-night-symbolic",
            "purple",
            true,
        ));
        if let Some(daemon) = self.daemon_handle() {
            stay_awake.connect_active_notify(move |row| {
                daemon.send(DaemonCommand::SetStayAwake(row.is_active()));
            });
        }
        access_group.add(&stay_awake);

        let password_row = adw::ActionRow::builder()
            .title("Session Password")
            .subtitle(password_subtitle(settings.session_password.is_some()))
            .build();

        password_row.add_prefix(&crate::window::badge(
            "dialog-password-symbolic",
            "orange",
            true,
        ));
        let set_password = gtk::Button::builder()
            .label(if settings.session_password.is_some() {
                "Change…"
            } else {
                "Set…"
            })
            .valign(gtk::Align::Center)
            .build();
        let remove_password = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove the session password")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .visible(settings.session_password.is_some())
            .build();
        remove_password.update_property(&[gtk::accessible::Property::Label(
            "Remove the session password",
        )]);
        if let Some(daemon) = self.daemon_handle() {
            // Weak references: both buttons live inside the row they update.
            let row = password_row.downgrade();
            let remove = remove_password.downgrade();
            let daemon_for_set = daemon.clone();
            set_password.connect_clicked(move |button| {
                let Some(parent) = row.upgrade() else {
                    return;
                };
                let (row, remove, button) = (row.clone(), remove.clone(), button.downgrade());
                let daemon = daemon_for_set.clone();
                ask_for_password(&parent, move |password| {
                    daemon.send(DaemonCommand::SetSessionPassword(Some(password)));
                    if let Some(row) = row.upgrade() {
                        row.set_subtitle(password_subtitle(true));
                    }
                    if let Some(button) = button.upgrade() {
                        button.set_label("Change…");
                    }
                    if let Some(remove) = remove.upgrade() {
                        remove.set_visible(true);
                    }
                });
            });
            let row = password_row.downgrade();
            let set = set_password.downgrade();
            remove_password.connect_clicked(move |remove| {
                daemon.send(DaemonCommand::SetSessionPassword(None));
                if let Some(row) = row.upgrade() {
                    row.set_subtitle(password_subtitle(false));
                }
                if let Some(set) = set.upgrade() {
                    set.set_label("Set…");
                }
                remove.set_visible(false);
            });
        }
        password_row.add_suffix(&remove_password);
        password_row.add_suffix(&set_password);
        access_group.add(&password_row);
        access.add(&access_group);

        let history_group = adw::PreferencesGroup::new();
        let history_row = adw::ActionRow::builder()
            .title("Connection History")
            .subtitle("Which phones connected, and what they did, for the last 30 days.")
            .activatable(true)
            .build();
        history_row.add_prefix(&crate::window::badge(
            "document-open-recent-symbolic",
            "slate",
            true,
        ));
        history_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        {
            let dialog = dialog.downgrade();
            history_row.connect_activated(move |_| {
                if let Some(dialog) = dialog.upgrade() {
                    dialog.push_subpage(
                        &adw::NavigationPage::builder()
                            .title("Connection History")
                            .child(&with_header(&history_content()))
                            .build(),
                    );
                }
            });
        }
        history_group.add(&history_row);
        access.add(&history_group);
        dialog.add(&access);

        // ── Away From Home ──────────────────────────────────────────────────
        let away = adw::PreferencesPage::builder()
            .title("Away From Home")
            .icon_name("network-server-symbolic")
            .build();
        away.add(&self.away_from_home_group(&settings));
        dialog.add(&away);

        dialog.present(Some(&parent));
    }

    /// The stock shortcuts dialog from `shortcuts-dialog.ui` where libadwaita
    /// has one (1.8); before that, the same shortcuts in a plain list.
    fn show_shortcuts(&self) {
        let Some(parent) = self.window_for_dialog() else {
            return;
        };
        if adw::minor_version() >= 8 {
            let builder = gtk::Builder::new();
            match builder.add_from_resource("/derx06/desklink/com/shortcuts-dialog.ui") {
                Ok(()) => {
                    if let Some(dialog) = builder.object::<adw::Dialog>("shortcuts_dialog") {
                        dialog.present(Some(&parent));
                        return;
                    }
                }
                Err(error) => eprintln!("[UI] No shortcuts dialog: {error}"),
            }
        }
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        for (title, shortcut) in [
            ("Pair New Device", "Ctrl+N"),
            ("Search Devices", "Ctrl+F"),
            ("Connection History", "Ctrl+H"),
            ("Preferences", "Ctrl+,"),
            ("Show Shortcuts", "Ctrl+?"),
            ("Close Window", "Ctrl+W"),
            ("Quit", "Ctrl+Q"),
        ] {
            let row = adw::ActionRow::builder().title(title).build();
            row.add_suffix(
                &gtk::Label::builder()
                    .label(shortcut)
                    .valign(gtk::Align::Center)
                    .css_classes(["dim-label", "monospace"])
                    .build(),
            );
            list.append(&row);
        }
        let content = adw::Clamp::builder()
            .child(&list)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        adw::Dialog::builder()
            .title("Keyboard Shortcuts")
            .content_width(420)
            .child(&with_header(&content))
            .build()
            .present(Some(&parent));
    }
}

/// `content` under a plain header bar, for a dialog or a pushed subpage.
fn with_header(content: &impl IsA<gtk::Widget>) -> adw::ToolbarView {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(content));
    toolbar
}

/// Connection history grouped by day, newest first, with a time for each.
fn history_content() -> gtk::Widget {
    use crate::device_links::connection_log;
    let entries = connection_log::recent();
    if entries.is_empty() {
        return adw::StatusPage::builder()
            .icon_name("document-open-recent-symbolic")
            .title("No Connections Yet")
            .description("Phones that connect to this computer are listed here for 30 days.")
            .vexpand(true)
            .build()
            .upcast();
    }
    let now = glib::DateTime::now_local().ok();
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(12)
        .margin_bottom(24)
        .margin_start(12)
        .margin_end(12)
        .build();
    let mut group: Option<(String, adw::PreferencesGroup)> = None;
    for entry in entries {
        let Some(when) = glib::DateTime::from_unix_local(entry.ts as i64).ok() else {
            continue;
        };
        let day = now
            .as_ref()
            .map(|now| day_label(now, &when))
            .unwrap_or_default();
        if group.as_ref().is_none_or(|(shown, _)| *shown != day) {
            let next = adw::PreferencesGroup::builder().title(&day).build();
            page.append(&next);
            group = Some((day, next));
        }
        let device = if entry.device_name.is_empty() {
            entry.device_id.clone()
        } else {
            entry.device_name.clone()
        };
        let mut event = entry.event.label().to_string();
        if let Some(path) = entry.path.as_deref().and_then(path_label) {
            event = format!("{event} · {path}");
        }
        let row = adw::ActionRow::builder()
            .title(device)
            .subtitle(event)
            .use_markup(false)
            .build();
        let (icon, hue) = event_icon(entry.event);
        row.add_prefix(&crate::window::badge(icon, hue, true));
        let time = now
            .as_ref()
            .map(|now| time_label(now, &when))
            .unwrap_or_default();
        row.add_suffix(
            &gtk::Label::builder()
                .label(time)
                .tooltip_text(
                    when.format("%e %B %Y, %H:%M")
                        .map(|text| text.trim().to_string())
                        .unwrap_or_default(),
                )
                .css_classes(["dim-label", "caption", "numeric"])
                .build(),
        );
        if let Some((_, group)) = &group {
            group.add(&row);
        }
    }
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&adw::Clamp::builder().child(&page).build())
        .build()
        .upcast()
}

fn path_label(path: &str) -> Option<&'static str> {
    match path {
        "lan" => Some("Local network"),
        "direct" => Some("Direct over the internet"),
        "turn" => Some("Through a TURN relay"),
        _ => None,
    }
}

/// The badge icon and hue for a history event.
fn event_icon(
    event: crate::device_links::connection_log::ConnectionEvent,
) -> (&'static str, &'static str) {
    use crate::device_links::connection_log::ConnectionEvent::*;
    match event {
        Connected => ("network-transmit-receive-symbolic", "green"),
        Disconnected => ("network-offline-symbolic", "slate"),
        ScreenViewStarted | ScreenControlStarted => ("screen-shared-symbolic", "blue"),
        ScreenSessionEnded => ("video-display-symbolic", "slate"),
        TerminalAttached => ("utilities-terminal-symbolic", "purple"),
        TerminalDetached => ("utilities-terminal-symbolic", "slate"),
        PasswordRejected => ("dialog-warning-symbolic", "red"),
        SessionsEnded => ("process-stop-symbolic", "orange"),
        Route => ("network-workgroup-symbolic", "teal"),
    }
}

/// "Today", "Yesterday", a weekday within the week, else the date.
fn day_label(now: &glib::DateTime, when: &glib::DateTime) -> String {
    let midnight = |time: &glib::DateTime| {
        let (year, month, day) = time.ymd();
        glib::DateTime::from_local(year, month, day, 0, 0, 0.0).ok()
    };
    let days = match (midnight(now), midnight(when)) {
        // Rounded, so a day with a daylight-saving change still counts as one.
        (Some(today), Some(then)) => (today.difference(&then).as_seconds() + 43_200) / 86_400,
        _ => i64::MAX,
    };
    let format = match days {
        0 => return "Today".to_string(),
        1 => return "Yesterday".to_string(),
        2..=6 => "%A",
        _ => "%e %B",
    };
    when.format(format)
        .map(|text| text.trim().to_string())
        .unwrap_or_default()
}

/// "Just now", "12 min ago" and "3 h ago" for the last few hours, else the
/// clock time.
fn time_label(now: &glib::DateTime, when: &glib::DateTime) -> String {
    let seconds = now.difference(when).as_seconds();
    match seconds {
        ..60 => "Just now".to_string(),
        60..3600 => format!("{} min ago", seconds / 60),
        3600..14400 => format!("{} h ago", seconds / 3600),
        _ => when
            .format("%H:%M")
            .map(|text| text.to_string())
            .unwrap_or_default(),
    }
}

/// The indicator's line for `device`, if it is using this computer.
pub(crate) fn session_line(device: &crate::device_links::device::DeviceView) -> Option<String> {
    use crate::device_links::device::DesktopSession;
    let screen = match device.desktop_session {
        DesktopSession::None => None,
        DesktopSession::Viewing => Some("viewing the screen"),
        DesktopSession::Controlling => Some("controlling the screen"),
    };
    let terminals = match device.terminal_sessions_attached {
        0 => None,
        1 => Some("1 terminal".to_string()),
        count => Some(format!("{count} terminals")),
    };
    let what = match (screen, terminals) {
        (None, None) => return None,
        (Some(screen), None) => screen.to_string(),
        (None, Some(terminals)) => format!("using {terminals}"),
        (Some(screen), Some(terminals)) => format!("{screen}, {terminals}"),
    };
    Some(format!("{} — {what}", device.name))
}

fn password_subtitle(set: bool) -> &'static str {
    if set {
        "On. Phones ask for it before viewing the screen or opening a terminal."
    } else {
        "Off. Pairing alone lets your phones view the screen and open terminals."
    }
}

/// Asks for a new session password twice; `done` gets it when both match.
fn ask_for_password(parent: &impl IsA<gtk::Widget>, done: impl Fn(String) + 'static) {
    let entry = adw::PasswordEntryRow::builder()
        .title("New password")
        .build();
    let confirm = adw::PasswordEntryRow::builder()
        .title("Confirm password")
        .activates_default(true)
        .build();
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    list.append(&entry);
    list.append(&confirm);

    let dialog = adw::AlertDialog::new(
        Some("Session Password"),
        Some("Your phone asks for this once per connection before it can view or control the screen or open a terminal."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("ok", "Set Password")]);
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");
    dialog.set_response_enabled("ok", false);
    dialog.set_extra_child(Some(&list));

    // Weak: `check` is connected on the entries and enables the dialog.
    let check = {
        let dialog = dialog.downgrade();
        let entry = entry.downgrade();
        let confirm = confirm.downgrade();
        move || {
            let (Some(dialog), Some(entry), Some(confirm)) =
                (dialog.upgrade(), entry.upgrade(), confirm.upgrade())
            else {
                return;
            };
            let password = entry.text();
            let usable = !password.is_empty()
                && password.len() <= crate::device_links::session_lock::MAX_PASSWORD_BYTES
                && password == confirm.text();
            dialog.set_response_enabled("ok", usable);
        }
    };
    entry.connect_changed({
        let check = check.clone();
        move |_| check()
    });
    confirm.connect_changed(move |_| check());

    dialog.choose(parent, gio::Cancellable::NONE, move |response| {
        if response == "ok" {
            done(entry.text().to_string());
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(year: i32, month: i32, day: i32, hour: i32, minute: i32) -> glib::DateTime {
        glib::DateTime::from_local(year, month, day, hour, minute, 0.0).unwrap()
    }

    #[test]
    fn history_days_read_as_today_yesterday_then_dates() {
        let now = at(2026, 10, 20, 9, 0);
        assert_eq!(day_label(&now, &at(2026, 10, 20, 0, 5)), "Today");
        assert_eq!(day_label(&now, &at(2026, 10, 19, 23, 50)), "Yesterday");
        assert_eq!(day_label(&now, &at(2026, 10, 16, 12, 0)), "Friday");
        assert_eq!(day_label(&now, &at(2026, 10, 1, 12, 0)), "1 October");
        // Across a new year, yesterday is still yesterday.
        assert_eq!(
            day_label(&at(2027, 1, 1, 9, 0), &at(2026, 12, 31, 22, 0)),
            "Yesterday"
        );
    }

    #[test]
    fn recent_history_times_are_relative_and_older_ones_are_clock_times() {
        let now = at(2026, 10, 20, 15, 0);
        assert_eq!(time_label(&now, &at(2026, 10, 20, 14, 59)), "1 min ago");
        assert_eq!(time_label(&now, &now), "Just now");
        assert_eq!(time_label(&now, &at(2026, 10, 20, 12, 30)), "2 h ago");
        assert_eq!(time_label(&now, &at(2026, 10, 20, 8, 5)), "08:05");
    }
}
