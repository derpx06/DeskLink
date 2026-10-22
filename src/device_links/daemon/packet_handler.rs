use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::clipboard::{clipboard_from_phone, set_clipboard_text_from_remote};
use super::dispatcher::{PacketSource, SharedPacketDispatcher};
use super::handle::DaemonEvent;
use super::state::{
    push_event, remove_notification, replace_notifications, update_battery_status,
    update_call_status, update_keyboard_state, update_media_from_packet, update_pair_state,
    upsert_notification,
};
use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, ReadOutcome, SessionBinding};
use crate::device_links::device::DeviceView;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_HEARTBEAT, PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST, PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION, PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PAIR, PACKET_TYPE_PING, PACKET_TYPE_SETTINGS_REQUEST, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_TELEPHONY, PACKET_TYPE_UNLOCK_REQUEST, PACKET_TYPE_WEBRTC_SIGNAL_V1,
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
pub(crate) fn is_desktop_locked() -> bool {
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

/// Asks logind for our display session's lock state (bounded; see
/// [`crate::device_links::logind`]). Unknown reads as unlocked.
fn read_desktop_lock_state() -> bool {
    crate::device_links::logind::locked().unwrap_or(false)
}

/// How long the screensaver is given to settle before its state is sampled.
const LOCK_SETTLE_DELAY: Duration = Duration::from_millis(300);

/// How long a sampled lock state is reused before `loginctl` is asked again.
const LOCK_STATE_VALIDITY: Duration = Duration::from_millis(1_000);

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
    debug_log!(
        "[Daemon] Telling {} it is not paired with this device",
        binding.device_id
    );
    // Best effort by design: if this cannot be delivered the peer is gone, and
    // it will be told again the next time it sends anything.
    let _ = binding.link.send_packet(&packet);
}

/// The link kinds a phone may open here (as the phone allows the other way):
/// never `file:` or an app's own scheme.
fn openable_link(url: &str) -> bool {
    let scheme = url
        .split_once(':')
        .map(|(scheme, _)| scheme.to_ascii_lowercase());
    matches!(
        scheme.as_deref(),
        Some("http" | "https" | "mailto" | "tel" | "geo" | "sms")
    ) && url.len() <= 8192
        && !url.chars().any(char::is_control)
}

