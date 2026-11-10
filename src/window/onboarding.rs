//! The first-run welcome (`docs/UI-REDESIGN.md` 3.1 and 3.10, the phone's
//! `screen/onboarding/`): five tour pages, "Set up this computer", then the
//! existing pairing page with its QR code as the last step.
//!
//! Shown once, on a computer with no paired phone whose settings do not say
//! it was seen ([`show_on_start`]); the main menu's "Welcome tour" replays it.
//! Pages change only when the person asks (no timer). The scenes are GTK
//! widgets on a fixed canvas, coloured by the kit's tokens; their few CSS
//! animations are finite and are not built at all when animations are off.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::ui_kit::{button, icon_square, status_pill, ButtonKind, Status, Tone};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::platform::{Check, Distro, Level};
use crate::shell_dbus::ExtensionStatus;

/// One page of the tour: the small line above the title, the title, the body.
pub(super) struct TourPage {
    pub kicker: &'static str,
    pub title: &'static str,
    pub body: &'static str,
}

/// The tour, in order (the phone's five pages, said from the computer's side).
pub(super) const TOUR: [TourPage; 5] = [
    TourPage {
        kicker: "Welcome to DeskLink",
        title: "Your phone and computer, together",
        body: "Pair your phone once. After that, its notifications, screen and files are here, \
               and this computer is one tap away on the phone.",
    },
    TourPage {
        kicker: "Screen and control",
        title: "See it. Steer it.",
        body: "Show your phone's screen in a window here. Or share this screen with your phone, \
               and use the phone as its touchpad and keyboard.",
    },
    TourPage {
        kicker: "Terminals",
        title: "Terminals that follow you",
        body: "Start a terminal on your phone and it opens here too, or pick up on the phone \
               one you started at this computer. Both show the same session.",
    },
    TourPage {
        kicker: "Share",
        title: "Files, copies and notes, both ways",
        body: "Drop files on this window to send them. Copy on one device and paste on the \
               other, and keep notes that show up on both.",
    },
    TourPage {
        kicker: "Private",
        title: "Yours, end to end",
        body: "What passes between your phone and this computer is encrypted, only phones you \
               pair can connect, and there is no account to make.",
    },
];

/// The step after the tour: what this computer needs.
pub(super) const SETUP: usize = TOUR.len();
pub(super) const SETUP_KICKER: &str = "Set up";
pub(super) const SETUP_TITLE: &str = "Set up this computer";
pub(super) const SETUP_BODY: &str = "DeskLink checked what it needs on this computer. Fix what \
    you like now, or later in Settings. You can go on either way.";

/// The tour is for someone who has not paired and has not seen it; a paired
/// person never sees it unasked.
pub(super) fn show_on_start(seen: bool, any_paired: bool) -> bool {
    !seen && !any_paired
}

/// How many segments the progress shows: the tour, the setup step and, when
/// it ends in pairing, the pairing page.
pub(super) fn segment_count(for_pairing: bool) -> usize {
    SETUP + 1 + usize::from(for_pairing)
}

/// A move through the steps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Nav {
    Next,
    Back,
    Skip,
}

/// Where `nav` goes from `step`: another step, or `None` to leave (to the
/// pairing page, or back to the phones on a replay). Skip leaves the tour for
/// the setup step; from the setup step it leaves.
pub(super) fn go(step: usize, nav: Nav) -> Option<usize> {
    match nav {
        Nav::Next if step < SETUP => Some(step + 1),
        Nav::Back => Some(step.saturating_sub(1)),
        Nav::Skip if step < SETUP => Some(SETUP),
        Nav::Next | Nav::Skip => None,
    }
}

/// The keys of the onboarding: Enter and the forward arrow go on, the back
/// arrow goes back, Esc skips. `rtl` swaps the arrows.
pub(super) fn key_nav(key: gdk::Key, rtl: bool) -> Option<Nav> {
    let (forward, backward) = if rtl {
        (gdk::Key::Left, gdk::Key::Right)
    } else {
        (gdk::Key::Right, gdk::Key::Left)
    };
    match key {
        gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::Page_Down => Some(Nav::Next),
        gdk::Key::Page_Up => Some(Nav::Back),
        gdk::Key::Escape => Some(Nav::Skip),
        key if key == forward => Some(Nav::Next),
        key if key == backward => Some(Nav::Back),
        _ => None,
    }
}

/// The main button's words on `step`.
pub(super) fn next_label(step: usize, for_pairing: bool) -> &'static str {
    match step {
        step if step < SETUP => "Next",
        _ if for_pairing => "Continue",
        _ => "Done",
    }
}

// ---- Set up this computer: the pure part ------------------------------------

/// What a setup row's button does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Fix {
    None,
    /// Start at login, with its current state.
    Switch(bool),
    /// Shows a notification, so the person sees they work.
    TestNotification,
    /// Installs and turns on DeskLink's top-bar extension.
    TurnOnExtension,
    /// Asks GNOME to log out (GNOME asks to confirm).
    LogOut,
    /// Copies an install command.
    Copy(String),
}

/// One row of the setup step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct SetupRow {
    pub id: &'static str,
    pub icon: &'static str,
    pub title: &'static str,
    pub status: Status,
    pub status_words: &'static str,
    pub detail: String,
    pub fix: Fix,
}

fn status_of(level: Level) -> (Status, &'static str) {
    match level {
        Level::Ok => (Status::Ok, "Ready"),
        Level::Limited => (Status::Warn, "Needs a step"),
        Level::Missing => (Status::Err, "Not available"),
    }
}

/// The facts the setup step is drawn from.
pub(super) struct SetupFacts<'a> {
    pub checks: &'a [Check],
    pub start_at_login: bool,
    /// DeskLink's top-bar extension; `None` where GNOME is not running.
    pub extension: Option<ExtensionStatus>,
    pub gnome: bool,
    pub distro: Option<Distro>,
}

