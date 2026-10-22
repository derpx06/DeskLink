//! Sound both ways (F1 A2), desk side: who sends sound to whom.
//!
//! One session per direction per phone. The requester is the one who hears:
//! the phone asks for the desk's sound with `audio-request {desk-to-phone}`,
//! the desk asks for the phone's with `audio-request {phone-to-desk}`. The
//! sender answers `audio-ready` once sound flows, or `audio-denied`. Either
//! side ends one with `audio-stop`; a new request replaces a live one
//! (`audio-stop {replaced}`). Messages carry their own sequence, outside the screen session.

use std::collections::HashMap;
use std::sync::Mutex;

use super::audio::Source;
use super::remote_session::{ControlKind, ControlMessage};

pub(crate) const DESK_TO_PHONE: &str = "desk-to-phone";
pub(crate) const PHONE_TO_DESK: &str = "phone-to-desk";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Session {
    pub(crate) id: String,
    pub(crate) source: Source,
    /// Sound flows (else asked, waiting).
    pub(crate) live: bool,
}

#[derive(Debug, Default)]
struct Phone {
    /// This computer's sound going to the phone.
    sending: Option<Session>,
    /// The phone's sound coming here.
    hearing: Option<Session>,
    sequence: i64,
}

static SESSIONS: Mutex<Option<HashMap<String, Phone>>> = Mutex::new(None);

fn with<R>(device_id: &str, run: impl FnOnce(&mut Phone) -> R) -> Option<R> {
    SESSIONS.lock().ok().map(|mut sessions| {
        run(sessions
            .get_or_insert_with(HashMap::new)
            .entry(device_id.to_string())
            .or_default())
    })
}

pub(crate) fn source_name(source: Source) -> &'static str {
    match source {
        Source::System => "system",
        Source::Mic => "mic",
    }
}

fn parse_source(source: Option<&str>) -> Source {
    if source == Some("mic") {
        Source::Mic
    } else {
        Source::System
    }
}

/// Both directions, for the window and the tray.
pub(crate) fn status(device_id: &str) -> (Option<Session>, Option<Session>) {
    with(device_id, |phone| {
        (phone.sending.clone(), phone.hearing.clone())
    })
    .unwrap_or_default()
}

/// Builds a message: kind, session id, sequence.
pub(crate) type Build<'a> = &'a dyn Fn(ControlKind, &str, i64) -> ControlMessage;

fn message(phone: &mut Phone, build: Build, kind: ControlKind, id: &str) -> ControlMessage {
    phone.sequence += 1;
    build(kind, id, phone.sequence)
}

fn stop_message(phone: &mut Phone, build: Build, id: &str, reason: &str) -> ControlMessage {
    let mut stop = message(phone, build, ControlKind::AudioStop, id);
    stop.reason = Some(reason.to_string());
    stop
}

