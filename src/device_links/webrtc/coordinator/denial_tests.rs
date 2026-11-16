//! The words a phone is told when control of this computer is refused.

use super::control_denied_reason;

#[test]
fn a_view_only_desktop_is_not_told_to_turn_on_a_switch_it_lacks() {
    let sway = control_denied_reason(false, true);
    assert!(!sway.contains("Allow remote interaction"), "{sway}");
    assert!(
        sway.contains("watch its screen but not control it"),
        "{sway}"
    );
    let gnome = control_denied_reason(false, false);
    assert!(gnome.contains("Allow remote interaction"), "{gnome}");
    assert!(control_denied_reason(true, true).contains("not active"));
}
