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
            crate::startup("application constructed");
            self.daemon.replace(Some(DaemonHandle::start()));
            crate::startup("daemon started");
            // The desk side of shared terminals (it leaves a running
            // instance's socket alone if this launch is a second one).
            crate::device_links::pty::desk::start_server();
            // Housekeeping that reads and rewrites files: off the main
            // thread, so the window does not wait for it.
            let _ = std::thread::Builder::new()
                .name("desklink-housekeeping".into())
                .spawn(|| {
                    crate::device_links::connection_log::prune();
                    crate::device_links::transfer_log::prune();
                    crate::thumbnails::prune();
                    // GNOME integration files this install provides for itself.
                    crate::search_provider::install_description();
                    crate::kde::install_runner();
                });
            crate::send_to::start(&obj);
            crate::startup("integration files written");
            obj.setup_gactions();
            self.hold.replace(Some(obj.hold()));
            obj.start_session_indicator();
            crate::bucket_progress::start(&obj);
            obj.watch_network();
            crate::startup("indicators and network watch ready");
            obj.set_accels_for_action("app.quit", &["<control>q"]);
            obj.set_accels_for_action("app.shortcuts", &["<control>question"]);
            obj.set_accels_for_action("app.preferences", &["<control>comma"]);
            obj.set_accels_for_action("app.history", &["<control>h"]);
            obj.set_accels_for_action("win.pair-new", &["<control>n"]);
            obj.set_accels_for_action("win.type-on-phone", &["<control><shift>k"]);
            obj.set_accels_for_action("win.toggle-search", &["<control>f"]);
            obj.set_accels_for_action("window.close", &["<control>w"]);
        }
    }

    impl ApplicationImpl for DesklinkApplication {
        fn startup(&self) {
            self.parent_startup();
            // Registered now (startup runs only in the primary instance), so
            // the notice is really shown before the crash is marked told.
            self.obj().report_previous_crash();
            register_bundled_fonts();
            install_brand_styles();
            crate::startup("styles and fonts ready");
            // A logout, a reboot or `kill` quits like File > Quit does, so
            // glide gives the pointer back on the way out.
            let app = self.obj().downgrade();
            for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
                let app = app.clone();
                glib::unix_signal_add_local(signal, move || {
                    if let Some(app) = app.upgrade() {
                        app.quit();
                    }
                    glib::ControlFlow::Break
                });
            }
        }

        fn shutdown(&self) {
            // The pointer is never left on a phone: bounded, so a compositor
            // that does not answer cannot hold the app open.
            crate::device_links::glide::shutdown();
            self.parent_shutdown();
        }

        // We connect to the activate callback to create a window when the application
        // has been launched. Additionally, this callback notifies us when the user
        // tries to launch a "second instance" of the application. When they try
        // to do that, we'll just present any existing window.
        fn activate(&self) {
            let application = self.obj();
            crate::startup("activate");
            let window = application.main_window();
            crate::startup("window built");

            // Ask the window manager/compositor to present the window
            window.connect_map(|_| crate::startup("window mapped"));
            window.present();
        }

        // `tel:` links (the desktop file's `x-scheme-handler/tel`). Anything
        // else just shows the window.
        fn open(&self, files: &[gio::File], _hint: &str) {
            let application = self.obj();
            let numbers: Vec<String> = files
                .iter()
                .map(|file| file.uri().to_string())
                .filter(|uri| {
                    uri.get(..4)
                        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("tel:"))
                })
                .collect();
            match numbers.first() {
                Some(number) => application.dial(number),
                None => application.activate(),
            }
        }

        // Before the name is owned, so top-bar clients find it with the name.
        fn dbus_register(
            &self,
            connection: &gio::DBusConnection,
            object_path: &str,
        ) -> Result<(), glib::Error> {
            self.parent_dbus_register(connection, object_path)?;
            crate::shell_dbus::export(&self.obj(), connection);
            crate::phone_mpris::start(&self.obj());
            crate::search_provider::export(&self.obj(), connection);
            crate::kde::export(&self.obj(), connection);
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
        // Lock this computer and offer the newest terminal to the phones:
        // the tray, the top-bar menu and the overview.
        let lock_hand_off_action = gio::ActionEntry::builder("lock-hand-off")
            .activate(move |app: &Self, _, _| {
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(crate::device_links::daemon::DaemonCommand::LockAndHandOff);
                }
            })
            .build();
        // A phone's Bucket page (the sync notification, the tray, the top bar).
        let open_bucket_action = gio::ActionEntry::builder("open-bucket")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                let window = app.main_window();
                window.present();
                if let Some(device) = target.and_then(|target| target.get::<String>()) {
                    window.show_device(&device);
                }
                window.show_page("files");
            })
            .build();
        let history_action = gio::ActionEntry::builder("history")
            .activate(move |app: &Self, _, _| app.show_history())
            .build();
        // (device, action, number): a call button in a notification — the
        // user's own tap, so it goes straight to the phone.
        let call_action = gio::ActionEntry::builder("call")
            .parameter_type(Some(glib::VariantTy::new("(sss)").expect("valid type")))
            .activate(move |app: &Self, _, target| {
                let Some((device, action, number)) =
                    target.and_then(|target| target.get::<(String, String, String)>())
                else {
                    return;
                };
                if let Some(daemon) = app.daemon_handle() {
                    daemon.send(crate::device_links::daemon::DaemonCommand::Telephony(
                        device,
                        action,
                        Some(number).filter(|number| !number.is_empty()),
                    ));
                }
            })
            .build();
        // A number from outside (`tel:` links, `desklink --dial`): asked
        // first, in the window.
        let dial_action = gio::ActionEntry::builder("dial")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(number) = target.and_then(|target| target.get::<String>()) {
                    app.dial(&number);
                }
            })
            .build();
        crate::send_to::add_action(self);
        crate::send_to::add_bucket_action(self);
        self.add_action_entries([
            quit_action,
            about_action,
            preferences_action,
            shortcuts_action,
            end_sessions_action,
            lock_hand_off_action,
            history_action,
            open_bucket_action,
            call_action,
            dial_action,
        ]);
    }

    /// Shows the window and asks before calling `number` with the phone.
    pub(crate) fn dial(&self, number: &str) {
        let window = self.main_window();
        window.present();
        crate::window::confirm_dial(&window, number, None);
    }

    /// Each known device's type, by id.
    fn device_types(&self) -> std::collections::HashMap<String, String> {
        self.daemon_handle()
            .map(|daemon| {
                daemon
                    .devices()
                    .into_iter()
                    .map(|device| (device.id, device.device_type))
                    .collect()
            })
            .unwrap_or_default()
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

    /// The main window, never the Phone Screen window that `active_window()`
    /// can return.
    pub(crate) fn main_window(&self) -> DesklinkWindow {
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
            run: Box::new({
                let app = app.clone();
                move |action| {
                    if let Some(app) = app.upgrade() {
                        crate::desk_menu::run(&app, action);
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
        crate::ticker::every_tick(move || {
            crate::guard("app tick", glib::ControlFlow::Continue, || {
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
                    let devices = daemon.devices();
                    let hand_off = devices.iter().any(|device| device.feature_transport_ready)
                        && crate::terminal_previews::sessions()
                            .iter()
                            .any(|info| info.exit_status.is_none());
                    let menu = crate::desk_menu::entries(
                        &devices,
                        &crate::device_links::config::app_settings(),
                        &crate::device_links::transfer_log::recent(),
                        hand_off,
                    );
                    let icon = crate::desk_menu::tray_icon(&devices);
                    tray.update(
                        active.into_iter().map(|(_, line)| line).collect(),
                        menu,
                        icon,
                        // DeskLink's own extension is in the top bar already.
                        crate::shell_dbus::extension_active(),
                    );
                }
                glib::ControlFlow::Continue
            })
        });
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
        if !self.is_registered() || !take_unreported_crash(&dir) {
            return;
        }
        let notification = gio::Notification::new("DeskLink stopped unexpectedly");
        notification.set_body(Some(&format!(
            "A crash report was saved in {}. Attaching it to a bug report helps fix this.",
            dir.display()
        )));
        self.send_notification(Some("crash-report"), &notification);
    }

    /// Connection History, a dialog (Settings -> Advanced opens the same).
    fn show_history(&self) {
        let window = self.main_window();
        window.present();
        if window.visible_dialog().is_some() {
            return;
        }
        crate::window::history::show_dialog(&window, &self.device_types(), None);
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

    /// The Settings page, or the computer-wide part as a dialog while no
    /// phone is paired.
    fn show_preferences(&self) {
        let window = self.main_window();
        window.present();
        if window.show_page("settings") || window.visible_dialog().is_some() {
            return;
        }
        if let Some(daemon) = self.daemon_handle() {
            crate::window::prefs::show_dialog(&window, &daemon);
        }
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
            ("Search Terminals", "Ctrl+F"),
            ("Connection History", "Ctrl+H"),
            ("Settings", "Ctrl+,"),
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

/// The palette, light and dark: (name, light, dark), from the DeskLink design
/// (`dl` / `dd`). libadwaita's own colours come first, then DeskLink's tokens
/// used by `style.css`.
const PALETTE: &[(&str, &str, &str)] = &[
    ("window_bg_color", "#f7f7f5", "#1c1c1f"),
    ("window_fg_color", "#18181b", "#f2f2f3"),
    ("view_bg_color", "#ffffff", "#26262a"),
    ("view_fg_color", "#18181b", "#f2f2f3"),
    ("card_bg_color", "#ffffff", "#26262a"),
    ("card_fg_color", "#18181b", "#f2f2f3"),
    ("headerbar_bg_color", "#f7f7f5", "#1c1c1f"),
    ("headerbar_fg_color", "#18181b", "#f2f2f3"),
    ("headerbar_backdrop_color", "#f7f7f5", "#1c1c1f"),
    ("sidebar_bg_color", "#f0f0ed", "#232326"),
    ("sidebar_fg_color", "#18181b", "#f2f2f3"),
    ("sidebar_backdrop_color", "#f0f0ed", "#232326"),
    ("popover_bg_color", "#ffffff", "#26262a"),
    ("popover_fg_color", "#18181b", "#f2f2f3"),
    ("dialog_bg_color", "#ffffff", "#26262a"),
    ("dialog_fg_color", "#18181b", "#f2f2f3"),
    ("accent_bg_color", "#3b63e6", "#7c95ff"),
    ("accent_fg_color", "#ffffff", "#0d0e10"),
    ("accent_color", "#2f55d4", "#98abff"),
    ("success_bg_color", "#0f9b72", "#3fd1a2"),
    ("success_fg_color", "#ffffff", "#0d0e10"),
    ("success_color", "#0b7a5a", "#3fd1a2"),
    ("warning_bg_color", "#c98a00", "#ffc24d"),
    ("warning_fg_color", "#ffffff", "#0d0e10"),
    ("warning_color", "#9a6a00", "#ffc24d"),
    ("error_bg_color", "#c93030", "#ff6b6b"),
    ("error_fg_color", "#ffffff", "#0d0e10"),
    ("error_color", "#c93030", "#ff6b6b"),
    ("destructive_bg_color", "#c93030", "#ff6b6b"),
    ("destructive_fg_color", "#ffffff", "#0d0e10"),
    ("destructive_color", "#c93030", "#ff6b6b"),
];

/// DeskLink's own tokens: (name, light, dark). The design's names are in the
/// comments. `text_faint` is darker than the design's `t3` (#9a9aa2 /
/// #71717a), which read at under 3:1: every text colour here meets 4.5:1 on
/// the surfaces it is used on (see the contrast test).
const TOKENS: &[(&str, &str, &str)] = &[
    // k: the soft fill of buttons, chips and icon squares.
    ("surface_2", "rgba(0,0,0,0.055)", "rgba(255,255,255,0.08)"),
    // l, b: hairlines and dashed edges.
    ("hairline", "rgba(0,0,0,0.07)", "rgba(255,255,255,0.07)"),
    (
        "hairline_strong",
        "rgba(0,0,0,0.14)",
        "rgba(255,255,255,0.16)",
    ),
    // t2, t3.
    ("text_dim", "#6b6b73", "#a1a1aa"),
    ("text_faint", "#6c6c74", "#8c8c95"),
    // m, o, ot, ob, obr: connected, and the other device in use.
    ("mint", "#0f9b72", "#3fd1a2"),
    ("coral", "#e5533d", "#ff7c66"),
    ("coral_text", "#c2412b", "#ff9a88"),
    ("coral_bg", "#fcefec", "#33221f"),
    (
        "coral_border",
        "rgba(229,83,61,0.25)",
        "rgba(255,124,102,0.3)",
    ),
    ("amber", "#9a6a00", "#ffc24d"),
    // av, avt: notification avatars.
    ("avatar_bg", "#e8ecf9", "#28305a"),
    ("avatar_fg", "#2f55d4", "#dfe5ff"),
    // rd, rdb: End Session, Unpair.
    ("danger", "#c93030", "#ff6b6b"),
    (
        "danger_bg",
        "rgba(211,58,58,0.09)",
        "rgba(255,107,107,0.12)",
    ),
    // sh: a card's edge and its soft shadow.
    ("card_edge", "rgba(0,0,0,0.06)", "rgba(255,255,255,0.06)"),
    ("card_shadow_near", "rgba(0,0,0,0.04)", "transparent"),
    // The terminal stays dark in both styles: it is the hero object.
    ("terminal_bg", "#0e0f11", "#0e0f11"),
    ("terminal_fg", "#d9dbe0", "#d9dbe0"),
    ("terminal_lines", "#8b9099", "#8b9099"),
    ("terminal_dim", "#7c818b", "#7c818b"),
    ("terminal_prompt", "#86d3a8", "#86d3a8"),
    ("terminal_coral", "#ff9a88", "#ff9a88"),
];

/// A token's colour in the current style, for widgets that draw themselves
/// (the link glyph, the battery ring). Unknown names are opaque grey.
pub(crate) fn token_color(name: &str) -> gtk::gdk::RGBA {
    let dark = adw::StyleManager::default().is_dark();
    PALETTE
        .iter()
        .chain(TOKENS)
        .find(|(token, _, _)| *token == name)
        .and_then(|(_, light, dark_value)| {
            gtk::gdk::RGBA::parse(if dark { *dark_value } else { *light }).ok()
        })
        .unwrap_or(gtk::gdk::RGBA::new(0.5, 0.5, 0.5, 1.0))
}

/// The CSS for one style. libadwaita 1.5 reads named colours; 1.6+ reads CSS
/// variables (`--window-bg-color`), so those are set too when present. In
/// high contrast libadwaita's own colours are kept and only our tokens are
/// set, with stronger edges.
fn palette_css(dark: bool, modern: bool, contrast: bool) -> String {
    let pick =
        |light: &'static str, dark_value: &'static str| if dark { dark_value } else { light };
    let mut css = String::new();
    let mut root = String::new();
    if !contrast {
        for (name, light, dark_value) in PALETTE.iter().filter(|(name, _, _)| !is_accent(name)) {
            let value = pick(light, dark_value);
            css.push_str(&format!("@define-color {name} {value};\n"));
            root.push_str(&format!("--{}: {value}; ", name.replace('_', "-")));
        }
    }
    for (name, light, dark_value) in TOKENS {
        let value = match *name {
            "hairline" | "hairline_strong" | "card_edge" if contrast => "alpha(currentColor, 0.6)",
            "text_dim" | "text_faint" if contrast => "alpha(currentColor, 0.85)",
            "card_shadow_near" if contrast => "transparent",
            _ => pick(light, dark_value),
        };
        css.push_str(&format!("@define-color {name} {value};\n"));
    }
    if modern && !root.is_empty() {
        css.push_str(&format!(":root {{ {root}}}\n"));
    }
    css
}

/// libadwaita's accent colours, which DeskLink keeps as its own blue.
fn is_accent(name: &str) -> bool {
    name.starts_with("accent_")
}

/// The accent part of the palette. It is loaded above the user's own
/// `gtk.css` as well as the system accent: a user stylesheet that sets
/// `--accent-bg-color` (as accent-colour tweaks do) otherwise turned
/// DeskLink's buttons, switches and links that colour while the rest of
/// the palette stayed DeskLink's. High contrast keeps libadwaita's.
fn accent_css(dark: bool, modern: bool, contrast: bool) -> String {
    if contrast {
        return String::new();
    }
    let mut css = String::new();
    let mut root = String::new();
    for (name, light, dark_value) in PALETTE.iter().filter(|(name, _, _)| is_accent(name)) {
        let value = if dark { dark_value } else { light };
        css.push_str(&format!("@define-color {name} {value};\n"));
        root.push_str(&format!("--{}: {value}; ", name.replace('_', "-")));
    }
    if modern {
        css.push_str(&format!(":root {{ {root}}}\n"));
    }
    css
}

/// Loads the palette for the current style and keeps it in step: light,
/// dark or high contrast, as the system or the Appearance setting changes.
fn install_brand_styles() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let modern = adw::minor_version() >= 6;
    let palette = gtk::CssProvider::new();
    // Above `style.css`, whose token fallbacks this replaces.
    gtk::style_context_add_provider_for_display(
        &display,
        &palette,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    let accent = gtk::CssProvider::new();
    gtk::style_context_add_provider_for_display(
        &display,
        &accent,
        gtk::STYLE_PROVIDER_PRIORITY_USER + 1,
    );
    let update = move |manager: &adw::StyleManager| {
        let (dark, contrast) = (manager.is_dark(), manager.is_high_contrast());
        palette.load_from_string(&palette_css(dark, modern, contrast));
        accent.load_from_string(&accent_css(dark, modern, contrast));
    };
    let manager = adw::StyleManager::default();
    manager.set_color_scheme(appearance_scheme(&load_appearance()));
    update(&manager);
    let update = std::rc::Rc::new(update);
    manager.connect_dark_notify({
        let update = update.clone();
        move |manager| update(manager)
    });
    manager.connect_high_contrast_notify(move |manager| update(manager));
}

/// Makes the bundled JetBrains Mono (SIL OFL 1.1, licence beside it in the
/// resources) available to this process, for `.mono` text. Fontconfig needs
/// a file, so the font is copied once into DeskLink's cache directory.
fn register_bundled_fonts() {
    #[link(name = "fontconfig")]
    extern "C" {
        fn FcConfigAppFontAddFile(
            config: *mut std::ffi::c_void,
            file: *const std::os::raw::c_uchar,
        ) -> std::os::raw::c_int;
    }
    let Ok(bytes) = gio::resources_lookup_data(
        "/derx06/desklink/com/fonts/JetBrainsMono.ttf",
        gio::ResourceLookupFlags::NONE,
    ) else {
        return;
    };
    let Some(dir) = dirs::cache_dir().map(|dir| dir.join("desklink")) else {
        return;
    };
    let path = dir.join("JetBrainsMono.ttf");
    let current = std::fs::metadata(&path).is_ok_and(|meta| meta.len() == bytes.len() as u64);
    if !current
        && (std::fs::create_dir_all(&dir).is_err() || std::fs::write(&path, &*bytes).is_err())
    {
        eprintln!("[UI] Could not cache the bundled monospace font");
        return;
    }
    let Ok(file) = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()) else {
        return;
    };
    // SAFETY: a null config means fontconfig's current one; the path is a
    // valid NUL-terminated string that outlives the call, which copies it.
    let added = unsafe { FcConfigAppFontAddFile(std::ptr::null_mut(), file.as_ptr().cast()) };
    if added == 0 {
        eprintln!("[UI] Fontconfig did not accept the bundled monospace font");
    }
}

/// Where the Appearance choice is kept: `~/.config/desklink/appearance`.
fn appearance_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("desklink").join("appearance"))
}

/// "system", "light" or "dark".
pub(crate) fn load_appearance() -> String {
    appearance_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| text.trim().to_string())
        .filter(|choice| matches!(choice.as_str(), "light" | "dark"))
        .unwrap_or_else(|| "system".to_string())
}

pub(crate) fn save_appearance(choice: &str) {
    if let Some(path) = appearance_path() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(error) = std::fs::write(&path, choice) {
            eprintln!("[UI] Could not save the appearance: {error}");
        }
    }
}

pub(crate) fn appearance_scheme(choice: &str) -> adw::ColorScheme {
    match choice {
        "light" => adw::ColorScheme::ForceLight,
        "dark" => adw::ColorScheme::ForceDark,
        _ => adw::ColorScheme::Default,
    }
}

/// `content` under a plain header bar, for a dialog or a pushed subpage.
pub(crate) fn with_header(content: &impl IsA<gtk::Widget>) -> adw::ToolbarView {
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(content));
    toolbar
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

/// Whether `dir` holds a crash report newer than the last one told, marking
/// it told.
fn take_unreported_crash(dir: &std::path::Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
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
        return false;
    };
    if reported_up_to.is_some_and(|reported| reported >= newest) {
        return false;
    }
    std::fs::write(&seen, b"").is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_crash_is_reported_once() {
        let dir = std::env::temp_dir().join(format!("desklink-crash-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!take_unreported_crash(&dir), "no crash");
        std::fs::write(dir.join("crash-1700000000.txt"), b"x").unwrap();
        assert!(take_unreported_crash(&dir));
        assert!(!take_unreported_crash(&dir), "told already");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// "#rrggbb" or "rgba(r,g,b,a)" as linear-light-ready 0..1 channels and alpha.
    fn parse(colour: &str) -> ([f64; 3], f64) {
        if let Some(hex) = colour.strip_prefix('#') {
            let channel =
                |at: usize| f64::from(u8::from_str_radix(&hex[at..at + 2], 16).unwrap()) / 255.0;
            return ([channel(0), channel(2), channel(4)], 1.0);
        }
        let inner = colour.trim_start_matches("rgba(").trim_end_matches(')');
        let parts: Vec<f64> = inner
            .split(',')
            .map(|part| part.trim().parse().unwrap())
            .collect();
        (
            [parts[0] / 255.0, parts[1] / 255.0, parts[2] / 255.0],
            parts[3],
        )
    }

    fn colour(name: &str, dark: bool) -> &'static str {
        let (_, light, dark_value) = PALETTE
            .iter()
            .chain(TOKENS)
            .find(|(token, _, _)| *token == name)
            .unwrap_or_else(|| panic!("no token {name}"));
        if dark {
            dark_value
        } else {
            light
        }
    }

    /// `name` painted over `under` (an opaque colour).
    fn composite(name: &str, under: [f64; 3], dark: bool) -> [f64; 3] {
        let (rgb, alpha) = parse(colour(name, dark));
        [0, 1, 2].map(|i| rgb[i] * alpha + under[i] * (1.0 - alpha))
    }

    fn luminance(rgb: [f64; 3]) -> f64 {
        let linear = |c: f64| {
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
    }

    fn contrast(a: [f64; 3], b: [f64; 3]) -> f64 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    /// WCAG AA: every text colour reads at 4.5:1 or more on each surface it
    /// is used on, light and dark.
    /// The accent goes in its own stylesheet, above the user's; the rest of
    /// the palette does not.
    #[test]
    fn the_accent_is_split_out_of_the_palette() {
        let palette = palette_css(false, true, false);
        let accent = accent_css(false, true, false);
        assert!(!palette.contains("accent_bg_color"), "{palette}");
        assert!(accent.contains("@define-color accent_bg_color #3b63e6;"));
        assert!(accent.contains("--accent-bg-color: #3b63e6;"));
        assert!(accent_css(true, true, false).contains("--accent-bg-color: #7c95ff;"));
        assert!(
            accent_css(false, true, true).is_empty(),
            "high contrast keeps its own"
        );
    }

    #[test]
    fn every_text_colour_meets_4_5_to_1_on_its_surfaces() {
        let text = [
            "window_fg_color",
            "text_dim",
            "text_faint",
            "accent_color",
            "coral_text",
            "danger",
        ];
        let surfaces = ["window_bg_color", "sidebar_bg_color", "card_bg_color"];
        for dark in [false, true] {
            let solid = |name: &str| parse(colour(name, dark)).0;
            let card = solid("card_bg_color");
            let mut pairs: Vec<(String, [f64; 3], String, [f64; 3])> = Vec::new();
            for fg in text {
                for bg in surfaces {
                    pairs.push((fg.into(), solid(fg), bg.into(), solid(bg)));
                }
            }
            // Buttons and chips: a soft fill on a card.
            let chip = composite("surface_2", card, dark);
            for fg in ["window_fg_color", "text_dim", "accent_color", "coral_text"] {
                pairs.push((fg.into(), solid(fg), "surface_2 on card".into(), chip));
            }
            for (fg, bg) in [
                ("accent_fg_color", "accent_bg_color"),
                ("destructive_fg_color", "destructive_bg_color"),
                ("avatar_fg", "avatar_bg"),
                ("window_fg_color", "coral_bg"),
                ("text_dim", "coral_bg"),
                ("coral_text", "coral_bg"),
                ("terminal_fg", "terminal_bg"),
                ("terminal_lines", "terminal_bg"),
                ("terminal_dim", "terminal_bg"),
                ("terminal_prompt", "terminal_bg"),
                ("terminal_coral", "terminal_bg"),
            ] {
                pairs.push((fg.into(), solid(fg), bg.into(), solid(bg)));
            }
            pairs.push((
                "danger".into(),
                solid("danger"),
                "danger_bg on card".into(),
                composite("danger_bg", card, dark),
            ));
            for (fg, fg_rgb, bg, bg_rgb) in pairs {
                let ratio = contrast(fg_rgb, bg_rgb);
                assert!(
                    ratio >= 4.5,
                    "{fg} on {bg} ({}) is {ratio:.2}:1",
                    if dark { "dark" } else { "light" }
                );
            }
        }
    }
}