/// The desk asks to hear the phone (`system` or `mic`): the messages to
/// send, a live session's replacement first.
pub(crate) fn ask_phone(device_id: &str, source: Source, build: Build) -> Vec<ControlMessage> {
    with(device_id, |phone| {
        let mut out = Vec::new();
        if let Some(old) = phone.hearing.take() {
            out.push(stop_message(phone, build, &old.id, "replaced"));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let mut request = message(phone, build, ControlKind::AudioRequest, &id);
        request.audio_direction = Some(PHONE_TO_DESK.to_string());
        request.source = Some(source_name(source).to_string());
        phone.hearing = Some(Session {
            id,
            source,
            live: false,
        });
        out.push(request);
        out
    })
    .unwrap_or_default()
}

/// The desk offers its own sound to the phone unasked (contract A2: "the
/// desk starts sending its own audio"): a new sending session. Returns its
/// id and a replaced session's stop.
pub(crate) fn offer_desk_sound(device_id: &str, build: Build) -> (String, Option<ControlMessage>) {
    with(device_id, |phone| {
        let replaced = phone
            .sending
            .take()
            .map(|old| stop_message(phone, build, &old.id, "replaced"));
        let id = uuid::Uuid::new_v4().to_string();
        phone.sending = Some(Session {
            id: id.clone(),
            source: Source::System,
            live: false,
        });
        (id, replaced)
    })
    .unwrap_or_default()
}

/// The `audio-ready` for a sending session that just started; `None` when
/// it was ended or replaced meanwhile.
pub(crate) fn sending_started(device_id: &str, id: &str, build: Build) -> Option<ControlMessage> {
    with(device_id, |phone| {
        let session = phone.sending.as_mut().filter(|session| session.id == id)?;
        session.live = true;
        let source = session.source;
        let mut ready = message(phone, build, ControlKind::AudioReady, id);
        ready.audio_direction = Some(DESK_TO_PHONE.to_string());
        ready.source = Some(source_name(source).to_string());
        ready.sample_rate = Some(48_000);
        ready.channels = Some(2);
        Some(ready)
    })
    .flatten()
}

/// A sending session that could not start: the denial to send.
pub(crate) fn sending_failed(device_id: &str, id: &str, build: Build) -> ControlMessage {
    deny(device_id, id, "unsupported", build)
}

fn deny(device_id: &str, id: &str, reason: &str, build: Build) -> ControlMessage {
    with(device_id, |phone| {
        if phone
            .sending
            .as_ref()
            .is_some_and(|session| session.id == id)
        {
            phone.sending = None;
        }
        let mut denied = message(phone, build, ControlKind::AudioDenied, id);
        denied.reason = Some(reason.to_string());
        denied
    })
    .unwrap_or_else(|| build(ControlKind::AudioDenied, id, 1))
}

/// Ends one direction here: the `audio-stop` to send, if one was running.
pub(crate) fn stop(device_id: &str, sending: bool, build: Build) -> Option<ControlMessage> {
    with(device_id, |phone| {
        let ended = if sending {
            phone.sending.take()
        } else {
            phone.hearing.take()
        }?;
        Some(stop_message(phone, build, &ended.id, "stopped"))
    })
    .flatten()
}

/// Whether the phone's sound (not its microphone) is asked for or plays here.
pub(crate) fn hearing_system(device_id: &str) -> bool {
    status(device_id)
        .1
        .is_some_and(|session| session.source == Source::System)
}

/// Whether this computer's sound (not its microphone) goes to the phone.
pub(crate) fn sending_system(device_id: &str) -> bool {
    status(device_id)
        .0
        .is_some_and(|session| session.source == Source::System)
}

/// The peer went: everything ends, silently.
pub(crate) fn end_all(device_id: &str) {
    if let Ok(mut sessions) = SESSIONS.lock() {
        if let Some(sessions) = sessions.as_mut() {
            sessions.remove(device_id);
        }
    }
}

/// What a phone's `audio-*` message asks of this desk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Start sending `source`, then answer with `sending_started(id)`;
    /// `replaced` is the session it replaces (stop that first).
    Send {
        id: String,
        source: Source,
        replaced: Option<String>,
    },
    /// Refuse it with this message.
    Deny(Box<ControlMessage>),
    /// Play what arrives (to the speakers, or as the microphone for `mic`).
    Hear(Source),
    /// Stop sending this computer's sound.
    StopSending,
    /// Stop playing the phone's; `Some(reason)` when the phone refused or
    /// ended it.
    StopHearing(Option<String>),
    Ignored,
}

