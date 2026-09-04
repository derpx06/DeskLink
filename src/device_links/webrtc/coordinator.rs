//! Desktop WebRTC bootstrap coordinator.
//!
//! This layer accepts only paired, signed LAN bootstrap signaling and translates
//! `webrtcbin` events back into those signed records. Feature envelopes are
//! deliberately handled by the shared dispatcher, not here.

use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use openssl::hash::{hash, MessageDigest};
use openssl::pkey::PKey;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, FeatureTransportSnapshot, SessionBinding};
use crate::device_links::daemon::handle::DaemonEvent;
use crate::device_links::daemon::state::clear_error;
use crate::device_links::device::DeviceView;
use crate::device_links::features::{is_webrtc_feature, webrtc_capabilities};
use crate::device_links::packet::NetworkPacket;

use super::file_protocol::{FileControl, FILE_CHUNK_MESSAGE_TYPE, FILE_CONTROL_MESSAGE_TYPE};
use super::ice::IceServerConfig;
use super::transfer_manager::{TransferEvent, WebRtcTransferManager};
use super::{
    AuthenticationTranscript, DesktopWebRtcPeer, HandoverControlKind, HandoverControlMessage,
    PeerEvent, SignalingMessageType, WebRtcChannel, WebRtcEnvelope, WebRtcPacketBridge,
    WebRtcSignalingMessage, WebRtcWireBinding, HANDOVER_MESSAGE_TYPE,
};
use crate::device_links::daemon::state::mark_error;

/// ICE configuration for one attempt against `binding`'s peer.
///
/// The local address is whatever the link reports about itself. For the LAN
/// bootstrap that is the TLS socket's address, which by construction has a
/// working route to this peer. A transport that is *not* the L3 path the media
/// will take — Bluetooth, or a relayed link — reports `None`, and must: a host
/// candidate with no route to the peer costs ICE its full timeout before it is
/// abandoned. See [`IceServerConfig::local_ip`] for why this matters far more
/// than STUN here.
fn ice_config_for(binding: &SessionBinding, config: &Arc<Mutex<Config>>) -> IceServerConfig {
    let mut ice = config
        .lock()
        .ok()
        .map(|config| config.ice_servers())
        .unwrap_or_default();
    ice.local_ip = binding.link.local_ip();
    ice
}

