use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use openssl::pkey::PKey;
use openssl::sha::sha256;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::device_links::config::Config;
use crate::device_links::core::device_manager::{DeviceManager, SessionBinding, WebRtcBinding};
use crate::device_links::core::events::{CoreEvent, EventBus};
use crate::device_links::daemon::network::send_packet;
use crate::device_links::daemon::state::update_screen_frame;
use crate::device_links::device::{DeviceView, ScreenFrame};
use crate::device_links::packet::NetworkPacket;
use crate::device_links::pairing::PairState;
use crate::device_links::webrtc::authentication::AuthenticationTranscript;
use crate::device_links::webrtc::channel::DataChannelSpec;
use crate::device_links::webrtc::envelope::MessageEnvelope;
use crate::device_links::webrtc::file_browser::{
    PhoneFileAction, PhoneFileRequest, PhoneFileResponse, PHONE_FILE_BROWSER_MESSAGE_TYPE,
};
use crate::device_links::webrtc::file_protocol::{
    FileTransferControl, FILE_CHUNK_MESSAGE_TYPE, FILE_CONTROL_MESSAGE_TYPE,
};
use crate::device_links::webrtc::file_transfer::{OutboundFileMessage, WebRtcFileTransferManager};
use crate::device_links::webrtc::handover::{
    HandoverControlKind, HandoverControlMessage, HandoverMessage, HANDOVER_MESSAGE_TYPE,
    HANDOVER_VERSION,
};
use crate::device_links::webrtc::packet_bridge::decode_packet;
use crate::device_links::webrtc::peer_connection::{PeerConnection, PeerEvent, PeerEventSink};
use crate::device_links::webrtc::recovery::RecoveryState;
use crate::device_links::webrtc::signaling::{ReplayGuard, SignalingMessage, SignalingMessageType};
use crate::device_links::webrtc::transport::WebRtcTransport;
use crate::device_links::webrtc::wire_binding::WebRtcWireBinding;
use crate::protocol::desklink_v9::PACKET_TYPE_WEBRTC_SIGNAL_V1;

const MAX_SDP_BYTES: usize = 256 * 1024;
const MAX_ICE_CANDIDATE_BYTES: usize = 16 * 1024;

fn decode_feature_message(
    wire: &WebRtcWireBinding,
    data_channel_label: &str,
    bytes: &[u8],
    feature_ready: bool,
) -> Result<NetworkPacket, String> {
    if !feature_ready {
        return Err("WebRTC feature handover is incomplete".to_string());
    }
    if bytes.len() > crate::device_links::webrtc::MAX_ENVELOPE_BYTES {
        return Err("WebRTC envelope exceeds the maximum size".to_string());
    }
    let envelope: MessageEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| format!("Malformed WebRTC envelope: {error}"))?;
    if envelope.channel != data_channel_label {
        return Err("WebRTC envelope arrived on the wrong data channel".to_string());
    }
    decode_packet(wire, &envelope).map_err(|error| error.to_string())
}

struct NegotiationAttempt {
    binding: SessionBinding,
    attempt_id: String,
    peer: Arc<PeerConnection>,
    handover: Arc<Mutex<HandoverRuntime>>,
}

#[derive(Default)]
struct HandoverRuntime {
    offer_sdp: Option<String>,
    answer_sdp: Option<String>,
    local_nonce: Option<String>,
    remote_nonce: Option<String>,
    authentication_timestamp: Option<i64>,
    peer_authenticated: bool,
    local_capabilities_sent: bool,
    remote_capabilities_received: bool,
    local_feature_ready_sent: bool,
    remote_feature_ready_received: bool,
}

pub type WebRtcPacketSink = Arc<dyn Fn(NetworkPacket) -> Result<(), String> + Send + Sync>;
type SharedDevices = Arc<Mutex<HashMap<String, DeviceView>>>;

struct PacketSinkRegistration {
    session_id: u64,
    generation: u64,
    sink: WebRtcPacketSink,
}

struct BrowserRequestBinding {
    device_id: String,
    session_id: u64,
    generation: u64,
    action: PhoneFileAction,
}

/// Coordinates exactly one paired WebRTC negotiation per current LAN session.
/// LAN TLS carries only bootstrap and signed signaling after pairing; feature
/// handlers are exposed only after authenticated bidirectional handover.
#[derive(Clone, Default)]
pub struct WebRtcCoordinator {
    attempts: Arc<Mutex<HashMap<String, NegotiationAttempt>>>,
    devices: Arc<Mutex<Option<SharedDevices>>>,
    packet_sinks: Arc<Mutex<HashMap<String, PacketSinkRegistration>>>,
    replay_guard: ReplayGuard,
    feature_replay_guard: ReplayGuard,
    file_transfers: WebRtcFileTransferManager,
    recoveries: Arc<Mutex<HashMap<String, RecoveryState>>>,
    browser_requests: Arc<Mutex<HashMap<String, BrowserRequestBinding>>>,
}

impl WebRtcCoordinator {
    pub fn attach_devices(&self, devices: SharedDevices) {
        if let Ok(mut target) = self.devices.lock() {
            *target = Some(devices);
        }
    }

    pub fn configure_file_transfers(
        &self,
        manager: crate::device_links::core::transfer_manager::TransferManager,
        store: crate::device_links::core::transfer_manager::TransferCheckpointStore,
        cancellations: Arc<Mutex<std::collections::HashSet<String>>>,
        download_root: std::path::PathBuf,
    ) -> Result<(), String> {
        self.file_transfers
            .configure(manager, store, cancellations, download_root)
    }

    pub fn start_file_send(
        &self,
        device_id: &str,
        source_path: std::path::PathBuf,
        transfer_id: String,
        sessions: &DeviceManager,
        events: &EventBus,
    ) -> Result<(), String> {
        let web_rtc = sessions
            .current_webrtc_binding(device_id)
            .filter(|binding| sessions.is_current_webrtc(binding))
            .ok_or_else(|| "DeskLink WebRTC feature transport is unavailable".to_string())?;
        if !web_rtc.transport.features_allowed() {
            return Err("DeskLink WebRTC feature handover is incomplete".to_string());
        }
        let message = self.file_transfers.start_send(
            &web_rtc.transport.wire_binding,
            source_path,
            transfer_id,
            events,
        )?;
        self.send_file_messages(&web_rtc, vec![message])
    }

