//! Tests of the remote-session control plane: message parsing and limits,
//! the session state machine, leases, heartbeats, expiry and input sequences.

use super::*;

fn wire() -> WebRtcWireBinding {
    WebRtcWireBinding::from_attempt("desktop", "phone", "01234567-89ab-cdef-0123-456789abcdef")
        .unwrap()
}

fn message(kind: ControlKind, session: &str) -> ControlMessage {
    let mut wire = wire();
    // Inbound messages carry the peer's device id in `deviceId`.
    std::mem::swap(&mut wire.sender_device_id, &mut wire.peer_device_id);
    ControlMessage::new(
        kind,
        "01234567-89ab-cdef-0123-456789abcdef",
        &wire,
        session,
        1,
        1_000,
    )
}

/// The webcam is gone: its kinds are not parsed, so a peer that still
/// sends one is refused at the boundary (never applied, never a crash),
/// while the same message with a live kind parses.
#[test]
fn the_removed_webcam_kinds_are_refused() {
    let id = Uuid::new_v4().to_string();
    let live = String::from_utf8(message(ControlKind::AudioStop, &id).to_json()).unwrap();
    assert!(ControlMessage::from_json(live.as_bytes()).is_ok());
    for kind in [
        "camera-request",
        "camera-ready",
        "camera-control",
        "camera-stopped",
    ] {
        assert!(ControlKind::parse(kind).is_none(), "{kind}");
        let json = live.replace("\"audio-stop\"", &format!("\"{kind}\""));
        assert_ne!(json, live);
        assert!(
            ControlMessage::from_json(json.as_bytes()).is_err(),
            "{kind}"
        );
    }
}

/// F1: audio's `direction` is its own (never a screen direction), its
/// fields survive the wire, and what the phone would refuse is refused.
#[test]
fn audio_messages_round_trip_with_their_own_direction() {
    let id = Uuid::new_v4().to_string();
    let attempt = "01234567-89ab-cdef-0123-456789abcdef";
    let mut ready = message(ControlKind::AudioReady, &id);
    ready.audio_direction = Some("desk-to-phone".into());
    ready.source = Some("system".into());
    ready.sample_rate = Some(48_000);
    ready.channels = Some(2);
    let json: Value = serde_json::from_slice(&ready.to_json()).unwrap();
    assert_eq!(
        (
            json["direction"].as_str(),
            json["source"].as_str(),
            json["sampleRate"].as_i64(),
            json["channels"].as_i64()
        ),
        (Some("desk-to-phone"), Some("system"), Some(48_000), Some(2))
    );
    let parsed = ControlMessage::from_json(&ready.to_json()).unwrap();
    assert_eq!(parsed, ready);
    assert!(parsed.kind.is_audio() && parsed.direction.is_none());
    assert!(parsed.validate(&wire(), attempt, 1_000).is_ok());

    let mut request = message(ControlKind::AudioRequest, &id);
    assert!(request.validate(&wire(), attempt, 1_000).is_err());
    // Values a newer peer may send pass validation: the handler answers
    // them as unsupported instead of the message vanishing.
    request.audio_direction = Some("sideways".into());
    assert!(request.validate(&wire(), attempt, 1_000).is_ok());
    request.audio_direction = Some("phone-to-desk".into());
    request.source = Some("radio".into());
    assert!(request.validate(&wire(), attempt, 1_000).is_ok());
    request.source = Some("mic".into());
    assert!(request.validate(&wire(), attempt, 1_000).is_ok());

    let mut control = message(ControlKind::AudioControl, &id);
    assert!(
        control.validate(&wire(), attempt, 1_000).is_err(),
        "no action"
    );
    control.action = Some("balance".into());
    assert!(
        control.validate(&wire(), attempt, 1_000).is_ok(),
        "a newer action"
    );
    control.action = Some("volume".into());
    control.value = Some(101);
    assert!(control.validate(&wire(), attempt, 1_000).is_err());
    control.value = Some(40);
    assert!(control.validate(&wire(), attempt, 1_000).is_ok());
    for kind in ["audio-stop", "audio-denied"] {
        assert!(ControlKind::parse(kind).is_some_and(ControlKind::is_audio));
    }
}

