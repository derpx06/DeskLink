//! Moving around the window (`impl DesklinkWindow`): which device and page
//! show, solo pages, onboarding, opening a note, a contact or the dial pad,
//! and the title.
//!
//! Sibling edges: `imp`, and `refresh`/`notices` (the same window type).

use super::*;

impl DesklinkWindow {
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

    /// Shows the welcome tour at `step` (a replay from the menu too). It ends
    /// in the pairing page when no phone is paired, else "Done" comes back.
    pub(super) fn start_onboarding(&self, step: usize) {
        let imp = self.imp();
        let Some(daemon) = self.daemon() else { return };
        if let Some(old) = imp.onboarding.take() {
            imp.main_stack.remove(&old.root);
        }
        let for_pairing = !self
            .known_devices(&daemon)
            .iter()
            .any(|device| device.trusted);
        let window = self.downgrade();
        let tour = onboarding::Onboarding::new(&daemon, for_pairing, step, move || {
            if let Some(window) = window.upgrade() {
                // After the click that left it, not inside its handler.
                glib::idle_add_local_once(move || window.end_onboarding(true, true));
            }
        });
        imp.main_stack.add_named(&tour.root, Some("onboarding"));
        imp.main_stack.set_visible_child_name("onboarding");
        imp.onboarding.replace(Some(tour.clone()));
        tour.next.grab_focus();
        if let Some(stop) = imp.welcome_qr.take() {
            pairing::stop_welcome(&daemon, &stop);
        }
    }

    /// Takes the tour down; `seen` remembers it was seen (left by the person,
    /// or a phone paired). `redraw`: show what comes next now (the pairing
    /// page or the phones).
    pub(super) fn end_onboarding(&self, redraw: bool, seen: bool) {
        let imp = self.imp();
        let Some(tour) = imp.onboarding.take() else {
            return;
        };
        imp.onboarding_to_pairing.set(tour.for_pairing);
        if seen && !crate::device_links::config::app_settings().onboarding_seen {
            if let Some(daemon) = self.daemon() {
                daemon.send(DaemonCommand::SetOnboardingSeen(true));
            }
        }
        imp.main_stack.set_visible_child_name(if tour.for_pairing {
            "welcome"
        } else {
            "devices"
        });
        imp.main_stack.remove(&tour.root);
        imp.snapshots.borrow_mut().clear();
        if redraw {
            self.refresh_devices();
        }
    }

