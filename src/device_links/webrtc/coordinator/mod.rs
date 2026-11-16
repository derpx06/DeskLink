//! Desktop WebRTC bootstrap coordinator.
//!
//! This layer accepts only paired, signed LAN bootstrap signaling and translates
//! `webrtcbin` events back into those signed records. Feature envelopes are
//! deliberately handled by the shared dispatcher, not here.
//!
//! Layout: this file holds the signalling entry points (start, inbound
//! signalling, restart, sending a feature packet), the helpers every part
//! uses (`now_millis`, `device_name`, `is_paired`), the per-device
//! `forget_liveness`, and the crate-facing re-exports. `signing` builds and
//! checks the signed records, `peer_worker` runs a peer (the hub, with the
//! `PeerWorkerGuard`), `envelopes` handles the handover and live envelopes,
//! `terminal_pump` the terminal channel, `file_link` the transfers,
//! `liveness` and `recovery` keep a link alive or give up explicitly,
//! `ice_setup` picks ICE servers and addresses, `desktop_share`,
//! `share_worker` and `desktop_input` share this computer's screen, and
//! `phone_remote` views and controls the phone. The parts have no cycles;
//! each module doc lists its sibling edges. May depend on: `core`,
//! `features`, `daemon` (state, events), `packet`, `config`, the sibling
//! `webrtc` modules.

use crate::device_links::connection_log::{self, ConnectionEvent};
use std::collections::HashMap;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;

use openssl::hash::{hash, MessageDigest};
use openssl::pkey::PKey;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::device_links::config::Config;
use crate::device_links::core::{
    DeviceManager, FeatureTransportSnapshot, Installed, SessionBinding,
};
use crate::device_links::daemon::handle::DaemonEvent;
use crate::device_links::daemon::state::clear_error;
use crate::device_links::device::DeviceView;
use crate::device_links::features::{
    handover_list_is_consistent, is_webrtc_feature, peer_accepts, record_peer_profile,
    webrtc_capabilities,
};
use crate::device_links::packet::NetworkPacket;

use super::file_protocol::{FileControl, FILE_CONTROL_MESSAGE_TYPE};
use super::ice::IceServerConfig;
use super::remote_desktop_portal::{
    input_actions, DesktopSession, PortalInputAction, RemoteDesktopPortal,
};
use super::remote_session;
use super::screen::DesktopSource;
use super::terminal_bridge::{self, TerminalEvent};
use super::terminal_protocol::{TERMINAL_CHUNK_MESSAGE_TYPE, TERMINAL_CONTROL_MESSAGE_TYPE};
use super::transfer_manager::{FileLink, WebRtcTransferManager};
use super::{
    AuthenticationTranscript, DesktopWebRtcPeer, HandoverControlKind, HandoverControlMessage,
    PeerEvent, SignalingMessageType, WebRtcChannel, WebRtcEnvelope, WebRtcPacketBridge,
    WebRtcSignalingMessage, WebRtcWireBinding, HANDOVER_MESSAGE_TYPE,
};
use crate::device_links::daemon::state::mark_error;

#[cfg(test)]
mod denial_tests;
#[cfg(test)]
mod input_log_tests;
#[cfg(test)]
mod tests;

mod desktop_input;
mod desktop_share;
mod envelopes;
mod file_link;
mod ice_setup;
mod liveness;
mod peer_worker;
mod phone_remote;
mod recovery;
mod share_worker;
mod signing;
mod terminal_pump;
use desktop_input::*;
use desktop_share::*;
use envelopes::*;
use file_link::*;
use ice_setup::*;
use liveness::*;
use peer_worker::*;
use phone_remote::*;
use recovery::*;
use share_worker::*;
use signing::*;
use terminal_pump::*;