/// A phone's `audio-*` message. `mic_allowed`: the desk's setting that lets
/// a phone hear this computer's microphone.
pub(crate) fn handle(
    device_id: &str,
    message: &ControlMessage,
    mic_allowed: bool,
    build: Build,
) -> Step {
    let id = message.remote_session_id.as_str();
    match message.kind {
        ControlKind::AudioRequest => {
            let source = parse_source(message.source.as_deref());
            if message.audio_direction.as_deref() != Some(DESK_TO_PHONE) {
                // The requester hears; this desk is never asked to hear.
                return Step::Deny(Box::new(deny(device_id, id, "unsupported", build)));
            }
            if source == Source::Mic && !mic_allowed {
                return Step::Deny(Box::new(deny(device_id, id, "declined", build)));
            }
            // The phone's own sound plays here: sending this computer's back
            // would loop it (the monitor hears what plays).
            if source == Source::System && hearing_system(device_id) {
                return Step::Deny(Box::new(deny(device_id, id, "busy", build)));
            }
            let replaced = with(device_id, |phone| {
                phone
                    .sending
                    .replace(Session {
                        id: id.to_string(),
                        source,
                        live: false,
                    })
                    .map(|old| old.id)
            })
            .flatten();
            Step::Send {
                id: id.to_string(),
                source,
                replaced,
            }
        }
        ControlKind::AudioReady => with(device_id, |phone| {
            if message.audio_direction.as_deref() == Some(DESK_TO_PHONE) {
                return Step::Ignored;
            }
            match phone.hearing.as_mut().filter(|session| session.id == id) {
                Some(session) if session.live => Step::Ignored,
                Some(session) => {
                    session.live = true;
                    Step::Hear(session.source)
                }
                None => Step::Ignored,
            }
        })
        .unwrap_or(Step::Ignored),
        ControlKind::AudioStop | ControlKind::AudioDenied => with(device_id, |phone| {
            if phone
                .sending
                .as_ref()
                .is_some_and(|session| session.id == id)
            {
                phone.sending = None;
                return Step::StopSending;
            }
            if phone
                .hearing
                .as_ref()
                .is_some_and(|session| session.id == id)
            {
                phone.hearing = None;
                let reason = (message.kind == ControlKind::AudioDenied)
                    .then(|| message.reason.clone().unwrap_or_else(|| "declined".into()));
                return Step::StopHearing(reason);
            }
            Step::Ignored
        })
        .unwrap_or(Step::Ignored),
        // The phone's volume is its own; mute is not asked of this desk.
        _ => Step::Ignored,
    }
}