/// A phone's "Open on computer" (C3): the desktop's default handler opens
/// it, through GIO (no `xdg-open` needed).
fn open_link_from_phone(url: &str) -> Result<(), String> {
    if !openable_link(url) {
        return Err("DeskLink refused a link it does not open from a phone".to_string());
    }
    let url = url.to_string();
    gtk::glib::MainContext::default().invoke(move || {
        if let Err(error) =
            gtk::gio::AppInfo::launch_default_for_uri(&url, gtk::gio::AppLaunchContext::NONE)
        {
            eprintln!("[DeskLink] Could not open the phone's link: {error}");
        }
    });
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) fn apply_lan_packet(
    binding: &SessionBinding,
    packet: NetworkPacket,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    let binding = binding.clone();
    let devices = Arc::clone(devices);
    let sessions = Arc::clone(sessions);
    let config = Arc::clone(config);
    let events = Arc::clone(events);
    let device_id = binding.device_id.clone();

    // Any packet is proof of life, recorded before anything can reject it: a
    // peer that is talking to us is alive even when what it says is refused.
    //
    // This lives in the dispatch rather than in the framing loop deliberately.
    // Putting it in the loop made liveness a property of one particular reader,
    // so every other path that dispatches a packet — the two-peer harness, and
    // any future transport — silently lost it.
    sessions.note_inbound(&device_id);

    // A heartbeat is liveness and nothing else: it carries no body, expects no
    // reply, and must never reach the feature path. Falling through would let
    // `notify_peer_not_paired` answer it with `pair:false` and tell a perfectly
    // healthy peer it was unpaired, once every ten seconds — the mechanism that
    // keeps links alive turned into one that tears pairings down.
    if packet.packet_type == PACKET_TYPE_HEARTBEAT {
        // Its one field is a state report: whether the phone is dozing.
        if crate::device_links::webrtc::coordinator::note_heartbeat(&device_id, &packet) {
            // The phone woke: a sign of it, so a link that dropped while it
            // slept comes back at once.
            crate::device_links::webrtc::coordinator::recovery_sign(
                &device_id, &sessions, &config, &devices, &events,
            );
        }
        return;
    }

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
        // Signaling is the only thing a peer that still believes it is paired
        // sends now that features never travel over LAN. Answering it here is
        // what tells a peer that missed our unpair, which would otherwise read
        // "Connecting" forever — and refusing it here, before the coordinator,
        // means an unpaired peer can never make this side build a pipeline.
        let paired = sessions
            .with_session(&binding, |session| {
                session.pairing.state == PairState::Paired
            })
            .unwrap_or(false);
        if !paired {
            notify_peer_not_paired(&binding, &sessions);
            return;
        }
        if let Err(error) = crate::device_links::webrtc::coordinator::handle_signaling_packet(
            &binding,
            &packet,
            Arc::clone(&sessions),
            Arc::clone(&config),
            Arc::clone(&devices),
            Arc::clone(&events),
        ) {
            eprintln!("[Daemon] Rejected DeskLink WebRTC signaling: {error}");
            if error.contains("clocks differ") {
                crate::device_links::daemon::state::mark_error(&devices, &device_id, error);
            }
        }
        return;
    }

    if packet.packet_type == crate::device_links::packet::PACKET_TYPE_PAIR_QR {
        handle_qr_pairing_proof(&binding, &packet, &sessions, &config, &devices, &events);
        return;
    }

    if packet.packet_type == PACKET_TYPE_PAIR {
        debug_log!(
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
                debug_log!(
                    "[Daemon] Re-confirming an existing pairing for {}",
                    device_id
                );
                let mut acknowledgement = NetworkPacket::new(PACKET_TYPE_PAIR);
                acknowledgement.set("pair", true);
                let _ = binding.link.send_packet(&acknowledgement);
            }
            debug_log!(
                "[Daemon] Pairing state transitioned from {:?} to {:?}",
                previous_state,
                state
            );
            let mut state = state;
            if state == PairState::Paired {
                // Same rule as `accept_pair`: never `Paired` without a saved
                // trust record. Ignoring this error left the session paired
                // with nothing on disk, so the pairing vanished on restart.
                let trusted = config
                    .lock()
                    .map_err(|_| "Config lock poisoned".to_string())
                    .and_then(|mut config| {
                        config
                            .trust_device(&binding.link.info, binding.link.certificate_pem.clone())
                    });
                if let Err(error) = trusted {
                    eprintln!(
                        "[Daemon] Could not save trust for {device_id}: {error}; pairing reverted"
                    );
                    let _ = sessions.with_session(&binding, |session| {
                        session.pairing = crate::device_links::pairing::PairingHandler::new(false);
                    });
                    state = PairState::NotPaired;
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
            if state == PairState::Paired {
                start_webrtc_after_pairing(&binding, &sessions, &config, &devices, &events);
            }
        }
    } else {
        // Unreachable for features: `authorize` refuses every non-bootstrap
        // type on LAN. The old feature arms here (xdg-open, lock, clipboard…)
        // were deleted so a future change to `authorize` cannot silently
        // re-expose them on the unauthenticated-handover path.
        debug_log!("[Daemon] Unhandled packet type: {}", packet.packet_type);
    }
}

fn start_webrtc_after_pairing(
    binding: &SessionBinding,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    if !binding
        .link
        .info
        .incoming_capabilities
        .iter()
        .any(|capability| capability == PACKET_TYPE_WEBRTC_SIGNAL_V1)
    {
        return;
    }
    if let Err(error) = crate::device_links::webrtc::coordinator::start_initiator(
        binding.clone(),
        Arc::clone(sessions),
        Arc::clone(config),
        Arc::clone(devices),
        Arc::clone(events),
    ) {
        eprintln!("[Daemon] DeskLink WebRTC negotiation unavailable: {error}");
    }
}