/// The rows of "Set up this computer", in order: start at login,
/// notifications, the top bar (GNOME), screen sharing, Glide, pairing keys,
/// the GStreamer plugins. Only what [`crate::platform`] found is said.
pub(super) fn setup_rows(facts: &SetupFacts) -> Vec<SetupRow> {
    let find = |id: &str| facts.checks.iter().find(|check| check.id == id);
    let install = |check: &Check| {
        check
            .install
            .filter(|_| check.level != Level::Ok)
            .map(|packages| packages.command(facts.distro))
    };
    let with_install = |check: &Check| match install(check) {
        Some(command) => format!("{}\n\nInstall: {command}", check.detail),
        None => check.detail.clone(),
    };
    let copy_or_none = |check: &Check| install(check).map_or(Fix::None, Fix::Copy);
    let mut rows = Vec::new();

    // Start at login.
    let autostart = find("autostart");
    let (status, status_words, detail) = match autostart {
        Some(check) if check.level != Level::Ok => {
            (Status::Warn, "Needs a step", check.detail.clone())
        }
        _ if facts.start_at_login => (
            Status::Ok,
            "On",
            "DeskLink starts in the background when you log in, so your phone can reach this \
             computer after a restart."
                .to_string(),
        ),
        _ => (
            Status::Idle,
            "Off",
            "Your phone can reach this computer only while DeskLink is open.".to_string(),
        ),
    };
    rows.push(SetupRow {
        id: "autostart",
        icon: "desklink-restart-alt-symbolic",
        title: "Start at login",
        status,
        status_words,
        detail,
        fix: Fix::Switch(facts.start_at_login),
    });

    // Notifications.
    if let Some(check) = find("notifications") {
        let (status, status_words) = status_of(check.level);
        rows.push(SetupRow {
            id: "notifications",
            icon: "desklink-notifications-symbolic",
            title: "Notifications",
            status,
            status_words,
            detail: if check.level == Level::Ok {
                "Your phone's notifications and DeskLink's alerts show here.".to_string()
            } else {
                with_install(check)
            },
            fix: if check.level == Level::Ok {
                Fix::TestNotification
            } else {
                copy_or_none(check)
            },
        });
    }

    // The top bar (GNOME only).
    if let Some(extension) = &facts.extension {
        let (status, status_words, detail, fix) = match extension {
            ExtensionStatus::Active => (
                Status::Ok,
                "On",
                "Your phone's battery, connection and quick actions are in the top bar.",
                Fix::None,
            ),
            ExtensionStatus::NeedsLogin => (
                Status::Warn,
                "Needs a step",
                "Installed. GNOME shows it after you log out and back in.",
                Fix::LogOut,
            ),
            ExtensionStatus::Off => (
                Status::Idle,
                "Off",
                "Shows your phone's battery, connection and quick actions in the top bar. \
                 Turning it on also allows GNOME user extensions, and GNOME shows it after you \
                 log out and back in. Nothing changes until you click.",
                Fix::TurnOnExtension,
            ),
        };
        rows.push(SetupRow {
            id: "extension",
            icon: "desklink-devices-symbolic",
            title: "Top bar menu",
            status,
            status_words,
            detail: detail.to_string(),
            fix,
        });
    }

    // Screen sharing.
    if let Some(check) = find("share") {
        let (status, status_words) = status_of(check.level);
        let through_portal = check.detail.starts_with("Through the desktop portal");
        let detail = match check.level {
            Level::Ok if through_portal && facts.gnome => {
                "The first time your phone asks to see or control this screen, GNOME shows a \
                 dialog here. Allow it, with Allow Remote Interaction turned on so the phone can \
                 control it. You are not asked again until DeskLink restarts."
                    .to_string()
            }
            Level::Ok if through_portal => {
                "The first time your phone asks to see or control this screen, your desktop \
                 shows a dialog here to allow it."
                    .to_string()
            }
            _ => with_install(check),
        };
        rows.push(SetupRow {
            id: "share",
            icon: "desklink-desktop-windows-symbolic",
            title: "Screen sharing",
            status,
            status_words,
            detail,
            fix: copy_or_none(check),
        });
    }

    // Glide.
    if let Some(check) = find("glide") {
        let (status, status_words) = status_of(check.level);
        let dialog = if facts.gnome {
            "GNOME's Capture Input dialog"
        } else {
            "a dialog to allow input capture"
        };
        rows.push(SetupRow {
            id: "glide",
            icon: "desklink-mouse-symbolic",
            title: "Glide",
            status,
            status_words,
            detail: if check.level == Level::Ok {
                format!(
                    "Move your mouse across to the phone, off an edge of this screen. Turn it on \
                     in Settings after pairing; it then shows {dialog}, and once you allow it \
                     you are not asked again until DeskLink restarts."
                )
            } else {
                format!("Move your mouse across to the phone. {}", check.detail)
            },
            fix: Fix::None,
        });
    }

    // Where the pairing keys live (no keyring is used).
    if let Some(check) = find("storage") {
        let (status, status_words) = status_of(check.level);
        rows.push(SetupRow {
            id: "keys",
            icon: "desklink-encrypted-symbolic",
            title: "Pairing keys",
            status,
            status_words,
            detail: if check.level == Level::Ok {
                "Kept in DeskLink's own folder, readable only by you. No keyring is needed."
                    .to_string()
            } else {
                check.detail.clone()
            },
            fix: Fix::None,
        });
    }

    // GStreamer: the connection, the phone's screen, sound.
    let parts: Vec<&Check> = ["link", "view", "sound"]
        .iter()
        .filter_map(|id| find(id))
        .collect();
    if !parts.is_empty() {
        let worst = parts
            .iter()
            .map(|check| check.level)
            .max_by_key(|level| match level {
                Level::Ok => 0,
                Level::Limited => 1,
                Level::Missing => 2,
            })
            .unwrap_or(Level::Ok);
        let (status, status_words) = status_of(worst);
        let mut commands: Vec<String> = Vec::new();
        for command in parts.iter().filter_map(|check| install(check)) {
            if !commands.contains(&command) {
                commands.push(command);
            }
        }
        let mut detail = if worst == Level::Ok {
            "GStreamer has what DeskLink needs for the connection, the phone's screen and sound."
                .to_string()
        } else {
            parts
                .iter()
                .filter(|check| check.level != Level::Ok)
                .map(|check| check.detail.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        };
        if !commands.is_empty() {
            detail.push_str(&format!("\n\nInstall: {}", commands.join("\n")));
        }
        rows.push(SetupRow {
            id: "plugins",
            icon: "desklink-movie-symbolic",
            title: "Video and sound plugins",
            status,
            status_words,
            detail,
            fix: if commands.is_empty() {
                Fix::None
            } else {
                Fix::Copy(commands.join("\n"))
            },
        });
    }
    rows
}

/// The setup facts of this computer, or `None` before the probe has finished.
fn real_facts() -> Option<(std::sync::Arc<Vec<Check>>, bool, Option<ExtensionStatus>)> {
    let checks = crate::platform::report()?;
    let start_at_login = crate::device_links::config::app_settings()
        .start_at_login
        .unwrap_or(true);
    Some((checks, start_at_login, extension_state()))
}

fn extension_state() -> Option<ExtensionStatus> {
    #[cfg(debug_assertions)]
    if let Some(fake) = DEBUG_EXTENSION.with(|fake| fake.borrow().clone()) {
        return fake;
    }
    crate::shell_dbus::extension_possible().then(crate::shell_dbus::extension_status)
}

fn gnome() -> bool {
    #[cfg(debug_assertions)]
    if let Some(fake) = DEBUG_EXTENSION.with(|fake| fake.borrow().clone()) {
        return fake.is_some();
    }
    crate::shell_dbus::is_gnome(&std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default())
}

#[cfg(debug_assertions)]
thread_local! {
    /// Debug builds: the top-bar state the setup step shows instead of this
    /// desktop's (`e2e onboarding-gnome off|login|active|none`).
    pub(super) static DEBUG_EXTENSION: RefCell<Option<Option<ExtensionStatus>>> =
        const { RefCell::new(None) };
}

// ---- The widgets -------------------------------------------------------------

/// The onboarding view. `root` goes into the window's main stack.
pub(super) struct Onboarding {
    pub root: adw::ToolbarView,
    /// Ends in the pairing page (no phone is paired); else "Done" goes back.
    pub for_pairing: bool,
    stack: gtk::Stack,
    step: Cell<usize>,
    segments: Vec<gtk::Button>,
    pub back: gtk::Button,
    pub next: gtk::Button,
    pub skip: gtk::Button,
    scenes: Vec<gtk::Box>,
    setup_body: gtk::Box,
    daemon: DaemonHandle,
    /// Called once when the person leaves the last step or skips out.
    on_leave: RefCell<Option<Box<dyn FnOnce()>>>,
    /// Set when it is torn down: the setup poll stops.
    closed: Rc<Cell<bool>>,
}

fn animations_on() -> bool {
    gtk::Settings::default().is_some_and(|settings| settings.is_gtk_enable_animations())
}

fn label(text: &str, classes: &[&str]) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .css_classes(classes)
        .build()
}

