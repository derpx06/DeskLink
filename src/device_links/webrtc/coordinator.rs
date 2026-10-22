//! Desktop WebRTC bootstrap coordinator.
//!
//! This layer accepts only paired, signed LAN bootstrap signaling and translates
//! `webrtcbin` events back into those signed records. Feature envelopes are
//! deliberately handled by the shared dispatcher, not here.

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
use crate::device_links::core::{DeviceManager, FeatureTransportSnapshot, SessionBinding};
use crate::device_links::daemon::handle::DaemonEvent;
use crate::device_links::daemon::state::clear_error;
use crate::device_links::device::DeviceView;
use crate::device_links::features::{
    handover_list_is_consistent, is_webrtc_feature, peer_accepts, record_peer_profile,
    webrtc_capabilities,
};
use crate::device_links::packet::NetworkPacket;

use super::file_protocol::{FileControl, FILE_CHUNK_MESSAGE_TYPE, FILE_CONTROL_MESSAGE_TYPE};
use super::ice::IceServerConfig;
use super::remote_desktop_portal::{
    input_actions, DesktopSession, PortalInputAction, RemoteDesktopPortal,
};
use super::remote_session;
use super::screen::DesktopSource;
use super::terminal_bridge::{self, TerminalEvent};
use super::terminal_protocol::{TERMINAL_CHUNK_MESSAGE_TYPE, TERMINAL_CONTROL_MESSAGE_TYPE};
use super::transfer_manager::{TransferEvent, WebRtcTransferManager};
use super::{
    AuthenticationTranscript, DesktopWebRtcPeer, HandoverControlKind, HandoverControlMessage,
    PeerEvent, SignalingMessageType, WebRtcChannel, WebRtcEnvelope, WebRtcPacketBridge,
    WebRtcSignalingMessage, WebRtcWireBinding, HANDOVER_MESSAGE_TYPE,
};
use crate::device_links::daemon::state::mark_error;

/// Merges runs of pointer motion, keeping everything else in order: the latest
/// absolute position wins, relative moves and scrolls add up. A click or key
/// ends a run, so motion is never reordered across it.
pub(crate) fn coalesce_motion(actions: Vec<PortalInputAction>) -> Vec<PortalInputAction> {
    let mut merged: Vec<PortalInputAction> = Vec::with_capacity(actions.len());
    for action in actions {
        match (merged.last_mut(), action) {
            (
                Some(PortalInputAction::AbsoluteMotion(x, y)),
                PortalInputAction::AbsoluteMotion(nx, ny),
            ) => {
                (*x, *y) = (nx, ny);
            }
            (
                Some(PortalInputAction::RelativeMotion(dx, dy)),
                PortalInputAction::RelativeMotion(ndx, ndy),
            ) => {
                (*dx, *dy) = (*dx + ndx, *dy + ndy);
            }
            (Some(PortalInputAction::Scroll(dx, dy)), PortalInputAction::Scroll(ndx, ndy)) => {
                (*dx, *dy) = (*dx + ndx, *dy + ndy);
            }
            (_, action) => merged.push(action),
        }
    }
    merged
}

enum DesktopPortalCommand {
    Input(Vec<PortalInputAction>),
    /// Show (and control) the next shared screen.
    SwitchScreen,
    /// Ends the share. With a reason, the phone is told it ended (a
    /// `screen-error`) and its session and input lease end with it, so it
    /// leaves the desk screen instead of waiting on a frozen picture.
    /// Without one (a newer share replaces this one) nothing is sent.
    Stop(Option<String>),
}

struct DesktopPortalService {
    /// Distinguishes this share from a later one for the same device, so a worker
    /// that is only now finishing can tell whether the registry entry is still
    /// its own before it removes anything.
    id: u64,
    sender: mpsc::Sender<DesktopPortalCommand>,
    can_control: bool,
    /// The phone is a second display (F2), not a mirror.
    extended: bool,
    /// Set when the share is released or replaced. A worker still waiting on
    /// the GNOME consent dialog gives up, instead of starting a capture nobody
    /// asked for.
    cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Disconnects when the worker has finished, its portal closed included.
    done: mpsc::Receiver<()>,
}

/// The done-signals of shares that were released, per device: the next share
/// for that phone waits on it (≤ [`RETIRE_WAIT`]) before opening its portal.
/// Otherwise a Release→RequestView pair raced two portal sessions, and the
/// old one closing tore down the new one's PipeWire node.
fn retiring() -> &'static Mutex<HashMap<String, mpsc::Receiver<()>>> {
    static RETIRING: std::sync::OnceLock<Mutex<HashMap<String, mpsc::Receiver<()>>>> =
        std::sync::OnceLock::new();
    RETIRING.get_or_init(Default::default)
}

/// How long a new share waits for the one it replaces to finish.
const RETIRE_WAIT: std::time::Duration = std::time::Duration::from_secs(3);

fn retire(device_id: &str, done: mpsc::Receiver<()>) {
    if let Ok(mut retiring) = retiring().lock() {
        retiring.insert(device_id.to_string(), done);
    }
}

/// Waits (bounded) until the share `done` belongs to has finished.
fn wait_for_retired(done: Option<mpsc::Receiver<()>>) {
    if let Some(done) = done {
        let _ = done.recv_timeout(RETIRE_WAIT);
    }
}

static NEXT_PORTAL_SERVICE_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn desktop_portal_services() -> &'static Mutex<HashMap<String, DesktopPortalService>> {
    static SERVICES: std::sync::OnceLock<Mutex<HashMap<String, DesktopPortalService>>> =
        std::sync::OnceLock::new();
    SERVICES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Removes `device_id`'s registry entry — but only if it is still `id`'s.
///
/// A worker finishing after a newer share has registered used to remove *that*
/// share's entry by device id, which ended the new capture right after the user
/// had approved it.
fn remove_service_if_current(device_id: &str, id: u64) -> bool {
    let Ok(mut services) = desktop_portal_services().lock() else {
        return false;
    };
    if services
        .get(device_id)
        .is_some_and(|service| service.id == id)
    {
        services.remove(device_id);
        true
    } else {
        false
    }
}

/// Runs when a portal worker ends, **however** it ends: every early return and
/// an unwinding panic included. The scattered per-return cleanup it replaces
/// missed the paths that left the registry entry behind, after which every later
/// share for the device failed as "already active".
struct PortalWorkerCleanup {
    device_id: String,
    id: u64,
    peer: Arc<DesktopWebRtcPeer>,
    /// Dropped last of all: tells a share waiting to replace this one.
    _done: mpsc::Sender<()>,
}

/// Declared right after the portal, so it runs *before* the portal closes:
/// the capture is stopped (pipeline to NULL) while its PipeWire node still
/// exists. The other way round, `Session.Close` removed the node under a
/// PLAYING `pipewiresrc` (`not-negotiated`, `not-linked`).
struct StopCaptureFirst {
    id: u64,
    peer: Arc<DesktopWebRtcPeer>,
}

impl Drop for StopCaptureFirst {
    fn drop(&mut self) {
        self.peer.stop_desktop_screen_capture_for(self.id);
    }
}

impl Drop for PortalWorkerCleanup {
    fn drop(&mut self) {
        remove_service_if_current(&self.device_id, self.id);
        self.peer.stop_desktop_screen_capture_for(self.id);
    }
}

/// A share whose capture produces nothing for this long is rebuilt. The
/// source re-sends its last frame every 500 ms even on a still desktop, so
/// silence means the stream died (PipeWire errors, a failed negotiation) and
/// the phone would otherwise wait for a picture forever.
const CAPTURE_STALL: std::time::Duration = std::time::Duration::from_secs(3);
/// Rebuilds in a row without steady video before the share is reported
/// failed to the phone (`screen-stopped`, "not producing any video").
const CAPTURE_RESTARTS: u32 = 3;
/// How long a rebuilt capture must keep sending before the rebuild count
/// starts over.
const CAPTURE_HEALTHY_AFTER: std::time::Duration = std::time::Duration::from_secs(10);
/// Scroll steps closer together than this are one gesture.
const SCROLL_FINISH_AFTER: std::time::Duration = std::time::Duration::from_millis(150);

fn desktop_capture_needed(direction: remote_session::ScreenDirection) -> bool {
    direction == remote_session::ScreenDirection::DesktopToPhone
}

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
    let settings = crate::device_links::config::app_settings();
    match binding.link.local_ip() {
        Some(ip) => ice.local_ips = vec![ip],
        // Bootstrapped through the relay: the internet path, with STUN for
        // the public address and, if allowed, TURN for when nothing direct
        // works.
        None => {
            use crate::device_links::relay_link::{client, route};
            ice.local_ips = route::default_route_addresses();
            let relay_servers = if settings.use_turn {
                client::ice_servers_for(&binding.device_id)
            } else {
                Vec::new()
            };
            let (stun, turn) = client::webrtcbin_uris(&relay_servers);
            if ice.stun_servers.is_empty() {
                ice.stun_servers = stun;
            }
            if ice.stun_servers.is_empty() {
                ice.stun_servers.push(DEFAULT_STUN_SERVER.to_string());
            }
            ice.turn_servers.extend(turn);
        }
    }
    if !settings.use_turn {
        ice.turn_servers.clear();
    }
    ice.relay_only =
        cfg!(debug_assertions) && std::env::var("DESKLINK_ICE_RELAY_ONLY").as_deref() == Ok("1");
    if let Ok(mut used) = ice_addresses().lock() {
        used.insert(binding.device_id.clone(), ice.local_ips.clone());
    }
    ice
}

/// The local addresses each device's current peer gathers on.
fn ice_addresses() -> &'static Mutex<HashMap<String, Vec<std::net::IpAddr>>> {
    static USED: std::sync::OnceLock<Mutex<HashMap<String, Vec<std::net::IpAddr>>>> =
        std::sync::OnceLock::new();
    USED.get_or_init(Default::default)
}

/// Whether a peer gathering on `used` lost every one of them.
pub(crate) fn addresses_vanished(used: &[std::net::IpAddr], present: &[std::net::IpAddr]) -> bool {
    !used.is_empty() && used.iter().all(|ip| !present.contains(ip))
}

/// The network changed: rebuilds every peer whose local addresses are all
/// gone (its ICE candidates can never work again), instead of waiting for
/// silence to bury it.
pub(crate) fn rebuild_peers_off_the_network(
    present: &[std::net::IpAddr],
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    for binding in sessions.current_bindings() {
        let used = ice_addresses()
            .lock()
            .ok()
            .and_then(|used| used.get(&binding.device_id).cloned())
            .unwrap_or_default();
        if !addresses_vanished(&used, present) {
            continue;
        }
        let Ok(Some(attempt_id)) = sessions.active_webrtc_attempt(&binding) else {
            continue;
        };
        rebuild_peer(
            &binding,
            &attempt_id,
            "This computer's network changed".to_string(),
            sessions,
            config,
            devices,
            events,
        );
    }
}

/// STUN for relay-bootstrapped peers when neither the relay nor the user
/// supplied one: the same provider as the default TURN service. Never used on
/// the LAN, which needs no server.
const DEFAULT_STUN_SERVER: &str = "stun://stun.cloudflare.com:3478";

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
        // The new peer cannot be feature-ready yet, so only the replaced
        // peer's device-keyed state goes; its superseded worker skips this.
        reset_device_webrtc_state(&binding.device_id);
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

    // Restart requests are handled before the attempt-bound peer lookup: they
    // carry the *sender's* attempt, which by definition no longer matches ours.
    // Refusing them is what left a desktop initiator whose recovery ladder had
    // run out unreachable forever while the LAN link stayed healthy.
    if matches!(
        message.message_type,
        SignalingMessageType::IceRestart | SignalingMessageType::RestartRequest
    ) {
        if local_device_id >= message.from_device_id {
            // This side answers; the initiator rebuilds on its own.
            return Ok(());
        }
        // A late request about an attempt that has since been replaced must not
        // kill the healthy newer one.
        let active = sessions
            .active_webrtc_attempt(binding)
            .map_err(|error| error.to_string())?;
        if active.is_some_and(|active| active != message.session_attempt_id) {
            return Ok(());
        }
        return restart_as_initiator(binding, sessions, config, devices, events);
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
        SignalingMessageType::IceRestart | SignalingMessageType::RestartRequest => {
            unreachable!("restart requests are handled before the peer lookup")
        }
        SignalingMessageType::Close => {
            peer.close();
            Ok(())
        }
    }
}