/// Starts the deterministic desktop-initiated negotiation, or asks the
/// deterministic remote initiator to rebuild its peer after this desktop
/// process has restarted. The restart request is signed bootstrap signaling;
/// it never carries paired feature traffic over LAN.
pub(crate) fn start_initiator(
    binding: SessionBinding,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    let (local_device_id, remote_device_id) = local_and_remote_device_ids(&binding, &config)?;
    if local_device_id >= remote_device_id {
        let request_id = Uuid::new_v4().to_string();
        let wire = WebRtcWireBinding::from_attempt(local_device_id, remote_device_id, &request_id)?;
        return send_signed_signal(
            &binding,
            &wire,
            &request_id,
            SignalingMessageType::RestartRequest,
            Map::new(),
            &config,
        );
    }
    if !sessions.is_current(&binding) {
        return Err("Stale DeskLink session cannot start WebRTC".to_string());
    }
    let already_negotiating = sessions
        .with_session(&binding, |session| session.active_webrtc.is_some())
        .map_err(|error| error.to_string())?;
    if already_negotiating {
        return Ok(());
    }
    let attempt_id = Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt(local_device_id, remote_device_id, &attempt_id)?;
    let transfer_manager = new_transfer_manager(&config)?;
    let (sender, receiver) = mpsc::channel();
    let peer = DesktopWebRtcPeer::new(true, sender, &ice_config_for(&binding, &config))?;
    let replaced = sessions
        .install_webrtc_peer(
            &binding,
            attempt_id.clone(),
            wire.clone(),
            peer,
            Arc::clone(&transfer_manager),
        )
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
        devices,
        events,
        transfer_manager,
    )?;
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
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    if !sessions.is_current(binding) {
        return Err("Stale DeskLink session rejected WebRTC signaling".to_string());
    }
    let message = WebRtcSignalingMessage::from_network_packet(packet)?;
    eprintln!(
        "[DL-WRTC-BOOT] handling {:?} from {}",
        message.message_type, message.from_device_id
    );
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
            match offer_attempt_disposition(
                sessions
                    .active_webrtc_attempt(binding)
                    .map_err(|error| error.to_string())?
                    .as_deref(),
                &message.session_attempt_id,
            )? {
                OfferAttemptDisposition::ReuseCurrent => sessions
                    .active_webrtc_peer(binding, &message.session_attempt_id)
                    .map_err(|error| error.to_string())?,
                OfferAttemptDisposition::InstallNew => install_responder(
                    binding.clone(),
                    message.session_attempt_id.clone(),
                    &local_device_id,
                    &message.from_device_id,
                    Arc::clone(&sessions),
                    Arc::clone(&config),
                    Arc::clone(&devices),
                    Arc::clone(&events),
                )?,
            }
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
            sessions.with_webrtc_handover(binding, &message.session_attempt_id, |handover| {
                match message.message_type {
                    SignalingMessageType::Offer => handover.record_offer(sdp.to_string()),
                    SignalingMessageType::Answer => handover.record_answer(sdp.to_string()),
                    _ => unreachable!("matched offer or answer above"),
                }
            })?;
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
        SignalingMessageType::EndOfCandidates => {
            // Forwarded, not dropped. The peer telling us it has finished
            // gathering is the only thing that lets our ICE agent conclude its
            // check list; discarding it left a peer whose candidates all failed
            // sitting in `checking` forever instead of reaching `failed`.
            //
            // Index 0 because the session is max-bundle: every media section
            // shares one ICE component, so there is exactly one gathering
            // agent to tell.
            peer.end_of_candidates(0);
            Ok(())
        }
        SignalingMessageType::IceRestart => Err(
            "DeskLink WebRTC ICE restart is not enabled until authenticated handover is active"
                .to_string(),
        ),
        SignalingMessageType::RestartRequest => {
            Err("Unexpected DeskLink WebRTC restart request on the responder".to_string())
        }
        SignalingMessageType::Close => {
            peer.close();
            Ok(())
        }
    }
}

/// Sends a paired feature through the authenticated WebRTC transport once the
/// session has reached mutual `feature-ready`. LAN is never used for paired
/// file data; the handover responder below must complete before a transfer can
/// start.
pub(crate) fn send_feature_packet(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    packet: &NetworkPacket,
) -> Result<(), String> {
    if !sessions.is_current(&transport.binding) {
        return Err("DeskLink session was replaced before feature send".to_string());
    }
    if !transport.paired {
        return Err("Device must be paired before sending DeskLink features".to_string());
    }
    if !transport.web_rtc_ready {
        return Err(
            "DeskLink WebRTC feature transport is not ready; paired features are not sent over LAN"
                .to_string(),
        );
    }
    if !is_webrtc_feature(&packet.packet_type) {
        return Err(format!(
            "{} is not enabled in the DeskLink WebRTC feature profile",
            packet.packet_type
        ));
    }
    let registry = crate::device_links::features::FeatureRegistry::desktop();
    if !registry
        .outgoing_capabilities()
        .iter()
        .any(|capability| capability == &packet.packet_type)
    {
        return Err(format!(
            "DeskLink has no outgoing handler for {}",
            packet.packet_type
        ));
    }
    if !transport
        .binding
        .link
        .info
        .incoming_capabilities
        .iter()
        .any(|capability| capability == &packet.packet_type)
    {
        return Err(format!(
            "The paired device does not accept {}",
            packet.packet_type
        ));
    }
    let peer = transport
        .web_rtc_peer
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC handover has no active peer".to_string())?;
    let wire = transport
        .web_rtc_wire
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC handover has no wire binding".to_string())?;
    let envelope = WebRtcPacketBridge::encode(wire, packet, now_millis())?;
    let text = String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink WebRTC feature serialization was not UTF-8".to_string())?;
    peer.send_text(WebRtcPacketBridge::channel_for(packet), &text)
}