    pub fn cancel_file_transfer(
        &self,
        transfer_id: &str,
        sessions: &DeviceManager,
        events: &EventBus,
    ) -> Result<(), String> {
        let Some((wire, message)) = self.file_transfers.cancel(transfer_id, events)? else {
            return Ok(());
        };
        let web_rtc = sessions
            .current_webrtc_binding(&wire.peer_device_id)
            .filter(|binding| {
                sessions.is_current_webrtc(binding) && binding.transport.wire_binding == wire
            })
            .ok_or_else(|| "Transfer session is no longer connected".to_string())?;
        self.send_file_messages(&web_rtc, vec![message])
    }

    pub fn start_desktop_screen_for_binding(
        &self,
        binding: &SessionBinding,
        sessions: &DeviceManager,
        events: &EventBus,
    ) -> Result<(), String> {
        let web_rtc = sessions
            .current_webrtc_binding(&binding.device_id)
            .filter(|value| sessions.is_current_webrtc(value))
            .ok_or_else(|| "DeskLink WebRTC screen transport is unavailable".to_string())?;
        if !web_rtc.transport.features_allowed() {
            return Err("DeskLink WebRTC screen handover is incomplete".to_string());
        }
        let peer = self
            .attempts
            .lock()
            .map_err(|_| "WebRTC attempt map poisoned".to_string())?
            .get(&binding.device_id)
            .filter(|attempt| {
                attempt.binding.session_id == binding.session_id
                    && attempt.binding.generation == binding.generation
            })
            .map(|attempt| Arc::clone(&attempt.peer))
            .ok_or_else(|| "No current WebRTC peer for screen capture".to_string())?;
        peer.start_screen_capture()?;
        let ready = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SCREEN_READY);
        web_rtc
            .transport
            .send_packet(&ready, now_millis())
            .map_err(|error| error.to_string())?;
        self.publish_state(
            events,
            binding,
            "ScreenReady",
            json!({"transport": "webrtc-vp8"}),
        );
        Ok(())
    }

    pub fn stop_desktop_screen_for_binding(&self, binding: &SessionBinding) {
        if let Some(peer) = self.attempts.lock().ok().and_then(|attempts| {
            attempts
                .get(&binding.device_id)
                .filter(|attempt| {
                    attempt.binding.session_id == binding.session_id
                        && attempt.binding.generation == binding.generation
                })
                .map(|attempt| Arc::clone(&attempt.peer))
        }) {
            peer.stop_screen_capture();
        }
    }

    pub fn request_phone_file_roots(
        &self,
        device_id: &str,
        sessions: &DeviceManager,
        events: &EventBus,
    ) -> Result<String, String> {
        self.send_phone_file_request(
            device_id,
            PhoneFileRequest::roots(Uuid::new_v4().to_string()),
            sessions,
            events,
        )
    }

    pub fn request_phone_file_list(
        &self,
        device_id: &str,
        entry_id: String,
        sessions: &DeviceManager,
        events: &EventBus,
    ) -> Result<String, String> {
        self.send_phone_file_request(
            device_id,
            PhoneFileRequest::list(Uuid::new_v4().to_string(), entry_id),
            sessions,
            events,
        )
    }

    pub fn request_phone_file_download(
        &self,
        device_id: &str,
        entry_id: String,
        sessions: &DeviceManager,
        events: &EventBus,
    ) -> Result<String, String> {
        self.send_phone_file_request(
            device_id,
            PhoneFileRequest::download(
                Uuid::new_v4().to_string(),
                entry_id,
                Uuid::new_v4().to_string(),
            ),
            sessions,
            events,
        )
    }

    pub fn send_phone_file_request(
        &self,
        device_id: &str,
        request: PhoneFileRequest,
        sessions: &DeviceManager,
        events: &EventBus,
    ) -> Result<String, String> {
        request.validate().map_err(str::to_string)?;
        let web_rtc = sessions
            .current_webrtc_binding(device_id)
            .filter(|binding| sessions.is_current_webrtc(binding))
            .ok_or_else(|| "DeskLink WebRTC phone-file transport is unavailable".to_string())?;
        if !web_rtc.transport.features_allowed() {
            return Err("DeskLink WebRTC phone-file handover is incomplete".to_string());
        }
        let request_id = request.request_id.clone();
        self.browser_requests
            .lock()
            .map_err(|_| "Phone-file request map poisoned".to_string())?
            .insert(
                request_id.clone(),
                BrowserRequestBinding {
                    device_id: device_id.to_string(),
                    session_id: web_rtc.session_id,
                    generation: web_rtc.generation,
                    action: request.action,
                },
            );
        let payload = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        if let Err(error) = web_rtc.transport.enqueue_payload(
            &DataChannelSpec::FILE_CONTROL,
            PHONE_FILE_BROWSER_MESSAGE_TYPE,
            &payload,
            now_millis(),
        ) {
            if let Ok(mut requests) = self.browser_requests.lock() {
                requests.remove(&request_id);
            }
            return Err(error.to_string());
        }
        events.publish(CoreEvent::FeatureStateChanged {
            device_id: device_id.to_string(),
            feature: "phone-files".to_string(),
            state: "Requesting".to_string(),
            details: json!({"requestId": request_id, "action": request.action}),
        });
        Ok(request_id)
    }

    pub fn register_packet_sink(&self, binding: &SessionBinding, sink: WebRtcPacketSink) {
        if let Ok(mut sinks) = self.packet_sinks.lock() {
            sinks.insert(
                binding.device_id.clone(),
                PacketSinkRegistration {
                    session_id: binding.session_id,
                    generation: binding.generation,
                    sink,
                },
            );
        }
    }

    pub fn unregister_packet_sink(&self, binding: &SessionBinding) {
        if let Ok(mut sinks) = self.packet_sinks.lock() {
            let remove = sinks.get(&binding.device_id).is_some_and(|registration| {
                registration.session_id == binding.session_id
                    && registration.generation == binding.generation
            });
            if remove {
                sinks.remove(&binding.device_id);
            }
        }
    }

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
                    sessions.clone(),
                    events.clone(),
                )?;
                if let Ok(attempt) =
                    self.current_attempt(binding, &message.session_attempt_id, &sessions)
                {
                    if let Ok(mut handover) = attempt.handover.lock() {
                        handover.offer_sdp = Some(sdp.clone());
                    }
                }
                peer.accept_offer_and_create_answer(&sdp)?;
            }
            SignalingMessageType::Answer => {
                let sdp = required_string(&message.payload, "sdp", MAX_SDP_BYTES)?;
                let attempt =
                    self.current_attempt(binding, &message.session_attempt_id, &sessions)?;
                if let Ok(mut handover) = attempt.handover.lock() {
                    handover.answer_sdp = Some(sdp.clone());
                }
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
                self.current_attempt(binding, &message.session_attempt_id, &sessions)?;
                self.schedule_recovery(
                    binding.clone(),
                    message.session_attempt_id,
                    "The peer requested WebRTC recovery".to_string(),
                    false,
                    config,
                    sessions,
                    events,
                );
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
        let ice = config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())?
            .webrtc_ice_servers()?;
        let peer = Arc::new(PeerConnection::new_negotiated_with_ice(
            sink,
            &ice.stun_servers,
            &ice.turn_servers,
        )?);
        let attempt = NegotiationAttempt {
            binding: binding.clone(),
            attempt_id,
            peer,
            handover: Arc::new(Mutex::new(HandoverRuntime::default())),
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
                    handover: Arc::clone(&attempt.handover),
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
            handover: Arc::clone(&attempt.handover),
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
            PeerEvent::Offer(sdp) => {
                if let Ok(mut handover) = attempt.handover.lock() {
                    handover.offer_sdp = Some(sdp.clone());
                }
                self.send_signal(
                    &attempt.binding,
                    &attempt.attempt_id,
                    SignalingMessageType::Offer,
                    json!({"sdp": sdp}),
                    config,
                    &events,
                )
            }
            PeerEvent::Answer(sdp) => {
                if let Ok(mut handover) = attempt.handover.lock() {
                    handover.answer_sdp = Some(sdp.clone());
                }
                self.send_signal(
                    &attempt.binding,
                    &attempt.attempt_id,
                    SignalingMessageType::Answer,
                    json!({"sdp": sdp}),
                    config,
                    &events,
                )
            }
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
                let local_device_id = match config.lock() {
                    Ok(config) => config.local_device_info().id,
                    Err(_) => {
                        self.publish_error(
                            &events,
                            &attempt.binding,
                            "Could not read DeskLink identity for WebRTC handover".to_string(),
                        );
                        return;
                    }
                };
                let wire_binding = match WebRtcWireBinding::from_attempt(
                    local_device_id,
                    &attempt.binding.device_id,
                    &attempt.attempt_id,
                ) {
                    Ok(binding) => binding,
                    Err(error) => {
                        self.publish_error(&events, &attempt.binding, error);
                        return;
                    }
                };
                let transport = WebRtcTransport::from_peer(wire_binding, Arc::clone(&attempt.peer));
                match sessions.register_webrtc_transport(&attempt.binding, transport) {
                    Ok(registration) => {
                        self.publish_state(
                            &events,
                            &attempt.binding,
                            "Authenticating",
                            json!({"sessionAttemptId": attempt.attempt_id}),
                        );
                        if let Err(error) = self.begin_handover(
                            &attempt,
                            &registration.binding,
                            Arc::clone(&config),
                            &events,
                        ) {
                            self.publish_error(&events, &attempt.binding, error);
                        }
                    }
                    Err(error) => self.publish_error(
                        &events,
                        &attempt.binding,
                        format!("WebRTC control channel opened for a stale session: {error:?}"),
                    ),
                }
            }
            PeerEvent::VideoFrame {
                sequence,
                width,
                height,
                rgba,
            } => {
                if width == 0 || height == 0 || rgba.len() > 64 * 1024 * 1024 {
                    self.publish_error(
                        &events,
                        &attempt.binding,
                        "Rejected an invalid WebRTC screen frame".to_string(),
                    );
                } else {
                    match encode_rgba_png(width, height, &rgba) {
                        Ok(png) => {
                            if let Some(devices) =
                                self.devices.lock().ok().and_then(|devices| devices.clone())
                            {
                                update_screen_frame(
                                    &devices,
                                    &attempt.binding.device_id,
                                    ScreenFrame {
                                        width,
                                        height,
                                        sequence,
                                        timestamp_millis: now_millis(),
                                        png,
                                    },
                                );
                            }
                        }
                        Err(error) => {
                            self.publish_error(&events, &attempt.binding, error);
                            return;
                        }
                    }
                    self.publish_state(
                        &events,
                        &attempt.binding,
                        "ScreenFrame",
                        json!({"width": width, "height": height, "bytes": rgba.len()}),
                    );
                }
            }
            PeerEvent::Message { channel, bytes } => {
                let Some(web_rtc) = sessions.current_webrtc_binding(&attempt.binding.device_id)
                else {
                    self.publish_error(
                        &events,
                        &attempt.binding,
                        "WebRTC message arrived before transport registration".to_string(),
                    );
                    return;
                };
                if !sessions.is_current_webrtc(&web_rtc) {
                    return;
                }
                let envelope: MessageEnvelope = match serde_json::from_slice(&bytes) {
                    Ok(envelope) => envelope,
                    Err(error) => {
                        self.publish_error(
                            &events,
                            &attempt.binding,
                            format!("Malformed WebRTC envelope: {error}"),
                        );
                        return;
                    }
                };
                if (now_millis() - envelope.timestamp).abs() > 5 * 60 * 1000 {
                    self.publish_error(
                        &events,
                        &attempt.binding,
                        "Expired WebRTC feature message".to_string(),
                    );
                    return;
                }
                if let Err(error) = self.feature_replay_guard.accept(&envelope.message_id) {
                    self.publish_error(
                        &events,
                        &attempt.binding,
                        format!("Rejected replayed WebRTC feature message: {error}"),
                    );
                    return;
                }
                if channel == DataChannelSpec::CONTROL.label
                    && envelope.message_type == HANDOVER_MESSAGE_TYPE
                {
                    if let Err(error) = self.handle_handover_message(
                        &attempt,
                        &web_rtc,
                        &envelope,
                        Arc::clone(&config),
                        &events,
                    ) {
                        self.publish_error(&events, &attempt.binding, error);
                    }
                    return;
                }
                if envelope.message_type == FILE_CONTROL_MESSAGE_TYPE
                    || envelope.message_type == FILE_CHUNK_MESSAGE_TYPE
                {
                    if !web_rtc.transport.features_allowed() {
                        self.publish_error(
                            &events,
                            &attempt.binding,
                            "WebRTC file message arrived before feature handover".to_string(),
                        );
                        return;
                    }
                    if let Err(error) = envelope.validate(
                        &web_rtc.transport.wire_binding.peer_device_id,
                        web_rtc.transport.wire_binding.session_id,
                        web_rtc.transport.wire_binding.generation,
                    ) {
                        self.publish_error(&events, &attempt.binding, error.to_string());
                        return;
                    }
                    let result = envelope
                        .decode_payload()
                        .map_err(|error| error.to_string())
                        .and_then(|payload| {
                            if envelope.message_type == FILE_CONTROL_MESSAGE_TYPE {
                                if channel != DataChannelSpec::FILE_CONTROL.label {
                                    return Err("WebRTC file control arrived on the wrong channel"
                                        .to_string());
                                }
                                let control: FileTransferControl = serde_json::from_slice(&payload)
                                    .map_err(|error| {
                                        format!("Malformed WebRTC file control: {error}")
                                    })?;
                                self.file_transfers.handle_control(
                                    &web_rtc.transport.wire_binding,
                                    control,
                                    &events,
                                )
                            } else {
                                if channel != DataChannelSpec::FILE_DATA.label {
                                    return Err("WebRTC file chunk arrived on the wrong channel"
                                        .to_string());
                                }
                                self.file_transfers.handle_chunk(
                                    &web_rtc.transport.wire_binding,
                                    &payload,
                                    &events,
                                )
                            }
                        })
                        .and_then(|outgoing| self.send_file_messages(&web_rtc, outgoing));
                    if let Err(error) = result {
                        self.publish_error(&events, &attempt.binding, error);
                    }
                    return;
                }
                if envelope.message_type == PHONE_FILE_BROWSER_MESSAGE_TYPE {
                    if channel != DataChannelSpec::FILE_CONTROL.label {
                        self.publish_error(
                            &events,
                            &attempt.binding,
                            "Phone-file response arrived on the wrong channel".to_string(),
                        );
                        return;
                    }
                    let result = envelope
                        .validate(
                            &web_rtc.transport.wire_binding.peer_device_id,
                            web_rtc.transport.wire_binding.session_id,
                            web_rtc.transport.wire_binding.generation,
                        )
                        .map_err(|error| error.to_string())
                        .and_then(|()| envelope.decode_payload().map_err(|error| error.to_string()))
                        .and_then(|payload| {
                            let response: PhoneFileResponse = serde_json::from_slice(&payload)
                                .map_err(|error| {
                                    format!("Malformed phone-file response: {error}")
                                })?;
                            response.validate().map_err(str::to_string)?;
                            let pending = self
                                .browser_requests
                                .lock()
                                .map_err(|_| "Phone-file request map poisoned".to_string())?
                                .remove(&response.request_id)
                                .ok_or_else(|| {
                                    "Unsolicited or replayed phone-file response".to_string()
                                })?;
                            if pending.device_id != attempt.binding.device_id
                                || pending.session_id != web_rtc.session_id
                                || pending.generation != web_rtc.generation
                            {
                                return Err("Stale phone-file response binding".to_string());
                            }
                            let state = if response.ok { "Ready" } else { "Error" };
                            events.publish(CoreEvent::FeatureStateChanged {
                                device_id: attempt.binding.device_id.clone(),
                                feature: "phone-files".to_string(),
                                state: state.to_string(),
                                details: json!({
                                    "requestId": response.request_id,
                                    "action": pending.action,
                                    "result": response.result,
                                    "error": response.error,
                                }),
                            });
                            Ok(())
                        });
                    if let Err(error) = result {
                        self.publish_error(&events, &attempt.binding, error);
                    }
                    return;
                }
                let packet = match decode_feature_message(
                    &web_rtc.transport.wire_binding,
                    &channel,
                    &bytes,
                    web_rtc.transport.features_allowed(),
                ) {
                    Ok(packet) => packet,
                    Err(error) => {
                        self.publish_error(&events, &attempt.binding, error);
                        return;
                    }
                };
                let sink = self.packet_sinks.lock().ok().and_then(|sinks| {
                    let registration = sinks.get(&attempt.binding.device_id)?;
                    (registration.session_id == attempt.binding.session_id
                        && registration.generation == attempt.binding.generation)
                        .then(|| Arc::clone(&registration.sink))
                });
                match sink {
                    Some(sink) => {
                        if let Err(error) = sink(packet) {
                            self.publish_error(&events, &attempt.binding, error);
                        }
                    }
                    None => self.publish_error(
                        &events,
                        &attempt.binding,
                        "No current desktop packet dispatcher for WebRTC".to_string(),
                    ),
                }
            }
            PeerEvent::Connected => self.publish_state(
                &events,
                &attempt.binding,
                "Connected",
                json!({"sessionAttemptId": attempt.attempt_id}),
            ),
            PeerEvent::Disconnected => self.schedule_recovery(
                attempt.binding,
                attempt.attempt_id,
                "WebRTC peer disconnected".to_string(),
                true,
                config,
                sessions,
                events,
            ),
            PeerEvent::Failure(error) => self.schedule_recovery(
                attempt.binding,
                attempt.attempt_id,
                error,
                true,
                config,
                sessions,
                events,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn schedule_recovery(
        &self,
        binding: SessionBinding,
        attempt_id: String,
        reason: String,
        notify_peer: bool,
        config: Arc<Mutex<Config>>,
        sessions: DeviceManager,
        events: EventBus,
    ) {
        if !sessions.is_current(&binding) {
            return;
        }
        let local_device_id = match config.lock() {
            Ok(config) => config.local_device_info().id,
            Err(_) => {
                self.publish_error(&events, &binding, "Config lock poisoned".to_string());
                return;
            }
        };
        if let Some(web_rtc) = sessions.current_webrtc_binding(&binding.device_id) {
            let _ = web_rtc
                .transport
                .set_state(crate::device_links::webrtc::transport::TransportState::Degraded);
            let _ = self
                .file_transfers
                .pause_for_wire(&web_rtc.transport.wire_binding, &events);
        }
        self.publish_state(
            &events,
            &binding,
            "Degraded",
            json!({"reason": reason, "recoverable": true}),
        );

        let (claimed, delay) = {
            let Ok(mut recoveries) = self.recoveries.lock() else {
                self.publish_error(
                    &events,
                    &binding,
                    "Recovery state lock poisoned".to_string(),
                );
                return;
            };
            let state = recoveries.entry(binding.device_id.clone()).or_default();
            let claimed = state.claim();
            (
                claimed.is_some(),
                (local_device_id < binding.device_id)
                    .then_some(claimed)
                    .flatten(),
            )
        };
        if !claimed {
            return;
        }

        let coordinator = self.clone();
        std::thread::spawn(move || {
            if notify_peer {
                coordinator.send_signal(
                    &binding,
                    &attempt_id,
                    SignalingMessageType::IceRestart,
                    json!({}),
                    Arc::clone(&config),
                    &events,
                );
            }
            coordinator.close_for_binding(&binding, &sessions);

            let Some(delay) = delay else {
                coordinator.publish_state(&events, &binding, "WaitingForRecoveryOffer", json!({}));
                return;
            };
            std::thread::sleep(delay);
            if let Ok(mut recoveries) = coordinator.recoveries.lock() {
                if let Some(state) = recoveries.get_mut(&binding.device_id) {
                    state.release();
                }
            }
            let Some(current) = sessions.current_binding(&binding.device_id) else {
                return;
            };
            if current.session_id != binding.session_id
                || current.generation != binding.generation
                || !sessions.is_current(&current)
            {
                return;
            }
            coordinator.publish_state(
                &events,
                &current,
                "Recovering",
                json!({"delaySeconds": delay.as_secs()}),
            );
            if let Err(error) = coordinator.start_attempt(
                current.clone(),
                Arc::clone(&config),
                sessions.clone(),
                events.clone(),
            ) {
                coordinator
                    .schedule_recovery(current, attempt_id, error, false, config, sessions, events);
            }
        });
    }

    fn begin_handover(
        &self,
        attempt: &NegotiationAttempt,
        web_rtc: &WebRtcBinding,
        config: Arc<Mutex<Config>>,
        events: &EventBus,
    ) -> Result<(), String> {
        let local_device_id = config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())?
            .local_device_info()
            .id;
        if local_device_id >= attempt.binding.device_id {
            return Ok(());
        }
        let nonce = Uuid::new_v4().to_string();
        {
            let mut runtime = attempt
                .handover
                .lock()
                .map_err(|_| "WebRTC handover state poisoned".to_string())?;
            runtime.local_nonce = Some(nonce.clone());
        }
        let message = control_message(
            HandoverControlKind::Hello,
            attempt,
            web_rtc,
            &local_device_id,
            Some(nonce),
            None,
            now_millis(),
            None,
            Vec::new(),
            Vec::new(),
        );
        self.send_control(web_rtc, &message)?;
        self.publish_state(
            events,
            &attempt.binding,
            "Authenticating",
            json!({"role": "initiator"}),
        );
        Ok(())
    }

    fn handle_handover_message(
        &self,
        attempt: &NegotiationAttempt,
        web_rtc: &WebRtcBinding,
        envelope: &MessageEnvelope,
        config: Arc<Mutex<Config>>,
        events: &EventBus,
    ) -> Result<(), String> {
        envelope
            .validate(
                &web_rtc.transport.wire_binding.peer_device_id,
                web_rtc.transport.wire_binding.session_id,
                web_rtc.transport.wire_binding.generation,
            )
            .map_err(|error| error.to_string())?;
        if envelope.channel != DataChannelSpec::CONTROL.label {
            return Err("WebRTC handover message arrived outside the control channel".to_string());
        }
        let payload = envelope
            .decode_payload()
            .map_err(|error| error.to_string())?;
        let message: HandoverControlMessage = serde_json::from_slice(&payload)
            .map_err(|error| format!("Malformed WebRTC handover message: {error}"))?;
        message
            .validate(
                &attempt.attempt_id,
                &attempt.binding.device_id,
                web_rtc.transport.wire_binding.session_id,
                web_rtc.transport.wire_binding.generation,
                now_millis(),
            )
            .map_err(str::to_string)?;

        let (local_device_id, private_key_pem, local_incoming, local_outgoing) = {
            let config = config
                .lock()
                .map_err(|_| "Config lock poisoned".to_string())?;
            let info = config.local_device_info();
            (
                info.id,
                config
                    .key()
                    .private_key_to_pem_pkcs8()
                    .map_err(|error| error.to_string())?,
                feature_capabilities(info.incoming_capabilities),
                feature_capabilities(info.outgoing_capabilities),
            )
        };
        let local_is_initiator = local_device_id < attempt.binding.device_id;

        match message.kind {
            HandoverControlKind::Hello => {
                if local_is_initiator {
                    return Err("WebRTC initiator received an unexpected hello".to_string());
                }
                let initiator_nonce = message
                    .nonce
                    .clone()
                    .filter(|nonce| !nonce.is_empty())
                    .ok_or_else(|| "WebRTC hello has no nonce".to_string())?;
                let responder_nonce = Uuid::new_v4().to_string();
                let timestamp = now_millis();
                let signature = {
                    let mut runtime = attempt
                        .handover
                        .lock()
                        .map_err(|_| "WebRTC handover state poisoned".to_string())?;
                    runtime.remote_nonce = Some(initiator_nonce.clone());
                    runtime.local_nonce = Some(responder_nonce.clone());
                    runtime.authentication_timestamp = Some(timestamp);
                    build_authentication_transcript(
                        attempt,
                        web_rtc,
                        &local_device_id,
                        &runtime,
                        timestamp,
                    )?
                    .sign(&private_key_pem)
                    .map_err(|error| error.to_string())?
                };
                self.send_control(
                    web_rtc,
                    &control_message(
                        HandoverControlKind::Challenge,
                        attempt,
                        web_rtc,
                        &local_device_id,
                        Some(responder_nonce),
                        Some(initiator_nonce),
                        timestamp,
                        Some(base64::engine::general_purpose::STANDARD.encode(signature)),
                        Vec::new(),
                        Vec::new(),
                    ),
                )?;
            }
            HandoverControlKind::Challenge => {
                if !local_is_initiator {
                    return Err("WebRTC responder received an unexpected challenge".to_string());
                }
                let responder_nonce = required_control_value(&message.nonce, "challenge nonce")?;
                let initiator_nonce = required_control_value(&message.peer_nonce, "peer nonce")?;
                let signature = decode_signature(&message)?;
                let local_signature = {
                    let mut runtime = attempt
                        .handover
                        .lock()
                        .map_err(|_| "WebRTC handover state poisoned".to_string())?;
                    if runtime.local_nonce.as_deref() != Some(initiator_nonce.as_str()) {
                        return Err(
                            "WebRTC challenge nonce does not match the initiator".to_string()
                        );
                    }
                    runtime.remote_nonce = Some(responder_nonce.clone());
                    runtime.authentication_timestamp = Some(message.timestamp);
                    let transcript = build_authentication_transcript(
                        attempt,
                        web_rtc,
                        &local_device_id,
                        &runtime,
                        message.timestamp,
                    )?;
                    let remote_key =
                        PKey::public_key_from_der(&attempt.binding.link.remote_public_der)
                            .map_err(|error| format!("invalid paired peer key: {error}"))?;
                    transcript
                        .verify_with_key(&remote_key, &signature)
                        .map_err(|error| error.to_string())?;
                    runtime.peer_authenticated = true;
                    transcript
                        .sign(&private_key_pem)
                        .map_err(|error| error.to_string())?
                };
                self.send_control(
                    web_rtc,
                    &control_message(
                        HandoverControlKind::Response,
                        attempt,
                        web_rtc,
                        &local_device_id,
                        Some(initiator_nonce),
                        Some(responder_nonce),
                        message.timestamp,
                        Some(base64::engine::general_purpose::STANDARD.encode(local_signature)),
                        Vec::new(),
                        Vec::new(),
                    ),
                )?;
            }
            HandoverControlKind::Response => {
                if local_is_initiator {
                    return Err("WebRTC initiator received an unexpected response".to_string());
                }
                let initiator_nonce = required_control_value(&message.nonce, "response nonce")?;
                let responder_nonce = required_control_value(&message.peer_nonce, "peer nonce")?;
                let signature = decode_signature(&message)?;
                {
                    let mut runtime = attempt
                        .handover
                        .lock()
                        .map_err(|_| "WebRTC handover state poisoned".to_string())?;
                    if runtime.remote_nonce.as_deref() != Some(initiator_nonce.as_str())
                        || runtime.local_nonce.as_deref() != Some(responder_nonce.as_str())
                        || runtime.authentication_timestamp != Some(message.timestamp)
                    {
                        return Err(
                            "WebRTC response does not match the active challenge".to_string()
                        );
                    }
                    let transcript = build_authentication_transcript(
                        attempt,
                        web_rtc,
                        &local_device_id,
                        &runtime,
                        message.timestamp,
                    )?;
                    let remote_key =
                        PKey::public_key_from_der(&attempt.binding.link.remote_public_der)
                            .map_err(|error| format!("invalid paired peer key: {error}"))?;
                    transcript
                        .verify_with_key(&remote_key, &signature)
                        .map_err(|error| error.to_string())?;
                    runtime.peer_authenticated = true;
                }
                web_rtc
                    .transport
                    .advance_handover(HandoverMessage::Authenticated)
                    .map_err(|error| error.to_string())?;
                self.send_control(
                    web_rtc,
                    &control_message(
                        HandoverControlKind::Authenticated,
                        attempt,
                        web_rtc,
                        &local_device_id,
                        None,
                        None,
                        now_millis(),
                        None,
                        Vec::new(),
                        Vec::new(),
                    ),
                )?;
                self.send_capabilities(
                    attempt,
                    web_rtc,
                    &local_device_id,
                    local_incoming,
                    local_outgoing,
                )?;
            }
            HandoverControlKind::Authenticated => {
                if !local_is_initiator {
                    return Err(
                        "WebRTC responder received an unexpected authentication acknowledgement"
                            .to_string(),
                    );
                }
                let peer_authenticated = attempt
                    .handover
                    .lock()
                    .map_err(|_| "WebRTC handover state poisoned".to_string())?
                    .peer_authenticated;
                if !peer_authenticated {
                    return Err(
                        "WebRTC authentication acknowledgement arrived before verification"
                            .to_string(),
                    );
                }
                web_rtc
                    .transport
                    .advance_handover(HandoverMessage::Authenticated)
                    .map_err(|error| error.to_string())?;
                self.send_capabilities(
                    attempt,
                    web_rtc,
                    &local_device_id,
                    local_incoming,
                    local_outgoing,
                )?;
            }
            HandoverControlKind::Capabilities => {
                verify_remote_capabilities(&attempt.binding, &message)?;
                {
                    let mut runtime = attempt
                        .handover
                        .lock()
                        .map_err(|_| "WebRTC handover state poisoned".to_string())?;
                    if runtime.remote_capabilities_received {
                        return Err("Duplicate WebRTC capability confirmation".to_string());
                    }
                    runtime.remote_capabilities_received = true;
                }
                web_rtc
                    .transport
                    .advance_handover(HandoverMessage::Capabilities)
                    .map_err(|error| error.to_string())?;
                self.send_feature_ready(attempt, web_rtc, &local_device_id)?;
            }
            HandoverControlKind::FeatureReady => {
                let should_activate = {
                    let mut runtime = attempt
                        .handover
                        .lock()
                        .map_err(|_| "WebRTC handover state poisoned".to_string())?;
                    runtime.remote_feature_ready_received = true;
                    runtime.local_feature_ready_sent && runtime.remote_feature_ready_received
                };
                if should_activate {
                    web_rtc
                        .transport
                        .advance_handover(HandoverMessage::FeatureReady)
                        .map_err(|error| error.to_string())?;
                    let mut notification_resync = NetworkPacket::new(
                        crate::device_links::packet::PACKET_TYPE_NOTIFICATION_REQUEST,
                    );
                    notification_resync.set("request", true);
                    web_rtc
                        .transport
                        .send_packet(&notification_resync, now_millis())
                        .map_err(|error| error.to_string())?;
                    let resumable = self
                        .file_transfers
                        .resume_sends(&web_rtc.transport.wire_binding, events)?;
                    self.send_file_messages(web_rtc, resumable)?;
                    if let Ok(mut recoveries) = self.recoveries.lock() {
                        recoveries.remove(&attempt.binding.device_id);
                    }
                    self.publish_state(
                        events,
                        &attempt.binding,
                        "FeatureReady",
                        json!({
                            "transportId": web_rtc.transport.transport_id,
                            "lanRole": "bootstrap-signaling-only"
                        }),
                    );
                }
            }
            HandoverControlKind::Degraded => {
                web_rtc
                    .transport
                    .advance_handover(HandoverMessage::Degraded)
                    .map_err(|error| error.to_string())?;
                self.publish_state(events, &attempt.binding, "Degraded", json!({}));
            }
            HandoverControlKind::Close => {
                let _ = web_rtc.transport.advance_handover(HandoverMessage::Close);
                web_rtc.transport.close();
            }
        }
        Ok(())
    }

    fn send_capabilities(
        &self,
        attempt: &NegotiationAttempt,
        web_rtc: &WebRtcBinding,
        local_device_id: &str,
        incoming_capabilities: Vec<String>,
        outgoing_capabilities: Vec<String>,
    ) -> Result<(), String> {
        {
            let runtime = attempt
                .handover
                .lock()
                .map_err(|_| "WebRTC handover state poisoned".to_string())?;
            if runtime.local_capabilities_sent {
                return Ok(());
            }
        }
        self.send_control(
            web_rtc,
            &control_message(
                HandoverControlKind::Capabilities,
                attempt,
                web_rtc,
                local_device_id,
                None,
                None,
                now_millis(),
                None,
                incoming_capabilities,
                outgoing_capabilities,
            ),
        )?;
        attempt
            .handover
            .lock()
            .map_err(|_| "WebRTC handover state poisoned".to_string())?
            .local_capabilities_sent = true;
        Ok(())
    }

    fn send_feature_ready(
        &self,
        attempt: &NegotiationAttempt,
        web_rtc: &WebRtcBinding,
        local_device_id: &str,
    ) -> Result<(), String> {
        self.send_control(
            web_rtc,
            &control_message(
                HandoverControlKind::FeatureReady,
                attempt,
                web_rtc,
                local_device_id,
                None,
                None,
                now_millis(),
                None,
                Vec::new(),
                Vec::new(),
            ),
        )?;
        attempt
            .handover
            .lock()
            .map_err(|_| "WebRTC handover state poisoned".to_string())?
            .local_feature_ready_sent = true;
        Ok(())
    }

    fn send_control(
        &self,
        web_rtc: &WebRtcBinding,
        message: &HandoverControlMessage,
    ) -> Result<(), String> {
        let payload = serde_json::to_vec(message).map_err(|error| error.to_string())?;
        web_rtc
            .transport
            .enqueue_payload(
                &DataChannelSpec::CONTROL,
                HANDOVER_MESSAGE_TYPE,
                &payload,
                now_millis(),
            )
            .map_err(|error| error.to_string())
    }

    fn send_file_messages(
        &self,
        web_rtc: &WebRtcBinding,
        messages: Vec<OutboundFileMessage>,
    ) -> Result<(), String> {
        for message in messages {
            match message {
                OutboundFileMessage::Control(control) => {
                    let payload =
                        serde_json::to_vec(&control).map_err(|error| error.to_string())?;
                    web_rtc
                        .transport
                        .enqueue_payload(
                            &DataChannelSpec::FILE_CONTROL,
                            FILE_CONTROL_MESSAGE_TYPE,
                            &payload,
                            now_millis(),
                        )
                        .map_err(|error| error.to_string())?;
                }
                OutboundFileMessage::Chunk(chunk) => web_rtc
                    .transport
                    .enqueue_payload(
                        &DataChannelSpec::FILE_DATA,
                        FILE_CHUNK_MESSAGE_TYPE,
                        &chunk,
                        now_millis(),
                    )
                    .map_err(|error| error.to_string())?,
            }
        }
        Ok(())
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

#[allow(clippy::too_many_arguments)]
fn control_message(
    kind: HandoverControlKind,
    attempt: &NegotiationAttempt,
    web_rtc: &WebRtcBinding,
    local_device_id: &str,
    nonce: Option<String>,
    peer_nonce: Option<String>,
    timestamp: i64,
    signature_base64: Option<String>,
    incoming_capabilities: Vec<String>,
    outgoing_capabilities: Vec<String>,
) -> HandoverControlMessage {
    HandoverControlMessage {
        handover_version: HANDOVER_VERSION,
        kind,
        session_attempt_id: attempt.attempt_id.clone(),
        device_id: local_device_id.to_string(),
        session_id: web_rtc.transport.wire_binding.session_id,
        connection_generation: web_rtc.transport.wire_binding.generation,
        nonce,
        peer_nonce,
        timestamp,
        signature_base64,
        incoming_capabilities,
        outgoing_capabilities,
    }
}

fn feature_capabilities(capabilities: Vec<String>) -> Vec<String> {
    capabilities
        .into_iter()
        .filter(|capability| capability != PACKET_TYPE_WEBRTC_SIGNAL_V1)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn verify_remote_capabilities(
    binding: &SessionBinding,
    message: &HandoverControlMessage,
) -> Result<(), String> {
    let expected_incoming = feature_capabilities(binding.link.info.incoming_capabilities.clone());
    let expected_outgoing = feature_capabilities(binding.link.info.outgoing_capabilities.clone());
    let incoming = message
        .incoming_capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let outgoing = message
        .outgoing_capabilities
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if incoming.len() != message.incoming_capabilities.len()
        || outgoing.len() != message.outgoing_capabilities.len()
        || incoming.into_iter().collect::<Vec<_>>() != expected_incoming
        || outgoing.into_iter().collect::<Vec<_>>() != expected_outgoing
    {
        return Err(
            "WebRTC capabilities do not match the authenticated device identity".to_string(),
        );
    }
    Ok(())
}

fn required_control_value(value: &Option<String>, label: &str) -> Result<String, String> {
    value
        .clone()
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .ok_or_else(|| format!("WebRTC handover has no valid {label}"))
}

fn decode_signature(message: &HandoverControlMessage) -> Result<Vec<u8>, String> {
    let encoded = message
        .signature_base64
        .as_deref()
        .ok_or_else(|| "WebRTC authentication signature is missing".to_string())?;
    if encoded.len() > 16 * 1024 {
        return Err("WebRTC authentication signature is too large".to_string());
    }
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "WebRTC authentication signature is invalid base64".to_string())
}

fn build_authentication_transcript(
    attempt: &NegotiationAttempt,
    web_rtc: &WebRtcBinding,
    local_device_id: &str,
    runtime: &HandoverRuntime,
    timestamp: i64,
) -> Result<AuthenticationTranscript, String> {
    let offer = runtime
        .offer_sdp
        .as_deref()
        .ok_or_else(|| "WebRTC authentication is missing the offer SDP".to_string())?;
    let answer = runtime
        .answer_sdp
        .as_deref()
        .ok_or_else(|| "WebRTC authentication is missing the answer SDP".to_string())?;
    let local_nonce = runtime
        .local_nonce
        .clone()
        .ok_or_else(|| "WebRTC authentication is missing the local nonce".to_string())?;
    let remote_nonce = runtime
        .remote_nonce
        .clone()
        .ok_or_else(|| "WebRTC authentication is missing the peer nonce".to_string())?;
    let local_is_initiator = local_device_id < attempt.binding.device_id.as_str();
    Ok(AuthenticationTranscript {
        session_attempt_id: attempt.attempt_id.clone(),
        initiator_device_id: if local_is_initiator {
            local_device_id.to_string()
        } else {
            attempt.binding.device_id.clone()
        },
        responder_device_id: if local_is_initiator {
            attempt.binding.device_id.clone()
        } else {
            local_device_id.to_string()
        },
        session_id: web_rtc.transport.wire_binding.session_id,
        connection_generation: web_rtc.transport.wire_binding.generation,
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
        offer_sha256: sha256_hex(offer.as_bytes()),
        answer_sha256: sha256_hex(answer.as_bytes()),
        initiator_dtls_fingerprint: dtls_fingerprint(offer)?,
        responder_dtls_fingerprint: dtls_fingerprint(answer)?,
        protocol_version: 1,
        timestamp,
    })
}

fn sha256_hex(value: &[u8]) -> String {
    sha256(value)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn dtls_fingerprint(sdp: &str) -> Result<String, String> {
    sdp.lines()
        .find_map(|line| {
            line.trim_end_matches('\r')
                .strip_prefix("a=fingerprint:sha-256 ")
                .map(str::to_string)
        })
        .filter(|fingerprint| !fingerprint.is_empty() && fingerprint.len() <= 256)
        .ok_or_else(|| "WebRTC SDP has no SHA-256 DTLS fingerprint".to_string())
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

fn encode_rgba_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let expected = usize::try_from(width)
        .ok()
        .and_then(|width| {
            usize::try_from(height)
                .ok()
                .and_then(|height| width.checked_mul(height))
        })
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "WebRTC screen dimensions overflowed".to_string())?;
    if expected != rgba.len() || expected > 64 * 1024 * 1024 {
        return Err("WebRTC screen frame has an invalid RGBA payload".to_string());
    }
    let mut output = Vec::new();
    let mut encoder = png::Encoder::new(&mut output, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder
        .write_header()
        .map_err(|error| format!("Could not encode WebRTC screen frame: {error}"))?;
    writer
        .write_image_data(rgba)
        .map_err(|error| format!("Could not encode WebRTC screen frame: {error}"))?;
    drop(writer);
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::webrtc::packet_bridge::encode_packet;
    use crate::device_links::webrtc::wire_binding::WebRtcWireBinding;

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

    #[test]
    fn feature_message_requires_ready_handover_and_matching_channel() {
        let sender = WebRtcWireBinding::from_attempt(
            "phone",
            "desktop",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let receiver = WebRtcWireBinding::from_attempt(
            "desktop",
            "phone",
            "01234567-89ab-cdef-0123-456789abcdef",
        )
        .unwrap();
        let packet = NetworkPacket::new("desklink.ping");
        let envelope = encode_packet(&sender, &packet, now_millis()).unwrap();
        let bytes = serde_json::to_vec(&envelope).unwrap();

        assert!(decode_feature_message(&receiver, &envelope.channel, &bytes, false).is_err());
        assert!(decode_feature_message(
            &receiver,
            crate::device_links::webrtc::channel::DataChannelSpec::CONTROL.label,
            &bytes,
            true,
        )
        .is_err());
        assert_eq!(
            decode_feature_message(&receiver, &envelope.channel, &bytes, true)
                .unwrap()
                .packet_type,
            "desklink.ping"
        );
    }
}
