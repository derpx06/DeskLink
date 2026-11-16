//! The GObject implementation of `DesklinkWindow`: its template children,
//! the per-window state cells, and the class and widget setup.
//!
//! Sibling edges: `navigation`, `refresh`, `notices` (its own methods), `blocks`.

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
    pub sidebar_header: TemplateChild<adw::HeaderBar>,
    #[template_child]
    pub content_page: TemplateChild<adw::NavigationPage>,
    #[template_child]
    pub drop_hint: TemplateChild<gtk::Label>,
    #[template_child]
    pub content_search: TemplateChild<gtk::Button>,
    #[template_child]
    pub sidebar_search: TemplateChild<gtk::Button>,
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
    /// The device the person chose (or the remembered one); see
    /// [`device_to_show`].
    pub chosen_device_id: RefCell<Option<String>>,
    /// The page shown, one of [`PAGES`]' names.
    pub page: RefCell<String>,
    /// What each page was last drawn from; a page is redrawn only when
    /// this changes.
    pub snapshots: RefCell<HashMap<String, String>>,
    /// A mouse button or a finger is down on the window: nothing is
    /// redrawn under it (that would drop the click), but after it lifts.
    pub pointer_held: Cell<bool>,
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
    /// The standing problem (`platform::sticky_notice`) the banner shows
    /// now, if any, and the ones the person dismissed this run.
    pub sticky_id: RefCell<Option<&'static str>>,
    /// The standing problem's install command: its banner button copies it.
    pub sticky_command: RefCell<Option<String>>,
    pub sticky_dismissed: RefCell<std::collections::HashSet<&'static str>>,
    /// The last error toast and when, so a repeating error toasts once.
    pub last_error_toast: RefCell<Option<(String, std::time::Instant)>>,
    pub notified_pair_requests: RefCell<HashSet<String>>,
    pub notified_remote_notifications: RefCell<announce::Tracker>,
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
    /// The window has been shown before (its first frame is the layout
    /// alone; see `set_daemon`).
    pub shown_once: Cell<bool>,
    /// Debug builds only: devices injected by the `e2e` action's
    /// `fake-device` verb, for screenshots without a phone.
    #[cfg(debug_assertions)]
    pub fake_devices: RefCell<Vec<DeviceView>>,
    /// The welcome tour on show, if it is.
    pub(super) onboarding: RefCell<Option<std::rc::Rc<super::onboarding::Onboarding>>>,
    /// Whether this run has decided to show the first-run tour.
    pub onboarding_decided: Cell<bool>,
    /// The tour ended on the pairing page: it shows the last step's bar.
    pub onboarding_to_pairing: Cell<bool>,
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
        let last = sidebar::load_last_device();
        // A phone was paired last time: the first frame is the real
        // layout (sidebar, an empty page) while the phones are read, not
        // an empty welcome page.
        if last.is_some() {
            self.main_stack.set_visible_child_name("devices");
            obj.update_title();
        }
        self.chosen_device_id.replace(last.clone());
        self.selected_device_id.replace(last);
        remember_window_size(&obj);
        // Window buttons or the window icon at the start (Xfce's
        // "icon,menu:minimize,maximize,close") sit in the sidebar's
        // header beside its two buttons: "DeskLink" no longer fits there
        // and read "Des…". The title is left out then; the window's own
        // title still says DeskLink.
        if let Some(settings) = gtk::Settings::default() {
            let header = self.sidebar_header.get();
            let fit = move |settings: &gtk::Settings| {
                let layout = settings.gtk_decoration_layout().unwrap_or_default();
                header.set_show_title(!crowded_start(&layout));
            };
            fit(&settings);
            settings.connect_gtk_decoration_layout_notify(fit);
        }

        calls::install_shortcuts(&obj);
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

        // The sidebar has the search field; with the sidebar folded away
        // the page's own header does.
        self.split_view
            .bind_property("collapsed", &*self.content_search, "visible")
            .sync_create()
            .build();
        self.sidebar.replace(Some(sidebar::Sidebar::new(&obj)));
        sidebar::install_window_drop(&obj);
        obj.hold_redraws_while_pressed();
        // The tile row follows the window as it is resized.
        obj.connect_default_width_notify(|window| {
            let window = window.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(window) = window.upgrade() {
                    window.apply_tile_columns();
                }
            });
        });
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
            // A new shared shell here. The target ("shell") is kept for
            // the existing call sites; agents start from the phone only.
            gio::ActionEntry::builder("new-terminal")
                .parameter_type(Some(glib::VariantTy::STRING))
                .activate(|window: &super::DesklinkWindow, _, _| {
                    match crate::device_links::pty::desk::new_desk_session() {
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
            // Ctrl+1 to Ctrl+6: a page by name, from the keyboard only.
            gio::ActionEntry::builder("page-shortcut")
                .parameter_type(Some(glib::VariantTy::STRING))
                .activate(|window: &super::DesklinkWindow, _, parameter| {
                    let Some(page) = parameter.and_then(|value| value.get::<String>()) else {
                        return;
                    };
                    let paired = window.selected_is_paired();
                    if page_shortcut_applies(&page, window.visible_dialog().is_some(), paired) {
                        window.show_page(&page);
                    }
                })
                .build(),
            // Ctrl+O: choose files and send them to the selected phone.
            gio::ActionEntry::builder("send-files")
                .activate(|window: &super::DesklinkWindow, _, _| {
                    // Not over a dialog, as Ctrl+N is not.
                    if window.visible_dialog().is_some() {
                        return;
                    }
                    let (Some(daemon), Some(id)) = (window.daemon(), window.selected_id()) else {
                        window.show_feature_toast("Pair a phone to send files");
                        return;
                    };
                    let device = window
                        .known_devices(&daemon)
                        .into_iter()
                        .find(|device| device.id == id);
                    match device {
                        Some(device)
                            if device.trusted
                                && can_send_feature(
                                    &device,
                                    crate::device_links::packet::PACKET_TYPE_SHARE_REQUEST,
                                ) =>
                        {
                            match unavailable_reason(&device) {
                                Some(reason) => window.show_feature_toast(&reason),
                                None => pick_and_send_files(&daemon, &id, window),
                            }
                        }
                        _ => window.show_feature_toast("Pair a phone to send files"),
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
            // "Get started": what DeskLink does, one card each.
            gio::ActionEntry::builder("guide")
                .activate(|window: &super::DesklinkWindow, _, _| {
                    if window.visible_dialog().is_none() {
                        super::guide::show(window);
                    }
                })
                .build(),
            gio::ActionEntry::builder("welcome-tour")
                .activate(|window: &super::DesklinkWindow, _, _| {
                    window.start_onboarding(0);
                    window.refresh_devices();
                })
                .build(),
            gio::ActionEntry::builder("dial-pad")
                .activate(|window: &super::DesklinkWindow, _, _| window.show_dial_pad())
                .build(),
            gio::ActionEntry::builder("pair-new")
                .activate(|window: &super::DesklinkWindow, _, _| window.show_discovery_dialog(true))
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
            // Ctrl+F: the terminal search on the Terminals page; anywhere
            // else (and not while typing, where Ctrl+F is the text's)
            // "Find anything".
            gio::ActionEntry::builder("toggle-search")
                .activate(|window: &super::DesklinkWindow, _, _| {
                    // Not over a dialog (the palette is one: Ctrl+K closes it).
                    if palette::is_open() || window.visible_dialog().is_some() {
                        return;
                    }
                    if window.imp().page.borrow().as_str() == "terminals"
                        && window.imp().main_stack.visible_child_name().as_deref()
                            == Some("devices")
                    {
                        if let Some(page) = window.imp().terminals.borrow().as_ref() {
                            page.focus_search();
                        }
                    } else if !palette::typing_in(window) {
                        palette::toggle(window, "");
                    }
                })
                .build(),
            // Ctrl+K: "Find anything".
            gio::ActionEntry::builder("palette")
                .activate(|window: &super::DesklinkWindow, _, _| palette::toggle(window, ""))
                .build(),
        ]);

        let window = obj.downgrade();
        self.error_banner.connect_button_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                // A standing problem's button dismisses it; an error's
                // opens the details.
                if window.dismiss_sticky_notice() {
                    return;
                }
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
