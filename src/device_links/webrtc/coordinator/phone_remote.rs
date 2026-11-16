//! Viewing and controlling the phone from the desk: requests, the input lease
//! and its heartbeat, expired requests, denials, and the audio and capture
//! control messages.
//!
//! Sibling edges: `desktop_share` (one session per device serves both
//! directions). Outside: `remote_session`, `screen`, `capture`,
//! `audio`, `audio_session`, `config`, `daemon` (events, clipboard).

use super::*;

/// Refuses to view the phone while this desktop's own screen is being shared to
/// it. One session per device serves both directions, so starting would
/// overwrite the live share's session id while its capture keeps streaming.
pub(super) fn ensure_phone_view_allowed(device_id: &str) -> Result<(), String> {
    let sharing = desktop_portal_services()
        .lock()
        .map(|services| services.contains_key(device_id))
        .unwrap_or(false);
    if sharing {
        Err("The phone is viewing this desktop. Stop that before viewing the phone".to_string())
    } else {
        Ok(())
    }
}

/// What to do about a request that expired without an answer.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ExpiredRequest {
    /// The phone asked to see this desktop and our consent dialog is still open.
    CancelDesktopShare,
    /// We asked to see the phone's screen and it never answered.
    ReleasePhoneView,
    Nothing,
}

pub(super) fn expired_request_action(
    previous: remote_session::SessionState,
    direction: Option<remote_session::ScreenDirection>,
) -> ExpiredRequest {
    match (previous, direction) {
        (
            remote_session::SessionState::RequestingView,
            Some(remote_session::ScreenDirection::DesktopToPhone),
        ) => ExpiredRequest::CancelDesktopShare,
        (
            remote_session::SessionState::RequestingView,
            Some(remote_session::ScreenDirection::PhoneToDesktop),
        ) => ExpiredRequest::ReleasePhoneView,
        _ => ExpiredRequest::Nothing,
    }
}

/// Ends any remote-session request that has waited too long, so nothing stays
/// "requesting" forever on either side.
pub(super) fn expire_remote_requests(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
) {
    let expired = remote_session::sessions().lock().ok().and_then(|mut open| {
        open.get_mut(&wire.peer_device_id).map(|session| {
            let direction = session.direction();
            let id = session.remote_session_id().map(str::to_string);
            (
                session.expire_pending(std::time::Instant::now()),
                direction,
                id,
            )
        })
    });
    let Some((Some(previous), direction, id)) = expired else {
        return;
    };
    debug_log!("[DeskLink] Remote-session request timed out while {previous:?}");
    match expired_request_action(previous, direction) {
        ExpiredRequest::CancelDesktopShare => {
            stop_desktop_portal_session(&wire.peer_device_id, None);
            crate::device_links::webrtc::screen::clear_stream(&wire.peer_device_id);
        }
        ExpiredRequest::ReleasePhoneView => {
            let (Some(id), Ok(peer)) = (id, sessions.active_webrtc_peer(binding, attempt_id))
            else {
                return;
            };
            let release = remote_session::ControlMessage::new(
                remote_session::ControlKind::Release,
                attempt_id,
                wire,
                &id,
                remote_session::sessions()
                    .lock()
                    .ok()
                    .and_then(|open| open.get(&wire.peer_device_id).map(|s| s.next_sequence()))
                    .unwrap_or(1),
                now_millis(),
            );
            let _ = send_remote_session_message(&peer, wire, &release);
        }
        ExpiredRequest::Nothing => {}
    }
}

