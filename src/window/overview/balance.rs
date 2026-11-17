//! Which Home column each movable card sits in, so two columns end near the
//! same height. Placed when Home is built (and again on a resize across the
//! wide breakpoint or a card appearing or going), never on a tick; a card
//! keeps its column from one build to the next unless the columns are then
//! clearly out of balance.
//!
//! Sibling edges: none (the parent builds the cards).

use std::cell::RefCell;
use std::collections::HashMap;

use super::*;

/// The columns may differ by this much (px) before a remembered placement is
/// given up: a notification arriving must not move a card under the pointer.
const KEEP_WITHIN: i32 = 160;

thread_local! {
    /// Per phone: the ids that sat on the left at the last wide build.
    static REMEMBERED: RefCell<HashMap<String, Vec<String>>> = RefCell::new(HashMap::new());
}

/// For each card (id, height) in importance order, whether it goes left.
/// `left0` and `right0` are what each column already holds. A remembered
/// placement is kept while its columns differ by at most [`KEEP_WITHIN`]
/// (or a sixth of the total); otherwise each card goes to the shorter column.
pub(super) fn assign(
    cards: &[(String, i32)],
    left0: i32,
    right0: i32,
    remembered: Option<&[String]>,
) -> Vec<bool> {
    const GAP: i32 = 16;
    let heights = |placed: &[bool]| {
        let (mut left, mut right) = (left0, right0);
        for ((_, height), is_left) in cards.iter().zip(placed) {
            if *is_left {
                left += height + GAP;
            } else {
                right += height + GAP;
            }
        }
        (left, right)
    };
    if let Some(left_ids) = remembered {
        let placed: Vec<bool> = cards.iter().map(|(id, _)| left_ids.contains(id)).collect();
        let (left, right) = heights(&placed);
        let total = left + right;
        if (left - right).abs() <= KEEP_WITHIN.max(total / 6) {
            return placed;
        }
    }
    let (mut left, mut right) = (left0, right0);
    cards
        .iter()
        .map(|(_, height)| {
            if left <= right {
                left += height + GAP;
                true
            } else {
                right += height + GAP;
                false
            }
        })
        .collect()
}

/// Splits `cards` (id, widget, importance order) between the two columns.
/// Below the wide breakpoint the reading order stays as it always was:
/// Notifications and Files, then the rest.
pub(super) fn place(
    win: &DesklinkWindow,
    device_id: &str,
    left: &gtk::Box,
    right: &gtk::Box,
    cards: Vec<(&'static str, gtk::Widget)>,
) {
    use adw::subclass::prelude::ObjectSubclassIsExt;
    let wide = win.imp().wide.get();
    let goes_left: Vec<bool> = if wide {
        let width = (win.width().max(1280) - 310).min(super::HOME_WIDE_WIDTH);
        let column = ((width - 24) / 2).max(300);
        let height_of = |widget: &gtk::Widget| widget.measure(gtk::Orientation::Vertical, column).1;
        let measured: Vec<(String, i32)> = cards
            .iter()
            .map(|(id, widget)| (id.to_string(), height_of(widget)))
            .collect();
        let remembered = REMEMBERED.with(|map| map.borrow().get(device_id).cloned());
        let placed = assign(
            &measured,
            height_of(left.upcast_ref()),
            height_of(right.upcast_ref()),
            remembered.as_deref(),
        );
        REMEMBERED.with(|map| {
            map.borrow_mut().insert(
                device_id.to_string(),
                cards
                    .iter()
                    .zip(&placed)
                    .filter(|(_, left)| **left)
                    .map(|((id, _), _)| id.to_string())
                    .collect(),
            )
        });
        placed
    } else {
        cards
            .iter()
            .map(|(id, _)| matches!(*id, "notifications" | "files"))
            .collect()
    };
    for ((_, widget), is_left) in cards.iter().zip(goes_left) {
        if is_left { left } else { right }.append(widget);
    }
}

#[cfg(test)]
mod tests {
    use super::assign;

    fn cards(heights: &[i32]) -> Vec<(String, i32)> {
        heights
            .iter()
            .enumerate()
            .map(|(i, h)| (format!("c{i}"), *h))
            .collect()
    }

    /// The shorter column takes the next card, so the ends meet.
    #[test]
    fn each_card_goes_to_the_shorter_column() {
        // Left holds 600, right 0: notifications (260) right, files (200) right,
        // notes (300) right (460 < 600), terminals (90) left.
        let placed = assign(&cards(&[260, 200, 300, 90]), 600, 0, None);
        assert_eq!(placed, [false, false, false, true]);
    }

    /// A card stays where it was while the columns are near; it moves when
    /// they are clearly apart.
    #[test]
    fn a_card_keeps_its_column_unless_the_balance_is_clearly_off() {
        let remembered = vec!["c0".to_string()];
        // c0 left (300), c1 right (350): 650 vs 366, 284 apart of 1016: kept? total/6 = 169, 160 -> 284 > 169: rebalanced.
        let moved = assign(&cards(&[300, 350]), 300, 0, Some(&remembered));
        assert_ne!(moved, [true, false], "clearly off: placed afresh");
        // Near: kept.
        let kept = assign(&cards(&[300, 350]), 300, 200, Some(&remembered));
        assert_eq!(kept, [true, false], "near enough: where it was");
    }
}