/// A phone that scanned this desktop's QR code proves it here. Valid ⇒ paired
/// immediately, with the same trust record the prompt flow writes; anything
/// else ⇒ `pair:false`. Nothing is compared or accepted by the user: the
/// scanned fingerprint and one-time secret are the authentication.
/// Checks a `desklink.pair.qr` proof from `link` against the live code
/// (consuming it on success).
pub(super) fn check_qr_proof(
    link: &crate::device_links::core::SessionLink,
    packet: &NetworkPacket,
    config: &Arc<Mutex<Config>>,
) -> Result<(), String> {
    use crate::device_links::pairing_qr;
    // The phone's fingerprint comes from the certificate this link really
    // presented, never from anything the phone claims.
    let phone_der = openssl::x509::X509::from_pem(link.certificate_pem.as_bytes())
        .and_then(|certificate| certificate.to_der())
        .map_err(|error| error.to_string())?;
    let (desktop_id, desktop_der) = {
        let config = config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())?;
        let der = config
            .certificate()
            .to_der()
            .map_err(|error| error.to_string())?;
        (config.local_device_info().id, der)
    };
    pairing_qr::verify(
        &desktop_id,
        &link.info.id,
        &pairing_qr::fingerprint(&desktop_der)?,
        &pairing_qr::fingerprint(&phone_der)?,
        packet.get_str("mac").unwrap_or_default(),
    )
}

fn handle_qr_pairing_proof(
    binding: &SessionBinding,
    packet: &NetworkPacket,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    let device_id = &binding.device_id;
    let verified = check_qr_proof(&binding.link, packet, config).and_then(|()| {
        let _ = sessions.with_session(binding, |session| {
            session.pairing = crate::device_links::pairing::PairingHandler::new(true);
        });
        let trusted = config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())
            .and_then(|mut config| {
                config.trust_device(&binding.link.info, binding.link.certificate_pem.clone())
            });
        if trusted.is_err() {
            let _ = sessions.with_session(binding, |session| {
                session.pairing = crate::device_links::pairing::PairingHandler::new(false);
            });
        }
        trusted
    });
    // A bad proof from a device that is already paired must not answer
    // pair:false: the phone takes that as an unpair, so a stale or mistyped
    // scan would silently destroy a working pairing.
    let already_paired = sessions
        .with_session(binding, |session| {
            session.pairing.state == PairState::Paired
        })
        .unwrap_or(false);
    if verified.is_ok() || !already_paired {
        let mut reply = NetworkPacket::new(PACKET_TYPE_PAIR);
        reply.set("pair", verified.is_ok());
        let _ = binding.link.send_packet(&reply);
    }
    match verified {
        Ok(()) => {
            debug_log!("[Daemon] Paired with {device_id} by QR code");
            update_pair_state(devices, device_id, PairState::Paired, None);
            super::state::push_event(
                events,
                DaemonEvent::PairedByQr {
                    device_id: device_id.clone(),
                    device_name: binding.link.info.name.clone(),
                },
            );
            start_webrtc_after_pairing(binding, sessions, config, devices, events);
        }
        Err(error) => {
            eprintln!("[Daemon] QR pairing from {device_id} refused: {error}");
            // In plain words on the phone's card (see `friendly_error`).
            super::state::mark_error(devices, device_id, error);
        }
    }
}

