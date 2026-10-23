/* window/mod.rs
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

//! The main window: a sidebar (the selected device's card, the pages, the
//! footer) and the selected page. This module keeps the window, the
//! once-a-second poll of the daemon and what it drives — events, pairing
//! dialogs, mirrored notifications, errors; each page lives in its own
//! module and is redrawn only when what it shows has changed.

mod bucket;
pub(crate) mod calls;
mod insert;
mod sound;
pub(crate) use calls::confirm_dial;
mod clipboard;
mod contacts;
pub(crate) mod history;
mod notes;
mod notes_clipboard;
pub(crate) mod notifications;
mod overview;
mod pairing;
mod pdf_view;
pub(crate) mod prefs;
mod sidebar;
mod terminals;
mod transfers;
mod viewer;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::device_links::daemon::{DaemonCommand, DaemonEvent, DaemonHandle};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::features::FeatureRegistry;
use crate::device_links::webrtc::transfer_board::{Finished, Outcome};

/// The pages, in sidebar order: (name, title, icon). The last two are the
/// sidebar's footer.
const PAGES: &[(&str, &str, &str)] = &[
    ("home", "Home", "user-home-symbolic"),
    ("files", "Files", "folder-symbolic"),
    ("notes", "Notes & Clipboard", "document-edit-symbolic"),
    (
        "notifications",
        "Notifications",
        "preferences-system-notifications-symbolic",
    ),
    ("contacts", "Contacts", "x-office-address-book-symbolic"),
    ("terminals", "Terminals", "utilities-terminal-symbolic"),
    ("settings", "Settings", "emblem-system-symbolic"),
];

/// How many of [`PAGES`] are in the sidebar's main list; Settings is the
/// footer.
const MAIN_PAGES: usize = 6;

/// The page a name opens, and the tab of Notes & Clipboard it means, if any:
/// `clipboard` and `snippets` are tabs of Notes & Clipboard (the tray and the
/// top-bar menu address them that way); every other name is a page of the
/// sidebar.
pub(crate) fn resolve_page(name: &str) -> (&str, Option<&'static str>) {
    match name {
        "clipboard" => ("notes", Some("clipboard")),
        "snippets" => ("notes", Some("snippets")),
        other => (other, None),
    }
}

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::{HashMap, HashSet};

    #[derive(Debug, Default, gtk::CompositeTemplate)]
    #[template(resource = "/derx06/desklink/com/window.ui")]
    pub struct DesklinkWindow {
        #[template_child]
        pub main_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub welcome_body: TemplateChild<gtk::Box>,
        #[template_child]
        pub split_view: TemplateChild<adw::NavigationSplitView>,
        #[template_child]
        pub content_page: TemplateChild<adw::NavigationPage>,
        #[template_child]
        pub device_card: TemplateChild<gtk::MenuButton>,
        #[template_child]
        pub nav_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub footer_list: TemplateChild<gtk::ListBox>,
        #[template_child]
        pub page_stack: TemplateChild<gtk::Stack>,
        #[template_child]
        pub error_banner: TemplateChild<adw::Banner>,
        #[template_child]
        pub call_bar: TemplateChild<gtk::Revealer>,
        #[template_child]
        pub narrow_breakpoint: TemplateChild<adw::Breakpoint>,
        #[template_child]
        pub compact_breakpoint: TemplateChild<adw::Breakpoint>,
        #[template_child]
        pub toast_overlay: TemplateChild<adw::ToastOverlay>,
        /// The window is phone-narrow: grids go one to a line.
        pub narrow: Cell<bool>,
        /// The page area is under 760sp wide (window narrow, or the sidebar
        /// takes the room): rows stack.
        pub compact: Cell<bool>,
        pub daemon: RefCell<Option<DaemonHandle>>,
        pub selected_device_id: RefCell<Option<String>>,
        /// The page shown, one of [`PAGES`]' names.
        pub page: RefCell<String>,
        /// What each page was last drawn from; a page is redrawn only when
        /// this changes.
        pub snapshots: RefCell<HashMap<String, String>>,
        /// The simple pages' bodies, by page name.
        pub bodies: RefCell<HashMap<String, (gtk::ScrolledWindow, gtk::Box)>>,
        pub(super) sidebar: RefCell<Option<sidebar::Sidebar>>,
        pub(super) terminals: RefCell<Option<terminals::Page>>,
        pub(super) notifications: RefCell<Option<notifications::Page>>,
        pub(super) transfers: RefCell<Option<transfers::Page>>,
        pub(super) contacts: RefCell<Option<contacts::Page>>,
        pub(super) notes: RefCell<Option<notes_clipboard::Page>>,
        /// Notes and Snippets on their own, with no phone to show pages for.
        pub(super) solo: RefCell<Option<notes::Solo>>,
        /// The welcome page's pairing code is on show.
        pub welcome_qr: RefCell<Option<std::rc::Rc<Cell<bool>>>>,
        /// When each device's active session started ("14:02"), by id.
        pub session_since: RefCell<HashMap<String, String>>,
        /// Devices whose remote-control approval is waiting on the portal.
        pub approving: RefCell<HashSet<String>>,
        /// The daemon's own words for the last error, behind "Details".
        pub last_raw_error: RefCell<String>,
        /// When a lasting error (offline) was last reported.
        pub banner_since: Cell<Option<std::time::Instant>>,
        /// The last error toast and when, so a repeating error toasts once.
        pub last_error_toast: RefCell<Option<(String, std::time::Instant)>>,
        pub notified_pair_requests: RefCell<HashSet<String>>,
        pub notified_remote_notifications: RefCell<HashSet<String>>,
        /// Ringing calls ("device:callId") whose notification is up.
        pub notified_calls: RefCell<HashSet<String>>,
        /// What the call bar was last drawn from.
        pub call_bar_key: RefCell<String>,
        /// The pairing dialog on screen for each incoming request, with the
        /// code it shows. A request whose code changes gets a fresh dialog, so
        /// Pair always confirms the code the user actually compared.
        pub pair_dialogs: RefCell<HashMap<String, (Option<String>, adw::AlertDialog)>>,
        /// Set while `refresh_devices` runs. Selecting a row emits its
        /// handler synchronously, and that handler refreshes again; the nested
        /// call must not redraw what the outer one is in the middle of.
        pub refreshing: Cell<bool>,
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
            let obj = self.obj();
            self.page.replace("home".to_string());
            self.selected_device_id.replace(sidebar::load_last_device());

            let window = obj.downgrade();
            self.narrow_breakpoint.connect_apply(move |_| {
                if let Some(window) = window.upgrade() {
                    window.imp().narrow.set(true);
                    window.apply_layout();
                }
            });
            let window = obj.downgrade();
            self.narrow_breakpoint.connect_unapply(move |_| {
                if let Some(window) = window.upgrade() {
                    window.imp().narrow.set(false);
                    window.apply_layout();
                }
            });
            for (apply, compact) in [(true, true), (false, false)] {
                let window = obj.downgrade();
                let changed = move |_: &adw::Breakpoint| {
                    if let Some(window) = window.upgrade() {
                        window.imp().compact.set(compact);
                        window.apply_layout();
                    }
                };
                if apply {
                    self.compact_breakpoint.connect_apply(changed);
                } else {
                    self.compact_breakpoint.connect_unapply(changed);
                }
            }

            self.sidebar.replace(Some(sidebar::Sidebar::new(&obj)));
            for (name, _, _) in PAGES {
                let child: gtk::Widget = match *name {
                    "terminals" => {
                        let page = terminals::Page::new(&obj);
                        let root = page.root.clone();
                        self.terminals.replace(Some(page));
                        root
                    }
                    "notifications" => {
                        let page = notifications::Page::new(&obj);
                        let root = page.root.clone();
                        self.notifications.replace(Some(page));
                        root
                    }
                    "files" => {
                        // The Bucket above, the send zone and the recent
                        // transfers below, in one scrolling page. The Bucket
                        // fills its own slot like any simple page's body.
                        let transfers = transfers::Page::new(&obj);
                        let slot = gtk::Box::builder()
                            .orientation(gtk::Orientation::Vertical)
                            .build();
                        let body = gtk::Box::builder()
                            .orientation(gtk::Orientation::Vertical)
                            .spacing(18)
                            .margin_top(8)
                            .margin_bottom(28)
                            .build();
                        body.append(&slot);
                        body.append(
                            &adw::Clamp::builder()
                                .maximum_size(1000)
                                .child(&transfers.content)
                                .build(),
                        );
                        let scroll = gtk::ScrolledWindow::builder()
                            .hscrollbar_policy(gtk::PolicyType::Never)
                            .vexpand(true)
                            .child(&body)
                            .build();
                        transfers.accept_drops_on(&scroll);
                        self.bodies
                            .borrow_mut()
                            .insert("bucket".to_string(), (scroll.clone(), slot));
                        self.transfers.replace(Some(transfers));
                        scroll.upcast()
                    }
                    "notes" => {
                        let page = notes_clipboard::Page::new(&obj);
                        let root = page.root.clone();
                        self.notes.replace(Some(page));
                        root
                    }
                    "contacts" => {
                        let page = contacts::Page::new(&obj);
                        let root = page.root.clone();
                        self.contacts.replace(Some(page));
                        root
                    }
                    _ => {
                        let body = gtk::Box::builder()
                            .orientation(gtk::Orientation::Vertical)
                            .build();
                        let scroll = gtk::ScrolledWindow::builder()
                            .hscrollbar_policy(gtk::PolicyType::Never)
                            .vexpand(true)
                            .child(&body)
                            .build();
                        self.bodies
                            .borrow_mut()
                            .insert(name.to_string(), (scroll.clone(), body));
                        scroll.upcast()
                    }
                };
                self.page_stack.add_named(&child, Some(name));
            }
            let solo = notes::Solo::new(&obj);
            self.main_stack.add_named(&solo.root, Some("solo"));
            self.solo.replace(Some(solo));

            obj.add_action_entries([
                // "Try Again" on a failed send's toast: the device, a newline, the file.
                gio::ActionEntry::builder("send-again")
                    .parameter_type(Some(glib::VariantTy::STRING))
                    .activate(|window: &super::DesklinkWindow, _, parameter| {
                        let Some(target) = parameter.and_then(|value| value.get::<String>()) else {
                            return;
                        };
                        // Only a retry this window offered: any other process on
                        // the session bus could call this action with a path.
                        if !take_retry(&target) {
                            return;
                        }
                        if let (Some((device, path)), Some(daemon)) =
                            (target.split_once('\n'), window.daemon())
                        {
                            daemon.send(DaemonCommand::SendFile(
                                device.to_string(),
                                std::path::PathBuf::from(path),
                            ));
                        }
                    })
                    .build(),
                // "Show in folder" on a received-file toast or row.
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
                // "shell", "claude" or "codex": a new shared terminal here.
                gio::ActionEntry::builder("new-terminal")
                    .parameter_type(Some(glib::VariantTy::STRING))
                    .activate(|window: &super::DesklinkWindow, _, parameter| {
                        let program = parameter
                            .and_then(|value| value.get::<String>())
                            .filter(|program| program != "shell");
                        match crate::terminal_previews::start(program.as_deref()) {
                            Ok(_) => window.refresh_soon(),
                            Err(error) => {
                                eprintln!("[UI] Could not start a terminal: {error}");
                                window.show_feature_toast("Could not start a terminal");
                            }
                        }
                    })
                    .build(),
                // A new shell here, started in a folder ("~/api").
                gio::ActionEntry::builder("new-terminal-in")
                    .parameter_type(Some(glib::VariantTy::STRING))
                    .activate(|window: &super::DesklinkWindow, _, parameter| {
                        let Some(folder) = parameter.and_then(|value| value.get::<String>()) else {
                            return;
                        };
                        match crate::device_links::pty::desk::new_desk_session_in(&folder) {
                            Ok(_) => window.refresh_soon(),
                            Err(error) => {
                                eprintln!("[UI] Could not start a terminal in {folder}: {error}");
                                window.show_feature_toast("Could not start a terminal there");
                            }
                        }
                    })
                    .build(),
                gio::ActionEntry::builder("discover")
                    .activate(|window: &super::DesklinkWindow, _, _| {
                        if let Some(daemon) = window.imp().daemon.borrow().clone() {
                            daemon.send(DaemonCommand::Discover);
                            window.show_feature_toast("Searching for devices…");
                        }
                    })
                    .build(),
                gio::ActionEntry::builder("pair-new")
                    .activate(|window: &super::DesklinkWindow, _, _| {
                        window.show_discovery_dialog(true)
                    })
                    .build(),
                // Ctrl+Shift+K: type on the selected phone.
                gio::ActionEntry::builder("type-on-phone")
                    .activate(|window: &super::DesklinkWindow, _, _| {
                        if let (Some(daemon), Some(id)) = (window.daemon(), window.selected_id()) {
                            crate::type_on_phone::open(&daemon, &id, Some(window.upcast_ref()));
                        }
                    })
                    .build(),
                // A page by name, e.g. from a card's "View All".
                gio::ActionEntry::builder("show-page")
                    .parameter_type(Some(glib::VariantTy::STRING))
                    .activate(|window: &super::DesklinkWindow, _, parameter| {
                        if let Some(page) = parameter.and_then(|value| value.get::<String>()) {
                            window.show_page(&page);
                        }
                    })
                    .build(),
                // Ctrl+F: the terminal search.
                gio::ActionEntry::builder("toggle-search")
                    .activate(|window: &super::DesklinkWindow, _, _| {
                        if window.show_page("terminals") {
                            if let Some(page) = window.imp().terminals.borrow().as_ref() {
                                page.focus_search();
                            }
                        }
                    })
                    .build(),
            ]);

            let window = obj.downgrade();
            self.error_banner.connect_button_clicked(move |_| {
                if let Some(window) = window.upgrade() {
                    let raw = window.imp().last_raw_error.borrow().clone();
                    window.show_error_details(&raw);
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

    /// Selects `device_id` and shows its overview (from the top-bar extension).
    pub fn show_device(&self, device_id: &str) {
        self.select_device(device_id);
        self.show_page("home");
    }

    /// A new note, open to type in (the + on Notes, the tray, the top bar).
    pub fn new_note(&self) {
        self.show_page("notes");
        let imp = self.imp();
        if imp.main_stack.visible_child_name().as_deref() == Some("solo") {
            if let Some(solo) = imp.solo.borrow().as_ref() {
                solo.notes.new_note();
            }
        } else if let Some(page) = imp.notes.borrow().as_ref() {
            page.show_tab("notes");
            page.notes.new_note();
        }
    }

    /// Notes or Snippets ("notes", "snippets") full-window, for when no
    /// paired phone has pages to show them in.
    fn show_solo(&self, which: &str) {
        let imp = self.imp();
        if let Some(solo) = imp.solo.borrow().as_ref() {
            solo.show(which);
        }
        imp.main_stack.set_visible_child_name("solo");
        self.apply_layout();
        self.refresh_soon();
    }

    /// Back from [`Self::show_solo`]: the next refresh picks the welcome
    /// page or the devices.
    pub(super) fn leave_solo(&self) {
        let imp = self.imp();
        imp.main_stack.set_visible_child_name("welcome");
        imp.snapshots.borrow_mut().clear();
        self.refresh_soon();
    }

    /// Whether the selected device is a paired one, with pages to show.
    fn selected_is_paired(&self) -> bool {
        let (Some(daemon), Some(id)) = (self.daemon(), self.selected_id()) else {
            return false;
        };
        self.known_devices(&daemon)
            .iter()
            .any(|device| device.id == id && device.trusted)
    }

    /// Shows one of `device_id`'s contacts (from GNOME search).
    pub fn show_contact(&self, device_id: &str, uid: &str) {
        self.select_device(device_id);
        self.show_page("contacts");
        if let Some(page) = self.imp().contacts.borrow().as_ref() {
            page.show_contact(self, device_id, uid);
        }
    }

    /// Makes `device_id` the device every page is about, and remembers it.
    fn select_device(&self, device_id: &str) {
        let imp = self.imp();
        if imp.selected_device_id.borrow().as_deref() == Some(device_id) {
            return;
        }
        imp.selected_device_id.replace(Some(device_id.to_string()));
        sidebar::save_last_device(device_id);
        imp.snapshots.borrow_mut().clear();
        if let Some(page) = imp.terminals.borrow().as_ref() {
            page.close_detail();
        }
        self.refresh_soon();
    }

    /// Shows page `name`; false while there is no device to show pages for.
    pub fn show_page(&self, name: &str) -> bool {
        let imp = self.imp();
        // Notes and snippets are the user's own, phone or not.
        if matches!(name, "notes" | "snippets")
            && (imp.main_stack.visible_child_name().as_deref() != Some("devices")
                || !self.selected_is_paired())
        {
            self.show_solo(name);
            return true;
        }
        // Earlier names, and the tabs of Notes & Clipboard.
        let (name, tab) = resolve_page(name);
        if imp.main_stack.visible_child_name().as_deref() != Some("devices")
            || !PAGES.iter().any(|(page, _, _)| *page == name)
        {
            return false;
        }
        if let (Some(tab), Some(page)) = (tab, imp.notes.borrow().as_ref()) {
            page.show_tab(tab);
        }
        imp.page.replace(name.to_string());
        imp.page_stack.set_visible_child_name(name);
        if let Some(sidebar) = imp.sidebar.borrow().as_ref() {
            sidebar.select(name);
        }
        imp.split_view.set_show_content(true);
        imp.snapshots.borrow_mut().remove(name);
        self.update_title();
        self.refresh_soon();
        true
    }

    /// The header's title: the page's name, or the terminal being looked at.
    fn update_title(&self) {
        let imp = self.imp();
        let page = imp.page.borrow().clone();
        let detail = (page == "terminals")
            .then(|| {
                imp.terminals
                    .borrow()
                    .as_ref()
                    .and_then(|page| page.detail_title())
            })
            .flatten();
        let title = detail.unwrap_or_else(|| {
            PAGES
                .iter()
                .find(|(name, _, _)| *name == page)
                .map_or("DeskLink", |(_, title, _)| title)
                .to_string()
        });
        imp.content_page.set_title(&title);
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
                    // "sound ID desk-sound|phone-sound|phone-mic on|off" (F1).
                    if verb == "sound" {
                        let (what, state) = arg.split_once(' ').unwrap_or((&arg, "on"));
                        e2e_daemon.send(DaemonCommand::Sound(id, what.into(), state == "on"));
                        return;
                    }
                    // "glide ID left|right|top|bottom|off" (F4; an `e2e-*`
                    // id glides with no phone, its packets logged).
                    if verb == "glide" {
                        let edge = crate::device_links::glide::Edge::ALL
                            .into_iter()
                            .find(|edge| format!("{edge:?}").eq_ignore_ascii_case(&arg));
                        let id = if id == "selected" {
                            win.selected_id().unwrap_or_default()
                        } else {
                            id
                        };
                        e2e_daemon.send(DaemonCommand::SetGlide(id, edge));
                        return;
                    }
                    if verb == "desk-terminal" {
                        let result = crate::device_links::pty::desk::new_desk_session();
                        debug_log!("[E2E] desk-terminal: {result:?}");
                        return;
                    }
                    // "extend-selftest WxH": F2's second display without a phone.
                    if verb == "extend-selftest" {
                        let size = id
                            .split_once('x')
                            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                            .unwrap_or((2400, 1080));
                        let _ = std::thread::Builder::new()
                            .name("desklink-extend-selftest".into())
                            .spawn(move || {
                                let result =
                                    crate::device_links::webrtc::virtual_display::selftest(size);
                                debug_log!("[E2E] extend-selftest: {result:?}");
                            });
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
        // The tickers run every second while this window shows and slowly
        // when it does not: showing it brings them back at once.
        self.connect_map(|window| {
            crate::device_links::daemon::set_ui_visible(true);
            window.refresh_soon();
            // Every ticker was waiting for a window nobody could see.
            crate::ticker::nudge_all();
        });
        self.connect_unmap(|window| {
            crate::device_links::daemon::set_ui_visible(false);
            // A window that stays hidden gives its memory back: the pictures
            // go, the page that held them is redrawn on the next look, and
            // the allocator returns what it can.
            let window = window.downgrade();
            glib::timeout_add_seconds_local_once(HIDDEN_TRIM_SECONDS, move || {
                let Some(window) = window.upgrade().filter(|window| !window.is_mapped()) else {
                    return;
                };
                window.trim_memory();
            });
        });
        // A window nobody is looking at (another desktop, behind others)
        // stops its pulsing dots, so it costs no frames.
        self.connect_is_active_notify(mark_focus);
        mark_focus(self);
        self.refresh_devices();
        // Events (a call ringing, a ping) refresh at once, not at the tick.
        crate::device_links::daemon::state::set_ui_wake(refresh_open_windows);

        let weak = self.downgrade();
        crate::ticker::every_tick(move || {
            crate::guard("window refresh", glib::ControlFlow::Continue, || {
                if let Some(window) = weak.upgrade() {
                    window.refresh_devices();
                    glib::ControlFlow::Continue
                } else {
                    glib::ControlFlow::Break
                }
            })
        });
    }

    /// The devices the window knows: paired ones and those mid-pairing.
    fn known_devices(&self, daemon: &DaemonHandle) -> Vec<DeviceView> {
        #[allow(unused_mut)]
        let mut devices = daemon.devices();
        #[cfg(debug_assertions)]
        devices.extend(self.imp().fake_devices.borrow().iter().cloned());
        devices
            .into_iter()
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
            .collect()
    }

    fn refresh_devices(&self) {
        crate::device_links::daemon::set_ui_visible(self.is_visible());
        let imp = self.imp();
        if imp.refreshing.get() {
            return;
        }
        let Some(daemon) = imp.daemon.borrow().clone() else {
            return;
        };
        imp.refreshing.set(true);
        // RAII rather than a set at the end: this returns early from several
        // places, and any missed reset would wedge the window permanently.
        let _guard = RefreshGuard(&imp.refreshing);

        // The transfer cards' own timer runs only while something moves.
        if let Some(transfers) = imp.transfers.borrow().as_ref() {
            transfers.kick();
        }

        let errors = daemon.drain_errors();
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
                DaemonEvent::PairedByQr {
                    device_id,
                    device_name,
                } => {
                    // The scan is done: leave "Pair New Device" for the phone
                    // it just paired. The page renders later in this refresh.
                    if let Some(dialog) = self.visible_dialog() {
                        if dialog.title() == "Pair New Device" {
                            dialog.close();
                        }
                    }
                    imp.selected_device_id.replace(Some(device_id.clone()));
                    sidebar::save_last_device(&device_id);
                    imp.page.replace("home".to_string());
                    imp.split_view.set_show_content(true);
                    self.show_feature_toast(&format!("Paired with {device_name}"));
                }
                DaemonEvent::PhoneScreenOffered { device_id } => {
                    crate::remote_control::show_remote_control_dialog(
                        daemon.clone(),
                        device_id,
                        self,
                    );
                }
                DaemonEvent::MissedCall {
                    device_id,
                    device_name,
                    caller,
                    number,
                } => calls::missed(self, &device_id, &device_name, caller, number),
                DaemonEvent::CallRefused { message } => self.show_feature_toast(&message),
                DaemonEvent::PairingReplaced { name } => {
                    let text = format!(
                        "The earlier pairing of {name} was replaced. If that was a different phone, pair it again."
                    );
                    self.send_feature_notification(
                        &format!("desklink-replaced-{}", glib::monotonic_time()),
                        &format!("Replaced the old pairing of {name}"),
                        &text,
                    );
                    self.show_feature_toast(&text);
                }
                DaemonEvent::SoundNotice { message, .. }
                | DaemonEvent::CaptureNotice { message, .. } => self.show_feature_toast(&message),
                DaemonEvent::WifiFromPhone { network, .. } => self.offer_wifi(network),
                // Handled by this refresh itself (the call card and notification).
                DaemonEvent::CallChanged { .. } => {}
            }
        }

        self.follow_transfers(&daemon);

        let devices = self.known_devices(&daemon);
        // A device connected over the network proves the network is up: an
        // "offline" report then is stale (e.g. the relay failing), not news.
        let connected = devices
            .iter()
            .any(|device| matches!(device.status, DeviceStatus::Paired));
        if let Some(error) = errors.last() {
            self.report_error(error, connected);
        }
        {
            // When each active session started, for "since 14:02".
            let mut since = imp.session_since.borrow_mut();
            since.retain(|id, _| {
                devices
                    .iter()
                    .any(|device| &device.id == id && session_summary(device).is_some())
            });
            for device in devices.iter().filter(|d| session_summary(d).is_some()) {
                since
                    .entry(device.id.clone())
                    .or_insert_with(|| clock_time(glib::DateTime::now_local().ok()));
            }
        }
        if connected && is_offline_message(&imp.error_banner.title()) {
            imp.error_banner.set_revealed(false);
            imp.banner_since.set(None);
        }
        self.mirror_device_side_effects(&daemon, &devices, errors.is_empty());
        calls::notify(self, &devices);

        let selected = imp.selected_device_id.borrow().clone();
        if selected
            .as_ref()
            .is_none_or(|id| devices.iter().all(|device| &device.id != id))
        {
            // The remembered device was unpaired, or none was chosen: the
            // first one, preferring a connected device.
            let first = devices
                .iter()
                .find(|device| matches!(device.status, DeviceStatus::Paired))
                .or(devices.first())
                .map(|device| device.id.clone());
            imp.selected_device_id.replace(first);
            imp.snapshots.borrow_mut().clear();
        }

        if imp.main_stack.visible_child_name().as_deref() == Some("solo") {
            // Stays until Back, even if a phone pairs meanwhile.
            if let Some(stop) = imp.welcome_qr.take() {
                pairing::stop_welcome(&daemon, &stop);
            }
            if let Some(solo) = imp.solo.borrow().as_ref() {
                solo.update();
            }
            return;
        }
        if devices.is_empty() {
            imp.notified_pair_requests.borrow_mut().clear();
            imp.notified_remote_notifications.borrow_mut().clear();
            if imp.main_stack.visible_child_name().as_deref() != Some("welcome") {
                imp.main_stack.set_visible_child_name("welcome");
                imp.snapshots.borrow_mut().clear();
            }
            if imp.welcome_qr.borrow().is_none() {
                let shown = pairing::welcome_page(self, &daemon);
                imp.welcome_qr.replace(Some(shown));
            }
            return;
        }
        if let Some(stop) = imp.welcome_qr.take() {
            pairing::stop_welcome(&daemon, &stop);
        }
        if imp.main_stack.visible_child_name().as_deref() != Some("devices") {
            imp.main_stack.set_visible_child_name("devices");
            let page = imp.page.borrow().clone();
            imp.page_stack.set_visible_child_name(&page);
            self.update_title();
        }
        let selected_id = imp.selected_device_id.borrow().clone();
        let Some(device) = devices
            .iter()
            .find(|device| Some(&device.id) == selected_id.as_ref())
            .cloned()
        else {
            return;
        };
        if let Some(sidebar) = imp.sidebar.borrow().as_ref() {
            sidebar.update(self, &devices, &device);
        }
        self.render_page(&daemon, &device);
        calls::update_bar(self, &daemon, &devices);
    }

    /// Draws the page on show, if what it shows has changed.
    fn render_page(&self, daemon: &DaemonHandle, device: &DeviceView) {
        let imp = self.imp();
        let mut page = imp.page.borrow().clone();
        // A device still pairing has only its pairing view.
        if !device.trusted && page != "home" {
            page = "home".to_string();
            imp.page.replace(page.clone());
            imp.page_stack.set_visible_child_name(&page);
            self.update_title();
        }
        match page.as_str() {
            "terminals" => {
                if let Some(terminals) = imp.terminals.borrow().as_ref() {
                    terminals.update(self, device);
                }
            }
            "notifications" => {
                if let Some(notifications) = imp.notifications.borrow().as_ref() {
                    notifications.update(self, daemon, device);
                }
            }
            "files" => {
                bucket::update(self, device);
                if let Some(transfers) = imp.transfers.borrow().as_ref() {
                    transfers.update(self, daemon, device);
                }
            }
            "home" => overview::update(self, daemon, device),
            "notes" => {
                if let Some(notes) = imp.notes.borrow().as_ref() {
                    notes.update(device);
                }
            }
            "contacts" => {
                if let Some(contacts) = imp.contacts.borrow().as_ref() {
                    contacts.update(self, daemon, device);
                }
            }
            "settings" => prefs::update(self, daemon, device),
            _ => {}
        }
    }

    /// Whether page `name` must be redrawn for `key`, recording it.
    fn page_changed(&self, name: &str, key: String) -> bool {
        let previous = self
            .imp()
            .snapshots
            .borrow_mut()
            .insert(name.to_string(), key.clone());
        previous.as_ref() != Some(&key)
    }

    /// Replaces page `name`'s content, keeping its scroll position and, when
    /// it is the same device, where keyboard focus was.
    fn replace_body(&self, name: &str, content: &impl IsA<gtk::Widget>) {
        let Some((scroll, body)) = self.imp().bodies.borrow().get(name).cloned() else {
            return;
        };
        let body_widget = body.clone().upcast::<gtk::Widget>();
        let focus_path =
            gtk::prelude::RootExt::focus(self).and_then(|focus| widget_path(&body_widget, &focus));
        let adjustment = scroll.vadjustment();
        let offset = adjustment.value();
        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        body.append(content);
        self.apply_layout();
        adjustment.set_value(offset);
        let body = body.downgrade();
        glib::idle_add_local_once(move || {
            adjustment.set_value(offset);
            let Some(body) = body.upgrade() else {
                return;
            };
            if let Some(widget) = focus_path.and_then(|path| widget_at(body.upcast_ref(), &path)) {
                widget.grab_focus();
            }
        });
    }

    /// Lays named boxes and grids out for the window's width: the overview's
    /// two columns and the hero stack when phone-narrow, the terminal grid
    /// goes one to a line.
    fn apply_layout(&self) {
        let narrow = self.imp().narrow.get();
        let compact = narrow || self.imp().compact.get();
        // Tighter page edges, for `style.css`.
        if narrow {
            self.add_css_class("narrow");
        } else {
            self.remove_css_class("narrow");
        }
        let mut stack = vec![self.imp().main_stack.get().upcast::<gtk::Widget>()];
        while let Some(widget) = stack.pop() {
            match widget.widget_name().as_str() {
                "responsive" => {
                    if let Some(row) = widget.downcast_ref::<gtk::Box>() {
                        row.set_orientation(if compact {
                            gtk::Orientation::Vertical
                        } else {
                            gtk::Orientation::Horizontal
                        });
                    }
                }
                "terminal-grid" => {
                    if let Some(grid) = widget.downcast_ref::<gtk::FlowBox>() {
                        grid.set_max_children_per_line(if narrow { 1 } else { 3 });
                    }
                }
                // Right-aligned beside the hero's name; under it when the
                // hero stacks.
                "hero-actions" => widget.set_halign(if compact {
                    gtk::Align::Fill
                } else {
                    gtk::Align::End
                }),
                // Notes: the list or the note, one at a time, when narrow.
                "notes-split" => {
                    if let Some(split) = widget.downcast_ref::<adw::NavigationSplitView>() {
                        split.set_collapsed(compact);
                    }
                }
                // Notes and Snippets with no phone: the switcher.
                "solo-switcher" | "tabs-switcher" => widget.set_visible(!narrow),
                "solo-bar" | "tabs-bar" => {
                    if let Some(bar) = widget.downcast_ref::<adw::ViewSwitcherBar>() {
                        bar.set_reveal(narrow);
                    }
                }
                "hero-wrap" => widget.set_halign(if compact {
                    gtk::Align::Start
                } else {
                    gtk::Align::End
                }),
                _ => {}
            }
            let mut child = widget.first_child();
            while let Some(next) = child {
                child = next.next_sibling();
                stack.push(next);
            }
        }
    }

    /// Runs `refresh_devices` once the current handler has returned, e.g.
    /// after a click whose own button the refresh may replace.
    /// The window has been hidden a while: drops decoded pictures and the
    /// Files page's cards (redrawn when it is shown again) and hands free
    /// memory back to the system.
    fn trim_memory(&self) {
        let imp = self.imp();
        transfers::forget_pictures();
        if let Some(transfers) = imp.transfers.borrow().as_ref() {
            transfers.release_cards();
        }
        // Redrawn on the next look.
        imp.snapshots.borrow_mut().remove("bucket");
        self.replace_body("bucket", &gtk::Box::new(gtk::Orientation::Vertical, 0));
        // SAFETY: malloc_trim has no preconditions; it only returns free memory.
        unsafe {
            libc::malloc_trim(0);
        }
    }

    fn refresh_soon(&self) {
        let window = self.downgrade();
        glib::idle_add_local_once(move || {
            if let Some(window) = window.upgrade() {
                window.refresh_devices();
            }
        });
    }

    /// Pairing dialogs, mirrored phone notifications and the error banner.
    ///
    /// Driven by every known device, not by the page on show: a pair request
    /// or a notification from a device not selected must still be seen.
    fn mirror_device_side_effects(
        &self,
        daemon: &DaemonHandle,
        devices: &[DeviceView],
        errors_empty: bool,
    ) {
        let imp = self.imp();

        let current_pair_requests = devices
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
                    !devices.iter().any(|device| {
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
        for device in devices
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
            // device's overview still offers Pair and Reject.
            if is_new || reopen.contains(&device.id) {
                pairing::present_pair_dialog(self, daemon, device);
            }
        }

        // A lasting state (offline) keeps its banner a while after its last
        // report, so a once-a-cycle error does not make it flicker.
        let banner_settled = imp
            .banner_since
            .get()
            .is_none_or(|since| since.elapsed() >= std::time::Duration::from_secs(10));
        if errors_empty
            && banner_settled
            && devices.iter().all(|device| device.last_error.is_none())
        {
            imp.error_banner.set_revealed(false);
        }

        let settings = crate::device_links::config::app_settings();
        let current_notifications = devices
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
        for device in devices {
            let shown = settings.shows_notifications(&device.id);
            for notification in &device.notifications {
                let unique_id = format!("{}:{}", device.id, notification.id);
                let is_new = imp
                    .notified_remote_notifications
                    .borrow_mut()
                    .insert(unique_id.clone());
                // Hidden: marked as seen, so turning mirroring back on does
                // not replay the backlog. A resync (`silent`) never pops.
                // With the top-bar extension on, it shows them as GNOME's
                // own notifications: never twice.
                if !pops_up(is_new, shown, notification.silent)
                    || crate::shell_dbus::client_handles("notifications")
                {
                    continue;
                }
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
                // In front of the user: a toast; otherwise the desktop's own.
                if self.is_active() && self.is_visible() {
                    let toast = adw::Toast::new(&format!("{title} — {body}"));
                    toast.set_use_markup(false);
                    toast.set_timeout(5);
                    toast.set_button_label(Some("View"));
                    toast.set_action_name(Some("win.show-page"));
                    toast.set_action_target_value(Some(&"notifications".to_variant()));
                    imp.toast_overlay.add_toast(toast);
                } else {
                    self.send_app_notification(
                        &format!("phone-notification-{unique_id}"),
                        &title,
                        body,
                    );
                }
            }
        }
    }

    fn quick_action(&self, action: QuickAction) {
        let Some(daemon) = self.imp().daemon.borrow().clone() else {
            return;
        };
        let Some(id) = self.imp().selected_device_id.borrow().clone() else {
            return;
        };
        let name = daemon
            .devices()
            .into_iter()
            .find(|device| device.id == id)
            .map(|device| device.name)
            .unwrap_or_default();
        let (command, message) = match action {
            QuickAction::Ring => (DaemonCommand::SendFindPhone(id), format!("Ringing {name}")),
            QuickAction::Lock => (
                DaemonCommand::SendLockRequest(id, true),
                format!("Locking {name}"),
            ),
            QuickAction::Ping => (DaemonCommand::SendPing(id), "Ping sent".to_string()),
            QuickAction::Clipboard => (
                DaemonCommand::SendClipboard(id),
                format!("Clipboard sent to {name}"),
            ),
        };
        daemon.send(command);
        self.show_feature_toast(&message);
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

    /// Tells the user how each transfer ended. The Transfers page shows the
    /// ones still moving.
    fn follow_transfers(&self, daemon: &DaemonHandle) {
        let finished = daemon.take_finished_transfers();
        if finished.is_empty() {
            return;
        }
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
        // "Received here" has a new entry.
        self.imp().snapshots.borrow_mut().remove("files");
    }

    fn announce_transfer(&self, done: &Finished, device: &str) {
        let name = &done.name;
        let (message, notify) = match &done.outcome {
            Outcome::Received(path) => (
                format!("Received {name} from {device}. Saved in {}", saved_in(path)),
                true,
            ),
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
        } else if let (Outcome::Failed(_), Some(source)) = (&done.outcome, &done.retry) {
            // A send that failed after the last block (the phone could not
            // place the file) can be sent again.
            let target = format!("{}\n{}", done.device_id, source.to_string_lossy());
            offer_retry(&target);
            toast.set_button_label(Some("Try Again"));
            toast.set_action_name(Some("win.send-again"));
            toast.set_action_target_value(Some(&target.to_variant()));
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
    fn report_error(&self, raw: &str, connected: bool) {
        let imp = self.imp();
        let (message, lasting) = friendly_error(raw);
        imp.last_raw_error.replace(raw.to_string());
        if connected && is_offline_message(&message) {
            debug_log!("[UI] Offline report while a device is connected: {raw}");
            return;
        }
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

    /// A Wi-Fi QR code from the phone (F3): its text is already copied;
    /// Join adds the network through NetworkManager.
    fn offer_wifi(&self, network: crate::device_links::webrtc::capture::Wifi) {
        let toast = adw::Toast::new(&format!("Wi-Fi network “{}” scanned", network.ssid));
        toast.set_use_markup(false);
        toast.set_timeout(15);
        toast.set_button_label(Some("Join"));
        let window = self.downgrade();
        toast.connect_button_clicked(move |_| {
            let (network, window) = (network.clone(), window.clone());
            let (sender, receiver) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(crate::device_links::webrtc::capture::join_wifi(&network));
            });
            glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
                let result = match try_result(&receiver) {
                    Poll::Waiting => return glib::ControlFlow::Continue,
                    Poll::Done(result) => result,
                    // The worker died without answering: there is nothing to wait for.
                    Poll::Gone => Err("NetworkManager did not answer".to_string()),
                };
                if let (Err(error), Some(window)) = (result, window.upgrade()) {
                    window.show_feature_toast(&format!("Could not join: {error}"));
                }
                glib::ControlFlow::Break
            });
        });
        self.imp().toast_overlay.add_toast(toast);
    }

    pub(crate) fn show_feature_toast(&self, message: &str) {
        let toast = adw::Toast::new(message);
        toast.set_use_markup(false);
        toast.set_timeout(3);
        self.imp().toast_overlay.add_toast(toast);
    }

    fn daemon(&self) -> Option<DaemonHandle> {
        self.imp().daemon.borrow().clone()
    }

    fn selected_id(&self) -> Option<String> {
        self.imp().selected_device_id.borrow().clone()
    }
}

#[derive(Clone, Copy)]
enum QuickAction {
    Ring,
    Lock,
    Ping,
    /// Send this computer's clipboard text to the phone now.
    Clipboard,
}

/// Refreshes every open DeskLink window soon (on the main loop).
/// Where a result sent from a worker thread stands.
#[derive(Debug, PartialEq, Eq)]
enum Poll<T> {
    Waiting,
    Done(T),
    /// The sender is gone without sending (the thread panicked).
    Gone,
}

fn try_result<T>(receiver: &std::sync::mpsc::Receiver<T>) -> Poll<T> {
    match receiver.try_recv() {
        Ok(value) => Poll::Done(value),
        Err(std::sync::mpsc::TryRecvError::Empty) => Poll::Waiting,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => Poll::Gone,
    }
}

thread_local! {
    /// The "Try Again" buttons on offer: `device\npath`, newest last.
    static RETRIES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// How many "Try Again" offers are remembered.
const MAX_RETRIES: usize = 16;

/// A failed send's "Try Again" is on screen: only these can be invoked.
fn offer_retry(target: &str) {
    RETRIES.with(|retries| {
        let mut retries = retries.borrow_mut();
        retries.retain(|known| known != target);
        if retries.len() >= MAX_RETRIES {
            retries.remove(0);
        }
        retries.push(target.to_string());
    });
}

/// Whether `target` is an offered retry; it is used up (one try per offer).
fn take_retry(target: &str) -> bool {
    RETRIES.with(|retries| {
        let mut retries = retries.borrow_mut();
        let before = retries.len();
        retries.retain(|known| known != target);
        retries.len() != before
    })
}

/// Where a received file is, for a person: "Downloads", or the folder's name.
pub(crate) fn saved_in(path: &std::path::Path) -> String {
    saved_in_with(
        path,
        glib::user_special_dir(glib::UserDirectory::Downloads).as_deref(),
    )
}

/// [`saved_in`] given the Downloads folder. A Downloads that is a link is
/// the same folder as the one its files were put in (the real directory).
fn saved_in_with(path: &std::path::Path, downloads: Option<&std::path::Path>) -> String {
    let folder = path.parent();
    if let (Some(folder), Some(downloads)) = (folder, downloads) {
        let same = |a: &std::path::Path, b: &std::path::Path| {
            a == b
                || std::fs::canonicalize(a)
                    .ok()
                    .zip(std::fs::canonicalize(b).ok())
                    .is_some_and(|(a, b)| a == b)
        };
        if same(folder, downloads) {
            return "Downloads".to_string();
        }
    }
    folder
        .and_then(std::path::Path::file_name)
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "your downloads folder".to_string())
}

/// How long the window may stay hidden before it gives its memory back.
const HIDDEN_TRIM_SECONDS: u32 = 20;

/// The `unfocused` class (see `style.css`): on while the window is not the
/// one in use.
fn mark_focus(window: &DesklinkWindow) {
    if window.is_active() {
        window.remove_css_class("unfocused");
    } else {
        window.add_css_class("unfocused");
    }
}

fn refresh_open_windows() {
    for window in gtk::Window::list_toplevels() {
        if let Ok(window) = window.downcast::<DesklinkWindow>() {
            window.refresh_soon();
        }
    }
}

/// Clears the re-entrancy flag however `refresh_devices` exits.
struct RefreshGuard<'a>(&'a std::cell::Cell<bool>);

impl Drop for RefreshGuard<'_> {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

/// "14:02" for `time`, or nothing.
/// Whether a phone notification is announced here (toast or desktop
/// notification): new, mirrored, and not a resync (`silent`).
fn pops_up(is_new: bool, shown: bool, silent: bool) -> bool {
    is_new && shown && !silent
}

/// "last seen 14:02" (or "last seen 3 Oct 14:02") while the phone's link is
/// down and what it last reported is stale; `None` while it is live.
pub(super) fn last_seen_words(device: &DeviceView) -> Option<String> {
    if device.feature_transport_ready {
        return None;
    }
    let seen = glib::DateTime::from_unix_local(device.last_seen? as i64).ok()?;
    let today = glib::DateTime::now_local()
        .ok()
        .is_some_and(|now| now.ymd() == seen.ymd());
    let format = if today { "%H:%M" } else { "%-d %b %H:%M" };
    Some(format!("last seen {}", seen.format(format).ok()?))
}

fn clock_time(time: Option<glib::DateTime>) -> String {
    time.and_then(|time| time.format("%H:%M").ok())
        .map(|text| text.to_string())
        .unwrap_or_default()
}

/// "using 2 terminals, viewing your screen": what the phone is doing here.
fn session_summary(device: &DeviceView) -> Option<String> {
    use crate::device_links::device::DesktopSession;
    let mut parts = Vec::new();
    match device.terminal_sessions_attached {
        0 => {}
        1 => parts.push("in a terminal".to_string()),
        count => parts.push(format!("in {count} terminals")),
    }
    match (device.desktop_session, device.desktop_extended) {
        (DesktopSession::None, _) => {}
        (_, true) => parts.push("a second display".to_string()),
        (DesktopSession::Viewing, false) => parts.push("viewing your screen".to_string()),
        (DesktopSession::Controlling, false) => parts.push("controlling your screen".to_string()),
    }
    (!parts.is_empty()).then(|| parts.join(", "))
}

const OFFLINE: &str = "Offline. DeskLink will reconnect automatically.";

fn is_offline_message(message: &str) -> bool {
    message == OFFLINE
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
        (OFFLINE, true)
    } else if has(&["trusted device certificate changed"]) {
        (
            "This phone's identity changed. If DeskLink was reinstalled on it, pair it again \
             by scanning the QR code: the old pairing is replaced.",
            true,
        )
    } else if has(&["qr pairing code expired"]) {
        (
            "That QR code expired. Scan the new one shown on this computer.",
            false,
        )
    } else if has(&["no qr pairing code is active"]) {
        (
            "Open the pairing QR code on this computer first, then scan it.",
            false,
        )
    } else if has(&["invalid qr pairing proof"]) {
        (
            "That QR code is not from this computer. Scan the one shown here.",
            false,
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

/// A device's glyph and the word for it, from the identity packet's type
/// ("phone", "tablet", "tv", "laptop", "desktop"). The glyphs are bundled in
/// the app's resources, so every distribution shows the same distinct ones.
fn device_kind(device_type: &str) -> (&'static str, &'static str) {
    match device_type.to_lowercase().as_str() {
        "phone" | "smartphone" => ("desklink-device-phone-symbolic", "Phone"),
        "tablet" => ("desklink-device-tablet-symbolic", "Tablet"),
        "tv" | "television" => ("desklink-device-tv-symbolic", "TV"),
        "laptop" => ("desklink-device-laptop-symbolic", "Laptop"),
        "desktop" => ("desklink-device-desktop-symbolic", "Computer"),
        _ => ("desklink-device-generic-symbolic", "Device"),
    }
}

pub(crate) fn device_type_icon(device_type: &str) -> &'static str {
    device_kind(device_type).0
}

/// "phone" → "Phone", "tv" → "TV".
pub(crate) fn device_type_label(device_type: &str) -> String {
    device_kind(device_type).1.to_string()
}

/// How a device's reachability reads, and the CSS class that goes with it.
///
/// The sidebar, the switcher and every page derive it from here, so they
/// cannot disagree about whether a device is connected.
fn device_status_presentation(device: &DeviceView) -> (&'static str, &'static str) {
    match &device.status {
        // Reachable over the bootstrap link but still negotiating the encrypted
        // feature transport. Calling it plain "Connected" hid a stuck handover.
        DeviceStatus::Paired if !device.feature_transport_ready => ("Connecting…", "connecting"),
        DeviceStatus::Paired => ("Connected", "connected"),
        DeviceStatus::Unreachable => ("Offline", "offline"),
        DeviceStatus::PairRequestedByPeer => ("Wants to pair", "attention"),
        DeviceStatus::PairRequested => ("Waiting…", "connecting"),
        other => (other.label(), "neutral"),
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

/// Whether any widget below `widget` is a label with text.
fn has_label_descendant(widget: &gtk::Widget) -> bool {
    let mut child = widget.first_child();
    while let Some(next) = child {
        if next
            .downcast_ref::<gtk::Label>()
            .is_some_and(|label| !label.label().is_empty())
            || has_label_descendant(&next)
        {
            return true;
        }
        child = next.next_sibling();
    }
    false
}

/// Every widget under `root` whose name starts with `prefix`.
fn named_widgets(root: &gtk::Widget, prefix: &str) -> Vec<gtk::Widget> {
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if widget.widget_name().starts_with(prefix) {
            found.push(widget.clone());
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
    found
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

/// Why an action on `device` cannot run now, if it cannot.
fn unavailable_reason(device: &DeviceView) -> Option<String> {
    match device.status {
        DeviceStatus::Paired if device.feature_transport_ready => None,
        DeviceStatus::Paired => Some(format!("Connecting to {}…", device.name)),
        _ => Some(format!("{} is offline", device.name)),
    }
}

/// Shows a toast in the main window that holds `widget`.
fn toast_from(widget: &impl IsA<gtk::Widget>, message: &str) {
    if let Some(window) = widget.root().and_downcast::<DesklinkWindow>() {
        window.show_feature_toast(message);
    }
}

// ── Small building blocks shared by the pages ─────────────────────────────

/// A label with classes, left-aligned.
fn text(label: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(label)
        .xalign(0.0)
        .use_markup(false)
        .css_classes(classes.to_vec())
        .build()
}

/// The same, ellipsized at the end.
fn ellipsized(label: &str, classes: &[&str]) -> gtk::Label {
    let label = text(label, classes);
    label.set_ellipsize(gtk::pango::EllipsizeMode::End);
    label
}

/// A rounded square with an icon in it: a card's or row's badge.
fn icon_square(icon: &str, size: i32, class: &str) -> gtk::Box {
    let square = gtk::Box::builder()
        .width_request(size)
        .height_request(size)
        .valign(gtk::Align::Center)
        .halign(gtk::Align::Center)
        .css_classes([class])
        .build();
    square.set_hexpand(false);
    square.append(
        &gtk::Image::builder()
            .icon_name(icon)
            .hexpand(true)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .accessible_role(gtk::AccessibleRole::Presentation)
            .build(),
    );
    square
}

/// A button with an icon and a label.
fn icon_button(icon: &str, label: &str, classes: &[&str]) -> gtk::Button {
    let content = gtk::Box::builder()
        .spacing(7)
        .halign(gtk::Align::Center)
        .build();
    content.append(&gtk::Image::from_icon_name(icon));
    content.append(&gtk::Label::new(Some(label)));
    let button = gtk::Button::builder()
        .child(&content)
        .valign(gtk::Align::Center)
        .css_classes(classes.to_vec())
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// A button showing only text, styled like the design's soft chips.
fn soft_button(label: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .css_classes(["soft-button"])
        .build()
}

/// "View All ›": an accent link to another page.
fn page_link(label: &str, page: &str) -> gtk::Button {
    let content = gtk::Box::builder().spacing(2).build();
    content.append(&gtk::Label::new(Some(label)));
    content.append(&gtk::Image::from_icon_name("go-next-symbolic"));
    let button = gtk::Button::builder()
        .child(&content)
        .valign(gtk::Align::Center)
        .action_name("win.show-page")
        .action_target(&page.to_variant())
        .css_classes(["link-button"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(label)]);
    button
}

/// A card: the design's white rounded surface with a soft edge.
fn card(spacing: i32) -> gtk::Box {
    gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(spacing)
        .css_classes(["dl-card"])
        .build()
}

/// Asks before `confirm`ing something that cuts the phone off.
fn confirm(
    parent: &impl IsA<gtk::Widget>,
    heading: &str,
    body: &str,
    action: &str,
    confirm: impl Fn() + 'static,
) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_responses(&[("cancel", "_Cancel"), ("go", action)]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    dialog.choose(parent, gio::Cancellable::NONE, move |response| {
        if response == "go" {
            confirm();
        }
    });
}

/// Asks for files and sends each to `id`.
fn pick_and_send_files(daemon: &DaemonHandle, id: &str, widget: &impl IsA<gtk::Widget>) {
    let Some(root) = widget.root().and_downcast::<gtk::Window>() else {
        return;
    };
    let dialog = gtk::FileDialog::builder()
        .title("Select Files to Send")
        .build();
    let (daemon, id) = (daemon.clone(), id.to_string());
    dialog.open_multiple(Some(&root), gio::Cancellable::NONE, move |result| {
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

/// Lets files be dropped on `target`, sent to the device `resolve` names
/// (with the daemon), or refused with a word why when it names none.
/// Folders are refused too: only files travel.
fn accept_dropped_files(
    target: &impl IsA<gtk::Widget>,
    resolve: impl Fn() -> Option<(DaemonHandle, String)> + 'static,
) {
    let drop = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    let widget = target.as_ref().downgrade();
    drop.connect_drop(move |_, value, _, _| {
        let Ok(files) = value.get::<gtk::gdk::FileList>() else {
            return false;
        };
        let say = |message: &str| {
            if let Some(widget) = widget.upgrade() {
                toast_from(&widget, message);
            }
        };
        let Some((daemon, id)) = resolve() else {
            say("Files can be sent once the phone is connected.");
            return false;
        };
        let (mut sent, mut folders, mut remote) = (0, 0, 0);
        for file in files.files() {
            match file.path() {
                // A local stat only; a remote location has no path at all.
                Some(path) if std::fs::metadata(&path).is_ok_and(|meta| meta.is_dir()) => {
                    folders += 1
                }
                Some(path) => {
                    daemon.send(DaemonCommand::SendFile(id.clone(), path));
                    sent += 1;
                }
                None => remote += 1,
            }
        }
        if folders > 0 {
            say("Folders can't be sent. Drop the files inside instead.");
        } else if remote > 0 {
            say("Only files stored on this computer can be sent.");
        }
        sent > 0
    });
    target.add_controller(drop);
}

/// Debug builds only: the `e2e` verbs that drive the UI itself, for
/// screenshots without a phone. Returns whether `verb` was one of them.
///
/// - `screenshot PATH`: the main window to PATH, every other visible window
///   beside it as `<stem>-<title>.png`.
/// - `fake-device KIND[:TYPE] NAME`: shows a made-up device (`paired`,
///   `paired-turn`, `connecting`, `unreachable`, `pair-request`, `requested`,
///   `discovered`; TYPE `phone`, `tablet`, `tv`, `laptop`, `desktop`);
///   `fake-clear` removes them.
/// - `page NAME`, `select N` (the Nth fake device), `open-terminal N`,
///   `open-switcher`, `window-size W H`, `color-scheme dark|light|default`,
///   `open-pair`, `open-codes`, `nav content|sidebar`,
///   `fake-transfer NAME DONE/TOTAL` (a leading `-` on DONE makes it
///   incoming; TOTAL 0 is still preparing).
#[cfg(debug_assertions)]
fn debug_ui_verb(win: &DesklinkWindow, verb: &str, id: &str, arg: &str) -> bool {
    match verb {
        "screenshot" => debug_screenshot(win, id),
        "fake-device" => {
            let device = debug_fake_device(id, arg);
            // Shown as already seen: no toasts in the screenshots.
            for notification in &device.notifications {
                win.imp()
                    .notified_remote_notifications
                    .borrow_mut()
                    .insert(format!("{}:{}", device.id, notification.id));
            }
            win.imp().fake_devices.borrow_mut().push(device);
            win.imp().snapshots.borrow_mut().clear();
            win.refresh_devices();
        }
        "fake-clear" => {
            win.imp().fake_devices.borrow_mut().clear();
            win.imp().snapshots.borrow_mut().clear();
            win.refresh_devices();
        }
        "select" => {
            let device = id
                .parse::<usize>()
                .ok()
                .and_then(|index| win.imp().fake_devices.borrow().get(index).cloned());
            if let Some(device) = device {
                win.select_device(&device.id);
            }
        }
        "page" => {
            win.imp().device_card.popdown();
            win.show_page(id);
        }
        "open-terminal" => {
            let sessions = crate::terminal_previews::sessions();
            if let Some(info) = id.parse::<usize>().ok().and_then(|n| sessions.get(n)) {
                if let Some(page) = win.imp().terminals.borrow().as_ref() {
                    page.open_detail(win, &info.id);
                }
            }
        }
        "open-switcher" => win.imp().device_card.popup(),
        // "fake-shell-device KIND NAME": a made-up device for the GNOME
        // integration (extension, media, search), not this window.
        "fake-shell-device" => {
            let mut device = debug_fake_device(id, arg);
            if id == "live" {
                device.media_status = Some(crate::device_links::device::MediaStatus {
                    player: "Spotify".into(),
                    player_list: vec!["Spotify".into()],
                    title: "Blurred".into(),
                    artist: "Kiasmos".into(),
                    album: "Blurred EP".into(),
                    is_playing: true,
                    volume: Some(60),
                    position: Some(72_000),
                    length: Some(281_000),
                    can_seek: true,
                    album_art_url: None,
                    album_art: None,
                });
            }
            if id.starts_with("unreachable") {
                device.last_seen = Some(crate::device_links::connection_log::now() - 3600);
            }
            if let Ok(mut fake) = crate::device_links::daemon::handle::DEBUG_SHELL_DEVICES.lock() {
                fake.push(device);
            }
        }
        // "scroll-end PAGE": that page scrolled to its end.
        // "scroll-to PAGE Y": that page scrolled to Y pixels.
        "scroll-to" => {
            if let Some((scroll, _)) = win.imp().bodies.borrow().get(id).cloned() {
                scroll.vadjustment().set_value(arg.parse().unwrap_or(0.0));
            }
        }
        "scroll-end" => {
            if let Some((scroll, _)) = win.imp().bodies.borrow().get(id).cloned() {
                let adjustment = scroll.vadjustment();
                adjustment.set_value(adjustment.upper());
            }
        }
        // New Terminal's menu, with its "Start in" folders.
        "open-new-terminal" => {
            if let Some(page) = win.imp().terminals.borrow().as_ref() {
                for widget in named_widgets(&page.root, "new-terminal") {
                    if let Some(button) = widget.downcast_ref::<adw::SplitButton>() {
                        button.popup();
                    }
                }
            }
        }
        // "open-reply ID": notification ID's reply entry.
        "open-reply" => {
            if let Some(page) = win.imp().notifications.borrow().as_ref() {
                page.open_reply(id);
            }
        }
        // A shell session at the desk without opening a window for it.
        "spawn-terminal" => {
            let created = crate::device_links::pty::registry::registry()
                .lock()
                .map_err(|_| "registry lock".to_string())
                .and_then(|mut registry| registry.create(None, 24, 80, "desk"));
            debug_log!("[E2E] spawn-terminal: {:?}", created.map(|info| info.id));
        }
        // "fake-attach N": the selected device attached to terminal N.
        "fake-attach" => {
            let sessions = crate::terminal_previews::sessions();
            if let (Some(info), Some(device)) = (
                id.parse::<usize>().ok().and_then(|n| sessions.get(n)),
                win.selected_id(),
            ) {
                if let Ok(mut fake) =
                    crate::device_links::webrtc::terminal_bridge::DEBUG_ATTACHED.lock()
                {
                    fake.push((info.id.clone(), device));
                }
            }
        }
        // "desk-note-close ID": the phone dismissing this computer's
        // notification ID (C1, the real path).
        "desk-note-close" => crate::device_links::desk_notifications::close_from_phone(id),
        // "clip-image-in PATH": a phone's copied PNG arriving (the real path).
        "clip-image-in" => crate::device_links::daemon::clipboard::debug_receive_image(id),
        // "clip-in TEXT": a phone's clipboard arriving (the real path).
        "clip-in" => {
            crate::device_links::daemon::clipboard::debug_receive(format!("{id} {arg}").trim());
        }
        // "fake-clip TEXT" / "fake-clip-phone TEXT": a clipboard history item.
        "fake-clip" | "fake-clip-phone" => {
            let text = format!("{id} {arg}");
            crate::device_links::daemon::clipboard::debug_remember(
                text.trim(),
                (verb == "fake-clip-phone").then_some("Pixel 9"),
            );
        }
        // These two write history files: only into an isolated runner's own
        // state directory, never the user's.
        "fake-history" | "fake-received" | "fake-sent"
            if std::env::var_os("XDG_STATE_HOME").is_none() =>
        {
            debug_log!("[E2E] {verb} refused: set XDG_STATE_HOME to an isolated directory");
        }
        // "fake-history N": a few connection-history entries for fake device N.
        // "fake-contacts N": N shared contacts for the selected device, as
        // if its phone had sent them (some with photos).
        "fake-contacts" => {
            use crate::device_links::packet::NetworkPacket;
            let Some(device) = win.selected_id() else {
                return true;
            };
            let count: usize = id.parse().unwrap_or(40);
            let first = [
                "Mara", "Bo", "Åsa", "Lars", "Chen", "Priya", "Omar", "Zoe", "Ines", "Kai", "Noor",
                "Femi", "Yuki", "Diego", "Hana",
            ];
            let last = [
                "Lindqvist",
                "Okafor",
                "Sato",
                "Nguyen",
                "Berg",
                "Silva",
                "Kaur",
            ];
            let photo =
                gtk::gdk_pixbuf::Pixbuf::new(gtk::gdk_pixbuf::Colorspace::Rgb, false, 8, 96, 96)
                    .and_then(|pixbuf| {
                        pixbuf.fill(0x3d7a_c4ff);
                        pixbuf.save_to_bufferv("jpeg", &[]).ok()
                    })
                    .map(|bytes| {
                        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)
                    });
            let uids: Vec<String> = (0..count).map(|n| format!("lookup-{n}")).collect();
            let mut snapshot = NetworkPacket::new("desklink.contacts.response_uids_timestamps");
            snapshot.set("uids", uids.clone());
            for uid in &uids {
                snapshot.set(uid, "1");
            }
            snapshot.set("sharing", true);
            let _ = crate::device_links::contacts::on_snapshot(&device, None, &snapshot);
            let mut cards = NetworkPacket::new("desklink.contacts.response_vcards");
            cards.set("uids", uids.clone());
            for (n, uid) in uids.iter().enumerate() {
                let mut card = format!(
                    "BEGIN:VCARD\nVERSION:3.0\nFN:{} {}\nTEL;TYPE=CELL:+46 70 {:03} {:02} {:02}\n",
                    first[n % first.len()],
                    last[n % last.len()],
                    n,
                    n % 100,
                    (n * 7) % 100
                );
                if n % 3 == 0 {
                    card.push_str("TEL;TYPE=WORK:08-123 456\nEMAIL:someone@example.org\nORG:Acme\nNOTE:Met at the conference\\nlikes tea\n");
                }
                if let (0, Some(photo)) = (n % 4, &photo) {
                    card.push_str(&format!("PHOTO;ENCODING=b;TYPE=JPEG:{photo}\n"));
                }
                card.push_str("END:VCARD");
                cards.set(uid, card);
            }
            crate::device_links::contacts::on_vcards(&device, &cards);
            win.refresh_soon();
        }
        // "fake-snippet TEXT" (underscores for spaces) and "fake-note": F6
        // content without a phone.
        // F8b: "fake-bucket": a Bucket with items in every state for the
        // selected device (shots without a phone); "bucket-on" turns the real
        // one on; "bucket-add PATH" adds a file or folder as a drop would.
        "fake-bucket" => {
            use crate::device_links::folders::{
                self, Item, ItemState, Pair, PairView, Phase, Side,
            };
            let Some(device) = win.selected_id() else {
                return true;
            };
            let root = std::env::var_os("DESKLINK_FAKE_BUCKET")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| glib::home_dir().join("DeskLink Bucket"));
            let item = |path: &str, size: u64, ago: i64, from: Side, state: ItemState| Item {
                dir: false,
                path: path.into(),
                size,
                mtime: 1_761_130_000_000 - ago * 60_000,
                from,
                state,
            };
            let folder = |path: &str, ago: i64| Item {
                dir: true,
                ..item(path, 0, ago, Side::Desk, ItemState::Synced)
            };
            let items = vec![
                folder("Trips", 2),
                folder("Trips/Rome", 3),
                folder("Receipts", 500),
                item(
                    "Trips/Rome/Video.mov",
                    600_000_000,
                    2,
                    Side::Phone,
                    ItemState::Receiving,
                ),
                item(
                    "Trips/Rome/Ticket.pdf",
                    90_000,
                    3,
                    Side::Phone,
                    ItemState::Synced,
                ),
                item("Trips/Map.jpg", 2_400_000, 3, Side::Desk, ItemState::Synced),
                item(
                    "Boarding pass.pdf",
                    182_000,
                    1,
                    Side::Phone,
                    ItemState::Receiving,
                ),
                item(
                    "Sunset at the pier.jpg",
                    3_400_000,
                    4,
                    Side::Phone,
                    ItemState::Synced,
                ),
                item(
                    "Quarterly report.pdf",
                    920_000,
                    15,
                    Side::Desk,
                    ItemState::Sending,
                ),
                item("notes.md", 4_200, 40, Side::Desk, ItemState::Conflict),
                item("Wi-Fi password.txt", 64, 90, Side::Phone, ItemState::Synced),
                item(
                    "github.com 2026-10-22.url",
                    90,
                    120,
                    Side::Desk,
                    ItemState::Waiting,
                ),
                item(
                    "Invoice 2041.pdf",
                    210_000,
                    300,
                    Side::Desk,
                    ItemState::Synced,
                ),
            ];
            let bytes = items.iter().map(|item| item.size).sum();
            let items: Vec<Item> = if id == "empty" { Vec::new() } else { items };
            folders::debug_show(
                vec![PairView {
                    pair: Pair {
                        id: folders::BUCKET_ID.into(),
                        name: "DeskLink Bucket".into(),
                        root,
                        device: device.clone(),
                        include_hidden: false,
                        ignore: Vec::new(),
                        bucket: true,
                    },
                    phase: Phase::UpToDate,
                    files: items.len(),
                    last_sync: Some(glib::real_time() / 1000 - 120_000),
                    conflicts: vec![folders::Conflict {
                        path: "notes.md".into(),
                        copy: "notes (conflict from Pixel 9 2026-10-22).md".into(),
                        at: 0,
                    }],
                    items,
                    bytes,
                    skipped: if id == "empty" {
                        Vec::new()
                    } else {
                        vec!["Old photos".into()]
                    },
                }],
                Vec::new(),
            );
            win.refresh_soon();
        }
        // "fake-bucket-progress [paused]": the fake Bucket's moving items
        // as slow transfers (about 8 MB/s each), for progress shots.
        "fake-bucket-progress" => {
            use crate::device_links::webrtc::transfer_board::{self, TransferRow};
            let Some(device) = win.selected_id() else {
                return true;
            };
            let rows = [
                ("fake-in", "Boarding pass.pdf", true, 1_200_000_000u64),
                ("fake-in2", "Trips/Rome/Video.mov", true, 600_000_000),
                ("fake-out", "Quarterly report.pdf", false, 900_000_000),
            ];
            for (id, name, incoming, total) in rows {
                transfer_board::begin_synced(TransferRow {
                    id: id.into(),
                    device_id: device.clone(),
                    name: name.into(),
                    incoming,
                    done: if incoming { 340_000_000 } else { 0 },
                    total,
                    preparing: false,
                    paused: false,
                });
            }
            if id == "paused" {
                transfer_board::pause_device(&device);
                return true;
            }
            let started = std::time::Instant::now();
            glib::timeout_add_local(std::time::Duration::from_millis(250), move || {
                let moved = (started.elapsed().as_secs_f64() * 8e6) as u64;
                transfer_board::progress("fake-in", 340_000_000 + moved);
                // The second file starts after a second ("Waiting…" first).
                transfer_board::progress("fake-out", moved.saturating_sub(8_000_000));
                glib::ControlFlow::Continue
            });
        }
        // "bucket-open Trips/Rome": the Bucket page inside that folder ("top"
        // for the top); "bucket-select a|b": those items, selecting.
        "bucket-open" => bucket::debug_open(win, if id == "top" { "" } else { id }),
        "bucket-select" => bucket::debug_select(id.split('|').map(str::to_string).collect()),
        "bucket-on" => {
            if let Some(device) = win.selected_id() {
                crate::device_links::folders::turn_on_bucket(&device);
            }
        }
        "bucket-add" => {
            if let Some(device) = win.selected_id() {
                debug_log!("[E2E] bucket-add");
                crate::device_links::folders::import(
                    &device,
                    vec![std::path::PathBuf::from(id)],
                    arg,
                );
            }
        }
        "fake-snippet" => {
            let _ = crate::device_links::notes::add_snippet(&id.replace('_', " "));
            win.refresh_soon();
        }
        "fake-note" => {
            let verb_id = id;
            let id = crate::device_links::notes::new_note(
                "# Release checklist\nShip **F6** after the *real-phone* run.\n\n\
                 - [x] contract\n- [ ] Android side\n- `cargo test` green\n1. tag\n2. push",
            );
            if let (Some(note), "pinned") = (id, verb_id) {
                let _ = crate::device_links::notes::edit_note(&note, |note| note.pinned = true);
            }
            win.refresh_soon();
        }
        // "open-note N": the Nth note in the list, as if clicked.
        "open-note" => {
            let solo = win.imp().main_stack.visible_child_name().as_deref() == Some("solo");
            if solo {
                if let Some(solo) = win.imp().solo.borrow().as_ref() {
                    solo.notes.open_nth(id.parse().unwrap_or(0));
                }
            } else if let Some(page) = win.imp().notes.borrow().as_ref() {
                page.notes.open_nth(id.parse().unwrap_or(0));
            }
        }
        // "open-contact UID": the selected device's contact, as from search.
        "open-contact" => {
            if let Some(device) = win.selected_id() {
                win.show_contact(&device, id);
            }
        }
        "fake-history" => {
            use crate::device_links::connection_log::{
                record_with_path, remember_name, ConnectionEvent,
            };
            let device = id
                .parse::<usize>()
                .ok()
                .and_then(|index| win.imp().fake_devices.borrow().get(index).cloned());
            if let Some(device) = device {
                remember_name(&device.id, &device.name);
                record_with_path(&device.id, ConnectionEvent::Connected, Some("turn"));
                record_with_path(&device.id, ConnectionEvent::TerminalAttached, None);
                record_with_path(&device.id, ConnectionEvent::ScreenViewStarted, None);
                record_with_path(&device.id, ConnectionEvent::Connected, Some("lan"));
            }
        }
        // "fake-received N NAME": a file received from fake device N.
        // "fake-received N PATH" / "fake-sent N PATH": a file in the
        // history of fake device N (PATH absolute, or a name in /tmp).
        "fake-received" | "fake-sent" => {
            let device = id
                .parse::<usize>()
                .ok()
                .and_then(|index| win.imp().fake_devices.borrow().get(index).cloned());
            let path = std::path::Path::new(arg);
            let path = if path.is_absolute() {
                path.to_path_buf()
            } else {
                std::env::temp_dir().join(arg)
            };
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            if let Some(device) = device {
                if verb == "fake-sent" {
                    crate::device_links::transfer_log::record_sent(
                        &device.id,
                        &name,
                        &path,
                        std::fs::metadata(&path).map_or(0, |meta| meta.len()),
                    );
                } else {
                    crate::device_links::transfer_log::record_received(&device.id, &name, &path);
                }
            }
        }
        // "click TEXT": clicks the visible button whose tooltip starts with
        // TEXT (underscores for spaces), e.g. to open the Camera popover.
        "click" => {
            let wanted = id.replace('_', " ");
            let mut stack = vec![win.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if let Some(button) = widget.downcast_ref::<gtk::Button>() {
                    if button.is_mapped()
                        && button
                            .tooltip_text()
                            .is_some_and(|tip| tip.starts_with(&wanted))
                    {
                        button.emit_clicked();
                        break;
                    }
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
        }
        "close-dialog" => {
            if let Some(dialog) = win.visible_dialog() {
                dialog.close();
            }
            let mut stack = vec![win.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
                    popover.popdown();
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
        }
        // "open-viewer N": the selected device's Nth file still on disk.
        "open-viewer" => {
            let selected = win.selected_id().unwrap_or_default();
            let files: Vec<_> = crate::device_links::transfer_log::recent()
                .into_iter()
                .filter(|file| file.device_id == selected && file.path.is_file())
                .collect();
            if let Some(index) = id.parse::<usize>().ok().filter(|n| *n < files.len()) {
                viewer::show(win, &files, index, "Pixel 9");
            }
        }
        // "too-wide N [window]": the widgets on the visible page (or the
        // whole window) whose minimum width is over N px, deepest first (the
        // ones to fix for a narrow window).
        "too-wide" => {
            let limit: i32 = id.parse().unwrap_or(340);
            let mut found = Vec::new();
            let root: gtk::Widget = if arg == "window" {
                win.imp().toast_overlay.get().upcast()
            } else {
                win.imp().page_stack.get().upcast()
            };
            let mut stack = vec![(root, 0)];
            while let Some((widget, depth)) = stack.pop() {
                let (min, _, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                if min > limit && widget.is_visible() && widget.is_mapped() {
                    let wide_child = {
                        let mut child = widget.first_child();
                        let mut any = false;
                        while let Some(next) = child {
                            if next.is_mapped()
                                && next.measure(gtk::Orientation::Horizontal, -1).0 > limit
                            {
                                any = true;
                            }
                            child = next.next_sibling();
                        }
                        any
                    };
                    if !wide_child {
                        found.push(format!(
                            "{min}px {} {:?} {:?}",
                            widget.type_().name(),
                            widget.widget_name(),
                            widget.css_classes()
                        ));
                    }
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    stack.push((next.clone(), depth + 1));
                    child = next.next_sibling();
                }
            }
            debug_log!("[E2E] too-wide {limit}: {found:#?}");
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
        "open-pair" => win.show_discovery_dialog(true),
        "open-codes" => win.show_discovery_dialog(false),
        // "fake-transfer NAME DONE/TOTAL": a transfer row.
        "fake-transfer" => {
            let (done, total) = arg.split_once('/').unwrap_or(("0", "0"));
            transfers::FAKE_ROWS.with(|rows| {
                rows.borrow_mut()
                    .push(crate::device_links::webrtc::transfer_board::TransferRow {
                        id: format!("fake-{}", glib::monotonic_time()),
                        device_id: String::new(),
                        name: id.to_string(),
                        incoming: done.starts_with('-'),
                        done: done.trim_start_matches('-').parse().unwrap_or(0),
                        total: total.parse().unwrap_or(0),
                        preparing: total == "0",
                        paused: false,
                    })
            });
        }
        "size-info" => {
            let imp = win.imp();
            debug_log!(
                "[E2E] window {}x{}, content {}x{}, page stack {}x{}",
                win.width(),
                win.height(),
                imp.content_page.width(),
                imp.content_page.height(),
                imp.page_stack.width(),
                imp.page_stack.height()
            );
            if let Some((scroll, body)) = imp.bodies.borrow().get(id).cloned() {
                let adjustment = scroll.vadjustment();
                debug_log!(
                    "[E2E] {id}: scroll upper {} page {} value {}, slot {}x{}",
                    adjustment.upper(),
                    adjustment.page_size(),
                    adjustment.value(),
                    body.width(),
                    body.height()
                );
            }
            for widget in named_widgets(&imp.page_stack.get().upcast(), "hero-") {
                let (min, nat, _, _) = widget.measure(gtk::Orientation::Horizontal, -1);
                debug_log!(
                    "[E2E] {}: {} wide, min {min}, natural {nat}",
                    widget.widget_name(),
                    widget.width()
                );
            }
        }
        // "a11y": every button, switch and menu button on the shown page with
        // no accessible name (its label, tooltip or an explicit one).
        "a11y" => {
            let mut bad = 0;
            let mut stack = vec![win.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                let interactive = widget.is::<gtk::Button>()
                    || widget.is::<gtk::MenuButton>()
                    || widget.is::<gtk::Switch>()
                    || widget.is::<gtk::ToggleButton>();
                if interactive && widget.is_mapped() && widget.is_visible() {
                    let named = widget
                        .downcast_ref::<gtk::Button>()
                        .map(|b| b.label().is_some_and(|l| !l.is_empty()))
                        .unwrap_or(false)
                        || widget.tooltip_text().is_some_and(|t| !t.is_empty())
                        || widget.has_css_class("has-label")
                        || widget.is::<gtk::Switch>()
                            && widget
                                .parent()
                                .is_some_and(|p| p.is::<gtk::Box>() && p.parent().is_some());
                    let labelled = has_label_descendant(&widget)
                        || widget.type_().name() == "AdwViewSwitcherButton"
                        || widget
                            .ancestor(gtk::WindowControls::static_type())
                            .is_some()
                        || widget.ancestor(gtk::Calendar::static_type()).is_some()
                        // The inner toggle of a menu button is named by it.
                        || widget.parent().is_some_and(|p| {
                            p.is::<gtk::MenuButton>() || p.type_().name() == "AdwSplitButton"
                        });
                    if !named && !labelled {
                        bad += 1;
                        debug_log!(
                            "[E2E] a11y: unnamed {} {:?} in {}",
                            widget.type_().name(),
                            widget.css_classes(),
                            widget
                                .parent()
                                .and_then(|p| p.parent())
                                .map(|p| p.type_().name().to_string())
                                .unwrap_or_default()
                        );
                    }
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
            debug_log!("[E2E] a11y: {bad} unnamed");
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
        // Open popovers are their own surfaces: drawn in under their button.
        for popover in named_popovers(window.upcast_ref()) {
            let Some(bounds) = popover
                .parent()
                .and_then(|anchor| anchor.compute_bounds(&window))
            else {
                continue;
            };
            snapshot.save();
            snapshot.translate(&gtk::graphene::Point::new(
                bounds.x(),
                bounds.y() + bounds.height(),
            ));
            gtk::WidgetPaintable::new(Some(&popover)).snapshot(
                &snapshot,
                f64::from(popover.width()),
                f64::from(popover.height()),
            );
            snapshot.restore();
        }
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

/// The popovers open under `root`.
#[cfg(debug_assertions)]
fn named_popovers(root: &gtk::Widget) -> Vec<gtk::Popover> {
    let mut found = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(widget) = stack.pop() {
        if let Some(popover) = widget.downcast_ref::<gtk::Popover>() {
            if popover.is_visible() {
                found.push(popover.clone());
            }
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            child = next.next_sibling();
            stack.push(next);
        }
    }
    found
}

#[cfg(debug_assertions)]
fn debug_fake_device(kind: &str, name: &str) -> DeviceView {
    use crate::device_links::device::{BatteryStatus, DesktopSession, DeviceNotification};
    use crate::device_links::device_info::DeviceInfo;
    // "KIND:TYPE", e.g. "paired:tablet"; the type defaults to a phone.
    let (kind, device_type) = kind.split_once(':').unwrap_or((kind, "phone"));
    let info = DeviceInfo {
        id: format!("fake{:028x}", glib::monotonic_time()),
        name: if name.is_empty() { "Pixel 9" } else { name }.replace('_', " "),
        device_type: device_type.to_string(),
        protocol_version: 9,
        incoming_capabilities: FeatureRegistry::desktop().outgoing_capabilities().to_vec(),
        outgoing_capabilities: FeatureRegistry::desktop().incoming_capabilities().to_vec(),
        instance_id: None,
    };
    let mut device = DeviceView::from_info(&info, "192.168.1.41".to_string(), true);
    device.battery_status = Some(BatteryStatus {
        current_charge: 82,
        is_charging: true,
        battery_quantity: None,
        threshold_event: None,
        charge_time_remaining: Some(25 * 60_000),
    });
    device.notifications = vec![
        DeviceNotification {
            id: "1".into(),
            app_name: "Signal".into(),
            title: "Mara".into(),
            text: "Are you deploying today?".into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: Some("r1".into()),
            actions: vec![],
            silent: false,
        },
        DeviceNotification {
            id: "2".into(),
            app_name: "GitHub".into(),
            title: "Pull request approved".into(),
            text: "derpx06/DeskLink #48 · rate limiting".into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: None,
            actions: vec![],
            silent: false,
        },
        DeviceNotification {
            id: "3".into(),
            app_name: "Calendar".into(),
            title: "Standup in 10 minutes".into(),
            text: "Meet · 11:00 – 11:15".into(),
            ticker: String::new(),
            is_clearable: true,
            request_reply_id: None,
            actions: vec!["Snooze".into()],
            silent: false,
        },
    ];
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
        "connecting" => device.status = DeviceStatus::Paired,
        _ => {
            device.status = DeviceStatus::Paired;
            device.feature_transport_ready = true;
            device.connection_path = Some(
                match kind {
                    "paired-turn" => "turn",
                    _ => "lan",
                }
                .into(),
            );
            device.terminal_access = true;
            device.rtt_ms = Some(9);
            if kind == "live" || kind == "extended" {
                device.terminal_sessions_attached = 1;
                device.desktop_session = DesktopSession::Viewing;
            }
            // "extended": a second display, with sound going and a capture
            // asked for — every bar the overview can show.
            if kind == "extended" {
                device.desktop_extended = true;
                device.sound = Some(crate::device_links::device::SoundUse {
                    sending: Some("system".into()),
                    hearing: Some("mic".into()),
                    hearing_live: true,
                });
                device.capture = Some(crate::device_links::device::CaptureUse {
                    kind: "scan".into(),
                    receiving: false,
                });
            }
        }
    }
    // "ringing" / "talking": a call on the phone.
    use crate::device_links::device::{CallState, CallStatus, PhoneCall};
    let call = match kind {
        "ringing" => Some(CallState::Ringing),
        "talking" => Some(CallState::Talking),
        _ => None,
    };
    if let Some(state) = call {
        device.call_status = Some(CallStatus {
            calls: vec![PhoneCall {
                call_id: "fake-call".into(),
                state,
                contact_name: Some("Mara Lindqvist".into()),
                number: Some("+46 70 123 45 67".into()),
                started_at: (state == CallState::Talking)
                    .then(|| glib::real_time() / 1000 - 83_000),
                incoming: true,
            }],
            muted: false,
            silenced: false,
        });
    }
    device
}

#[cfg(test)]
mod tests {
    #[test]
    fn pairing_errors_are_said_in_plain_words_and_the_reinstall_hint_is_right() {
        for (raw, needle) in [
            (
                "Trusted device certificate changed",
                "pair it again by scanning the QR code",
            ),
            ("The QR pairing code expired", "Scan the new one"),
            ("No QR pairing code is active", "Open the pairing QR code"),
            ("Invalid QR pairing proof", "not from this computer"),
        ] {
            let (words, _) = super::friendly_error(raw);
            assert!(words.contains(needle), "{raw}: {words}");
            assert!(
                !words.to_lowercase().contains("unpair"),
                "never tells the user to unpair first: {words}"
            );
        }
        assert!(
            super::friendly_error("Trusted device certificate changed").1,
            "shown as a banner"
        );
    }

    use super::*;

    #[test]
    fn resynced_notifications_never_pop_up() {
        assert!(pops_up(true, true, false));
        assert!(
            !pops_up(true, true, true),
            "silent: a resync after reconnecting"
        );
        assert!(!pops_up(false, true, false), "already announced");
        assert!(!pops_up(true, false, false), "hidden for this phone");
    }
    use crate::device_links::device_info::DeviceInfo;
    use crate::device_links::packet::{PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_PING};

    fn phone() -> DeviceView {
        let info = DeviceInfo {
            id: "0123456789abcdef0123456789abcdef".to_string(),
            name: "Phone".to_string(),
            device_type: "phone".to_string(),
            protocol_version: 9,
            incoming_capabilities: vec![PACKET_TYPE_PING.to_string()],
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        DeviceView::from_info(&info, "127.0.0.1".to_string(), true)
    }

    #[test]
    fn feature_tiles_require_both_a_local_sender_and_peer_receiver() {
        let device = phone();
        assert!(can_send_feature(&device, PACKET_TYPE_PING));
        assert!(!can_send_feature(&device, PACKET_TYPE_MOUSEPAD_REQUEST));
    }

    /// The page used to render a hardcoded green "● Connected" for every
    /// paired device, so an unreachable phone read as connected right next to
    /// a sidebar row that said otherwise. Both now derive from this function.
    #[test]
    fn status_presentation_never_reports_an_unreachable_device_as_connected() {
        let mut device = phone();
        device.status = DeviceStatus::Unreachable;
        assert_eq!(device_status_presentation(&device), ("Offline", "offline"));

        // Paired but the WebRTC handover has not finished.
        device.status = DeviceStatus::Paired;
        device.feature_transport_ready = false;
        assert_eq!(
            device_status_presentation(&device),
            ("Connecting…", "connecting")
        );

        device.feature_transport_ready = true;
        assert_eq!(
            device_status_presentation(&device),
            ("Connected", "connected")
        );
    }

    /// Every action on an offline or connecting phone says why it is
    /// unavailable, rather than failing after the click.
    #[test]
    fn actions_say_why_they_are_unavailable() {
        let mut device = phone();
        device.status = DeviceStatus::Unreachable;
        assert_eq!(
            unavailable_reason(&device).as_deref(),
            Some("Phone is offline")
        );
        device.status = DeviceStatus::Paired;
        assert_eq!(
            unavailable_reason(&device).as_deref(),
            Some("Connecting to Phone…")
        );
        device.feature_transport_ready = true;
        assert_eq!(unavailable_reason(&device), None);
    }

    #[test]
    fn the_live_banner_says_what_the_phone_is_doing() {
        use crate::device_links::device::DesktopSession;
        let mut device = phone();
        assert_eq!(session_summary(&device), None);
        device.terminal_sessions_attached = 1;
        device.desktop_session = DesktopSession::Viewing;
        assert_eq!(
            session_summary(&device).as_deref(),
            Some("in a terminal, viewing your screen")
        );
        device.terminal_sessions_attached = 2;
        device.desktop_session = DesktopSession::Controlling;
        assert_eq!(
            session_summary(&device).as_deref(),
            Some("in 2 terminals, controlling your screen")
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
    fn daemon_errors_read_as_plain_words() {
        let (offline, lasting) = friendly_error("Network is unreachable (os error 101)");
        assert!(is_offline_message(&offline), "{offline}");
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

    #[test]
    fn every_device_type_has_its_own_glyph_and_name() {
        let kinds: Vec<_> = ["phone", "tablet", "tv", "laptop", "desktop", "unknown"]
            .iter()
            .map(|kind| device_kind(kind))
            .collect();
        let glyphs: std::collections::HashSet<_> = kinds.iter().map(|(glyph, _)| glyph).collect();
        assert_eq!(glyphs.len(), 6, "a tablet must not look like a phone");
        assert_eq!(device_kind("tablet").1, "Tablet");
        assert_eq!(device_kind("TV").1, "TV");
        assert_eq!(device_kind("toaster"), device_kind("unknown"));
        // Every glyph is bundled in the resources.
        let resources = include_str!("../desklink.gresource.xml");
        for (glyph, _) in kinds {
            assert!(
                resources.contains(&format!("{glyph}.svg")),
                "{glyph} is not bundled"
            );
        }
    }

    /// The sidebar, top to bottom: Home, Files, Notes & Clipboard,
    /// Notifications, Contacts, Terminals, then the Settings footer. The
    /// phone's navigation uses the same names in the same order.
    #[test]
    fn the_sidebar_lists_the_six_pages_then_settings() {
        let titles: Vec<_> = PAGES.iter().map(|(_, title, _)| *title).collect();
        assert_eq!(
            titles,
            [
                "Home",
                "Files",
                "Notes & Clipboard",
                "Notifications",
                "Contacts",
                "Terminals",
                "Settings"
            ]
        );
        let names: Vec<_> = PAGES.iter().map(|(name, _, _)| *name).collect();
        assert_eq!(&names[MAIN_PAGES..], ["settings"]);
        assert_eq!(MAIN_PAGES, 6);
    }

    /// The tabs of Notes & Clipboard are addressed by name, every other name
    /// is a page of the sidebar.
    /// `win.send-again` acts only on a retry that was offered.
    #[test]
    fn send_again_ignores_a_path_nobody_offered() {
        assert!(!take_retry("phone\n/etc/passwd"));
        offer_retry("phone\n/home/u/a.bin");
        assert!(
            !take_retry("phone\n/etc/passwd"),
            "another path stays refused"
        );
        assert!(take_retry("phone\n/home/u/a.bin"), "the offered one works");
        assert!(!take_retry("phone\n/home/u/a.bin"), "once");
        for n in 0..MAX_RETRIES + 5 {
            offer_retry(&format!("p\n/f{n}"));
        }
        assert!(!take_retry("p\n/f0"), "the oldest offers are forgotten");
        assert!(take_retry(&format!("p\n/f{}", MAX_RETRIES + 4)));
    }

    #[test]
    fn a_saved_file_says_downloads_even_when_downloads_is_a_link() {
        use std::path::Path;
        let base = std::env::temp_dir().join(format!("desklink-saved-{}", uuid::Uuid::new_v4()));
        let real = base.join("disk/files");
        std::fs::create_dir_all(&real).unwrap();
        let link = base.join("Downloads");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let file = std::fs::canonicalize(&real).unwrap().join("a.pdf");
        assert_eq!(saved_in_with(&file, Some(&link)), "Downloads");
        assert_eq!(saved_in_with(&file, None), "files");
        assert_eq!(
            saved_in_with(Path::new("/home/u/Music/a.pdf"), Some(&link)),
            "Music"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A worker that panicked ends the wait; a slow one is waited for.
    #[test]
    fn a_dead_worker_ends_the_wait_for_its_result() {
        let (sender, receiver) = std::sync::mpsc::channel::<u32>();
        assert_eq!(try_result(&receiver), Poll::Waiting);
        sender.send(7).unwrap();
        assert_eq!(try_result(&receiver), Poll::Done(7));
        let worker = std::thread::spawn(move || {
            let _sender = sender;
            panic!("the worker fell over");
        });
        assert!(worker.join().is_err());
        assert_eq!(try_result(&receiver), Poll::Gone);
    }

    #[test]
    fn the_notes_and_clipboard_tabs_are_addressed_by_name() {
        assert_eq!(resolve_page("clipboard"), ("notes", Some("clipboard")));
        assert_eq!(resolve_page("snippets"), ("notes", Some("snippets")));
        for (name, _, _) in PAGES {
            assert_eq!(resolve_page(name), (*name, None));
        }
        let tabs: Vec<_> = notes_clipboard::TABS.iter().map(|(_, t, _)| *t).collect();
        assert_eq!(tabs, ["Notes", "Snippets", "Clipboard history"]);
        assert_eq!(notes_clipboard::tab_of("clipboard"), Some("clipboard"));
        assert_eq!(notes_clipboard::tab_of("files"), None);
    }
}