/// The sharing side reports text focus with `editable`; the field survives
/// the wire. A phone's report counts only about its own screen shown here.
#[test]
fn input_focus_reports_round_trip_and_count_only_from_the_sharing_side() {
    let id = Uuid::new_v4().to_string();
    let mut report = message(ControlKind::InputFocus, &id);
    report.editable = Some(true);
    let parsed = ControlMessage::from_json(&report.to_json()).unwrap();
    assert_eq!(parsed.kind, ControlKind::InputFocus);
    assert_eq!(parsed.editable, Some(true));
    assert_eq!(
        ControlKind::parse("input-focus"),
        Some(ControlKind::InputFocus)
    );

    let mut session = RemoteSession::new();
    assert!(session.accept(&parsed, 1_000).is_err(), "no session");
    // This desk shares its own screen: the phone has nothing to report on.
    let mut desk_share = RemoteSession::new();
    let shared = desk_share.begin_view(ScreenDirection::DesktopToPhone);
    assert!(desk_share
        .accept(&message(ControlKind::InputFocus, &shared), 1_000)
        .is_err());
}

/// The phone says why the Phone Screen window's keys reached no text field
/// (F: keys vanished, owner 11-16); typing that works again clears it, and a
/// new share starts with nothing said.
#[test]
fn the_phone_says_why_typing_reached_no_field_and_a_success_clears_it() {
    let mut session = RemoteSession::new();
    let id = session.begin_view(ScreenDirection::PhoneToDesktop);
    let mut refused = message(ControlKind::InputFocus, &id);
    refused.editable = Some(false);
    refused.reason = Some(crate::device_links::packet::TYPING_NO_TEXT_FIELD.to_string());
    session.accept(&refused, 1_000).unwrap();
    assert_eq!(
        session.typing_refusal().map(|(code, _)| code),
        Some("no-text-field")
    );
    let mut typed = message(ControlKind::InputFocus, &id);
    typed.editable = Some(true);
    session.accept(&typed, 1_000).unwrap();
    assert!(session.typing_refusal().is_none());

    session.accept(&refused, 1_000).unwrap();
    let stale = message(ControlKind::InputFocus, &Uuid::new_v4().to_string());
    assert!(
        session.accept(&stale, 1_000).is_err(),
        "another share's report"
    );
    session.begin_view(ScreenDirection::PhoneToDesktop);
    assert!(session.typing_refusal().is_none());
}

#[test]
fn optional_fields_are_omitted_rather_than_emitted_empty() {
    let json = message(ControlKind::Release, &Uuid::new_v4().to_string()).to_json();
    let value: Value = serde_json::from_slice(&json).unwrap();
    let object = value.as_object().unwrap();

    // The peer reads these with `takeIf { isNotEmpty() }`, so an explicit
    // empty string or null is not equivalent to absence.
    for absent in ["leaseId", "direction", "ownerDeviceId", "reason"] {
        assert!(!object.contains_key(absent), "{absent} must be omitted");
    }
    assert_eq!(object["remoteSessionVersion"], Value::from(1));
    assert_eq!(object["kind"], Value::from("release"));
}

#[test]
fn a_control_message_round_trips() {
    let mut original = message(ControlKind::ScreenReady, &Uuid::new_v4().to_string());
    original.direction = Some(ScreenDirection::PhoneToDesktop);
    original.screen_width = Some(1080);
    original.screen_height = Some(2400);
    original.screen_rotation = Some(90);

    let parsed = ControlMessage::from_json(&original.to_json()).unwrap();

    assert_eq!(parsed, original);
}

#[test]
fn unsupported_remote_session_versions_are_rejected() {
    let mut value: Value = serde_json::from_slice(
        &message(ControlKind::Release, &Uuid::new_v4().to_string()).to_json(),
    )
    .unwrap();
    value["remoteSessionVersion"] = Value::from(VERSION + 1);

    let bytes = serde_json::to_vec(&value).unwrap();
    assert!(ControlMessage::from_json(&bytes).is_err());
}

