//! "Your notes can't be saved yet": shown on a page whose sealed file could
//! not be read just now (the key, or the disk). What is typed meanwhile is
//! kept in memory; Try Again reads the file again and merges it in, Start
//! Fresh gives the old file up (a copy stays beside it).

use adw::prelude::*;

use crate::device_links::vault;

#[derive(Debug)]
pub(super) struct VaultBanner {
    pub(super) root: gtk::Box,
    label: gtk::Label,
    folder: &'static str,
}

/// The words of the banner for a page about `what` ("notes", "contacts").
pub(super) fn words(what: &str) -> String {
    format!(
        "Your {what} can't be saved yet: their file couldn't be read just now (the key or the disk). \
         Nothing was lost. What you type meanwhile is kept until DeskLink quits."
    )
}

impl VaultBanner {
    /// `folder` is the vault folder ("notes", "contacts"); `retry` returns
    /// whether saving works again; `fresh` gives the old file up.
    pub(super) fn new(
        folder: &'static str,
        what: &'static str,
        retry: fn() -> bool,
        fresh: fn(),
    ) -> Self {
        let label = gtk::Label::builder()
            .label(words(what))
            .wrap(true)
            .xalign(0.0)
            .hexpand(true)
            .build();
        let again = gtk::Button::with_label("Try Again");
        again.connect_clicked(move |button| {
            if !retry() {
                super::toast_from(button, "Still can't read it");
            }
        });
        let start = gtk::Button::with_label("Start Fresh…");
        start.add_css_class("destructive-action");
        start.connect_clicked(move |button| {
            super::confirm(
                button,
                "Start fresh?",
                "The old file is kept as a backup beside it. What you type from now on is saved.",
                "Start Fresh",
                fresh,
            );
        });
        let root = gtk::Box::builder()
            .spacing(12)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .css_classes(["state-banner"])
            .visible(false)
            .build();
        root.append(&label);
        root.append(&again);
        root.append(&start);
        root.update_property(&[gtk::accessible::Property::Label(&words(what))]);
        Self {
            root,
            label,
            folder,
        }
    }

    /// Shown while a file under the folder cannot be saved.
    pub(super) fn update(&self) {
        let blocked = !vault::blocked_under(self.folder).is_empty();
        if self.root.is_visible() != blocked {
            self.root.set_visible(blocked);
        }
        let _ = &self.label;
    }
}