/// Begins a desktop-driven phone session: request the phone's screen first,
/// then request an input lease only after its capture reports `screen-ready`.
pub(crate) fn start_phone_remote_control(
    sessions: &Arc<DeviceManager>,
    transport: &FeatureTransportSnapshot,
) -> Result<(), String> {
    if !transport.web_rtc_ready || !sessions.is_current(&transport.binding) {
        return Err("DeskLink remote control needs a ready WebRTC session".to_string());
    }
    let wire = transport
        .web_rtc_wire
        .as_ref()
        .ok_or_else(|| "DeskLink remote control has no authenticated wire binding".to_string())?;
    ensure_phone_view_allowed(&wire.peer_device_id)?;
    let attempt_id = sessions
        .active_webrtc_attempt(&transport.binding)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "DeskLink remote control has no active WebRTC attempt".to_string())?;
    let (session_id, sequence) = {
        let mut open = remote_session::sessions()
            .lock()
            .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
        let session = open.entry(wire.peer_device_id.clone()).or_default();
        let id = session.begin_view(remote_session::ScreenDirection::PhoneToDesktop);
        (id, session.next_sequence())
    };
    let mut message = remote_session::ControlMessage::new(
        remote_session::ControlKind::RequestView,
        &attempt_id,
        wire,
        &session_id,
        sequence,
        now_millis(),
    );
    message.direction = Some(remote_session::ScreenDirection::PhoneToDesktop);
    let peer = transport
        .web_rtc_peer
        .as_ref()
        .ok_or_else(|| "DeskLink remote control has no active WebRTC peer".to_string())?;
    if let Err(error) = send_remote_session_message(peer, wire, &message) {
        if let Ok(mut open) = remote_session::sessions().lock() {
            if let Some(session) = open.get_mut(&wire.peer_device_id) {
                session.stop();
            }
        }
        return Err(error);
    }
    Ok(())
}

/// The phone offered its own screen unasked: adopt its id and answer with an
/// ordinary `request-view` (the phone has its consent already), and open the
/// Phone Screen window; or `view-denied`, reason `busy`, while any session is
/// under way.
pub(super) fn take_up_phone_offer(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    offer: &remote_session::ControlMessage,
    sessions: &Arc<DeviceManager>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    if offer.direction != Some(remote_session::ScreenDirection::PhoneToDesktop) {
        return Err("A screen offer must be the phone's own screen".to_string());
    }
    let busy = desktop_portal_services()
        .lock()
        .is_ok_and(|services| services.contains_key(&wire.peer_device_id));
    let (adopted, sequence) = {
        let mut open = remote_session::sessions()
            .lock()
            .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
        let session = open.entry(wire.peer_device_id.clone()).or_default();
        let adopted = if busy {
            Err("busy".to_string())
        } else {
            session.adopt_offer(&offer.remote_session_id)
        };
        (adopted, session.next_sequence())
    };
    if adopted.as_ref().is_err_and(|reason| reason == "stale") {
        // Not asked for now: no request-view, and no denial either.
        return Ok(());
    }
    let peer = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?;
    let reply = match adopted {
        Ok(()) => {
            let mut request = remote_session::ControlMessage::new(
                remote_session::ControlKind::RequestView,
                attempt_id,
                wire,
                &offer.remote_session_id,
                sequence,
                now_millis(),
            );
            request.direction = Some(remote_session::ScreenDirection::PhoneToDesktop);
            crate::device_links::daemon::push_event(
                events,
                DaemonEvent::PhoneScreenOffered {
                    device_id: wire.peer_device_id.clone(),
                },
            );
            request
        }
        Err(reason) => {
            let mut denied = remote_session::ControlMessage::new(
                remote_session::ControlKind::ViewDenied,
                attempt_id,
                wire,
                &offer.remote_session_id,
                sequence,
                now_millis(),
            );
            denied.reason = Some(reason);
            denied
        }
    };
    send_remote_session_message(&peer, wire, &reply)
}