/// A display mode a newer phone may ask for is a well-formed request
/// (the view handler denies it as unsupported); its other fields keep
/// their checks.
#[test]
fn an_unknown_display_mode_is_valid_and_its_sizes_are_still_checked() {
    let mut request = message(ControlKind::RequestView, &Uuid::new_v4().to_string());
    request.direction = Some(ScreenDirection::DesktopToPhone);
    request.display = Some("hologram".into());
    let parsed = ControlMessage::from_json(&request.to_json()).unwrap();
    assert_eq!(parsed.display.as_deref(), Some("hologram"));
    assert!(parsed
        .validate(&wire(), "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_ok());
    request.screen_width = Some(0);
    request.screen_height = Some(10);
    assert!(request
        .validate(&wire(), "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_err());
}

#[test]
fn validation_mirrors_the_peers_rules() {
    let session = Uuid::new_v4().to_string();
    let wire = wire();

    let mut ok = message(ControlKind::ViewGranted, &session);
    ok.direction = Some(ScreenDirection::DesktopToPhone);
    assert!(ok
        .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_ok());

    // Direction is mandatory on this kind.
    let mut missing_direction = ok.clone();
    missing_direction.direction = None;
    assert!(missing_direction
        .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_err());

    // Sequence must be positive and the clock within a minute.
    let mut bad_sequence = ok.clone();
    bad_sequence.sequence = 0;
    assert!(bad_sequence
        .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_err());
    assert!(ok
        .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 200_000)
        .is_err());

    // A grant without an expiry, or already expired, is refused.
    let mut grant = message(ControlKind::ControlGranted, &session);
    grant.lease_id = Some("lease".to_string());
    assert!(grant
        .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_err());
    grant.lease_expires_at = Some(500);
    assert!(grant
        .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_err());
    grant.lease_expires_at = Some(60_000);
    assert!(grant
        .validate(&wire, "01234567-89ab-cdef-0123-456789abcdef", 1_000)
        .is_ok());
}

/// The peer keeps one `lastSequence` per lease and advances it from both
/// control messages and input packets, so both must share this counter.
#[test]
fn a_phone_offer_is_adopted_under_its_id_unless_a_session_is_running() {
    let id = Uuid::new_v4().to_string();
    let mut offer = message(ControlKind::OfferView, &id);
    offer.direction = Some(ScreenDirection::PhoneToDesktop);
    let parsed = ControlMessage::from_json(&offer.to_json()).unwrap();
    assert_eq!(parsed.kind, ControlKind::OfferView);
    assert!(String::from_utf8(offer.to_json())
        .unwrap()
        .contains("\"offer-view\""));

    let mut session = RemoteSession::new();
    session.adopt_offer(&id).unwrap();
    assert_eq!(session.state(), SessionState::RequestingView);
    assert_eq!(session.remote_session_id.as_deref(), Some(id.as_str()));
    // The phone then grants the view it offered, under the same id.
    let mut granted = message(ControlKind::ViewGranted, &id);
    granted.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&granted, 1_000).unwrap();
    assert_eq!(session.state(), SessionState::Viewing);
    assert_eq!(session.adopt_offer("other").unwrap_err(), "busy");
    // Once it ends, a late copy of the same offer asks for nothing.
    session.stop();
    assert_eq!(session.adopt_offer(&id).unwrap_err(), "stale");
    assert!(session.adopt_offer("a-new-offer").is_ok());
}

#[test]
fn one_counter_serves_control_messages_and_input() {
    let mut session = RemoteSession::new();
    let session_id = session.begin_view(ScreenDirection::PhoneToDesktop);
    assert_eq!(session.direction(), Some(ScreenDirection::PhoneToDesktop));
    session
        .accept(&message(ControlKind::ScreenReady, &session_id), 1_000)
        .unwrap();
    session.begin_control().unwrap();
    assert_eq!(session.state(), SessionState::RequestingControl);
    let granted = {
        let mut granted = message(ControlKind::ControlGranted, &session_id);
        granted.lease_id = Some("lease-1".to_string());
        granted.lease_expires_at = Some(90_000);
        granted
    };
    session.accept(&granted, 1_000).unwrap();

    let first = session.next_sequence();
    let mut packet = crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
    session.stamp_input_at(&mut packet, 1_000).unwrap();
    let stamped = packet.get_i64(FIELD_INPUT_SEQUENCE).unwrap();
    let third = session.next_sequence();

    assert_eq!(first, 1);
    assert_eq!(stamped, 2, "input must draw from the same counter");
    assert_eq!(third, 3);
    assert_eq!(packet.get_str(FIELD_LEASE_ID), Some("lease-1"));
}

#[test]
fn input_is_refused_without_an_active_lease() {
    let mut session = RemoteSession::new();
    let mut packet = crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
    assert!(session.stamp_input(&mut packet).is_err());

    session.begin_view(ScreenDirection::DesktopToPhone);
    assert!(session.stamp_input(&mut packet).is_err());
}

#[test]
fn input_is_refused_after_the_control_lease_expires() {
    let mut session = RemoteSession::new();
    let id = session.begin_view(ScreenDirection::PhoneToDesktop);
    session
        .accept(&message(ControlKind::ScreenReady, &id), 1_000)
        .unwrap();
    session.begin_control().unwrap();
    let mut grant = message(ControlKind::ControlGranted, &id);
    grant.lease_id = Some("lease-1".to_string());
    grant.lease_expires_at = Some(2_000);
    session.accept(&grant, 1_000).unwrap();

    let mut packet = crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
    assert!(session.stamp_input_at(&mut packet, 2_000).is_err());
}

#[test]
fn an_unsolicited_control_grant_cannot_activate_input() {
    let mut session = RemoteSession::new();
    let id = session.begin_view(ScreenDirection::PhoneToDesktop);
    session
        .accept(&message(ControlKind::ScreenReady, &id), 1_000)
        .unwrap();
    let mut grant = message(ControlKind::ControlGranted, &id);
    grant.lease_id = Some("lease-1".to_string());
    grant.lease_expires_at = Some(31_000);

    assert!(session.accept(&grant, 1_000).is_err());
    assert_eq!(session.state(), SessionState::Viewing);
}

#[test]
fn a_phone_may_request_control_when_the_desktop_is_sharing_its_screen() {
    let mut session = RemoteSession::new();
    let session_id = Uuid::new_v4().to_string();
    let mut request_view = message(ControlKind::RequestView, &session_id);
    request_view.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request_view, 1_000).unwrap();

    let mut screen_ready = message(ControlKind::ScreenReady, &session_id);
    screen_ready.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&screen_ready, 1_001).unwrap();

    let mut request_control = message(ControlKind::RequestControl, &session_id);
    request_control.direction = Some(ScreenDirection::DesktopToPhone);
    let result = session.accept(&request_control, 1_002);

    assert!(
        result.is_ok(),
        "phone control request should enter host authorization: {result:?}"
    );
    assert_eq!(session.state(), SessionState::AuthorizingPeerControl);
}

