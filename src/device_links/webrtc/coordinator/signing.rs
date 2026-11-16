//! Signed bootstrap records: capabilities and control messages, the
//! authentication transcript, offer attempts, DTLS fingerprints and sending a
//! signed signal over the LAN link.
//!
//! Sibling edges: none (the parent's helpers only). Outside: `core::SessionLink`,
//! `packet`, `features`, openssl.

use super::*;

pub(super) fn send_capabilities(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
) -> Result<(), String> {
    let needs_send = sessions.with_webrtc_handover(binding, attempt_id, |handover| {
        Ok(!handover.local_capabilities_confirmed())
    })?;
    if !needs_send {
        return Ok(());
    }
    let info = config
        .lock()
        .map_err(|_| "DeskLink configuration lock poisoned".to_string())?
        .local_device_info();
    let mut message = HandoverControlMessage::new(
        HandoverControlKind::Capabilities,
        attempt_id,
        wire,
        now_millis(),
    );
    message.incoming_capabilities = webrtc_capabilities(&info.incoming_capabilities);
    message.outgoing_capabilities = webrtc_capabilities(&info.outgoing_capabilities);
    send_control(binding, attempt_id, wire, message, sessions)?;
    sessions.with_webrtc_handover(binding, attempt_id, |handover| {
        handover.mark_local_capabilities_confirmed()
    })
}

pub(super) fn send_control(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    message: HandoverControlMessage,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    let payload = message.to_json()?;
    let envelope = WebRtcEnvelope::create(
        wire,
        WebRtcChannel::Control,
        HANDOVER_MESSAGE_TYPE,
        &payload,
        now_millis(),
    )?;
    let text = String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink WebRTC control serialization was not UTF-8".to_string())?;
    sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?
        .send_text(WebRtcChannel::Control, &text)
}

pub(super) fn authentication_transcript(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
) -> Result<AuthenticationTranscript, String> {
    let runtime =
        sessions.with_webrtc_handover(binding, attempt_id, |handover| Ok(handover.clone()))?;
    let offer = runtime
        .offer_sdp
        .ok_or_else(|| "DeskLink WebRTC authentication is missing the offer SDP".to_string())?;
    let answer = runtime
        .answer_sdp
        .ok_or_else(|| "DeskLink WebRTC authentication is missing the answer SDP".to_string())?;
    let local_nonce = runtime
        .local_nonce
        .ok_or_else(|| "DeskLink WebRTC authentication is missing the local nonce".to_string())?;
    let remote_nonce = runtime
        .remote_nonce
        .ok_or_else(|| "DeskLink WebRTC authentication is missing the peer nonce".to_string())?;
    let timestamp = runtime.authentication_timestamp.ok_or_else(|| {
        "DeskLink WebRTC authentication is missing its challenge timestamp".to_string()
    })?;
    let local_is_initiator =
        crate::device_links::local_goes_first(&wire.sender_device_id, &wire.peer_device_id);
    Ok(AuthenticationTranscript {
        session_attempt_id: attempt_id.to_string(),
        initiator_device_id: if local_is_initiator {
            wire.sender_device_id.clone()
        } else {
            wire.peer_device_id.clone()
        },
        responder_device_id: if local_is_initiator {
            wire.peer_device_id.clone()
        } else {
            wire.sender_device_id.clone()
        },
        session_id: wire.session_id,
        connection_generation: wire.generation,
        initiator_nonce: if local_is_initiator {
            local_nonce.clone()
        } else {
            remote_nonce.clone()
        },
        responder_nonce: if local_is_initiator {
            remote_nonce
        } else {
            local_nonce
        },
        offer_sha256: sha256_hex(&offer),
        answer_sha256: sha256_hex(&answer),
        initiator_dtls_fingerprint: dtls_fingerprint(&offer)?,
        responder_dtls_fingerprint: dtls_fingerprint(&answer)?,
        protocol_version: 1,
        timestamp,
    })
}

pub(super) fn verify_remote_capabilities(
    binding: &SessionBinding,
    message: &HandoverControlMessage,
) -> Result<(), String> {
    // The peer's handover may claim less than its identity (an older or a
    // newer build with a different profile), never more. What is used from
    // then on is the intersection with our profile.
    let info = &binding.link.info;
    if !handover_list_is_consistent(&message.incoming_capabilities, &info.incoming_capabilities) {
        return Err("WebRTC incoming capabilities do not match the paired identity".to_string());
    }
    if !handover_list_is_consistent(&message.outgoing_capabilities, &info.outgoing_capabilities) {
        return Err("WebRTC outgoing capabilities do not match the paired identity".to_string());
    }
    record_peer_profile(
        &info.id,
        &message.incoming_capabilities,
        &message.outgoing_capabilities,
    );
    Ok(())
}

