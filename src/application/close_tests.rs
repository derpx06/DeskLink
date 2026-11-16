//! What closing the main window does, with and without a way back.

use super::*;

#[test]
fn a_window_is_never_hidden_with_no_way_to_get_it_back() {
    assert_eq!(close_action(true, true, false), CloseAction::Hide);
    assert_eq!(close_action(true, false, false), CloseAction::Hide);
    assert_eq!(close_action(false, true, false), CloseAction::HideAndTell);
    assert_eq!(close_action(false, false, false), CloseAction::Minimize);
}

#[test]
fn closing_twice_where_a_window_cannot_minimise_quits() {
    assert_eq!(close_action(false, false, true), CloseAction::Quit);
    // A tray or a notification service means the first close is enough.
    assert_eq!(close_action(false, true, true), CloseAction::HideAndTell);
    assert_eq!(close_action(true, false, true), CloseAction::Hide);
    assert_eq!(QUIT_ON_SECOND_CLOSE, std::time::Duration::from_secs(2));
}

#[test]
fn the_no_tray_notice_comes_once_per_run() {
    let mut told = false;
    assert!(take_hide_notice(&mut told));
    assert!(!take_hide_notice(&mut told));
}
