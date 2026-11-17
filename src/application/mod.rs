/* application/mod.rs
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

//! The application ([`DesklinkApplication`]): start-up, the daemon handle,
//! the main window, the tray and top-bar hand-offs, and the app-wide rules
//! for closing, calls and Bucket questions.
//!
//! Layout: this file holds the GObject (`imp`, kept inline: its
//! `constructed`/`startup` are pinned by `primary_only_tests`), the core of
//! `DesklinkApplication`, the close and call rules and small helpers;
//! `actions` and `indicator` are the rest of its methods (one type in
//! several files), `styles` the palette and appearance. No cycles. May
//! depend on: `window`, `device_links` (the daemon handle, and directly
//! `config`, `asks`, `connection_log`, `desk_features`, `device`, `dnd`,
//! `folders`, `gallery`, `glide`, `private_fs`, `pty`, `transfer_log`,
//! `vault`, `webrtc`), `shell_dbus`, `tray`, `send_to`, `platform`, `kde`,
//! `search_provider`, `phone_mpris`, `bucket_progress`, `thumbnails`,
//! `signals`.

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::device_links::daemon::DaemonHandle;
use crate::DesklinkWindow;

mod actions;
mod indicator;
mod styles;
use styles::*;

// The crate-facing surface (everything else stays inside).
#[cfg(test)]
pub(crate) use styles::modern_css;
pub(crate) use styles::{appearance_scheme, load_appearance, save_appearance, token_color};

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
            // The update check: its own thread, inert without an address.
            crate::update::start();
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
            if let Some(connection) = self.obj().dbus_connection() {
                super::follow_activation_tokens(&connection);
            }
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
            super::present_window(&window);
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

/// A web link's host, for a button ("Open example.com").
fn link_host(url: &str) -> &str {
    let rest = url
        .split_once(':')
        .map_or(url, |(_, rest)| rest)
        .trim_start_matches('/');
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit('@').next().unwrap_or(host);
    if host.is_empty() {
        "link"
    } else {
        host
    }
}

/// The held-links notification's (title, body).
fn held_links_words(phone: &str, links: &[String]) -> (String, String) {
    let title = match links.len() {
        1 => format!("1 more link from {phone}"),
        n => format!("{n} more links from {phone}"),
    };
    let hosts: Vec<&str> = links
        .iter()
        .rev()
        .take(3)
        .map(|url| link_host(url))
        .collect();
    (
        title,
        format!(
            "Not opened yet, so they do not fill the screen: {}",
            hosts.join(", ")
        ),
    )
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

    /// The one notification for the web links held from `device_id`: "N
    /// more links from <phone>", an Open button for each of the newest
    /// three; withdrawn when none is left.
    pub(crate) fn show_held_links(&self, device_id: &str) {
        let id = format!("held-links-{device_id}");
        let Some((phone, links)) = crate::device_links::daemon::held_links(device_id) else {
            self.withdraw_notification(&id);
            return;
        };
        let (title, body) = held_links_words(&phone, &links);
        let notification = crate::application::desk_notification(&title);
        notification.set_body(Some(&body));
        for url in links.iter().rev().take(3) {
            notification.add_button_with_target_value(
                &format!("Open {}", link_host(url)),
                "app.open-held-link",
                Some(&(device_id, url.as_str()).to_variant()),
            );
        }
        self.send_notification(Some(&id), &notification);
    }

    /// Like [`Self::dial_with`], for a text message.
    pub(crate) fn message_with(&self, number: &str, phone: &str) {
        let window = self.main_window();
        present_window(&window);
        crate::window::confirm_message(&window, number, Some(phone));
    }

    /// Like [`Self::dial`], with the phone that must place the call.
    pub(crate) fn dial_with(&self, number: &str, phone: &str) {
        let window = self.main_window();
        present_window(&window);
        crate::window::confirm_dial(&window, number, Some(phone));
    }

    /// Shows the window and asks before calling `number` with the phone.
    pub(crate) fn dial(&self, number: &str) {
        let window = self.main_window();
        present_window(&window);
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

    /// The person answered `device`'s ask for `key` here (the notification,
    /// the window, the tray). Only an ask that is waiting turns anything on,
    /// and only from this click: a phone's packet never does.
    pub(crate) fn answer_ask(&self, device: &str, key: &str, allow: bool) {
        use crate::device_links::daemon::DaemonCommand;
        let Some(key) = crate::device_links::asks::take_incoming(device, key) else {
            return;
        };
        self.withdraw_notification(&crate::device_links::asks::notice_id(device, key));
        if !allow {
            crate::device_links::desk_features::push_declined(device, key);
            return;
        }
        let Some(daemon) = self.daemon_handle() else {
            return;
        };
        let id = device.to_string();
        match key {
            "terminalAccess" => daemon.send(DaemonCommand::SetTerminalAccess(id, true)),
            "unlockNearby" => daemon.send(DaemonCommand::SetUnlockNearby(id, true)),
            "galleryAccepted" => daemon.send(DaemonCommand::SetGalleryAccept(id, true)),
            "keepAwakeAllowed" => daemon.send(DaemonCommand::SetKeepAwakeFrom(id, true)),
            "dndSync" => daemon.send(DaemonCommand::SetDndShare(id, true)),
            "glide" => daemon.send(DaemonCommand::SetGlide(
                id,
                Some(crate::device_links::glide::Edge::Right),
            )),
            // A portal grant: Allow opens the desktop's own dialog.
            "control" => {
                let window = self.main_window();
                present_window(&window);
                crate::window::approve_control(&window, device);
            }
            _ => {}
        }
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
                        let note =
                            crate::application::desk_notification("DeskLink is still running");
                        note.set_body(Some(
                            "There is no tray on this desktop, so DeskLink stays connected in \
                             the background. Click here, or start DeskLink from your launcher, \
                             to open it again.",
                        ));
                        crate::application::open_on_click(&note, "app.show-window", None);
                        app.send_notification(Some("desklink-background"), &note);
                    }
                }
                CloseAction::Hide => {
                    // Once, ever: closing did not quit, and nothing said so (J16).
                    if take_first_hide(&crate::device_links::config::desklink_config_root()) {
                        let note =
                            crate::application::desk_notification("DeskLink is still running");
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
        present_window(&window);
        window.visible_dialog().is_none().then_some(window)
    }
}

/// A notification with DeskLink's icon. GNOME Shell takes the icon from the
/// application; Plasma, Xfce, MATE and Cinnamon draw a generic one or none
/// when the notification names none.
pub(crate) fn desk_notification(title: &str) -> gio::Notification {
    let notification = gio::Notification::new(title);
    notification.set_icon(&gio::ThemedIcon::new("derx06.desklink.com-symbolic"));
    notification
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

/// The xdg-activation token the last click came with (a notification's
/// `ActivationToken`, a tray host's `ProvideXdgActivationToken`), and when.
/// On Wayland a compositor that enforces focus stealing prevention (KWin)
/// raises a window only when it is shown with one; GLib's notification
/// backend does not pass it on, so DeskLink keeps it itself.
static ACTIVATION: std::sync::Mutex<Option<(String, std::time::Instant)>> =
    std::sync::Mutex::new(None);

/// How long a token waits for the click it came with to show the window.
const ACTIVATION_FRESH: std::time::Duration = std::time::Duration::from_secs(3);

pub(crate) fn note_activation_token(token: &str) {
    if token.is_empty() {
        return;
    }
    if let Ok(mut held) = ACTIVATION.lock() {
        *held = Some((token.to_string(), std::time::Instant::now()));
    }
}

/// The waiting token, once, if it is fresh at `now`.
fn take_activation_token(now: std::time::Instant) -> Option<String> {
    ACTIVATION
        .lock()
        .ok()?
        .take()
        .filter(|(_, at)| now.saturating_duration_since(*at) <= ACTIVATION_FRESH)
        .map(|(token, _)| token)
}

/// Shows `window` in front, with the click's activation token when one is
/// waiting (KDE Wayland ignored a plain `present`).
fn present_window(window: &impl IsA<gtk::Window>) {
    if let Some(token) = take_activation_token(std::time::Instant::now()) {
        window.set_startup_id(&token);
    }
    window.present();
}

/// Keeps the notification server's `ActivationToken` (sent just before its
/// `ActionInvoked`) for [`present_window`].
fn follow_activation_tokens(connection: &gio::DBusConnection) {
    let _ = connection.signal_subscribe(
        Some("org.freedesktop.Notifications"),
        Some("org.freedesktop.Notifications"),
        Some("ActivationToken"),
        Some("/org/freedesktop/Notifications"),
        None,
        gio::DBusSignalFlags::NONE,
        |_, _, _, _, _, parameters| {
            if let Some((_, token)) = parameters.get::<(u32, String)>() {
                note_activation_token(&token);
            }
        },
    );
}

/// What a click on `notification` does: its default action, or on a server
/// that draws the default action as an unlabelled button (xfce4-notifyd), an
/// "Open" button instead.
pub(crate) fn open_on_click(
    notification: &gio::Notification,
    action: &str,
    target: Option<&glib::Variant>,
) {
    if crate::device_links::asks::default_action_shows_as_button() {
        notification.add_button_with_target_value("Open", action, target);
    } else {
        notification.set_default_action_and_target_value(action, target);
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
mod approval_notice_tests;
#[cfg(test)]
mod bucket_ask_tests;
#[cfg(test)]
mod call_action_tests;
#[cfg(test)]
mod close_tests;
#[cfg(test)]
mod first_hide_tests;
#[cfg(test)]
mod held_links_tests;
#[cfg(test)]
mod primary_only_tests;
#[cfg(test)]
mod tests;
