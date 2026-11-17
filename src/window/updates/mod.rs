//! The update popup and its Settings controls (`docs/UPDATES.md` section 6).
//! The popup is a dialog over whatever page is showing, shown only when the
//! person opens DeskLink (a cold start, or the window shown again after it was
//! hidden) or asks for it (the top bar's row, Check for updates); it is not in
//! any page, and nothing here posts a notification. Settings carries only the
//! controls.
//!
//! Layout: `model` (the popup's words and rows, pure), `words` (the strings),
//! `illustration` (the drawn picture), `dialog` (the `adw::Dialog` and its
//! guidance panel), `coordinator` (when it shows: the glue over
//! `update::plan_open`), `settings` (the Updates group in Settings).
//!
//! May depend on: `crate::update` (the snapshot, `Command`, the open rules,
//! the install source), `ui_kit`, `link_glyph::token_color`. Used by:
//! `application` (the window being shown), `shell_dbus` (`ShowUpdate`),
//! `prefs` (the group) and the debug verbs.

use adw::prelude::*;
use adw::subclass::prelude::ObjectSubclassIsExt;
use gtk::{gio, glib};

use super::ui_kit::{self, ButtonKind, Tone};
use super::DesklinkWindow;

mod coordinator;
mod dialog;
mod illustration;
mod model;
mod settings;
mod words;

// The crate-facing surface.
pub(crate) use coordinator::{on_window_presented, show_requested};
pub(in crate::window) use settings::updates_rows;

/// The main window, when the application has one (for the Settings row).
pub(in crate::window) fn main_window() -> Option<DesklinkWindow> {
    gio::Application::default()
        .and_downcast::<crate::application::DesklinkApplication>()
        .map(|app| app.main_window())
}
#[cfg(debug_assertions)]
pub(in crate::window) use dialog::debug_show;

#[cfg(test)]
mod tests;

/// The muted line under a phone's name when it runs another DeskLink minor
/// version (`docs/UPDATES.md` 6.8): never a card, never a gate.
pub(in crate::window) fn skew_line(
    device: &crate::device_links::device::DeviceView,
) -> Option<String> {
    let ours = crate::update::Version::parse(crate::config::VERSION)?;
    match crate::update::skew(ours, device.app_version.as_deref())? {
        crate::update::Skew::PhoneOlder(v) => Some(words::skew_older(&v.to_string())),
        crate::update::Skew::PhoneNewer(v) => Some(words::skew_newer(&v.to_string())),
    }
}

/// Whether a Settings row title is one of the update controls: desk-only
/// controls that are not in the settings contract shared with the phone.
#[cfg(test)]
pub(in crate::window) fn is_update_row(title: &str) -> bool {
    DESK_ONLY_ROWS.contains(&title)
}

/// The titles of the update controls, spelled out (not read from `words`),
/// so renaming one is a decision, not a side effect: the contract test lets
/// exactly these through.
#[cfg(test)]
pub(in crate::window) const DESK_ONLY_ROWS: [&str; 5] = [
    "Updates",
    "Check for updates",
    "Check automatically",
    "Update channel",
    "What a check sends",
];