// The crate-facing surface (everything else stays inside).
#[cfg(debug_assertions)]
pub(crate) use desktop_input::debug_inject_desktop_input;
pub(crate) use desktop_input::dispatch_desktop_remote_input;
pub(crate) use desktop_share::{
    desktop_extended, desktop_session, end_phone_sessions, phones_awaiting_approval,
    take_share_failures,
};
pub(crate) use file_link::{
    cancel_file_transfer, start_clipboard_transfer, start_file_transfer, start_folder_transfer,
};
pub(crate) use ice_setup::ice_config_for;
pub(crate) use liveness::{
    classify_peer_state, data_silence, is_dozing, link_silence, liveness, note_heartbeat,
    probe_soon, Liveness, PeerStateAction, NEGOTIATION_TIMEOUT, PEER_DEAD_AFTER,
    PEER_DEAD_AFTER_DOZING, PEER_PROBE_AFTER, TRANSIENT_DISCONNECT_GRACE,
};
pub(crate) use peer_worker::rebuild_peers_off_the_network;
pub(crate) use phone_remote::{
    audio_switch, capture_request, has_remote_control_lease, request_phone_control,
    stamp_remote_input, start_phone_remote_control, stop_phone_remote_control,
};
pub(crate) use recovery::{
    claim_start, is_unavailable, reconnect, recovery_delay, recovery_sign, update_words,
};
#[cfg(test)]
pub(crate) use recovery::{unavailable_words, HEALTHY_AFTER};

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
    if !is_paired(&sessions, &binding) {
        return Err("DeskLink does not start WebRTC with an unpaired device".to_string());
    }
    // One start at a time per phone: the wake, the beacon, the network change
    // and the ladder's own timer can all arrive within milliseconds, and each
    // used to build a pipeline or send its own restart request.
    let Some(starting) = claim_start(&binding.device_id) else {
        return Ok(());
    };
    start_claimed(binding, sessions, config, devices, events, starting)
}

