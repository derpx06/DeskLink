//! Envelopes from a live peer: the handover, feature-ready, file envelopes and
//! the per-peer work lane, plus flushing features queued before it.
//!
//! Sibling edges: `signing`, `phone_remote`, `desktop_share`, `share_worker`,
//! `liveness`, `recovery`, `terminal_pump`. Outside: `features` (profile),
//! `transfer_manager`, `session_lock`, `transport`, `daemon::packet_handler`,
//! `desk_features`, `connection_log`.

use super::*;

/// Sends, in order, the features queued while WebRTC was not ready.
fn flush_pending_features(binding: &SessionBinding, sessions: &Arc<DeviceManager>) {
    let pending = sessions.take_pending_features(binding);
    if pending.is_empty() {
        return;
    }
    match crate::device_links::transport::FeatureTransport::for_binding(sessions, binding) {
        Ok(transport) => {
            for packet in pending {
                if let Err(error) = transport.send(&packet) {
                    eprintln!("[DeskLink] Queued {} not sent: {error}", packet.packet_type);
                }
            }
        }
        Err(error) => eprintln!("[DeskLink] Queued features not sent: {error}"),
    }
}

pub(super) fn begin_handover(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    // The lexicographically lower device ID is the deterministic initiator on
    // both platforms. The responder waits for its signed hello.
    if wire.sender_device_id >= wire.peer_device_id {
        return Ok(());
    }
    let nonce = Uuid::new_v4().to_string();
    let should_send = sessions.with_webrtc_handover(binding, attempt_id, |handover| {
        if handover.local_nonce.is_some() {
            return Ok(false);
        }
        handover.record_local_nonce(nonce.clone())?;
        Ok(true)
    })?;
    if !should_send {
        return Ok(());
    }
    let mut message =
        HandoverControlMessage::new(HandoverControlKind::Hello, attempt_id, wire, now_millis());
    message.nonce = Some(nonce);
    send_control(binding, attempt_id, wire, message, sessions)
}

