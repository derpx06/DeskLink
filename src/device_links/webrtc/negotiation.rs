use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use openssl::pkey::PKey;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::device_links::config::Config;
use crate::device_links::core::device_manager::{DeviceManager, SessionBinding};
use crate::device_links::core::events::{CoreEvent, EventBus};
use crate::device_links::daemon::network::send_packet;
use crate::device_links::packet::NetworkPacket;
use crate::device_links::pairing::PairState;
use crate::device_links::webrtc::peer_connection::{PeerConnection, PeerEvent, PeerEventSink};
use crate::device_links::webrtc::signaling::{ReplayGuard, SignalingMessage, SignalingMessageType};
use crate::device_links::webrtc::transport::WebRtcTransport;
use crate::protocol::desklink_v9::PACKET_TYPE_WEBRTC_SIGNAL_V1;

const MAX_SDP_BYTES: usize = 256 * 1024;
const MAX_ICE_CANDIDATE_BYTES: usize = 16 * 1024;

struct NegotiationAttempt {
    binding: SessionBinding,
    attempt_id: String,
    peer: Arc<PeerConnection>,
}

/// Coordinates exactly one opt-in, paired WebRTC negotiation per current LAN
/// session. LAN TLS remains the signed signaling path and the fallback data
/// path; this coordinator never exposes WebRTC to a feature handler until the
/// control channel itself opens.
#[derive(Clone, Default)]
pub struct WebRtcCoordinator {
    attempts: Arc<Mutex<HashMap<String, NegotiationAttempt>>>,
    replay_guard: ReplayGuard,
}

impl WebRtcCoordinator {
    pub fn begin_if_supported(
        &self,
        binding: &SessionBinding,
        config: Arc<Mutex<Config>>,
        sessions: DeviceManager,
        events: EventBus,
    ) {
        if !sessions.is_current(binding)
            || sessions.pairing_state(binding).ok() != Some(PairState::Paired)
        {
            return;
        }
        let Ok(config_guard) = config.lock() else {
            return;
        };
        if !config_guard.webrtc_enabled() {
            return;
        }
        let local_id = config_guard.local_device_info().id;
        let remote_accepts_signal = binding
            .link
            .info
            .incoming_capabilities
            .iter()
            .any(|capability| capability == PACKET_TYPE_WEBRTC_SIGNAL_V1);
        drop(config_guard);
        if !remote_accepts_signal {
            self.publish_state(
                &events,
                binding,
                "Unavailable",
                json!({"reason": "The paired device does not support DeskLink WebRTC signaling"}),
            );
            return;
        }

        // A stable ordering prevents offer glare without introducing a server
        // or a second logical connection. The smaller device ID initiates.
        if local_id < binding.device_id {
            if let Err(error) =
                self.start_attempt(binding.clone(), config, sessions, events.clone())
            {
                self.publish_error(&events, binding, error);
            }
        } else {
            self.publish_state(
                &events,
                binding,
                "WaitingForOffer",
                json!({"initiator": binding.device_id}),
            );
        }
    }

