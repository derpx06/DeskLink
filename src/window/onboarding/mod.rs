//! The first-run welcome (`docs/UI-REDESIGN.md` 3.1 and 3.10, the phone's
//! `screen/onboarding/`): the glance (what DeskLink does and the three steps
//! to start), three tour pages, "Set up this computer", then the existing
//! pairing page with its QR code as the last step.
//!
//! Shown once, on a computer with no paired phone whose settings do not say
//! it was seen ([`show_on_start`]); the main menu's "Welcome tour" replays it.
//! Pages change only when the person asks (no timer). The scenes are GTK
//! widgets on a fixed canvas, coloured by the kit's tokens; their few CSS
//! animations are finite and are not built at all when animations are off.
//!
//! Layout: this file holds the [`Onboarding`] widget (its steps, keys,
//! animation and the facts it reads); `steps` the words and rules with no
//! widgets, `scenes` the pages and drawn scenes. No cycles.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use super::ui_kit::{button, icon_square, status_pill, ButtonKind, Status, Tone};
use crate::device_links::daemon::{DaemonCommand, DaemonHandle};
use crate::platform::{Check, Distro, Level};
use crate::shell_dbus::ExtensionStatus;

mod scenes;
mod steps;
use scenes::*;

// What the rest of the module's parent uses (everything else stays inside).
pub(super) use scenes::scene;
#[cfg(test)]
pub(super) use steps::GLANCE_START;
pub(super) use steps::{
    go, key_nav, next_label, segment_count, setup_rows, show_on_start, Fix, Nav, SetupFacts,
    SetupRow, GLANCE_FOOTER, GLANCE_KICKER, GLANCE_LINES, GLANCE_STEPS, GLANCE_STEPS_TITLE,
    GLANCE_TITLE, SETUP, SETUP_BODY, SETUP_KICKER, SETUP_TITLE, TOUR,
};

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
    /// The latest setup poll: an older one (an earlier Check again) stops.
    poll: Cell<u64>,
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
        stack.add_named(&glance_page(&narrow), Some("0"));
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
            // Two equal columns, so the scene stays put from page to page.
            let layout = gtk::Box::builder()
                .spacing(40)
                .homogeneous(true)
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
            narrow.add_setter(&layout, "homogeneous", Some(&false.to_value()));
            narrow.add_setter(&card, "halign", Some(&gtk::Align::Fill.to_value()));
            stack.add_named(&layout, Some(&(index + 1).to_string()));
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

        let titles: Vec<&str> = [GLANCE_TITLE]
            .into_iter()
            .chain(TOUR.iter().map(|page| page.title))
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
            poll: Cell::new(0),
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
        // Step 0 is the glance; tour page `n` is step `n + 1`.
        let page = step.checked_sub(1);
        if let Some((page, card)) = page.and_then(|page| Some((page, self.scenes.get(page)?))) {
            while let Some(child) = card.first_child() {
                card.remove(&child);
            }
            card.append(&scene(page, !animations_on()));
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
        let title = match page {
            None => GLANCE_TITLE,
            Some(page) => TOUR.get(page).map_or(SETUP_TITLE, |page| page.title),
        };
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
        let ticket = self.poll.get() + 1;
        self.poll.set(ticket);
        let started = std::time::Instant::now();
        let weak = Rc::downgrade(self);
        let closed = self.closed.clone();
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            let Some(this) = weak
                .upgrade()
                .filter(|this| !closed.get() && this.poll.get() == ticket)
            else {
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

    /// Which setup control has the keyboard focus: its row's id, or
    /// "check-again"; `None` when the focus is elsewhere.
    fn setup_focus(&self) -> Option<String> {
        let focus = self.root.root().and_then(|root| root.focus())?;
        if !focus.is_ancestor(&self.setup_body) {
            return None;
        }
        if focus.widget_name() == "check-again" {
            return Some("check-again".into());
        }
        focus
            .ancestor(gtk::ListBoxRow::static_type())
            .map(|row| row.widget_name().to_string())
    }

    /// Puts the focus back on the control `id` had before a redraw.
    fn restore_setup_focus(&self, id: &str) {
        let Some(holder) = super::named_widgets(self.setup_body.upcast_ref(), id)
            .into_iter()
            .next()
        else {
            return;
        };
        let control = super::named_widgets(&holder, "")
            .into_iter()
            .find(|widget| widget.is::<gtk::Button>() || widget.is::<gtk::Switch>());
        if let Some(control) = control {
            control.grab_focus();
        }
    }

    fn draw_setup(self: &Rc<Self>, gave_up: bool) {
        // Redrawn in place of the old rows: the focus goes back to the same
        // row's control, not to the window.
        let focused = self.setup_focus();
        self.fill_setup_rows(gave_up);
        if let Some(id) = focused {
            self.restore_setup_focus(&id);
        }
    }

    fn fill_setup_rows(self: &Rc<Self>, gave_up: bool) {
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
        again.set_widget_name("check-again");
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

/// The rules of `UI-REDESIGN.md` 5.1 for first-run and guide words: no
/// protocol or technical words, no exclamation marks, nothing promised
/// off this network while the relay is off (1.3).
#[cfg(test)]
pub(super) fn assert_plain(words: &str) {
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
        "LAN",
        "mDNS",
        "TLS",
        "protocol",
        "daemon",
        "packet",
        "port",
        "certificate",
        "fingerprint",
        "GStreamer",
        "plugin",
        "codec",
    ] {
        assert!(!words.contains(banned), "{banned:?} in {words:?}");
    }
    if !crate::device_links::relay_link::RELAY_ENABLED {
        let lower = words.to_lowercase();
        for promise in [
            "away from home",
            "anywhere",
            "internet",
            "another network",
            "off your network",
        ] {
            assert!(!lower.contains(promise), "{promise:?} in {words:?}");
        }
    }
}

#[cfg(test)]
mod tests;
