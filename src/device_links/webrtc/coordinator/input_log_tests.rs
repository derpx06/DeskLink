//! The debug log line for a burst of the phone's input.

use super::{input_log, PortalInputAction};

#[test]
fn logged_phone_input_never_says_what_was_typed() {
    let shown = input_log(&[
        PortalInputAction::TypeText {
            text: "hunter2".into(),
            modifiers: 0,
        },
        PortalInputAction::Key {
            keysym: 0x61,
            pressed: true,
        },
        PortalInputAction::Scroll(0.0, 1.0),
    ]);
    assert_eq!(shown, "TypeText(7 chars), Key, Scroll(0.0, 1.0)");
}