/// Asks the phone for control again while its screen is being viewed. The
/// phone refuses while its accessibility service is not running, and control
/// used to be asked for only once (at screen-ready): enabling the service
/// afterwards changed nothing and the window sat on "requesting control".
pub(crate) fn request_phone_control(
    sessions: &Arc<DeviceManager>,
    transport: &FeatureTransportSnapshot,
) -> Result<(), String> {
    if !transport.web_rtc_ready || !sessions.is_current(&transport.binding) {
        return Err("DeskLink remote control needs a ready WebRTC session".to_string());
    }
    let wire = transport
        .web_rtc_wire
        .as_ref()
        .ok_or_else(|| "DeskLink remote control has no authenticated wire binding".to_string())?;
    let attempt_id = sessions
        .active_webrtc_attempt(&transport.binding)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "DeskLink remote control has no active WebRTC attempt".to_string())?;
    let message = {
        let mut open = remote_session::sessions()
            .lock()
            .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
        let Some(session) = open.get_mut(&wire.peer_device_id) else {
            return Ok(());
        };
        if session.state() != remote_session::SessionState::Viewing
            || session.direction() != Some(remote_session::ScreenDirection::PhoneToDesktop)
        {
            return Ok(());
        }
        let Some(session_id) = session.remote_session_id().map(str::to_string) else {
            return Ok(());
        };
        session.begin_control()?;
        remote_session::ControlMessage::new(
            remote_session::ControlKind::RequestControl,
            &attempt_id,
            wire,
            &session_id,
            session.next_sequence(),
            now_millis(),
        )
    };
    let peer = transport
        .web_rtc_peer
        .as_ref()
        .ok_or_else(|| "DeskLink remote control has no active WebRTC peer".to_string())?;
    send_remote_session_message(peer, wire, &message)
}

pub(crate) fn stamp_remote_input(
    device_id: &str,
    packet: &mut NetworkPacket,
) -> Result<(), String> {
    let mut open = remote_session::sessions()
        .lock()
        .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
    open.get_mut(device_id)
        .ok_or_else(|| "DeskLink remote-control session has not started".to_string())?
        .stamp_input(packet)
}

pub(crate) fn has_remote_control_lease(device_id: &str) -> bool {
    remote_session::sessions()
        .lock()
        .ok()
        .and_then(|sessions| {
            sessions
                .get(device_id)
                .map(|session| session.heartbeat_lease(now_millis()).is_some())
        })
        .unwrap_or(false)
}

pub(super) fn send_remote_heartbeat(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    let now = now_millis();
    let message = {
        let mut open = remote_session::sessions()
            .lock()
            .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
        let Some(session) = open.get_mut(&wire.peer_device_id) else {
            return Ok(());
        };
        let Some(lease) = session.heartbeat_lease(now).cloned() else {
            return Ok(());
        };
        let session_id = session
            .remote_session_id()
            .ok_or_else(|| "DeskLink remote-control session has no id".to_string())?
            .to_string();
        let mut message = remote_session::ControlMessage::new(
            remote_session::ControlKind::Heartbeat,
            attempt_id,
            wire,
            &session_id,
            session.next_sequence(),
            now,
        );
        message.lease_id = Some(lease.id);
        message.owner_device_id = Some(wire.sender_device_id.clone());
        message
    };
    let peer = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?;
    send_remote_session_message(&peer, wire, &message)?;
    if let Ok(mut open) = remote_session::sessions().lock() {
        if let Some(session) = open.get_mut(&wire.peer_device_id) {
            session.accept(&message, now)?;
        }
    }
    Ok(())
}

pub(crate) fn stop_phone_remote_control(
    sessions: &Arc<DeviceManager>,
    transport: &FeatureTransportSnapshot,
) -> Result<(), String> {
    let Some(wire) = transport.web_rtc_wire.as_ref() else {
        return Ok(());
    };
    let Some(attempt_id) = sessions
        .active_webrtc_attempt(&transport.binding)
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    let message = {
        let mut open = remote_session::sessions()
            .lock()
            .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
        let Some(session) = open.get_mut(&wire.peer_device_id) else {
            return Ok(());
        };
        // This ends the desktop's view of the *phone*. When the session is the
        // phone viewing this desktop, releasing it here would end that share's
        // session state while its portal capture kept streaming.
        if session.direction() == Some(remote_session::ScreenDirection::DesktopToPhone) {
            return Ok(());
        }
        let Some(session_id) = session.remote_session_id().map(str::to_string) else {
            session.stop();
            return Ok(());
        };
        remote_session::ControlMessage::new(
            remote_session::ControlKind::Release,
            &attempt_id,
            wire,
            &session_id,
            session.next_sequence(),
            now_millis(),
        )
    };
    let send_result = if transport.web_rtc_ready {
        if let Some(peer) = &transport.web_rtc_peer {
            send_remote_session_message(peer, wire, &message)
        } else {
            Ok(())
        }
    } else {
        Ok(())
    };
    if let Ok(mut open) = remote_session::sessions().lock() {
        if let Some(session) = open.get_mut(&wire.peer_device_id) {
            session.stop();
        }
    }
    crate::device_links::webrtc::screen::clear_stream(&wire.peer_device_id);
    send_result
}

