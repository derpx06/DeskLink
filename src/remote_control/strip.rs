//! The Phone Screen window's strip: what it says about the session, control
//! and typing (`status_notice`), with its one button; and the paced counts
//! the window logs instead of a line per event. Pure: no GTK, tested in the
//! parent's tests.

use crate::device_links::webrtc::remote_session::SessionState;

/// Why the phone gave no control, and the one action. The phone refuses
/// control of its screen for one reason a person can fix: DeskLink's
/// accessibility service is off there. There is no per-computer switch for
/// this. A phone that can be asked for it ([`Seen::askable`]) gets Ask on
/// phone (its Allow opens the service's setup there); an older one posts its
/// own notification, and the action asks for control again.
fn control_denied(seen: Seen) -> Notice {
    use crate::device_links::asks::{ask_label, words, AskState};
    let (denial, phone) = (seen.denial.unwrap_or_default(), seen.phone);
    let again = Some(("Ask again", Action::RequestControl));
    if !denial.to_ascii_lowercase().contains("accessibility") {
        return Notice {
            stopped: false,
            text: format!("View only: {phone} did not allow control ({denial})"),
            button: again,
        };
    }
    // It was on and the phone's system switched it off (ColorOS does on every
    // force stop and app update): said as such, not as never set up.
    let stopped = denial.contains("turned off") || denial.contains("not running");
    if stopped && seen.control_ask != Some(AskState::Waiting) {
        return Notice {
            stopped: true,
            text: format!(
                "View only: DeskLink\u{2019}s accessibility service is not running on {phone}. \
                 Turn it off and on again there."
            ),
            button: if seen.askable {
                Some((ask_label(seen.control_ask), Action::AskPhone))
            } else {
                again
            },
        };
    }
    if !seen.askable {
        return Notice {
            stopped: false,
            text: format!(
                "View only: control needs DeskLink\u{2019}s accessibility service on {phone}. \
                 Turn it on there."
            ),
            button: again,
        };
    }
    match seen.control_ask {
        Some(AskState::Waiting) => Notice {
            stopped: false,
            text: words(AskState::Waiting, phone),
            button: None,
        },
        Some(state) => Notice {
            stopped: false,
            text: words(state, phone),
            button: Some((ask_label(Some(state)), Action::AskPhone)),
        },
        None => Notice {
            stopped: false,
            text: format!(
                "View only: control needs DeskLink\u{2019}s accessibility service on {phone}. \
                 Ask on phone, then Allow there."
            ),
            button: Some((ask_label(None), Action::AskPhone)),
        },
    }
}

/// What the strip's one button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    /// Ask the phone for control again.
    RequestControl,
    /// Ask on phone (`asks.json` "control"): a request there whose Allow
    /// opens DeskLink's accessibility setup.
    AskPhone,
}

/// One line about the phone screen's state, shown in a strip *above* the picture
/// (never over it), with its one button (label and action), if any.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) text: String,
    pub(crate) button: Option<(&'static str, Action)>,
    /// The phone's service was on and Android turned it off (its tooltip says why).
    pub(crate) stopped: bool,
}

/// What the window knows when it words its state.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Seen<'a> {
    pub(crate) state: Option<SessionState>,
    /// The window stopped asking: the phone said no, stopped, or never answered.
    pub(crate) gave_up: bool,
    /// ... because the phone never answered within [`ASK_WAIT`].
    pub(crate) timed_out: bool,
    /// The phone's person tapped Allow (F33): Android's own share sheet is next.
    pub(crate) answered: bool,
    pub(crate) has_picture: bool,
    /// Why the phone refused control.
    pub(crate) denial: Option<&'a str>,
    /// Why the phone ended the view ("declined": Not now on its prompt).
    pub(crate) ended: Option<&'a str>,
    pub(crate) lease_active: bool,
    pub(crate) phone: &'a str,
    /// The phone can be asked for control (its report has `control`).
    pub(crate) askable: bool,
    /// Where an Ask on phone for control stands.
    pub(crate) control_ask: Option<crate::device_links::asks::AskState>,
    /// The phone's recent word on keys typed here that reached no text field
    /// there (a `TYPING_*` code).
    pub(crate) typing: Option<&'a str>,
    /// A key was typed here a moment ago without control of the phone.
    pub(crate) typed_without_control: bool,
}

/// How long the strip keeps a word about typing after the last key.
pub(super) const TYPING_HINT_FOR: std::time::Duration = std::time::Duration::from_secs(6);

/// Events counted, and logged with the count at most once per interval,
/// never per event: a drag is dozens a second, and a line per key would give
/// away the rhythm (and length) of what is typed, a password too.
#[derive(Debug)]
pub(crate) struct PacedCount {
    every: std::time::Duration,
    count: u32,
    logged: Option<std::time::Instant>,
}

impl PacedCount {
    pub(crate) fn every(every: std::time::Duration) -> Self {
        Self {
            every,
            count: 0,
            logged: None,
        }
    }

    /// Counts one event; how many to log now, when it is time to.
    pub(crate) fn count(&mut self, now: std::time::Instant) -> Option<u32> {
        self.count += 1;
        if self
            .logged
            .is_some_and(|at| now.saturating_duration_since(at) < self.every)
        {
            return None;
        }
        self.logged = Some(now);
        Some(std::mem::take(&mut self.count))
    }
}

