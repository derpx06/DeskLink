//! "Your notes can't be saved yet": shown on a page whose sealed file could
//! not be read just now (the key, or the disk). What is typed meanwhile is
//! kept in memory; Try again reads the file again and merges it in, Start
//! fresh gives the old file up (a copy stays beside it).

use adw::prelude::*;

use super::ui_kit::banner::{banner, BannerKind};
use super::ui_kit::{button, ButtonKind};
use crate::device_links::vault;

#[derive(Debug)]
pub(super) struct VaultBanner {
    pub(super) root: gtk::Box,
    folder: &'static str,
}

/// The banner's headline for a page about `what` ("notes", "contacts").
pub(super) fn title(what: &str) -> String {
    format!("Your {what} can't be saved yet")
}

/// What happened, and what happens meanwhile.
const DETAIL: &str = "Their file couldn't be read just now (the key or the disk). \
     Nothing was lost. What you type meanwhile is kept until DeskLink quits.";

impl VaultBanner {
    /// `folder` is the vault folder ("notes", "contacts"); `retry` returns
    /// whether saving works again; `fresh` gives the old file up.
    pub(super) fn new(
        folder: &'static str,
        what: &'static str,
        retry: fn() -> bool,
        fresh: fn(),
    ) -> Self {
        let warning = banner(
            BannerKind::Warn,
            "desklink-warning-symbolic",
            &title(what),
            Some(DETAIL),
        );
        let again = button(ButtonKind::Tonal, "Try again", None);
        again.connect_clicked(move |button| {
            if !retry() {
                super::toast_from(button, "Still can't read it");
            }
        });
        let start = button(ButtonKind::Danger, "Start fresh…", None);
        start.connect_clicked(move |button| {
            super::confirm(
                button,
                "Start fresh?",
                "The old file is kept as a backup beside it. What you type from now on is saved.",
                "Start fresh",
                fresh,
            );
        });
        warning.add_action(&again);
        warning.add_action(&start);
        let root = warning.root;
        root.set_margin_start(12);
        root.set_margin_end(12);
        root.set_margin_top(6);
        root.set_margin_bottom(6);
        root.set_visible(false);
        Self { root, folder }
    }

    /// Shown while a file under the folder cannot be saved.
    pub(super) fn update(&self) {
        let blocked = !vault::blocked_under(self.folder).is_empty();
        if self.root.is_visible() != blocked {
            self.root.set_visible(blocked);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static RETRIES: AtomicU32 = AtomicU32::new(0);
    static FRESH: AtomicU32 = AtomicU32::new(0);

    fn buttons(widget: &gtk::Widget, found: &mut Vec<gtk::Button>) {
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            found.push(button.clone());
        }
        let mut child = widget.first_child();
        while let Some(next) = child {
            buttons(&next, found);
            child = next.next_sibling();
        }
    }

    /// Try Again asks to read the file again (and toasts when it still
    /// can't, which needs a window and is skipped without one); Start Fresh
    /// asks first and gives the old file up only when confirmed.
    /// (Skipped without a display.)
    #[test]
    fn try_again_retries_and_start_fresh_asks_before_giving_the_file_up() {
        crate::test_display::run(|| {
            let banner = VaultBanner::new(
                "banner-test",
                "notes",
                || {
                    RETRIES.fetch_add(1, Ordering::SeqCst);
                    false
                },
                || {
                    FRESH.fetch_add(1, Ordering::SeqCst);
                },
            );
            let window = adw::Window::new();
            window.set_content(Some(&banner.root));
            let mut found = Vec::new();
            buttons(banner.root.upcast_ref(), &mut found);
            let labelled = |text: &str| {
                found
                    .iter()
                    .find(|button| button.label().as_deref() == Some(text))
                    .unwrap_or_else(|| panic!("no {text:?} button"))
                    .clone()
            };
            assert!(!banner.root.is_visible(), "hidden while nothing is blocked");
            labelled("Try again").emit_clicked();
            assert_eq!(RETRIES.load(Ordering::SeqCst), 1);
            assert_eq!(FRESH.load(Ordering::SeqCst), 0);

            labelled("Start fresh…").emit_clicked();
            assert_eq!(FRESH.load(Ordering::SeqCst), 0, "asked first");
            let dialog = window
                .visible_dialog()
                .and_downcast::<adw::AlertDialog>()
                .expect("a question was asked");
            assert_eq!(dialog.heading().as_deref(), Some("Start fresh?"));
            // Cancel (the close response) gives nothing up.
            dialog.close();
            let context = gtk::glib::MainContext::default();
            while context.iteration(false) {}
            assert_eq!(FRESH.load(Ordering::SeqCst), 0);

            // Asked again and confirmed, it does.
            labelled("Start fresh…").emit_clicked();
            let dialog = window
                .visible_dialog()
                .and_downcast::<adw::AlertDialog>()
                .expect("asked again");
            let mut found = Vec::new();
            buttons(dialog.upcast_ref(), &mut found);
            let go = found
                .iter()
                .find(|button| button.label().as_deref() == Some("Start fresh"))
                .expect("the dialog's Start Fresh button");
            go.emit_clicked();
            while context.iteration(false) {}
            assert_eq!(FRESH.load(Ordering::SeqCst), 1, "confirmed");
        });
    }
}
