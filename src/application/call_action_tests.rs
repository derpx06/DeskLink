//! Which call actions skip the question.

use super::{call_action_kind, CallAction};

/// Nothing that starts something outward runs unasked through `app.call`.
#[test]
fn only_calls_already_going_skip_the_question() {
    assert_eq!(call_action_kind("dial"), CallAction::Dial);
    assert_eq!(call_action_kind("message"), CallAction::Message);
    for action in ["answer", "decline", "silence", "end", "mute", "unmute"] {
        assert_eq!(call_action_kind(action), CallAction::OnTheCall, "{action}");
    }
    for action in ["sms", "send", "dial ", "", "reply", "hangup-all"] {
        assert_eq!(call_action_kind(action), CallAction::Refused, "{action}");
    }
}