/// The phone re-sends request-control every few seconds until it sees
/// the grant; a repeat after the grant is the same request, not an error.
#[test]
fn a_repeated_control_request_after_the_grant_is_granted_again() {
    let mut session = RemoteSession::new();
    let session_id = Uuid::new_v4().to_string();
    let mut request_view = message(ControlKind::RequestView, &session_id);
    request_view.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request_view, 1_000).unwrap();
    let mut screen_ready = message(ControlKind::ScreenReady, &session_id);
    screen_ready.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&screen_ready, 1_001).unwrap();
    let mut request_control = message(ControlKind::RequestControl, &session_id);
    request_control.direction = Some(ScreenDirection::DesktopToPhone);

    session.accept(&request_control, 1_002).unwrap();
    session
        .grant_peer_control("lease-1".into(), "phone".into(), 60_000)
        .unwrap();
    assert_eq!(session.state(), SessionState::PeerControlling);

    session.accept(&request_control, 4_002).unwrap();
    session
        .grant_peer_control("lease-2".into(), "phone".into(), 64_000)
        .unwrap();
    assert_eq!(session.state(), SessionState::PeerControlling);
    assert_eq!(
        session
            .peer_input_lease
            .as_ref()
            .map(|lease| lease.id.as_str()),
        Some("lease-2"),
        "the new grant replaces the lease"
    );
    // Another session's id is still refused.
    let other = message(ControlKind::RequestControl, &Uuid::new_v4().to_string());
    assert!(session.accept(&other, 5_000).is_err());
}

#[test]
fn a_phone_screen_stopped_while_it_was_shown_again_is_not_wanted() {
    let mut session = RemoteSession::new();
    let id = session.begin_view(ScreenDirection::PhoneToDesktop);
    assert!(session.shows_phone_screen(&id));
    assert!(!session.shows_phone_screen(&Uuid::new_v4().to_string()));
    session.stop();
    assert!(!session.shows_phone_screen(&id));
    let desk = session.begin_view(ScreenDirection::DesktopToPhone);
    assert!(!session.shows_phone_screen(&desk));
}

#[test]
fn a_share_ending_late_does_not_stop_the_one_that_replaced_it() {
    let mut session = RemoteSession::new();
    let old = session.begin_view(ScreenDirection::DesktopToPhone);
    let new = session.begin_view(ScreenDirection::DesktopToPhone);
    assert!(!session.stop_if_current(&old));
    assert_eq!(session.state(), SessionState::RequestingView);
    assert_eq!(session.remote_session_id(), Some(new.as_str()));
    assert!(session.stop_if_current(&new));
    assert_eq!(session.state(), SessionState::Stopped);
}