/// Starts a desktop-originated transfer through the current authenticated
/// WebRTC peer. It has no LAN fallback once feature-ready is active.
pub(crate) fn start_file_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    source: &std::path::Path,
) -> Result<(), String> {
    if !sessions.is_current(&transport.binding) || !transport.web_rtc_ready {
        return Err("DeskLink WebRTC file transport is unavailable".to_string());
    }
    let peer = transport
        .web_rtc_peer
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC file transfer has no active peer".to_string())?;
    let wire = transport
        .web_rtc_wire
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC file transfer has no wire binding".to_string())?;
    let manager = transport
        .transfer_manager
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC file transfer manager is unavailable".to_string())?;
    send_transfer_events(peer, wire, manager.start_send(wire, source)?)
}

#[allow(clippy::too_many_arguments)]
fn install_responder(
    binding: SessionBinding,
    attempt_id: String,
    local_device_id: &str,
    remote_device_id: &str,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<Arc<DesktopWebRtcPeer>, String> {
    let wire = WebRtcWireBinding::from_attempt(local_device_id, remote_device_id, &attempt_id)?;
    let transfer_manager = new_transfer_manager(&config)?;
    let (sender, receiver) = mpsc::channel();
    let peer = DesktopWebRtcPeer::new(false, sender, &ice_config_for(&binding, &config))?;
    let replaced = sessions
        .install_webrtc_peer(
            &binding,
            attempt_id.clone(),
            wire.clone(),
            peer,
            Arc::clone(&transfer_manager),
        )
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
        devices,
        events,
        transfer_manager,
    )?;
    sessions
        .active_webrtc_peer(&binding, &attempt_id)
        .map_err(|error| error.to_string())
}