/// [`start_initiator`] for a caller that already holds the start claim.
fn start_claimed(
    binding: SessionBinding,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
    _starting: Starting,
) -> Result<(), String> {
    // Its offer (or restart request) could never be delivered: a peer built
    // now only times out, rung after rung. The next link starts it.
    if binding.link.is_closed() {
        return Ok(());
    }
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
    // Only if the session still has none, under the install's own lock.
    let replaced = match sessions
        .install_webrtc_peer_if_none(
            &binding,
            attempt_id.clone(),
            wire.clone(),
            peer,
            Arc::clone(&transfer_manager),
        )
        .map_err(|error| error.to_string())?
    {
        Installed::Replaced(replaced) => replaced,
        Installed::Occupied(unused) => {
            // Another starter got there first.
            unused.close();
            return Ok(());
        }
    };
    if let Some(replaced) = replaced {
        // The new peer cannot be feature-ready yet, so only the replaced
        // peer's device-keyed state goes; its superseded worker skips this.
        reset_device_webrtc_state(&binding.device_id);
        replaced.close();
    }
    // Attached before the worker starts: a worker that ends at once (its
    // attempt already replaced) detaches and stops the manager on its way
    // out, and an attach after that registered lanes for a stopped engine.
    let peer = sessions
        .active_webrtc_peer(&binding, &attempt_id)
        .map_err(|error| error.to_string())?;
    attach_transfer_manager(&transfer_manager, &peer, &wire, Some(Arc::clone(&config)));
    if let Err(error) = spawn_event_worker(
        binding.clone(),
        attempt_id.clone(),
        wire.clone(),
        receiver,
        Arc::clone(&sessions),
        config,
        devices,
        events,
        Arc::clone(&transfer_manager),
    ) {
        transfer_manager.detach();
        transfer_manager.shut_down();
        return Err(error);
    }
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
    debug_log!(
        "[DL-WRTC-BOOT] handling {:?} from {}",
        message.message_type,
        message.from_device_id
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

    // Each signed request is taken once, now: a replayed offer must not tear
    // down the live peer (the per-attempt check comes after the replacement),
    // nor a replayed restart request force a rebuild.
    sessions
        .note_signal_request(binding, message.request_id.clone())
        .map_err(|error| error.to_string())?;

    // Restart requests are handled before the attempt-bound peer lookup: they
    // carry the *sender's* attempt, which by definition no longer matches ours.
    // Refusing them is what left a desktop initiator whose recovery ladder had
    // run out unreachable forever while the LAN link stayed healthy.
    if matches!(
        message.message_type,
        SignalingMessageType::IceRestart | SignalingMessageType::RestartRequest
    ) {
        let active = sessions
            .active_webrtc_attempt(binding)
            .map_err(|error| error.to_string())?;
        return match restart_request_action(
            local_device_id < message.from_device_id,
            active.as_deref(),
            &message.session_attempt_id,
        ) {
            RestartAction::Ignore => Ok(()),
            RestartAction::Restart => {
                restart_as_initiator(binding, sessions, config, devices, events)
            }
        };
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
                OfferAttemptDisposition::InstallNew => {
                    // The phone is dialling: a sign of it, so a ladder that ran
                    // out while it was away starts over.
                    let _ = sessions.with_session(binding, |session| {
                        session.webrtc_recovery_attempt = 0;
                    });
                    clear_error(&devices, &binding.device_id);
                    install_responder(
                        binding.clone(),
                        message.session_attempt_id.clone(),
                        &local_device_id,
                        &message.from_device_id,
                        Arc::clone(&sessions),
                        Arc::clone(&config),
                        Arc::clone(&devices),
                        Arc::clone(&events),
                    )?
                }
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
        SignalingMessageType::IceRestart | SignalingMessageType::RestartRequest => {
            unreachable!("restart requests are handled before the peer lookup")
        }
        SignalingMessageType::Close => {
            peer.close();
            Ok(())
        }
    }
}

/// Drops the current peer, caps the backoff ladder at its last rung (it does
/// not reset it) and offers afresh. Used when the responder asks for a
/// restart: it cannot rebuild on its own.
pub(crate) fn restart_as_initiator(
    binding: &SessionBinding,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    // A start already under way (a rebuild timer, a beacon) is the fresh
    // offer the phone asks for: clearing it and then losing the claim to it
    // left no peer, no timer and no error.
    let Some(starting) = claim_start(&binding.device_id) else {
        return Ok(());
    };
    let replaced = sessions
        .with_session(binding, |session| {
            // Cap rather than reset: zeroing on every request meant a network
            // where ICE can never succeed retried forever without the ladder
            // ever reaching "unavailable". At the cap it retries every 16 s.
            session.webrtc_recovery_attempt = session
                .webrtc_recovery_attempt
                .min(RECOVERY_BACKOFF_MILLIS.len() as u32 - 1);
            session.clear_webrtc_state()
        })
        .map_err(|error| error.to_string())?;
    if let Some(peer) = replaced {
        reset_device_webrtc_state(&binding.device_id);
        peer.close();
    }
    debug_log!(
        "[DeskLink] Restarting WebRTC for {} at the peer's request",
        binding.device_id
    );
    start_claimed(binding.clone(), sessions, config, devices, events, starting)
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
    if !peer_accepts(&transport.binding.device_id, &packet.packet_type) {
        return Err(format!(
            "{} is not in the feature profile this phone confirmed",
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

pub(super) fn device_name(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
) -> String {
    devices
        .lock()
        .ok()
        .and_then(|devices| devices.get(device_id).map(|device| device.name.clone()))
        .unwrap_or_else(|| "the phone".to_string())
}

/// Whether the session behind `binding` is a paired one.
pub(super) fn is_paired(sessions: &Arc<DeviceManager>, binding: &SessionBinding) -> bool {
    sessions
        .with_session(binding, |session| {
            session.pairing.state == crate::device_links::pairing::PairState::Paired
        })
        .unwrap_or(false)
}

pub(super) fn now_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

/// The device is gone (unpaired): what is remembered of its liveness goes.
pub(crate) fn forget_liveness(device_id: &str) {
    liveness::forget(device_id);
    recovery::forget(device_id);
    ice_setup::forget(device_id);
}