#[test]
fn phone_control_is_not_authorized_during_a_phone_screen_session() {
    let mut session = RemoteSession::new();
    let session_id = Uuid::new_v4().to_string();
    let mut request_view = message(ControlKind::RequestView, &session_id);
    request_view.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&request_view, 1_000).unwrap();

    let mut screen_ready = message(ControlKind::ScreenReady, &session_id);
    screen_ready.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&screen_ready, 1_001).unwrap();

    let mut request_control = message(ControlKind::RequestControl, &session_id);
    request_control.direction = Some(ScreenDirection::PhoneToDesktop);

    assert!(session.accept(&request_control, 1_002).is_err());
    assert_eq!(session.state(), SessionState::Viewing);
}

#[test]
fn phone_input_requires_an_explicit_peer_owned_lease_and_strict_sequence() {
    let mut session = RemoteSession::new();
    let id = Uuid::new_v4().to_string();
    let mut request_view = message(ControlKind::RequestView, &id);
    request_view.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request_view, 1_000).unwrap();
    let mut ready = message(ControlKind::ScreenReady, &id);
    ready.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&ready, 1_001).unwrap();
    let mut request = message(ControlKind::RequestControl, &id);
    request.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request, 1_002).unwrap();

    let mut packet = crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
    packet.set(FIELD_REMOTE_SESSION_ID, id.clone());
    packet.set(FIELD_LEASE_ID, "lease-from-host");
    packet.set(FIELD_INPUT_SEQUENCE, 1i64);
    assert!(session
        .verify_peer_input(&packet, "phone-device", 1_003)
        .is_err());

    session
        .grant_peer_control(
            "lease-from-host".to_string(),
            "phone-device".to_string(),
            31_003,
        )
        .unwrap();
    assert_eq!(session.state(), SessionState::PeerControlling);
    assert!(session
        .verify_peer_input(&packet, "phone-device", 1_003)
        .is_ok());
    assert!(session
        .verify_peer_input(&packet, "phone-device", 1_004)
        .is_err());
    assert!(session
        .verify_peer_input(&packet, "other-device", 1_004)
        .is_err());

    packet.set(FIELD_INPUT_SEQUENCE, 2i64);
    assert!(session
        .verify_peer_input(&packet, "phone-device", 31_003)
        .is_err());
}

#[test]
fn unique_input_reordered_across_webrtc_channels_is_accepted_once() {
    let mut session = RemoteSession::new();
    let id = Uuid::new_v4().to_string();
    let mut request_view = message(ControlKind::RequestView, &id);
    request_view.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request_view, 1_000).unwrap();
    let mut ready = message(ControlKind::ScreenReady, &id);
    ready.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&ready, 1_001).unwrap();
    let mut request = message(ControlKind::RequestControl, &id);
    request.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request, 1_002).unwrap();
    session
        .grant_peer_control("lease".to_string(), "phone-device".to_string(), 31_002)
        .unwrap();

    let mut packet = crate::device_links::packet::NetworkPacket::new("desklink.mousepad.request");
    packet.set(FIELD_REMOTE_SESSION_ID, id);
    packet.set(FIELD_LEASE_ID, "lease");
    packet.set(FIELD_INPUT_SEQUENCE, 4i64);
    assert!(session
        .verify_peer_input(&packet, "phone-device", 1_003)
        .is_ok());

    packet.set(FIELD_INPUT_SEQUENCE, 3i64);
    assert!(session
        .verify_peer_input(&packet, "phone-device", 1_004)
        .is_ok());

    packet.set(FIELD_INPUT_SEQUENCE, 4i64);
    assert!(session
        .verify_peer_input(&packet, "phone-device", 1_005)
        .is_err());
}

#[test]
fn expired_peer_input_lease_cannot_be_revived_by_a_late_heartbeat() {
    let mut session = RemoteSession::new();
    let id = Uuid::new_v4().to_string();
    let mut request_view = message(ControlKind::RequestView, &id);
    request_view.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request_view, 1_000).unwrap();
    let mut ready = message(ControlKind::ScreenReady, &id);
    ready.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&ready, 1_001).unwrap();
    let mut request = message(ControlKind::RequestControl, &id);
    request.direction = Some(ScreenDirection::DesktopToPhone);
    session.accept(&request, 1_002).unwrap();
    session
        .grant_peer_control("lease".to_string(), "phone-device".to_string(), 2_000)
        .unwrap();

    let mut heartbeat = message(ControlKind::Heartbeat, &id);
    heartbeat.lease_id = Some("lease".to_string());
    heartbeat.owner_device_id = Some("phone-device".to_string());
    heartbeat.sequence = 2;
    assert!(session.accept(&heartbeat, 2_000).is_err());
    assert_eq!(session.state(), SessionState::Viewing);
    assert!(session.peer_input_lease().is_none());
}