#[allow(clippy::too_many_arguments)] // Captures one immutable peer/session context.
fn spawn_event_worker(
    binding: SessionBinding,
    attempt_id: String,
    wire: WebRtcWireBinding,
    receiver: Receiver<PeerEvent>,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
    transfer_manager: Arc<WebRtcTransferManager>,
) -> Result<(), String> {
    thread::spawn(move || {
        // Held for the worker's whole life; see `PeerWorkerGuard`.
        let _guard = PeerWorkerGuard {
            binding: binding.clone(),
            attempt_id: attempt_id.clone(),
            sessions: Arc::clone(&sessions),
        };
        let mut disconnected_since: Option<std::time::Instant> = None;
        loop {
            // A timed receive lets the transient-disconnect grace window expire
            // even when the peer has gone completely silent.
            let event = match receiver.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if !sessions.is_current(&binding)
                || sessions.active_webrtc_peer(&binding, &attempt_id).is_err()
            {
                break;
            }
            if let Some(event) = event {
                match event {
                    PeerEvent::LocalDescription { message_type, sdp } => {
                        if let Err(error) =
                            sessions.with_webrtc_handover(&binding, &attempt_id, |handover| {
                                match message_type {
                                    SignalingMessageType::Offer => {
                                        handover.record_offer(sdp.clone())
                                    }
                                    SignalingMessageType::Answer => {
                                        handover.record_answer(sdp.clone())
                                    }
                                    _ => unreachable!("peer only creates SDP offers and answers"),
                                }
                            })
                        {
                            eprintln!("[DeskLink] WebRTC local SDP rejected: {error}");
                            break;
                        }
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
                            if is_fatal_signaling_error(&error) {
                                break;
                            }
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
                            if is_fatal_signaling_error(&error) {
                                break;
                            }
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
                    PeerEvent::Envelope { channel, bytes } => {
                        if let Err(error) = handle_peer_envelope(
                            &binding,
                            &attempt_id,
                            &wire,
                            channel,
                            &bytes,
                            &sessions,
                            &config,
                            &devices,
                            &events,
                            &transfer_manager,
                        ) {
                            eprintln!("[DeskLink] Rejected WebRTC data-channel message: {error}");
                        }
                    }
                    PeerEvent::Binary { channel, bytes } => {
                        // Android wraps file data in an authenticated envelope,
                        // including when the underlying data channel is binary.
                        // Raw chunks have no generation/replay binding and are
                        // therefore rejected rather than falling back to a TLS
                        // payload socket.
                        eprintln!(
                            "[DeskLink] Rejected raw WebRTC binary payload on {} ({} bytes)",
                            channel.label(),
                            bytes.len(),
                        );
                    }
                    PeerEvent::ScreenStream(stream) => {
                        eprintln!(
                            "[DeskLink] Screen stream received from {}",
                            binding.device_id
                        );
                        super::screen::publish_stream(&binding.device_id, stream);
                    }
                    PeerEvent::Error(error) => eprintln!("[DeskLink] WebRTC peer error: {error}"),
                    PeerEvent::ConnectionChanged(state) => {
                        eprintln!("[DeskLink] WebRTC peer state: {state}");
                        match classify_peer_state(&state) {
                            PeerStateAction::Ignore => {
                                if state.to_ascii_lowercase().contains("connected") {
                                    disconnected_since = None;
                                }
                            }
                            PeerStateAction::WaitForRecovery => {
                                if disconnected_since.is_none() {
                                    eprintln!(
                                    "[DeskLink] WebRTC peer disconnected; waiting {}s for it to recover",
                                    TRANSIENT_DISCONNECT_GRACE.as_secs()
                                );
                                    disconnected_since = Some(std::time::Instant::now());
                                }
                            }
                            PeerStateAction::Rebuild => {
                                rebuild_peer(
                                    &binding,
                                    &attempt_id,
                                    format!("WebRTC peer entered terminal state: {state}"),
                                    &sessions,
                                    &config,
                                    &devices,
                                    &events,
                                );
                                break;
                            }
                        }
                    }
                    PeerEvent::ChannelOpened(WebRtcChannel::Control) => {
                        if let Err(error) = begin_handover(&binding, &attempt_id, &wire, &sessions)
                        {
                            eprintln!("[DeskLink] Could not start WebRTC authentication: {error}");
                        }
                    }
                    PeerEvent::ChannelOpened(_) => {}
                    PeerEvent::Closed => break,
                }
            }

            // Evaluated every iteration, after any event has been applied.
            // Checking it only when the receive times out let a peer that keeps
            // emitting events — a failing SCTP association produces a steady
            // stream of channel errors — defer the window forever, so a dead
            // peer was never rebuilt. Applying the event first is what lets a
            // recovery notification on the expiry tick clear the timer instead
            // of being discarded with the peer it reported healthy.
            if expired(disconnected_since) {
                rebuild_peer(
                    &binding,
                    &attempt_id,
                    "WebRTC peer did not recover from a transient disconnect".to_string(),
                    &sessions,
                    &config,
                    &devices,
                    &events,
                );
                break;
            }
        }

        // Nothing services this peer's events once the worker exits, so it must
        // not be left installed. An orphaned peer is worse than no peer: every
        // inbound feature is dropped in silence, and because the handover still
        // reports feature-ready, `wants_bootstrap_connection` stays false — so
        // discovery never dials a replacement and an inbound socket is
        // discarded too. The device then reads as connected forever.
        //
        // Doing it here rather than at each `break` makes that structurally
        // impossible instead of a property of every exit path remembering to.
        // Exits that already cleared their attempt get `StaleBinding` back,
        // which is a no-op.
    });
    Ok(())
}

/// Releases a WebRTC peer when its event worker stops, however it stops.
///
/// Nothing services a peer's events once the worker is gone, so it must not be
/// left installed. An orphaned peer is worse than no peer: every inbound
/// feature is dropped in silence, and because the handover still reports
/// feature-ready, `wants_bootstrap_connection` stays false — so discovery never
/// dials a replacement and an inbound socket is discarded too. The device then
/// reads as connected forever.
///
/// This is a `Drop` guard rather than a statement after the loop so the
/// guarantee also covers an unwinding panic. It is not hypothetical:
/// `thread::spawn` panics when the process hits its thread limit, and the lock
/// handler spawns from inside this worker.
///
/// Exits that already cleared their own attempt get `StaleBinding` back here,
/// which is a no-op — including after `rebuild_peer`, whose scheduled retry
/// therefore cannot be cancelled by this.
struct PeerWorkerGuard {
    binding: SessionBinding,
    attempt_id: String,
    sessions: Arc<DeviceManager>,
}

impl Drop for PeerWorkerGuard {
    fn drop(&mut self) {
        super::screen::clear_stream(&self.binding.device_id);
        if let Ok(Some(peer)) = self.sessions.clear_webrtc_if_current(
            &self.binding,
            &self.attempt_id,
            "The WebRTC event worker ended".to_string(),
        ) {
            eprintln!("[DeskLink] WebRTC worker ended; released its peer");
            peer.close();
        }
    }
}

/// Clears a dead peer and schedules one backed-off replacement attempt.
///
/// The wait happens on a detached thread so a GStreamer callback thread is
/// never blocked, and the attempt is re-validated against the current session
/// generation after the delay: a socket replacement during the wait supersedes
/// this recovery rather than racing it.
#[allow(clippy::too_many_arguments)]
fn rebuild_peer(
    binding: &SessionBinding,
    attempt_id: &str,
    reason: String,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    match sessions.clear_webrtc_if_current(binding, attempt_id, reason.clone()) {
        Ok(Some(peer)) => peer.close(),
        Ok(None) => {}
        Err(error) => {
            eprintln!("[DeskLink] Ignored stale WebRTC peer failure: {error}");
            return;
        }
    }

    // The counter lives on the session, not on this worker: each rebuild ends
    // its worker and starts a new one, so a worker-local counter would reset
    // the backoff to zero on every single failure.
    let recovery_attempt = sessions
        .with_session(binding, |session| {
            let attempt = session.webrtc_recovery_attempt;
            session.webrtc_recovery_attempt = attempt.saturating_add(1);
            attempt
        })
        .unwrap_or(0);
    let Some(delay) = recovery_delay(recovery_attempt) else {
        // The ladder is exhausted. Stop dialling and say so, rather than
        // retrying every 30 seconds forever behind a UI that reads
        // "Connecting". Inbound offers are still accepted — the phone may well
        // be the deterministic initiator — and one resets this counter.
        eprintln!(
            "[DeskLink] {reason}; giving up after {} attempts. Waiting for the peer to offer.",
            recovery_attempt
        );
        mark_error(
            devices,
            &binding.device_id,
            "Could not establish a direct connection. This network may block peer-to-peer traffic."
                .to_string(),
        );
        return;
    };
    eprintln!(
        "[DeskLink] {reason}; rebuilding the WebRTC peer in {}ms (attempt {})",
        delay.as_millis(),
        recovery_attempt + 1
    );

    let binding = binding.clone();
    let sessions = Arc::clone(sessions);
    let config = Arc::clone(config);
    let devices = Arc::clone(devices);
    let events = Arc::clone(events);
    thread::spawn(move || {
        thread::sleep(delay);
        if !sessions.is_current(&binding) {
            eprintln!("[DeskLink] WebRTC recovery superseded by a newer session generation");
            return;
        }
        if sessions
            .active_webrtc_attempt(&binding)
            .ok()
            .flatten()
            .is_some()
        {
            return;
        }
        if let Err(error) = start_initiator(binding, sessions, config, devices, events) {
            eprintln!("[DeskLink] WebRTC recovery attempt failed: {error}");
        }
    });
}

/// How the desktop should react to a `webrtcbin` connection-state change.
///
/// `disconnected` is explicitly transient in WebRTC and appears routinely when
/// the phone turns its screen off or switches Wi-Fi power state. Treating it as
/// terminal tore the peer down on every blip and started a fresh signed attempt
/// while Android was still inside its own grace window, which is what made the
/// two ends flap against each other. The Android application waits
/// `TRANSIENT_DISCONNECT_GRACE` before rebuilding; this mirrors that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerStateAction {
    Ignore,
    WaitForRecovery,
    Rebuild,
}

