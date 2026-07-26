//! Desktop WebRTC bootstrap coordinator.
//!
//! This layer accepts only paired, signed LAN bootstrap signaling and translates
//! `webrtcbin` events back into those signed records. Feature envelopes are
//! deliberately handled by the shared dispatcher, not here.

use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use openssl::pkey::PKey;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, SessionBinding};
use crate::device_links::daemon::network::send_packet;
use crate::device_links::packet::NetworkPacket;

use super::{
    DesktopWebRtcPeer, PeerEvent, SignalingMessageType, WebRtcSignalingMessage, WebRtcWireBinding,
};

/// Starts the deterministic desktop-initiated negotiation. The caller must
/// first confirm that the paired peer advertised signed WebRTC signaling.
pub(crate) fn start_initiator(
    binding: SessionBinding,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
) -> Result<(), String> {
    let (local_device_id, remote_device_id) = local_and_remote_device_ids(&binding, &config)?;
    if local_device_id >= remote_device_id {
        return Ok(());
    }
    if !sessions.is_current(&binding) {
        return Err("Stale DeskLink session cannot start WebRTC".to_string());
    }
    let attempt_id = Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt(local_device_id, remote_device_id, &attempt_id)?;
    let (sender, receiver) = mpsc::channel();
    let peer = DesktopWebRtcPeer::new(true, sender)?;
    let replaced = sessions
        .install_webrtc_peer(&binding, attempt_id.clone(), peer)
        .map_err(|error| error.to_string())?;
    if let Some(replaced) = replaced {
        replaced.close();
    }
    spawn_event_worker(
        binding.clone(),
        attempt_id.clone(),
        wire,
        receiver,
        Arc::clone(&sessions),
        config,
    );
    let peer = sessions
        .active_webrtc_peer(&binding, &attempt_id)
        .map_err(|error| error.to_string())?;
    peer.create_offer()
}

/// Processes one signed LAN bootstrap SDP/ICE record. All ordinary feature
/// packets remain outside this path and are never accepted here.
pub(crate) fn handle_signaling_packet(
    binding: &SessionBinding,
    packet: &NetworkPacket,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
) -> Result<(), String> {
    if !sessions.is_current(binding) {
        return Err("Stale DeskLink session rejected WebRTC signaling".to_string());
    }
    let message = WebRtcSignalingMessage::from_network_packet(packet)?;
    let local_device_id = config
        .lock()
        .map_err(|_| "DeskLink configuration lock poisoned".to_string())?
        .local_device_info()
        .id;
    message.validate_for(&local_device_id, now_millis())?;
    if message.from_device_id != binding.device_id {
        return Err(
            "DeskLink WebRTC signaling sender does not match the paired device".to_string(),
        );
    }
    let remote_key = PKey::public_key_from_der(&binding.link.remote_public_der)
        .map_err(|error| error.to_string())?;
    if !message.verify(&remote_key) {
        return Err("Invalid paired identity signature on DeskLink WebRTC signaling".to_string());
    }

    let peer = match message.message_type {
        SignalingMessageType::Offer => {
            if local_device_id <= message.from_device_id {
                return Err(
                    "Rejected unexpected DeskLink WebRTC offer from non-initiator".to_string(),
                );
            }
            install_responder(
                binding.clone(),
                message.session_attempt_id.clone(),
                &local_device_id,
                &message.from_device_id,
                Arc::clone(&sessions),
                Arc::clone(&config),
            )?
        }
        _ => sessions
            .active_webrtc_peer(binding, &message.session_attempt_id)
            .map_err(|error| error.to_string())?,
    };

    sessions
        .accept_webrtc_signal(
            binding,
            &message.session_attempt_id,
            message.request_id.clone(),
        )
        .map_err(|error| error.to_string())?;
    match message.message_type {
        SignalingMessageType::Offer | SignalingMessageType::Answer => {
            let sdp = message
                .payload
                .get("sdp")
                .and_then(Value::as_str)
                .ok_or_else(|| "Missing DeskLink WebRTC SDP".to_string())?;
            peer.set_remote_description(message.message_type, sdp)
        }
        SignalingMessageType::IceCandidate => {
            let index = message
                .payload
                .get("sdpMLineIndex")
                .and_then(Value::as_u64)
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| "Missing DeskLink WebRTC ICE m-line index".to_string())?;
            let candidate = message
                .payload
                .get("candidate")
                .and_then(Value::as_str)
                .ok_or_else(|| "Missing DeskLink WebRTC ICE candidate".to_string())?;
            peer.add_ice_candidate(index, candidate)
        }
        SignalingMessageType::EndOfCandidates => Ok(()),
        SignalingMessageType::IceRestart => Err(
            "DeskLink WebRTC ICE restart is not enabled until authenticated handover is active"
                .to_string(),
        ),
        SignalingMessageType::Close => {
            peer.close();
            Ok(())
        }
    }
}