    pub fn handle_packet(
        &self,
        binding: &SessionBinding,
        packet: &NetworkPacket,
        config: Arc<Mutex<Config>>,
        sessions: DeviceManager,
        events: EventBus,
    ) -> Result<(), String> {
        let message =
            SignalingMessage::from_legacy_packet(packet).map_err(|error| error.to_string())?;
        let (local_id, local_enabled) = {
            let config = config
                .lock()
                .map_err(|_| "Config lock poisoned".to_string())?;
            (config.local_device_info().id, config.webrtc_enabled())
        };
        if !local_enabled {
            return Err("WebRTC signaling is disabled locally".to_string());
        }
        if !sessions.is_current(binding)
            || sessions.pairing_state(binding).ok() != Some(PairState::Paired)
        {
            return Err("WebRTC signaling requires the current paired session".to_string());
        }
        message
            .validate_for(&local_id, now_millis())
            .map_err(|error| error.to_string())?;
        if message.from_device_id != binding.device_id {
            return Err("WebRTC signaling sender does not match the current link".to_string());
        }
        let remote_key = PKey::public_key_from_der(&binding.link.remote_public_der)
            .map_err(|error| format!("invalid paired peer key: {error}"))?;
        message
            .verify_with_key(&remote_key)
            .map_err(|error| error.to_string())?;
        self.replay_guard
            .accept(&message.request_id)
            .map_err(|error| error.to_string())?;

        match message.message_type {
            SignalingMessageType::Offer => {
                // The deterministic initiator must not be displaced by an
                // unexpected remote offer. This makes duplicate TCP links and
                // stale signaling harmless.
                if local_id < binding.device_id {
                    return Err("Rejected unexpected WebRTC offer from non-initiator".to_string());
                }
                let sdp = required_string(&message.payload, "sdp", MAX_SDP_BYTES)?;
                let peer = self.start_responder_attempt(
                    binding.clone(),
                    message.session_attempt_id.clone(),
                    config,
                    sessions,
                    events.clone(),
                )?;
                peer.accept_offer_and_create_answer(&sdp)?;
            }
            SignalingMessageType::Answer => {
                let sdp = required_string(&message.payload, "sdp", MAX_SDP_BYTES)?;
                let attempt =
                    self.current_attempt(binding, &message.session_attempt_id, &sessions)?;
                attempt.peer.accept_answer(&sdp)?;
            }
            SignalingMessageType::IceCandidate => {
                let candidate =
                    required_string(&message.payload, "candidate", MAX_ICE_CANDIDATE_BYTES)?;
                let index = message
                    .payload
                    .get("sdpMLineIndex")
                    .and_then(Value::as_u64)
                    .and_then(|value| u32::try_from(value).ok())
                    .ok_or_else(|| "WebRTC ICE candidate is missing sdpMLineIndex".to_string())?;
                let attempt =
                    self.current_attempt(binding, &message.session_attempt_id, &sessions)?;
                attempt.peer.add_ice_candidate(index, &candidate)?;
            }
            SignalingMessageType::EndOfCandidates => {
                self.current_attempt(binding, &message.session_attempt_id, &sessions)?;
            }
            SignalingMessageType::IceRestart => {
                return Err("WebRTC ICE restart is not implemented yet".to_string());
            }
            SignalingMessageType::Close => {
                self.close_attempt(binding, &message.session_attempt_id, &sessions)
            }
        }
        Ok(())
    }

    pub fn close_for_binding(&self, binding: &SessionBinding, sessions: &DeviceManager) {
        let attempt = self.attempts.lock().ok().and_then(|mut attempts| {
            let current = attempts.get(&binding.device_id)?;
            if current.binding.session_id != binding.session_id
                || current.binding.generation != binding.generation
            {
                return None;
            }
            attempts.remove(&binding.device_id)
        });
        if let Some(attempt) = attempt {
            attempt.peer.close();
        }
        if let Some(transport) = sessions.current_webrtc_binding(&binding.device_id) {
            if transport.session_id == binding.session_id
                && transport.generation == binding.generation
            {
                sessions.clear_webrtc_if_current(&transport);
            }
        }
    }

    /// A newly authenticated LAN generation makes any prior peer attempt for
    /// this device stale, even if that attempt had not yet opened a control
    /// channel and therefore was not installed in `DeviceManager`.
    pub fn close_for_device(&self, device_id: &str) {
        let attempt = self
            .attempts
            .lock()
            .ok()
            .and_then(|mut attempts| attempts.remove(device_id));
        if let Some(attempt) = attempt {
            attempt.peer.close();
        }
    }

    fn start_attempt(
        &self,
        binding: SessionBinding,
        config: Arc<Mutex<Config>>,
        sessions: DeviceManager,
        events: EventBus,
    ) -> Result<(), String> {
        let attempt_id = Uuid::new_v4().to_string();
        let attempt = self.create_attempt(binding, attempt_id, config, sessions, events.clone())?;
        attempt
            .peer
            .create_channels()
            .map_err(|error| error.to_string())?;
        self.publish_state(
            &events,
            &attempt.binding,
            "CreatingOffer",
            json!({"sessionAttemptId": attempt.attempt_id}),
        );
        attempt.peer.start_offer()
    }