/// The strip's words for the phone's control denial `denial` (`wire_values.json`
/// pins the phone's reasons).
#[cfg(test)]
pub(crate) fn denial_words(denial: &str, phone: &str) -> String {
    control_denied(Seen {
        state: Some(SessionState::Viewing),
        has_picture: true,
        denial: Some(denial),
        phone,
        askable: true,
        ..Seen::default()
    })
    .text
}

/// What the strip says when the phone's keys reached no text field there.
pub(crate) fn typing_words(code: &str, phone: &str) -> String {
    use crate::device_links::packet::{
        TYPING_ACCESSIBILITY_OFF, TYPING_LOCKED, TYPING_NOT_EDITABLE, TYPING_NO_TEXT_FIELD,
    };
    match code {
        TYPING_LOCKED => format!("Typing: {phone} is locked \u{b7} unlock it first"),
        TYPING_NO_TEXT_FIELD => {
            format!("Typing: no text field is selected on {phone} \u{b7} click one first")
        }
        TYPING_NOT_EDITABLE => {
            format!("Typing: what is selected on {phone} does not take text \u{b7} click a text field")
        }
        TYPING_ACCESSIBILITY_OFF => format!(
            "Typing needs DeskLink\u{2019}s accessibility service on {phone} \u{b7} turn it on there"
        ),
        _ => format!("Typing: the app on {phone} did not take that key"),
    }
}

/// What the banner's tooltip says beyond its title: why the service may have
/// stopped, and, with a button, the steps for a greyed-out switch.
pub(crate) fn banner_tooltip(notice: &Notice, denial: &str, steps: &str) -> Option<String> {
    let mut parts = Vec::new();
    if notice.stopped {
        parts.push(
            "Some phones turn the service off after an update or a force stop. \
             DeskLink\u{2019}s notification on the phone opens the setting."
                .to_string(),
        );
    }
    if notice.button.is_some() {
        parts.push(format!("{denial} {steps}").trim().to_string());
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// What the strip says. Control granted (`lease_active`) says nothing at all, so
/// the strip goes the moment the phone allows it, with no click. Before the
/// phone's picture there is no control to ask for: the phone's person has
/// its prompt to answer (asking for control then was refused every 2 s with
/// "not being shared", read here as "Control is off", forever).
pub(crate) fn status_notice(seen: Seen) -> Option<Notice> {
    let notice = state_notice(seen);
    let shown = seen.has_picture
        && matches!(
            seen.state,
            Some(
                SessionState::Viewing | SessionState::RequestingControl | SessionState::Controlling
            )
        );
    if !shown {
        return notice;
    }
    // Keys never vanish: with control, the phone's word on why they reached
    // no text field; without it, that typing needs control first.
    if seen.lease_active {
        return seen
            .typing
            .map(|code| Notice {
                stopped: false,
                text: typing_words(code, seen.phone),
                button: None,
            })
            .or(notice);
    }
    match notice {
        Some(mut notice) if seen.typed_without_control => {
            let why = notice.text.trim_start_matches("View only: ");
            notice.text = format!("Typing needs control \u{b7} {why}");
            Some(notice)
        }
        notice => notice,
    }
}

/// What the strip says about the session itself.
fn state_notice(seen: Seen) -> Option<Notice> {
    let say = |text: String| {
        Some(Notice {
            stopped: false,
            text,
            button: None,
        })
    };
    let phone = seen.phone;
    let waiting = || {
        say(if seen.answered {
            format!("Waiting for {phone}: confirm sharing on the phone")
        } else {
            format!("Waiting for {phone}: tap Allow on the phone")
        })
    };
    match seen.state {
        Some(SessionState::RequestingView) => waiting(),
        Some(
            SessionState::Viewing | SessionState::RequestingControl | SessionState::Controlling,
        ) if !seen.has_picture => waiting(),
        Some(
            SessionState::Viewing | SessionState::RequestingControl | SessionState::Controlling,
        ) if seen.lease_active => None,
        Some(SessionState::Viewing | SessionState::RequestingControl) if seen.denial.is_some() => {
            Some(control_denied(seen))
        }
        Some(
            SessionState::Viewing | SessionState::RequestingControl | SessionState::Controlling,
        ) => say(format!("View only: asking {phone} for control…")),
        Some(SessionState::PausedLocked) => {
            say(format!("{phone} is locked · unlock it to continue"))
        }
        Some(SessionState::Stopped | SessionState::Idle) | None if seen.gave_up => {
            if seen.timed_out && seen.answered {
                say(format!("{phone} did not start sharing"))
            } else if seen.timed_out {
                say(format!("{phone} did not answer"))
            } else if seen
                .ended
                .is_some_and(|why| why == "declined" || why.contains("not granted"))
            {
                say(format!("Not allowed on {phone}"))
            } else {
                say(format!("{phone} stopped sharing its screen"))
            }
        }
        Some(SessionState::Stopped) => say("Screen stopped · reconnecting…".to_string()),
        _ => None,
    }
}