pub(crate) const TRANSIENT_DISCONNECT_GRACE: std::time::Duration =
    std::time::Duration::from_secs(12);

/// Bounded reconnect backoff, matching `WebRtcRecoveryPolicy` on Android.
/// Rebuilding immediately and unconditionally allocated a fresh GStreamer
/// pipeline and seven data channels per failure, so a peer that could not be
/// reached at all span the CPU instead of settling into a retry cadence.
const RECOVERY_BACKOFF_MILLIS: [u64; 6] = [1_000, 2_000, 4_000, 8_000, 16_000, 30_000];

/// The delay before retry `attempt`, or `None` once the ladder is exhausted.
///
/// Previously this clamped to the last rung and retried every 30 seconds
/// forever. Nothing ever concluded that the connection was not going to happen,
/// so `feature_transport_ready` stayed false indefinitely and the UI sat on
/// "Connecting" for as long as the application ran — the state the user
/// reported. Returning `None` is what lets the caller surface an honest
/// terminal state instead.
///
/// This bounds *initiation only*. The desktop must keep accepting inbound
/// offers afterwards: the phone is the deterministic initiator whenever its
/// device id sorts lower, and `WebRtcRecoveryPolicy` on Android never gives up
/// on attempt count. An inbound offer resets the ladder.
pub(crate) fn recovery_delay(attempt: u32) -> Option<std::time::Duration> {
    RECOVERY_BACKOFF_MILLIS
        .get(attempt as usize)
        .map(|millis| std::time::Duration::from_millis(*millis))
}

