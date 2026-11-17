//! Ctrl+F: search THIS page. A page that has a search box of its own
//! registers it ([`register`]); Ctrl+F focuses the one on the visible page
//! and selects its text, so typing replaces it. On a page without one the key
//! opens "Find anything" (the global search, Ctrl+K), so it is never dead:
//! that is the clean fallback, because the person asked to search and the
//! global search is the only search left. Where the focus is in text that is
//! not the page's own box (the note editor, a field), Ctrl+F stays the
//! text's. A terminal is embedded in the Terminals page: Ctrl+F reaches its
//! program (readline's forward-char) only because the window's capture-phase
//! key controller for terminals (`terminal_view::hand_keys_to_terminals`)
//! sends every Ctrl key but Ctrl+Shift+C/V/K and Ctrl+Tab to the focused
//! terminal and stops it (`key_route`; pinned by
//! `ctrl_keys_are_the_terminals_but_for_copy_paste_and_the_palette`). The
//! Phone Screen is a window of its own (an `adw::Window`, no `win.` actions),
//! so this accelerator does not exist there and its keys go to the phone.
//!
//! Sibling edges: none (the window's helpers).

use adw::prelude::*;
use adw::subclass::prelude::*;

use super::{named_widgets, palette, DesklinkWindow};

/// The widget name every page search box carries.
pub(super) const PAGE_SEARCH: &str = "page-search";

/// Marks `entry` as its page's search box: Ctrl+F finds it by name, and
/// Escape clears it and gives the focus back to the page.
pub(super) fn register(entry: &gtk::SearchEntry) {
    entry.set_widget_name(PAGE_SEARCH);
    entry.connect_stop_search(|entry| {
        entry.set_text("");
        entry.emit_move_focus(gtk::DirectionType::TabForward);
    });
}

/// The search box of the page on screen, if it has one showing.
fn visible_page_search(window: &DesklinkWindow) -> Option<gtk::SearchEntry> {
    named_widgets(window.imp().main_stack.upcast_ref(), PAGE_SEARCH)
        .into_iter()
        .filter(|widget| widget.is_mapped())
        .find_map(|widget| widget.downcast::<gtk::SearchEntry>().ok())
}

/// Ctrl+F.
pub(super) fn search_this_page(window: &DesklinkWindow) {
    // Not over a dialog (the palette is one: Ctrl+K closes it).
    if palette::is_open() || window.visible_dialog().is_some() {
        return;
    }
    let imp = window.imp();
    let focus = gtk::prelude::GtkWindowExt::focus(window);
    let page_search = visible_page_search(window);
    // In the page's own box already: select what is there.
    if let (Some(entry), Some(focus)) = (&page_search, &focus) {
        if focus.is_ancestor(entry) || focus == entry.upcast_ref::<gtk::Widget>() {
            entry.select_region(0, -1);
            return;
        }
    }
    // Other text (the note editor, a field) keeps Ctrl+F.
    if palette::typing_in(window) {
        return;
    }
    // The Terminals page: its search also leaves an open terminal (whose box
    // is then showing); if it cannot be reached the key falls through.
    let page_search = if imp.page.borrow().as_str() == "terminals"
        && imp.main_stack.visible_child_name().as_deref() == Some("devices")
    {
        if let Some(page) = imp.terminals.borrow().as_ref() {
            page.focus_search();
        }
        visible_page_search(window)
    } else {
        page_search
    };
    match page_search {
        Some(entry) => {
            entry.grab_focus();
            entry.select_region(0, -1);
        }
        None => palette::toggle(window, ""),
    }
}
