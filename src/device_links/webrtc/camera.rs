//! The phone as this computer's webcam (contract C4), desk side.
//!
//! The desk asks with `camera-request {facing, width, height, fps, audio}`;
//! the phone answers `camera-ready {facing, width, height, audio}` and feeds
//! its camera into the video track the screen share already negotiated, so
//! nothing is renegotiated. That track's frames then go to a PipeWire
//! `Video/Source` node, "DeskLink Camera", that every app can pick (and an
//! existing v4l2loopback device). `camera-control {flip|stop}` and
//! `camera-stopped {reason}` end or change it. Messages carry their own
//! sequence, outside the screen session. With `audio: true` the phone's
//! microphone comes as `audio-ready {phone-to-desk, mic}` under the camera's
//! id and is published as "DeskLink Microphone" (`audio_session`).

use std::collections::HashMap;
use std::sync::Mutex;

use super::remote_session::{ControlKind, ControlMessage};

/// What the desk asks for when nothing is chosen.
pub(crate) const DEFAULT_SIZE: (i32, i32) = (1280, 720);

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CameraState {
    /// Asked; the phone shows its consent prompt.
    Requested,
    Live {
        width: i32,
        height: i32,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CameraSession {
    pub(crate) id: String,
    pub(crate) facing: String,
    pub(crate) state: CameraState,
    sequence: i64,
}

static SESSIONS: Mutex<Option<HashMap<String, CameraSession>>> = Mutex::new(None);

fn with<R>(run: impl FnOnce(&mut HashMap<String, CameraSession>) -> R) -> Option<R> {
    SESSIONS
        .lock()
        .ok()
        .map(|mut sessions| run(sessions.get_or_insert_with(HashMap::new)))
}

/// The device's camera session, for the window and the tray.
pub(crate) fn status(device_id: &str) -> Option<CameraSession> {
    with(|sessions| sessions.get(device_id).cloned()).flatten()
}

/// A request's size, kept within what the phone serves (long side 1920,
/// short side 1080, shape kept) and fps within 1..=30.
pub(crate) fn clamp_request(width: i32, height: i32, fps: i32) -> (i32, i32, i32) {
    let (long, short) = (width.max(height).max(1), width.min(height).max(1));
    let scale = (1920.0 / f64::from(long))
        .min(1080.0 / f64::from(short))
        .min(1.0);
    let fit = |side: i32| ((f64::from(side) * scale).round() as i32).max(2) & !1;
    (fit(width.max(1)), fit(height.max(1)), fps.clamp(1, 30))
}

/// Starts a new session: the message to send. A session already asked or
/// live is replaced (the phone treats the same desk re-asking that way).
pub(crate) fn begin(
    device_id: &str,
    facing: &str,
    width: i32,
    height: i32,
    fps: i32,
    audio: bool,
    build: impl FnOnce(ControlKind, &str, i64) -> ControlMessage,
) -> ControlMessage {
    let facing = if facing == "front" { "front" } else { "back" };
    let (width, height, fps) = clamp_request(width, height, fps);
    let id = uuid::Uuid::new_v4().to_string();
    let mut message = build(ControlKind::CameraRequest, &id, 1);
    message.facing = Some(facing.to_string());
    message.width = Some(width);
    message.height = Some(height);
    message.fps = Some(i64::from(fps));
    message.audio = Some(audio);
    with(|sessions| {
        sessions.insert(
            device_id.to_string(),
            CameraSession {
                id,
                facing: facing.to_string(),
                state: CameraState::Requested,
                sequence: 1,
            },
        )
    });
    message
}

/// A `camera-control` for the running session (`flip` or `stop`); `stop`
/// also ends it here.
pub(crate) fn control(
    device_id: &str,
    action: &str,
    build: impl FnOnce(ControlKind, &str, i64) -> ControlMessage,
) -> Option<ControlMessage> {
    let (id, sequence) = with(|sessions| {
        let session = sessions.get_mut(device_id)?;
        session.sequence += 1;
        Some((session.id.clone(), session.sequence))
    })
    .flatten()?;
    let mut message = build(ControlKind::CameraControl, &id, sequence);
    message.action = Some(action.to_string());
    if action == "stop" {
        end(device_id);
    }
    Some(message)
}

/// What a phone's camera message did, for the window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// It is streaming now: attach the outputs.
    Live,
    /// It ended there, and why (in the user's words).
    Stopped(String),
    /// Not for the current session, or nothing to do.
    Ignored,
}

/// The reason a phone gave, in the user's words.
pub(crate) fn reason_words(phone: &str, reason: Option<&str>) -> String {
    match reason {
        Some("busy") => {
            format!("{phone} is sharing its screen, or another computer uses its camera")
        }
        Some("denied") => format!("{phone} did not allow its camera"),
        Some("permission") => format!("DeskLink has no camera permission on {phone}"),
        Some("stopped") => format!("The camera was stopped on {phone}"),
        _ => format!("{phone}'s camera stopped"),
    }
}

/// A `camera-*` message from the phone.
pub(crate) fn handle(device_id: &str, phone: &str, message: &ControlMessage) -> Outcome {
    let current = with(|sessions| {
        sessions
            .get(device_id)
            .filter(|session| session.id == message.remote_session_id)
            .cloned()
    })
    .flatten();
    let Some(session) = current else {
        return Outcome::Ignored;
    };
    match message.kind {
        ControlKind::CameraReady => {
            let (width, height) = (
                message.width.unwrap_or(DEFAULT_SIZE.0),
                message.height.unwrap_or(DEFAULT_SIZE.1),
            );
            with(|sessions| {
                if let Some(live) = sessions.get_mut(device_id) {
                    live.state = CameraState::Live { width, height };
                    if let Some(facing) = &message.facing {
                        live.facing = facing.clone();
                    }
                }
            });
            if matches!(session.state, CameraState::Live { .. }) {
                Outcome::Ignored
            } else {
                Outcome::Live
            }
        }
        ControlKind::CameraStopped => {
            end(device_id);
            Outcome::Stopped(reason_words(phone, message.reason.as_deref()))
        }
        ControlKind::CameraControl if message.action.as_deref() == Some("stop") => {
            end(device_id);
            Outcome::Stopped(reason_words(phone, Some("stopped")))
        }
        _ => Outcome::Ignored,
    }
}

/// Ends the session here (and the outputs).
pub(crate) fn end(device_id: &str) {
    let ended = with(|sessions| sessions.remove(device_id)).flatten();
    if ended.is_some() {
        super::screen::detach_camera(device_id);
    }
}

/// Whether the phone's video should feed the camera outputs.
pub(crate) fn is_live(device_id: &str) -> bool {
    status(device_id).is_some_and(|session| matches!(session.state, CameraState::Live { .. }))
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

    fn reply(kind: ControlKind, id: &str) -> ControlMessage {
        build(kind, id, 1)
    }

    #[test]
    fn a_request_goes_live_and_ends_with_the_phones_reason() {
        let request = begin("cam-a", "front", 1920, 1080, 30, true, build);
        assert_eq!(request.kind, ControlKind::CameraRequest);
        assert_eq!(request.facing.as_deref(), Some("front"));
        assert_eq!(
            (request.width, request.height, request.fps),
            (Some(1920), Some(1080), Some(30))
        );
        assert_eq!(request.audio, Some(true));
        let id = request.remote_session_id.clone();
        assert!(uuid::Uuid::parse_str(&id).is_ok());
        assert!(!is_live("cam-a"));

        // Another session's answer changes nothing.
        let stray = reply(
            ControlKind::CameraReady,
            "00000000-0000-0000-0000-000000000000",
        );
        assert_eq!(handle("cam-a", "Pixel", &stray), Outcome::Ignored);

        let mut ready = reply(ControlKind::CameraReady, &id);
        ready.width = Some(1280);
        ready.height = Some(720);
        ready.facing = Some("back".into());
        assert_eq!(handle("cam-a", "Pixel", &ready), Outcome::Live);
        assert!(is_live("cam-a"));
        assert_eq!(status("cam-a").unwrap().facing, "back");
        assert_eq!(
            handle("cam-a", "Pixel", &ready),
            Outcome::Ignored,
            "a repeat changes nothing"
        );

        let mut stopped = reply(ControlKind::CameraStopped, &id);
        stopped.reason = Some("stopped".into());
        assert_eq!(
            handle("cam-a", "Pixel", &stopped),
            Outcome::Stopped("The camera was stopped on Pixel".into())
        );
        assert!(status("cam-a").is_none());
    }

    #[test]
    fn controls_count_their_own_sequence_and_stop_ends_it() {
        let request = begin("cam-b", "sideways", 640, 480, 99, false, build);
        assert_eq!(
            request.facing.as_deref(),
            Some("back"),
            "unknown facing means back"
        );
        assert_eq!(request.fps, Some(30));
        let flip = control("cam-b", "flip", build).unwrap();
        assert_eq!((flip.kind, flip.sequence), (ControlKind::CameraControl, 2));
        assert_eq!(flip.action.as_deref(), Some("flip"));
        let stop = control("cam-b", "stop", build).unwrap();
        assert_eq!(stop.sequence, 3);
        assert!(status("cam-b").is_none());
        assert!(
            control("cam-b", "flip", build).is_none(),
            "nothing to control"
        );
    }

    #[test]
    fn requests_stay_within_what_the_phone_serves() {
        assert_eq!(clamp_request(3840, 2160, 60), (1920, 1080, 30));
        assert_eq!(
            clamp_request(1080, 1920, 30),
            (1080, 1920, 30),
            "portrait kept"
        );
        assert_eq!(clamp_request(1280, 720, 0), (1280, 720, 1));
        assert_eq!(
            reason_words("Pixel", Some("busy")),
            "Pixel is sharing its screen, or another computer uses its camera"
        );
    }
}