/// Whether a failed bootstrap signal should end this peer's event worker.
///
/// Losing the bootstrap socket while an authenticated WebRTC transport is
/// retained is a designed state, and a late ICE candidate written into that
/// closed socket fails as a matter of course. Ending the worker for it left the
/// peer installed with nothing servicing its events: inbound features were
/// dropped silently, and because `has_ready_webrtc` still reported true,
/// discovery refused to dial a replacement and inbound sockets were discarded —
/// wedged until the application restarted.
///
/// The peer itself is still healthy in that state, so the worker keeps running
/// and the ordinary disconnect paths remain responsible for tearing it down.
fn is_fatal_signaling_error(error: &str) -> bool {
    !error.contains("Session link is closed")
        && !error.contains("Session link has no network stream")
}

fn expired(since: Option<std::time::Instant>) -> bool {
    since.is_some_and(|since| since.elapsed() >= TRANSIENT_DISCONNECT_GRACE)
}

pub(crate) fn classify_peer_state(state: &str) -> PeerStateAction {
    let state = state.to_ascii_lowercase();
    if state.contains("failed") || state.contains("closed") {
        PeerStateAction::Rebuild
    } else if state.contains("disconnected") {
        PeerStateAction::WaitForRecovery
    } else {
        PeerStateAction::Ignore
    }
}

