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

    fn daemon_handle(&self) -> Option<DaemonHandle> {
        self.imp().daemon.borrow().clone()
    }

    fn main_window(&self) -> gtk::Window {
        self.active_window().unwrap_or_else(|| {
            let window = DesklinkWindow::new(self);
            if let Some(daemon) = self.daemon_handle() {
                window.set_daemon(daemon);
            }
            // Closing hides: the window's poll keeps delivering pings,
            // find-my-phone and errors, and reopening is instant.
            window.set_hide_on_close(true);
            window.upcast()
        })
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
                .is_some_and(|tray| tray.hosted());
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
        let status = adw::ActionRow::builder().title("Status").build();
        let status_label = gtk::Label::builder().css_classes(["dim-label"]).build();
        status.add_suffix(&status_label);

        if let Some(daemon) = self.daemon_handle() {
            let send_relay = {
                let daemon = daemon.clone();
                let enabled = enabled.clone();
                let url = url.clone();
                move || {
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
        use crate::device_links::connection_log;
        let parent = self.main_window();
        let window = adw::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .default_width(520)
            .default_height(560)
            .title("Connection History")
            .build();
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let entries = connection_log::recent();
        let content: gtk::Widget = if entries.is_empty() {
            adw::StatusPage::builder()
                .icon_name("document-open-recent-symbolic")
                .title("No Connections Yet")
                .description("Phones that connect to this computer are listed here for 30 days.")
                .build()
                .upcast()
        } else {
            let list = gtk::ListBox::builder()
                .selection_mode(gtk::SelectionMode::None)
                .css_classes(["boxed-list"])
                .build();
            for entry in entries {
                let when = glib::DateTime::from_unix_local(entry.ts as i64)
                    .and_then(|time| time.format("%e %b, %H:%M"))
                    .map(|text| text.to_string())
                    .unwrap_or_default();
                let device = if entry.device_name.is_empty() {
                    entry.device_id.clone()
                } else {
                    entry.device_name.clone()
                };
                list.append(
                    &adw::ActionRow::builder()
                        .title(glib::markup_escape_text(entry.event.label()).as_str())
                        .subtitle(glib::markup_escape_text(&format!("{device} · {when}")).as_str())
                        .build(),
                );
            }
            let clamp = adw::Clamp::builder()
                .child(&list)
                .margin_top(12)
                .margin_bottom(12)
                .margin_start(12)
                .margin_end(12)
                .build();
            gtk::ScrolledWindow::builder()
                .child(&clamp)
                .vexpand(true)
                .build()
                .upcast()
        };
        toolbar.set_content(Some(&content));
        window.set_content(Some(&toolbar));
        window.present();
    }

    fn show_about(&self) {
        let window = self.main_window();
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
        let parent = self.main_window();
        let window = adw::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .default_width(520)
            .default_height(360)
            .title("Preferences")
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());

        let page = adw::PreferencesPage::new();
        let network_group = adw::PreferencesGroup::builder()
            .title("Connection")
            .description("DeskLink Protocol v9 over the local network, on ports 1716-1764.")
            .build();

        let discovery_row = adw::ActionRow::builder()
            .title("Device Discovery")
            .subtitle("Broadcast identity packets and listen for nearby DeskLink devices.")
            .build();
        let discover = gtk::Button::builder()
            .label("Discover Now")
            .valign(gtk::Align::Center)
            .css_classes(["suggested-action"])
            .build();
        if let Some(daemon) = self.daemon_handle() {
            discover.connect_clicked(move |_| {
                daemon.send(crate::device_links::daemon::DaemonCommand::Discover);
            });
        }
        discovery_row.add_suffix(&discover);
        network_group.add(&discovery_row);

        let clipboard_row = adw::ActionRow::builder()
            .title("Clipboard Sync")
            .subtitle("Enabled automatically for paired devices while DeskLink is running.")
            .build();
        clipboard_row.add_suffix(
            &gtk::Image::builder()
                .icon_name("emblem-ok-symbolic")
                .css_classes(["success"])
                .build(),
        );
        network_group.add(&clipboard_row);

        let downloads_row = adw::ActionRow::builder()
            .title("Received Files")
            .subtitle("Incoming files are saved to your Downloads folder with automatic renaming.")
            .build();
        network_group.add(&downloads_row);

        page.add(&network_group);

        let settings = crate::device_links::config::app_settings();
        let access_group = adw::PreferencesGroup::builder()
            .title("Remote Access")
            .build();
        let stay_awake = adw::SwitchRow::builder()
            .title("Stay Awake for Your Phone")
            .subtitle("Don't sleep, even with the lid closed, while a phone is paired.")
            .active(settings.stay_awake)
            .build();
        if let Some(daemon) = self.daemon_handle() {
            stay_awake.connect_active_notify(move |row| {
                daemon.send(crate::device_links::daemon::DaemonCommand::SetStayAwake(
                    row.is_active(),
                ));
            });
        }
        access_group.add(&stay_awake);

        let password_row = adw::ActionRow::builder()
            .title("Session Password")
            .subtitle(password_subtitle(settings.session_password.is_some()))
            .build();
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
        if let Some(daemon) = self.daemon_handle() {
            let row = password_row.clone();
            let remove = remove_password.clone();
            let daemon_for_set = daemon.clone();
            set_password.connect_clicked(move |button| {
                let row = row.clone();
                let remove = remove.clone();
                let button = button.clone();
                let daemon = daemon_for_set.clone();
                let parent = row.clone();
                ask_for_password(&parent, move |password| {
                    daemon.send(
                        crate::device_links::daemon::DaemonCommand::SetSessionPassword(Some(
                            password,
                        )),
                    );
                    row.set_subtitle(password_subtitle(true));
                    button.set_label("Change…");
                    remove.set_visible(true);
                });
            });
            let row = password_row.clone();
            let set = set_password.clone();
            remove_password.connect_clicked(move |remove| {
                daemon.send(crate::device_links::daemon::DaemonCommand::SetSessionPassword(None));
                row.set_subtitle(password_subtitle(false));
                set.set_label("Set…");
                remove.set_visible(false);
            });
        }
        password_row.add_suffix(&remove_password);
        password_row.add_suffix(&set_password);
        access_group.add(&password_row);

        let history_row = adw::ActionRow::builder()
            .title("Connection History")
            .subtitle("Which phones connected, and what they did, for the last 30 days.")
            .activatable(true)
            .action_name("app.history")
            .build();
        history_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
        access_group.add(&history_row);
        page.add(&access_group);
        page.add(&self.away_from_home_group(&settings));

        toolbar.set_content(Some(&page));
        window.set_content(Some(&toolbar));
        window.present();
    }

    fn show_shortcuts(&self) {
        let parent = self.main_window();
        let window = adw::Window::builder()
            .transient_for(&parent)
            .modal(true)
            .default_width(420)
            .default_height(260)
            .title("Keyboard Shortcuts")
            .build();

        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .margin_top(20)
            .margin_bottom(20)
            .margin_start(20)
            .margin_end(20)
            .css_classes(["boxed-list"])
            .build();

        for (title, shortcut) in [("Show Shortcuts", "Ctrl+?"), ("Quit", "Ctrl+Q")] {
            let row = adw::ActionRow::builder().title(title).build();
            row.add_suffix(
                &gtk::Label::builder()
                    .label(shortcut)
                    .valign(gtk::Align::Center)
                    .css_classes(["caption", "monospace"])
                    .build(),
            );
            list.append(&row);
        }

        toolbar.set_content(Some(&list));
        window.set_content(Some(&toolbar));
        window.present();
    }
}

/// The indicator's line for `device`, if it is using this computer.
fn session_line(device: &crate::device_links::device::DeviceView) -> Option<String> {
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

    let check = {
        let dialog = dialog.clone();
        let entry = entry.clone();
        let confirm = confirm.clone();
        move || {
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