pub(super) fn required_nonce(value: Option<&str>, label: &str) -> Result<String, String> {
    value
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(ToString::to_string)
        .ok_or_else(|| format!("DeskLink WebRTC handover has no valid {label}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OfferAttemptDisposition {
    InstallNew,
    ReuseCurrent,
}

/// A different attempt id is a restart, not a stale record: the only party
/// that can produce a validly signed offer is the paired initiator, and it
/// sends exactly one offer per attempt over one ordered TLS stream, so a
/// newer offer cannot be overtaken by an older one. Rejecting it — the
/// previous rule — is what left a restarted phone locked out for ~25 s while
/// this side clung to the dead peer of its previous process.
pub(super) fn offer_attempt_disposition(
    active_attempt: Option<&str>,
    received_attempt: &str,
) -> Result<OfferAttemptDisposition, String> {
    if received_attempt.is_empty() {
        return Err("DeskLink WebRTC offer has no attempt identifier".to_string());
    }
    match active_attempt {
        None => Ok(OfferAttemptDisposition::InstallNew),
        Some(active) if active == received_attempt => Ok(OfferAttemptDisposition::ReuseCurrent),
        Some(_) => Ok(OfferAttemptDisposition::InstallNew),
    }
}

fn sha256_hex(value: &str) -> String {
    hash(MessageDigest::sha256(), value.as_bytes())
        .map(|digest| digest.iter().map(|byte| format!("{byte:02x}")).collect())
        .unwrap_or_default()
}

fn dtls_fingerprint(sdp: &str) -> Result<String, String> {
    sdp.lines()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("a=fingerprint:sha-256 "))
        .filter(|fingerprint| !fingerprint.is_empty())
        .map(ToString::to_string)
        .ok_or_else(|| "DeskLink WebRTC SDP has no SHA-256 DTLS fingerprint".to_string())
}

pub(super) fn send_signed_signal(
    binding: &SessionBinding,
    wire: &WebRtcWireBinding,
    attempt_id: &str,
    message_type: SignalingMessageType,
    payload: Map<String, Value>,
    config: &Arc<Mutex<Config>>,
) -> Result<(), String> {
    let packet = {
        let config = config
            .lock()
            .map_err(|_| "DeskLink configuration lock poisoned".to_string())?;
        WebRtcSignalingMessage::unsigned(
            attempt_id,
            wire.sender_device_id.clone(),
            wire.peer_device_id.clone(),
            now_millis(),
            message_type,
            payload,
        )
        .sign(config.key())?
        .to_network_packet()
    };
    // Report the *outcome*, not the intention. Logging "sending" before the
    // write made a signal that never left the machine look identical to one the
    // peer acknowledged, which is why a stuck handover was undiagnosable from
    // the log alone.
    let result = binding.link.send_packet(&packet);
    match &result {
        Ok(()) => eprintln!(
            "[DL-WRTC-BOOT] sent {message_type:?} to {} on session {} generation {}",
            binding.device_id, binding.session_id, binding.generation
        ),
        Err(error) => {
            eprintln!(
                "[DL-WRTC-BOOT] FAILED to send {message_type:?} to {}: {error}",
                binding.device_id
            );
            close_link_after_failed_write(&binding.link, error);
        }
    }
    result
}

/// A write that failed on the socket itself (EPIPE, ECONNRESET, a write
/// timeout) means the socket is dead whatever its reader has noticed: it is
/// closed now, so the reader ends, `wants_bootstrap_connection` turns true and
/// the redialer replaces it, instead of the link lingering up to 30 s (90 s
/// dozing) until the liveness tick buries it. A WebRTC peer that is ready is
/// untouched: the link is retained for it exactly as when the reader ends.
/// "Closed" and "no stream" errors are not a write and need nothing.
pub(super) fn close_link_after_failed_write(
    link: &crate::device_links::core::SessionLink,
    error: &str,
) {
    if error.contains("Session link is closed")
        || error.contains("Session link has no network stream")
    {
        return;
    }
    link.close_at(link.epoch());
}

pub(super) fn local_and_remote_device_ids(
    binding: &SessionBinding,
    config: &Arc<Mutex<Config>>,
) -> Result<(String, String), String> {
    let local = config
        .lock()
        .map_err(|_| "DeskLink configuration lock poisoned".to_string())?
        .local_device_info()
        .id;
    Ok((local, binding.device_id.clone()))
}