#[allow(clippy::too_many_arguments)] // Captures immutable session/event context without global state.
pub(super) fn handle_peer_envelope(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    channel: WebRtcChannel,
    bytes: &[u8],
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
    transfer_manager: &Arc<WebRtcTransferManager>,
) -> Result<(), String> {
    let envelope = WebRtcEnvelope::from_json(bytes)?;
    if envelope.message_type == FILE_CONTROL_MESSAGE_TYPE {
        return handle_file_envelope(
            binding,
            attempt_id,
            wire,
            channel,
            envelope,
            sessions,
            transfer_manager,
        );
    }
    if channel == WebRtcChannel::Terminal {
        return handle_terminal_envelope(binding, attempt_id, wire, envelope, sessions);
    }
    if channel != WebRtcChannel::Control {
        // The envelope declares its own channel, and the bridge checks that
        // against the packet type. Neither check looks at the channel the bytes
        // actually arrived on, so an envelope claiming the reliable events
        // channel could be delivered over the unordered zero-retransmit input
        // channel. Bind the two together before decoding.
        if envelope.channel != channel.label() {
            return Err(
                "DeskLink WebRTC envelope arrived on a different channel than it declares"
                    .to_string(),
            );
        }
        let packet = WebRtcPacketBridge::decode(wire, &envelope)?;
        sessions
            .accept_webrtc_envelope(binding, attempt_id, envelope.message_id.clone())
            .map_err(|error| error.to_string())?;
        if packet.packet_type == crate::device_links::packet::PACKET_TYPE_HEARTBEAT {
            // Liveness only: arriving at all refreshed the worker's clock.
            note_heartbeat(&wire.peer_device_id, &packet);
            if is_leaving(&wire.peer_device_id) {
                crate::device_links::daemon::state::set_quiet(devices, &wire.peer_device_id);
            }
            return if packet.get_bool("reply").unwrap_or(false) {
                Ok(())
            } else {
                send_keepalive(binding, attempt_id, wire, sessions, true)
            };
        }
        return crate::device_links::daemon::packet_handler::dispatch_webrtc_feature_packet(
            binding, packet, devices, sessions, config, events,
        );
    }
    if envelope.message_type == remote_session::MESSAGE_TYPE {
        // Android has always sent this on the control channel; the desktop used
        // to reject every non-handover control message, so the screen control
        // plane was dead on this side while `remote_session` sat unreferenced.
        if envelope.channel != WebRtcChannel::Control.label() {
            return Err("DeskLink remote-session control arrived on the wrong channel".to_string());
        }
        // As for files, terminals and features: nothing before the handover
        // has proved both sides (the phone gates the same way).
        if !sessions
            .webrtc_features_ready(binding, attempt_id)
            .map_err(|error| error.to_string())?
        {
            return Err(
                "DeskLink remote-session control arrived before feature handover".to_string(),
            );
        }
        let payload = envelope.validate(wire)?;
        sessions
            .accept_webrtc_envelope(binding, attempt_id, envelope.message_id.clone())
            .map_err(|error| error.to_string())?;
        let message = remote_session::ControlMessage::from_json(&payload)?;
        if message.kind != remote_session::ControlKind::Heartbeat {
            debug_log!(
                "[DeskLink] Remote session {:?} from {}{}",
                message.kind,
                wire.peer_device_id,
                message
                    .reason
                    .as_deref()
                    .map(|reason| format!(": {reason}"))
                    .unwrap_or_default()
            );
        }
        let now = now_millis();
        message.validate(wire, attempt_id, now)?;
        if message.kind.is_audio() {
            return handle_audio_message(binding, attempt_id, wire, &message, sessions, events);
        }
        if message.kind.is_capture() {
            handle_capture_message(&wire.peer_device_id, &binding.link.info.name, &message);
            return Ok(());
        }
        if message.kind == remote_session::ControlKind::OfferView {
            return take_up_phone_offer(binding, attempt_id, wire, &message, sessions, events);
        }
        if message.kind == remote_session::ControlKind::RequestView {
            let sequence = {
                let mut open = remote_session::sessions()
                    .lock()
                    .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
                let session = open.entry(wire.peer_device_id.clone()).or_default();
                session.accept(&message, now)?;
                session.next_sequence()
            };
            let peer = sessions
                .active_webrtc_peer(binding, attempt_id)
                .map_err(|error| error.to_string())?;
            let direction = message
                .direction
                .ok_or_else(|| "DeskLink screen request has no direction".to_string())?;
            if !desktop_capture_needed(direction) {
                // The phone is sharing its own screen. The Android coordinator
                // handles its local MediaProjection consent and sends the
                // corresponding view-granted/screen-ready controls; desktop
                // portal capture would invert the requested session direction.
                stop_desktop_portal_session(&wire.peer_device_id, None);
                crate::device_links::webrtc::screen::clear_stream(&wire.peer_device_id);
                return Ok(());
            }
            if let Err(reason) = crate::device_links::session_lock::check(
                &wire.peer_device_id,
                wire.session_id,
                message.password.as_deref(),
            ) {
                return deny_view(
                    &peer,
                    wire,
                    attempt_id,
                    &message.remote_session_id,
                    sequence,
                    reason,
                );
            }
            // The phone's own size and rate (contract V1); older phones ask
            // nothing and get the fallback.
            let mut request = crate::device_links::webrtc::screen::ScreenRequest::from_parts(
                message.max_dimension,
                message.fps,
            );
            // A display mode this build does not know (a newer phone's) is
            // answered as unsupported, never shown as a mirror nor dropped.
            if message
                .display
                .as_deref()
                .is_some_and(|display| !matches!(display, "mirror" | "extend"))
            {
                return deny_view(
                    &peer,
                    wire,
                    attempt_id,
                    &message.remote_session_id,
                    sequence,
                    "unsupported".to_string(),
                );
            }
            // A second display (F2) at the phone's own pixel size.
            if message.display.as_deref() == Some("extend") {
                let size = message.screen_width.zip(message.screen_height);
                // The phone names the size: bounded here, not by 8192 x 8192
                // (a 268 MB frame composited and encoded at up to 60 fps).
                match size
                    .filter(|(width, height)| extend_size_ok(*width, *height))
                    .filter(|_| crate::device_links::webrtc::virtual_display::supported())
                {
                    Some(size) => request.extend = Some(size),
                    None => {
                        return deny_view(
                            &peer,
                            wire,
                            attempt_id,
                            &message.remote_session_id,
                            sequence,
                            "extend-unsupported".to_string(),
                        )
                    }
                }
            }
            let started = start_desktop_portal_session(
                wire.peer_device_id.clone(),
                message.remote_session_id.clone(),
                attempt_id.to_string(),
                wire.clone(),
                peer.clone(),
                request,
            );
            if started.is_ok() {
                connection_log::record(&wire.peer_device_id, ConnectionEvent::ScreenViewStarted);
            }
            if let Err(error) = started {
                return deny_view(
                    &peer,
                    wire,
                    attempt_id,
                    &message.remote_session_id,
                    sequence,
                    error,
                );
            }
            return Ok(());
        }
        if message.kind == remote_session::ControlKind::SwitchScreen {
            // The share's worker owns the portal; hand it the request.
            if let Ok(services) = desktop_portal_services().lock() {
                if let Some(service) = services.get(&wire.peer_device_id) {
                    let _ = service.sender.try_send(DesktopPortalCommand::SwitchScreen);
                }
            }
            return Ok(());
        }
        if matches!(
            message.kind,
            remote_session::ControlKind::RequestControl
                | remote_session::ControlKind::TakeoverRequest
        ) {
            let peer = sessions
                .active_webrtc_peer(binding, attempt_id)
                .map_err(|error| error.to_string())?;
            let lease_id = Uuid::new_v4().to_string();
            let expires_at = now_millis() + remote_session::LEASE_LIFETIME.as_millis() as i64;
            // Checked before the session lock: hashing takes a moment, and the
            // GTK main loop takes that lock too.
            let unlocked = crate::device_links::session_lock::check(
                &wire.peer_device_id,
                wire.session_id,
                message.password.as_deref(),
            );
            let response = {
                let mut open = remote_session::sessions()
                    .lock()
                    .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
                let session = open.entry(wire.peer_device_id.clone()).or_default();
                let authorization = session.accept(&message, now);
                let authorized = authorization.is_ok();
                let can_control = desktop_portal_services()
                    .lock()
                    .map(|services| {
                        services
                            .get(&wire.peer_device_id)
                            .is_some_and(|service| service.can_control)
                    })
                    .unwrap_or(false);
                let grant = can_control && authorization.is_ok() && unlocked.is_ok();
                if !grant {
                    debug_log!(
                        "[DeskLink] Denying phone control for {}: portal_can_control={}, session_authorized={}, reason={}",
                        wire.peer_device_id,
                        can_control,
                        authorization.is_ok(),
                        authorization.as_ref().err().map_or("portal input devices not granted", String::as_str)
                    );
                }
                if grant {
                    connection_log::record(
                        &wire.peer_device_id,
                        ConnectionEvent::ScreenControlStarted,
                    );
                    session.grant_peer_control(
                        lease_id.clone(),
                        wire.peer_device_id.clone(),
                        expires_at,
                    )?;
                }
                let mut response = remote_session::ControlMessage::new(
                    if grant {
                        if message.kind == remote_session::ControlKind::TakeoverRequest {
                            remote_session::ControlKind::TakeoverGranted
                        } else {
                            remote_session::ControlKind::ControlGranted
                        }
                    } else {
                        remote_session::ControlKind::ControlDenied
                    },
                    attempt_id,
                    wire,
                    &message.remote_session_id,
                    session.next_sequence(),
                    now_millis(),
                );
                if grant {
                    response.lease_id = Some(lease_id);
                    response.owner_device_id = Some(wire.peer_device_id.clone());
                    response.lease_expires_at = Some(expires_at);
                } else {
                    response.reason =
                        Some(unlocked.err().or(authorization.err()).unwrap_or_else(|| {
                            control_denied_reason(
                                can_control,
                                crate::platform::note("share").is_some(),
                            )
                        }));
                    // Back to the view the request was made on. A request
                    // that was itself refused changed nothing, and applying
                    // the denial then would invent a view that is not there.
                    if authorized {
                        let _ = session.accept(&response, now_millis());
                    }
                }
                response
            };
            send_remote_session_message(&peer, wire, &response)?;
            return Ok(());
        }
        // Recorded under the session lock, acted on after it: stopping a capture
        // or clearing the preview changes GStreamer element state, which can wait
        // on the GTK main loop — and the main loop takes this same lock.
        let teardown;
        let mut resume = false;
        let request_control = {
            let mut open = remote_session::sessions()
                .lock()
                .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
            let session = open.entry(wire.peer_device_id.clone()).or_default();
            session.accept(&message, now)?;
            teardown = matches!(
                message.kind,
                remote_session::ControlKind::Release
                    | remote_session::ControlKind::ScreenStopped
                    | remote_session::ControlKind::ScreenError
            );
            if message.kind == remote_session::ControlKind::ScreenReady
                && message.direction == Some(remote_session::ScreenDirection::PhoneToDesktop)
            {
                // A second view of the phone arrives on the pad the first one
                // parked; it is shown again after the lock (the first view
                // comes via pad-added).
                resume = true;
                // A second `screen-ready` is the phone turning its screen:
                // the size is taken (in `accept`), control is not asked again.
                if matches!(
                    session.state(),
                    remote_session::SessionState::Controlling
                        | remote_session::SessionState::RequestingControl
                ) {
                    None
                } else {
                    session.begin_control()?;
                    Some(remote_session::ControlMessage::new(
                        remote_session::ControlKind::RequestControl,
                        attempt_id,
                        wire,
                        &message.remote_session_id,
                        session.next_sequence(),
                        now_millis(),
                    ))
                }
            } else {
                None
            }
        };
        // A desk stop may have run while the picture was being shown again
        // (it waits on the main loop): a stream nobody wants any more goes.
        if resume && crate::device_links::webrtc::screen::resume_parked(&wire.peer_device_id) {
            let wanted = remote_session::sessions()
                .lock()
                .ok()
                .and_then(|open| {
                    open.get(&wire.peer_device_id)
                        .map(|session| session.shows_phone_screen(&message.remote_session_id))
                })
                .unwrap_or(false);
            if !wanted {
                crate::device_links::webrtc::screen::clear_stream(&wire.peer_device_id);
            }
        }
        if teardown {
            crate::device_links::webrtc::screen::clear_stream(&wire.peer_device_id);
            stop_desktop_portal_session(&wire.peer_device_id, None);
        }
        if let Some(request) = request_control {
            let peer = sessions
                .active_webrtc_peer(binding, attempt_id)
                .map_err(|error| error.to_string())?;
            send_remote_session_message(&peer, wire, &request)?;
        }
        return Ok(());
    }
    if envelope.message_type != HANDOVER_MESSAGE_TYPE {
        return Err("Unsupported DeskLink WebRTC control message type".to_string());
    }
    if envelope.channel != WebRtcChannel::Control.label() {
        return Err("DeskLink WebRTC handover arrived on the wrong channel".to_string());
    }
    let payload = envelope.validate(wire)?;
    sessions
        .accept_webrtc_envelope(binding, attempt_id, envelope.message_id.clone())
        .map_err(|error| error.to_string())?;
    let message = HandoverControlMessage::from_json(&payload)?;
    message.validate(wire, attempt_id, now_millis())?;
    let local_is_initiator =
        crate::device_links::local_goes_first(&wire.sender_device_id, &wire.peer_device_id);

    match message.kind {
        HandoverControlKind::Hello => {
            if local_is_initiator {
                return Err("DeskLink WebRTC initiator received an unexpected hello".to_string());
            }
            let initiator_nonce = required_nonce(message.nonce.as_deref(), "hello nonce")?;
            let responder_nonce = Uuid::new_v4().to_string();
            let timestamp = now_millis();
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                handover.record_remote_nonce(initiator_nonce.clone())?;
                handover.record_local_nonce(responder_nonce.clone())?;
                handover.record_authentication_timestamp(timestamp)
            })?;
            let transcript = authentication_transcript(binding, attempt_id, wire, sessions)?;
            let signature = {
                let config = config
                    .lock()
                    .map_err(|_| "DeskLink configuration lock poisoned".to_string())?;
                transcript.sign(config.key())?
            };
            let mut response = HandoverControlMessage::new(
                HandoverControlKind::Challenge,
                attempt_id,
                wire,
                timestamp,
            );
            response.nonce = Some(responder_nonce);
            response.peer_nonce = Some(initiator_nonce);
            response.signature_base64 =
                Some(AuthenticationTranscript::signature_to_base64(&signature));
            send_control(binding, attempt_id, wire, response, sessions)
        }
        HandoverControlKind::Challenge => {
            if !local_is_initiator {
                return Err(
                    "DeskLink WebRTC responder received an unexpected challenge".to_string()
                );
            }
            let responder_nonce = required_nonce(message.nonce.as_deref(), "challenge nonce")?;
            let initiator_nonce = required_nonce(message.peer_nonce.as_deref(), "peer nonce")?;
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                if handover.local_nonce.as_deref() != Some(initiator_nonce.as_str()) {
                    return Err(
                        "DeskLink WebRTC challenge nonce does not match the initiator".to_string(),
                    );
                }
                handover.record_remote_nonce(responder_nonce.clone())?;
                handover.record_authentication_timestamp(message.timestamp)
            })?;
            let transcript = authentication_transcript(binding, attempt_id, wire, sessions)?;
            let remote_key = PKey::public_key_from_der(&binding.link.remote_public_der)
                .map_err(|error| error.to_string())?;
            let signature = AuthenticationTranscript::signature_from_base64(
                message
                    .signature_base64
                    .as_deref()
                    .ok_or_else(|| "DeskLink WebRTC challenge has no signature".to_string())?,
            )?;
            if !transcript.verify(&remote_key, &signature) {
                return Err("Invalid paired identity signature on WebRTC challenge".to_string());
            }
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                handover.mark_remote_authenticated()
            })?;
            let signed_response = {
                let config = config
                    .lock()
                    .map_err(|_| "DeskLink configuration lock poisoned".to_string())?;
                transcript.sign(config.key())?
            };
            let mut response = HandoverControlMessage::new(
                HandoverControlKind::Response,
                attempt_id,
                wire,
                message.timestamp,
            );
            response.nonce = Some(initiator_nonce);
            response.peer_nonce = Some(responder_nonce);
            response.signature_base64 = Some(AuthenticationTranscript::signature_to_base64(
                &signed_response,
            ));
            send_control(binding, attempt_id, wire, response, sessions)
        }
        HandoverControlKind::Response => {
            if local_is_initiator {
                return Err("DeskLink WebRTC initiator received an unexpected response".to_string());
            }
            let initiator_nonce = required_nonce(message.nonce.as_deref(), "response nonce")?;
            let responder_nonce = required_nonce(message.peer_nonce.as_deref(), "peer nonce")?;
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                if handover.remote_nonce.as_deref() != Some(initiator_nonce.as_str())
                    || handover.local_nonce.as_deref() != Some(responder_nonce.as_str())
                    || handover.authentication_timestamp != Some(message.timestamp)
                {
                    return Err("DeskLink WebRTC response does not match its challenge".to_string());
                }
                Ok(())
            })?;
            let transcript = authentication_transcript(binding, attempt_id, wire, sessions)?;
            let remote_key = PKey::public_key_from_der(&binding.link.remote_public_der)
                .map_err(|error| error.to_string())?;
            let signature = AuthenticationTranscript::signature_from_base64(
                message
                    .signature_base64
                    .as_deref()
                    .ok_or_else(|| "DeskLink WebRTC response has no signature".to_string())?,
            )?;
            if !transcript.verify(&remote_key, &signature) {
                return Err("Invalid paired identity signature on WebRTC response".to_string());
            }
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                handover.mark_remote_authenticated()?;
                handover.mark_local_authenticated()
            })?;
            send_control(
                binding,
                attempt_id,
                wire,
                HandoverControlMessage::new(
                    HandoverControlKind::Authenticated,
                    attempt_id,
                    wire,
                    now_millis(),
                ),
                sessions,
            )?;
            send_capabilities(binding, attempt_id, wire, sessions, config)
        }
        HandoverControlKind::Authenticated => {
            if !local_is_initiator {
                return Err("DeskLink WebRTC responder received an unexpected authentication acknowledgement".to_string());
            }
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                if !handover.remote_authenticated() {
                    return Err(
                        "DeskLink WebRTC authentication acknowledgement arrived too early"
                            .to_string(),
                    );
                }
                handover.mark_local_authenticated()
            })?;
            send_capabilities(binding, attempt_id, wire, sessions, config)
        }
        HandoverControlKind::Capabilities => {
            verify_remote_capabilities(binding, &message)?;
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                handover.mark_remote_capabilities_confirmed()
            })?;
            // `send_capabilities` is intentionally idempotent and makes the
            // order of the two peers' messages harmless.
            send_capabilities(binding, attempt_id, wire, sessions, config)?;
            send_feature_ready_if_possible(binding, attempt_id, wire, sessions).map(|_| ())
        }
        HandoverControlKind::FeatureReady => {
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                handover.mark_remote_feature_ready()
            })?;
            send_feature_ready_if_possible(binding, attempt_id, wire, sessions)?;
            if sessions
                .webrtc_features_ready(binding, attempt_id)
                .map_err(|error| error.to_string())?
            {
                finish_feature_ready(binding, sessions, devices, transfer_manager);
            }
            Ok(())
        }
        HandoverControlKind::Degraded => {
            sessions.with_webrtc_handover(binding, attempt_id, |handover| {
                handover.fail();
                Ok(())
            })?;
            Err("Remote DeskLink WebRTC peer entered degraded state".to_string())
        }
        HandoverControlKind::Close => {
            // Clear the session state as well as closing the peer. Closing
            // alone left `webrtc_handover` installed and feature-ready with no
            // worker to service it, which then blocked discovery from ever
            // offering a replacement connection.
            match sessions.clear_webrtc_if_current(
                binding,
                attempt_id,
                "The paired device closed the WebRTC session".to_string(),
            ) {
                Ok(Some(peer)) => peer.close(),
                Ok(None) => {}
                Err(error) => eprintln!("[DeskLink] Ignored a stale WebRTC close: {error}"),
            }
            Ok(())
        }
    }
}