/// A refusal in the user's words.
pub(crate) fn reason_words(phone: &str, reason: &str) -> String {
    match reason {
        "busy" => format!("{phone}'s sound is already in use"),
        "permission" => format!("DeskLink may not record sound on {phone}"),
        "unsupported" => format!("{phone} cannot share its sound (it needs Android 10)"),
        _ => format!("{phone} did not share its sound"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::webrtc::WebRtcWireBinding;

    fn build(kind: ControlKind, id: &str, sequence: i64) -> ControlMessage {
        let wire = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        ControlMessage::new(kind, "attempt", &wire, id, sequence, 1)
    }

    fn from_phone(kind: ControlKind, id: &str, direction: &str, source: &str) -> ControlMessage {
        let mut message = build(kind, id, 1);
        message.audio_direction = Some(direction.to_string());
        message.source = Some(source.to_string());
        message
    }

    #[test]
    fn the_phone_asks_for_the_desks_sound_and_a_new_request_replaces_it() {
        let device = "sound-phone-a";
        let first = uuid::Uuid::new_v4().to_string();
        let request = from_phone(ControlKind::AudioRequest, &first, DESK_TO_PHONE, "system");
        assert_eq!(
            handle(device, &request, false, &build),
            Step::Send {
                id: first.clone(),
                source: Source::System,
                replaced: None
            }
        );
        let ready = sending_started(device, &first, &build).unwrap();
        assert_eq!(ready.kind, ControlKind::AudioReady);
        assert_eq!(ready.audio_direction.as_deref(), Some(DESK_TO_PHONE));
        assert_eq!((ready.sample_rate, ready.channels), (Some(48_000), Some(2)));
        assert!(status(device).0.unwrap().live);

        let second = uuid::Uuid::new_v4().to_string();
        let again = from_phone(ControlKind::AudioRequest, &second, DESK_TO_PHONE, "system");
        assert_eq!(
            handle(device, &again, false, &build),
            Step::Send {
                id: second.clone(),
                source: Source::System,
                replaced: Some(first.clone())
            }
        );
        // A stop for the replaced one changes nothing; the current one ends.
        let late = build(ControlKind::AudioStop, &first, 2);
        assert_eq!(handle(device, &late, false, &build), Step::Ignored);
        let stop = build(ControlKind::AudioStop, &second, 3);
        assert_eq!(handle(device, &stop, false, &build), Step::StopSending);
        assert_eq!(status(device), (None, None));
    }

    #[test]
    fn the_desk_microphone_needs_the_setting_and_the_desk_is_never_asked_to_hear() {
        let device = "sound-phone-b";
        let id = uuid::Uuid::new_v4().to_string();
        let mic = from_phone(ControlKind::AudioRequest, &id, DESK_TO_PHONE, "mic");
        match handle(device, &mic, false, &build) {
            Step::Deny(denied) => assert_eq!(denied.reason.as_deref(), Some("declined")),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            handle(device, &mic, true, &build),
            Step::Send {
                source: Source::Mic,
                ..
            }
        ));
        let backwards = from_phone(ControlKind::AudioRequest, &id, PHONE_TO_DESK, "mic");
        match handle(device, &backwards, true, &build) {
            Step::Deny(denied) => assert_eq!(denied.reason.as_deref(), Some("unsupported")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_desk_hears_the_phone_once_it_is_ready_and_a_denial_says_why() {
        let device = "sound-phone-c";
        let out = ask_phone(device, Source::Mic, &build);
        assert_eq!(out.len(), 1);
        let id = out[0].remote_session_id.clone();
        assert_eq!(out[0].audio_direction.as_deref(), Some(PHONE_TO_DESK));
        assert_eq!(out[0].source.as_deref(), Some("mic"));
        let stranger = from_phone(
            ControlKind::AudioReady,
            &uuid::Uuid::new_v4().to_string(),
            PHONE_TO_DESK,
            "mic",
        );
        assert_eq!(handle(device, &stranger, false, &build), Step::Ignored);
        let ready = from_phone(ControlKind::AudioReady, &id, PHONE_TO_DESK, "mic");
        assert_eq!(
            handle(device, &ready, false, &build),
            Step::Hear(Source::Mic)
        );
        assert_eq!(handle(device, &ready, false, &build), Step::Ignored);

        // Asking again replaces it, and says so.
        let again = ask_phone(device, Source::System, &build);
        assert_eq!(again[0].kind, ControlKind::AudioStop);
        assert_eq!(again[0].reason.as_deref(), Some("replaced"));
        let mut denied = build(ControlKind::AudioDenied, &again[1].remote_session_id, 2);
        denied.reason = Some("busy".into());
        assert_eq!(
            handle(device, &denied, false, &build),
            Step::StopHearing(Some("busy".into()))
        );
        assert!(sequence_rises(&again));

        // While the phone's sound plays here, this computer's is not sent
        // back (it would loop through the monitor).
        let heard = ask_phone(device, Source::System, &build);
        assert_eq!(heard.len(), 1);
        let request = from_phone(
            ControlKind::AudioRequest,
            &uuid::Uuid::new_v4().to_string(),
            DESK_TO_PHONE,
            "system",
        );
        match handle(device, &request, false, &build) {
            Step::Deny(denied) => assert_eq!(denied.reason.as_deref(), Some("busy")),
            other => panic!("{other:?}"),
        }
    }

    fn sequence_rises(messages: &[ControlMessage]) -> bool {
        messages
            .windows(2)
            .all(|pair| pair[0].sequence < pair[1].sequence)
    }
}