fn install_responder(
    binding: SessionBinding,
    attempt_id: String,
    local_device_id: &str,
    remote_device_id: &str,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
) -> Result<Arc<DesktopWebRtcPeer>, String> {
    let wire = WebRtcWireBinding::from_attempt(local_device_id, remote_device_id, &attempt_id)?;
    let (sender, receiver) = mpsc::channel();
    let peer = DesktopWebRtcPeer::new(false, sender)?;
    let replaced = sessions
        .install_webrtc_peer(&binding, attempt_id.clone(), peer)
        .map_err(|error| error.to_string())?;
    if let Some(replaced) = replaced {
        replaced.close();
    }
    spawn_event_worker(
        binding.clone(),
        attempt_id.clone(),
        wire,
        receiver,
        Arc::clone(&sessions),
        config,
    );
    sessions
        .active_webrtc_peer(&binding, &attempt_id)
        .map_err(|error| error.to_string())
}

fn spawn_event_worker(
    binding: SessionBinding,
    attempt_id: String,
    wire: WebRtcWireBinding,
    receiver: Receiver<PeerEvent>,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
) {
    thread::spawn(move || {
        while let Ok(event) = receiver.recv() {
            if !sessions.is_current(&binding)
                || sessions.active_webrtc_peer(&binding, &attempt_id).is_err()
            {
                break;
            }
            match event {
                PeerEvent::LocalDescription { message_type, sdp } => {
                    let mut payload = Map::new();
                    payload.insert("sdp".to_string(), Value::String(sdp));
                    if let Err(error) = send_signed_signal(
                        &binding,
                        &wire,
                        &attempt_id,
                        message_type,
                        payload,
                        &config,
                    ) {
                        eprintln!("[DeskLink] WebRTC local description failed: {error}");
                        break;
                    }
                }
                PeerEvent::IceCandidate {
                    sdp_m_line_index,
                    candidate,
                } => {
                    let mut payload = Map::new();
                    payload.insert(
                        "sdpMLineIndex".to_string(),
                        Value::from(i64::from(sdp_m_line_index)),
                    );
                    payload.insert("candidate".to_string(), Value::String(candidate));
                    if let Err(error) = send_signed_signal(
                        &binding,
                        &wire,
                        &attempt_id,
                        SignalingMessageType::IceCandidate,
                        payload,
                        &config,
                    ) {
                        eprintln!("[DeskLink] WebRTC ICE signaling failed: {error}");
                        break;
                    }
                }
                PeerEvent::EndOfCandidates => {
                    let _ = send_signed_signal(
                        &binding,
                        &wire,
                        &attempt_id,
                        SignalingMessageType::EndOfCandidates,
                        Map::new(),
                        &config,
                    );
                }
                PeerEvent::Envelope { .. } => {
                    // The next shared-dispatcher slice consumes envelopes here.
                    // Do not pass them to the LAN feature reader.
                    eprintln!("[DeskLink] Received WebRTC data before feature handover is active");
                }
                PeerEvent::Error(error) => eprintln!("[DeskLink] WebRTC peer error: {error}"),
                PeerEvent::ConnectionChanged(state) => {
                    eprintln!("[DeskLink] WebRTC peer state: {state}")
                }
                PeerEvent::ChannelOpened(_) => {}
                PeerEvent::Closed => break,
            }
        }
    });
}

fn send_signed_signal(
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
    send_packet(binding.link.stream()?, &packet)
}

fn local_and_remote_device_ids(
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

fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