/// Reads `binding`'s socket number `epoch` (see `SessionLink::epoch`) until
/// it ends or the link adopts a newer one.
pub(super) fn packet_read_loop(
    binding: SessionBinding,
    epoch: u64,
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
    let mut line = Vec::new();
    // Once the link adopts a newer socket, this reader's socket ending is
    // not the link ending.
    let superseded = || binding.link.epoch() != epoch;

    loop {
        if !sessions.is_current(&binding) || superseded() {
            break;
        }
        match binding.link.read_byte_at(epoch) {
            Ok(ReadOutcome::Closed | ReadOutcome::Idle) | Err(_) if superseded() => break,
            Ok(ReadOutcome::Closed) => {
                if super::handshake::handle_reader_disconnect(
                    &binding,
                    epoch,
                    &devices,
                    &sessions,
                    "Peer closed the connection".to_string(),
                ) {
                    debug_log!(
                        "[Daemon] Active link closed for {}. Marked unreachable.",
                        device_id
                    );
                }
                break;
            }
            Ok(ReadOutcome::Byte(value)) => {
                line.push(value);
                // An unpaired peer only sends identity, pairing and signaling,
                // all small; letting it stream 32 MiB lines (times every such
                // peer) was a memory-exhaustion vector before any trust.
                let limit = if line.len() > super::network::MAX_UNPAIRED_PACKET_BYTES
                    && sessions
                        .with_session(&binding, |session| {
                            session.pairing.state != PairState::Paired
                        })
                        .unwrap_or(true)
                {
                    super::network::MAX_UNPAIRED_PACKET_BYTES
                } else {
                    32 * 1024 * 1024
                };
                if line.len() > limit {
                    debug_log!(
                        "[Daemon] Read line too long for {}. Disconnecting.",
                        device_id
                    );
                    super::handshake::handle_reader_disconnect(
                        &binding,
                        epoch,
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
                    debug_log!(
                        "[DL-WRTC-BOOT] received signed bootstrap signal from {}",
                        device_id
                    );
                }

                if !sessions.is_current(&binding) {
                    break;
                }

                apply_lan_packet(&binding, packet, &devices, &sessions, &config, &events);
            }
            Ok(ReadOutcome::Idle) => {
                // Yield briefly so a concurrent feature send can take the
                // transport's lock instead of contending with the next read.
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => {
                if super::handshake::handle_reader_disconnect(
                    &binding,
                    epoch,
                    &devices,
                    &sessions,
                    error.clone(),
                ) {
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
    // Type only, never the body: clipboard and notification text stay out of
    // the journal. This line is what the device E2E script asserts on.
    debug_log!(
        "[Daemon] WebRTC feature {} from {device_id}",
        packet.packet_type
    );
    match packet.packet_type.as_str() {
        PACKET_TYPE_PING => {
            debug_log!("[Daemon] WebRTC ping received from {device_id}");
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
            // `connect` (every connection) carries when the phone's copy was
            // made; a plain `clipboard` is a copy made now.
            let connect = (packet.packet_type == PACKET_TYPE_CLIPBOARD_CONNECT).then(|| {
                packet
                    .body
                    .get("timestamp")
                    .and_then(serde_json::Value::as_u64)
            });
            // Off means off both ways: the phone's copies are dropped there.
            let outcome = clipboard_from_phone(content, &binding.link.info.name, connect);
            debug_log!(
                "[Clipboard] {} from {device_id}: {} chars, {outcome}",
                packet.packet_type,
                content.chars().count()
            );
            Ok(())
        }
        PACKET_TYPE_SHARE_REQUEST => {
            if let Some(text) = packet.get_str("text") {
                set_clipboard_text_from_remote(text);
                Ok(())
            } else if let Some(url) = packet.get_str("url") {
                open_link_from_phone(url)
            } else {
                Err("DeskLink WebRTC file transfer requires the file-data channel".to_string())
            }
        }
        PACKET_TYPE_MPRIS => {
            update_media_from_packet(devices, device_id, &packet);
            if packet.get_str("rev").is_some() {
                crate::device_links::sync::note_received(
                    device_id,
                    binding.link.info.instance_id.as_deref(),
                    "media",
                    &packet,
                );
            } else {
                // A live change: what was remembered no longer describes the
                // media shown, and the phone's next snapshot may carry the
                // very rev that was.
                crate::device_links::sync::forget_feature(device_id, "media");
            }
            Ok(())
        }
        PACKET_TYPE_NOTIFICATION => {
            if packet.get_bool("snapshot").unwrap_or(false) {
                let missing = replace_notifications(devices, device_id, &packet);
                // Remembered once every id has its body here, not on the ids.
                crate::device_links::sync::note_when_complete(
                    device_id,
                    binding.link.info.instance_id.as_deref(),
                    "notifications",
                    &packet,
                    missing,
                );
            } else if packet.get_bool("isCancel").unwrap_or(false) {
                let id = packet
                    .get_str("id")
                    .ok_or_else(|| "DeskLink WebRTC notification cancel has no id".to_string())?;
                remove_notification(devices, device_id, id);
                crate::device_links::sync::item_changed(device_id, "notifications", None);
            } else {
                let stored = upsert_notification(devices, device_id, &packet);
                // The id a snapshot was waiting for completes it; any other
                // upsert changes the list, so its remembered rev goes.
                crate::device_links::sync::item_changed(
                    device_id,
                    "notifications",
                    stored.as_ref().map(|notification| notification.id.as_str()),
                );
            }
            Ok(())
        }
        // The phone swiped away one of this computer's notifications (C1):
        // it is closed here too. Nothing else is asked this way.
        PACKET_TYPE_NOTIFICATION_REQUEST => {
            // The phone asks once on load (`request: true`): the snapshot.
            match packet.get_str("cancel") {
                Some(id) => crate::device_links::desk_notifications::close_from_phone(id),
                None if packet.get_bool("request") == Some(true) => {
                    crate::device_links::desk_notifications::send_snapshot(Some(device_id))
                }
                None => debug_log!("[Notifications] Ignored a notification request"),
            }
            Ok(())
        }
        // A snapshot, a missed call or a refusal: a report, never an
        // instruction (nothing here answers or ends a call).
        PACKET_TYPE_TELEPHONY => {
            use crate::device_links::calls;
            let named = crate::device_links::config::app_settings().names_callers_from(device_id);
            let lookup = |uid: Option<&str>, number: Option<&str>| {
                crate::device_links::contacts::caller_name(device_id, uid, number)
            };
            if let Some(mut status) = calls::parse_status(&packet) {
                if let Some(status) = status.as_mut().filter(|_| named) {
                    calls::name_callers(status, &calls::contact_uids(&packet), lookup);
                }
                let in_call = update_call_status(devices, device_id, status);
                calls::hold_media(device_id, in_call);
                // The card, the bar and the notification follow within a frame.
                push_event(
                    events,
                    DaemonEvent::CallChanged {
                        device_id: device_id.clone(),
                    },
                );
            } else if let Some(missed) = calls::parse_missed(&packet) {
                push_event(
                    events,
                    DaemonEvent::MissedCall {
                        device_id: device_id.clone(),
                        device_name: binding.link.info.name.clone(),
                        caller: missed.contact_name.or_else(|| {
                            named
                                .then(|| {
                                    lookup(missed.contact_uid.as_deref(), missed.number.as_deref())
                                })
                                .flatten()
                        }),
                        number: missed.number,
                    },
                );
            } else if let Some(message) = calls::parse_error(&packet) {
                push_event(events, DaemonEvent::CallRefused { message });
            }
            Ok(())
        }
        // The phone's contacts (F5): its snapshot, then the vCards asked for.
        // Decrypting and saving the cache stays off the peer worker.
        crate::device_links::packet::PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS => {
            let (device_id, packet) = (device_id.clone(), packet.clone());
            let instance = binding.link.info.instance_id.clone();
            crate::device_links::desk_features::run_later_reliably(move || {
                for request in crate::device_links::contacts::on_snapshot(
                    &device_id,
                    instance.as_deref(),
                    &packet,
                ) {
                    crate::device_links::desk_features::send(&device_id, &request);
                }
            });
            Ok(())
        }
        // Folder sync (F8): to its own worker.
        crate::device_links::packet::PACKET_TYPE_FOLDERS => {
            crate::device_links::folders::on_packet(device_id, &packet);
            Ok(())
        }
        // Delta resync (W2): what the phone already holds from this desk.
        crate::device_links::packet::PACKET_TYPE_SYNC_HAVE => {
            let (device_id, packet) = (device_id.clone(), packet.clone());
            // The connection it arrived on, taken now: the job runs later.
            let connection = sessions.active_webrtc_attempt(binding).ok().flatten();
            crate::device_links::desk_features::run_later_reliably(move || {
                crate::device_links::desk_features::on_sync_have(&device_id, &packet, &connection)
            });
            Ok(())
        }
        // Glide (F4): the phone hands the pointer back.
        crate::device_links::packet::PACKET_TYPE_SHARE_INPUT_DEVICES => {
            crate::device_links::glide::phone_released(device_id, &packet);
            Ok(())
        }
        // Snippets and notes (F6): index, fetch or items, both ways.
        crate::device_links::packet::PACKET_TYPE_SNIPPETS
        | crate::device_links::packet::PACKET_TYPE_NOTES => {
            let (device_id, packet) = (device_id.clone(), packet.clone());
            let instance = binding.link.info.instance_id.clone();
            crate::device_links::desk_features::run_later_reliably(move || {
                let answers = crate::device_links::notes::on_packet(&device_id, &packet);
                // An index with nothing to fetch is fully applied: remembered
                // for the next resync.
                let feature =
                    if packet.packet_type == crate::device_links::packet::PACKET_TYPE_SNIPPETS {
                        "snippets"
                    } else {
                        "notes"
                    };
                if packet.get_str("op") == Some("index") {
                    if answers
                        .iter()
                        .all(|answer| answer.get_str("op") != Some("fetch"))
                    {
                        crate::device_links::sync::note_received(
                            &device_id,
                            instance.as_deref(),
                            feature,
                            &packet,
                        );
                    } else {
                        // Partly applied: what was remembered of the older
                        // index must not be claimed any more.
                        crate::device_links::sync::forget_feature(&device_id, feature);
                    }
                } else {
                    // An item or a change arrived live.
                    crate::device_links::sync::forget_feature(&device_id, feature);
                }
                for answer in answers {
                    crate::device_links::desk_features::send(&device_id, &answer);
                }
            });
            Ok(())
        }
        crate::device_links::packet::PACKET_TYPE_CONTACTS_RESPONSE_VCARDS => {
            // Its own queue: a first sync is a burst of hundreds of replies
            // to requests this desk made (the shared desk queue holds 16).
            crate::device_links::contacts::queue_vcards(device_id, packet.clone());
            Ok(())
        }
        PACKET_TYPE_FINDMYPHONE_REQUEST => {
            find_device_received(events, device_id, &binding.link.info.name, spawn_player);
            Ok(())
        }
        // A report of the phone's focus ("Type on phone"), never an input.
        crate::device_links::packet::PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE => {
            update_keyboard_state(
                devices,
                device_id,
                packet.get_bool("state"),
                packet.get_bool("allowed"),
            );
            Ok(())
        }
        PACKET_TYPE_MOUSEPAD_REQUEST => {
            crate::device_links::webrtc::coordinator::dispatch_desktop_remote_input(
                binding, &packet,
            )
        }
        PACKET_TYPE_LOCK | PACKET_TYPE_LOCK_REQUEST => {
            // Answer the same two questions the LAN reader answers, so the
            // phone's lock control behaves identically on both transports
            // instead of silently losing its confirmation after handover.
            //
            // Sampling the result costs a settle delay, and this runs on the
            // single worker that also carries SDP, ICE and file chunks — so the
            // wait and the reply happen on their own thread.
            // An unlock is never a plain request: only the phone's approval of
            // the desk's own nonce, through every check in `unlock`.
            if packet.get_bool("setLocked") == Some(false) {
                let (device_id, nonce) = (
                    device_id.clone(),
                    packet.get_str("nonce").map(str::to_string),
                );
                crate::device_links::desk_features::run_later(move || {
                    crate::device_links::desk_features::handle_unlock_approval(
                        &device_id,
                        nonce.as_deref(),
                    )
                });
                return Ok(());
            }
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
        PACKET_TYPE_MPRIS_REQUEST => {
            // D-Bus calls to the players: off the peer worker.
            let (device_id, packet) = (device_id.clone(), packet.clone());
            crate::device_links::desk_features::run_later(move || {
                if let Err(error) = crate::device_links::mpris::handle_request(&device_id, &packet)
                {
                    debug_log!("[DeskLink] Media request from {device_id} failed: {error}");
                }
            });
            Ok(())
        }
        PACKET_TYPE_SETTINGS_REQUEST => {
            let (device_id, packet) = (device_id.clone(), packet.clone());
            crate::device_links::desk_features::run_later(move || {
                crate::device_links::desk_features::handle_settings_request(&device_id, &packet)
            });
            Ok(())
        }
        PACKET_TYPE_UNLOCK_REQUEST => {
            // Reads the lock state (loginctl): off the peer worker.
            let (device_id, packet) = (device_id.clone(), packet.clone());
            crate::device_links::desk_features::run_later(move || {
                crate::device_links::desk_features::handle_unlock_request(&device_id, &packet)
            });
            Ok(())
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
    // Unlocking is never taken from a packet: any paired phone could open this
    // desktop. Unlock with the phone needs its own opt-in and checks.
    if !requested_lock {
        debug_log!("[Daemon] Ignoring a request to unlock this desktop");
        return None;
    }

    if !admit_lock_operation() {
        return None;
    }
    debug_log!("[Daemon] Received setLocked command: {requested_lock}");
    // logind is asked synchronously; not on the peer worker.
    thread::spawn(lock_desktop);
    Some(requested_lock)
}

/// One lock or unlock operation per settle window, process-wide: whether
/// this one may go ahead.
pub(crate) fn admit_lock_operation() -> bool {
    // A peer repeating lock or unlock packets would otherwise flap the
    // screensaver and hammer logind. A timestamp rather than an in-flight
    // flag: a flag has to be released by hand, and a wedged logind would leak
    // it and disable locking for good. An elapsed-time check cannot leak.
    {
        let now = std::time::Instant::now();
        let Ok(mut last) = LAST_LOCK_ACTION.lock() else {
            return false;
        };
        if last.is_some_and(|last| now.duration_since(last) < LOCK_MIN_INTERVAL) {
            debug_log!("[Daemon] Ignoring a lock request received too soon after the last one");
            return false;
        }
        *last = Some(now);
    }
    true
}

/// Locks this desktop session, every way a desktop might listen.
pub(crate) fn lock_desktop() {
    match crate::device_links::logind::lock() {
        Ok(()) => return,
        Err(error) => debug_log!("[Daemon] logind could not lock the session: {error}"),
    }
    // Without logind (or refused): the freedesktop screensaver interface
    // every desktop's locker implements (KDE, Xfce, Cinnamon, MATE), then
    // GNOME's own name. Over D-Bus in-process: no helper program needed.
    let Ok(bus) = gtk::gio::bus_get_sync(gtk::gio::BusType::Session, gtk::gio::Cancellable::NONE)
    else {
        return;
    };
    for (name, path, interface) in [
        (
            "org.freedesktop.ScreenSaver",
            "/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        ),
        (
            "org.freedesktop.ScreenSaver",
            "/org/freedesktop/ScreenSaver",
            "org.freedesktop.ScreenSaver",
        ),
        (
            "org.gnome.ScreenSaver",
            "/org/gnome/ScreenSaver",
            "org.gnome.ScreenSaver",
        ),
    ] {
        let locked = bus.call_sync(
            Some(name),
            path,
            interface,
            "Lock",
            None,
            None,
            gtk::gio::DBusCallFlags::NONE,
            2_000,
            gtk::gio::Cancellable::NONE,
        );
        if locked.is_ok() {
            return;
        }
    }
}

/// The phone asked this computer to make itself known. The notification is
/// the part that matters, so it goes out first; the sound is best effort and
/// tries each player in turn, since none of them is guaranteed installed.
fn find_device_received(
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
    device_id: &str,
    device_name: &str,
    mut spawn: impl FnMut(&str, &[&str]) -> std::io::Result<()>,
) {
    push_event(
        events,
        DaemonEvent::FindPhoneReceived {
            device_id: device_id.to_string(),
            device_name: device_name.to_string(),
        },
    );
    if spawn("canberra-gtk-play", &["-i", "bell"]).is_ok() {
        return;
    }
    let Some(sound) = [
        "/usr/share/sounds/freedesktop/stereo/bell.oga",
        "/usr/share/sounds/freedesktop/stereo/complete.oga",
    ]
    .into_iter()
    .find(|file| std::path::Path::new(file).exists()) else {
        return;
    };
    let _ = spawn("paplay", &[sound]).or_else(|_| spawn("pw-play", &[sound]));
}

/// Starts `program` and reaps it in the background.
fn spawn_player(program: &str, args: &[&str]) -> std::io::Result<()> {
    let mut child = std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {

    #[test]
    fn only_web_mail_phone_and_map_links_open_from_a_phone() {
        for url in [
            "https://example.com/a?b",
            "HTTP://x.org",
            "mailto:a@b.c",
            "tel:+15550100",
            "geo:1,2",
            "sms:555",
        ] {
            assert!(super::openable_link(url), "{url}");
        }
        for url in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "steam://run/1",
            "no-scheme",
            "https://a\nb",
        ] {
            assert!(!super::openable_link(url), "{url}");
        }
    }

    #[test]
    fn a_request_to_unlock_is_refused() {
        let mut packet = crate::device_links::packet::NetworkPacket::new(
            crate::device_links::packet::PACKET_TYPE_LOCK_REQUEST,
        );
        packet.set("setLocked", false);
        assert_eq!(super::apply_lock_request(&packet), None);
    }

    #[test]
    fn find_device_notifies_even_with_no_sound_player() {
        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut tried = Vec::new();
        super::find_device_received(&events, "phone", "Pixel", |program, _| {
            tried.push(program.to_string());
            Err(std::io::ErrorKind::NotFound.into())
        });
        assert!(matches!(
            events.lock().unwrap().as_slice(),
            [super::DaemonEvent::FindPhoneReceived { device_id, device_name }]
                if device_id == "phone" && device_name == "Pixel"
        ));
        assert_eq!(tried.first().map(String::as_str), Some("canberra-gtk-play"));
    }

    /// A paired peer must never be able to drive this machine's keyboard or
    /// pointer over the LAN bootstrap link. Input is permitted only on the
    /// authenticated WebRTC path, behind a portal lease the local user approved.
    ///
    /// The LAN arm used to inject input directly. This pins its absence: the
    /// input-synthesis crate may not be a dependency, and no code line in the
    /// LAN dispatcher may mention it.
    #[test]
    fn the_lan_path_has_no_input_injector() {
        let injector = concat!("en", "igo");

        let manifest = include_str!("../../../Cargo.toml");
        assert!(
            !manifest.contains(injector),
            "the input injector crate must not be a dependency"
        );

        let source = include_str!("packet_handler.rs");
        let offending: Vec<&str> = source
            .lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .filter(|line| line.contains(injector))
            .filter(|line| !line.contains("concat!"))
            .collect();
        assert!(
            offending.is_empty(),
            "LAN input injector is back: {offending:?}"
        );
    }
}