fn new_transfer_manager(config: &Arc<Mutex<Config>>) -> Result<Arc<WebRtcTransferManager>, String> {
    let config = config
        .lock()
        .map_err(|_| "DeskLink configuration lock poisoned".to_string())?;
    let transfer_state = config.transfer_state_dir();
    let downloads = dirs::download_dir().unwrap_or_else(|| transfer_state.join("downloads"));
    Ok(Arc::new(WebRtcTransferManager::new(
        transfer_state,
        downloads,
    )?))
}

fn begin_handover(
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
fn handle_peer_envelope(
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
    if matches!(
        envelope.message_type.as_str(),
        FILE_CONTROL_MESSAGE_TYPE | FILE_CHUNK_MESSAGE_TYPE
    ) {
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
        return crate::device_links::daemon::packet_handler::dispatch_webrtc_feature_packet(
            binding, packet, devices, sessions, config, events,
        );
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
            send_feature_ready_if_possible(binding, attempt_id, wire, sessions)
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
                clear_error(devices, &binding.device_id);
                // A completed handover means the backoff ladder has served its
                // purpose. The next unrelated failure starts from the top again
                // instead of inheriting a 30-second delay.
                let _ = sessions.with_session(binding, |session| {
                    session.webrtc_recovery_attempt = 0;
                });
                eprintln!(
                    "[DeskLink] WebRTC feature transport ready for {}",
                    binding.device_id
                );
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
fn send_feature_ready_if_possible(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    let should_send = sessions.with_webrtc_handover(binding, attempt_id, |handover| {
        Ok(handover.capabilities_confirmed() && !handover.local_feature_ready())
    })?;
    if !should_send {
        return Ok(());
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
    })
}

#[allow(clippy::too_many_arguments)]
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
    let expected_channel = match envelope.message_type.as_str() {
        FILE_CONTROL_MESSAGE_TYPE => WebRtcChannel::FileControl,
        FILE_CHUNK_MESSAGE_TYPE => WebRtcChannel::FileData,
        _ => unreachable!("file envelope has already been classified"),
    };
    if channel != expected_channel || envelope.channel != expected_channel.label() {
        return Err("DeskLink WebRTC file message arrived on the wrong channel".to_string());
    }
    let payload = envelope.validate(wire)?;
    sessions
        .accept_webrtc_envelope(binding, attempt_id, envelope.message_id)
        .map_err(|error| error.to_string())?;
    let events = match expected_channel {
        WebRtcChannel::FileControl => {
            transfer_manager.handle_control(wire, FileControl::from_json(&payload)?)?
        }
        WebRtcChannel::FileData => transfer_manager.handle_chunk(wire, &payload)?,
        _ => unreachable!("file channel classification is exhaustive"),
    };
    let peer = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?;
    send_transfer_events(&peer, wire, events)
}

fn send_transfer_events(
    peer: &DesktopWebRtcPeer,
    wire: &WebRtcWireBinding,
    events: Vec<TransferEvent>,
) -> Result<(), String> {
    for event in events {
        match event {
            TransferEvent::SendControl(control) => {
                let encoded = control.to_json()?;
                let envelope = WebRtcEnvelope::create(
                    wire,
                    WebRtcChannel::FileControl,
                    FILE_CONTROL_MESSAGE_TYPE,
                    &encoded,
                    now_millis(),
                )?;
                let text = String::from_utf8(envelope.to_json()?)
                    .map_err(|_| "DeskLink WebRTC file envelope was not UTF-8".to_string())?;
                peer.send_text(WebRtcChannel::FileControl, &text)?;
            }
            TransferEvent::SendChunk(chunk) => {
                let envelope = WebRtcEnvelope::create(
                    wire,
                    WebRtcChannel::FileData,
                    FILE_CHUNK_MESSAGE_TYPE,
                    &chunk,
                    now_millis(),
                )?;
                let text = String::from_utf8(envelope.to_json()?)
                    .map_err(|_| "DeskLink WebRTC file envelope was not UTF-8".to_string())?;
                peer.send_text(WebRtcChannel::FileData, &text)?;
            }
            TransferEvent::Completed { transfer_id, path } => {
                eprintln!(
                    "[DeskLink] WebRTC transfer {transfer_id} completed at {}",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn send_capabilities(
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

fn send_control(
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

fn authentication_transcript(
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

fn verify_remote_capabilities(
    binding: &SessionBinding,
    message: &HandoverControlMessage,
) -> Result<(), String> {
    let expected_incoming = webrtc_capabilities(&binding.link.info.incoming_capabilities);
    let expected_outgoing = webrtc_capabilities(&binding.link.info.outgoing_capabilities);
    if webrtc_capabilities(&message.incoming_capabilities) != expected_incoming
        || message.incoming_capabilities != expected_incoming
    {
        return Err("WebRTC incoming capabilities do not match the paired identity".to_string());
    }
    if webrtc_capabilities(&message.outgoing_capabilities) != expected_outgoing
        || message.outgoing_capabilities != expected_outgoing
    {
        return Err("WebRTC outgoing capabilities do not match the paired identity".to_string());
    }
    Ok(())
}

fn required_nonce(value: Option<&str>, label: &str) -> Result<String, String> {
    value
        .filter(|value| !value.is_empty() && value.len() <= 256)
        .map(ToString::to_string)
        .ok_or_else(|| format!("DeskLink WebRTC handover has no valid {label}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OfferAttemptDisposition {
    InstallNew,
    ReuseCurrent,
}

/// A random attempt ID has no trustworthy chronological ordering.  Once a
/// generation owns an offer, a different offer can only be a delayed/stale
/// record; a socket replacement creates a new session generation before a
/// legitimate restart is accepted.
fn offer_attempt_disposition(
    active_attempt: Option<&str>,
    received_attempt: &str,
) -> Result<OfferAttemptDisposition, String> {
    if received_attempt.is_empty() {
        return Err("DeskLink WebRTC offer has no attempt identifier".to_string());
    }
    match active_attempt {
        None => Ok(OfferAttemptDisposition::InstallNew),
        Some(active) if active == received_attempt => Ok(OfferAttemptDisposition::ReuseCurrent),
        Some(_) => Err("Rejected stale DeskLink WebRTC offer for a replaced attempt".to_string()),
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
        Err(error) => eprintln!(
            "[DL-WRTC-BOOT] FAILED to send {message_type:?} to {}: {error}",
            binding.device_id
        ),
    }
    result
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

#[cfg(test)]
mod tests {
    use super::{
        offer_attempt_disposition, recovery_delay, OfferAttemptDisposition, RECOVERY_BACKOFF_MILLIS,
    };

    /// The backoff used to clamp at its last rung and retry every 30 seconds
    /// for the life of the process, so a connection that could never succeed
    /// presented as "Connecting" forever. Exhausting the ladder must terminate
    /// initiation so the UI can say something true.
    #[test]
    fn recovery_backoff_gives_up_instead_of_retrying_forever() {
        let delays: Vec<_> = (0..RECOVERY_BACKOFF_MILLIS.len() as u32)
            .map(|attempt| recovery_delay(attempt).expect("ladder rung"))
            .collect();

        // Strictly increasing, so each failure costs more than the last.
        assert!(delays.windows(2).all(|pair| pair[0] < pair[1]));

        let past_the_end = RECOVERY_BACKOFF_MILLIS.len() as u32;
        assert_eq!(recovery_delay(past_the_end), None);
        assert_eq!(recovery_delay(u32::MAX), None);
    }

    #[test]
    fn stale_offer_cannot_replace_the_current_attempt() {
        assert_eq!(
            offer_attempt_disposition(Some("current"), "current").unwrap(),
            OfferAttemptDisposition::ReuseCurrent
        );
        assert!(offer_attempt_disposition(Some("current"), "delayed").is_err());
    }
}