/// The progress bars: one per step, each a 40 px high button around a 4 px bar.
fn segment(index: usize, title: &str) -> gtk::Button {
    let bar = gtk::Box::builder()
        .valign(gtk::Align::Center)
        .hexpand(true)
        .css_classes(["ob-seg-bar"])
        .build();
    let words = format!("Go to step {}: {title}", index + 1);
    let button = gtk::Button::builder()
        .child(&bar)
        .hexpand(true)
        .tooltip_text(&words)
        .css_classes(["ob-seg"])
        .build();
    button.update_property(&[gtk::accessible::Property::Label(&words)]);
    button
}

impl Onboarding {
    /// Builds it at `step`. `on_leave` runs when the person leaves it.
    pub(super) fn new(
        daemon: &DaemonHandle,
        for_pairing: bool,
        step: usize,
        on_leave: impl FnOnce() + 'static,
    ) -> Rc<Self> {
        let animate = animations_on();
        let stack = gtk::Stack::builder()
            .transition_type(if animate {
                gtk::StackTransitionType::Crossfade
            } else {
                gtk::StackTransitionType::None
            })
            .transition_duration(200)
            .vhomogeneous(false)
            .hhomogeneous(false)
            .build();

        let narrow = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            720.0,
            adw::LengthUnit::Sp,
        ));
        let mut scenes = Vec::new();
        for (index, page) in TOUR.iter().enumerate() {
            let card = gtk::Box::builder()
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .css_classes(["ob-scene-card"])
                .build();
            card.update_property(&[gtk::accessible::Property::Label(page.title)]);
            scenes.push(card.clone());
            let words = words_box(page.kicker, page.title, page.body);
            words.set_valign(gtk::Align::Center);
            words.set_hexpand(true);
            let layout = gtk::Box::builder()
                .spacing(40)
                .valign(gtk::Align::Center)
                .vexpand(true)
                .build();
            layout.append(&card);
            layout.append(&words);
            narrow.add_setter(
                &layout,
                "orientation",
                Some(&gtk::Orientation::Vertical.to_value()),
            );
            narrow.add_setter(&layout, "spacing", Some(&24.to_value()));
            narrow.add_setter(&card, "halign", Some(&gtk::Align::Fill.to_value()));
            stack.add_named(&layout, Some(&index.to_string()));
        }
        let setup_body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .build();
        let setup = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .build();
        setup.append(&words_box(SETUP_KICKER, SETUP_TITLE, SETUP_BODY));
        setup.append(&setup_body);
        stack.add_named(&setup, Some(&SETUP.to_string()));

        let titles: Vec<&str> = TOUR
            .iter()
            .map(|page| page.title)
            .chain([SETUP_TITLE, "Pair a phone"])
            .take(segment_count(for_pairing))
            .collect();
        let segments_row = gtk::Box::builder()
            .spacing(6)
            .css_classes(["ob-segments"])
            .build();
        let segments: Vec<gtk::Button> = titles
            .iter()
            .enumerate()
            .map(|(index, title)| segment(index, title))
            .collect();
        for button in &segments {
            segments_row.append(button);
        }

        let back = button(
            ButtonKind::Outline,
            "Back",
            Some("desklink-arrow-back-symbolic"),
        );
        let next = button(ButtonKind::Primary, "Next", None);
        next.set_widget_name("onboarding-next");
        let skip = button(ButtonKind::Text, "Skip", None);
        skip.set_tooltip_text(Some("Skip the tour (Esc)"));
        skip.set_valign(gtk::Align::Center);
        let bar = gtk::Box::builder()
            .spacing(12)
            .css_classes(["ob-bottom"])
            .build();
        bar.append(&back);
        bar.append(&gtk::Box::builder().hexpand(true).build());
        bar.append(&next);

        let page = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["ob-page"])
            .build();
        page.append(&stack);
        let scroller = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(
                &adw::Clamp::builder()
                    .maximum_size(960)
                    .tightening_threshold(600)
                    .child(&page)
                    .build(),
            )
            .build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let brand = gtk::Box::builder()
            .spacing(8)
            .css_classes(["ob-brand-row"])
            .build();
        brand.append(
            &gtk::Image::builder()
                .icon_name("derx06.desklink.com")
                .pixel_size(28)
                .accessible_role(gtk::AccessibleRole::Presentation)
                .build(),
        );
        let name = label("DeskLink", &["ob-brand"]);
        name.set_hexpand(true);
        brand.append(&name);
        brand.append(&skip);
        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        top.append(&segments_row);
        top.append(&brand);
        content.append(&adw::Clamp::builder().maximum_size(960).child(&top).build());
        content.append(&scroller);
        content.append(&adw::Clamp::builder().maximum_size(960).child(&bar).build());
        let bin = adw::BreakpointBin::builder()
            .width_request(320)
            .height_request(280)
            .child(&content)
            .build();
        bin.add_breakpoint(narrow);

        let header = adw::HeaderBar::builder()
            .show_title(false)
            .css_classes(["flat"])
            .build();
        let root = adw::ToolbarView::builder()
            .content(&bin)
            .css_classes(["ob"])
            .build();
        root.add_top_bar(&header);

        let this = Rc::new(Self {
            root,
            for_pairing,
            stack,
            step: Cell::new(0),
            segments,
            back,
            next,
            skip,
            scenes,
            setup_body,
            daemon: daemon.clone(),
            on_leave: RefCell::new(Some(Box::new(on_leave))),
            closed: Rc::new(Cell::new(false)),
        });

        let weak = Rc::downgrade(&this);
        let act = move |nav: Nav| {
            if let Some(this) = weak.upgrade() {
                this.apply(nav);
            }
        };
        this.next.connect_clicked({
            let act = act.clone();
            move |_| act(Nav::Next)
        });
        this.back.connect_clicked({
            let act = act.clone();
            move |_| act(Nav::Back)
        });
        this.skip.connect_clicked({
            let act = act.clone();
            move |_| act(Nav::Skip)
        });
        for (index, button) in this.segments.iter().enumerate() {
            let weak = Rc::downgrade(&this);
            button.connect_clicked(move |_| {
                if let Some(this) = weak.upgrade() {
                    if index > SETUP {
                        this.leave();
                    } else {
                        this.show(index);
                    }
                }
            });
        }
        // Keys that nothing focused wanted: a focused button takes its own
        // Enter before this sees it.
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |controller, key, _, modifiers| {
            if modifiers.intersects(
                gdk::ModifierType::CONTROL_MASK
                    | gdk::ModifierType::ALT_MASK
                    | gdk::ModifierType::SUPER_MASK,
            ) {
                return glib::Propagation::Proceed;
            }
            let rtl = controller
                .widget()
                .is_some_and(|widget| widget.direction() == gtk::TextDirection::Rtl);
            match key_nav(key, rtl) {
                Some(nav) => {
                    act(nav);
                    glib::Propagation::Stop
                }
                None => glib::Propagation::Proceed,
            }
        });
        this.root.add_controller(keys);
        this.show(step);
        this
    }

    #[cfg(test)]
    pub(super) fn step(&self) -> usize {
        self.step.get()
    }

    /// Moves as `nav` says, or leaves.
    pub(super) fn apply(self: &Rc<Self>, nav: Nav) {
        match go(self.step.get(), nav) {
            Some(step) => self.show(step),
            None => self.leave(),
        }
    }

    fn leave(&self) {
        self.closed.set(true);
        let leave = self.on_leave.borrow_mut().take();
        if let Some(leave) = leave {
            leave();
        }
    }

    /// Shows `step`: its page, the bars up to it, its buttons. The scene is
    /// built afresh, so its animation plays when the page is seen.
    pub(super) fn show(self: &Rc<Self>, step: usize) {
        let step = step.min(SETUP);
        self.step.set(step);
        if let Some(card) = self.scenes.get(step) {
            while let Some(child) = card.first_child() {
                card.remove(&child);
            }
            card.append(&scene(step, !animations_on()));
        }
        if step == SETUP {
            self.fill_setup();
        }
        self.stack.set_visible_child_name(&step.to_string());
        for (index, segment) in self.segments.iter().enumerate() {
            if index <= step {
                segment.add_css_class("on");
            } else {
                segment.remove_css_class("on");
            }
            segment.update_state(&[gtk::accessible::State::Selected(Some(index == step))]);
        }
        self.back.set_visible(step > 0);
        self.skip.set_visible(step < SETUP);
        let words = next_label(step, self.for_pairing);
        self.next.set_label(words);
        let title = TOUR.get(step).map_or(SETUP_TITLE, |page| page.title);
        let where_ = format!(
            "Step {} of {}: {title}",
            step + 1,
            segment_count(self.for_pairing)
        );
        self.next
            .update_property(&[gtk::accessible::Property::Description(&where_)]);
        self.next.grab_focus();
    }

    /// "Set up this computer": the probe's answer as rows, or "Checking…"
    /// until it comes. The probe runs again each time the step is shown.
    fn fill_setup(self: &Rc<Self>) {
        let before = crate::platform::generation();
        // Tests fake the report and never probe the session they run in.
        if !cfg!(test) {
            crate::platform::refresh();
        }
        self.draw_setup(false);
        let started = std::time::Instant::now();
        let weak = Rc::downgrade(self);
        let closed = self.closed.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            let Some(this) = weak.upgrade().filter(|_| !closed.get()) else {
                return glib::ControlFlow::Break;
            };
            if crate::platform::generation() > before {
                this.draw_setup(false);
                glib::ControlFlow::Break
            } else if started.elapsed() > std::time::Duration::from_secs(20) {
                this.draw_setup(true);
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    fn draw_setup(self: &Rc<Self>, gave_up: bool) {
        let body = &self.setup_body;
        while let Some(child) = body.first_child() {
            body.remove(&child);
        }
        let Some((checks, start_at_login, extension)) = real_facts() else {
            body.append(
                &gtk::Label::builder()
                    .label(if gave_up {
                        "DeskLink could not check this computer in time. You can go on, and \
                         see the full list later in Settings, Advanced, System check."
                    } else {
                        "Checking this computer…"
                    })
                    .wrap(true)
                    .xalign(0.0)
                    .css_classes(["dl-secondary"])
                    .accessible_role(gtk::AccessibleRole::Status)
                    .build(),
            );
            return;
        };
        let rows = setup_rows(&SetupFacts {
            checks: &checks,
            start_at_login,
            extension,
            gnome: gnome(),
            distro: crate::platform::distro(),
        });
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["dl-row-group", "ob-setup"])
            .build();
        for row in &rows {
            list.append(&self.setup_row(row));
        }
        body.append(&list);
        let again = button(
            ButtonKind::Text,
            "Check again",
            Some("desklink-refresh-symbolic"),
        );
        again.set_halign(gtk::Align::Start);
        let weak = Rc::downgrade(self);
        again.connect_clicked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.fill_setup();
            }
        });
        body.append(&again);
    }

    fn setup_row(self: &Rc<Self>, row: &SetupRow) -> gtk::ListBoxRow {
        let line = gtk::Box::builder().spacing(12).build();
        let title = label(row.title, &["ob-row-title"]);
        title.set_hexpand(true);
        title.set_valign(gtk::Align::Center);
        line.append(&title);
        let pill = status_pill(row.status, row.status_words);
        pill.set_valign(gtk::Align::Center);
        line.append(&pill);
        let text = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .hexpand(true)
            .build();
        text.append(&line);
        let detail = label(&row.detail, &["dl-secondary"]);
        detail.set_selectable(true);
        text.append(&detail);
        if let Some(action) = self.fix_widget(row) {
            action.set_halign(gtk::Align::Start);
            action.set_margin_top(6);
            text.append(&action);
        }
        let icon = icon_square(
            row.icon,
            match row.status {
                Status::Ok => Tone::Success,
                Status::Warn => Tone::Warn,
                Status::Err => Tone::Error,
                _ => Tone::Neutral,
            },
        );
        icon.set_valign(gtk::Align::Start);
        let content = gtk::Box::builder()
            .spacing(12)
            .css_classes(["ob-row"])
            .build();
        content.append(&icon);
        content.append(&text);
        let list_row = gtk::ListBoxRow::builder()
            .activatable(false)
            .focusable(false)
            .child(&content)
            .build();
        list_row.set_widget_name(row.id);
        list_row.update_property(&[gtk::accessible::Property::Label(&format!(
            "{}: {}",
            row.title, row.status_words
        ))]);
        list_row
    }

    fn fix_widget(self: &Rc<Self>, row: &SetupRow) -> Option<gtk::Widget> {
        let weak = Rc::downgrade(self);
        let redraw = move || {
            if let Some(this) = weak.upgrade() {
                this.draw_setup(false);
            }
        };
        Some(match &row.fix {
            Fix::None => return None,
            Fix::Switch(on) => {
                let switch = gtk::Switch::builder()
                    .active(*on)
                    .valign(gtk::Align::Center)
                    .build();
                switch.update_property(&[gtk::accessible::Property::Label("Start at login")]);
                let holder = gtk::Box::builder().spacing(12).build();
                holder.append(&switch);
                holder.append(&label("Start DeskLink when I log in", &["dl-label"]));
                let daemon = self.daemon.clone();
                switch.connect_active_notify(move |switch| {
                    daemon.send(DaemonCommand::SetStartAtLogin(switch.is_active()));
                    // The settings change on the daemon thread; redraw after.
                    let redraw = redraw.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(300), redraw);
                });
                holder.upcast()
            }
            Fix::TestNotification => {
                let test = button(ButtonKind::Tonal, "Send a test", None);
                test.connect_clicked(|button| {
                    if let Some(app) = gio::Application::default() {
                        let notification =
                            gio::Notification::new("DeskLink can show notifications");
                        notification
                            .set_body(Some("Your phone's notifications will appear like this."));
                        app.send_notification(Some("onboarding-test"), &notification);
                    }
                    super::toast_from(button, "Test notification sent");
                });
                test.upcast()
            }
            Fix::TurnOnExtension => {
                let on = button(ButtonKind::Tonal, "Turn on", None);
                on.update_property(&[gtk::accessible::Property::Label("Turn on the top bar menu")]);
                on.connect_clicked(move |button| {
                    match crate::shell_dbus::turn_on_extension() {
                        Ok(_) => super::toast_from(button, "Top bar menu turned on"),
                        Err(error) => super::toast_from(
                            button,
                            &format!("Could not turn on the top bar menu: {error}"),
                        ),
                    }
                    redraw();
                });
                on.upcast()
            }
            Fix::LogOut => {
                let out = button(ButtonKind::Tonal, "Log out…", None);
                out.set_tooltip_text(Some("GNOME asks you to confirm"));
                out.connect_clicked(|_| crate::shell_dbus::log_out());
                out.upcast()
            }
            Fix::Copy(command) => {
                let copy = button(
                    ButtonKind::Tonal,
                    "Copy command",
                    Some("desklink-content-copy-symbolic"),
                );
                let command = command.clone();
                copy.connect_clicked(move |button| {
                    button.display().clipboard().set_text(&command);
                    super::toast_from(button, "Command copied");
                });
                copy.upcast()
            }
        })
    }
}

