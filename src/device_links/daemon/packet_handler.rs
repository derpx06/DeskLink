use enigo::{Axis, Button, Coordinate, Direction, Key};
use openssl::ssl::SslStream;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::clipboard::set_clipboard_text_from_remote;
use super::file_transfer::{receive_file_payload, ReceiveFilePersistence, ReceiveFileRequest};
use super::handshake::handle_disconnect;
use super::screen_stream::receive_screen_frame_payload;
use super::screen_stream::start_desktop_screen_stream;
use super::state::{
    mark_error, publish_device_changed, remove_notification, update_battery_status,
    update_connectivity_status, update_contacts, update_media_from_packet, update_pair_state,
    update_remote_commands, update_screen_frame, update_sftp_status, update_sms_messages,
    update_telephony_status, update_volume_status, upsert_notification,
};
use crate::device_links::config::Config;
use crate::device_links::core::device_manager::{DeviceManager, SessionBinding};
use crate::device_links::core::events::{CoreEvent, EventBus};
use crate::device_links::core::packet_router::{PacketDirection, PacketRouter};
use crate::device_links::core::transfer_manager::{
    TransferCheckpointStore, TransferManager, TransferState,
};
use crate::device_links::device::DeviceView;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_CONNECTIVITY_REPORT, PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS,
    PACKET_TYPE_CONTACTS_RESPONSE_VCARDS, PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST, PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION, PACKET_TYPE_NOTIFICATION_CANCEL,
    PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PAIR, PACKET_TYPE_PING, PACKET_TYPE_PRESENTER,
    PACKET_TYPE_RUNCOMMAND, PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SCREEN_ERROR,
    PACKET_TYPE_SCREEN_FRAME, PACKET_TYPE_SCREEN_READY, PACKET_TYPE_SCREEN_REQUEST,
    PACKET_TYPE_SCREEN_STOP, PACKET_TYPE_SFTP, PACKET_TYPE_SHARE_REQUEST, PACKET_TYPE_SMS_MESSAGES,
    PACKET_TYPE_SYSTEMVOLUME, PACKET_TYPE_SYSTEMVOLUME_REQUEST, PACKET_TYPE_TELEPHONY,
};
use crate::device_links::pairing::PairState;
use crate::device_links::plugins::{
    connectivity, contacts, mpris, presenter, run_commands, sms, telephony, volume,
};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerMotion {
    None,
    Relative { dx: i32, dy: i32 },
    Absolute { x: i32, y: i32 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketAuthorization {
    PairingAllowed,
    PairedFeatureAllowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PacketAuthorizationError {
    DeviceNotPaired,
    PacketNotAllowedBeforePairing,
    UnknownDevice,
    UnsupportedPacket,
}

/// Authorize packets before they reach any normal feature handler.
///
/// Pairing is the only pre-pairing allowlisted packet. Every ordinary feature
/// packet requires the active link to be paired; an absent link is denied as
/// an unknown device.
fn authorize_incoming_packet(
    pair_state: Option<PairState>,
    packet: &NetworkPacket,
    local_incoming_capabilities: &[String],
) -> Result<PacketAuthorization, PacketAuthorizationError> {
    let Some(pair_state) = pair_state else {
        return Err(PacketAuthorizationError::UnknownDevice);
    };

    if packet.packet_type == PACKET_TYPE_PAIR {
        return Ok(PacketAuthorization::PairingAllowed);
    }

    if pair_state == PairState::Paired {
        let router = PacketRouter::new(local_incoming_capabilities.to_vec(), Vec::<String>::new());
        if router.authorize(packet, PacketDirection::Incoming).is_ok() {
            return Ok(PacketAuthorization::PairedFeatureAllowed);
        }
        return Err(PacketAuthorizationError::UnsupportedPacket);
    }

    Err(match pair_state {
        PairState::NotPaired => PacketAuthorizationError::DeviceNotPaired,
        PairState::Requested | PairState::RequestedByPeer => {
            PacketAuthorizationError::PacketNotAllowedBeforePairing
        }
        PairState::Paired => unreachable!("paired state was handled above"),
    })
}

pub(super) fn packet_read_loop(
    binding: SessionBinding,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    config: Arc<Mutex<Config>>,
    events: EventBus,
    transfer_cancellations: Arc<Mutex<HashSet<String>>>,
    sessions: DeviceManager,
) {
    let device_id = binding.device_id.clone();
    let stream = Arc::clone(&binding.link.stream);
    // Do not request a RemoteDesktop/EIS permission session while the daemon
    // is merely connected. The first authorized input packet starts the
    // backend and therefore makes the permission request user-driven.
    let mut input_backend: Option<crate::platform::wayland_remote_desktop::RemoteInputBackend> =
        None;
    // A portal denial/closure is a session state, not a reason to prompt on
    // every subsequent mouse packet. A deliberate new remote-control session
    // creates a fresh reader and resets this state.
    let mut input_backend_failed = false;
    let mut desktop_screen_stream: Option<Arc<AtomicBool>> = None;
    let mut line = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        if !sessions.is_current(&binding) || binding.cancellation.load(Ordering::SeqCst) {
            stop_desktop_screen_stream(&mut desktop_screen_stream);
            break;
        }
        let read_result = {
            let Ok(mut locked_stream) = stream.lock() else {
                handle_disconnect(&binding, &sessions, &devices, &events);
                break;
            };
            locked_stream.read(&mut byte)
        };

        match read_result {
            Ok(0) => {
                stop_desktop_screen_stream(&mut desktop_screen_stream);
                if handle_disconnect(&binding, &sessions, &devices, &events) {
                    eprintln!(
                        "[Daemon] Active link closed for {}. Marked unreachable.",
                        device_id
                    );
                }
                break;
            }
            Ok(_) => {
                line.push(byte[0]);
                if line.len() > 32 * 1024 * 1024 {
                    eprintln!(
                        "[Daemon] Read line too long for {}. Disconnecting.",
                        device_id
                    );
                    events.publish(CoreEvent::Error {
                        scope: "protocol".to_string(),
                        device_id: Some(device_id.clone()),
                        message: "Incoming packet exceeded the maximum size".to_string(),
                        retryable: false,
                    });
                    stop_desktop_screen_stream(&mut desktop_screen_stream);
                    handle_disconnect(&binding, &sessions, &devices, &events);
                    break;
                }
                if byte[0] != b'\n' {
                    continue;
                }

                let packet = NetworkPacket::deserialize(&line);
                line.clear();
                let packet = match packet {
                    Ok(packet) => packet,
                    Err(error) => {
                        events.publish(CoreEvent::Error {
                            scope: "protocol".to_string(),
                            device_id: Some(device_id.clone()),
                            message: format!("Malformed packet rejected: {error}"),
                            retryable: false,
                        });
                        continue;
                    }
                };

                // A newer authenticated connection may have replaced this
                // reader while it was decoding the packet. Do not let a
                // stale generation expire pairing state or touch device/UI
                // state after replacement.
                if !sessions.is_current(&binding) {
                    break;
                }

                let pairing_expired = match sessions.with_session(&binding, |session| {
                    session
                        .pairing
                        .lock()
                        .map(|mut pairing| pairing.expire_if_needed())
                        .unwrap_or(false)
                }) {
                    Ok(expired) => expired,
                    Err(_) => break,
                };
                let pair_state =
                    match sessions.with_session(&binding, |session| session.pair_state()) {
                        Ok(state) => state,
                        Err(_) => break,
                    };
                if pairing_expired {
                    update_pair_state(&devices, &device_id, pair_state, None);
                }
                let pair_state = Some(pair_state);
                let local_incoming_capabilities = config
                    .lock()
                    .ok()
                    .map(|config| config.local_device_info().incoming_capabilities)
                    .unwrap_or_default();
                if let Err(error) =
                    authorize_incoming_packet(pair_state, &packet, &local_incoming_capabilities)
                {
                    eprintln!(
                        "[Daemon] Rejected unauthorized packet: peer_id={} packet_type={} pair_state={:?} authorization_error={:?}",
                        device_id, packet.packet_type, pair_state, error
                    );
                    events.publish(CoreEvent::Error {
                        scope: "authorization".to_string(),
                        device_id: Some(device_id.clone()),
                        message: format!("Rejected packet {}: {error:?}", packet.packet_type),
                        retryable: false,
                    });
                    continue;
                }

                if !sessions.is_current(&binding) {
                    break;
                }

                // All non-pairing packets must pass the paired-device
                // authorization gate before reaching feature handlers.
                if packet.packet_type == PACKET_TYPE_PAIR {
                    eprintln!(
                        "[Daemon] Received pair packet from device {}: pair={:?}, timestamp={:?}",
                        device_id,
                        packet.get_bool("pair"),
                        packet.get_i64("timestamp")
                    );
                    {
                        let link = &binding.link;
                        {
                            let transition = sessions.with_session(&binding, |session| {
                                let previous_state = session.pair_state();
                                if let Ok(mut pairing) = session.pairing.lock() {
                                    pairing.receive(&packet);
                                }
                                (previous_state, session.pair_state())
                            });
                            let Ok((previous_state, current_state)) = transition else {
                                break;
                            };
                            eprintln!(
                                "[Daemon] Pairing state transitioned from {:?} to {:?}",
                                previous_state, current_state
                            );
                            if current_state == PairState::Paired {
                                if let Ok(mut config) = config.lock() {
                                    eprintln!("[Daemon] Trusting device {}", device_id);
                                    let _ = config
                                        .trust_device(&link.info, link.certificate_pem.clone());
                                }
                            } else if previous_state == PairState::Paired
                                && current_state == PairState::NotPaired
                            {
                                if let Ok(mut config) = config.lock() {
                                    eprintln!(
                                        "[Daemon] Untrusting device {} due to state transition",
                                        device_id
                                    );
                                    let _ = config.untrust_device(&device_id);
                                }
                            }
                            let key = if current_state == PairState::RequestedByPeer {
                                sessions.verification_key(&binding).ok()
                            } else {
                                None
                            };
                            let state = current_state;
                            update_pair_state(&devices, &device_id, state, key);
                            events.publish(CoreEvent::PairingChanged {
                                device_id: device_id.clone(),
                                state,
                            });
                        }
                    }
                } else if packet.packet_type == PACKET_TYPE_PING {
                    eprintln!(
                        "[Daemon] Ping received from {}: {:?}",
                        device_id,
                        packet.get_str("message")
                    );
                } else if packet.packet_type == PACKET_TYPE_SCREEN_FRAME {
                    if let (Some(size), Some(info), Some(transfer_token)) = (
                        packet.payload_size,
                        &packet.payload_transfer_info,
                        packet.get_str("transferToken").map(str::to_string),
                    ) {
                        let port = info
                            .get("port")
                            .and_then(|value| value.as_i64())
                            .unwrap_or(0);
                        if port == 0 || port > u16::MAX as i64 {
                            eprintln!("[Daemon] Rejecting screen frame with invalid payload port");
                            continue;
                        }
                        if let Ok(Ok(peer_addr)) =
                            stream.lock().map(|value| value.get_ref().peer_addr())
                        {
                            let device_id_clone = device_id.clone();
                            let peer_ip = peer_addr.ip().to_string();
                            let devices_clone = Arc::clone(&devices);
                            let config_clone = Arc::clone(&config);
                            thread::spawn(move || {
                                match receive_screen_frame_payload(
                                    &device_id_clone,
                                    &peer_ip,
                                    port as u16,
                                    size,
                                    &transfer_token,
                                    config_clone,
                                ) {
                                    Ok(frame) => {
                                        update_screen_frame(&devices_clone, &device_id_clone, frame)
                                    }
                                    Err(error) => {
                                        eprintln!("[Daemon] Screen frame failed: {error}")
                                    }
                                }
                            });
                        }
                    } else {
                        eprintln!(
                            "[Daemon] Rejecting screen frame without an authenticated payload"
                        );
                    }
                } else if packet.packet_type == PACKET_TYPE_SCREEN_READY {
                    eprintln!("[Daemon] Phone screen stream is ready for {device_id}");
                } else if packet.packet_type == PACKET_TYPE_SCREEN_ERROR {
                    let error = packet
                        .get_str("message")
                        .unwrap_or("Phone screen stream failed")
                        .to_string();
                    mark_error(&devices, &device_id, error.clone());
                    eprintln!("[Daemon] Phone screen stream failed for {device_id}: {error}");
                } else if packet.packet_type == PACKET_TYPE_SCREEN_REQUEST {
                    if packet.get_str("role") == Some("desktop-screen") {
                        if let Some(running) = desktop_screen_stream.take() {
                            running.store(false, Ordering::Relaxed);
                        }
                        let fps = packet.get_i64("fps").unwrap_or(6);
                        desktop_screen_stream = Some(start_desktop_screen_stream(
                            device_id.clone(),
                            Arc::clone(&stream),
                            Arc::clone(&config),
                            fps,
                        ));
                    }
                } else if packet.packet_type == PACKET_TYPE_SCREEN_STOP {
                    if let Some(running) = desktop_screen_stream.take() {
                        running.store(false, Ordering::Relaxed);
                    }
                } else if packet.packet_type == PACKET_TYPE_BATTERY {
                    update_battery_status(&devices, &device_id, &packet);
                } else if packet.packet_type == PACKET_TYPE_CONTACTS_RESPONSE_UIDS_TIMESTAMPS {
                    // The UID/timestamp response is a synchronization index.
                    // Keep the packet validated and visible without exposing
                    // raw untrusted JSON to the UI.
                    if packet
                        .body
                        .get("uids")
                        .and_then(|value| value.as_array())
                        .is_none()
                    {
                        events.publish(CoreEvent::Error {
                            scope: "contacts".to_string(),
                            device_id: Some(device_id.clone()),
                            message: "Contacts response has no UID list".to_string(),
                            retryable: false,
                        });
                    }
                } else if packet.packet_type == PACKET_TYPE_CONTACTS_RESPONSE_VCARDS {
                    match contacts::parse_vcards(&packet) {
                        Ok(values) => update_contacts(&devices, &device_id, values),
                        Err(error) => events.publish(CoreEvent::Error {
                            scope: "contacts".to_string(),
                            device_id: Some(device_id.clone()),
                            message: error,
                            retryable: false,
                        }),
                    }
                } else if packet.packet_type == PACKET_TYPE_SMS_MESSAGES {
                    match sms::parse_messages(&packet) {
                        Ok(messages) => update_sms_messages(&devices, &device_id, messages),
                        Err(error) => events.publish(CoreEvent::Error {
                            scope: "sms".to_string(),
                            device_id: Some(device_id.clone()),
                            message: error,
                            retryable: false,
                        }),
                    }
                } else if packet.packet_type == PACKET_TYPE_TELEPHONY {
                    match telephony::parse_status(&packet) {
                        Ok(status) => update_telephony_status(&devices, &device_id, status),
                        Err(error) => events.publish(CoreEvent::Error {
                            scope: "telephony".to_string(),
                            device_id: Some(device_id.clone()),
                            message: error,
                            retryable: false,
                        }),
                    }
                } else if packet.packet_type == PACKET_TYPE_CONNECTIVITY_REPORT {
                    match connectivity::parse_report(&packet) {
                        Ok(report) => update_connectivity_status(&devices, &device_id, report),
                        Err(error) => events.publish(CoreEvent::Error {
                            scope: "connectivity".to_string(),
                            device_id: Some(device_id.clone()),
                            message: error,
                            retryable: false,
                        }),
                    }
                } else if packet.packet_type == PACKET_TYPE_FINDMYPHONE_REQUEST {
                    eprintln!(
                        "[Daemon] Received find-this-device request from {}",
                        device_id
                    );
                    let _ = std::process::Command::new("canberra-gtk-play")
                        .args(["-i", "bell"])
                        .spawn();
                } else if packet.packet_type == PACKET_TYPE_PRESENTER {
                    match presenter::pointer_delta(&packet) {
                        Ok(Some((dx, dy))) => {
                            if input_backend.is_none() && !input_backend_failed {
                                match crate::platform::wayland_remote_desktop::RemoteInputBackend::new() {
                                    Ok(input) => input_backend = Some(input),
                                    Err(error) => {
                                        input_backend_failed = true;
                                        events.publish(CoreEvent::Error {
                                            scope: "presenter".to_string(),
                                            device_id: Some(device_id.clone()),
                                            message: format!(
                                                "Presenter input permission/backend unavailable: {error}"
                                            ),
                                            retryable: false,
                                        });
                                        continue;
                                    }
                                }
                            }
                            if let Some(ref mut input) = input_backend {
                                log_input_result(
                                    "presenter pointer",
                                    input.move_mouse(dx, dy, Coordinate::Rel),
                                );
                            }
                        }
                        Ok(None) => {}
                        Err(error) => events.publish(CoreEvent::Error {
                            scope: "presenter".to_string(),
                            device_id: Some(device_id.clone()),
                            message: error,
                            retryable: false,
                        }),
                    }
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
                    let pointer_motion = mousepad_pointer_motion(&packet);

                    eprintln!(
                        "[Daemon] Received remote input request: dx={:?}, dy={:?}, x={:?}, y={:?}, pointer={:?}, scroll={:?}, key={:?}, specialKey={:?}",
                        dx, dy, x, y, pointer_motion, scroll, key, special_key
                    );

                    if input_backend.is_none() && !input_backend_failed {
                        match crate::platform::wayland_remote_desktop::RemoteInputBackend::new() {
                            Ok(input) => input_backend = Some(input),
                            Err(error) => {
                                input_backend_failed = true;
                                events.publish(CoreEvent::Error {
                                    scope: "remote-input".to_string(),
                                    device_id: Some(device_id.clone()),
                                    message: format!(
                                        "Remote input permission/backend unavailable: {error}"
                                    ),
                                    retryable: false,
                                });
                                continue;
                            }
                        }
                    }
                    if let Some(ref mut input) = input_backend {
                        let has_discrete_action = singleclick
                            || doubleclick
                            || middleclick
                            || rightclick
                            || singlehold
                            || singlerelease
                            || key.is_some()
                            || special_key > 0;
                        if !scroll && has_discrete_action {
                            apply_pointer_motion(input, pointer_motion);
                        }

                        if scroll {
                            if dy != 0.0 {
                                log_input_result(
                                    "scroll vertical",
                                    input.scroll(dy as i32, Axis::Vertical),
                                );
                            }
                            if dx != 0.0 {
                                log_input_result(
                                    "scroll horizontal",
                                    input.scroll(dx as i32, Axis::Horizontal),
                                );
                            }
                        } else if singleclick {
                            log_input_result(
                                "left click",
                                input.button(Button::Left, Direction::Click),
                            );
                        } else if doubleclick {
                            log_input_result(
                                "double click 1",
                                input.button(Button::Left, Direction::Click),
                            );
                            log_input_result(
                                "double click 2",
                                input.button(Button::Left, Direction::Click),
                            );
                        } else if middleclick {
                            log_input_result(
                                "middle click",
                                input.button(Button::Middle, Direction::Click),
                            );
                        } else if rightclick {
                            log_input_result(
                                "right click",
                                input.button(Button::Right, Direction::Click),
                            );
                        } else if singlehold {
                            log_input_result(
                                "left press",
                                input.button(Button::Left, Direction::Press),
                            );
                        } else if singlerelease {
                            log_input_result(
                                "left release",
                                input.button(Button::Left, Direction::Release),
                            );
                        } else if key.is_some() || special_key > 0 {
                            let ctrl = packet.get_bool("ctrl").unwrap_or(false);
                            let alt = packet.get_bool("alt").unwrap_or(false);
                            let shift = packet.get_bool("shift").unwrap_or(false);
                            let super_key = packet.get_bool("super").unwrap_or(false);

                            if ctrl {
                                log_input_result(
                                    "control press",
                                    input.key(Key::Control, Direction::Press),
                                );
                            }
                            if alt {
                                log_input_result(
                                    "alt press",
                                    input.key(Key::Alt, Direction::Press),
                                );
                            }
                            if shift {
                                log_input_result(
                                    "shift press",
                                    input.key(Key::Shift, Direction::Press),
                                );
                            }
                            if super_key {
                                log_input_result(
                                    "meta press",
                                    input.key(Key::Meta, Direction::Press),
                                );
                            }

                            if special_key > 0 {
                                let enigo_key = match special_key {
                                    1 => Some(Key::Backspace),
                                    2 => Some(Key::Tab),
                                    3 => Some(Key::Return),
                                    4 => Some(Key::LeftArrow),
                                    5 => Some(Key::UpArrow),
                                    6 => Some(Key::RightArrow),
                                    7 => Some(Key::DownArrow),
                                    8 => Some(Key::PageUp),
                                    9 => Some(Key::PageDown),
                                    10 => Some(Key::Home),
                                    11 => Some(Key::End),
                                    12 => Some(Key::Return),
                                    13 => Some(Key::Delete),
                                    14 => Some(Key::Escape),
                                    21 => Some(Key::F1),
                                    22 => Some(Key::F2),
                                    23 => Some(Key::F3),
                                    24 => Some(Key::F4),
                                    25 => Some(Key::F5),
                                    26 => Some(Key::F6),
                                    27 => Some(Key::F7),
                                    28 => Some(Key::F8),
                                    29 => Some(Key::F9),
                                    30 => Some(Key::F10),
                                    31 => Some(Key::F11),
                                    32 => Some(Key::F12),
                                    _ => None,
                                };
                                if let Some(ek) = enigo_key {
                                    log_input_result(
                                        "special key",
                                        input.key(ek, Direction::Click),
                                    );
                                }
                            } else if let Some(k) = key {
                                log_input_result("text input", input.text(k));
                            }

                            if ctrl {
                                log_input_result(
                                    "control release",
                                    input.key(Key::Control, Direction::Release),
                                );
                            }
                            if alt {
                                log_input_result(
                                    "alt release",
                                    input.key(Key::Alt, Direction::Release),
                                );
                            }
                            if shift {
                                log_input_result(
                                    "shift release",
                                    input.key(Key::Shift, Direction::Release),
                                );
                            }
                            if super_key {
                                log_input_result(
                                    "meta release",
                                    input.key(Key::Meta, Direction::Release),
                                );
                            }
                        } else {
                            apply_pointer_motion(input, pointer_motion);
                        }
                    }
                } else if packet.packet_type == PACKET_TYPE_CLIPBOARD
                    || packet.packet_type == PACKET_TYPE_CLIPBOARD_CONNECT
                {
                    if let Some(content) = packet.get_str("content") {
                        if content.len() > super::clipboard::MAX_CLIPBOARD_BYTES {
                            eprintln!("[Daemon] Rejecting oversized clipboard packet");
                            continue;
                        }
                        eprintln!("[Daemon] Received clipboard content: {:?}", content);
                        set_clipboard_text_from_remote(content);
                    }
                } else if packet.packet_type == PACKET_TYPE_SHARE_REQUEST {
                    if let (Some(size), Some(info)) =
                        (packet.payload_size, &packet.payload_transfer_info)
                    {
                        let filename = packet
                            .get_str("filename")
                            .unwrap_or("received_file")
                            .to_string();
                        let Some(transfer_token) =
                            packet.get_str("transferToken").map(str::to_string)
                        else {
                            eprintln!("[Daemon] Rejecting payload without a transfer token");
                            continue;
                        };
                        let transfer_id = packet
                            .get_str("transferId")
                            .unwrap_or(&transfer_token)
                            .to_string();
                        let expected_sha256 = packet.get_str("sha256").map(str::to_string);
                        let port = info.get("port").and_then(|v| v.as_i64()).unwrap_or(0) as u16;
                        eprintln!("[Daemon] Incoming file transfer request: filename={}, size={} bytes, port={}", filename, size, port);

                        if port > 0 {
                            if let Ok(Ok(peer_ip)) = stream.lock().map(|s| s.get_ref().peer_addr())
                            {
                                let ip = peer_ip.ip().to_string();
                                let device_id_clone = device_id.clone();
                                let config_clone = Arc::clone(&config);
                                let transfer_store = config.lock().ok().and_then(|config| {
                                    TransferCheckpointStore::new(config.transfer_state_dir()).ok()
                                });
                                let transfer_manager = TransferManager::default();
                                let events_clone = events.clone();
                                let cancellation_clone = Arc::clone(&transfer_cancellations);

                                thread::spawn(move || {
                                    let Some(transfer_store) = transfer_store else {
                                        events_clone.publish(CoreEvent::Error {
                                            scope: "transfer".to_string(),
                                            device_id: Some(device_id_clone.clone()),
                                            message:
                                                "Could not initialize transfer checkpoint storage"
                                                    .to_string(),
                                            retryable: true,
                                        });
                                        return;
                                    };
                                    let failure_manager = transfer_manager.clone();
                                    let failure_store = transfer_store.clone();
                                    let failure_events = events_clone.clone();
                                    if let Err(e) = receive_file_payload(
                                        ReceiveFileRequest {
                                            device_id: device_id_clone.clone(),
                                            peer_ip: ip,
                                            port,
                                            size,
                                            filename,
                                            transfer_token,
                                            transfer_id: transfer_id.clone(),
                                            expected_sha256,
                                        },
                                        ReceiveFilePersistence {
                                            config: config_clone,
                                            transfer_manager,
                                            transfer_store,
                                            events: events_clone,
                                            cancellations: cancellation_clone,
                                        },
                                    ) {
                                        eprintln!("[Daemon] File download failed: {}", e);
                                        if let Ok(Some(mut checkpoint)) =
                                            failure_store.load(&transfer_id)
                                        {
                                            checkpoint.state = TransferState::Failed;
                                            let _ = failure_manager.register(checkpoint.clone());
                                            let _ = failure_store.save(&checkpoint);
                                            failure_events.publish(CoreEvent::TransferChanged {
                                                transfer_id: transfer_id.clone(),
                                                state: "failed".to_string(),
                                                bytes_done: checkpoint.offset,
                                                bytes_total: checkpoint.total_size,
                                                can_resume: checkpoint.offset > 0,
                                                error: Some(e.clone()),
                                            });
                                        }
                                        failure_events.publish(CoreEvent::Error {
                                            scope: "transfer".to_string(),
                                            device_id: Some(device_id_clone),
                                            message: e,
                                            retryable: true,
                                        });
                                    }
                                });
                            }
                        }
                    } else if let Some(text) = packet.get_str("text") {
                        eprintln!("[Daemon] Received shared text: {:?}", text);
                        set_clipboard_text_from_remote(text);
                    } else if let Some(url) = packet.get_str("url") {
                        eprintln!("[Daemon] Received shared URL: {:?}", url);
                        if let Err(error) = crate::platform::url::open_http_url(url) {
                            eprintln!("[Daemon] Could not open shared URL: {error}");
                        }
                    }
                } else if packet.packet_type == PACKET_TYPE_MPRIS {
                    update_media_from_packet(&devices, &device_id, &packet);
                } else if packet.packet_type == PACKET_TYPE_MPRIS_REQUEST {
                    let result = if packet.get_bool("requestPlayerList").unwrap_or(false) {
                        mpris::player_list_packet()
                    } else if packet.get_bool("requestNowPlaying").unwrap_or(false) {
                        packet
                            .get_str("player")
                            .ok_or_else(|| "Media request has no player".to_string())
                            .and_then(mpris::status_packet)
                    } else {
                        mpris::apply_request(&packet).map(|_| {
                            packet
                                .get_str("player")
                                .and_then(|player| mpris::status_packet(player).ok())
                                .unwrap_or_else(|| NetworkPacket::new(PACKET_TYPE_MPRIS))
                        })
                    };
                    match result {
                        Ok(reply) => {
                            if let Err(error) = send_packet_reply(&stream, &reply) {
                                eprintln!("[Daemon] Failed to send media response: {error}");
                            }
                        }
                        Err(error) => eprintln!("[Daemon] Media request failed: {error}"),
                    }
                } else if packet.packet_type == PACKET_TYPE_NOTIFICATION {
                    if packet.get_bool("isCancel").unwrap_or(false) {
                        if let Some(id) = packet.get_str("id") {
                            remove_notification(&devices, &device_id, id);
                            publish_device_changed(&devices, &events, &device_id);
                        }
                    } else if let Some(notification) =
                        upsert_notification(&devices, &device_id, &packet)
                    {
                        events.publish(CoreEvent::NotificationReceived {
                            device_id: device_id.clone(),
                            notification,
                        });
                        publish_device_changed(&devices, &events, &device_id);
                    }
                } else if packet.packet_type == PACKET_TYPE_NOTIFICATION_CANCEL {
                    if let Some(id) = packet.get_str("id") {
                        remove_notification(&devices, &device_id, id);
                        publish_device_changed(&devices, &events, &device_id);
                    }
                } else if packet.packet_type == PACKET_TYPE_NOTIFICATION_REQUEST {
                    if let Some(cancel_id) = packet.get_str("cancel") {
                        remove_notification(&devices, &device_id, cancel_id);
                        publish_device_changed(&devices, &events, &device_id);
                    }
                } else if packet.packet_type == PACKET_TYPE_SYSTEMVOLUME {
                    update_volume_status(&devices, &device_id, &packet);
                } else if packet.packet_type == PACKET_TYPE_SYSTEMVOLUME_REQUEST {
                    let result = if packet.get_bool("requestSinks").unwrap_or(false) {
                        volume::status_packet()
                    } else {
                        volume::apply_request(&packet).and_then(|_| volume::status_packet())
                    };
                    match result {
                        Ok(reply) => {
                            if let Err(error) = send_packet_reply(&stream, &reply) {
                                eprintln!("[Daemon] Failed to send volume response: {error}");
                            }
                        }
                        Err(error) => {
                            let mut reply = NetworkPacket::new(PACKET_TYPE_SYSTEMVOLUME);
                            reply.set("errorMessage", error.clone());
                            if let Err(send_error) = send_packet_reply(&stream, &reply) {
                                eprintln!("[Daemon] Failed to report volume error: {send_error}");
                            }
                        }
                    }
                } else if packet.packet_type == PACKET_TYPE_RUNCOMMAND {
                    update_remote_commands(&devices, &device_id, &packet);
                } else if packet.packet_type == PACKET_TYPE_RUNCOMMAND_REQUEST {
                    if packet.get_bool("requestCommandList").unwrap_or(false) {
                        match config.lock() {
                            Ok(config) => {
                                let reply = run_commands::command_list_packet(&config);
                                if let Err(error) = send_packet_reply(&stream, &reply) {
                                    eprintln!("[Daemon] Failed to send command response: {error}");
                                }
                            }
                            Err(_) => {
                                eprintln!("[Daemon] Config lock poisoned while listing commands")
                            }
                        }
                    } else if let Some(key) = packet.get_str("key") {
                        let result = config
                            .lock()
                            .map_err(|_| "Config lock poisoned".to_string())
                            .and_then(|config| config.execute_command(key));
                        let mut reply = NetworkPacket::new(PACKET_TYPE_RUNCOMMAND);
                        reply.set("key", key);
                        reply.set("commandResult", result.is_ok());
                        if let Err(error) = result {
                            reply.set("errorMessage", error);
                        }
                        if let Err(error) = send_packet_reply(&stream, &reply) {
                            eprintln!("[Daemon] Failed to send command result: {error}");
                        }
                    }
                } else if packet.packet_type == PACKET_TYPE_SFTP {
                    update_sftp_status(&devices, &device_id, &packet);
                } else if packet.packet_type == PACKET_TYPE_LOCK
                    || packet.packet_type == PACKET_TYPE_LOCK_REQUEST
                {
                    // Check if they want to query status: "requestLocked" key is present
                    if packet.body.contains_key("requestLocked") {
                        eprintln!("[Daemon] Received requestLocked query");
                        match crate::platform::logind::is_locked() {
                            Ok(is_locked) => {
                                let mut reply = NetworkPacket::new(PACKET_TYPE_LOCK);
                                reply.set("isLocked", is_locked);
                                if let Err(error) = send_packet_reply(&stream, &reply) {
                                    eprintln!(
                                        "[Daemon] Failed to send lock status response: {error}"
                                    );
                                }
                            }
                            Err(error) => {
                                eprintln!("[Daemon] Failed to query lock state: {error}");
                            }
                        }
                    }

                    // Check if they want to set lock state
                    let lock_opt = packet
                        .get_bool("setLocked")
                        .or_else(|| packet.get_bool("isLocked"));
                    if let Some(set_locked) = lock_opt {
                        eprintln!(
                            "[Daemon] Received setLocked/isLocked command: {}",
                            set_locked
                        );
                        match crate::platform::logind::set_locked(set_locked) {
                            Ok(()) => match crate::platform::logind::is_locked() {
                                Ok(success) => {
                                    let mut result = NetworkPacket::new(PACKET_TYPE_LOCK);
                                    result.set("lockResult", success);
                                    result.set("isLocked", success);
                                    if let Err(error) = send_packet_reply(&stream, &result) {
                                        eprintln!("[Daemon] Failed to send lock result: {error}");
                                    }
                                }
                                Err(error) => {
                                    eprintln!("[Daemon] Failed to verify lock state: {error}")
                                }
                            },
                            Err(error) => {
                                let mut result = NetworkPacket::new(PACKET_TYPE_LOCK);
                                result.set("lockResult", false);
                                result.set("errorMessage", error.clone());
                                if let Err(send_error) = send_packet_reply(&stream, &result) {
                                    eprintln!("[Daemon] Failed to report lock error: {send_error}");
                                }
                            }
                        }
                    }
                } else {
                    eprintln!("[Daemon] Unhandled packet type: {}", packet.packet_type);
                }
                if !sessions.is_current(&binding) {
                    break;
                }
                publish_device_changed(&devices, &events, &device_id);
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                stop_desktop_screen_stream(&mut desktop_screen_stream);
                if handle_disconnect(&binding, &sessions, &devices, &events) {
                    eprintln!(
                        "[Daemon] Read error for {}: {:?}. Marked unreachable.",
                        device_id, error
                    );
                }
                break;
            }
        }
    }
}

fn send_packet_reply(
    stream: &Arc<Mutex<SslStream<TcpStream>>>,
    packet: &NetworkPacket,
) -> Result<(), String> {
    let mut locked_stream = stream
        .lock()
        .map_err(|_| "Stream lock poisoned".to_string())?;
    let line = packet.serialize_line().map_err(|error| error.to_string())?;
    locked_stream
        .write_all(&line)
        .map_err(|error| error.to_string())?;
    locked_stream.flush().map_err(|error| error.to_string())
}

fn stop_desktop_screen_stream(stream: &mut Option<Arc<AtomicBool>>) {
    if let Some(running) = stream.take() {
        running.store(false, Ordering::Relaxed);
    }
}

fn log_input_result(action: &str, result: Result<(), String>) {
    if let Err(error) = result {
        eprintln!("[Daemon] Remote input failed during {action}: {error}");
    }
}

fn mousepad_pointer_motion(packet: &NetworkPacket) -> PointerMotion {
    if packet.body.contains_key("x") && packet.body.contains_key("y") {
        let x = packet.body.get("x").and_then(value_as_i32).unwrap_or(0);
        let y = packet.body.get("y").and_then(value_as_i32).unwrap_or(0);
        return PointerMotion::Absolute { x, y };
    }

    let dx = packet.body.get("dx").and_then(value_as_i32).unwrap_or(0);
    let dy = packet.body.get("dy").and_then(value_as_i32).unwrap_or(0);
    if dx != 0 || dy != 0 {
        PointerMotion::Relative { dx, dy }
    } else {
        PointerMotion::None
    }
}

fn value_as_i32(value: &serde_json::Value) -> Option<i32> {
    let number = value
        .as_i64()
        .map(|value| value as f64)
        .or_else(|| value.as_f64())?;
    if number.is_finite() && number >= i32::MIN as f64 && number <= i32::MAX as f64 {
        Some(number.round() as i32)
    } else {
        None
    }
}

fn apply_pointer_motion(
    input: &mut crate::platform::wayland_remote_desktop::RemoteInputBackend,
    motion: PointerMotion,
) {
    match motion {
        PointerMotion::None => {}
        PointerMotion::Relative { dx, dy } => {
            log_input_result(
                "relative pointer move",
                input.move_mouse(dx, dy, Coordinate::Rel),
            );
        }
        PointerMotion::Absolute { x, y } => {
            log_input_result(
                "absolute pointer move",
                input.move_mouse(x, y, Coordinate::Abs),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::core::capability_registry::desktop_capabilities;
    use crate::device_links::packet::PACKET_TYPE_NOTIFICATION_ACTION;
    use serde_json::Value;

    fn authorization(
        packet_type: &str,
        state: Option<PairState>,
    ) -> Result<PacketAuthorization, PacketAuthorizationError> {
        let (incoming, _) = desktop_capabilities();
        authorize_incoming_packet(state, &NetworkPacket::new(packet_type), &incoming)
    }

    #[test]
    fn paired_devices_may_use_feature_packets_and_pairing_packets() {
        for packet_type in [
            PACKET_TYPE_CLIPBOARD,
            PACKET_TYPE_MOUSEPAD_REQUEST,
            PACKET_TYPE_SHARE_REQUEST,
            PACKET_TYPE_LOCK_REQUEST,
            PACKET_TYPE_SCREEN_REQUEST,
            PACKET_TYPE_NOTIFICATION_REQUEST,
        ] {
            assert_eq!(
                authorization(packet_type, Some(PairState::Paired)),
                Ok(PacketAuthorization::PairedFeatureAllowed),
                "paired packet {packet_type} should be allowed"
            );
        }
        assert_eq!(
            authorization(PACKET_TYPE_PAIR, Some(PairState::Paired)),
            Ok(PacketAuthorization::PairingAllowed)
        );
    }

    #[test]
    fn only_pairing_packets_are_allowed_before_pairing() {
        for (state, expected_error) in [
            (
                PairState::NotPaired,
                PacketAuthorizationError::DeviceNotPaired,
            ),
            (
                PairState::Requested,
                PacketAuthorizationError::PacketNotAllowedBeforePairing,
            ),
            (
                PairState::RequestedByPeer,
                PacketAuthorizationError::PacketNotAllowedBeforePairing,
            ),
        ] {
            assert_eq!(
                authorization(PACKET_TYPE_PAIR, Some(state)),
                Ok(PacketAuthorization::PairingAllowed),
                "pairing packet should be allowed in {state:?}"
            );

            for packet_type in [
                PACKET_TYPE_CLIPBOARD,
                PACKET_TYPE_MOUSEPAD_REQUEST,
                PACKET_TYPE_SHARE_REQUEST,
                PACKET_TYPE_LOCK_REQUEST,
                PACKET_TYPE_SCREEN_REQUEST,
                PACKET_TYPE_NOTIFICATION_REQUEST,
                PACKET_TYPE_NOTIFICATION_ACTION,
            ] {
                assert_eq!(
                    authorization(packet_type, Some(state)),
                    Err(expected_error),
                    "unpaired packet {packet_type} should be denied in {state:?}"
                );
            }
        }
    }

    #[test]
    fn packets_without_an_active_device_are_denied_as_unknown() {
        assert_eq!(
            authorization(PACKET_TYPE_PAIR, None),
            Err(PacketAuthorizationError::UnknownDevice)
        );
        assert_eq!(
            authorization(PACKET_TYPE_CLIPBOARD, None),
            Err(PacketAuthorizationError::UnknownDevice)
        );
        assert_eq!(
            authorization("unknown.future.packet", Some(PairState::Paired)),
            Err(PacketAuthorizationError::UnsupportedPacket)
        );
    }

    #[test]
    fn mousepad_absolute_coordinates_are_detected_even_at_zero() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_MOUSEPAD_REQUEST);
        packet.body.insert("x".to_string(), Value::from(0));
        packet.body.insert("y".to_string(), Value::from(0));

        assert_eq!(
            mousepad_pointer_motion(&packet),
            PointerMotion::Absolute { x: 0, y: 0 }
        );
    }

    #[test]
    fn mousepad_absolute_coordinates_take_precedence_over_relative_delta() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_MOUSEPAD_REQUEST);
        packet.body.insert("x".to_string(), Value::from(100));
        packet.body.insert("y".to_string(), Value::from(200));
        packet.body.insert("dx".to_string(), Value::from(5.0));
        packet.body.insert("dy".to_string(), Value::from(6.0));

        assert_eq!(
            mousepad_pointer_motion(&packet),
            PointerMotion::Absolute { x: 100, y: 200 }
        );
    }

    #[test]
    fn mousepad_relative_motion_rounds_delta_for_enigo() {
        let mut packet = NetworkPacket::new(PACKET_TYPE_MOUSEPAD_REQUEST);
        packet.body.insert("dx".to_string(), Value::from(4.8));
        packet.body.insert("dy".to_string(), Value::from(-2.2));

        assert_eq!(
            mousepad_pointer_motion(&packet),
            PointerMotion::Relative { dx: 5, dy: -2 }
        );
    }
}