/// Refuses a `request-view` and ends this side's session for it, so a denied
/// request does not linger as `RequestingView` until it times out.
pub(super) fn deny_view(
    peer: &DesktopWebRtcPeer,
    wire: &WebRtcWireBinding,
    attempt_id: &str,
    remote_session_id: &str,
    sequence: i64,
    reason: String,
) -> Result<(), String> {
    let mut denied = remote_session::ControlMessage::new(
        remote_session::ControlKind::ViewDenied,
        attempt_id,
        wire,
        remote_session_id,
        sequence,
        now_millis(),
    );
    denied.reason = Some(reason);
    apply_own_denial(&wire.peer_device_id, &denied);
    send_remote_session_message(peer, wire, &denied)
}

/// Applies a denial this side sent to its own copy of the session.
pub(super) fn apply_own_denial(device_id: &str, denial: &remote_session::ControlMessage) {
    if let Ok(mut open) = remote_session::sessions().lock() {
        if let Some(session) = open.get_mut(device_id) {
            let _ = session.accept(denial, now_millis());
        }
    }
}

/// Sends the control message `make` builds (from a kind, session id and
/// sequence) to the phone over its ready link.
fn send_control_message(
    sessions: &Arc<DeviceManager>,
    transport: &FeatureTransportSnapshot,
    make: impl FnOnce(
        &dyn Fn(remote_session::ControlKind, &str, i64) -> remote_session::ControlMessage,
    ) -> Option<remote_session::ControlMessage>,
) -> Result<(), String> {
    if !transport.web_rtc_ready || !sessions.is_current(&transport.binding) {
        return Err("The phone is not connected".to_string());
    }
    let wire = transport
        .web_rtc_wire
        .as_ref()
        .ok_or_else(|| "The phone is not connected".to_string())?;
    let attempt_id = sessions
        .active_webrtc_attempt(&transport.binding)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "The phone is not connected".to_string())?;
    let build = |kind, id: &str, sequence| {
        remote_session::ControlMessage::new(kind, &attempt_id, wire, id, sequence, now_millis())
    };
    let Some(message) = make(&build) else {
        return Ok(());
    };
    let peer = transport
        .web_rtc_peer
        .as_ref()
        .ok_or_else(|| "The phone is not connected".to_string())?;
    send_remote_session_message(peer, wire, &message)
}