/// Completes the desktop half of the symmetric handover. Android sends its
/// `feature-ready` after receiving desktop capabilities; desktop must answer
/// with its own message before either side installs the feature transport.
/// Work for a peer's file and terminal thread.
pub(super) type WorkJob = Box<dyn FnOnce() + Send>;

/// Starts a peer's work thread (it ends with the worker that owns the
/// sender). Unbounded in messages, bounded in bytes: each queued message holds
/// a `Credit` from the channel's backlog budget.
pub(super) fn spawn_work_lane() -> Result<mpsc::Sender<WorkJob>, String> {
    spawn_work_lane_with(|name, body| {
        thread::Builder::new()
            .name(name.into())
            .spawn(body)
            .map(|_| ())
    })
}

/// The lane, started through `spawn`. A lane whose thread did not start is an
/// error: its sender would accept jobs that never run, and the peer would
/// drop every file and terminal message while reading as connected.
pub(super) fn spawn_work_lane_with(
    spawn: impl FnOnce(&str, Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
) -> Result<mpsc::Sender<WorkJob>, String> {
    let (sender, jobs) = mpsc::channel::<WorkJob>();
    spawn(
        "desklink-peer-work",
        Box::new(move || {
            for job in jobs {
                crate::guard("peer work", (), job);
            }
        }),
    )
    .map_err(|error| format!("Could not start the peer's work thread: {error}"))?;
    Ok(sender)
}

/// What happens once, when both sides are feature-ready: the queued features
/// go, the connection log and the phone hear of it, interrupted sends resume.
pub(super) fn finish_feature_ready(
    binding: &SessionBinding,
    sessions: &Arc<DeviceManager>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    transfer_manager: &Arc<WebRtcTransferManager>,
) {
    clear_error(devices, &binding.device_id);
    note_lacking(&binding.device_id, false);
    // Not a reset of the backoff ladder: a link that dies right
    // after becoming ready (a failure after ready) must keep
    // climbing, or it loops at the first rung for ever. The
    // ladder resets after [`HEALTHY_AFTER`] of healthy ready time,
    // or on a sign of the phone (`recovery_sign`).
    debug_log!(
        "[DeskLink] WebRTC feature transport ready for {}",
        binding.device_id
    );
    flush_pending_features(binding, sessions);
    crate::device_links::connection_log::note_ready(&binding.device_id, true);
    // Shown now, not at the next status pass and then the next window tick
    // (F26: the label said Connected up to 2 s after feature-ready).
    crate::device_links::daemon::state::reconcile_connection_status(
        devices,
        &sessions.connection_snapshot(),
    );
    crate::device_links::daemon::state::nudge_tickers();
    crate::device_links::desk_features::on_feature_ready(&binding.device_id);
    // A send a lost connection interrupted carries on where the
    // phone stopped; the phone does the same for its own sends.
    transfer_manager.resume_sends();
}

/// Sends this side's `feature-ready` once capabilities are confirmed and
/// the required channels (control, events, liveness) are open. Whether it
/// was sent now.
pub(super) fn send_feature_ready_if_possible(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
) -> Result<bool, String> {
    let should_send = sessions.with_webrtc_handover(binding, attempt_id, |handover| {
        Ok(handover.capabilities_confirmed() && !handover.local_feature_ready())
    })?;
    if !should_send {
        return Ok(false);
    }
    // Not before every required channel is open: a probe or a feature sent
    // into a channel that is not there yet is lost.
    let open = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?
        .required_channels_open();
    if !open {
        return Ok(false);
    }

    let message = HandoverControlMessage::new(
        HandoverControlKind::FeatureReady,
        attempt_id,
        wire,
        now_millis(),
    );
    send_control(binding, attempt_id, wire, message, sessions)?;
    sessions.with_webrtc_handover(binding, attempt_id, |handover| {
        handover.mark_local_feature_ready()
    })?;
    Ok(true)
}

/// A control message of the file protocol. (File data never comes here: it is
/// binary frames on its own channel, handed to the manager as they arrive.)
fn handle_file_envelope(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    channel: WebRtcChannel,
    envelope: WebRtcEnvelope,
    sessions: &Arc<DeviceManager>,
    transfer_manager: &Arc<WebRtcTransferManager>,
) -> Result<(), String> {
    if !sessions
        .webrtc_features_ready(binding, attempt_id)
        .map_err(|error| error.to_string())?
    {
        return Err("DeskLink WebRTC file transfer arrived before feature handover".to_string());
    }
    if channel != WebRtcChannel::FileControl
        || envelope.channel != WebRtcChannel::FileControl.label()
    {
        return Err("DeskLink WebRTC file message arrived on the wrong channel".to_string());
    }
    let payload = envelope.validate(wire)?;
    sessions
        .accept_webrtc_envelope(binding, attempt_id, envelope.message_id)
        .map_err(|error| error.to_string())?;
    transfer_manager.on_control(FileControl::from_json(&payload)?);
    Ok(())
}