/// The peer does not clear its own session id when it denies, so neither
/// may this side — otherwise every later message is rejected as stale.
#[test]
fn a_denial_keeps_the_session_id() {
    let mut session = RemoteSession::new();
    let id = session.begin_view(ScreenDirection::PhoneToDesktop);
    session
        .accept(&message(ControlKind::ControlDenied, &id), 1_000)
        .unwrap();

    assert_eq!(session.remote_session_id(), Some(id.as_str()));
    assert_eq!(session.state(), SessionState::Viewing);
}

#[test]
fn a_release_ends_the_session() {
    let mut session = RemoteSession::new();
    let id = session.begin_view(ScreenDirection::PhoneToDesktop);
    session
        .accept(&message(ControlKind::Release, &id), 1_000)
        .unwrap();

    assert_eq!(session.state(), SessionState::Stopped);
    assert_eq!(session.remote_session_id(), None);
}

#[test]
fn only_the_lease_owner_heartbeats() {
    let mut session = RemoteSession::new();
    assert!(session.heartbeat_lease(1_000).is_none());

    let id = session.begin_view(ScreenDirection::DesktopToPhone);
    session
        .accept(&message(ControlKind::ScreenReady, &id), 1_000)
        .unwrap();
    session.begin_control().unwrap();
    let mut granted = message(ControlKind::ControlGranted, &id);
    granted.lease_id = Some("lease-1".to_string());
    granted.lease_expires_at = Some(31_000);
    session.accept(&granted, 1_000).unwrap();

    assert!(session.heartbeat_lease(1_000).is_some());
    // An expired lease is not worth heartbeating; it needs re-granting.
    assert!(session.heartbeat_lease(40_000).is_none());
}

#[test]
fn a_stale_session_id_is_rejected() {
    let mut session = RemoteSession::new();
    session.begin_view(ScreenDirection::PhoneToDesktop);

    let error = session
        .accept(
            &message(ControlKind::ViewGranted, &Uuid::new_v4().to_string()),
            1_000,
        )
        .unwrap_err();

    assert!(error.contains("stale"));
}

/// A denied view is not a view. Mapping it to `Viewing` left the desktop's
/// dialog reading "Screen connected · requesting control…" forever.
#[test]
fn a_view_denial_or_screen_error_ends_the_session() {
    for kind in [ControlKind::ViewDenied, ControlKind::ScreenError] {
        let mut session = RemoteSession::new();
        let id = session.begin_view(ScreenDirection::PhoneToDesktop);

        session.accept(&message(kind, &id), 1_000).unwrap();

        assert_eq!(session.state(), SessionState::Stopped, "{kind:?}");
        assert_eq!(session.remote_session_id(), None, "{kind:?}");
        // The window says why ("Not now" on the phone's prompt), until the next ask.
        let mut said = message(kind, &id);
        let id = session.begin_view(ScreenDirection::PhoneToDesktop);
        said.remote_session_id = id.clone();
        said.reason = Some("declined".to_string());
        assert_eq!(session.ended_reason(), None);
        session.accept(&said, 2_000).unwrap();
        assert_eq!(session.ended_reason(), Some("declined"), "{kind:?}");
        session.begin_view(ScreenDirection::PhoneToDesktop);
        assert_eq!(session.ended_reason(), None, "{kind:?}");
    }
}

/// A delayed release, denial or error for a session this side has already
/// left must not stop the one it is in — and so must not tear down that
/// session's portal capture either.
#[test]
fn a_late_terminal_message_for_another_session_changes_nothing() {
    for kind in [
        ControlKind::Release,
        ControlKind::ScreenStopped,
        ControlKind::ScreenError,
        ControlKind::ViewDenied,
        ControlKind::ControlDenied,
    ] {
        let mut session = RemoteSession::new();
        let current = session.begin_view(ScreenDirection::DesktopToPhone);

        let error = session
            .accept(&message(kind, &Uuid::new_v4().to_string()), 1_000)
            .unwrap_err();

        assert!(error.contains("stale"), "{kind:?}: {error}");
        assert_eq!(session.remote_session_id(), Some(current.as_str()));
        assert_eq!(session.state(), SessionState::RequestingView, "{kind:?}");
    }
}

