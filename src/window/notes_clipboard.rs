//! Notes & Clipboard: one page, three chips (Notes | Snippets | Clipboard).
//! The clipboard sharing switch is at the head of the Clipboard chip's tab
//! (and in Settings), not in the page header. Replaces the Clipboard page
//! and the Notes page of earlier builds.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use super::ui_kit::{self, Tone};
use super::{clipboard, notes, DesklinkWindow};
use crate::device_links::daemon::DaemonCommand;
use crate::device_links::device::DeviceView;

/// The tabs, in order: (name, title, icon).
pub(super) const TABS: [(&str, &str, &str); 3] = [
    ("notes", "Notes", "desklink-edit-note-symbolic"),
    ("snippets", "Snippets", "desklink-keep-symbolic"),
    (
        "clipboard",
        "Clipboard history",
        "desklink-content-paste-symbolic",
    ),
];

/// What a tab's chip says (the third is shorter than its title).
fn chip_label(name: &str, title: &'static str) -> &'static str {
    if name == "clipboard" {
        "Clipboard"
    } else {
        title
    }
}

/// The tab a (possibly old) page name opens: `clipboard` and `snippets` are
/// tabs of this page now.
pub(super) fn tab_of(name: &str) -> Option<&'static str> {
    TABS.iter()
        .find(|(tab, _, _)| *tab == name)
        .map(|(tab, _, _)| *tab)
}

#[derive(Debug)]
pub(super) struct Page {
    pub(super) root: gtk::Widget,
    pub(super) notes: notes::Page,
    vault: super::vault_banner::VaultBanner,
    stack: adw::ViewStack,
    chips: Vec<(String, gtk::ToggleButton)>,
    snippets: gtk::Box,
    history: gtk::Box,
    switch: gtk::Switch,
    share_row: adw::ActionRow,
    /// The notes generation the Snippets tab was drawn from.
    snippets_shown: Cell<u64>,
    /// What the Clipboard history tab was drawn from.
    history_shown: RefCell<String>,
    /// Set while the switch is being moved by code, not by a click.
    syncing: Rc<Cell<bool>>,
}

/// A tab's body: scrolled, centred, with page margins.
fn tab_body(content: &gtk::Box) -> gtk::ScrolledWindow {
    content.set_margin_top(12);
    content.set_margin_bottom(28);
    content.set_margin_start(18);
    content.set_margin_end(18);
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(
            &adw::Clamp::builder()
                .maximum_size(760)
                .child(content)
                .build(),
        )
        .build()
}

impl Page {
    /// Saves a note being typed (the window is closing).
    pub(super) fn flush(&self) {
        self.notes.flush();
    }
    pub(super) fn new(win: &DesklinkWindow) -> Self {
        clipboard::ensure_css();
        let notes = notes::Page::new(win);
        let snippets = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let history = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();

        // The sharing switch: the head of the Clipboard tab.
        let switch = gtk::Switch::builder().valign(gtk::Align::Center).build();
        switch.update_property(&[gtk::accessible::Property::Label("Share clipboard")]);
        let share_row = ui_kit::list_row(
            "desklink-content-paste-symbolic",
            Tone::Accent,
            "Share clipboard",
            Some(clipboard::sharing_note()),
        );
        share_row.add_suffix(&switch);
        share_row.set_activatable_widget(Some(&switch));
        let clip_body = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .build();
        clip_body.append(&ui_kit::row_group(std::slice::from_ref(&share_row)));
        clip_body.append(&history);

        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(&notes.root, Some(TABS[0].0), TABS[0].1, TABS[0].2);
        stack.add_titled_with_icon(&tab_body(&snippets), Some(TABS[1].0), TABS[1].1, TABS[1].2);
        stack.add_titled_with_icon(&tab_body(&clip_body), Some(TABS[2].0), TABS[2].1, TABS[2].2);

        let (chip_row, chips) = clipboard::tab_chips(
            &TABS.map(|(name, title, icon)| (name, chip_label(name, title), icon)),
            TABS[0].0,
            {
                let stack = stack.downgrade();
                move |name| {
                    if let Some(stack) = stack.upgrade() {
                        stack.set_visible_child_name(name);
                    }
                }
            },
        );
        // Three chips in 336 px do not fit with large text: they scroll.
        let top = gtk::ScrolledWindow::builder()
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .margin_start(8)
            .margin_end(8)
            .margin_top(6)
            .margin_bottom(2)
            .child(&chip_row)
            .build();

        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let vault = super::vault_banner::VaultBanner::new(
            "notes",
            "notes and snippets",
            crate::device_links::notes::retry_blocked,
            crate::device_links::notes::start_fresh_blocked,
        );
        root.append(&vault.root);
        root.append(&top);
        stack.set_vexpand(true);
        root.append(&stack);

        let page = Self {
            root: root.upcast(),
            notes,
            vault,
            stack,
            chips,
            snippets,
            history,
            switch,
            share_row,
            snippets_shown: Cell::new(u64::MAX),
            history_shown: RefCell::default(),
            syncing: Rc::new(Cell::new(false)),
        };
        let (window, syncing) = (win.downgrade(), Rc::clone(&page.syncing));
        page.switch.connect_active_notify(move |switch| {
            if syncing.get() {
                return;
            }
            if let Some(window) = window.upgrade() {
                if let Some(daemon) = window.daemon() {
                    daemon.send(DaemonCommand::SetClipboardSync(switch.is_active()));
                }
                window.refresh_soon();
            }
        });
        page
    }

    /// Shows one of the tabs.
    pub(super) fn show_tab(&self, tab: &str) {
        if tab_of(tab).is_some() {
            self.stack.set_visible_child_name(tab);
            clipboard::pick_chip(&self.chips, tab);
        }
    }

    pub(super) fn update(&self, device: &DeviceView) {
        let on = crate::device_links::config::app_settings().clipboard_sync;
        self.share_row
            .set_title(&gtk::glib::markup_escape_text(&format!(
                "Share clipboard with {}",
                device.name
            )));
        self.share_row
            .set_subtitle(&gtk::glib::markup_escape_text(clipboard::sharing_note()));
        if self.switch.is_active() != on {
            self.syncing.set(true);
            self.switch.set_active(on);
            self.syncing.set(false);
        }
        self.notes.update();
        self.vault.update();
        let generation = crate::device_links::notes::generation();
        if self.snippets_shown.replace(generation) != generation {
            while let Some(child) = self.snippets.first_child() {
                self.snippets.remove(&child);
            }
            self.snippets.append(&clipboard::snippets_section());
        }
        let key = clipboard::history_key(on);
        if *self.history_shown.borrow() != key {
            self.history_shown.replace(key);
            while let Some(child) = self.history.first_child() {
                self.history.remove(&child);
            }
            self.history.append(&clipboard::history_box(on));
        }
        clipboard::refresh_times(self.history.upcast_ref());
    }
}