/// A phone's `audio-*` message (F1 A2): start or stop sending this
/// computer's sound, or play the phone's.
pub(super) fn handle_audio_message(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    message: &remote_session::ControlMessage,
    sessions: &Arc<DeviceManager>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    use crate::device_links::webrtc::audio_session::{self, Step};
    let device_id = &wire.peer_device_id;
    let phone = &binding.link.info.name;
    let peer = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?;
    let build = |kind, id: &str, sequence| {
        remote_session::ControlMessage::new(kind, attempt_id, wire, id, sequence, now_millis())
    };
    let mic_allowed = crate::device_links::config::app_settings().desk_mic_to_phone;
    match audio_session::handle(device_id, message, mic_allowed, &build) {
        Step::Send { id, source, .. } => match peer.start_audio_send(source) {
            Ok(()) => {
                if let Some(ready) = audio_session::sending_started(device_id, &id, &build) {
                    send_remote_session_message(&peer, wire, &ready)?;
                }
            }
            Err(error) => {
                eprintln!("[DeskLink] Could not send sound to {phone}: {error}");
                peer.stop_audio_send();
                let denied = audio_session::sending_failed(device_id, &id, &build);
                send_remote_session_message(&peer, wire, &denied)?;
            }
        },
        Step::Deny(denied) => send_remote_session_message(&peer, wire, &denied)?,
        Step::Hear(source) => {
            let output = if source == crate::device_links::webrtc::audio::Source::Mic {
                crate::device_links::webrtc::audio::Output::Microphone
            } else {
                crate::device_links::webrtc::audio::Output::Speakers
            };
            peer.start_audio_playback(output, phone)?;
        }
        Step::StopSending => peer.stop_audio_send(),
        Step::StopHearing(reason) => {
            peer.stop_audio_playback();
            if let Some(reason) = reason {
                crate::device_links::daemon::push_event(
                    events,
                    DaemonEvent::SoundNotice {
                        device_id: device_id.clone(),
                        message: audio_session::reason_words(phone, &reason),
                    },
                );
            }
        }
        Step::Ignored => {}
    }
    Ok(())
}

/// A phone's `capture-*` message (F3): text goes on the clipboard (a Wi-Fi
/// QR code also offers to join), a file is awaited, an end is said.
pub(super) fn handle_capture_message(
    device_id: &str,
    phone: &str,
    message: &remote_session::ControlMessage,
) {
    use crate::device_links::daemon::handle::DaemonEvent;
    use crate::device_links::webrtc::capture::{self, Outcome};
    match capture::handle(device_id, phone, message) {
        Outcome::Text { kind, text } => {
            crate::device_links::daemon::clipboard::set_clipboard_text_from_remote(&text);
            let wifi = (kind == "qr").then(|| capture::wifi(&text)).flatten();
            let event = match wifi {
                Some(network) => DaemonEvent::WifiFromPhone {
                    device_id: device_id.to_string(),
                    network,
                },
                None => DaemonEvent::CaptureNotice {
                    device_id: device_id.to_string(),
                    message: if kind == "qr" {
                        format!("QR code from {phone} copied")
                    } else {
                        format!("Text from {phone} copied — paste it anywhere")
                    },
                },
            };
            capture::notify(event);
        }
        Outcome::Ended(Some(words)) => capture::notify(DaemonEvent::CaptureNotice {
            device_id: device_id.to_string(),
            message: words,
        }),
        Outcome::AwaitFile | Outcome::Ended(None) | Outcome::Ignored => {}
    }
}

/// Asks the phone for a photo, scan, text or QR code (F3), or (`kind`
/// "cancel") cancels the open one.
pub(crate) fn capture_request(
    sessions: &Arc<DeviceManager>,
    transport: &FeatureTransportSnapshot,
    kind: &str,
    target: &str,
) -> Result<(), String> {
    let device_id = transport.binding.device_id.clone();
    send_control_message(sessions, transport, |build| {
        if kind == "cancel" {
            crate::device_links::webrtc::capture::cancel(&device_id, build)
        } else if crate::device_links::webrtc::capture::KINDS.contains(&kind) {
            Some(crate::device_links::webrtc::capture::begin(
                &device_id, kind, target, build,
            ))
        } else {
            None
        }
    })
}