    fn start_responder_attempt(
        &self,
        binding: SessionBinding,
        attempt_id: String,
        config: Arc<Mutex<Config>>,
        sessions: DeviceManager,
        events: EventBus,
    ) -> Result<Arc<PeerConnection>, String> {
        let attempt = self.create_attempt(binding, attempt_id, config, sessions, events)?;
        Ok(attempt.peer)
    }

    fn create_attempt(
        &self,
        binding: SessionBinding,
        attempt_id: String,
        config: Arc<Mutex<Config>>,
        sessions: DeviceManager,
        events: EventBus,
    ) -> Result<NegotiationAttempt, String> {
        if !sessions.is_current(&binding) {
            return Err("Cannot negotiate on a stale DeskLink session".to_string());
        }
        let coordinator = self.clone();
        let event_binding = binding.clone();
        let event_attempt_id = attempt_id.clone();
        let event_config = Arc::clone(&config);
        let event_sessions = sessions.clone();
        let event_events = events.clone();
        let sink: PeerEventSink = Arc::new(move |event| {
            coordinator.on_peer_event(
                &event_binding,
                &event_attempt_id,
                event,
                Arc::clone(&event_config),
                event_sessions.clone(),
                event_events.clone(),
            );
        });
        let peer = Arc::new(PeerConnection::new_negotiated(sink)?);
        let attempt = NegotiationAttempt {
            binding: binding.clone(),
            attempt_id,
            peer,
        };
        let replaced = self
            .attempts
            .lock()
            .map_err(|_| "WebRTC attempt map poisoned".to_string())?
            .insert(
                binding.device_id.clone(),
                NegotiationAttempt {
                    binding: attempt.binding.clone(),
                    attempt_id: attempt.attempt_id.clone(),
                    peer: Arc::clone(&attempt.peer),
                },
            );
        if let Some(replaced) = replaced {
            replaced.peer.close();
        }
        Ok(attempt)
    }

    fn current_attempt(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        sessions: &DeviceManager,
    ) -> Result<NegotiationAttempt, String> {
        if !sessions.is_current(binding) {
            return Err("WebRTC signaling arrived for a stale session".to_string());
        }
        let attempts = self
            .attempts
            .lock()
            .map_err(|_| "WebRTC attempt map poisoned".to_string())?;
        let attempt = attempts
            .get(&binding.device_id)
            .ok_or_else(|| "No WebRTC negotiation exists for this device".to_string())?;
        if attempt.attempt_id != attempt_id
            || attempt.binding.session_id != binding.session_id
            || attempt.binding.generation != binding.generation
        {
            return Err("WebRTC signaling belongs to a stale negotiation".to_string());
        }
        Ok(NegotiationAttempt {
            binding: attempt.binding.clone(),
            attempt_id: attempt.attempt_id.clone(),
            peer: Arc::clone(&attempt.peer),
        })
    }

    fn close_attempt(&self, binding: &SessionBinding, attempt_id: &str, sessions: &DeviceManager) {
        if let Ok(attempt) = self.current_attempt(binding, attempt_id, sessions) {
            self.close_for_binding(&attempt.binding, sessions);
        }
    }