/// Drops the current peer, resets the backoff ladder and offers afresh. Used
/// when the responder asks for a restart: it cannot rebuild on its own.
fn restart_as_initiator(
    binding: &SessionBinding,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    let replaced = sessions
        .with_session(binding, |session| {
            // Cap rather than reset: zeroing on every request meant a network
            // where ICE can never succeed retried forever without the ladder
            // ever reaching "unavailable". At the cap it retries every 30 s.
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
    start_initiator(binding.clone(), sessions, config, devices, events)
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

/// Refuses to view the phone while this desktop's own screen is being shared to
/// it. One session per device serves both directions, so starting would
/// overwrite the live share's session id while its capture keeps streaming.
fn ensure_phone_view_allowed(device_id: &str) -> Result<(), String> {
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
enum ExpiredRequest {
    /// The phone asked to see this desktop and our consent dialog is still open.
    CancelDesktopShare,
    /// We asked to see the phone's screen and it never answered.
    ReleasePhoneView,
    Nothing,
}

fn expired_request_action(
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
fn expire_remote_requests(
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
            super::screen::clear_stream(&wire.peer_device_id);
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
fn take_up_phone_offer(
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

fn send_remote_heartbeat(
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
    super::screen::clear_stream(&wire.peer_device_id);
    send_result
}

/// Refuses a `request-view` and ends this side's session for it, so a denied
/// request does not linger as `RequestingView` until it times out.
fn deny_view(
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
fn apply_own_denial(device_id: &str, denial: &remote_session::ControlMessage) {
    if let Ok(mut open) = remote_session::sessions().lock() {
        if let Some(session) = open.get_mut(device_id) {
            let _ = session.accept(denial, now_millis());
        }
    }
}

/// Sends the camera message `make` builds (from a kind, session id and
/// sequence) to the phone over its ready link (C4).
pub(crate) fn send_camera_message(
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
fn handle_audio_message(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    message: &remote_session::ControlMessage,
    sessions: &Arc<DeviceManager>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    use super::audio_session::{self, Step};
    let device_id = &wire.peer_device_id;
    let phone = &binding.link.info.name;
    let peer = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?;
    let build = |kind, id: &str, sequence| {
        remote_session::ControlMessage::new(kind, attempt_id, wire, id, sequence, now_millis())
    };
    let camera = super::camera::status(device_id).map(|camera| camera.id);
    let mic_allowed = crate::device_links::config::app_settings().desk_mic_to_phone;
    match audio_session::handle(device_id, message, mic_allowed, camera.as_deref(), &build) {
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
            let output = if source == super::audio::Source::Mic {
                super::audio::Output::Microphone
            } else {
                super::audio::Output::Speakers
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
fn handle_capture_message(device_id: &str, phone: &str, message: &remote_session::ControlMessage) {
    use super::capture::{self, Outcome};
    use crate::device_links::daemon::handle::DaemonEvent;
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
    send_camera_message(sessions, transport, |build| {
        if kind == "cancel" {
            super::capture::cancel(&device_id, build)
        } else if super::capture::KINDS.contains(&kind) {
            Some(super::capture::begin(&device_id, kind, target, build))
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
    use super::audio::Source;
    use super::audio_session;
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

fn send_remote_session_message(
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

/// The phone as a second display (F2): a virtual monitor of `pixels`, its
/// scale set so it is readable, input through EIS. Returns what the share
/// worker needs, with the monitor's logical size (what the phone maps its
/// taps into).
#[allow(clippy::type_complexity)]
fn start_extended_display(
    device_id: &str,
    id: u64,
    cancel: &std::sync::atomic::AtomicBool,
    peer: &Arc<DesktopWebRtcPeer>,
    request: super::screen::ScreenRequest,
    pixels: (i32, i32),
) -> Result<
    (
        Box<dyn DesktopSession>,
        i32,
        i32,
        bool,
        Option<std::os::fd::OwnedFd>,
    ),
    String,
> {
    use super::virtual_display::{self, VirtualDisplay};
    // Its own remembered grant: a mirror's would select a monitor.
    let key = format!("{device_id}:extend");
    let stored = crate::device_links::config::remote_desktop_token(&key);
    let (display, source) = match VirtualDisplay::start(cancel, stored.as_deref(), pixels) {
        Err(error) if stored.is_some() => {
            debug_log!(
                "[DeskLink] Remembered extended-display grant failed ({error}); asking again"
            );
            crate::device_links::config::set_remote_desktop_token(&key, None);
            VirtualDisplay::start(cancel, None, pixels)?
        }
        started => started?,
    };
    crate::device_links::config::set_remote_desktop_token(&key, source.token_to_remember());
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        return Err("The desktop share was cancelled".to_string());
    }
    let switch_fd = source
        .fd
        .try_clone()
        .map_err(|error| format!("Could not keep the PipeWire connection: {error}"))?;
    let before = virtual_display::connectors();
    peer.start_desktop_screen_capture(
        id,
        DesktopSource::PipeWire {
            fd: source.fd,
            node_id: source.node_id,
            serial: source.pipewire_serial,
            virtual_size: Some(pixels),
        },
        pixels.0,
        pixels.1,
        request,
    )?;
    // The monitor appears once the capture has negotiated its size.
    let scale = virtual_display::apply_scale(&before, pixels);
    let logical = virtual_display::logical_size(pixels, scale);
    display.settle(logical);
    debug_log!(
        "[DeskLink] Extended display {}x{} px at scale {scale} ({}x{} logical) for {device_id}",
        pixels.0,
        pixels.1,
        logical.0,
        logical.1
    );
    let can_control = display.can_control();
    Ok((
        Box::new(display),
        logical.0,
        logical.1,
        can_control,
        Some(switch_fd),
    ))
}

fn start_desktop_portal_session(
    device_id: String,
    session_id: String,
    attempt_id: String,
    wire: WebRtcWireBinding,
    peer: Arc<DesktopWebRtcPeer>,
    request: super::screen::ScreenRequest,
) -> Result<(), String> {
    use std::sync::atomic::Ordering;

    let id = NEXT_PORTAL_SERVICE_ID.fetch_add(1, Ordering::Relaxed);
    let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (sender, receiver) = mpsc::channel();
    let (done_sender, done) = mpsc::channel();
    let previous_done = {
        let mut services = desktop_portal_services()
            .lock()
            .map_err(|_| "Desktop portal service registry lock poisoned".to_string())?;
        // A newer request supersedes an earlier share for this phone — a retry,
        // or an older phone build that never released the previous one. Refusing
        // it as "already active" left every later request denied.
        let previous = services.remove(&device_id).map(|previous| {
            previous.cancel.store(true, Ordering::Release);
            let _ = previous.sender.send(DesktopPortalCommand::Stop(None));
            previous.done
        });
        services.insert(
            device_id.clone(),
            DesktopPortalService {
                id,
                sender,
                can_control: false,
                extended: request.extend.is_some(),
                cancel: Arc::clone(&cancel),
                done,
            },
        );
        // Else one released a moment ago may still be closing.
        previous.or_else(|| {
            retiring()
                .lock()
                .ok()
                .and_then(|mut retiring| retiring.remove(&device_id))
        })
    };
    let worker_device_id = device_id.clone();
    let spawned = thread::Builder::new()
        .name(format!("desklink-portal-{device_id}"))
        .spawn(move || {
            run_desktop_portal_worker(
                worker_device_id,
                id,
                session_id,
                attempt_id,
                wire,
                peer,
                cancel,
                receiver,
                (done_sender, previous_done),
                request,
            )
        });
    if let Err(error) = spawned {
        remove_service_if_current(&device_id, id);
        return Err(format!(
            "Could not start the desktop portal worker: {error}"
        ));
    }
    Ok(())
}

/// One desktop-screen share: consent, capture, input dispatch, teardown.
#[allow(clippy::too_many_arguments)]
fn run_desktop_portal_worker(
    device_id: String,
    id: u64,
    session_id: String,
    attempt_id: String,
    wire: WebRtcWireBinding,
    peer: Arc<DesktopWebRtcPeer>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    receiver: mpsc::Receiver<DesktopPortalCommand>,
    (done, previous_done): (mpsc::Sender<()>, Option<mpsc::Receiver<()>>),
    request: super::screen::ScreenRequest,
) {
    use std::sync::atomic::Ordering;

    // Declared first, so it runs last: after every lock and portal handle below
    // has been released, on every way out of this function.
    let _cleanup = PortalWorkerCleanup {
        device_id: device_id.clone(),
        id,
        peer: Arc::clone(&peer),
        _done: done,
    };
    // The share this one replaces closes its portal first.
    wait_for_retired(previous_done);

    #[allow(clippy::type_complexity)]
    let setup =
        (|| -> Result<(Box<dyn DesktopSession>, i32, i32, bool, Option<std::os::fd::OwnedFd>), String> {
            if super::x11_desktop::wanted() {
                // No portal, so no dialog here: only a phone approved at this
                // desk (Remote Control → Approve…) may share it.
                if crate::device_links::config::remote_desktop_token(&device_id).as_deref()
                    != Some(super::x11_desktop::DESK_APPROVAL)
                {
                    return Err("This X11 desktop has no screen-sharing portal to ask permission with. \
                                Approve this phone in DeskLink on the computer first \
                                (device page → Remote Control → Approve…)"
                        .to_string());
                }
                let desk = super::x11_desktop::X11Desktop::connect()?;
                let screen = desk.active();
                peer.start_desktop_screen_capture(
                    id,
                    DesktopSource::X11,
                    screen.width,
                    screen.height,
                    request,
                )?;
                let can_control = desk.can_control();
                return Ok((Box::new(desk), screen.width, screen.height, can_control, None));
            }
            if let Some(pixels) = request.extend {
                return start_extended_display(&device_id, id, &cancel, &peer, request, pixels);
            }
            let stored_grant = crate::device_links::config::remote_desktop_token(&device_id);
            let (portal, source) =
                match RemoteDesktopPortal::start("", &cancel, stored_grant.as_deref()) {
                    // A remembered grant for screens that changed: forget it
                    // and ask once more without it (GNOME shows its dialog).
                    Err(error)
                        if stored_grant.is_some()
                            && error == super::remote_desktop_portal::NO_SCREEN_STREAM =>
                    {
                        debug_log!("[DeskLink] Remembered screen grant gave no screen; asking again");
                        crate::device_links::config::set_remote_desktop_token(&device_id, None);
                        RemoteDesktopPortal::start("", &cancel, None)?
                    }
                    started => started?,
                };
            // Tokens are single-use: keep the fresh one (if it carries control).
            crate::device_links::config::set_remote_desktop_token(
                &device_id,
                source.token_to_remember(),
            );
            let can_control = source.granted_device_types & 3 == 3;
            if cancel.load(Ordering::Acquire) {
                // Approved in the instant the share was released or replaced.
                return Err("The desktop share was cancelled".to_string());
            }
            // One PipeWire connection serves every shared screen; a copy is kept
            // for switching between them.
            let switch_fd = source
                .fd
                .try_clone()
                .map_err(|error| format!("Could not keep the PipeWire connection: {error}"))?;
            peer.start_desktop_screen_capture(
                id,
                DesktopSource::PipeWire {
                    fd: source.fd,
                    node_id: source.node_id,
                    serial: source.pipewire_serial,
                    virtual_size: None,
                },
                source.width,
                source.height,
                request,
            )?;
            Ok((
                Box::new(portal),
                source.width,
                source.height,
                can_control,
                Some(switch_fd),
            ))
        })();
    let (portal, width, height, can_control, switch_fd) = match setup {
        Ok(ready) => ready,
        Err(error) => {
            // Released or replaced: nobody is waiting for an answer to *this*
            // request, and a denial quoting its id would only confuse the phone.
            if cancel.load(Ordering::Acquire) || !remove_service_if_current(&device_id, id) {
                return;
            }
            eprintln!(
                "[DeskLink] Desktop screen/control portal setup failed for {device_id}: {error}"
            );
            let now = now_millis();
            let denied = remote_session::sessions().lock().ok().and_then(|mut open| {
                open.get_mut(&device_id).map(|session| {
                    let mut denied = remote_session::ControlMessage::new(
                        remote_session::ControlKind::ViewDenied,
                        &attempt_id,
                        &wire,
                        &session_id,
                        session.next_sequence(),
                        now,
                    );
                    denied.reason = Some(super::remote_desktop_portal::plain_portal_error(&error));
                    let _ = session.accept(&denied, now);
                    denied
                })
            });
            if let Some(denied) = denied {
                let _ = send_remote_session_message(&peer, &wire, &denied);
            }
            return;
        }
    };
    // Dropped before `portal`: see `StopCaptureFirst`.
    let _stop_capture_first = StopCaptureFirst {
        id,
        peer: Arc::clone(&peer),
    };
    debug_log!(
        "[DeskLink] Desktop portal granted screen {width}x{height} (1 of {} shared) and input-device mask {} for {device_id}",
        portal.stream_count(),
        if can_control { 3 } else { 0 }
    );
    {
        // One health line a few seconds in: a share that sends nothing is
        // otherwise invisible from this side.
        let (peer, device_id) = (Arc::clone(&peer), device_id.clone());
        let _ = thread::Builder::new()
            .name("desklink-share-health".into())
            .spawn(move || {
                thread::sleep(std::time::Duration::from_secs(3));
                debug_log!(
                    "[DeskLink] Desktop share to {device_id}: {} RTP packet groups after 3 s",
                    peer.desktop_screen_frames_sent()
                );
            });
    }
    // Record what the portal granted — on this share's entry only.
    match desktop_portal_services().lock() {
        Ok(mut services) => match services
            .get_mut(&device_id)
            .filter(|service| service.id == id)
        {
            Some(service) => service.can_control = can_control,
            None => return, // superseded while consent was pending
        },
        Err(_) => return,
    }

    // Move to `viewing` and learn the sequence for the announcement — but only
    // if this share is still the session's current one.
    let sequence = {
        let Ok(mut open) = remote_session::sessions().lock() else {
            return;
        };
        let Some(session) = open.get_mut(&device_id) else {
            return;
        };
        if session.remote_session_id() != Some(session_id.as_str()) {
            return;
        }
        let mut local_ready = remote_session::ControlMessage::new(
            remote_session::ControlKind::ScreenReady,
            &attempt_id,
            &wire,
            &session_id,
            session.next_sequence(),
            now_millis(),
        );
        local_ready.direction = Some(remote_session::ScreenDirection::DesktopToPhone);
        if let Err(error) = session.accept(&local_ready, now_millis()) {
            eprintln!("[DeskLink] Could not activate desktop screen session: {error}");
            return;
        }
        local_ready.sequence
    };
    let mut ready = remote_session::ControlMessage::new(
        remote_session::ControlKind::ScreenReady,
        &attempt_id,
        &wire,
        &session_id,
        sequence,
        now_millis(),
    );
    ready.direction = Some(remote_session::ScreenDirection::DesktopToPhone);
    // The desktop's own coordinate space: the phone maps its taps into this, not
    // into the (downscaled) picture it decodes.
    ready.screen_width = Some(width);
    ready.screen_height = Some(height);
    ready.screen_count = Some(portal.stream_count() as i32);
    if request.extend.is_some() {
        ready.display = Some("extend".to_string());
    }
    if send_remote_session_message(&peer, &wire, &ready).is_err() {
        return;
    }
    // (Re)starts capture of `stream` on the share's PipeWire connection.
    let capture = |stream: super::remote_desktop_portal::PortalStream| -> Result<(), String> {
        let source = match &switch_fd {
            Some(fd) => DesktopSource::PipeWire {
                fd: fd
                    .try_clone()
                    .map_err(|error| format!("Could not reuse the PipeWire connection: {error}"))?,
                node_id: stream.node_id,
                serial: stream.pipewire_serial,
                virtual_size: request.extend,
            },
            None => DesktopSource::X11,
        };
        // An extended display is captured at its pixel size, not the
        // logical one the phone maps its taps into.
        let (width, height) = request.extend.unwrap_or((stream.width, stream.height));
        peer.start_desktop_screen_capture(id, source, width, height, request)
    };
    // Moves the share to the next screen and tells the phone its new size.
    let switch_screen = || {
        let next = portal.next_stream();
        if let Err(error) = capture(next) {
            eprintln!("[DeskLink] Could not switch screens: {error}");
            return;
        }
        let sequence = remote_session::sessions().lock().ok().and_then(|mut open| {
            open.get_mut(&device_id)
                .map(|session| session.next_sequence())
        });
        if let Some(sequence) = sequence {
            let mut ready = remote_session::ControlMessage::new(
                remote_session::ControlKind::ScreenReady,
                &attempt_id,
                &wire,
                &session_id,
                sequence,
                now_millis(),
            );
            ready.direction = Some(remote_session::ScreenDirection::DesktopToPhone);
            ready.screen_width = Some(next.width);
            ready.screen_height = Some(next.height);
            ready.screen_count = Some(portal.stream_count() as i32);
            let _ = send_remote_session_message(&peer, &wire, &ready);
        }
        debug_log!(
            "[DeskLink] Desktop share for {device_id} switched to a {}x{} screen",
            next.width,
            next.height
        );
    };

    // While the phone can type here, tell it when a text input takes focus,
    // so its keyboard comes up on its own. Lives as long as this share.
    let _text_focus = can_control.then(|| {
        let (peer, wire, attempt_id, session_id, device_id) = (
            Arc::clone(&peer),
            wire.clone(),
            attempt_id.clone(),
            session_id.clone(),
            device_id.clone(),
        );
        super::text_focus::TextFocusWatcher::start(move |editable| {
            let message = remote_session::sessions().lock().ok().and_then(|mut open| {
                let session = open.get_mut(&device_id)?;
                let mut message = remote_session::ControlMessage::new(
                    remote_session::ControlKind::InputFocus,
                    &attempt_id,
                    &wire,
                    &session_id,
                    session.next_sequence(),
                    now_millis(),
                );
                message.editable = Some(editable);
                Some(message)
            });
            if let Some(message) = message {
                let _ = send_remote_session_message(&peer, &wire, &message);
            }
        })
    });

    let mut held_inputs = std::collections::HashSet::<(bool, i32)>::new();
    let mut portal_revoked = false;
    let mut no_video = false;
    // Why the desk ended the share, when it did: the phone is told.
    let mut stopped_here: Option<String> = None;
    let mut last_frames = 0u64;
    let mut last_progress = std::time::Instant::now();
    let mut restarts = 0u32;
    let mut last_rebuild: Option<std::time::Instant> = None;
    let mut scroll_open: Option<std::time::Instant> = None;
    'serve: loop {
        // A scroll gesture ends when its steps stop coming.
        if scroll_open.is_some_and(|last| last.elapsed() >= SCROLL_FINISH_AFTER) {
            let _ = portal.pointer_axis(0.0, 0.0, true);
            scroll_open = None;
        }
        // Capture watchdog; see `CAPTURE_STALL`.
        let frames = peer.desktop_screen_frames_sent();
        if frames != last_frames {
            last_frames = frames;
            last_progress = std::time::Instant::now();
            // Only video that keeps flowing clears the count: a rebuilt chain
            // that let a few packets out and then died reset it every time,
            // and the share rebuilt every 4 s for as long as the phone looked.
            if last_rebuild.is_none_or(|at| at.elapsed() >= CAPTURE_HEALTHY_AFTER) {
                restarts = 0;
            }
        } else if last_progress.elapsed() >= CAPTURE_STALL {
            if restarts >= CAPTURE_RESTARTS {
                no_video = true;
                break;
            }
            restarts += 1;
            debug_log!(
                "[DeskLink] Desktop share to {device_id} produced no video for {:?}; rebuilding the capture ({restarts} of {CAPTURE_RESTARTS})",
                CAPTURE_STALL
            );
            // Nothing ever came out of a VA-scaled chain: the scaler is the
            // likeliest cause (the share feeds it PipeWire's buffers, which
            // the startup probe cannot), so the rebuild scales in software.
            if peer.desktop_screen_frames_sent() == 0 && peer.desktop_screen_va_scaled() {
                super::screen::va_scaler_failed("no frames");
            }
            if let Err(error) = capture(portal.active()) {
                eprintln!("[DeskLink] Could not rebuild the desktop capture: {error}");
            }
            last_frames = peer.desktop_screen_frames_sent();
            last_progress = std::time::Instant::now();
            last_rebuild = Some(last_progress);
        }
        let wait = if scroll_open.is_some() {
            SCROLL_FINISH_AFTER
        } else {
            std::time::Duration::from_millis(500)
        };
        match receiver.recv_timeout(wait) {
            Ok(DesktopPortalCommand::Input(mut actions)) => {
                // Each portal call is a blocking D-Bus round trip, while touch
                // motion arrives at 120-240 Hz: one call per event let the
                // cursor fall ever further behind the finger. Take everything
                // queued meanwhile and merge consecutive motion.
                let mut stop_after = false;
                let mut switch_after = false;
                loop {
                    match receiver.try_recv() {
                        Ok(DesktopPortalCommand::Input(more)) => actions.extend(more),
                        Ok(DesktopPortalCommand::SwitchScreen) => switch_after = true,
                        Ok(DesktopPortalCommand::Stop(reason)) => {
                            stopped_here = reason;
                            stop_after = true;
                            break;
                        }
                        Err(_) => break,
                    }
                }
                // Positions go in the screen's real pixels; learn them from the
                // capture once it has negotiated (see `pointer_move`).
                if !portal.knows_active_pixels() {
                    if let Some(size) = peer.desktop_screen_source_size() {
                        debug_log!(
                            "[DeskLink] Phone input mapped onto {}x{} screen pixels",
                            size.0,
                            size.1
                        );
                        portal.set_active_pixels(size);
                    }
                }
                if actions
                    .iter()
                    .any(|action| matches!(action, PortalInputAction::Scroll(..)))
                {
                    scroll_open = Some(std::time::Instant::now());
                }
                for action in coalesce_motion(actions) {
                    let held = match &action {
                        PortalInputAction::Button { evdev_code, .. } => Some((false, *evdev_code)),
                        PortalInputAction::Key { keysym, .. } => Some((true, *keysym)),
                        _ => None,
                    };
                    let pressed = match &action {
                        PortalInputAction::Button { pressed, .. }
                        | PortalInputAction::Key { pressed, .. } => Some(*pressed),
                        _ => None,
                    };
                    let described = format!("{action:?}");
                    if let Err(error) = portal.apply(action) {
                        eprintln!(
                            "[DeskLink] Portal input dispatch failed for {described}: {error}"
                        );
                        // A revoked session is what makes every call fail; without
                        // this check it is noticed only after 500 ms of idle, which
                        // continuous motion never allows.
                        if portal.is_session_closed() {
                            portal_revoked = true;
                            break 'serve;
                        }
                        break;
                    }
                    if let (Some(held), Some(pressed)) = (held, pressed) {
                        if pressed {
                            held_inputs.insert(held);
                        } else {
                            held_inputs.remove(&held);
                        }
                    }
                }
                // Held keys and buttons are released whenever the phone no longer
                // holds input — not only when a lease *expires*: a pause, a stale
                // heartbeat, a replaced request all clear the lease first, and the
                // expiry check then sees nothing to release. A drag would leave
                // the mouse button stuck down.
                if !held_inputs.is_empty() && !peer_input_active(&device_id) {
                    release_held_portal_inputs(&*portal, &mut held_inputs);
                }
                if switch_after {
                    switch_screen();
                }
                if stop_after {
                    break;
                }
            }
            Ok(DesktopPortalCommand::SwitchScreen) => switch_screen(),
            Ok(DesktopPortalCommand::Stop(reason)) => {
                stopped_here = reason;
                break;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if portal.is_session_closed() {
                    portal_revoked = true;
                    break;
                }
                if !held_inputs.is_empty() && !peer_input_active(&device_id) {
                    release_held_portal_inputs(&*portal, &mut held_inputs);
                }
                // Let a silent phone-owned lease lapse.
                if let Ok(mut open) = remote_session::sessions().lock() {
                    if let Some(session) = open.get_mut(&device_id) {
                        session.expire_peer_input_lease(now_millis());
                    }
                }
            }
        }
    }
    release_held_portal_inputs(&*portal, &mut held_inputs);
    let told = if no_video {
        Some("The desktop is not producing any video".to_string())
    } else if portal_revoked {
        Some("Desktop screen/control permission was revoked locally".to_string())
    } else {
        stopped_here
    };
    if let Some(reason) = told {
        let sequence = remote_session::sessions()
            .lock()
            .ok()
            .and_then(|mut sessions| {
                sessions.get_mut(&device_id).map(|session| {
                    // Only this share's session: a newer one may already be
                    // running for the same phone.
                    session.stop_if_current(&session_id);
                    session.next_sequence()
                })
            })
            .unwrap_or(1);
        let stopped = screen_stopped(&attempt_id, &wire, &session_id, sequence, reason);
        let _ = send_remote_session_message(&peer, &wire, &stopped);
    }
    portal.release_all();
    // `_cleanup` removes this share's registry entry and stops its capture.
}

/// The `screen-error` that tells the phone its view of this screen ended,
/// and why.
fn screen_stopped(
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    session_id: &str,
    sequence: i64,
    reason: String,
) -> remote_session::ControlMessage {
    let mut stopped = remote_session::ControlMessage::new(
        remote_session::ControlKind::ScreenError,
        attempt_id,
        wire,
        session_id,
        sequence,
        now_millis(),
    );
    stopped.reason = Some(super::remote_desktop_portal::plain_portal_error(&reason));
    stopped
}

/// Whether the phone currently holds a valid input lease for `device_id`.
fn peer_input_active(device_id: &str) -> bool {
    remote_session::sessions()
        .lock()
        .ok()
        .and_then(|open| {
            open.get(device_id)
                .map(|session| session.state() == remote_session::SessionState::PeerControlling)
        })
        .unwrap_or(false)
}

fn release_held_portal_inputs(
    portal: &dyn DesktopSession,
    held_inputs: &mut std::collections::HashSet<(bool, i32)>,
) {
    for (is_key, code) in held_inputs.drain() {
        let action = if is_key {
            PortalInputAction::Key {
                keysym: code,
                pressed: false,
            }
        } else {
            PortalInputAction::Button {
                evdev_code: code,
                pressed: false,
            }
        };
        let _ = portal.apply(action);
    }
}

pub(crate) fn dispatch_desktop_remote_input(
    binding: &SessionBinding,
    packet: &NetworkPacket,
) -> Result<(), String> {
    let sender = desktop_portal_services()
        .lock()
        .map_err(|_| "Desktop portal service registry lock poisoned".to_string())?
        .get(&binding.device_id)
        .map(|service| service.sender.clone())
        .ok_or_else(|| "No approved desktop-control portal session is active".to_string())?;
    let actions = {
        let mut sessions = remote_session::sessions()
            .lock()
            .map_err(|_| "DeskLink remote-session lock poisoned".to_string())?;
        let session = sessions
            .get_mut(&binding.device_id)
            .ok_or_else(|| "DeskLink remote session has not started".to_string())?;
        authorize_desktop_remote_input(session, packet, &binding.device_id, now_millis())?
    };
    sender
        .send(DesktopPortalCommand::Input(actions))
        .map_err(|_| "Desktop portal worker has stopped".to_string())
}

/// The drawing pad of one phone: its size, and whether the pen is down.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Pad {
    width: f64,
    height: f64,
    down: bool,
}

fn pads() -> &'static Mutex<HashMap<String, Pad>> {
    static PADS: std::sync::OnceLock<Mutex<HashMap<String, Pad>>> = std::sync::OnceLock::new();
    PADS.get_or_init(Default::default)
}

/// What one `desklink.digitizer(.session)` packet does to the pointer: the
/// pad's place as a fraction of the screen, the pen touching as the left
/// button held. No pressure.
pub(crate) fn digitizer_actions(
    pad: &mut Option<Pad>,
    packet: &NetworkPacket,
) -> Vec<PortalInputAction> {
    const LEFT: i32 = 0x110;
    let release = |pad: &mut Pad, actions: &mut Vec<PortalInputAction>| {
        if pad.down {
            pad.down = false;
            actions.push(PortalInputAction::Button {
                evdev_code: LEFT,
                pressed: false,
            });
        }
    };
    let mut actions = Vec::new();
    if packet.packet_type == crate::device_links::packet::PACKET_TYPE_DIGITIZER_SESSION {
        match packet.get_str("action") {
            Some("start") => {
                let size = |key: &str| packet.get_i64(key).filter(|value| *value > 0);
                if let (Some(width), Some(height)) = (size("width"), size("height")) {
                    if let Some(old) = pad.as_mut() {
                        release(old, &mut actions);
                    }
                    *pad = Some(Pad {
                        width: width as f64,
                        height: height as f64,
                        down: false,
                    });
                }
            }
            Some("end") => {
                if let Some(old) = pad.as_mut() {
                    release(old, &mut actions);
                }
                *pad = None;
            }
            _ => {}
        }
        return actions;
    }
    let Some(current) = pad.as_mut() else {
        return actions;
    };
    let coordinate = |key: &str| packet.body.get(key).and_then(serde_json::Value::as_f64);
    if let (Some(x), Some(y)) = (coordinate("x"), coordinate("y")) {
        if x.is_finite() && y.is_finite() {
            actions.push(PortalInputAction::NormalizedMotion(
                (x / current.width).clamp(0.0, 1.0),
                (y / current.height).clamp(0.0, 1.0),
            ));
        }
    }
    match packet.get_bool("touching") {
        Some(true) if !current.down => {
            current.down = true;
            actions.push(PortalInputAction::Button {
                evdev_code: LEFT,
                pressed: true,
            });
        }
        Some(false) => release(current, &mut actions),
        _ => {}
    }
    if packet.get_bool("active") == Some(false) {
        release(current, &mut actions);
    }
    actions
}

/// The drawing pad's strokes, as pointer drags on the shared screen. Only
/// under the control lease, like the mousepad.
pub(crate) fn dispatch_digitizer(
    binding: &SessionBinding,
    packet: &NetworkPacket,
) -> Result<(), String> {
    // Refused: forget the pad, so a stroke half-drawn when control ended is
    // not continued (with the button held) when it comes back.
    let refuse = |reason: &str| {
        forget_pad(&binding.device_id);
        Err(reason.to_string())
    };
    if !peer_input_active(&binding.device_id) {
        return refuse("The drawing pad needs control of this computer first");
    }
    let Some(sender) = desktop_portal_services()
        .lock()
        .map_err(|_| "Desktop portal service registry lock poisoned".to_string())?
        .get(&binding.device_id)
        .map(|service| service.sender.clone())
    else {
        return refuse("No approved desktop-control portal session is active");
    };
    let actions = {
        let mut pads = pads()
            .lock()
            .map_err(|_| "DeskLink drawing pad lock poisoned".to_string())?;
        let mut pad = pads.get(&binding.device_id).copied();
        let actions = digitizer_actions(&mut pad, packet);
        match pad {
            Some(pad) => pads.insert(binding.device_id.clone(), pad),
            None => pads.remove(&binding.device_id),
        };
        actions
    };
    if actions.is_empty() {
        return Ok(());
    }
    sender
        .send(DesktopPortalCommand::Input(actions))
        .map_err(|_| "Desktop portal worker has stopped".to_string())
}

/// Debug builds: feeds a mousepad body into the active share as if the phone
/// had sent it, skipping the lease, so the GNOME side can be measured alone.
#[cfg(debug_assertions)]
pub(crate) fn debug_inject_desktop_input(device_id: &str, body: &str) -> Result<(), String> {
    let command = if body == "switch" {
        DesktopPortalCommand::SwitchScreen
    } else {
        let mut packet = NetworkPacket::new("desklink.mousepad.request");
        packet.body = serde_json::from_str(body).map_err(|error| error.to_string())?;
        DesktopPortalCommand::Input(input_actions(&packet)?)
    };
    desktop_portal_services()
        .lock()
        .map_err(|_| "registry lock poisoned".to_string())?
        .get(device_id)
        .ok_or("no active share")?
        .sender
        .send(command)
        .map_err(|_| "share worker stopped".to_string())
}

fn authorize_desktop_remote_input(
    session: &mut remote_session::RemoteSession,
    packet: &NetworkPacket,
    owner_device_id: &str,
    now: i64,
) -> Result<Vec<PortalInputAction>, String> {
    // A syntactically invalid input must not refresh the lease. Parse the
    // complete action batch first, then consume its sequence and extend expiry.
    let actions = input_actions(packet)?;
    session.verify_peer_input(packet, owner_device_id, now)?;
    #[cfg(debug_assertions)]
    debug_log!("[DeskLink] Phone input -> {actions:?}");
    Ok(actions)
}

/// What `device_id` is doing with this computer's screen, for the UI.
pub(crate) fn desktop_session(device_id: &str) -> crate::device_links::device::DesktopSession {
    use crate::device_links::device::DesktopSession;
    let sharing = desktop_portal_services()
        .lock()
        .is_ok_and(|services| services.contains_key(device_id));
    match (sharing, peer_input_active(device_id)) {
        (false, _) => DesktopSession::None,
        (true, false) => DesktopSession::Viewing,
        (true, true) => DesktopSession::Controlling,
    }
}

/// Whether `device_id`'s share of this desk is a second display (F2).
pub(crate) fn desktop_extended(device_id: &str) -> bool {
    desktop_portal_services().lock().is_ok_and(|services| {
        services
            .get(device_id)
            .is_some_and(|service| service.extended)
    })
}

/// "End phone sessions": stops `device_id` viewing or controlling this screen
/// and detaches its terminals. Pairing and consent are untouched; the phone
/// may start again.
pub(crate) fn end_phone_sessions(device_id: &str) {
    let viewing = desktop_portal_services()
        .lock()
        .is_ok_and(|services| services.contains_key(device_id));
    let terminals = super::terminal_bridge::attached_count(device_id) > 0;
    if viewing || terminals {
        connection_log::record(device_id, ConnectionEvent::SessionsEnded);
    }
    stop_desktop_portal_session(device_id, Some(SESSIONS_ENDED_AT_DESK));
    super::terminal_bridge::end_device_sessions(device_id);
}

/// What the phone is told when the desk ends its view of the screen.
const SESSIONS_ENDED_AT_DESK: &str = "Sessions were ended at the desk";
const SHARING_STOPPED_AT_DESK: &str = "Sharing was stopped at the desk";

/// Stops `device_id` viewing or controlling this screen, leaving its
/// terminals attached: "Stop Sharing" at the desk.
pub(crate) fn end_screen_session(device_id: &str) {
    stop_desktop_portal_session(device_id, Some(SHARING_STOPPED_AT_DESK));
}

/// Ends `device_id`'s share of this screen. `tell` is the reason the phone
/// is given; `None` stops it without a word (the request already expired).
/// Drops `device_id`'s drawing pad (its session ended or was refused).
fn forget_pad(device_id: &str) {
    if let Ok(mut pads) = pads().lock() {
        pads.remove(device_id);
    }
}

fn stop_desktop_portal_session(device_id: &str, tell: Option<&str>) {
    forget_pad(device_id);
    let service = desktop_portal_services()
        .lock()
        .ok()
        .and_then(|mut services| services.remove(device_id));
    if let Some(DesktopPortalService {
        cancel,
        sender,
        done,
        ..
    }) = service
    {
        connection_log::record(device_id, ConnectionEvent::ScreenSessionEnded);
        // The next share for this phone waits for this one to finish.
        retire(device_id, done);
        // Cancelling too: a worker still waiting on the consent dialog never
        // reads the `Stop` command until that dialog is answered.
        cancel.store(true, std::sync::atomic::Ordering::Release);
        let _ = sender.send(DesktopPortalCommand::Stop(tell.map(str::to_string)));
    }
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

/// Sends a synced file (F8 P4) through the current peer.
pub(crate) fn start_folder_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    source: &std::path::Path,
    folder: super::file_protocol::FolderRef,
) -> Result<(), String> {
    if !sessions.is_current(&transport.binding) || !transport.web_rtc_ready {
        return Err("DeskLink WebRTC file transport is unavailable".to_string());
    }
    let (Some(peer), Some(wire), Some(manager)) = (
        transport.web_rtc_peer.as_ref(),
        transport.web_rtc_wire.as_ref(),
        transport.transfer_manager.as_ref(),
    ) else {
        return Err("DeskLink WebRTC file transfer is unavailable".to_string());
    };
    send_transfer_events(peer, wire, manager.start_send_folder(wire, source, folder)?)
}

/// Sends `source` as a clipboard copy (C3) of type `mime`.
pub(crate) fn start_clipboard_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    source: &std::path::Path,
    mime: &str,
) -> Result<(), String> {
    if !sessions.is_current(&transport.binding) || !transport.web_rtc_ready {
        return Err("DeskLink WebRTC file transport is unavailable".to_string());
    }
    let (Some(peer), Some(wire), Some(manager)) = (
        transport.web_rtc_peer.as_ref(),
        transport.web_rtc_wire.as_ref(),
        transport.transfer_manager.as_ref(),
    ) else {
        return Err("DeskLink WebRTC file transfer is unavailable".to_string());
    };
    send_transfer_events(peer, wire, manager.start_send_as(wire, source, Some(mime))?)
}

/// Ends a running file transfer at the user's request and tells the peer.
///
/// The transfer is over locally whether or not the peer is still listening: a
/// cancelled transfer is never resumed, so its state goes either way.
pub(crate) fn cancel_file_transfer(
    sessions: &DeviceManager,
    transport: &FeatureTransportSnapshot,
    transfer_id: &str,
) -> Result<(), String> {
    if !sessions.is_current(&transport.binding) {
        return Err("DeskLink WebRTC file transport is unavailable".to_string());
    }
    let wire = transport
        .web_rtc_wire
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC file transfer has no wire binding".to_string())?;
    let manager = transport
        .transfer_manager
        .as_ref()
        .ok_or_else(|| "DeskLink WebRTC file transfer manager is unavailable".to_string())?;
    let events = manager.cancel_transfer(wire, transfer_id)?;
    match transport.web_rtc_peer.as_ref() {
        Some(peer) => send_transfer_events(peer, wire, events),
        None => Ok(()),
    }
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
        // The new peer cannot be feature-ready yet, so only the replaced
        // peer's device-keyed state goes; its superseded worker skips this.
        reset_device_webrtc_state(&binding.device_id);
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
        let mut next_remote_heartbeat =
            std::time::Instant::now() + remote_session::HEARTBEAT_INTERVAL;
        // Data-channel liveness clocks; see `PEER_DEAD_AFTER`. They only count
        // once the handover is feature-ready, so slow ICE is not mistaken for
        // a dead peer — the ICE state machine owns that phase.
        let mut last_inbound = std::time::Instant::now();
        let mut last_probe = std::time::Instant::now();
        let mut was_ready = false;
        let mut ever_ready = false;
        let started = std::time::Instant::now();
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
            expire_remote_requests(&binding, &attempt_id, &wire, &sessions);
            if std::time::Instant::now() >= next_remote_heartbeat {
                if let Err(error) = send_remote_heartbeat(&binding, &attempt_id, &wire, &sessions) {
                    eprintln!("[DeskLink] Remote-control heartbeat failed: {error}");
                }
                next_remote_heartbeat =
                    std::time::Instant::now() + remote_session::HEARTBEAT_INTERVAL;
            }
            if matches!(
                event,
                Some(PeerEvent::Envelope { .. } | PeerEvent::Binary { .. })
            ) {
                last_inbound = std::time::Instant::now();
            }
            let ready = sessions
                .webrtc_features_ready(&binding, &attempt_id)
                .unwrap_or(false);
            if ready && !was_ready {
                // Start the silence budget at the moment features go live.
                last_inbound = std::time::Instant::now();
            }
            was_ready = ready;
            ever_ready |= ready;
            // Nothing else ends a negotiation that stalls before feature-ready:
            // a lost offer or answer, a failed set-description or a lost hello
            // left webrtcbin in `checking` forever while LAN heartbeats kept
            // the session looking healthy. Rebuilding sends `RestartRequest`
            // when this side is the responder, so either role recovers.
            if !ever_ready && started.elapsed() >= NEGOTIATION_TIMEOUT {
                rebuild_peer(
                    &binding,
                    &attempt_id,
                    format!(
                        "WebRTC negotiation not feature-ready after {}s",
                        NEGOTIATION_TIMEOUT.as_secs()
                    ),
                    &sessions,
                    &config,
                    &devices,
                    &events,
                );
                break;
            }
            if ready {
                let dozing = is_dozing(&binding.device_id);
                if take_probe_now(&binding.device_id) {
                    last_probe = std::time::Instant::now();
                    if let Err(error) =
                        send_keepalive(&binding, &attempt_id, &wire, &sessions, false)
                    {
                        eprintln!("[DeskLink] WebRTC wake probe failed: {error}");
                    }
                }
                let silence = effective_silence(
                    last_inbound.elapsed(),
                    sessions.silence(&binding.device_id),
                    dozing,
                );
                match liveness(silence, last_probe.elapsed(), dozing) {
                    Liveness::Fine => {}
                    Liveness::Probe => {
                        last_probe = std::time::Instant::now();
                        if let Err(error) =
                            send_keepalive(&binding, &attempt_id, &wire, &sessions, false)
                        {
                            eprintln!("[DeskLink] WebRTC heartbeat failed: {error}");
                        }
                    }
                    Liveness::Dead => {
                        rebuild_peer(
                            &binding,
                            &attempt_id,
                            format!(
                                "WebRTC peer silent for {}s on every data channel{}",
                                if dozing {
                                    PEER_DEAD_AFTER_DOZING
                                } else {
                                    PEER_DEAD_AFTER
                                }
                                .as_secs(),
                                if dozing { " (dozing)" } else { "" }
                            ),
                            &sessions,
                            &config,
                            &devices,
                            &events,
                        );
                        break;
                    }
                }
            }
            if let Some(event) = event {
                match event {
                    PeerEvent::LocalDescription { message_type, sdp } => {
                        #[cfg(debug_assertions)]
                        for line in sdp.lines().filter(|line| {
                            line.starts_with("m=")
                                || line.starts_with("a=extmap")
                                || line.starts_with("a=rtcp-fb")
                                || line.starts_with("a=rtpmap")
                                || line.starts_with("a=sendrecv")
                                || line.starts_with("a=recvonly")
                                || line.starts_with("a=sendonly")
                                || line.starts_with("a=inactive")
                                || line.starts_with("a=msid")
                                || line.starts_with("a=ssrc")
                                || line.starts_with("a=mid")
                        }) {
                            debug_log!("[DL-SDP] local {message_type:?}: {line}");
                        }
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
                        debug_log!(
                            "[DeskLink] Screen stream received from {}",
                            binding.device_id
                        );
                        super::screen::publish_stream(&binding.device_id, stream);
                        // The camera's first frames create the pad: feed it.
                        if super::camera::is_live(&binding.device_id) {
                            if let Err(error) = super::screen::attach_camera(&binding.device_id) {
                                eprintln!("[DeskLink] {error}");
                            }
                        }
                    }
                    // Most peer errors are not fatal (an oversized candidate, an
                    // empty message); a negotiation that truly cannot finish is
                    // caught by NEGOTIATION_TIMEOUT instead.
                    PeerEvent::Error(error) => eprintln!("[DeskLink] WebRTC peer error: {error}"),
                    PeerEvent::ConnectionChanged(state) => {
                        debug_log!("[DeskLink] WebRTC peer state: {state}");
                        match classify_peer_state(&state) {
                            PeerStateAction::Ignore => {
                                if state.to_ascii_lowercase().contains("connected") {
                                    disconnected_since = None;
                                }
                            }
                            PeerStateAction::WaitForRecovery => {
                                if disconnected_since.is_none() {
                                    debug_log!(
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
                    PeerEvent::ChannelOpened(WebRtcChannel::Terminal) => {
                        // Terminal output gets its own thread; this worker is a
                        // single thread with a 500 ms timeout serving every
                        // other channel too.
                        spawn_terminal_pump(
                            binding.clone(),
                            attempt_id.clone(),
                            wire.clone(),
                            Arc::clone(&sessions),
                        );
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

/// Ends everything keyed by device rather than by attempt: desktop screen
/// share, the remote-control session, an open phone-file dialog and transfer
/// rows. Run when a peer goes away for good, and when a new peer replaces a
/// live one (the replaced worker skips it, see `PeerWorkerGuard`).
fn reset_device_webrtc_state(device_id: &str) {
    stop_desktop_portal_session(device_id, None);
    // A lost or replaced peer ends the camera too (the desk asks again),
    // and every sound session (the phone asks again on its next link).
    super::camera::end(device_id);
    super::audio_session::end_all(device_id);
    super::capture::end(device_id);
    super::screen::clear_stream(device_id);
    super::screen::forget_parked(device_id);
    // The session state must not outlive its peer: a stale `Controlling`
    // lease survived a rebuild, and this side's own heartbeats kept it alive
    // while the UI read "Control active".
    if let Ok(mut open) = remote_session::sessions().lock() {
        if let Some(session) = open.get_mut(device_id) {
            session.stop();
        }
    }
    // Transfers ride this peer. Both sides keep their checkpoints and the
    // phone resumes them when it reconnects (this side re-offers its own
    // sends): shown as paused, not as failures.
    super::transfer_board::pause_device(device_id);
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
        // The teardown below is keyed by device, not attempt. When a fresh
        // offer has already installed a newer peer, running it would tear down
        // that peer's screen share, remote session and transfers — so a
        // superseded worker only releases its own (already replaced) peer.
        let superseded = matches!(
            self.sessions.active_webrtc_attempt(&self.binding),
            Ok(Some(ref active)) if active != &self.attempt_id
        );
        if superseded {
            return;
        }
        reset_device_webrtc_state(&self.binding.device_id);
        if let Ok(Some(peer)) = self.sessions.clear_webrtc_if_current(
            &self.binding,
            &self.attempt_id,
            "The WebRTC event worker ended".to_string(),
        ) {
            debug_log!("[DeskLink] WebRTC worker ended; released its peer");
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
        debug_log!(
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
    debug_log!(
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
            debug_log!("[DeskLink] WebRTC recovery superseded by a newer session generation");
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

/// How long a fresh peer may take to reach feature-ready before it is rebuilt.
/// The measured LAN path is under 1 s; 10 s leaves room for a slow phone.
pub(crate) const NEGOTIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Data-channel liveness, mirrored by `startLivenessLocked` on Android. Once the
/// handover is feature-ready, a side that has heard nothing on any data
/// channel for `PEER_PROBE_AFTER` sends a `desklink.heartbeat`; the other side
/// answers with `reply: true`. Silence for `PEER_DEAD_AFTER` means the peer is
/// gone — typically its process restarted — and it is rebuilt at once instead
/// of waiting ~25 s for ICE consent freshness to notice.
pub(crate) const PEER_PROBE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const PEER_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(15);
/// [`PEER_DEAD_AFTER`] while the phone reports `dozing`. A phone whose screen
/// is off is frozen by its OS (ColorOS: cgroup freezer, scene LcdOff), and only
/// inbound TCP thaws it — the desk's 10 s LAN heartbeat, which stays the
/// deliberate thaw — while data-channel UDP does not. 15 s left a 4–5 s margin
/// and every missed thaw rebuilt the peer.
pub(crate) const PEER_DEAD_AFTER_DOZING: std::time::Duration = std::time::Duration::from_secs(35);

/// `desklink.heartbeat {dozing}`: whether the sender says it is dozing. A
/// state report, never a command; absent or not a boolean is awake (older
/// builds never send it).
pub(crate) fn reports_dozing(packet: &NetworkPacket) -> bool {
    packet.get_bool("dozing") == Some(true)
}

fn dozing_devices() -> &'static Mutex<std::collections::HashSet<String>> {
    static DOZING: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    DOZING.get_or_init(Default::default)
}

/// Records what `device_id`'s latest heartbeat (LAN or data channel) said.
/// A phone that just woke (`dozing: false` after `true`) is probed on the
/// data channel at once: its link may have died while it slept.
pub(crate) fn note_heartbeat(device_id: &str, packet: &NetworkPacket) {
    if set_dozing(device_id, reports_dozing(packet)) {
        if let Ok(mut woke) = probe_now().lock() {
            woke.insert(device_id.to_string());
        }
    }
}

fn probe_now() -> &'static Mutex<std::collections::HashSet<String>> {
    static PROBE_NOW: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    PROBE_NOW.get_or_init(Default::default)
}

/// Whether `device_id` woke since this was last asked (and forgets it).
fn take_probe_now(device_id: &str) -> bool {
    probe_now()
        .lock()
        .is_ok_and(|mut woke| woke.remove(device_id))
}

/// Records the phone's doze state. Only the phone clears it: a finished
/// handover says nothing about whether its screen is on. Whether it woke.
fn set_dozing(device_id: &str, dozing: bool) -> bool {
    let Ok(mut devices) = dozing_devices().lock() else {
        return false;
    };
    if dozing {
        devices.insert(device_id.to_string());
        false
    } else {
        devices.remove(device_id)
    }
}

/// The silence liveness judges by: the data channel's, or while the phone
/// dozes the more recent of the data channel's and the LAN link's (a frozen
/// phone thawed by the LAN heartbeat answers on either).
pub(crate) fn effective_silence(
    data: std::time::Duration,
    lan: Option<std::time::Duration>,
    dozing: bool,
) -> std::time::Duration {
    match lan {
        Some(lan) if dozing => data.min(lan),
        _ => data,
    }
}

pub(crate) fn is_dozing(device_id: &str) -> bool {
    dozing_devices()
        .lock()
        .is_ok_and(|devices| devices.contains(device_id))
}

/// What the worker should do about data-channel silence right now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Liveness {
    Fine,
    Probe,
    Dead,
}

/// While `dozing`, the dead time is [`PEER_DEAD_AFTER_DOZING`] and nothing is
/// probed: a frozen phone cannot answer, and a probe does not thaw it.
pub(crate) fn liveness(
    silence: std::time::Duration,
    since_probe: std::time::Duration,
    dozing: bool,
) -> Liveness {
    let dead_after = if dozing {
        PEER_DEAD_AFTER_DOZING
    } else {
        PEER_DEAD_AFTER
    };
    if silence >= dead_after {
        Liveness::Dead
    } else if !dozing && silence >= PEER_PROBE_AFTER && since_probe >= PEER_PROBE_AFTER {
        Liveness::Probe
    } else {
        Liveness::Fine
    }
}

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

/// Sends a data-channel heartbeat; `reply` marks an answer, which is never
/// answered in turn.
fn send_keepalive(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &DeviceManager,
    reply: bool,
) -> Result<(), String> {
    let peer = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?;
    let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
    if reply {
        packet.set("reply", true);
    }
    let envelope = WebRtcPacketBridge::encode(wire, &packet, now_millis())?;
    let text = String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink heartbeat serialization was not UTF-8".to_string())?;
    peer.send_text(WebRtcPacketBridge::channel_for(&packet), &text)
}

/// Bounded reconnect backoff, matching `WebRtcRecoveryPolicy` on Android.
/// Rebuilding immediately and unconditionally allocated a fresh GStreamer
/// pipeline and seven data channels per failure, so a peer that could not be
/// reached at all span the CPU instead of settling into a retry cadence.
/// Mirrored by Android's `WebRtcRecoveryPolicy` (contract B3): a healthy
/// rebuild takes under a second, so the first rung is short.
const RECOVERY_BACKOFF_MILLIS: [u64; 6] = [250, 1_000, 2_000, 4_000, 8_000, 16_000];

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
/// on attempt count. A new or adopted bootstrap link, or a restart request
/// from the responder, resets the ladder.
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
        if message.kind.is_camera() {
            let device_id = &wire.peer_device_id;
            let camera_id = super::camera::status(device_id).map(|camera| camera.id);
            match super::camera::handle(device_id, &binding.link.info.name, &message) {
                super::camera::Outcome::Live => match super::screen::attach_camera(device_id) {
                    Ok(true) => {}
                    Ok(false) => debug_log!("[DeskLink] Camera ready; waiting for its first frame"),
                    Err(error) => crate::device_links::daemon::push_event(
                        events,
                        DaemonEvent::CameraNotice {
                            device_id: device_id.clone(),
                            message: error,
                        },
                    ),
                },
                super::camera::Outcome::Stopped(words) => {
                    if camera_id.is_some_and(|id| {
                        super::audio_session::end_camera_microphone(device_id, &id)
                    }) {
                        if let Ok(peer) = sessions.active_webrtc_peer(binding, attempt_id) {
                            peer.stop_audio_playback();
                        }
                    }
                    crate::device_links::daemon::push_event(
                        events,
                        DaemonEvent::CameraNotice {
                            device_id: device_id.clone(),
                            message: words,
                        },
                    )
                }
                super::camera::Outcome::Ignored => {}
            }
            return Ok(());
        }
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
                super::screen::clear_stream(&wire.peer_device_id);
                return Ok(());
            }
            if let Err(reason) = crate::device_links::session_lock::check(
                &wire.peer_device_id,
                wire.generation,
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
            let mut request =
                super::screen::ScreenRequest::from_parts(message.max_dimension, message.fps);
            // A second display (F2) at the phone's own pixel size.
            if message.display.as_deref() == Some("extend") {
                let size = message.screen_width.zip(message.screen_height);
                match size.filter(|_| super::virtual_display::supported()) {
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
                    let _ = service.sender.send(DesktopPortalCommand::SwitchScreen);
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
                wire.generation,
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
                            if can_control {
                                "The approved desktop-control session is not active".to_string()
                            } else {
                                // The portal was approved for the picture only.
                                "The computer shared its screen without control. Reopen the \
                             screen and, on the computer, turn on \"Allow remote interaction\" \
                             before sharing."
                                    .to_string()
                            }
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
                // parked; show it again (the first view comes via pad-added).
                super::screen::resume_parked(&wire.peer_device_id);
                if let (Some(width), Some(height)) = (message.screen_width, message.screen_height) {
                    session.set_phone_screen(width, height);
                }
                session.begin_control()?;
                Some(remote_session::ControlMessage::new(
                    remote_session::ControlKind::RequestControl,
                    attempt_id,
                    wire,
                    &message.remote_session_id,
                    session.next_sequence(),
                    now_millis(),
                ))
            } else {
                None
            }
        };
        if teardown {
            super::screen::clear_stream(&wire.peer_device_id);
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
                debug_log!(
                    "[DeskLink] WebRTC feature transport ready for {}",
                    binding.device_id
                );
                flush_pending_features(binding, sessions);
                crate::device_links::connection_log::note_ready(&binding.device_id, true);
                crate::device_links::desk_features::on_feature_ready(&binding.device_id);
                // A send a lost connection interrupted carries on where the
                // phone stopped; the phone does the same for its own sends.
                let resumed = transfer_manager.resume_sends(wire);
                if !resumed.is_empty() {
                    let sent = sessions
                        .active_webrtc_peer(binding, attempt_id)
                        .map_err(|error| error.to_string())
                        .and_then(|peer| send_transfer_events(&peer, wire, resumed));
                    if let Err(error) = sent {
                        eprintln!("[DeskLink] Could not resume interrupted file sends: {error}");
                    }
                }
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

/// Queue ceiling for terminal output on one peer.
///
/// libwebrtc tears a data channel down with `RESOURCE_EXHAUSTED` around 16 MiB
/// of queued data, which would take the whole peer connection with it — not
/// just the terminal. Staying two orders of magnitude below that leaves room
/// for the other channels sharing the transport.
const TERMINAL_HIGH_WATER: u64 = 256 * 1024;

/// Interval between output pushes. Mosh measured ~8 ms as the point where a
/// higher rate stops being perceptible, and coalescing at that interval keeps a
/// flooding program from turning into a message storm.
const TERMINAL_PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);

/// Handles one `desklink-terminal-v1` envelope.
///
/// Everything that matters — the handover and binding gates, the user's
/// per-device consent, the replay cache — lives in
/// [`terminal_bridge::process_envelope`], where it can be tested without a peer
/// connection. Replies are queued on the peer's bridge and go out through
/// [`run_terminal_pump`], the only thing that sends terminal data, so they are
/// ordered with the output and subject to the same backpressure.
fn handle_terminal_envelope(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    envelope: WebRtcEnvelope,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    terminal_bridge::process_envelope(binding, attempt_id, wire, envelope, sessions)
}

/// Encodes `events` and sends them on the terminal channel, in order.
///
/// A message that cannot be encoded — too large for an envelope, say — is
/// dropped and logged: it is one bad message, and stopping the pump for it
/// would silence the terminal for good. A message that cannot be *sent* ends
/// the pump, because the channel itself is gone.
fn send_terminal_events(
    peer: &DesktopWebRtcPeer,
    wire: &WebRtcWireBinding,
    events: Vec<TerminalEvent>,
) -> Result<(), String> {
    for event in events {
        let frame = match terminal_frame(wire, event) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("[DeskLink] Dropped an unsendable terminal message: {error}");
                continue;
            }
        };
        peer.send_text(WebRtcChannel::Terminal, &frame)?;
    }
    Ok(())
}

fn terminal_frame(wire: &WebRtcWireBinding, event: TerminalEvent) -> Result<String, String> {
    let (message_type, payload) = match event {
        TerminalEvent::SendControl(control) => (TERMINAL_CONTROL_MESSAGE_TYPE, control.to_json()?),
        TerminalEvent::SendChunk(chunk) => (TERMINAL_CHUNK_MESSAGE_TYPE, chunk.encode()?),
    };
    let envelope = WebRtcEnvelope::create(
        wire,
        WebRtcChannel::Terminal,
        message_type,
        &payload,
        now_millis(),
    )?;
    String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink terminal envelope was not UTF-8".to_string())
}

/// Pushes terminal output to one peer until its connection is replaced.
///
/// A dedicated thread on purpose. The peer event worker is a single thread with
/// a 500 ms timeout serving SDP, ICE, file chunks and handover; interactive
/// terminal output there would both feel slow and starve the rest.
///
/// The bridge is claimed *here*, on the worker thread, before the pump thread
/// exists: a message on this connection cannot then arrive ahead of it. The
/// claim's token is what lets the thread's own cleanup end only its own
/// attachments — a phone that reconnects gets a new bridge and a new token, and
/// this thread noticing a moment later must not tear that down.
fn spawn_terminal_pump(
    binding: SessionBinding,
    attempt_id: String,
    wire: WebRtcWireBinding,
    sessions: Arc<DeviceManager>,
) {
    let token = terminal_bridge::begin(&wire);
    thread::spawn(move || {
        run_terminal_pump(&binding, &attempt_id, &wire, &sessions, token);
        // Whatever ended the pump, this connection is no longer receiving output
        // and no longer holds the right to type. Leaving the lease behind would
        // lock the next client out of a session nobody is driving.
        terminal_bridge::end(&wire.peer_device_id, token);
    });
}

fn run_terminal_pump(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
    token: u64,
) {
    loop {
        thread::sleep(TERMINAL_PUMP_INTERVAL);
        if !sessions.is_current(binding) {
            break;
        }
        let Ok(peer) = sessions.active_webrtc_peer(binding, attempt_id) else {
            break;
        };
        // Read the queue depth first and send only what fits under the ceiling.
        // Stopping here is what makes the PTY reader stop too: the ring stops
        // being drained, the kernel buffer fills, and the writing program
        // blocks in `write(2)` instead of us buffering it. Replies and screen
        // snapshots are charged against the same budget.
        let buffered = peer.buffered_amount(WebRtcChannel::Terminal);
        let budget = usize::try_from(TERMINAL_HIGH_WATER.saturating_sub(buffered))
            .unwrap_or(usize::MAX)
            .min(terminal_bridge::DEFAULT_PUMP_BUDGET);
        if budget == 0 {
            continue;
        }
        // `None`: a newer connection from this device has claimed the bridge.
        let Some(events) = terminal_bridge::pump_once(&wire.peer_device_id, token, budget) else {
            break;
        };
        if events.is_empty() {
            continue;
        }
        if let Err(error) = send_terminal_events(&peer, wire, events) {
            if crate::device_links::transport::should_surface_send_error(&error) {
                eprintln!("[DeskLink] Terminal pump stopped: {error}");
            }
            break;
        }
    }
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
            TransferEvent::Completed {
                transfer_id,
                path,
                capture_id,
            } => {
                if let Some(capture_id) = capture_id {
                    super::capture::file_arrived(&capture_id);
                }
                debug_log!(
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

/// A different attempt id is a restart, not a stale record: the only party
/// that can produce a validly signed offer is the paired initiator, and it
/// sends exactly one offer per attempt over one ordered TLS stream, so a
/// newer offer cannot be overtaken by an older one. Rejecting it — the
/// previous rule — is what left a restarted phone locked out for ~25 s while
/// this side clung to the dead peer of its previous process.
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
        apply_own_denial, authorize_desktop_remote_input, coalesce_motion, desktop_capture_needed,
        desktop_portal_services, effective_silence, end_screen_session, ensure_phone_view_allowed,
        expired_request_action, is_dozing, liveness, note_heartbeat, offer_attempt_disposition,
        recovery_delay, remote_session, remove_service_if_current, reports_dozing, screen_stopped,
        stop_desktop_portal_session, take_probe_now, DesktopPortalCommand, DesktopPortalService,
        ExpiredRequest, Liveness, OfferAttemptDisposition, RECOVERY_BACKOFF_MILLIS,
        SHARING_STOPPED_AT_DESK,
    };
    use crate::device_links::packet::NetworkPacket;
    use crate::device_links::webrtc::remote_session::SessionState;
    use crate::device_links::webrtc::remote_session::{
        ControlKind, ControlMessage, RemoteSession, ScreenDirection,
    };
    use crate::device_links::webrtc::WebRtcWireBinding;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};

    fn register_share(
        device: &str,
        id: u64,
    ) -> (Arc<AtomicBool>, mpsc::Receiver<DesktopPortalCommand>) {
        let cancel = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::channel();
        let (_, done) = mpsc::channel();
        desktop_portal_services().lock().unwrap().insert(
            device.to_string(),
            DesktopPortalService {
                id,
                sender,
                can_control: false,
                extended: false,
                cancel: Arc::clone(&cancel),
                done,
            },
        );
        (cancel, receiver)
    }

    /// A worker that is only now finishing must not remove the entry of the
    /// share that replaced it — that ended the new capture right after the user
    /// approved it.
    #[test]
    fn a_finishing_worker_removes_only_its_own_registry_entry() {
        let device = "test-device-own-entry";
        let _ = register_share(device, 41);
        let _ = register_share(device, 42); // a newer share for the same phone

        assert!(!remove_service_if_current(device, 41), "41 was replaced");
        assert!(desktop_portal_services()
            .lock()
            .unwrap()
            .contains_key(device));

        assert!(remove_service_if_current(device, 42));
        assert!(!desktop_portal_services()
            .lock()
            .unwrap()
            .contains_key(device));
    }

    /// A worker still waiting on the GNOME dialog never reads the `Stop`
    /// command, so stopping must cancel it as well.
    #[test]
    fn stopping_a_share_cancels_a_worker_waiting_on_the_consent_dialog() {
        let device = "test-device-cancel";
        let (cancel, receiver) = register_share(device, 7);

        stop_desktop_portal_session(device, None);

        assert!(cancel.load(Ordering::Acquire));
        assert!(matches!(
            receiver.try_recv(),
            Ok(DesktopPortalCommand::Stop(None))
        ));
        assert!(!desktop_portal_services()
            .lock()
            .unwrap()
            .contains_key(device));
    }

    /// "Stop Sharing" at the desk tells the phone, so it leaves the desk
    /// screen instead of waiting on a frozen picture.
    #[test]
    fn stopping_a_share_at_the_desk_tells_the_phone_why() {
        let device = "test-device-stop-told";
        let (_cancel, receiver) = register_share(device, 9);
        end_screen_session(device);
        assert!(matches!(
            receiver.try_recv(),
            Ok(DesktopPortalCommand::Stop(Some(reason))) if reason == SHARING_STOPPED_AT_DESK
        ));

        let wire = WebRtcWireBinding {
            sender_device_id: "desk".into(),
            peer_device_id: device.into(),
            session_id: 3,
            generation: 1,
        };
        let message = screen_stopped(
            "attempt",
            &wire,
            "remote-1",
            4,
            SHARING_STOPPED_AT_DESK.to_string(),
        );
        assert_eq!(message.kind, remote_session::ControlKind::ScreenError);
        assert_eq!(message.reason.as_deref(), Some(SHARING_STOPPED_AT_DESK));
        assert_eq!(message.remote_session_id, "remote-1");
    }

    /// One session per device serves both directions. Starting to view the
    /// phone while the desktop is being shared would overwrite the live share's
    /// session id while its capture keeps streaming.
    #[test]
    fn viewing_the_phone_is_refused_while_the_desktop_is_being_shared() {
        let device = "test-device-conflict";
        assert!(ensure_phone_view_allowed(device).is_ok());

        let _ = register_share(device, 9);
        assert!(ensure_phone_view_allowed(device).is_err());

        stop_desktop_portal_session(device, None);
        assert!(ensure_phone_view_allowed(device).is_ok());
    }

    #[test]
    fn an_expired_view_request_is_cleaned_up_on_the_side_that_owns_it() {
        assert_eq!(
            expired_request_action(
                SessionState::RequestingView,
                Some(ScreenDirection::DesktopToPhone)
            ),
            ExpiredRequest::CancelDesktopShare
        );
        assert_eq!(
            expired_request_action(
                SessionState::RequestingView,
                Some(ScreenDirection::PhoneToDesktop)
            ),
            ExpiredRequest::ReleasePhoneView
        );
        // A control request that timed out only falls back to the view.
        assert_eq!(
            expired_request_action(
                SessionState::RequestingControl,
                Some(ScreenDirection::PhoneToDesktop)
            ),
            ExpiredRequest::Nothing
        );
    }

    /// A real file, both directions, over real ICE/DTLS/SCTP data channels: the
    /// two transfer managers are driven exactly as `handle_file_envelope` drives
    /// them, and every envelope crosses a live connection. The unit tests in
    /// `transfer_manager` cannot see transport problems (ordering, message size,
    /// backpressure); this can.
    fn transfer_over_loopback(offerer_sends: bool, size: usize) {
        use super::send_transfer_events;
        use crate::device_links::webrtc::file_protocol::{
            FileControl, FILE_CHUNK_MESSAGE_TYPE, FILE_CONTROL_MESSAGE_TYPE,
        };
        use crate::device_links::webrtc::peer_connection::tests as net;
        use crate::device_links::webrtc::transfer_manager::{TransferEvent, WebRtcTransferManager};
        use crate::device_links::webrtc::{
            PeerEvent, WebRtcChannel, WebRtcEnvelope, WebRtcWireBinding,
        };

        let pair = net::loopback();
        let all = [
            WebRtcChannel::Control,
            WebRtcChannel::FileControl,
            WebRtcChannel::FileData,
        ];
        net::wait_for_channels(&pair.offerer_events, &all);
        net::wait_for_channels(&pair.answerer_events, &all);

        let (sender_peer, receiver_peer, sender_events, receiver_events) = if offerer_sends {
            (
                &pair.offerer,
                &pair.answerer,
                &pair.offerer_events,
                &pair.answerer_events,
            )
        } else {
            (
                &pair.answerer,
                &pair.offerer,
                &pair.answerer_events,
                &pair.offerer_events,
            )
        };
        let wire_sender = WebRtcWireBinding {
            sender_device_id: "sender".into(),
            peer_device_id: "receiver".into(),
            session_id: 11,
            generation: 1,
        };
        let wire_receiver = WebRtcWireBinding {
            sender_device_id: "receiver".into(),
            peer_device_id: "sender".into(),
            session_id: 11,
            generation: 1,
        };
        let root = std::env::temp_dir().join(format!(
            "desklink-loopback-{}-{size}-{offerer_sends}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("payload.bin");
        // Deterministic, incompressible-looking bytes.
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let bytes: Vec<u8> = (0..size)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect();
        std::fs::write(&source, &bytes).unwrap();

        let sender =
            WebRtcTransferManager::new(root.join("sender-state"), root.join("sender-out")).unwrap();
        let receiver =
            WebRtcTransferManager::new(root.join("receiver-state"), root.join("receiver-out"))
                .unwrap();

        let first = sender.start_send(&wire_sender, &source).unwrap();
        send_transfer_events(sender_peer, &wire_sender, first).unwrap();

        let mut published = None;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
        while published.is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "the transfer did not finish in time"
            );
            for (events, manager, wire, peer) in [
                (receiver_events, &receiver, &wire_receiver, receiver_peer),
                (sender_events, &sender, &wire_sender, sender_peer),
            ] {
                let Ok(PeerEvent::Envelope { bytes, .. }) =
                    events.recv_timeout(std::time::Duration::from_millis(5))
                else {
                    continue;
                };
                let envelope = WebRtcEnvelope::from_json(&bytes).unwrap();
                let payload = envelope.validate(wire).unwrap();
                let out = match envelope.message_type.as_str() {
                    FILE_CONTROL_MESSAGE_TYPE => manager
                        .handle_control(wire, FileControl::from_json(&payload).unwrap())
                        .unwrap(),
                    FILE_CHUNK_MESSAGE_TYPE => manager.handle_chunk(wire, &payload).unwrap(),
                    other => panic!("unexpected message type {other}"),
                };
                for event in &out {
                    if let TransferEvent::Completed { path, .. } = event {
                        published = Some(path.clone());
                    }
                }
                send_transfer_events(peer, wire, out).unwrap();
            }
        }

        let arrived = std::fs::read(published.unwrap()).unwrap();
        assert_eq!(arrived.len(), bytes.len());
        assert!(arrived == bytes, "the file arrived corrupted");
        pair.offerer.close();
        pair.answerer.close();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_crosses_a_live_connection_from_the_offerer() {
        transfer_over_loopback(true, 2 * 1024 * 1024 + 123);
    }

    /// `cargo test --release transfer_throughput -- --ignored --nocapture`:
    /// real webrtcbin on both ends, so this measures the protocol and stack,
    /// not Wi-Fi.
    #[test]
    #[ignore]
    fn transfer_throughput_over_loopback() {
        let size = 64 * 1024 * 1024;
        let started = std::time::Instant::now();
        transfer_over_loopback(true, size);
        let seconds = started.elapsed().as_secs_f64();
        eprintln!(
            "[bench] {} MiB in {seconds:.2}s = {:.1} MB/s",
            size >> 20,
            size as f64 / seconds / 1e6
        );
    }

    #[test]
    fn a_file_crosses_a_live_connection_from_the_answerer() {
        transfer_over_loopback(false, 2 * 1024 * 1024 + 123);
    }

    /// `cargo test --release transfer_throughput -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn transfer_throughput() {
        let size = 200 * 1024 * 1024;
        let started = std::time::Instant::now();
        transfer_over_loopback(true, size);
        let seconds = started.elapsed().as_secs_f64();
        eprintln!(
            "THROUGHPUT {:.1} MB/s ({} MiB in {seconds:.1} s)",
            size as f64 / seconds / 1e6,
            size >> 20
        );
    }

    #[test]
    fn an_empty_and_a_one_byte_file_cross_a_live_connection() {
        transfer_over_loopback(true, 1);
    }

    fn session_control(kind: ControlKind, session_id: &str) -> ControlMessage {
        ControlMessage {
            kind,
            session_attempt_id: "attempt".to_string(),
            device_id: "phone".to_string(),
            session_id: 1,
            connection_generation: 1,
            remote_session_id: session_id.to_string(),
            sequence: 1,
            timestamp: 1_000,
            lease_id: None,
            direction: Some(ScreenDirection::DesktopToPhone),
            owner_device_id: None,
            lease_expires_at: None,
            reason: None,
            screen_width: None,
            screen_height: None,
            screen_rotation: None,
            editable: None,
            screen_count: None,
            max_dimension: None,
            fps: None,
            password: None,
            facing: None,
            width: None,
            height: None,
            audio: None,
            action: None,
            audio_direction: None,
            source: None,
            sample_rate: None,
            channels: None,
            value: None,
            capture_kind: None,
            target: None,
            text: None,
            mime_type: None,
            pages: None,
            display: None,
        }
    }

    #[test]
    fn malformed_input_does_not_refresh_the_peer_control_lease() {
        let mut session = RemoteSession::new();
        let session_id = session.begin_view(ScreenDirection::DesktopToPhone);
        session
            .accept(
                &session_control(ControlKind::ScreenReady, &session_id),
                1_001,
            )
            .unwrap();
        session
            .accept(
                &session_control(ControlKind::RequestControl, &session_id),
                1_002,
            )
            .unwrap();
        session
            .grant_peer_control("lease".to_string(), "phone".to_string(), 100_000)
            .unwrap();

        let mut packet = NetworkPacket::new("desklink.mousepad.request");
        packet.set(super::remote_session::FIELD_REMOTE_SESSION_ID, session_id);
        packet.set(super::remote_session::FIELD_LEASE_ID, "lease");
        packet.set(super::remote_session::FIELD_INPUT_SEQUENCE, 1i64);
        packet.set("x", 25);

        assert!(authorize_desktop_remote_input(&mut session, &packet, "phone", 5_000).is_err());
        assert_eq!(session.peer_input_lease().unwrap().expires_at, 100_000);
    }

    #[test]
    fn a_denied_view_request_leaves_no_session_behind() {
        let device = "test-device-denied-view";
        remote_session::sessions()
            .lock()
            .unwrap()
            .entry(device.to_string())
            .or_default()
            .accept(&session_control(ControlKind::RequestView, "denied"), 1_000)
            .unwrap();
        assert_eq!(
            remote_session::sessions().lock().unwrap()[device].state(),
            SessionState::RequestingView
        );

        apply_own_denial(device, &session_control(ControlKind::ViewDenied, "denied"));

        let open = remote_session::sessions().lock().unwrap();
        assert_eq!(open[device].state(), SessionState::Stopped);
        assert_eq!(open[device].remote_session_id(), None);
    }

    #[test]
    fn only_desktop_to_phone_requests_need_desktop_capture() {
        assert!(desktop_capture_needed(ScreenDirection::DesktopToPhone));
        assert!(!desktop_capture_needed(ScreenDirection::PhoneToDesktop));
    }

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
    fn a_pen_stroke_is_a_drag_across_the_screen() {
        use super::super::remote_desktop_portal::PortalInputAction as A;
        use super::{digitizer_actions, Pad};
        let packet = |kind: &str, body: serde_json::Value| {
            let mut packet = NetworkPacket::new(kind);
            packet.body = body.as_object().unwrap().clone();
            packet
        };
        let mut pad: Option<Pad> = None;
        // Strokes before a session do nothing.
        assert!(digitizer_actions(
            &mut pad,
            &packet(
                "desklink.digitizer",
                serde_json::json!({"x": 1, "y": 1, "touching": true})
            )
        )
        .is_empty());
        digitizer_actions(
            &mut pad,
            &packet(
                "desklink.digitizer.session",
                serde_json::json!({"action": "start", "width": 1000, "height": 500}),
            ),
        );
        assert_eq!(
            digitizer_actions(
                &mut pad,
                &packet(
                    "desklink.digitizer",
                    serde_json::json!({"x": 500, "y": 250, "touching": true, "active": true})
                )
            ),
            vec![
                A::NormalizedMotion(0.5, 0.5),
                A::Button {
                    evdev_code: 0x110,
                    pressed: true
                }
            ]
        );
        assert_eq!(
            digitizer_actions(
                &mut pad,
                &packet(
                    "desklink.digitizer",
                    serde_json::json!({"x": 1000, "y": 900})
                )
            ),
            vec![A::NormalizedMotion(1.0, 1.0)],
            "a move while down is a drag, clamped to the screen"
        );
        assert_eq!(
            digitizer_actions(
                &mut pad,
                &packet(
                    "desklink.digitizer",
                    serde_json::json!({"touching": false, "active": false})
                )
            ),
            vec![A::Button {
                evdev_code: 0x110,
                pressed: false
            }]
        );
        // Ending with the pen down lifts it.
        digitizer_actions(
            &mut pad,
            &packet(
                "desklink.digitizer",
                serde_json::json!({"x": 1, "y": 1, "touching": true}),
            ),
        );
        assert_eq!(
            digitizer_actions(
                &mut pad,
                &packet(
                    "desklink.digitizer.session",
                    serde_json::json!({"action": "end"})
                )
            ),
            vec![A::Button {
                evdev_code: 0x110,
                pressed: false
            }]
        );
        assert!(pad.is_none());
    }

    #[test]
    fn motion_is_coalesced_without_crossing_clicks() {
        use super::super::remote_desktop_portal::PortalInputAction as A;
        let merged = coalesce_motion(vec![
            A::AbsoluteMotion(1.0, 1.0),
            A::AbsoluteMotion(2.0, 2.0),
            A::Button {
                evdev_code: 272,
                pressed: true,
            },
            A::RelativeMotion(1.0, 0.0),
            A::RelativeMotion(2.0, 3.0),
            A::Scroll(0.0, 1.0),
            A::Scroll(0.0, 1.0),
            A::Button {
                evdev_code: 272,
                pressed: false,
            },
        ]);
        assert_eq!(
            merged,
            vec![
                A::AbsoluteMotion(2.0, 2.0),
                A::Button {
                    evdev_code: 272,
                    pressed: true
                },
                A::RelativeMotion(3.0, 3.0),
                A::Scroll(0.0, 2.0),
                A::Button {
                    evdev_code: 272,
                    pressed: false
                },
            ]
        );
    }

    #[test]
    fn liveness_probes_after_five_seconds_and_declares_dead_after_fifteen() {
        use std::time::Duration;
        let s = Duration::from_secs;
        assert_eq!(liveness(s(4), s(60), false), Liveness::Fine);
        assert_eq!(liveness(s(5), s(60), false), Liveness::Probe);
        // One probe per interval, not one per 500 ms worker tick.
        assert_eq!(liveness(s(7), s(2), false), Liveness::Fine);
        assert_eq!(liveness(s(14), s(5), false), Liveness::Probe);
        assert_eq!(liveness(s(15), s(0), false), Liveness::Dead);
    }

    #[test]
    fn a_dozing_phone_gets_thirty_five_seconds_and_no_probes() {
        use std::time::Duration;
        let s = Duration::from_secs;
        assert_eq!(liveness(s(5), s(60), true), Liveness::Fine, "no probe");
        assert_eq!(liveness(s(20), s(60), true), Liveness::Fine, "past 15 s");
        assert_eq!(liveness(s(34), s(60), true), Liveness::Fine);
        assert_eq!(liveness(s(35), s(0), true), Liveness::Dead);
    }

    #[test]
    fn a_peer_is_rebuilt_only_when_all_its_addresses_are_gone() {
        use super::addresses_vanished;
        let ip = |text: &str| text.parse::<std::net::IpAddr>().unwrap();
        let present = [ip("192.168.1.20"), ip("fe80::1")];
        assert!(!addresses_vanished(&[ip("192.168.1.20")], &present));
        assert!(
            addresses_vanished(&[ip("10.0.0.5")], &present),
            "Wi-Fi switched"
        );
        assert!(
            !addresses_vanished(&[ip("10.0.0.5"), ip("fe80::1")], &present),
            "one left"
        );
        assert!(
            !addresses_vanished(&[], &present),
            "nothing recorded: nothing to judge"
        );
    }

    #[test]
    fn the_recovery_ladder_is_the_shared_one() {
        assert_eq!(
            RECOVERY_BACKOFF_MILLIS,
            [250, 1_000, 2_000, 4_000, 8_000, 16_000]
        );
    }

    #[test]
    fn a_dozing_phone_is_judged_by_its_most_recent_channel_and_probed_on_waking() {
        use std::time::Duration;
        let s = Duration::from_secs;
        assert_eq!(
            effective_silence(s(30), Some(s(4)), true),
            s(4),
            "LAN answered"
        );
        assert_eq!(
            effective_silence(s(30), Some(s(4)), false),
            s(30),
            "awake: data only"
        );
        assert_eq!(effective_silence(s(30), None, true), s(30));

        let device = "waking-test-phone";
        let mut heartbeat = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
        heartbeat.set("dozing", true);
        note_heartbeat(device, &heartbeat);
        assert!(!take_probe_now(device), "falling asleep probes nothing");
        heartbeat.set("dozing", false);
        note_heartbeat(device, &heartbeat);
        assert!(take_probe_now(device), "waking probes at once");
        assert!(!take_probe_now(device), "once");
        note_heartbeat(device, &heartbeat);
        assert!(!take_probe_now(device), "already awake: nothing new");
    }

    #[test]
    fn dozing_is_read_from_the_heartbeat_and_cleared_by_one_without_it() {
        let mut heartbeat = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
        assert!(!reports_dozing(&heartbeat), "older peers send no body");
        heartbeat.set("dozing", "yes");
        assert!(!reports_dozing(&heartbeat), "not a boolean");
        heartbeat.set("dozing", true);
        assert!(reports_dozing(&heartbeat));

        let device = "dozing-test-phone";
        note_heartbeat(device, &heartbeat);
        assert!(is_dozing(device));
        note_heartbeat(
            device,
            &NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT),
        );
        assert!(!is_dozing(device), "a heartbeat without it is awake");
        heartbeat.set("dozing", true);
        note_heartbeat(device, &heartbeat);
        heartbeat.set("dozing", false);
        note_heartbeat(device, &heartbeat);
        assert!(!is_dozing(device));
    }

    #[test]
    fn a_fresh_offer_replaces_the_current_attempt() {
        assert_eq!(
            offer_attempt_disposition(Some("current"), "current").unwrap(),
            OfferAttemptDisposition::ReuseCurrent
        );
        // A fresh signed offer replaces the peer: the initiator restarted.
        assert_eq!(
            offer_attempt_disposition(Some("current"), "fresh").unwrap(),
            OfferAttemptDisposition::InstallNew
        );
    }
}
