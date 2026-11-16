//! Where the Bucket page is and what is selected: the folder being browsed,
//! the tiles drawn, selection mode, toggling, ranges and select-all.
//!
//! Sibling edges: none (the parent's state types).

use super::*;

/// Where the page is and what is selected (F8c). Selection changes touch
/// the widgets in place: the page is not rebuilt, so scrolling stays.
#[derive(Default)]
pub(super) struct Browse {
    pub(super) device: String,
    /// The folder shown: "" the top, `A`, `A/B`.
    pub(super) current: String,
    pub(super) selecting: bool,
    pub(super) selected: BTreeSet<String>,
    /// For Shift-click ranges.
    pub(super) anchor: Option<String>,
    /// The items drawn, in order (never more than [`MAX_TILES`]): what
    /// Select All and Shift-click reach, so a destructive action only ever
    /// acts on what was on screen.
    pub(super) order: Vec<String>,
    /// Items in this folder that are not drawn.
    pub(super) hidden: usize,
    pub(super) tiles: Vec<TileRef>,
    pub(super) bar: Option<glib::WeakRef<gtk::Box>>,
    pub(super) count: Option<glib::WeakRef<gtk::Label>>,
    pub(super) toggle: Option<glib::WeakRef<gtk::ToggleButton>>,
}

pub(super) struct TileRef {
    pub(super) path: String,
    pub(super) button: glib::WeakRef<gtk::Button>,
    pub(super) check: glib::WeakRef<gtk::Box>,
}

thread_local! {
    pub(super) static BROWSE: std::cell::RefCell<Browse> = std::cell::RefCell::default();
}

pub(super) fn current() -> String {
    BROWSE.with(|browse| browse.borrow().current.clone())
}

/// Folder levels of the folder shown (0 at the top).
pub(super) fn current_depth() -> usize {
    let current = current();
    if current.is_empty() {
        0
    } else {
        current.split('/').count()
    }
}

pub(super) fn selecting() -> bool {
    BROWSE.with(|browse| browse.borrow().selecting)
}

pub(super) fn selected_paths() -> Vec<String> {
    BROWSE.with(|browse| browse.borrow().selected.iter().cloned().collect())
}

/// Shows the folder `path` ("" the top); selection ends.
pub(super) fn go_to(win: &DesklinkWindow, path: &str) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.current = path.to_string();
        browse.selecting = false;
        browse.selected.clear();
        browse.anchor = None;
    });
    win.refresh_soon();
}

pub(super) fn set_selecting(on: bool) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = on;
        if !on {
            browse.selected.clear();
            browse.anchor = None;
        }
    });
    refresh_selection();
}

/// Whether a plain click on a card opens it: not while selecting.
pub(super) fn opens_on_click(selecting: bool) -> bool {
    !selecting
}

/// Ctrl-click: one more in or out of the selection.
pub(super) fn toggle_item(path: &str) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = true;
        if !browse.selected.remove(path) {
            browse.selected.insert(path.to_string());
        }
        browse.anchor = Some(path.to_string());
    });
    refresh_selection();
}

/// Shift-click: from the last one clicked to this one.
pub(super) fn range_to(path: &str) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = true;
        let at = |name: &str, order: &[String]| order.iter().position(|item| item == name);
        let from = browse
            .anchor
            .as_deref()
            .and_then(|anchor| at(anchor, &browse.order))
            .or_else(|| at(path, &browse.order));
        if let (Some(from), Some(to)) = (from, at(path, &browse.order)) {
            let (low, high) = (from.min(to), from.max(to));
            let range: Vec<String> = browse.order[low..=high].to_vec();
            browse.selected.extend(range);
        }
    });
    refresh_selection();
}

pub(super) fn select_all() {
    BROWSE.with(|browse| browse.borrow_mut().select_all());
    refresh_selection();
}

impl Browse {
    /// A level of the Bucket is drawn: what Select All and Shift-click reach
    /// is the part that got a tile, and a selection keeps only what is drawn.
    pub(super) fn show_level<'a>(&mut self, paths: impl Iterator<Item = &'a str>) {
        let (order, hidden) = drawn(paths);
        self.hidden = hidden;
        self.tiles.clear();
        self.selected.retain(|path| order.contains(path));
        self.order = order;
    }

    /// Everything drawn, and nothing that is not.
    pub(super) fn select_all(&mut self) {
        self.selecting = true;
        self.selected = self.order.iter().cloned().collect();
    }
}

/// Makes the widgets say what the selection is (outside the borrow:
/// setting the toggle runs its handler).
pub(super) fn refresh_selection() {
    type Shown = (
        bool,
        Vec<(bool, glib::WeakRef<gtk::Button>, glib::WeakRef<gtk::Box>)>,
        Option<glib::WeakRef<gtk::Box>>,
        Option<glib::WeakRef<gtk::Label>>,
        Option<glib::WeakRef<gtk::ToggleButton>>,
        usize,
    );
    let (on, tiles, bar, count, toggle, selected): Shown = BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.tiles.retain(|tile| tile.button.upgrade().is_some());
        (
            browse.selecting,
            browse
                .tiles
                .iter()
                .map(|tile| {
                    (
                        browse.selected.contains(&tile.path),
                        tile.button.clone(),
                        tile.check.clone(),
                    )
                })
                .collect(),
            browse.bar.clone(),
            browse.count.clone(),
            browse.toggle.clone(),
            browse.selected.len(),
        )
    });
    for (chosen, button, check) in tiles {
        if let (Some(button), Some(check)) = (button.upgrade(), check.upgrade()) {
            if chosen {
                button.add_css_class("selected");
                check.add_css_class("on");
            } else {
                button.remove_css_class("selected");
                check.remove_css_class("on");
            }
            check.set_visible(on);
            button.update_state(&[gtk::accessible::State::Selected(if on {
                Some(chosen)
            } else {
                None
            })]);
        }
    }
    if let Some(bar) = bar.and_then(|bar| bar.upgrade()) {
        bar.set_visible(on);
    }
    if let Some(label) = count.and_then(|label| label.upgrade()) {
        label.set_label(&match selected {
            0 => "Select items".to_string(),
            1 => "1 selected".to_string(),
            count => format!("{count} selected"),
        });
    }
    if let Some(toggle) = toggle.and_then(|toggle| toggle.upgrade()) {
        if toggle.is_active() != on {
            toggle.set_active(on);
        }
    }
}

/// Debug builds: shots without clicking.
#[cfg(debug_assertions)]
pub(in crate::window) fn debug_open(win: &DesklinkWindow, path: &str) {
    go_to(win, path);
}

#[cfg(debug_assertions)]
pub(in crate::window) fn debug_select(paths: Vec<String>) {
    BROWSE.with(|browse| {
        let mut browse = browse.borrow_mut();
        browse.selecting = true;
        browse.selected = paths.into_iter().collect();
    });
    refresh_selection();
}

/// Tiles: two a line at 360 px, more as the page allows.
/// Tiles drawn at once (the grid is built whole, so a big folder must not
/// cost a page of thousands of widgets); past it, a line says how many more
/// there are and where they are.
pub(super) const MAX_TILES: usize = 300;

/// The paths of a level that get a tile, and how many do not.
fn drawn<'a>(level: impl Iterator<Item = &'a str>) -> (Vec<String>, usize) {
    let all: Vec<&str> = level.collect();
    let shown: Vec<String> = all
        .iter()
        .take(MAX_TILES)
        .map(|path| path.to_string())
        .collect();
    (shown, all.len().saturating_sub(MAX_TILES))
}
