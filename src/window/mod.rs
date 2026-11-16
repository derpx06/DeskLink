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
//!
//! Layout: this file keeps the page table, the window type (`glib::wrapper!`
//! over `imp`), the helpers the pages share and the crate-facing re-exports.
//! `DesklinkWindow` is one type whose methods are split by responsibility:
//! `navigation` (which device and page show, onboarding), `refresh` (the
//! daemon connection, the poll, layout) and `notices` (what the person is
//! told). `imp` is its GObject implementation, `words` the error and device
//! words, `blocks` the small widgets the pages build with, and `debug_verbs`
//! the debug-build e2e verbs. These parts call each other's methods because
//! they are one type in several files; outside that type there are no cycles.
//! Each page module (`overview`, `bucket`, `prefs`, ...) depends on this
//! module, never on another page's internals.

pub(crate) mod announce;
mod bucket;
pub(crate) mod calls;
mod dialpad;
mod everything;
mod gallery;
pub(crate) mod insert;
mod sound;
mod system_check;
mod vault_banner;
pub(crate) use calls::{confirm_dial, confirm_message};
mod clipboard;
mod contacts;
mod guide;
pub(crate) mod history;
mod notes;
mod notes_clipboard;
pub(crate) mod notifications;
mod onboarding;
mod overview;
pub(crate) mod requirements;
pub(crate) use overview::link_to_send;
mod pairing;
mod palette;
mod palette_model;
mod pdf_view;
pub(crate) mod prefs;
mod sidebar;
#[cfg(feature = "vte")]
mod terminal_view;
mod terminals;
mod transfers;
pub(crate) mod ui_kit;
mod viewer;

use adw::prelude::*;
use adw::subclass::prelude::*;
use gtk::{gio, glib};

use crate::device_links::daemon::{DaemonCommand, DaemonEvent, DaemonHandle};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::features::FeatureRegistry;
use crate::device_links::webrtc::transfer_board::{Finished, Outcome};

mod blocks;
#[cfg(debug_assertions)]
mod debug_verbs;
mod imp;
mod navigation;
mod notices;
mod refresh;
mod words;
use blocks::*;
#[cfg(debug_assertions)]
use debug_verbs::*;
use words::*;

// The crate-facing surface (everything else stays inside).
pub(crate) use blocks::{confirm, remember_send_folder, send_files_dialog};
pub(crate) use words::{because, device_type_icon, device_type_label};

/// Whether a Ctrl+digit page shortcut does anything now. Not over a dialog
/// (as Ctrl+N is not), and the pages of a phone only for a paired one: a
/// phone still pairing has just its overview, and showing the page would
/// flicker. Notes & clipboard is the user's own and opens with no phone too
/// (as the solo view); the other pages do nothing there.
fn page_shortcut_applies(page: &str, dialog_open: bool, paired: bool) -> bool {
    !dialog_open && (paired || page == "notes")
}

/// Whether the window lists `device`: a paired one, one mid-pairing, or a
/// `refused` one (it did not accept our request; its page says so).
fn is_listed(device: &DeviceView, refused: bool) -> bool {
    if refused && !device.trusted {
        return true;
    }
    (device.trusted
        || matches!(
            device.status,
            DeviceStatus::PairRequested | DeviceStatus::PairRequestedByPeer
        ))
        && matches!(
            device.status,
            DeviceStatus::Paired
                | DeviceStatus::Unreachable
                | DeviceStatus::PairRequested
                | DeviceStatus::PairRequestedByPeer
        )
}

/// The device the window should be about. `chosen` is the person's own
/// choice (or the one remembered from last time) and always wins while it is
/// listed. Otherwise the choice is ours and provisional: keep `current` while
/// it is reachable, else the first reachable phone, else `current` while it is
/// listed, else the first. So a stale remembered id that put an unreachable
/// phone up at start gives way when a phone connects.
fn device_to_show(
    chosen: Option<&str>,
    current: Option<&str>,
    devices: &[DeviceView],
) -> Option<String> {
    let listed = |id: &&str| devices.iter().any(|device| device.id == *id);
    let reachable = |id: &&str| {
        devices
            .iter()
            .any(|device| device.id == *id && matches!(device.status, DeviceStatus::Paired))
    };
    chosen
        .filter(listed)
        .or(current.filter(reachable))
        .or(devices
            .iter()
            .find(|device| matches!(device.status, DeviceStatus::Paired))
            .map(|device| device.id.as_str()))
        .or(current.filter(listed))
        .or(devices.first().map(|device| device.id.as_str()))
        .map(str::to_string)
}

/// The pages, in sidebar order: (name, title, icon). The last is the
/// sidebar's footer. Frequent pages cluster at the top; Contacts, which is
/// read-only and rare, comes after Terminals (`UI-REDESIGN.md` 3.9).
/// How long the page takes to fade in when the window first shows.
const FIRST_FADE_MS: u32 = 180;

/// The Files page's width (the Bucket and the transfers), and on a big
/// window: more cards a line, each the same size.
const FILES_WIDTH: i32 = 1000;
const FILES_WIDE_WIDTH: i32 = 1600;