impl std::fmt::Debug for Onboarding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Onboarding")
            .field("step", &self.step.get())
            .field("for_pairing", &self.for_pairing)
            .finish_non_exhaustive()
    }
}

impl Drop for Onboarding {
    fn drop(&mut self) {
        self.closed.set(true);
    }
}

/// The kicker, the title (a level 1 heading) and the body of a page.
fn words_box(kicker: &str, title: &str, body: &str) -> gtk::Box {
    let words = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    words.append(&label(kicker, &["ob-kicker"]));
    let heading = label(title, &["ob-title"]);
    heading.set_accessible_role(gtk::AccessibleRole::Heading);
    heading.update_property(&[gtk::accessible::Property::Level(1)]);
    words.append(&heading);
    let body = label(body, &["ob-body"]);
    body.set_max_width_chars(46);
    words.append(&body);
    words
}

// ---- The scenes ----------------------------------------------------------------

/// The scenes' canvas, in px: decoration, so it keeps its size at large text.
const SCENE_W: i32 = 360;
const SCENE_H: i32 = 240;

fn shape(classes: &[&str], width: i32, height: i32) -> gtk::Box {
    let shape = gtk::Box::builder().css_classes(classes).build();
    shape.set_size_request(width, height);
    shape
}

fn glyph(name: &str, size: i32, classes: &[&str]) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(name)
        .pixel_size(size)
        .css_classes(classes)
        .build()
}