    /// Over the pairing page after the tour: the last of the step bars, and
    /// Back to "Set up this computer".
    pub(super) fn onboarding_last_step(&self) -> gtk::Widget {
        let row = gtk::Box::builder()
            .spacing(12)
            .css_classes(["ob-last-step"])
            .build();
        let back = ui_kit::button(
            ui_kit::ButtonKind::Text,
            "Back",
            Some("desklink-arrow-back-symbolic"),
        );
        back.set_tooltip_text(Some("Back to Set up this computer"));
        let window = self.downgrade();
        back.connect_clicked(move |_| {
            if let Some(window) = window.upgrade() {
                window.imp().onboarding_to_pairing.set(false);
                window.start_onboarding(onboarding::SETUP);
            }
        });
        row.append(&back);
        let bars = gtk::Box::builder()
            .spacing(6)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let count = onboarding::segment_count(true);
        for _ in 0..count {
            bars.append(
                &gtk::Box::builder()
                    .hexpand(true)
                    .css_classes(["ob-seg-bar", "on"])
                    .build(),
            );
        }
        bars.update_property(&[gtk::accessible::Property::Label(&format!(
            "Step {count} of {count}: Pair a phone"
        ))]);
        row.append(&bars);
        adw::Clamp::builder()
            .maximum_size(960)
            .child(&row)
            .build()
            .upcast()
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
    pub(super) fn selected_is_paired(&self) -> bool {
        let (Some(daemon), Some(id)) = (self.daemon(), self.selected_id()) else {
            return false;
        };
        self.known_devices(&daemon)
            .iter()
            .any(|device| device.id == id && device.trusted)
    }

    /// The paired phones, with the made-up ones of a debug build.
    pub(super) fn paired_devices(&self) -> Vec<DeviceView> {
        #[allow(unused_mut)]
        let mut devices = self
            .daemon()
            .map(|daemon| daemon.devices())
            .unwrap_or_default();
        #[cfg(debug_assertions)]
        devices.extend(self.imp().fake_devices.borrow().iter().cloned());
        devices.retain(|device| device.trusted);
        devices
    }

    /// Opens the note `id` in the notes list (from "Find anything").
    pub(super) fn open_note(&self, id: &str) {
        self.show_page("notes");
        let imp = self.imp();
        if imp.main_stack.visible_child_name().as_deref() == Some("solo") {
            if let Some(solo) = imp.solo.borrow().as_ref() {
                solo.notes.open_id(id);
            }
        } else if let Some(page) = imp.notes.borrow().as_ref() {
            page.show_tab("notes");
            page.notes.open_id(id);
        }
    }

    /// Watches presses on the whole window (before any widget sees them,
    /// claiming nothing) for [`Self::refresh_devices`].
    pub(super) fn hold_redraws_while_pressed(&self) {
        let legacy = gtk::EventControllerLegacy::new();
        legacy.set_propagation_phase(gtk::PropagationPhase::Capture);
        let weak = self.downgrade();
        legacy.connect_event(move |_, event| {
            use gtk::gdk::EventType;
            let held = match event.event_type() {
                EventType::ButtonPress | EventType::TouchBegin => true,
                EventType::ButtonRelease | EventType::TouchEnd | EventType::TouchCancel => false,
                // A release that went elsewhere (a drag and drop took it).
                EventType::MotionNotify
                    if !event.modifier_state().intersects(
                        gtk::gdk::ModifierType::BUTTON1_MASK
                            | gtk::gdk::ModifierType::BUTTON2_MASK
                            | gtk::gdk::ModifierType::BUTTON3_MASK,
                    ) && weak
                        .upgrade()
                        .is_some_and(|window| window.imp().pointer_held.get()) =>
                {
                    false
                }
                _ => return glib::Propagation::Proceed,
            };
            if let Some(window) = weak.upgrade() {
                window.imp().pointer_held.set(held);
                if !held {
                    window.refresh_soon();
                }
            }
            glib::Propagation::Proceed
        });
        self.add_controller(legacy);
    }

    /// Shows Home with the "Everything DeskLink does" row `id` on screen and
    /// its button focused (from "Find anything"), once Home is redrawn.
    pub(super) fn show_feature_row(&self, id: &str) {
        if !self.show_page("home") {
            self.show_feature_toast("Pair a phone to use that");
            return;
        }
        let (weak, id) = (self.downgrade(), id.to_string());
        glib::timeout_add_local_once(std::time::Duration::from_millis(250), move || {
            let Some(window) = weak.upgrade() else {
                return;
            };
            let Some((_, body)) = window.imp().bodies.borrow().get("home").cloned() else {
                return;
            };
            let named = |root: &gtk::Widget, name: &str| {
                named_widgets(root, name)
                    .into_iter()
                    .find(|widget| widget.widget_name() == name)
            };
            let section = named(body.upcast_ref(), "everything");
            // The button when the row has one; the row itself (made
            // focusable for this) when it only says why it is off.
            let target = section.and_then(|section| {
                named(&section, &format!("everything-{id}"))
                    .or_else(|| named(&section, &id).inspect(|row| row.set_focusable(true)))
            });
            if let Some(target) = target {
                target.grab_focus();
            }
        });
    }

    /// The dial pad (Contacts page, Find anything, Ctrl+Shift+D).
    pub fn show_dial_pad(&self) {
        dialpad::show(self);
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
    pub(super) fn select_device(&self, device_id: &str) {
        let imp = self.imp();
        if imp.chosen_device_id.borrow().as_deref() != Some(device_id) {
            imp.chosen_device_id.replace(Some(device_id.to_string()));
            sidebar::save_last_device(device_id);
        }
        if imp.selected_device_id.borrow().as_deref() == Some(device_id) {
            return;
        }
        imp.selected_device_id.replace(Some(device_id.to_string()));
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
        // A page drawn while another was shown is laid out for the window's
        // width now (a big window's columns, wider list, more cards a line).
        self.apply_layout();
        self.refresh_soon();
        true
    }

    /// The header's title: the page's name, or the terminal being looked at.
    pub(super) fn update_title(&self) {
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
}