/// What the desk's sound switches do (F1): `desk-sound` sends this
/// computer's sound to the phone, `phone-sound` / `phone-mic` play the
/// phone's sound here or publish its microphone as "DeskLink Microphone".
pub(crate) fn audio_switch(
    sessions: &Arc<DeviceManager>,
    transport: &FeatureTransportSnapshot,
    what: &str,
    on: bool,
) -> Result<(), String> {
    use crate::device_links::webrtc::audio::Source;
    use crate::device_links::webrtc::audio_session;
    let not_connected = || "The phone is not connected".to_string();
    if !transport.web_rtc_ready || !sessions.is_current(&transport.binding) {
        return Err(not_connected());
    }
    let wire = transport.web_rtc_wire.as_ref().ok_or_else(not_connected)?;
    let peer = transport.web_rtc_peer.as_ref().ok_or_else(not_connected)?;
    let attempt_id = sessions
        .active_webrtc_attempt(&transport.binding)
        .map_err(|error| error.to_string())?
        .ok_or_else(not_connected)?;
    let device_id = &transport.binding.device_id;
    let build = |kind, id: &str, sequence| {
        remote_session::ControlMessage::new(kind, &attempt_id, wire, id, sequence, now_millis())
    };
    let sending = what == "desk-sound";
    // Sound both ways at once would loop through each side's capture: the
    // newer switch wins, and the other one stops.
    let loops = if sending {
        audio_session::hearing_system(device_id)
    } else {
        what == "phone-sound" && audio_session::sending_system(device_id)
    };
    if on && loops {
        if sending {
            peer.stop_audio_playback();
        } else {
            peer.stop_audio_send();
        }
        if let Some(stop) = audio_session::stop(device_id, !sending, &build) {
            send_remote_session_message(peer, wire, &stop)?;
        }
    }
    if !on {
        if sending {
            peer.stop_audio_send();
        } else {
            peer.stop_audio_playback();
        }
        return match audio_session::stop(device_id, sending, &build) {
            Some(stop) => send_remote_session_message(peer, wire, &stop),
            None => Ok(()),
        };
    }
    if sending {
        // The phone asked for it already: that session goes on.
        if audio_session::sending_system(device_id) {
            return Ok(());
        }
        let (id, replaced) = audio_session::offer_desk_sound(device_id, &build);
        if let Some(stop) = replaced {
            send_remote_session_message(peer, wire, &stop)?;
        }
        if let Err(error) = peer.start_audio_send(Source::System) {
            audio_session::stop(device_id, true, &build);
            return Err(error);
        }
        return match audio_session::sending_started(device_id, &id, &build) {
            Some(ready) => send_remote_session_message(peer, wire, &ready),
            None => Ok(()),
        };
    }
    if !peer.audio_negotiated() {
        return Err("This phone's DeskLink cannot send sound yet (update it)".to_string());
    }
    let source = if what == "phone-mic" {
        Source::Mic
    } else {
        Source::System
    };
    // A replaced session's sound stops here; the new one plays on `audio-ready`.
    peer.stop_audio_playback();
    for message in audio_session::ask_phone(device_id, source, &build) {
        send_remote_session_message(peer, wire, &message)?;
    }
    Ok(())
}

pub(super) fn send_remote_session_message(
    peer: &DesktopWebRtcPeer,
    wire: &WebRtcWireBinding,
    message: &remote_session::ControlMessage,
) -> Result<(), String> {
    let payload = message.to_json();
    let envelope = WebRtcEnvelope::create(
        wire,
        WebRtcChannel::Control,
        remote_session::MESSAGE_TYPE,
        &payload,
        now_millis(),
    )?;
    let text = String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink remote-session envelope was not UTF-8".to_string())?;
    peer.send_text(WebRtcChannel::Control, &text)
}

/// Why the phone may not control this screen when the session and password
/// allowed it: the control session lapsed, the desktop can only show its
/// screen (sway, Hyprland, COSMIC: ScreenCast without RemoteDesktop, so
/// there is no switch to turn on), or the picture alone was approved.
pub(super) fn control_denied_reason(can_control: bool, view_only_desktop: bool) -> String {
    if can_control {
        "The approved desktop-control session is not active".to_string()
    } else if view_only_desktop {
        "This computer's desktop lets the phone watch its screen but not control it. Only \
         GNOME and KDE Plasma allow control."
            .to_string()
    } else {
        "The computer shared its screen without control. Reopen the screen and, on the \
         computer, turn on \"Allow remote interaction\" before sharing."
            .to_string()
    }
}