/// With nothing active there is nothing for a stale message to disturb, so a
/// duplicate release stays a harmless no-op.
#[test]
fn a_release_with_no_session_is_a_harmless_no_op() {
    let mut session = RemoteSession::new();
    session
        .accept(
            &message(ControlKind::Release, &Uuid::new_v4().to_string()),
            1_000,
        )
        .unwrap();
    assert_eq!(session.state(), SessionState::Stopped);
}

/// Nothing may stay in a "requesting" state forever: the other side can be
/// silent (a phone whose consent screen nobody answers).
#[test]
fn an_unanswered_request_expires_and_a_live_view_never_does() {
    let mut session = RemoteSession::new();
    let start = std::time::Instant::now();
    session.begin_view(ScreenDirection::PhoneToDesktop);
    assert_eq!(
        session.expire_pending(start + VIEW_REQUEST_TIMEOUT - std::time::Duration::from_secs(1)),
        None,
        "still within its allowance"
    );
    assert_eq!(
        session.expire_pending(start + VIEW_REQUEST_TIMEOUT + std::time::Duration::from_secs(1)),
        Some(SessionState::RequestingView)
    );
    assert_eq!(session.state(), SessionState::Stopped);

    let mut viewing = RemoteSession::new();
    let id = viewing.begin_view(ScreenDirection::PhoneToDesktop);
    viewing
        .accept(&message(ControlKind::ScreenReady, &id), 1_000)
        .unwrap();
    assert_eq!(
        viewing.expire_pending(std::time::Instant::now() + std::time::Duration::from_secs(86_400)),
        None
    );
    assert_eq!(viewing.state(), SessionState::Viewing);
}

/// An unanswered control request falls back to the view, so the user can ask
/// again — it must not take the whole view down with it.
#[test]
fn an_unanswered_control_request_falls_back_to_the_view() {
    let mut session = RemoteSession::new();
    let id = session.begin_view(ScreenDirection::PhoneToDesktop);
    session
        .accept(&message(ControlKind::ScreenReady, &id), 1_000)
        .unwrap();
    session.begin_control().unwrap();

    let expired = session.expire_pending(
        std::time::Instant::now() + CONTROL_REQUEST_TIMEOUT + std::time::Duration::from_secs(1),
    );

    assert_eq!(expired, Some(SessionState::RequestingControl));
    assert_eq!(session.state(), SessionState::Viewing);
    assert_eq!(session.remote_session_id(), Some(id.as_str()));
}

/// The phone is turned while its screen is shared: its second
/// `screen-ready` carries the new size, taps map into it from then on, and
/// control is not dropped or asked for again.
#[test]
fn a_rotation_moves_the_mapping_and_keeps_control() {
    let mut session = RemoteSession::new();
    let id = Uuid::new_v4().to_string();
    let mut request = message(ControlKind::RequestView, &id);
    request.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&request, 1_000).unwrap();
    let mut ready = message(ControlKind::ScreenReady, &id);
    ready.direction = Some(ScreenDirection::PhoneToDesktop);
    ready.screen_width = Some(1080);
    ready.screen_height = Some(2400);
    session.accept(&ready, 1_001).unwrap();
    assert_eq!(session.phone_screen(), Some((1080, 2400)));
    session.begin_control().unwrap();
    let mut granted = message(ControlKind::ControlGranted, &id);
    granted.direction = Some(ScreenDirection::PhoneToDesktop);
    granted.lease_id = Some("lease".into());
    granted.lease_expires_at = Some(31_000);
    session.accept(&granted, 1_002).unwrap();
    assert_eq!(session.state(), SessionState::Controlling);
    // Turned to landscape.
    let mut turned = message(ControlKind::ScreenReady, &id);
    turned.direction = Some(ScreenDirection::PhoneToDesktop);
    turned.screen_width = Some(2400);
    turned.screen_height = Some(1080);
    turned.screen_rotation = Some(90);
    session.accept(&turned, 1_003).unwrap();
    assert_eq!(session.phone_screen(), Some((2400, 1080)));
    assert_eq!(session.state(), SessionState::Controlling, "control stays");
}

/// A denial with no session under way is nothing to act on: it must not start "viewing".
#[test]
fn a_control_denial_with_no_session_is_ignored() {
    let mut session = RemoteSession::new();
    let denied = message(ControlKind::ControlDenied, &Uuid::new_v4().to_string());
    session.accept(&denied, 1_000).unwrap();
    assert_eq!(session.state(), SessionState::Idle);
    assert_eq!(session.control_denial(), None);
}