fn chip(icon: &str, words: &str, delay: &str) -> gtk::Box {
    let chip = gtk::Box::builder()
        .spacing(6)
        .css_classes(["ob-chip", "float", delay])
        .build();
    chip.append(&glyph(icon, 16, &["ob-accent"]));
    chip.append(&gtk::Label::new(Some(words)));
    chip
}

/// A laptop at (x, y) whose screen is `w` x `h`; returns the screen's inside.
fn laptop(canvas: &gtk::Fixed, x: f64, y: f64, w: i32, h: i32) -> gtk::Box {
    let screen = shape(&["ob-laptop"], w, h);
    let display = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .css_classes(["ob-display"])
        .build();
    screen.append(&display);
    canvas.put(&screen, x, y);
    canvas.put(
        &shape(&["ob-laptop-base"], w + 28, 9),
        x - 14.0,
        y + f64::from(h),
    );
    display
}

/// A phone at (x, y), `w` x `h`; returns its screen.
fn phone(canvas: &gtk::Fixed, x: f64, y: f64, w: i32, h: i32) -> gtk::Box {
    let body = shape(&["ob-phone"], w, h);
    let display = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .spacing(5)
        .css_classes(["ob-phone-display"])
        .build();
    body.append(&display);
    canvas.put(&body, x, y);
    display
}

fn tile(icon: &str) -> gtk::CenterBox {
    gtk::CenterBox::builder()
        .css_classes(["ob-tile"])
        .center_widget(&glyph(icon, 14, &[]))
        .build()
}

fn term_lines(lines: &[&str], classes: &[&str]) -> gtk::Box {
    let term = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .vexpand(true)
        .css_classes(classes)
        .build();
    for (index, line) in lines.iter().enumerate() {
        let delay = ["l1", "l2", "l3", "l4"][index.min(3)];
        let prompt = line.starts_with('$');
        term.append(
            &gtk::Label::builder()
                .label(*line)
                .xalign(0.0)
                .css_classes(if prompt {
                    vec!["appear", delay, "prompt"]
                } else {
                    vec!["appear", delay]
                })
                .build(),
        );
    }
    term
}

