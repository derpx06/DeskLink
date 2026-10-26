//! Notes & Clipboard: one page, three tabs (Notes | Snippets | Clipboard
//! history), the clipboard sharing switch in its header. Replaces the
//! Clipboard page and the Notes page of earlier builds.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use super::{clipboard, notes, DesklinkWindow};
use crate::device_links::daemon::DaemonCommand;
use crate::device_links::device::DeviceView;

/// The tabs, in order: (name, title, icon).
pub(super) const TABS: [(&str, &str, &str); 3] = [
    ("notes", "Notes", "document-edit-symbolic"),
    ("snippets", "Snippets", "view-pin-symbolic"),
    ("clipboard", "Clipboard history", "edit-paste-symbolic"),
];

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
    snippets: gtk::Box,
    history: gtk::Box,
    switch: gtk::Switch,
    share_label: gtk::Label,
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
        let notes = notes::Page::new(win);
        let snippets = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let history = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        let stack = adw::ViewStack::new();
        stack.add_titled_with_icon(&notes.root, Some(TABS[0].0), TABS[0].1, TABS[0].2);
        stack.add_titled_with_icon(&tab_body(&snippets), Some(TABS[1].0), TABS[1].1, TABS[1].2);
        stack.add_titled_with_icon(&tab_body(&history), Some(TABS[2].0), TABS[2].1, TABS[2].2);

        let switcher = adw::ViewSwitcher::builder()
            .stack(&stack)
            .policy(adw::ViewSwitcherPolicy::Wide)
            .build();
        switcher.set_widget_name("tabs-switcher");
        let bar = adw::ViewSwitcherBar::builder().stack(&stack).build();
        bar.set_widget_name("tabs-bar");

        let share_label = gtk::Label::builder()
            .label("Share clipboard")
            .css_classes(["body-small", "text-dim"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .build();
        let switch = gtk::Switch::builder()
            .valign(gtk::Align::Center)
            .tooltip_text("Share the clipboard")
            .build();
        switch.update_property(&[gtk::accessible::Property::Label("Share the clipboard")]);
        share_label.set_mnemonic_widget(Some(&switch));
        let share = gtk::Box::builder()
            .spacing(8)
            .halign(gtk::Align::End)
            .hexpand(true)
            .build();
        share.set_widget_name("share-clipboard");
        share.append(&share_label);
        share.append(&switch);

        let top = gtk::Box::builder()
            .spacing(12)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        top.append(&switcher);
        top.append(&share);

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
        root.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        stack.set_vexpand(true);
        root.append(&stack);
        root.append(&bar);

        let page = Self {
            root: root.upcast(),
            notes,
            vault,
            stack,
            snippets,
            history,
            switch,
            share_label,
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
        }
    }

    pub(super) fn update(&self, device: &DeviceView) {
        let on = crate::device_links::config::app_settings().clipboard_sync;
        self.share_label
            .set_label(&format!("Share clipboard with {}", device.name));
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
    }
}
