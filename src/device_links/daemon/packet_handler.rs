use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::clipboard::set_clipboard_text_from_remote;
use super::dispatcher::{PacketSource, SharedPacketDispatcher};
use super::handle::DaemonEvent;
use super::handshake::handle_disconnect;
use super::state::{
    push_event, remove_notification, update_battery_status, update_media_from_packet,
    update_pair_state, update_remote_commands, update_sftp_status, update_volume_status,
    upsert_notification,
};
use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, ReadOutcome, SessionBinding};
use crate::device_links::device::DeviceView;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS, PACKET_TYPE_MPRIS_REQUEST,
    PACKET_TYPE_NOTIFICATION, PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PAIR, PACKET_TYPE_PING,
    PACKET_TYPE_RUNCOMMAND, PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SFTP,
    PACKET_TYPE_SFTP_REQUEST, PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_SYSTEMVOLUME,
    PACKET_TYPE_SYSTEMVOLUME_REQUEST, PACKET_TYPE_WEBRTC_SIGNAL_V1,
};
use crate::device_links::pairing::{needs_pair_reconfirmation, PairState};

/// Reads the session's lock state, memoised for [`LOCK_STATE_VALIDITY`].
///
/// Every call forks `loginctl`, and a paired peer can ask for this as fast as
/// the data channel allows — both by polling `requestLocked` and through the
/// sample that follows a lock. Caching the answer bounds that to one subprocess
/// per validity window no matter how many devices, transports, or packets are
/// involved, which is what stops a status-poll loop from turning into a fork
/// storm.
fn is_desktop_locked() -> bool {
    let now = std::time::Instant::now();
    if let Ok(cached) = CACHED_LOCK_STATE.lock() {
        if let Some((sampled_at, locked)) = *cached {
            if now.duration_since(sampled_at) < LOCK_STATE_VALIDITY {
                return locked;
            }
        }
    }

    let locked = read_desktop_lock_state();
    if let Ok(mut cached) = CACHED_LOCK_STATE.lock() {
        *cached = Some((std::time::Instant::now(), locked));
    }
    locked
}

/// Reads the lock state fresh, ignoring and then refreshing the memo.
///
/// The settle-and-sample after a lock command must not read through the cache.
/// Invalidating on issue is not enough: any query arriving during the 300 ms
/// settle window refills the memo with the *pre-settle* state, and the sample
/// then returns that instead of the state the command produced — reporting a
/// successful lock as a failure. A phone that polls its lock state right after
/// tapping Lock does exactly this.
fn sample_desktop_lock_state() -> bool {
    let locked = read_desktop_lock_state();
    if let Ok(mut cached) = CACHED_LOCK_STATE.lock() {
        *cached = Some((std::time::Instant::now(), locked));
    }
    locked
}