const PAGES: &[(&str, &str, &str)] = &[
    ("home", "Home", "desklink-home-symbolic"),
    ("files", "Files", "desklink-folder-symbolic"),
    ("notes", "Notes & clipboard", "desklink-edit-note-symbolic"),
    (
        "notifications",
        "Notifications",
        "desklink-notifications-symbolic",
    ),
    ("terminals", "Terminals", "desklink-terminal-symbolic"),
    ("contacts", "Contacts", "desklink-person-symbolic"),
    ("settings", "Settings", "desklink-settings-symbolic"),
];

/// Ctrl+1 to Ctrl+6 open the main pages in sidebar order (`win.show-page`);
/// Settings is Ctrl+, (`app.preferences`). Shown in the shortcuts dialog.
pub(crate) const PAGE_ACCELS: [(&str, &str); 6] = [
    ("home", "<control>1"),
    ("files", "<control>2"),
    ("notes", "<control>3"),
    ("notifications", "<control>4"),
    ("terminals", "<control>5"),
    ("contacts", "<control>6"),
];

/// Ctrl+K opens "Find anything" (`win.palette`); Ctrl+F does too, off the
/// Terminals page and outside a text field. Shown in the shortcuts dialog, on
/// the search buttons, as key caps in the sidebar's search field and next to
/// nothing else.
pub(crate) const PALETTE_ACCEL: &str = "<control>k";

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

    fn daemon(&self) -> Option<DaemonHandle> {
        self.imp().daemon.borrow().clone()
    }

    fn selected_id(&self) -> Option<String> {
        self.imp().selected_device_id.borrow().clone()
    }
}

/// How many toasts may be waiting or showing at once.
const TOASTS_AT_ONCE: usize = 3;

#[derive(Debug, PartialEq, Eq)]
enum ToastPlan {
    /// The same words are already up: nothing to add.
    Skip,
    /// Dismiss this many of the oldest first, then add.
    Dismiss(usize),
    Add,
}

/// What to do with a new toast titled `new` while `live` (oldest first) are up.
fn toast_plan(live: &[String], new: &str) -> ToastPlan {
    if live.iter().any(|title| title == new) {
        ToastPlan::Skip
    } else if live.len() >= TOASTS_AT_ONCE {
        ToastPlan::Dismiss(live.len() + 1 - TOASTS_AT_ONCE)
    } else {
        ToastPlan::Add
    }
}

/// Whether the pages are drawn now: only while the window is on screen.
fn should_draw_pages(mapped: bool) -> bool {
    mapped
}

/// What kind of link `url` is, in words for the question.
/// Approve the phone controlling this screen (the desktop's own dialog):
/// Allow on a phone's "control" ask.
pub(crate) fn approve_control(window: &DesklinkWindow, device_id: &str) {
    prefs::approve_remote_control(window, device_id);
}

pub(crate) fn link_kind_words(url: &str) -> &'static str {
    match url
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase())
        .as_deref()
    {
        Some("mailto") => "an email link",
        Some("sms") => "a text message link",
        Some("geo") => "a map location",
        Some("tel") => "a phone number link",
        _ => "a link",
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
        crate::device_links::transfer_log::downloads_dir().as_deref(),
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
#[cfg(debug_assertions)]
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

/// Where the window's size is kept: `~/.config/desklink/window-size`
/// ("width height maximized"), beside `last-device`.
fn window_size_path() -> Option<std::path::PathBuf> {
    Some(dirs::config_dir()?.join("desklink").join("window-size"))
}

/// "1180 760 0" as a size and whether it was maximized; nothing for text
/// that is not a sane size.
fn parse_window_size(text: &str) -> Option<(i32, i32, bool)> {
    let mut parts = text.split_whitespace();
    let width: i32 = parts.next()?.parse().ok()?;
    let height: i32 = parts.next()?.parse().ok()?;
    let maximized = parts.next() == Some("1");
    ((360..=10_000).contains(&width) && (294..=10_000).contains(&height))
        .then_some((width, height, maximized))
}

/// Opens at the size it had last time, and keeps the size when it is
/// closed (hidden) or the app quits.
fn remember_window_size(window: &DesklinkWindow) {
    // Test windows leave the person's own size alone.
    if cfg!(test) {
        return;
    }
    if let Some((width, height, maximized)) = window_size_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| parse_window_size(&text))
    {
        window.set_default_size(width, height);
        if maximized {
            window.maximize();
        }
    }
    let save = |window: &DesklinkWindow| {
        let (width, height) = window.default_size();
        let Some(path) = window_size_path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!("{width} {height} {}", u8::from(window.is_maximized()));
        let _ = std::fs::write(path, text);
    };
    window.connect_close_request(move |window| {
        save(window);
        glib::Propagation::Proceed
    });
    window.connect_unrealize(move |window| {
        if let Some(window) = window.downcast_ref::<DesklinkWindow>() {
            save(window);
        }
    });
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

pub(crate) fn can_send_feature(device: &DeviceView, packet_type: &str) -> bool {
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

/// Whether a `gtk-decoration-layout` puts anything at the start of a header
/// bar (before the colon): the window icon or a window button. GTK 4 draws
/// nothing for "menu" (there are no app menus), which is Cinnamon's and
/// MATE's start.
pub(crate) fn crowded_start(layout: &str) -> bool {
    layout
        .split(':')
        .next()
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .any(|item| matches!(item, "icon" | "minimize" | "maximize" | "close"))
}

#[cfg(test)]
mod decoration_tests;
#[cfg(test)]
mod jargon_tests;
#[cfg(test)]
mod tests;