/// The scene of tour page `page`; `still` when animations are off.
pub(super) fn scene(page: usize, still: bool) -> gtk::Fixed {
    let canvas = gtk::Fixed::builder()
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .css_classes(["ob-scene"])
        .can_target(false)
        .build();
    canvas.set_size_request(SCENE_W, SCENE_H);
    if still {
        canvas.add_css_class("still");
    }
    // A picture of what the words say: not read out.
    canvas.update_state(&[gtk::accessible::State::Hidden(true)]);
    match page {
        0 => {
            let screen = laptop(&canvas, 26.0, 56.0, 184, 116);
            let mark = glyph("desklink-link-symbolic", 40, &["ob-accent"]);
            mark.set_vexpand(true);
            screen.append(&mark);
            let display = phone(&canvas, 266.0, 40.0, 76, 148);
            let grid = gtk::Grid::builder()
                .row_spacing(6)
                .column_spacing(6)
                .halign(gtk::Align::Center)
                .valign(gtk::Align::Center)
                .vexpand(true)
                .build();
            for (index, icon) in [
                "desklink-desktop-windows-symbolic",
                "desklink-terminal-symbolic",
                "desklink-folder-symbolic",
                "desklink-content-paste-symbolic",
            ]
            .iter()
            .enumerate()
            {
                grid.attach(&tile(icon), (index % 2) as i32, (index / 2) as i32, 1, 1);
            }
            display.append(&grid);
            for (index, delay) in ["d1", "d2", "d3"].iter().enumerate() {
                canvas.put(
                    &shape(&["ob-packet", "travel", delay], 8, 8),
                    222.0 + 12.0 * index as f64,
                    110.0,
                );
            }
            canvas.put(
                &chip("desklink-wifi-symbolic", "Over Wi-Fi", "d1"),
                26.0,
                12.0,
            );
            canvas.put(
                &chip("desklink-encrypted-symbolic", "Encrypted", "d2"),
                196.0,
                202.0,
            );
        }
        1 => {
            let screen = laptop(&canvas, 20.0, 58.0, 196, 120);
            let window = shape(&["ob-window"], 96, 56);
            window.set_halign(gtk::Align::Start);
            window.set_margin_start(14);
            window.set_margin_top(14);
            screen.append(&window);
            canvas.put(
                &glyph("desklink-touch-app-symbolic", 22, &["ob-pointer", "drift"]),
                128.0,
                118.0,
            );
            let display = phone(&canvas, 262.0, 36.0, 82, 156);
            let mirror = shape(&["ob-mirror"], 60, 36);
            mirror.append(&shape(&["ob-window", "small"], 30, 18));
            display.append(&mirror);
            let pad = gtk::CenterBox::builder()
                .vexpand(true)
                .css_classes(["ob-touchpad"])
                .center_widget(&glyph("desklink-touchpad-mouse-symbolic", 20, &["ob-dim"]))
                .build();
            display.append(&pad);
            let keys = gtk::Box::builder().spacing(4).homogeneous(true).build();
            for _ in 0..3 {
                keys.append(&shape(&["ob-key"], 14, 10));
            }
            display.append(&keys);
            canvas.put(
                &chip(
                    "desklink-mobile-screen-share-symbolic",
                    "Phone screen",
                    "d1",
                ),
                20.0,
                14.0,
            );
            canvas.put(
                &chip("desklink-keyboard-symbolic", "Touchpad and keyboard", "d2"),
                150.0,
                204.0,
            );
        }
        2 => {
            let screen = laptop(&canvas, 18.0, 50.0, 214, 130);
            screen.add_css_class("dark");
            screen.append(&term_lines(
                &[
                    "$ ssh build-box",
                    "$ cargo build --release",
                    "   Compiling desklink",
                    "    Finished in 41 s",
                ],
                &["ob-term"],
            ));
            let display = phone(&canvas, 266.0, 38.0, 78, 152);
            display.add_css_class("dark");
            display.append(&term_lines(
                &["$ ssh build", "$ cargo build", " Compiling", " Finished"],
                &["ob-term", "small"],
            ));
            canvas.put(
                &chip(
                    "desklink-terminal-symbolic",
                    "Same terminal, both screens",
                    "d1",
                ),
                76.0,
                204.0,
            );
        }
        3 => {
            let screen = laptop(&canvas, 20.0, 60.0, 190, 118);
            let drop = gtk::CenterBox::builder()
                .vexpand(true)
                .css_classes(["ob-drop"])
                .center_widget(&glyph("desklink-download-symbolic", 26, &["ob-accent"]))
                .build();
            screen.append(&drop);
            let display = phone(&canvas, 264.0, 40.0, 78, 150);
            let note = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(4)
                .css_classes(["ob-note"])
                .build();
            for width in [44, 36, 40] {
                let line = shape(&["ob-line"], width, 4);
                line.set_halign(gtk::Align::Start);
                note.append(&line);
            }
            display.append(&note);
            let photo = gtk::CenterBox::builder()
                .vexpand(true)
                .css_classes(["ob-photo"])
                .center_widget(&glyph("desklink-image-symbolic", 20, &["ob-dim"]))
                .build();
            display.append(&photo);
            for (index, (icon, delay)) in [
                ("desklink-description-symbolic", "d1"),
                ("desklink-image-symbolic", "d2"),
                ("desklink-picture-as-pdf-symbolic", "d3"),
            ]
            .iter()
            .enumerate()
            {
                let file = gtk::CenterBox::builder()
                    .css_classes(["ob-file", "send", delay])
                    .center_widget(&glyph(icon, 16, &["ob-accent"]))
                    .build();
                file.set_size_request(26, 30);
                canvas.put(&file, 222.0, 74.0 + 34.0 * index as f64);
            }
            canvas.put(&chip("desklink-folder-symbolic", "Files", "d1"), 20.0, 14.0);
            canvas.put(
                &chip("desklink-content-paste-symbolic", "Clipboard", "d2"),
                112.0,
                14.0,
            );
            canvas.put(
                &chip("desklink-edit-note-symbolic", "Notes", "d3"),
                196.0,
                204.0,
            );
        }
        _ => {
            let shield = gtk::CenterBox::builder()
                .css_classes(["ob-shield"])
                .center_widget(&glyph("desklink-encrypted-symbolic", 48, &[]))
                .build();
            shield.set_size_request(112, 112);
            canvas.put(&shield, 124.0, 62.0);
            canvas.put(
                &chip("desklink-encrypted-symbolic", "Encrypted end to end", "d1"),
                12.0,
                24.0,
            );
            canvas.put(
                &chip("desklink-link-symbolic", "Only paired phones", "d2"),
                200.0,
                34.0,
            );
            canvas.put(
                &chip("desklink-person-symbolic", "No account", "d3"),
                128.0,
                196.0,
            );
        }
    }
    canvas
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::{evaluate, Probe};

    #[test]
    fn the_tour_is_for_an_unpaired_computer_that_has_not_seen_it() {
        assert!(show_on_start(false, false));
        assert!(!show_on_start(true, false), "seen once");
        assert!(!show_on_start(false, true), "a paired computer never");
        assert!(!show_on_start(true, true));
    }

    /// Five tour pages, the setup step, then (unpaired) the pairing page:
    /// Next walks them in order, Back goes one back and stops at the first,
    /// Skip jumps from any tour page to setup and leaves from setup.
    #[test]
    fn the_steps_go_in_order_and_skip_lands_on_setup() {
        assert_eq!(TOUR.len(), 5);
        assert_eq!(SETUP, 5);
        assert_eq!(segment_count(true), 7, "tour, setup, pairing");
        assert_eq!(segment_count(false), 6, "a replay ends at setup");
        let mut step = 0;
        let mut seen = vec![step];
        while let Some(next) = go(step, Nav::Next) {
            step = next;
            seen.push(step);
        }
        assert_eq!(seen, vec![0, 1, 2, 3, 4, 5], "then it leaves for pairing");
        assert_eq!(go(0, Nav::Back), Some(0));
        assert_eq!(go(5, Nav::Back), Some(4));
        for page in 0..SETUP {
            assert_eq!(go(page, Nav::Skip), Some(SETUP));
        }
        assert_eq!(go(SETUP, Nav::Skip), None);
        assert_eq!(next_label(0, true), "Next");
        assert_eq!(next_label(SETUP, true), "Continue");
        assert_eq!(next_label(SETUP, false), "Done");
    }

    #[test]
    fn enter_and_arrows_move_and_escape_skips() {
        use gdk::Key;
        assert_eq!(key_nav(Key::Return, false), Some(Nav::Next));
        assert_eq!(key_nav(Key::KP_Enter, false), Some(Nav::Next));
        assert_eq!(key_nav(Key::Right, false), Some(Nav::Next));
        assert_eq!(key_nav(Key::Left, false), Some(Nav::Back));
        assert_eq!(key_nav(Key::Escape, false), Some(Nav::Skip));
        assert_eq!(key_nav(Key::Left, true), Some(Nav::Next), "mirrored");
        assert_eq!(key_nav(Key::Right, true), Some(Nav::Back));
        assert_eq!(key_nav(Key::Tab, false), None, "Tab moves the focus");
        assert_eq!(key_nav(Key::space, false), None);
    }

    /// The copy follows the rules of `UI-REDESIGN.md` 5.1: sentence case, no
    /// protocol words, no exclamation marks.
    #[test]
    fn the_words_are_plain() {
        let mut all: Vec<&str> = TOUR
            .iter()
            .flat_map(|page| [page.kicker, page.title, page.body])
            .collect();
        all.extend([SETUP_KICKER, SETUP_TITLE, SETUP_BODY]);
        for words in all {
            for banned in [
                "relay",
                "WebRTC",
                "peer",
                "TURN",
                "handover",
                "!",
                "Please",
                "successfully",
                "laptop",
                "KDE",
            ] {
                assert!(!words.contains(banned), "{banned:?} in {words:?}");
            }
        }
        for page in &TOUR {
            let rest: String = page.title.chars().skip(1).collect();
            assert!(
                !rest.split_whitespace().any(|word| word
                    .chars()
                    .next()
                    .is_some_and(char::is_uppercase)
                    && word != "Steer"),
                "sentence case: {}",
                page.title
            );
        }
    }

    fn gnome_wayland() -> Probe {
        Probe {
            desktop: "GNOME".into(),
            session_type: Some("wayland".into()),
            wayland_display: true,
            runtime_dir: true,
            session_bus: true,
            system_bus: true,
            session_names: [
                "org.freedesktop.portal.Desktop",
                "org.freedesktop.Notifications",
            ]
            .map(String::from)
            .into(),
            system_names: ["org.freedesktop.login1".to_string()].into(),
            portals: [
                "ScreenCast",
                "RemoteDesktop",
                "InputCapture",
                "Notification",
            ]
            .map(String::from)
            .into(),
            input_capture: Some(3),
            elements: [
                "webrtcbin",
                "dtlsdec",
                "dtlssrtpenc",
                "nicesrc",
                "opusenc",
                "opusdec",
                "vp8dec",
                "vp8enc",
                "pipewiresrc",
                "pipewiresink",
            ]
            .map(String::from)
            .into(),
            pipewire_socket: true,
            config_writable: true,
            ..Probe::default()
        }
    }

    fn ids(rows: &[SetupRow]) -> Vec<&'static str> {
        rows.iter().map(|row| row.id).collect()
    }

    /// A complete GNOME computer: every row in order, all ready, and what the
    /// screen-sharing and Glide dialogs will ask said in GNOME's words.
    #[test]
    fn a_complete_gnome_computer_reads_ready_in_order() {
        let checks = evaluate(&gnome_wayland());
        let rows = setup_rows(&SetupFacts {
            checks: &checks,
            start_at_login: true,
            extension: Some(ExtensionStatus::Active),
            gnome: true,
            distro: Some(Distro::Arch),
        });
        assert_eq!(
            ids(&rows),
            [
                "autostart",
                "notifications",
                "extension",
                "share",
                "glide",
                "keys",
                "plugins"
            ]
        );
        for row in &rows {
            assert_eq!(row.status, Status::Ok, "{}: {}", row.id, row.detail);
        }
        let share = &rows[3];
        assert!(
            share.detail.contains("Allow Remote Interaction"),
            "{}",
            share.detail
        );
        assert!(share.detail.contains("until DeskLink restarts"));
        assert!(rows[4].detail.contains("Capture Input"));
        assert!(rows[4]
            .detail
            .starts_with("Move your mouse across to the phone"));
        assert!(rows[5].detail.contains("No keyring"));
        assert_eq!(rows[0].fix, Fix::Switch(true));
        assert_eq!(rows[1].fix, Fix::TestNotification);
        assert_eq!(rows[2].fix, Fix::None);
    }

    /// An empty machine: what is missing says why, with one copyable install
    /// command for this distribution; the top bar row is only on GNOME, and
    /// Continue is never blocked (nothing here is a gate).
    #[test]
    fn missing_pieces_say_why_and_offer_the_install_command() {
        let checks = evaluate(&Probe {
            desktop: "sway".into(),
            wayland_display: true,
            session_bus: true,
            config_writable: true,
            ..Probe::default()
        });
        let rows = setup_rows(&SetupFacts {
            checks: &checks,
            start_at_login: false,
            extension: None,
            gnome: false,
            distro: Some(Distro::Fedora),
        });
        assert!(!ids(&rows).contains(&"extension"), "no top bar off GNOME");
        let plugins = rows.iter().find(|row| row.id == "plugins").unwrap();
        assert_eq!(plugins.status, Status::Err);
        let Fix::Copy(command) = &plugins.fix else {
            panic!("{:?}", plugins.fix)
        };
        assert!(command.starts_with("sudo dnf install gstreamer1-plugins-bad-free"));
        assert!(plugins.detail.contains("Install: sudo dnf"));
        let autostart = &rows[0];
        assert_eq!(
            autostart.status,
            Status::Warn,
            "sway runs no autostart entries"
        );
        assert!(autostart.detail.contains("exec desklink"));
        let notifications = rows.iter().find(|row| row.id == "notifications").unwrap();
        assert!(matches!(notifications.fix, Fix::Copy(_)));
        let glide = rows.iter().find(|row| row.id == "glide").unwrap();
        assert_eq!(glide.status, Status::Err);
        assert!(glide
            .detail
            .starts_with("Move your mouse across to the phone"));
    }

    #[test]
    fn the_top_bar_row_offers_the_one_next_step() {
        let checks = evaluate(&gnome_wayland());
        let row = |extension| {
            setup_rows(&SetupFacts {
                checks: &checks,
                start_at_login: false,
                extension: Some(extension),
                gnome: true,
                distro: None,
            })
            .into_iter()
            .find(|row| row.id == "extension")
            .unwrap()
        };
        assert_eq!(row(ExtensionStatus::Off).fix, Fix::TurnOnExtension);
        assert!(row(ExtensionStatus::Off)
            .detail
            .contains("Nothing changes until you click"));
        assert_eq!(row(ExtensionStatus::NeedsLogin).fix, Fix::LogOut);
        assert_eq!(row(ExtensionStatus::Active).fix, Fix::None);
    }

    // ---- Widget tests (on the dedicated test display) -----------------------

    use super::super::DesklinkWindow;
    use adw::subclass::prelude::ObjectSubclassIsExt;

    fn window() -> (
        DesklinkWindow,
        std::sync::mpsc::Receiver<crate::device_links::daemon::DaemonCommand>,
    ) {
        let resources = crate::load_resources().expect("the bundled resources");
        gio::resources_register(&resources);
        let app = adw::Application::builder()
            .application_id("derx06.desklink.com.test")
            .build();
        let window = DesklinkWindow::new(&app);
        let (daemon, commands) = DaemonHandle::detached();
        window.set_daemon(daemon);
        (window, commands)
    }

    fn visible(window: &DesklinkWindow) -> String {
        window
            .imp()
            .main_stack
            .visible_child_name()
            .map(|name| name.to_string())
            .unwrap_or_default()
    }

    /// A fresh config with no phone: the tour is what the window shows.
    /// With a paired phone: the phones, never the tour.
    #[test]
    fn the_tour_shows_on_a_fresh_config_and_never_with_a_paired_phone() {
        crate::test_display::run(|| {
            let (fresh, _commands) = window();
            fresh.refresh_devices();
            assert_eq!(visible(&fresh), "onboarding");
            let tour = fresh.imp().onboarding.borrow().clone().expect("the tour");
            assert!(tour.for_pairing);
            assert_eq!(tour.step(), 0);
            assert!(!tour.back.get_visible(), "no Back on the first page");
            assert!(tour.skip.get_visible());

            let (paired, _commands) = window();
            let phone = super::super::debug_fake_device("paired", "Pixel 9");
            assert!(phone.trusted);
            paired.imp().fake_devices.borrow_mut().push(phone);
            paired.refresh_devices();
            assert!(paired.imp().onboarding.borrow().is_none());
            assert_eq!(visible(&paired), "devices");
        });
    }

    /// The keys and buttons walk the steps: Right and Enter go on, Left goes
    /// back, Esc skips to the setup step, Continue there leaves for the
    /// pairing page and says the tour was seen.
    #[test]
    fn keys_and_buttons_walk_the_tour_to_the_pairing_page() {
        let _held = crate::platform::TEST_REPORT
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_display::run(|| {
            crate::platform::set_report_for_tests(evaluate(&gnome_wayland()));
            let (window, commands) = window();
            window.refresh_devices();
            let tour = window.imp().onboarding.borrow().clone().expect("the tour");
            let press = |key: gdk::Key| tour.apply(key_nav(key, false).expect("a key it uses"));
            press(gdk::Key::Right);
            assert_eq!(tour.step(), 1);
            press(gdk::Key::Return);
            assert_eq!(tour.step(), 2);
            press(gdk::Key::Left);
            assert_eq!(tour.step(), 1);
            tour.next.emit_clicked();
            assert_eq!(tour.step(), 2);
            tour.back.emit_clicked();
            assert_eq!(tour.step(), 1);
            assert!(tour.back.get_visible());
            press(gdk::Key::Escape);
            assert_eq!(tour.step(), SETUP, "Skip lands on Set up this computer");
            assert!(!tour.skip.get_visible(), "nothing left to skip");
            assert_eq!(tour.next.label().as_deref(), Some("Continue"));
            // The rows are drawn from the report, each with its status.
            let mut names = Vec::new();
            let mut stack = vec![tour.root.clone().upcast::<gtk::Widget>()];
            while let Some(widget) = stack.pop() {
                if widget.is::<gtk::ListBoxRow>() {
                    names.push(widget.widget_name().to_string());
                }
                let mut child = widget.first_child();
                while let Some(next) = child {
                    child = next.next_sibling();
                    stack.push(next);
                }
            }
            for id in [
                "autostart",
                "notifications",
                "share",
                "glide",
                "keys",
                "plugins",
            ] {
                assert!(names.iter().any(|name| name == id), "{id} in {names:?}");
            }
            tour.next.emit_clicked();
            let context = glib::MainContext::default();
            while context.iteration(false) {}
            assert!(window.imp().onboarding.borrow().is_none());
            assert_eq!(visible(&window), "welcome", "then the QR code");
            let sent: Vec<_> = commands.try_iter().collect();
            assert!(
                sent.iter()
                    .any(|command| matches!(command, DaemonCommand::SetOnboardingSeen(true))),
                "remembered as seen"
            );
            crate::platform::clear_report_for_tests();
        });
    }

    /// The menu's replay on a paired computer ends with Done, back to the phones.
    #[test]
    fn a_replay_on_a_paired_computer_ends_with_done() {
        crate::test_display::run(|| {
            let (window, _commands) = window();
            let phone = super::super::debug_fake_device("paired", "Pixel 9");
            window.imp().fake_devices.borrow_mut().push(phone);
            window.refresh_devices();
            gtk::prelude::WidgetExt::activate_action(&window, "win.welcome-tour", None)
                .expect("the menu's action");
            assert_eq!(visible(&window), "onboarding");
            let tour = window.imp().onboarding.borrow().clone().expect("the tour");
            assert!(!tour.for_pairing);
            tour.apply(Nav::Skip);
            assert_eq!(tour.next.label().as_deref(), Some("Done"));
            tour.apply(Nav::Next);
            let context = glib::MainContext::default();
            while context.iteration(false) {}
            assert!(window.imp().onboarding.borrow().is_none());
            assert_eq!(visible(&window), "devices");
        });
    }
}
