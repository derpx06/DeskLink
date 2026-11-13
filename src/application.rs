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

/// The key bindings (detailed action, accelerator), besides the page keys of
/// `window::PAGE_ACCELS`. The shortcuts dialog lists these; a test reads both.
pub(crate) const ACCELS: [(&str, &str); 10] = [
    ("win.dial-pad", "<control><shift>d"),
    ("app.quit", "<control>q"),
    ("app.shortcuts", "<control>question"),
    ("app.preferences", "<control>comma"),
    ("app.history", "<control>h"),
    ("win.pair-new", "<control>n"),
    ("win.send-files", "<control>o"),
    ("win.toggle-search", "<control>f"),
    ("win.palette", crate::window::PALETTE_ACCEL),
    ("window.close", "<control>w"),
];

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
        /// Whether this run already said that closing the window leaves
        /// DeskLink running without a tray.
        pub told_hidden: RefCell<bool>,
        /// When a close request last only minimised the window.
        pub last_minimized: std::cell::Cell<Option<std::time::Instant>>,
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
            crate::startup("application constructed");
        }
    }

    impl DesklinkApplication {
        /// Everything that must run in the primary instance only: the
        /// daemon (port 1717, beacons, relay client), the terminal server
        /// and the indicators. `constructed` runs in a second launch too,
        /// before GApplication knows it is not primary (I14); `startup`
        /// does not.
        fn start_primary(&self) {
            let obj = self.obj();
            // Just after GTK's own start-up (`parent_startup`).
            crate::startup("GTK started");
            self.daemon.replace(Some(DaemonHandle::start()));
            crate::startup("daemon started");
            // What this system can do, found out off the main thread.
            crate::platform::start();
            // The desk side of shared terminals (it leaves a running
            // instance's socket alone if this launch is a second one).
            crate::device_links::pty::desk::start_server();
            // Housekeeping that reads and rewrites files: off the main
            // thread, so the window does not wait for it.
            let _ = std::thread::Builder::new()
                .name("desklink-housekeeping".into())
                .spawn(|| {
                    // Older versions made these world-readable.
                    crate::device_links::private_fs::tighten_tree(
                        &crate::device_links::config::desklink_config_root(),
                    );
                    if let Some(dir) = crate::crash_dir() {
                        crate::device_links::private_fs::tighten_tree(&dir);
                    }
                    crate::device_links::private_fs::tighten_tree(
                        &gtk::glib::user_cache_dir().join("desklink"),
                    );
                    // Where the phones were last seen (older versions made
                    // this folder and endpoints.json world-readable).
                    if let Some(state) = dirs::state_dir().or_else(dirs::data_local_dir) {
                        crate::device_links::private_fs::tighten_tree(&state.join("desklink"));
                    }
                    crate::device_links::connection_log::prune();
                    crate::device_links::gallery::trim();
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
            obj.watch_resume();
            crate::startup("indicators and network watch ready");
            for (action, accel) in ACCELS {
                obj.set_accels_for_action(action, &[accel]);
            }
            for (page, accel) in crate::window::PAGE_ACCELS {
                obj.set_accels_for_action(&format!("win.page-shortcut('{page}')"), &[accel]);
            }
        }
    }

    impl ApplicationImpl for DesklinkApplication {
        fn startup(&self) {
            self.parent_startup();
            self.start_primary();
            crate::device_links::dnd::init();
            // Registered now (startup runs only in the primary instance), so
            // the notice is really shown before the crash is marked told.
            self.obj().report_previous_crash();
            register_bundled_fonts();
            install_brand_styles();
            crate::startup("styles and fonts ready");
            // A logout, a reboot or `kill` quits like File > Quit does, so
            // glide gives the pointer back on the way out.
            // (Not GLib's signal sources: they run on the main loop, so a
            // wedged main thread could never be ended by `kill`.)
            if let Err(error) = crate::signals::watch(
                &[libc::SIGTERM, libc::SIGINT, libc::SIGHUP],
                crate::signals::GRACE,
                crate::signals::Real,
            ) {
                eprintln!("[DeskLink] Could not watch for quit signals: {error}");
            }
        }

        fn shutdown(&self) {
            // Do Not Disturb DeskLink set on a phone's behalf is put back
            // first, so nothing slower below can cost it.
            crate::device_links::dnd::quit_main();
            // A "Sending…" line must not outlive the process: its Cancel would
            // start a new one that can do nothing.
            if let Some(daemon) = self.obj().daemon_handle() {
                for device in daemon.devices() {
                    self.obj()
                        .withdraw_notification(&format!("transfers-{}", device.id));
                }
            }
            // The pointer is never left on a phone: bounded, so a compositor
            // that does not answer cannot hold the app open.
            crate::device_links::glide::shutdown();
            // Accessibility DeskLink turned on goes back as it was.
            crate::device_links::webrtc::text_focus::release_accessibility();
            // What was being typed, and whatever the vault writer still
            // holds, is saved on the way out.
            for window in self.obj().windows() {
                if let Ok(window) = window.downcast::<DesklinkWindow>() {
                    window.flush_unsaved();
                }
            }
            crate::device_links::vault::flush();
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
            window.connect_map(|window| {
                crate::startup("window mapped");
                first_paint(window);
            });
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
            crate::startup("connected to the session bus");
            self.parent_dbus_register(connection, object_path)?;
            crate::shell_dbus::export(&self.obj(), connection);
            crate::phone_mpris::start(&self.obj());
            crate::search_provider::export(&self.obj(), connection);
            crate::kde::export(&self.obj(), connection);
            crate::startup("D-Bus objects exported");
            Ok(())
        }
    }

    impl GtkApplicationImpl for DesklinkApplication {}
    impl AdwApplicationImpl for DesklinkApplication {}
}

/// `DESKLINK_DEBUG`: the moment the window's first frame is on screen
/// (the frame clock's first `after-paint`), the number a launch is judged by.
fn first_paint(window: &DesklinkWindow) {
    let Some(clock) = window.frame_clock().filter(|_| crate::debug_logging()) else {
        return;
    };
    let handler = std::rc::Rc::new(std::cell::Cell::new(None));
    let once = handler.clone();
    handler.set(Some(clock.connect_after_paint(move |clock| {
        crate::startup("first frame painted");
        if let Some(id) = once.take() {
            clock.disconnect(id);
        }
    })));
}

/// Which phones' "approve it at this computer" notices to post and which to
/// take down, given the phones waiting now and those already announced.
fn approval_changes(
    waiting: &[String],
    shown: &mut std::collections::HashSet<String>,
) -> (Vec<String>, Vec<String>) {
    let gone: Vec<String> = shown
        .iter()
        .filter(|device| !waiting.contains(device))
        .cloned()
        .collect();
    for device in &gone {
        shown.remove(device);
    }
    let fresh = waiting
        .iter()
        .filter(|device| shown.insert((*device).clone()))
        .cloned()
        .collect();
    (fresh, gone)
}

/// The phone Glide's "Capture Input" dialog is up for, if it is.
fn glide_waiting(asking: bool, phone: Option<String>) -> Vec<String> {
    phone.filter(|_| asking).into_iter().collect()
}

/// The Bucket questions (a phone asked, the desk has not turned it on) not
/// yet told about: each is returned once. `told` forgets a question once it is
/// gone, so a phone that asks again later is told again.
fn new_bucket_questions(
    offers: &[crate::device_links::folders::Offer],
    told: &mut std::collections::HashSet<String>,
) -> Vec<crate::device_links::folders::Offer> {
    told.retain(|device| {
        offers
            .iter()
            .any(|offer| offer.bucket && &offer.device == device)
    });
    offers
        .iter()
        .filter(|offer| offer.bucket)
        .filter(|offer| told.insert(offer.device.clone()))
        .cloned()
        .collect()
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
        // "Turn On" on the notification that says a phone asked for a Bucket.
        let turn_on_bucket_action = gio::ActionEntry::builder("bucket-turn-on")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(device) = target.and_then(|target| target.get::<String>()) {
                    // Any process on the session bus can activate this: it
                    // only answers an ask the phone actually made.
                    if !bucket_ask_pending(&crate::device_links::folders::offers(), &device) {
                        return;
                    }
                    crate::device_links::folders::turn_on_bucket(&device);
                    app.withdraw_notification(&format!("bucket-ask-{device}"));
                }
            })
            .build();
        // "Cancel" on the walk-away warning. It only ever keeps the computer
        // unlocked for one absence; nothing outward.
        let walk_away_cancel_action = gio::ActionEntry::builder("walk-away-cancel")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(device) = target.and_then(|target| target.get::<String>()) {
                    crate::device_links::walk_away::cancel(&device);
                    app.withdraw_notification(&format!("walk-away-{device}"));
                }
            })
            .build();
        // "Cancel" on the file transfer notification: every transfer with that
        // phone that is running or waiting (never the Bucket's own sync).
        let cancel_transfers_action = gio::ActionEntry::builder("cancel-transfers")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                let (Some(device), Some(daemon)) = (
                    target.and_then(|target| target.get::<String>()),
                    app.daemon_handle(),
                ) else {
                    return;
                };
                for row in crate::device_links::webrtc::transfer_board::rows() {
                    if row.device_id == device {
                        daemon.send(crate::device_links::daemon::DaemonCommand::CancelTransfer(
                            device.clone(),
                            row.id,
                        ));
                    }
                }
            })
            .build();
        // "Open" and "Show in Files" on a received file's notification. Any
        // process on the session bus can activate them, so only a file that is
        // in the received history opens.
        let open_received_action = gio::ActionEntry::builder("open-received")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(path) = target.and_then(|target| target.get::<String>()) {
                    app.open_received(&path, false);
                }
            })
            .build();
        let show_received_action = gio::ActionEntry::builder("show-received")
            .parameter_type(Some(glib::VariantTy::STRING))
            .activate(move |app: &Self, _, target| {
                if let Some(path) = target.and_then(|target| target.get::<String>()) {
                    app.open_received(&path, true);
                }
            })
            .build();
        // The "closed to the background" notification's click: with no tray
        // to reopen from, the notification is the way back.
        let show_window_action = gio::ActionEntry::builder("show-window")
            .activate(move |app: &Self, _, _| app.main_window().present())
            .build();
        let history_action = gio::ActionEntry::builder("history")
            .activate(move |app: &Self, _, _| app.show_history())
            .build();
        // (device, action, number): a call button in a notification. Answer,
        // decline, end and the like go straight to the phone; a *dial* never
        // does: anything on the session bus can activate this action, so
        // starting a call is asked first, in the window, like every other dial.
        let call_action = gio::ActionEntry::builder("call")
            .parameter_type(Some(glib::VariantTy::new("(sss)").expect("valid type")))
            .activate(move |app: &Self, _, target| {
                let Some((device, action, number)) =
                    target.and_then(|target| target.get::<(String, String, String)>())
                else {
                    return;
                };
                match call_action_kind(&action) {
                    CallAction::Dial => app.dial_with(&number, &device),
                    CallAction::Message => app.message_with(&number, &device),
                    CallAction::OnTheCall => {
                        if let Some(daemon) = app.daemon_handle() {
                            daemon.send(crate::device_links::daemon::DaemonCommand::Telephony(
                                device, action, None,
                            ));
                        }
                    }
                    CallAction::Refused => {}
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
        crate::send_to::add_direct_actions(self);
        self.add_action_entries([
            quit_action,
            about_action,
            preferences_action,
            shortcuts_action,
            end_sessions_action,
            lock_hand_off_action,
            history_action,
            show_window_action,
            open_bucket_action,
            turn_on_bucket_action,
            walk_away_cancel_action,
            cancel_transfers_action,
            open_received_action,
            show_received_action,
            call_action,
            dial_action,
        ]);
    }

    /// Like [`Self::dial_with`], for a text message.
    pub(crate) fn message_with(&self, number: &str, phone: &str) {
        let window = self.main_window();
        window.present();
        crate::window::confirm_message(&window, number, Some(phone));
    }

    /// Like [`Self::dial`], with the phone that must place the call.
    pub(crate) fn dial_with(&self, number: &str, phone: &str) {
        let window = self.main_window();
        window.present();
        crate::window::confirm_dial(&window, number, Some(phone));
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
        // find-my-phone and errors, and reopening is instant. Where nothing
        // could bring the window back (no tray, no notification service) it
        // minimises instead, so it is never hidden for good.
        window.set_hide_on_close(true);
        let app = self.downgrade();
        window.connect_close_request(move |window| {
            let Some(app) = app.upgrade() else {
                return glib::Propagation::Proceed;
            };
            let way_back = app.tray_hosted() || crate::shell_dbus::extension_active();
            let repeated = app
                .imp()
                .last_minimized
                .get()
                .is_some_and(|at| at.elapsed() < QUIT_ON_SECOND_CLOSE);
            match close_action(way_back, crate::platform::works("notifications"), repeated) {
                CloseAction::Minimize => {
                    app.imp()
                        .last_minimized
                        .set(Some(std::time::Instant::now()));
                    window.minimize();
                    return glib::Propagation::Stop;
                }
                // Closing twice in a row where the window can neither hide
                // nor minimise (a tiling WM) is a request to quit.
                CloseAction::Quit => {
                    app.quit();
                    return glib::Propagation::Proceed;
                }
                CloseAction::HideAndTell => {
                    // No tray to show where it went: said each session.
                    if take_hide_notice(&mut app.imp().told_hidden.borrow_mut()) {
                        let note = gio::Notification::new("DeskLink is still running");
                        note.set_body(Some(
                            "There is no tray on this desktop, so DeskLink stays connected in \
                             the background. Click here, or start DeskLink from your launcher, \
                             to open it again.",
                        ));
                        note.set_default_action("app.show-window");
                        app.send_notification(Some("desklink-background"), &note);
                    }
                }
                CloseAction::Hide => {
                    // Once, ever: closing did not quit, and nothing said so (J16).
                    if take_first_hide(&crate::device_links::config::desklink_config_root()) {
                        let note = gio::Notification::new("DeskLink is still running");
                        note.set_body(Some(
                            "Closing the window keeps DeskLink connected in the background. \
                             Quit it from the tray or the app menu.",
                        ));
                        app.send_notification(Some("desklink-background"), &note);
                    }
                }
            }
            glib::Propagation::Proceed
        });
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
        let mut asked = std::collections::HashSet::<String>::new();
        let mut approving = std::collections::HashSet::<String>::new();
        let mut glide_asking = std::collections::HashSet::<String>::new();
        let mut notices = crate::transfer_notice::Notices::default();
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
                // A phone that asks for a Bucket is answered with one click
                // on a notification, once per ask (turning it on stays the
                // person's choice).
                for offer in
                    new_bucket_questions(&crate::device_links::folders::offers(), &mut asked)
                {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|device| device.id == offer.device)
                        .map_or(offer.name.clone(), |device| device.name);
                    let notification =
                        gio::Notification::new(&format!("{phone} wants to share a Bucket"));
                    notification.set_body(Some(
                        "One place for files, pictures, text and links on both devices.",
                    ));
                    notification.add_button_with_target_value(
                        "Turn On",
                        "app.bucket-turn-on",
                        Some(&offer.device.to_variant()),
                    );
                    notification.set_default_action_and_target_value(
                        "app.open-bucket",
                        Some(&offer.device.to_variant()),
                    );
                    app.send_notification(
                        Some(&format!("bucket-ask-{}", offer.device)),
                        &notification,
                    );
                }
                // A phone's screen request waits for an answer in GNOME's own dialog,
                // which says nothing about DeskLink: say where to look, and take
                // the notice down the moment it is answered.
                let waiting = crate::device_links::webrtc::coordinator::phones_awaiting_approval();
                let (to_post, to_withdraw) = approval_changes(&waiting, &mut approving);
                for device in to_post {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|known| known.id == device)
                        .map_or_else(|| "A phone".to_string(), |known| known.name);
                    let notification =
                        gio::Notification::new(&format!("{phone} wants to show this screen"));
                    notification.set_body(Some("Approve it in the window that just opened."));
                    notification.set_priority(gio::NotificationPriority::High);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some(&format!("screen-approve-{device}")), &notification);
                }
                for device in to_withdraw {
                    app.withdraw_notification(&format!("screen-approve-{device}"));
                }
                // A share that could not start: said here as well as on the phone.
                for (device, reason) in
                    crate::device_links::webrtc::coordinator::take_share_failures()
                {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|known| known.id == device)
                        .map_or_else(|| "the phone".to_string(), |known| known.name);
                    let notification = gio::Notification::new(&format!(
                        "This screen couldn't be shown on {phone}"
                    ));
                    notification.set_body(Some(&reason));
                    notification.set_priority(gio::NotificationPriority::High);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some(&format!("screen-failed-{device}")), &notification);
                }
                // Chrome (or an Electron app) was open when accessibility came
                // on: it cannot report text fields until restarted. Said once.
                if crate::device_links::webrtc::text_focus::take_restart_hint() {
                    let notification =
                        gio::Notification::new("Restart Chrome once for the phone keyboard");
                    notification.set_body(Some(
                        "Chrome and apps like VS Code that were already open can't tell DeskLink \
                         when you click a text field. Restart them once and the phone keyboard \
                         comes up for their text fields too.",
                    ));
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some("text-focus-restart"), &notification);
                }
                // Glide is about to put GNOME's "Capture Input" dialog up: say
                // so, and take the notice down the moment it is answered.
                let glide = crate::device_links::glide::status();
                let (to_post, to_withdraw) = approval_changes(
                    &glide_waiting(
                        glide.asking,
                        crate::device_links::config::app_settings()
                            .glide
                            .map(|setting| setting.device),
                    ),
                    &mut glide_asking,
                );
                for device in to_post {
                    let phone = daemon
                        .devices()
                        .into_iter()
                        .find(|known| known.id == device)
                        .map_or_else(|| "your phone".to_string(), |known| known.name);
                    let notification = gio::Notification::new(&format!(
                        "DeskLink is asking to move your mouse to {phone} (Glide)"
                    ));
                    notification.set_body(Some("Approve it in the window that just opened."));
                    notification.set_priority(gio::NotificationPriority::High);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    app.send_notification(Some(&format!("glide-ask-{device}")), &notification);
                }
                for device in to_withdraw {
                    app.withdraw_notification(&format!("glide-ask-{device}"));
                }
                {
                    let devices = daemon.devices();
                    app.walk_away_step(&devices);
                    app.transfer_notices(&mut notices, &devices);
                    let settings = crate::device_links::config::app_settings();
                    let paired = || devices.iter().filter(|device| device.trusted);
                    if let Some(problem) = crate::device_links::dnd::step(
                        &paired()
                            .filter(|d| settings.shares_dnd_with(&d.id))
                            .map(|d| d.id.clone())
                            .collect(),
                        &paired()
                            .filter(|d| d.feature_transport_ready)
                            .map(|d| d.id.clone())
                            .collect(),
                    ) {
                        let notification = gio::Notification::new("Do Not Disturb was not shared");
                        notification.set_body(Some(&problem));
                        app.send_notification(Some("dnd-problem"), &notification);
                    }
                }
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
                        crate::desk_menu::tooltip(&devices),
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

    /// Walk-away lock: the look at every phone, and what it asks for (a
    /// warning with Cancel, then the lock). See `walk_away`.
    fn walk_away_step(&self, devices: &[crate::device_links::device::DeviceView]) {
        use crate::device_links::walk_away::{network_is_up, observe, Seen, Step};
        let settings = crate::device_links::config::app_settings();
        for device in devices.iter().filter(|device| device.trusted) {
            let seen = Seen::of(
                device,
                settings.locks_when_away(&device.id),
                crate::device_links::webrtc::coordinator::is_dozing(&device.id),
                network_is_up(gio::NetworkMonitor::default().connectivity()),
            );
            let id = format!("walk-away-{}", device.id);
            match observe(&device.id, seen) {
                Some(Step::Warn) => {
                    let notification =
                        gio::Notification::new("Locking this computer in 10 seconds");
                    notification.set_body(Some(&format!(
                        "{} has been out of reach. Cancel to stay unlocked.",
                        glib::markup_escape_text(&device.name)
                    )));
                    notification.set_priority(gio::NotificationPriority::Urgent);
                    notification.add_button_with_target_value(
                        "Cancel",
                        "app.walk-away-cancel",
                        Some(&device.id.to_variant()),
                    );
                    self.send_notification(Some(&id), &notification);
                }
                Some(Step::ClearWarning) => self.withdraw_notification(&id),
                Some(Step::Lock) => {
                    self.withdraw_notification(&id);
                    // D-Bus with a timeout: not on the main loop.
                    let _ = std::thread::Builder::new()
                        .name("desklink-walk-away".into())
                        .spawn(crate::device_links::daemon::packet_handler::lock_desktop);
                }
                None => {}
            }
        }
    }

    /// The "DeskLink" notification for files moving (see `transfer_notice`):
    /// progress while they move, how they ended after.
    fn transfer_notices(
        &self,
        notices: &mut crate::transfer_notice::Notices,
        devices: &[crate::device_links::device::DeviceView],
    ) {
        use crate::device_links::webrtc::transfer_board as board;
        use crate::transfer_notice::Action;
        // Together, under one lock; the Bucket's own sync is not here (its
        // module announces it).
        let (rows, news, seen) = board::snapshot(notices.seen());
        notices.set_seen(seen);
        notices.set_lanes(board::lanes());
        let name_of = |id: &str| {
            devices
                .iter()
                .find(|device| device.id == id)
                .map_or_else(|| "your phone".to_string(), |device| device.name.clone())
        };
        let actions = notices.look(std::time::Instant::now(), &rows, &news, &name_of, &|id| {
            board::note(id)
        });
        // Do Not Disturb on: the desktop keeps banners back, and a progress
        // line every few seconds is not worth interrupting for.
        let quiet = crate::device_links::dnd::desk_state() == Some(true);
        for action in actions {
            match action {
                Action::Progress { device, body } if !quiet => {
                    let notification = gio::Notification::new("DeskLink");
                    notification.set_body(Some(&body));
                    notification.set_priority(gio::NotificationPriority::Low);
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    notification.add_button_with_target_value(
                        "Cancel",
                        "app.cancel-transfers",
                        Some(&device.to_variant()),
                    );
                    notification.set_default_action_and_target_value(
                        "app.open-bucket",
                        Some(&device.to_variant()),
                    );
                    self.send_notification(Some(&format!("transfers-{device}")), &notification);
                }
                Action::Progress { .. } => {}
                Action::Ended { .. } if quiet => {}
                Action::Ended {
                    device,
                    title,
                    path,
                } => {
                    let notification = gio::Notification::new("DeskLink");
                    notification.set_body(Some(&title));
                    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
                    notification.set_default_action_and_target_value(
                        "app.open-bucket",
                        Some(&device.to_variant()),
                    );
                    if let Some(path) = path {
                        let target = path.to_string_lossy().to_variant();
                        notification.add_button_with_target_value(
                            "Open",
                            "app.open-received",
                            Some(&target),
                        );
                        notification.add_button_with_target_value(
                            "Show in Files",
                            "app.show-received",
                            Some(&target),
                        );
                    }
                    self.send_notification(Some(&format!("transfers-{device}")), &notification);
                }
                Action::Clear { device } => {
                    self.withdraw_notification(&format!("transfers-{device}"));
                }
            }
        }
    }

    /// Opens a received file, or the folder it is in: only one the received
    /// history knows.
    fn open_received(&self, path: &str, show_folder: bool) {
        let path = std::path::PathBuf::from(path);
        let known = crate::device_links::transfer_log::recent()
            .iter()
            .any(|file| file.path == path);
        if !known || !path.exists() {
            return;
        }
        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
        let window = self.active_window();
        if show_folder {
            launcher.open_containing_folder(window.as_ref(), gio::Cancellable::NONE, |_| {});
        } else {
            launcher.launch(window.as_ref(), gio::Cancellable::NONE, |_| {});
        }
    }

    /// A resume from sleep is a sign of the network changing under the
    /// links: after the network has had two seconds to come back, the same
    /// recovery runs as for a network change.
    fn watch_resume(&self) {
        let app = self.downgrade();
        crate::device_links::logind::watch_resume(move || {
            let app = app.clone();
            glib::timeout_add_seconds_local_once(2, move || {
                if let Some(daemon) = app.upgrade().and_then(|app| app.daemon_handle()) {
                    daemon.send(crate::device_links::daemon::DaemonCommand::NetworkChanged);
                }
            });
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
            ("Home, Files, Notes & clipboard", "Ctrl+1 to 3"),
            ("Notifications, Terminals, Contacts", "Ctrl+4 to 6"),
            ("Pair another phone", "Ctrl+N"),
            ("Send files", "Ctrl+O"),
            ("Find anything", "Ctrl+K"),
            ("Search terminals", "Ctrl+F"),
            ("Connection history", "Ctrl+H"),
            ("Settings", "Ctrl+,"),
            ("Keyboard shortcuts", "Ctrl+?"),
            ("Close window", "Ctrl+W"),
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
            .title("Keyboard shortcuts")
            .content_width(420)
            .child(&with_header(&content))
            .build()
            .present(Some(&parent));
    }
}

/// The palette, light and dark: (name, light, dark), from the DeskLink design
/// (`dl` / `dd`). libadwaita's own colours come first, then DeskLink's tokens
/// used by `style.css`. The neutrals (window, header bar, sidebar, cards,
/// popovers, dialogs) are the UI kit's `dl_bg`, `dl_sidebar` and `dl_surface`
/// (`UI-REDESIGN.md` 2.1), so a window not redesigned yet sits on the same
/// greys as one that is.
const PALETTE: &[(&str, &str, &str)] = &[
    ("window_bg_color", "#f7f7f4", "#131315"),
    ("window_fg_color", "#18181b", "#f2f2f3"),
    ("view_bg_color", "#ffffff", "#1b1b1e"),
    ("view_fg_color", "#18181b", "#f2f2f3"),
    ("card_bg_color", "#ffffff", "#1b1b1e"),
    ("card_fg_color", "#18181b", "#f2f2f3"),
    ("headerbar_bg_color", "#f7f7f4", "#131315"),
    ("headerbar_fg_color", "#18181b", "#f2f2f3"),
    ("headerbar_backdrop_color", "#f7f7f4", "#131315"),
    ("sidebar_bg_color", "#efefeb", "#17171a"),
    ("sidebar_fg_color", "#18181b", "#f2f2f3"),
    ("sidebar_backdrop_color", "#efefeb", "#17171a"),
    // An inner split view's list (Notes): between the sidebar and the window,
    // in the same warm greys, not libadwaita's cool default.
    ("secondary_sidebar_bg_color", "#f3f3f0", "#151518"),
    ("secondary_sidebar_fg_color", "#18181b", "#f2f2f3"),
    ("secondary_sidebar_backdrop_color", "#f3f3f0", "#151518"),
    ("popover_bg_color", "#ffffff", "#1b1b1e"),
    ("popover_fg_color", "#18181b", "#f2f2f3"),
    ("dialog_bg_color", "#ffffff", "#1b1b1e"),
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
    ("coral_text", "#b03a25", "#ff9a88"),
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
        .map(|(_, light, dark_value)| if dark { *dark_value } else { *light })
        .or_else(|| crate::window::ui_kit::tokens::value(name, dark, false))
        .and_then(|value| gtk::gdk::RGBA::parse(value).ok())
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
    // The UI kit's `dl_*` tokens (docs/UI-REDESIGN.md 2.1).
    css.push_str(&crate::window::ui_kit::tokens::css(dark, modern, contrast));
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

/// Whether the running GTK parses CSS custom properties (`:root { --x: }`,
/// GTK 4.16 with libadwaita 1.6). Older ones (Ubuntu 24.04: GTK 4.14,
/// libadwaita 1.5) get the `@define-color` names alone.
pub(crate) fn modern_css() -> bool {
    adw::minor_version() >= 6
}

/// Loads the palette for the current style and keeps it in step: light,
/// dark or high contrast, as the system or the Appearance setting changes.
fn install_brand_styles() {
    let Some(display) = gtk::gdk::Display::default() else {
        return;
    };
    let modern = modern_css();
    let palette = gtk::CssProvider::new();
    // Above `style.css`, whose token fallbacks this replaces.
    gtk::style_context_add_provider_for_display(
        &display,
        &palette,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    // The UI kit's classes, after `style.css`; their colours are the
    // palette's `dl_*` names.
    let kit = gtk::CssProvider::new();
    kit.load_from_resource("/derx06/desklink/com/ui-kit.css");
    gtk::style_context_add_provider_for_display(
        &display,
        &kit,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );
    // The Files area's look, after the kit's.
    let files = gtk::CssProvider::new();
    files.load_from_resource("/derx06/desklink/com/files-ui.css");
    gtk::style_context_add_provider_for_display(
        &display,
        &files,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
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
    // Sound is a use of this computer too: its sound or microphone going to
    // the phone, or the phone's playing here. With the window closed the
    // top bar and the tray are all that say so.
    let sound = device.sound.as_ref().and_then(|sound| {
        match (sound.sending.as_deref(), sound.hearing.as_deref()) {
            (Some("mic"), _) => Some("sending its microphone"),
            (Some(_), _) => Some("sending its sound"),
            (None, Some(_)) if sound.hearing_live => Some("playing the phone's sound"),
            _ => None,
        }
    });
    // An Insert waiting on the phone (a photo, a scan) is a use too.
    let capture = device
        .capture
        .as_ref()
        .map(|capture| match capture.kind.as_str() {
            "scan" => "waiting for a scan",
            "text" => "waiting for text from a picture",
            "qr" => "waiting for a QR code",
            _ => "waiting for a photo",
        });
    let mut parts: Vec<String> = Vec::new();
    parts.extend(screen.map(str::to_string));
    if let Some(terminals) = terminals {
        parts.push(if screen.is_some() {
            terminals
        } else {
            format!("using {terminals}")
        });
    }
    parts.extend(sound.map(str::to_string));
    parts.extend(capture.map(str::to_string));
    if parts.is_empty() {
        return None;
    }
    Some(format!("{} — {}", device.name, parts.join(", ")))
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
        .filter(|entry| crate::is_crash_report(&entry.file_name().to_string_lossy()))
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
    fn ask(device: &str, bucket: bool) -> crate::device_links::folders::Offer {
        crate::device_links::folders::Offer {
            device: device.into(),
            id: "bucket".into(),
            name: "Bucket".into(),
            bucket,
        }
    }

    /// A phone asking for a Bucket is told about once; asking again after the
    /// question went away is a new question; a folder offer is not a Bucket.
    #[test]
    fn a_bucket_question_is_notified_once_per_ask() {
        let mut told = std::collections::HashSet::new();
        let first = new_bucket_questions(&[ask("p1", true), ask("p2", false)], &mut told);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].device, "p1");
        // Still asked on the next tick: not again.
        assert!(new_bucket_questions(&[ask("p1", true)], &mut told).is_empty());
        // Answered (the offer is gone), then asked again later.
        assert!(new_bucket_questions(&[], &mut told).is_empty());
        assert_eq!(new_bucket_questions(&[ask("p1", true)], &mut told).len(), 1);
    }

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

    /// An Insert waiting on the phone shows in the indicator too, so the tray
    /// and the notification offer to end it.
    #[test]
    fn the_session_line_includes_a_capture_waiting_on_the_phone() {
        use crate::device_links::device::{CaptureUse, DeviceView};
        let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        device.capture = Some(CaptureUse {
            kind: "scan".into(),
            receiving: false,
        });
        assert_eq!(
            session_line(&device).as_deref(),
            Some("Pixel — waiting for a scan")
        );
        device.capture = Some(CaptureUse {
            kind: "photo".into(),
            receiving: true,
        });
        assert_eq!(
            session_line(&device).as_deref(),
            Some("Pixel — waiting for a photo")
        );
    }

    /// The indicator also says when this computer's sound or microphone goes
    /// to a phone, or the phone's sound plays here (not only the screen and
    /// terminals), so a closed window does not hide it.
    #[test]
    fn the_session_line_includes_sound() {
        use crate::device_links::device::{DeviceView, SoundUse};
        let info = crate::device_links::device_info::DeviceInfo::local("p".into(), "Pixel".into());
        let mut device = DeviceView::from_info(&info, "10.0.0.2".into(), true);
        assert_eq!(session_line(&device), None);
        device.sound = Some(SoundUse {
            sending: Some("mic".into()),
            hearing: None,
            hearing_live: false,
        });
        assert_eq!(
            session_line(&device).as_deref(),
            Some("Pixel — sending its microphone")
        );
        device.sound = Some(SoundUse {
            sending: Some("system".into()),
            hearing: None,
            hearing_live: false,
        });
        assert_eq!(
            session_line(&device).as_deref(),
            Some("Pixel — sending its sound")
        );
        device.sound = Some(SoundUse {
            sending: None,
            hearing: Some("system".into()),
            hearing_live: true,
        });
        device.terminal_sessions_attached = 2;
        assert_eq!(
            session_line(&device).as_deref(),
            Some("Pixel — using 2 terminals, playing the phone's sound")
        );
        // Asked for but not flowing yet: nothing to indicate.
        device.terminal_sessions_attached = 0;
        device.sound = Some(SoundUse {
            sending: None,
            hearing: Some("system".into()),
            hearing_live: false,
        });
        assert_eq!(session_line(&device), None);
    }
}

/// What closing the main window does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CloseAction {
    /// Hide it; a tray (or the top-bar extension) shows DeskLink is running.
    Hide,
    /// Hide it and say, once per run, how to get it back.
    HideAndTell,
    /// Nothing could bring a hidden window back: keep it, minimised.
    Minimize,
    /// The same, closed again right after: really quit (a window manager that
    /// cannot minimise would otherwise make closing impossible).
    Quit,
}

/// How soon a second close request after a minimise quits.
const QUIT_ON_SECOND_CLOSE: std::time::Duration = std::time::Duration::from_secs(2);

/// Closing the window, given whether a tray (or the extension) is showing
/// DeskLink and whether a notification can be shown.
pub(crate) fn close_action(way_back: bool, can_notify: bool, repeated: bool) -> CloseAction {
    match (way_back, can_notify, repeated) {
        (true, _, _) => CloseAction::Hide,
        (false, true, _) => CloseAction::HideAndTell,
        (false, false, false) => CloseAction::Minimize,
        (false, false, true) => CloseAction::Quit,
    }
}

/// True the first time it is called for `told` (and remembers it).
fn take_hide_notice(told: &mut bool) -> bool {
    !std::mem::replace(told, true)
}

/// Whether this is the first time the window was closed (and remembers it):
/// the note about DeskLink staying in the background is shown once.
pub(crate) fn take_first_hide(dir: &std::path::Path) -> bool {
    let marker = dir.join(".background-told");
    if marker.exists() {
        return false;
    }
    let _ = std::fs::create_dir_all(dir);
    std::fs::write(&marker, b"").is_ok()
}

#[cfg(test)]
mod close_tests {
    use super::*;

    #[test]
    fn a_window_is_never_hidden_with_no_way_to_get_it_back() {
        assert_eq!(close_action(true, true, false), CloseAction::Hide);
        assert_eq!(close_action(true, false, false), CloseAction::Hide);
        assert_eq!(close_action(false, true, false), CloseAction::HideAndTell);
        assert_eq!(close_action(false, false, false), CloseAction::Minimize);
    }

    #[test]
    fn closing_twice_where_a_window_cannot_minimise_quits() {
        assert_eq!(close_action(false, false, true), CloseAction::Quit);
        // A tray or a notification service means the first close is enough.
        assert_eq!(close_action(false, true, true), CloseAction::HideAndTell);
        assert_eq!(close_action(true, false, true), CloseAction::Hide);
        assert_eq!(QUIT_ON_SECOND_CLOSE, std::time::Duration::from_secs(2));
    }

    #[test]
    fn the_no_tray_notice_comes_once_per_run() {
        let mut told = false;
        assert!(take_hide_notice(&mut told));
        assert!(!take_hide_notice(&mut told));
    }
}

#[cfg(test)]
mod first_hide_tests {
    #[test]
    fn the_background_note_is_for_the_first_close_only() {
        let dir = std::env::temp_dir().join(format!("desklink-hide-{}", uuid::Uuid::new_v4()));
        assert!(super::take_first_hide(&dir));
        assert!(!super::take_first_hide(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod primary_only_tests {
    /// I14: `constructed` runs in a second launch too, so it must not start
    /// the daemon (port 1717, beacons, relay client) or the terminal server.
    #[test]
    fn a_second_launch_does_not_start_the_daemon() {
        let source = include_str!("application.rs");
        let start = source.find("fn constructed(&self)").unwrap();
        let end = start
            + source[start..]
                .find("fn startup")
                .map_or(600, |i| i.min(600));
        let constructed = &source[start..end];
        assert!(
            !constructed.contains("DaemonHandle::start"),
            "{constructed}"
        );
        assert!(!constructed.contains("start_server"));
        assert!(source.contains("fn start_primary"));
        // And the only caller of `start_primary` is `startup`, which GApplication
        // runs in the primary instance only.
        let startup = source
            .find("fn startup(&self) {\n            self.parent_startup();")
            .unwrap();
        let calls: Vec<usize> = source
            .match_indices(concat!("self.start_primary", "()"))
            .map(|(at, _)| at)
            .collect();
        assert_eq!(calls.len(), 1, "one call");
        assert!(
            calls[0] > startup && calls[0] < startup + 200,
            "made from `startup`"
        );
    }
}

/// What `app.call` may do for whoever activates it (any process on the
/// session bus can).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CallAction {
    /// Asked first, in the window.
    Dial,
    /// Asked first, in the window.
    Message,
    /// Acts on a call that is already going: no number, nothing outward.
    OnTheCall,
    Refused,
}

pub(crate) fn call_action_kind(action: &str) -> CallAction {
    match action {
        "dial" => CallAction::Dial,
        "message" => CallAction::Message,
        "answer" | "decline" | "silence" | "end" | "mute" | "unmute" => CallAction::OnTheCall,
        _ => CallAction::Refused,
    }
}

#[cfg(test)]
mod call_action_tests {
    use super::{call_action_kind, CallAction};

    /// Nothing that starts something outward runs unasked through `app.call`.
    #[test]
    fn only_calls_already_going_skip_the_question() {
        assert_eq!(call_action_kind("dial"), CallAction::Dial);
        assert_eq!(call_action_kind("message"), CallAction::Message);
        for action in ["answer", "decline", "silence", "end", "mute", "unmute"] {
            assert_eq!(call_action_kind(action), CallAction::OnTheCall, "{action}");
        }
        for action in ["sms", "send", "dial ", "", "reply", "hangup-all"] {
            assert_eq!(call_action_kind(action), CallAction::Refused, "{action}");
        }
    }
}

/// Whether `device` has asked to turn on a Bucket and is waiting for the
/// answer.
pub(crate) fn bucket_ask_pending(
    offers: &[crate::device_links::folders::Offer],
    device: &str,
) -> bool {
    offers
        .iter()
        .any(|offer| offer.bucket && offer.device == device)
}

#[cfg(test)]
mod approval_notice_tests {
    use super::approval_changes;
    use std::collections::HashSet;

    #[test]
    fn the_approval_notice_is_posted_once_and_withdrawn_when_answered() {
        let mut shown = HashSet::new();
        let waiting = vec!["phone-a".to_string()];
        assert_eq!(
            approval_changes(&waiting, &mut shown),
            (vec!["phone-a".to_string()], vec![])
        );
        assert_eq!(
            approval_changes(&waiting, &mut shown),
            (vec![], vec![]),
            "not again"
        );
        assert_eq!(
            approval_changes(&[], &mut shown),
            (vec![], vec!["phone-a".to_string()])
        );
        assert_eq!(approval_changes(&[], &mut shown), (vec![], vec![]));
    }

    #[test]
    fn the_glide_notice_lives_exactly_as_long_as_the_dialog() {
        use super::glide_waiting;
        let mut shown = HashSet::new();
        let phone = || Some("phone-a".to_string());
        // Glide on but no dialog (the usual case): nothing is posted.
        assert_eq!(
            approval_changes(&glide_waiting(false, phone()), &mut shown),
            (vec![], vec![])
        );
        // The dialog goes up: posted once, however many ticks it takes.
        assert_eq!(
            approval_changes(&glide_waiting(true, phone()), &mut shown),
            (vec!["phone-a".to_string()], vec![])
        );
        assert_eq!(
            approval_changes(&glide_waiting(true, phone()), &mut shown),
            (vec![], vec![])
        );
        // Answered (or Glide switched off while it was up): withdrawn.
        assert_eq!(
            approval_changes(&glide_waiting(false, phone()), &mut shown),
            (vec![], vec!["phone-a".to_string()])
        );
        assert!(glide_waiting(true, None).is_empty());
    }
}

#[cfg(test)]
mod bucket_ask_tests {
    use crate::device_links::folders::Offer;

    #[test]
    fn turning_on_a_bucket_answers_only_an_ask_that_was_made() {
        let offers = vec![Offer {
            device: "phone-a".into(),
            id: "bucket".into(),
            name: "Bucket".into(),
            bucket: true,
        }];
        assert!(super::bucket_ask_pending(&offers, "phone-a"));
        assert!(!super::bucket_ask_pending(&offers, "phone-b"));
        assert!(!super::bucket_ask_pending(&[], "phone-a"));
    }
}
