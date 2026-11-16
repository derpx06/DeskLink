//! The approval and Glide notices live exactly as long as their dialogs.

use super::approval_changes;
use std::collections::HashSet;

#[test]
fn the_approval_notice_is_posted_once_and_withdrawn_when_answered() {
    let mut shown = HashSet::new();
    let waiting = vec!["phone-a".to_string()];
    assert_eq!(
        approval_changes(&waiting, &mut shown),
        (vec!["phone-a".to_string()], vec![])
    );
    assert_eq!(
        approval_changes(&waiting, &mut shown),
        (vec![], vec![]),
        "not again"
    );
    assert_eq!(
        approval_changes(&[], &mut shown),
        (vec![], vec!["phone-a".to_string()])
    );
    assert_eq!(approval_changes(&[], &mut shown), (vec![], vec![]));
}

#[test]
fn the_glide_notice_lives_exactly_as_long_as_the_dialog() {
    use super::glide_waiting;
    let mut shown = HashSet::new();
    let phone = || Some("phone-a".to_string());
    // Glide on but no dialog (the usual case): nothing is posted.
    assert_eq!(
        approval_changes(&glide_waiting(false, phone()), &mut shown),
        (vec![], vec![])
    );
    // The dialog goes up: posted once, however many ticks it takes.
    assert_eq!(
        approval_changes(&glide_waiting(true, phone()), &mut shown),
        (vec!["phone-a".to_string()], vec![])
    );
    assert_eq!(
        approval_changes(&glide_waiting(true, phone()), &mut shown),
        (vec![], vec![])
    );
    // Answered (or Glide switched off while it was up): withdrawn.
    assert_eq!(
        approval_changes(&glide_waiting(false, phone()), &mut shown),
        (vec![], vec!["phone-a".to_string()])
    );
    assert!(glide_waiting(true, None).is_empty());
}