/// A grant shields the session from a late denial only while the lease is still good.
#[test]
fn a_denial_after_the_lease_expired_is_not_shielded() {
    let mut session = RemoteSession::new();
    let id = Uuid::new_v4().to_string();
    let mut request = message(ControlKind::RequestView, &id);
    request.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&request, 1_000).unwrap();
    let mut ready = message(ControlKind::ScreenReady, &id);
    ready.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&ready, 1_001).unwrap();
    session.begin_control().unwrap();
    let mut granted = message(ControlKind::ControlGranted, &id);
    granted.lease_id = Some("lease".into());
    granted.lease_expires_at = Some(2_000);
    session.accept(&granted, 1_002).unwrap();
    let mut denied = message(ControlKind::ControlDenied, &id);
    denied.reason = Some("too late".into());
    session.accept(&denied, 2_500).unwrap();
    assert_eq!(session.state(), SessionState::Viewing);
    assert_eq!(session.control_denial(), Some("too late"));
}

/// The reason a phone refused control belongs to the session it was said in.
#[test]
fn the_control_denial_does_not_outlive_its_session() {
    let denied_session = || {
        let mut session = RemoteSession::new();
        let id = Uuid::new_v4().to_string();
        let mut request = message(ControlKind::RequestView, &id);
        request.direction = Some(ScreenDirection::PhoneToDesktop);
        session.accept(&request, 1_000).unwrap();
        let mut denied = message(ControlKind::ControlDenied, &id);
        denied.reason = Some("accessibility is off".into());
        session.accept(&denied, 1_001).unwrap();
        assert_eq!(session.control_denial(), Some("accessibility is off"));
        (session, id)
    };
    let (mut session, _) = denied_session();
    session.begin_view(ScreenDirection::DesktopToPhone);
    assert_eq!(session.control_denial(), None, "begin_view");

    let (mut session, _) = denied_session();
    session.stop();
    assert_eq!(session.control_denial(), None, "stop");

    let (mut session, _) = denied_session();
    session.stop();
    session.adopt_offer("another-session").unwrap();
    assert_eq!(session.control_denial(), None, "adopt_offer");

    let (mut session, _) = denied_session();
    let mut request = message(ControlKind::RequestView, "a-new-session");
    request.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&request, 1_002).unwrap();
    assert_eq!(session.control_denial(), None, "request-view");
}

/// The phone asks for control again every couple of seconds; the answer to an
/// earlier request (a denial) can arrive after the grant. It must not take
/// control away, and a denial with nothing granted still does its job.
#[test]
fn a_late_denial_does_not_undo_a_grant() {
    let mut session = RemoteSession::new();
    let id = Uuid::new_v4().to_string();
    let mut request = message(ControlKind::RequestView, &id);
    request.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&request, 1_000).unwrap();
    let mut ready = message(ControlKind::ScreenReady, &id);
    ready.direction = Some(ScreenDirection::PhoneToDesktop);
    session.accept(&ready, 1_001).unwrap();
    session.begin_control().unwrap();
    let mut denied = message(ControlKind::ControlDenied, &id);
    denied.reason = Some("first answer".into());
    session.accept(&denied, 1_002).unwrap();
    assert_eq!(session.state(), SessionState::Viewing);
    assert_eq!(session.control_denial(), Some("first answer"));

    session.begin_control().unwrap();
    let mut granted = message(ControlKind::ControlGranted, &id);
    granted.lease_id = Some("lease".into());
    granted.lease_expires_at = Some(31_000);
    session.accept(&granted, 1_003).unwrap();
    assert_eq!(session.control_denial(), None, "a grant clears the reason");

    let mut late = message(ControlKind::ControlDenied, &id);
    late.reason = Some("late answer".into());
    session.accept(&late, 1_004).unwrap();
    assert_eq!(session.state(), SessionState::Controlling);
    assert_eq!(session.control_denial(), None);
    assert!(
        session.heartbeat_lease(1_005).is_some(),
        "the lease survives"
    );
}

/// Captured text counted in characters by the phone can be more bytes than
/// the bound: it is cut at a whole character (and still accepted), not
/// refused whole.
#[test]
fn captured_text_is_cut_in_bytes_not_refused() {
    let cjk = "字".repeat(12_000); // 36,000 bytes
    let cut = cut_to_bytes(cjk.clone(), MAX_CAPTURE_TEXT_BYTES);
    assert!(cut.len() <= MAX_CAPTURE_TEXT_BYTES);
    assert!(cut.len() > MAX_CAPTURE_TEXT_BYTES - 3, "as much as fits");
    assert!(cut.chars().all(|c| c == '字'), "never half a character");
    assert_eq!(cut_to_bytes("short".into(), 100), "short");
}