    fn on_peer_event(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        event: PeerEvent,
        config: Arc<Mutex<Config>>,
        sessions: DeviceManager,
        events: EventBus,
    ) {
        let Ok(attempt) = self.current_attempt(binding, attempt_id, &sessions) else {
            return;
        };
        match event {
            PeerEvent::Offer(sdp) => self.send_signal(
                &attempt.binding,
                &attempt.attempt_id,
                SignalingMessageType::Offer,
                json!({"sdp": sdp}),
                config,
                &events,
            ),
            PeerEvent::Answer(sdp) => self.send_signal(
                &attempt.binding,
                &attempt.attempt_id,
                SignalingMessageType::Answer,
                json!({"sdp": sdp}),
                config,
                &events,
            ),
            PeerEvent::IceCandidate {
                mline_index,
                candidate,
            } => self.send_signal(
                &attempt.binding,
                &attempt.attempt_id,
                SignalingMessageType::IceCandidate,
                json!({"sdpMLineIndex": mline_index, "candidate": candidate}),
                config,
                &events,
            ),
            PeerEvent::EndOfCandidates => self.send_signal(
                &attempt.binding,
                &attempt.attempt_id,
                SignalingMessageType::EndOfCandidates,
                json!({}),
                config,
                &events,
            ),
            PeerEvent::ControlChannelOpen => {
                let transport = WebRtcTransport::from_peer(
                    &attempt.binding.device_id,
                    attempt.binding.session_id,
                    attempt.binding.generation,
                    Arc::clone(&attempt.peer),
                );
                match sessions.register_webrtc_transport(&attempt.binding, transport) {
                    Ok(_) => self.publish_state(
                        &events,
                        &attempt.binding,
                        "Ready",
                        json!({
                            "transportId": format!(
                                "webrtc:{}:{}:{}",
                                attempt.binding.device_id,
                                attempt.binding.session_id,
                                attempt.binding.generation
                            ),
                            "sessionAttemptId": attempt.attempt_id,
                            "fallback": "LAN TLS remains active until feature handover is enabled"
                        }),
                    ),
                    Err(error) => self.publish_error(
                        &events,
                        &attempt.binding,
                        format!("WebRTC control channel opened for a stale session: {error:?}"),
                    ),
                }
            }
            PeerEvent::Failure(error) => self.publish_error(&events, &attempt.binding, error),
        }
    }

    fn send_signal(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        message_type: SignalingMessageType,
        payload: Value,
        config: Arc<Mutex<Config>>,
        events: &EventBus,
    ) {
        let result = (|| -> Result<(), String> {
            let config = config
                .lock()
                .map_err(|_| "Config lock poisoned".to_string())?;
            if !config.webrtc_enabled() {
                return Err("WebRTC signaling was disabled while negotiating".to_string());
            }
            let mut message = SignalingMessage {
                signaling_version: 1,
                request_id: Uuid::new_v4().to_string(),
                session_attempt_id: attempt_id.to_string(),
                from_device_id: config.local_device_info().id,
                to_device_id: binding.device_id.clone(),
                timestamp: now_millis(),
                message_type,
                payload,
                signature: String::new(),
            };
            message
                .sign_with_key(config.key())
                .map_err(|error| error.to_string())?;
            let packet = message
                .to_legacy_packet()
                .map_err(|error| error.to_string())?;
            send_packet(&binding.link.stream, &packet)
        })();
        if let Err(error) = result {
            self.publish_error(
                events,
                binding,
                format!("Could not send WebRTC signaling: {error}"),
            );
        }
    }

    fn publish_state(
        &self,
        events: &EventBus,
        binding: &SessionBinding,
        state: &str,
        details: Value,
    ) {
        eprintln!("[WebRTC] device={} state={state}", binding.device_id);
        events.publish(CoreEvent::FeatureStateChanged {
            device_id: binding.device_id.clone(),
            feature: "webrtc".to_string(),
            state: state.to_string(),
            details,
        });
    }

    fn publish_error(&self, events: &EventBus, binding: &SessionBinding, message: String) {
        self.publish_state(events, binding, "Failed", json!({"error": message}));
        events.publish(CoreEvent::Error {
            scope: "webrtc".to_string(),
            device_id: Some(binding.device_id.clone()),
            message,
            retryable: true,
        });
    }
}

fn required_string(payload: &Value, key: &str, maximum: usize) -> Result<String, String> {
    let value = payload
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("WebRTC signaling payload is missing {key}"))?;
    if value.is_empty() || value.len() > maximum {
        return Err(format!(
            "WebRTC signaling payload {key} has an invalid size"
        ));
    }
    Ok(value.to_string())
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_missing_and_oversized_signal_payloads() {
        assert!(required_string(&json!({}), "sdp", MAX_SDP_BYTES).is_err());
        assert!(required_string(
            &json!({"sdp": "x".repeat(MAX_SDP_BYTES + 1)}),
            "sdp",
            MAX_SDP_BYTES
        )
        .is_err());
    }
}