/// Asks `loginctl` for the current lock state, without blocking indefinitely.
///
/// `Command::output()` waits forever, and this runs inline on a device's packet
/// reader. A wedged `systemd-logind` — which happens around suspend/resume and
/// logind restarts — would freeze that reader for good: it would never re-check
/// the session, never disconnect, and never return to `read()`, so even the TCP
/// keepalive detection could not surface through it.
fn read_desktop_lock_state() -> bool {
    let Some(session_id) = std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let Ok(mut child) = std::process::Command::new("loginctl")
        .args(["show-session", &session_id, "-p", "LockedHint", "--value"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    else {
        return false;
    };

    let deadline = std::time::Instant::now() + LOCK_STATE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                eprintln!("[Daemon] loginctl did not answer in time; assuming unlocked");
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
            Err(_) => {
                // `try_wait` does not retry `EINTR`, and `Child` has no `Drop`
                // that reaps. Returning here without this would leave a zombie
                // whenever a signal interrupts the poll.
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }

    let mut stdout = String::new();
    if let Some(mut pipe) = child.stdout.take() {
        let _ = pipe.read_to_string(&mut stdout);
    }
    stdout.trim() == "yes"
}

/// How long the screensaver is given to settle before its state is sampled.
const LOCK_SETTLE_DELAY: Duration = Duration::from_millis(300);

/// How long a sampled lock state is reused before `loginctl` is asked again.
const LOCK_STATE_VALIDITY: Duration = Duration::from_millis(1_000);

/// How long `loginctl` is given to answer before it is killed.
const LOCK_STATE_TIMEOUT: Duration = Duration::from_millis(2_000);

/// Shortest interval between two applied lock operations, process-wide.
///
/// Long enough that the previous operation's settle-and-sample has finished,
/// short enough that a user who locks and immediately unlocks is not ignored.
/// It exists to damp a flood, not to ration ordinary use.
const LOCK_MIN_INTERVAL: Duration = Duration::from_millis(500);

/// When a lock operation was last applied. See [`apply_lock_request`].
static LAST_LOCK_ACTION: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Memoised lock state and when it was sampled. See [`is_desktop_locked`].
static CACHED_LOCK_STATE: Mutex<Option<(std::time::Instant, bool)>> = Mutex::new(None);

/// Applies one packet that arrived over the LAN bootstrap link.
///
/// Split out of `packet_read_loop` so the dispatch can be driven without a
/// socket: the loop keeps framing and I/O, this keeps every decision. A pure
/// move — no branch was added, removed or reordered.
///
/// `enigo` is borrowed rather than constructed here so a caller with no input
/// device (the two-peer test harness) can pass `&mut None` and never touch one.
/// How long to wait before repeating a "not paired" notice to the same peer.
///
/// Mirrors Android's `notPairedNoticeIntervalMillis`. A peer that still thinks
/// it is paired sends clipboard updates at typing speed, and answering each one
/// would turn a stale pairing into a packet storm.
const NOT_PAIRED_NOTICE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Tells a peer this side does not consider it paired, so the two converge.
///
/// The local revocation is always committed even when its `pair:false` could
/// not be delivered, which is what makes a one-sided unpair possible in the
/// first place. This is the repair: the peer finds out the next time it uses
/// the connection, with no queue to persist and no retry to schedule.
fn notify_peer_not_paired(binding: &SessionBinding, sessions: &Arc<DeviceManager>) {
    if !sessions.claim_not_paired_notice(binding, NOT_PAIRED_NOTICE_INTERVAL) {
        return;
    }
    let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
    packet.set("pair", false);
    eprintln!(
        "[Daemon] Telling {} it is not paired with this device",
        binding.device_id
    );
    // Best effort by design: if this cannot be delivered the peer is gone, and
    // it will be told again the next time it sends anything.
    let _ = binding.link.send_packet(&packet);
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_lan_packet(
    binding: &SessionBinding,
    packet: NetworkPacket,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
    enigo_opt: &mut Option<Enigo>,
) {
    let binding = binding.clone();
    let devices = Arc::clone(devices);
    let sessions = Arc::clone(sessions);
    let config = Arc::clone(config);
    let events = Arc::clone(events);
    let device_id = binding.device_id.clone();

    let local_info = match config.lock() {
        Ok(config) => config.local_device_info(),
        Err(_) => {
            eprintln!("[Daemon] Rejected packet: DeskLink configuration lock failed");
            return;
        }
    };
    if let Err(error) = SharedPacketDispatcher::authorize(
        PacketSource::LanBootstrap,
        &binding,
        &sessions,
        &local_info,
        &packet,
    ) {
        eprintln!("[Daemon] Rejected {} packet: {error}", packet.packet_type);
        if error == crate::device_links::daemon::dispatcher::DispatchError::UnpairedFeature {
            notify_peer_not_paired(&binding, &sessions);
        }
        return;
    }

    if packet.packet_type == PACKET_TYPE_WEBRTC_SIGNAL_V1 {
        if let Err(error) = crate::device_links::webrtc::coordinator::handle_signaling_packet(
            &binding,
            &packet,
            Arc::clone(&sessions),
            Arc::clone(&config),
            Arc::clone(&devices),
            Arc::clone(&events),
        ) {
            eprintln!("[Daemon] Rejected DeskLink WebRTC signaling: {error}");
        }
        return;
    }

    if packet.packet_type == PACKET_TYPE_PAIR {
        eprintln!(
            "[Daemon] Received pair packet from device {}: pair={:?}, timestamp={:?}",
            device_id,
            packet.get_bool("pair"),
            packet.get_i64("timestamp")
        );
        let pairing = sessions.with_session(&binding, |session| {
            let previous_state = session.pairing.state;
            session.pairing.receive(
                &packet,
                crate::device_links::local_goes_first(&local_info.id, &device_id),
            );
            let verification_key = if session.pairing.state == PairState::RequestedByPeer {
                Some(binding.link.verification_key(session.pairing.timestamp()))
            } else {
                None
            };
            let reconfirm =
                needs_pair_reconfirmation(previous_state, session.pairing.state, &packet);
            (
                previous_state,
                session.pairing.state,
                verification_key,
                reconfirm,
            )
        });
        if let Ok((previous_state, state, verification_key, reconfirm)) = pairing {
            if reconfirm {
                eprintln!(
                    "[Daemon] Re-confirming an existing pairing for {}",
                    device_id
                );
                let mut acknowledgement = NetworkPacket::new(PACKET_TYPE_PAIR);
                acknowledgement.set("pair", true);
                let _ = binding.link.send_packet(&acknowledgement);
            }
            eprintln!(
                "[Daemon] Pairing state transitioned from {:?} to {:?}",
                previous_state, state
            );
            if state == PairState::Paired {
                if let Ok(mut config) = config.lock() {
                    let _ = config
                        .trust_device(&binding.link.info, binding.link.certificate_pem.clone());
                }
            } else if previous_state == PairState::Paired && state == PairState::NotPaired {
                // A remote unpair invalidates the signed WebRTC
                // peer and every handover/replay record tied to
                // the old pairing.  Without this cleanup a later
                // re-pair sees an old peer and never negotiates a
                // fresh feature transport.
                match sessions.revoke_pairing(
                    &binding,
                    "The paired device removed this pairing".to_string(),
                ) {
                    Ok((_packet, Some(peer))) => peer.close(),
                    Ok((_packet, None)) => {}
                    Err(error) => {
                        eprintln!("[Daemon] Could not clear WebRTC after remote unpair: {error}")
                    }
                }
                if let Ok(mut config) = config.lock() {
                    let _ = config.untrust_device(&device_id);
                }
            }
            update_pair_state(&devices, &device_id, state, verification_key);
            if state == PairState::Paired
                && binding
                    .link
                    .info
                    .incoming_capabilities
                    .iter()
                    .any(|capability| capability == PACKET_TYPE_WEBRTC_SIGNAL_V1)
            {
                if let Err(error) = crate::device_links::webrtc::coordinator::start_initiator(
                    binding.clone(),
                    Arc::clone(&sessions),
                    Arc::clone(&config),
                    Arc::clone(&devices),
                    Arc::clone(&events),
                ) {
                    eprintln!("[Daemon] DeskLink WebRTC negotiation unavailable: {error}");
                }
            }
        }
    } else if packet.packet_type == PACKET_TYPE_PING {
        eprintln!(
            "[Daemon] Ping received from {}: {:?}",
            device_id,
            packet.get_str("message")
        );
        push_event(
            &events,
            DaemonEvent::PingReceived {
                device_id: device_id.clone(),
                device_name: binding.link.info.name.clone(),
                message: packet.get_str("message").map(str::to_string),
            },
        );
    } else if packet.packet_type == PACKET_TYPE_BATTERY {
        update_battery_status(&devices, &device_id, &packet);
    } else if packet.packet_type == PACKET_TYPE_FINDMYPHONE_REQUEST {
        eprintln!(
            "[Daemon] Received find-this-device request from {}",
            device_id
        );
        let _ = std::process::Command::new("canberra-gtk-play")
            .args(["-i", "bell"])
            .spawn();
        push_event(
            &events,
            DaemonEvent::FindPhoneReceived {
                device_id: device_id.clone(),
                device_name: binding.link.info.name.clone(),
            },
        );
    } else if packet.packet_type == PACKET_TYPE_MOUSEPAD_REQUEST {
        let dx = packet
            .body
            .get("dx")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let dy = packet
            .body
            .get("dy")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let x = packet.body.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let y = packet.body.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let singleclick = packet.get_bool("singleclick").unwrap_or(false);
        let doubleclick = packet.get_bool("doubleclick").unwrap_or(false);
        let middleclick = packet.get_bool("middleclick").unwrap_or(false);
        let rightclick = packet.get_bool("rightclick").unwrap_or(false);
        let singlehold = packet.get_bool("singlehold").unwrap_or(false);
        let singlerelease = packet.get_bool("singlerelease").unwrap_or(false);
        let scroll = packet.get_bool("scroll").unwrap_or(false);
        let key = packet.get_str("key");
        let special_key = packet.get_i64("specialKey").unwrap_or(0);

        eprintln!(
            "[Daemon] Received remote input request: dx={:?}, dy={:?}, x={:?}, y={:?}, scroll={:?}, key={:?}, specialKey={:?}",
            dx, dy, x, y, scroll, key, special_key
        );

        if let Some(ref mut enigo) = enigo_opt {
            if scroll {
                if dy != 0.0 {
                    let _ = enigo.scroll(dy as i32, Axis::Vertical);
                }
                if dx != 0.0 {
                    let _ = enigo.scroll(dx as i32, Axis::Horizontal);
                }
            } else if singleclick {
                let _ = enigo.button(Button::Left, Direction::Click);
            } else if doubleclick {
                let _ = enigo.button(Button::Left, Direction::Click);
                let _ = enigo.button(Button::Left, Direction::Click);
            } else if middleclick {
                let _ = enigo.button(Button::Middle, Direction::Click);
            } else if rightclick {
                let _ = enigo.button(Button::Right, Direction::Click);
            } else if singlehold {
                let _ = enigo.button(Button::Left, Direction::Press);
            } else if singlerelease {
                let _ = enigo.button(Button::Left, Direction::Release);
            } else if key.is_some() || special_key > 0 {
                let ctrl = packet.get_bool("ctrl").unwrap_or(false);
                let alt = packet.get_bool("alt").unwrap_or(false);
                let shift = packet.get_bool("shift").unwrap_or(false);
                let super_key = packet.get_bool("super").unwrap_or(false);

                if ctrl {
                    let _ = enigo.key(enigo::Key::Control, Direction::Press);
                }
                if alt {
                    let _ = enigo.key(enigo::Key::Alt, Direction::Press);
                }
                if shift {
                    let _ = enigo.key(enigo::Key::Shift, Direction::Press);
                }
                if super_key {
                    let _ = enigo.key(enigo::Key::Meta, Direction::Press);
                }

                if special_key > 0 {
                    let enigo_key = match special_key {
                        1 => Some(enigo::Key::Backspace),
                        2 => Some(enigo::Key::Tab),
                        3 => Some(enigo::Key::Return),
                        4 => Some(enigo::Key::LeftArrow),
                        5 => Some(enigo::Key::UpArrow),
                        6 => Some(enigo::Key::RightArrow),
                        7 => Some(enigo::Key::DownArrow),
                        8 => Some(enigo::Key::PageUp),
                        9 => Some(enigo::Key::PageDown),
                        10 => Some(enigo::Key::Home),
                        11 => Some(enigo::Key::End),
                        12 => Some(enigo::Key::Return),
                        13 => Some(enigo::Key::Delete),
                        14 => Some(enigo::Key::Escape),
                        21 => Some(enigo::Key::F1),
                        22 => Some(enigo::Key::F2),
                        23 => Some(enigo::Key::F3),
                        24 => Some(enigo::Key::F4),
                        25 => Some(enigo::Key::F5),
                        26 => Some(enigo::Key::F6),
                        27 => Some(enigo::Key::F7),
                        28 => Some(enigo::Key::F8),
                        29 => Some(enigo::Key::F9),
                        30 => Some(enigo::Key::F10),
                        31 => Some(enigo::Key::F11),
                        32 => Some(enigo::Key::F12),
                        _ => None,
                    };
                    if let Some(ek) = enigo_key {
                        let _ = enigo.key(ek, Direction::Click);
                    }
                } else if let Some(k) = key {
                    let _ = enigo.text(k);
                }

                if ctrl {
                    let _ = enigo.key(enigo::Key::Control, Direction::Release);
                }
                if alt {
                    let _ = enigo.key(enigo::Key::Alt, Direction::Release);
                }
                if shift {
                    let _ = enigo.key(enigo::Key::Shift, Direction::Release);
                }
                if super_key {
                    let _ = enigo.key(enigo::Key::Meta, Direction::Release);
                }
            } else if dx != 0.0 || dy != 0.0 {
                let _ = enigo.move_mouse(dx as i32, dy as i32, Coordinate::Rel);
            }
        }
    } else if packet.packet_type == PACKET_TYPE_CLIPBOARD
        || packet.packet_type == PACKET_TYPE_CLIPBOARD_CONNECT
    {
        if let Some(content) = packet.get_str("content") {
            eprintln!("[Daemon] Received clipboard content: {:?}", content);
            set_clipboard_text_from_remote(content);
        }
    } else if packet.packet_type == PACKET_TYPE_SHARE_REQUEST {
        if packet.payload_size.is_some() || packet.payload_transfer_info.is_some() {
            // SharedPacketDispatcher rejects this before reaching
            // the handler.  Keep this guard as defense in depth
            // for future LAN reader call sites.
            eprintln!("[Daemon] Rejected legacy LAN file payload; WebRTC file-data is required");
        } else if let Some(text) = packet.get_str("text") {
            eprintln!("[Daemon] Received shared text: {:?}", text);
            set_clipboard_text_from_remote(text);
        } else if let Some(url) = packet.get_str("url") {
            eprintln!("[Daemon] Received shared URL: {:?}", url);
            std::process::Command::new("xdg-open").arg(url).spawn().ok();
        }
    } else if packet.packet_type == PACKET_TYPE_MPRIS {
        update_media_from_packet(&devices, &device_id, &packet);
    } else if packet.packet_type == PACKET_TYPE_NOTIFICATION {
        if packet.get_bool("isCancel").unwrap_or(false) {
            if let Some(id) = packet.get_str("id") {
                remove_notification(&devices, &device_id, id);
            }
        } else {
            upsert_notification(&devices, &device_id, &packet);
        }
    } else if packet.packet_type == PACKET_TYPE_NOTIFICATION_REQUEST {
        if let Some(cancel_id) = packet.get_str("cancel") {
            remove_notification(&devices, &device_id, cancel_id);
        }
    } else if packet.packet_type == PACKET_TYPE_SYSTEMVOLUME {
        update_volume_status(&devices, &device_id, &packet);
    } else if packet.packet_type == PACKET_TYPE_SYSTEMVOLUME_REQUEST {
        if packet.get_bool("requestSinks").unwrap_or(false) {
            let mut reply = NetworkPacket::new(PACKET_TYPE_SYSTEMVOLUME);
            reply.set("sinkList", serde_json::Value::Array(Vec::new()));
            let _ = binding.link.send_packet(&reply);
        }
    } else if packet.packet_type == PACKET_TYPE_RUNCOMMAND {
        update_remote_commands(&devices, &device_id, &packet);
    } else if packet.packet_type == PACKET_TYPE_RUNCOMMAND_REQUEST {
        if packet.get_bool("requestCommandList").unwrap_or(false) {
            let mut reply = NetworkPacket::new(PACKET_TYPE_RUNCOMMAND);
            reply.set("commandList", "{}");
            reply.set("canAddCommand", false);
            let _ = binding.link.send_packet(&reply);
        }
    } else if packet.packet_type == PACKET_TYPE_SFTP {
        update_sftp_status(&devices, &device_id, &packet);
    } else if packet.packet_type == PACKET_TYPE_LOCK
        || packet.packet_type == PACKET_TYPE_LOCK_REQUEST
    {
        if packet.body.contains_key("requestLocked") {
            eprintln!("[Daemon] Received requestLocked query");
            let mut reply = NetworkPacket::new(PACKET_TYPE_LOCK);
            reply.set("isLocked", is_desktop_locked());
            let _ = binding.link.send_packet(&reply);
        }
        if let Some(requested_lock) = apply_lock_request(&packet) {
            // Sampling the result means waiting for the screensaver
            // to settle. This reader owns one device, so the wait is
            // contained here — `apply_lock_request` itself must stay
            // non-blocking because the WebRTC path calls it from the
            // single worker that also carries SDP, ICE and file
            // chunks for every feature.
            thread::sleep(LOCK_SETTLE_DELAY);
            let locked = sample_desktop_lock_state();
            if requested_lock {
                let mut acknowledgement = NetworkPacket::new(PACKET_TYPE_LOCK);
                acknowledgement.set("lockResult", locked);
                let _ = binding.link.send_packet(&acknowledgement);
            }
            let mut state = NetworkPacket::new(PACKET_TYPE_LOCK);
            state.set("isLocked", locked);
            let _ = binding.link.send_packet(&state);
        }
    } else {
        eprintln!("[Daemon] Unhandled packet type: {}", packet.packet_type);
    }
}

pub(super) fn packet_read_loop(
    binding: SessionBinding,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) {
    let device_id = binding.device_id.clone();
    if binding.link.is_closed() {
        eprintln!(
            "[Daemon] Session reader could not start for {device_id}: Session link is closed"
        );
        return;
    }
    let mut enigo_opt = Enigo::new(&Settings::default()).ok();
    let mut line = Vec::new();

    loop {
        if !sessions.is_current(&binding) {
            break;
        }
        match binding.link.read_byte() {
            Ok(ReadOutcome::Closed) => {
                if handle_disconnect(
                    &binding,
                    &devices,
                    &sessions,
                    "Peer closed the connection".to_string(),
                ) {
                    eprintln!(
                        "[Daemon] Active link closed for {}. Marked unreachable.",
                        device_id
                    );
                }
                break;
            }
            Ok(ReadOutcome::Byte(value)) => {
                line.push(value);
                if line.len() > 32 * 1024 * 1024 {
                    eprintln!(
                        "[Daemon] Read line too long for {}. Disconnecting.",
                        device_id
                    );
                    handle_disconnect(
                        &binding,
                        &devices,
                        &sessions,
                        "Packet exceeded the maximum size".to_string(),
                    );
                    break;
                }
                if value != b'\n' {
                    continue;
                }

                let packet = NetworkPacket::deserialize(&line);
                line.clear();
                let Ok(packet) = packet else {
                    continue;
                };

                if packet.packet_type == PACKET_TYPE_WEBRTC_SIGNAL_V1 {
                    eprintln!(
                        "[DL-WRTC-BOOT] received signed bootstrap signal from {}",
                        device_id
                    );
                }

                if !sessions.is_current(&binding) {
                    break;
                }

                apply_lan_packet(
                    &binding,
                    packet,
                    &devices,
                    &sessions,
                    &config,
                    &events,
                    &mut enigo_opt,
                );
            }
            Ok(ReadOutcome::Idle) => {
                // Yield briefly so a concurrent feature send can take the
                // transport's lock instead of contending with the next read.
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                if handle_disconnect(&binding, &devices, &sessions, error.clone()) {
                    eprintln!("[Daemon] {error} for {device_id}. Marked unreachable.");
                }
                break;
            }
        }
    }
}

/// Applies a packet that arrived through an authenticated WebRTC envelope.
///
/// This intentionally starts with the state-only handlers that have no
/// dependency on the legacy TLS stream. Requests that need a reply, the legacy
/// payload socket, or the future portal input lease remain rejected until
/// their WebRTC-native implementations are complete. The coordinator never
/// sends `feature-ready` while any of those handlers are unavailable.
pub(crate) fn dispatch_webrtc_feature_packet(
    binding: &SessionBinding,
    packet: NetworkPacket,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    let local_info = config
        .lock()
        .map_err(|_| "DeskLink configuration lock poisoned".to_string())?
        .local_device_info();
    SharedPacketDispatcher::authorize(
        PacketSource::WebRtc,
        binding,
        sessions,
        &local_info,
        &packet,
    )
    .map_err(|error| error.to_string())?;

    let device_id = &binding.device_id;
    match packet.packet_type.as_str() {
        PACKET_TYPE_PING => {
            eprintln!("[Daemon] WebRTC ping received from {device_id}");
            push_event(
                events,
                DaemonEvent::PingReceived {
                    device_id: device_id.clone(),
                    device_name: binding.link.info.name.clone(),
                    message: packet.get_str("message").map(str::to_string),
                },
            );
            Ok(())
        }
        PACKET_TYPE_BATTERY => {
            update_battery_status(devices, device_id, &packet);
            Ok(())
        }
        PACKET_TYPE_CLIPBOARD | PACKET_TYPE_CLIPBOARD_CONNECT => {
            let content = packet
                .get_str("content")
                .ok_or_else(|| "DeskLink WebRTC clipboard packet has no content".to_string())?;
            set_clipboard_text_from_remote(content);
            Ok(())
        }
        PACKET_TYPE_SHARE_REQUEST => {
            if let Some(text) = packet.get_str("text") {
                set_clipboard_text_from_remote(text);
                Ok(())
            } else if let Some(url) = packet.get_str("url") {
                std::process::Command::new("xdg-open")
                    .arg(url)
                    .spawn()
                    .map_err(|error| format!("Could not open shared URL: {error}"))?;
                Ok(())
            } else {
                Err("DeskLink WebRTC file transfer requires the file-data channel".to_string())
            }
        }
        PACKET_TYPE_MPRIS => {
            update_media_from_packet(devices, device_id, &packet);
            Ok(())
        }
        PACKET_TYPE_NOTIFICATION => {
            if packet.get_bool("isCancel").unwrap_or(false) {
                let id = packet
                    .get_str("id")
                    .ok_or_else(|| "DeskLink WebRTC notification cancel has no id".to_string())?;
                remove_notification(devices, device_id, id);
            } else {
                upsert_notification(devices, device_id, &packet);
            }
            Ok(())
        }
        PACKET_TYPE_NOTIFICATION_REQUEST => {
            if let Some(cancel_id) = packet.get_str("cancel") {
                remove_notification(devices, device_id, cancel_id);
                Ok(())
            } else {
                Err("DeskLink WebRTC notification request needs a response handler".to_string())
            }
        }
        PACKET_TYPE_SYSTEMVOLUME => {
            update_volume_status(devices, device_id, &packet);
            Ok(())
        }
        PACKET_TYPE_RUNCOMMAND => {
            update_remote_commands(devices, device_id, &packet);
            Ok(())
        }
        PACKET_TYPE_SFTP => {
            update_sftp_status(devices, device_id, &packet);
            Ok(())
        }
        PACKET_TYPE_FINDMYPHONE_REQUEST => {
            std::process::Command::new("canberra-gtk-play")
                .args(["-i", "bell"])
                .spawn()
                .map_err(|error| format!("Could not play DeskLink find-device sound: {error}"))?;
            push_event(
                events,
                DaemonEvent::FindPhoneReceived {
                    device_id: device_id.clone(),
                    device_name: binding.link.info.name.clone(),
                },
            );
            Ok(())
        }
        PACKET_TYPE_MOUSEPAD_REQUEST => Err(
            "DeskLink WebRTC remote input requires a granted GNOME RemoteDesktop portal lease"
                .to_string(),
        ),
        PACKET_TYPE_LOCK | PACKET_TYPE_LOCK_REQUEST => {
            // Answer the same two questions the LAN reader answers, so the
            // phone's lock control behaves identically on both transports
            // instead of silently losing its confirmation after handover.
            //
            // Sampling the result costs a settle delay, and this runs on the
            // single worker that also carries SDP, ICE and file chunks — so the
            // wait and the reply happen on their own thread.
            let wants_state = packet.body.contains_key("requestLocked");
            let requested_lock = apply_lock_request(&packet);
            if wants_state || requested_lock.is_some() {
                let sessions = Arc::clone(sessions);
                let binding = binding.clone();
                thread::spawn(move || {
                    // A command was applied, so the memo cannot be trusted for
                    // the confirmation; a plain query may use it.
                    let locked = if requested_lock.is_some() {
                        thread::sleep(LOCK_SETTLE_DELAY);
                        sample_desktop_lock_state()
                    } else {
                        is_desktop_locked()
                    };
                    let Ok(transport) =
                        crate::device_links::transport::FeatureTransport::for_binding(
                            &sessions, &binding,
                        )
                    else {
                        return;
                    };
                    if requested_lock == Some(true) {
                        let mut acknowledgement = NetworkPacket::new(PACKET_TYPE_LOCK);
                        acknowledgement.set("lockResult", locked);
                        let _ = transport.send(&acknowledgement);
                    }
                    let mut state = NetworkPacket::new(PACKET_TYPE_LOCK);
                    state.set("isLocked", locked);
                    let _ = transport.send(&state);
                });
            }
            Ok(())
        }
        PACKET_TYPE_SYSTEMVOLUME_REQUEST
        | PACKET_TYPE_RUNCOMMAND_REQUEST
        | PACKET_TYPE_MPRIS_REQUEST
        | PACKET_TYPE_SFTP_REQUEST => {
            Err("DeskLink WebRTC request needs its corresponding desktop backend".to_string())
        }
        _ => Err(format!(
            "DeskLink has no active WebRTC handler for {}",
            packet.packet_type
        )),
    }
}

/// Applies a `setLocked` request to the desktop session.
///
/// Shared by the LAN reader and the WebRTC dispatcher. Keeping one
/// implementation is what stops the two transports from disagreeing about
/// whether a feature exists — the WebRTC arm used to reject this outright while
/// the LAN arm performed it, so the phone's Lock button worked or did nothing
/// depending on whether the handover had completed.
pub(crate) fn apply_lock_request(packet: &NetworkPacket) -> Option<bool> {
    // `setLocked` is the only command field. `isLocked` is a *status report*,
    // and accepting it as an instruction is how a peer's confirmation of a lock
    // the user asked for ends up locking this desktop too — the exact bug the
    // Android lock plugin had in the mirror direction.
    //
    // Keeping the check on the field rather than the packet type makes it
    // structurally safe: `desklink.lock` is kept out of `DESKTOP_INCOMING`, but
    // `desklink.lock.request` is accepted, and a status-shaped body arriving
    // under that type would otherwise still be obeyed.
    let requested_lock = packet.get_bool("setLocked")?;

    // One lock operation per settle window, process-wide.
    //
    // Each admitted request spawns up to five subprocesses — `loginctl`, two
    // `dbus-send`, `xdg-screensaver`, and one more `loginctl` for the
    // confirmation sample, which alone bypasses the state memo. Nothing else in
    // the feature profile spawns anything at all, so a peer
    // repeating this packet is uniquely amplifying: a stuck toggle or a retry
    // loop would flap the screensaver and bog the machine down. The LAN reader
    // was incidentally limited by its inline settle delay; the WebRTC path
    // moved that delay off the worker and so had no limit at all.
    //
    // This is a timestamp rather than an in-flight flag on purpose. A flag has
    // to be released by hand, and the release sits after `is_desktop_locked()`
    // — a `loginctl` call with no timeout. A wedged logind would then leak the
    // flag and disable locking for every device until the process restarted.
    // An elapsed-time check cannot leak, whatever hangs or panics.
    {
        let now = std::time::Instant::now();
        let mut last = LAST_LOCK_ACTION.lock().ok()?;
        if last.is_some_and(|last| now.duration_since(last) < LOCK_MIN_INTERVAL) {
            eprintln!("[Daemon] Ignoring a lock request received too soon after the last one");
            return None;
        }
        *last = Some(now);
    }
    eprintln!("[Daemon] Received setLocked command: {requested_lock}");

    let command = if requested_lock {
        "lock-session"
    } else {
        "unlock-session"
    };
    let _ = std::process::Command::new("loginctl").arg(command).spawn();

    if requested_lock {
        for args in [
            [
                "--session",
                "--dest=org.freedesktop.ScreenSaver",
                "--type=method_call",
                "/ScreenSaver",
                "org.freedesktop.ScreenSaver.Lock",
            ],
            [
                "--session",
                "--dest=org.gnome.ScreenSaver",
                "--type=method_call",
                "/org/gnome/ScreenSaver",
                "org.gnome.ScreenSaver.Lock",
            ],
        ] {
            let _ = std::process::Command::new("dbus-send").args(args).spawn();
        }
        let _ = std::process::Command::new("xdg-screensaver")
            .arg("lock")
            .spawn();
    } else {
        let _ = std::process::Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.gnome.ScreenSaver",
                "--type=method_call",
                "/org/gnome/ScreenSaver",
                "org.gnome.ScreenSaver.SetActive",
                "boolean:false",
            ])
            .spawn();
    }

    Some(requested_lock)
}
