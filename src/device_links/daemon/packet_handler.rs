use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
use openssl::ssl::SslStream;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::clipboard::set_clipboard_text_from_remote;
use super::dispatcher::{PacketSource, SharedPacketDispatcher};
use super::handshake::handle_disconnect;
use super::state::{
    remove_notification, update_battery_status, update_media_from_packet, update_pair_state,
    update_remote_commands, update_sftp_status, update_volume_status, upsert_notification,
};
use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, SessionBinding};
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
use crate::device_links::pairing::PairState;

fn is_desktop_locked() -> bool {
    let Some(session_id) = std::env::var("XDG_SESSION_ID")
        .ok()
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    let output = std::process::Command::new("loginctl")
        .args(["show-session", &session_id, "-p", "LockedHint", "--value"])
        .output();

    output
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map(|stdout| stdout.trim() == "yes")
        .unwrap_or(false)
}

pub(super) fn packet_read_loop(
    binding: SessionBinding,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
) {
    let device_id = binding.device_id.clone();
    let stream = match binding.link.stream() {
        Ok(stream) => Arc::clone(stream),
        Err(error) => {
            eprintln!("[Daemon] Session reader could not start for {device_id}: {error}");
            return;
        }
    };
    let mut enigo_opt = Enigo::new(&Settings::default()).ok();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        if !sessions.is_current(&binding) {
            break;
        }
        let read_result = {
            let Ok(mut locked_stream) = stream.lock() else {
                handle_disconnect(
                    &binding,
                    &devices,
                    &sessions,
                    "Session stream lock failed".to_string(),
                );
                break;
            };
            locked_stream.read(&mut byte)
        };

        match read_result {
            Ok(0) => {
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
            Ok(_) => {
                line.push(byte[0]);
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
                if byte[0] != b'\n' {
                    continue;
                }

                let packet = NetworkPacket::deserialize(&line);
                line.clear();
                let Ok(packet) = packet else {
                    continue;
                };

                if packet.packet_type == PACKET_TYPE_WEBRTC_SIGNAL_V1 {
                    eprintln!(
                        "[Daemon] Received WebRTC bootstrap signaling from {}",
                        device_id
                    );
                }

                if !sessions.is_current(&binding) {
                    break;
                }

                let local_info = match config.lock() {
                    Ok(config) => config.local_device_info(),
                    Err(_) => {
                        eprintln!("[Daemon] Rejected packet: DeskLink configuration lock failed");
                        continue;
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
                    continue;
                }

                if packet.packet_type == PACKET_TYPE_WEBRTC_SIGNAL_V1 {
                    eprintln!(
                        "[Daemon] Dispatching WebRTC bootstrap signaling from {}",
                        device_id
                    );
                    if let Err(error) =
                        crate::device_links::webrtc::coordinator::handle_signaling_packet(
                            &binding,
                            &packet,
                            Arc::clone(&sessions),
                            Arc::clone(&config),
                            Arc::clone(&devices),
                        )
                    {
                        eprintln!("[Daemon] Rejected DeskLink WebRTC signaling: {error}");
                    }
                    continue;
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
                        session.pairing.receive(&packet);
                        let verification_key =
                            if session.pairing.state == PairState::RequestedByPeer {
                                Some(binding.link.verification_key(session.pairing.timestamp()))
                            } else {
                                None
                            };
                        (previous_state, session.pairing.state, verification_key)
                    });
                    if let Ok((previous_state, state, verification_key)) = pairing {
                        eprintln!(
                            "[Daemon] Pairing state transitioned from {:?} to {:?}",
                            previous_state, state
                        );
                        if state == PairState::Paired {
                            if let Ok(mut config) = config.lock() {
                                let _ = config.trust_device(
                                    &binding.link.info,
                                    binding.link.certificate_pem.clone(),
                                );
                            }
                        } else if previous_state == PairState::Paired
                            && state == PairState::NotPaired
                        {
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
                            if let Err(error) =
                                crate::device_links::webrtc::coordinator::start_initiator(
                                    binding.clone(),
                                    Arc::clone(&sessions),
                                    Arc::clone(&config),
                                    Arc::clone(&devices),
                                )
                            {
                                eprintln!(
                                    "[Daemon] DeskLink WebRTC negotiation unavailable: {error}"
                                );
                            }
                        }
                    }
                } else if packet.packet_type == PACKET_TYPE_PING {
                    eprintln!(
                        "[Daemon] Ping received from {}: {:?}",
                        device_id,
                        packet.get_str("message")
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
                        eprintln!(
                            "[Daemon] Rejected legacy LAN file payload; WebRTC file-data is required"
                        );
                        continue;
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
                        send_packet_reply(&stream, &reply);
                    }
                } else if packet.packet_type == PACKET_TYPE_RUNCOMMAND {
                    update_remote_commands(&devices, &device_id, &packet);
                } else if packet.packet_type == PACKET_TYPE_RUNCOMMAND_REQUEST {
                    if packet.get_bool("requestCommandList").unwrap_or(false) {
                        let mut reply = NetworkPacket::new(PACKET_TYPE_RUNCOMMAND);
                        reply.set("commandList", "{}");
                        reply.set("canAddCommand", false);
                        send_packet_reply(&stream, &reply);
                    }
                } else if packet.packet_type == PACKET_TYPE_SFTP {
                    update_sftp_status(&devices, &device_id, &packet);
                } else if packet.packet_type == PACKET_TYPE_LOCK
                    || packet.packet_type == PACKET_TYPE_LOCK_REQUEST
                {
                    // Check if they want to query status: "requestLocked" key is present
                    if packet.body.contains_key("requestLocked") {
                        eprintln!("[Daemon] Received requestLocked query");
                        let is_locked = is_desktop_locked();
                        let mut reply = NetworkPacket::new(PACKET_TYPE_LOCK);
                        reply.set("isLocked", is_locked);
                        send_packet_reply(&stream, &reply);
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
                        let cmd = if set_locked {
                            "lock-session"
                        } else {
                            "unlock-session"
                        };

                        // Execute standard loginctl
                        let _ = std::process::Command::new("loginctl").arg(cmd).spawn();

                        if set_locked {
                            // Extra desktop-specific locking commands
                            let _ = std::process::Command::new("dbus-send")
                                .args([
                                    "--session",
                                    "--dest=org.freedesktop.ScreenSaver",
                                    "--type=method_call",
                                    "/ScreenSaver",
                                    "org.freedesktop.ScreenSaver.Lock",
                                ])
                                .spawn();

                            let _ = std::process::Command::new("dbus-send")
                                .args([
                                    "--session",
                                    "--dest=org.gnome.ScreenSaver",
                                    "--type=method_call",
                                    "/org/gnome/ScreenSaver",
                                    "org.gnome.ScreenSaver.Lock",
                                ])
                                .spawn();

                            let _ = std::process::Command::new("xdg-screensaver")
                                .arg("lock")
                                .spawn();
                        } else {
                            // Unlocking
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

                        // Wait a short moment for lock to take effect
                        std::thread::sleep(Duration::from_millis(300));
                        let success = is_desktop_locked();

                        if set_locked {
                            let mut result = NetworkPacket::new(PACKET_TYPE_LOCK);
                            result.set("lockResult", success);
                            send_packet_reply(&stream, &result);
                        }

                        let mut state = NetworkPacket::new(PACKET_TYPE_LOCK);
                        state.set("isLocked", success);
                        send_packet_reply(&stream, &state);
                    }
                } else {
                    eprintln!("[Daemon] Unhandled packet type: {}", packet.packet_type);
                }
            }
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::TimedOut =>
            {
                thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                if handle_disconnect(
                    &binding,
                    &devices,
                    &sessions,
                    format!("Read error: {error}"),
                ) {
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
            Ok(())
        }
        PACKET_TYPE_MOUSEPAD_REQUEST => apply_remote_input(&packet),
        PACKET_TYPE_LOCK | PACKET_TYPE_LOCK_REQUEST => {
            Err("DeskLink WebRTC lock control requires the logind backend".to_string())
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

/// Applies an already authenticated remote-input packet.  This function is
/// called only from the WebRTC feature dispatcher; the LAN reader keeps its
/// legacy branch disabled by the source-aware authorization gate.
fn apply_remote_input(packet: &NetworkPacket) -> Result<(), String> {
    let mut enigo = Enigo::new(&Settings::default())
        .map_err(|_| "DeskLink could not initialize the desktop input backend".to_string())?;

    let dx = packet
        .body
        .get("dx")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let dy = packet
        .body
        .get("dy")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    let singleclick = packet.get_bool("singleclick").unwrap_or(false);
    let doubleclick = packet.get_bool("doubleclick").unwrap_or(false);
    let middleclick = packet.get_bool("middleclick").unwrap_or(false);
    let rightclick = packet.get_bool("rightclick").unwrap_or(false);
    let singlehold = packet.get_bool("singlehold").unwrap_or(false);
    let singlerelease = packet.get_bool("singlerelease").unwrap_or(false);
    let scroll = packet.get_bool("scroll").unwrap_or(false);
    let key = packet.get_str("key");
    let special_key = packet.get_i64("specialKey").unwrap_or(0);

    if scroll {
        if dy != 0.0 && enigo.scroll(dy as i32, Axis::Vertical).is_err() {
            return Err("DeskLink could not send vertical scroll input".to_string());
        }
        if dx != 0.0 && enigo.scroll(dx as i32, Axis::Horizontal).is_err() {
            return Err("DeskLink could not send horizontal scroll input".to_string());
        }
        return Ok(());
    }

    let button = if singleclick || doubleclick || middleclick || rightclick {
        if singleclick || doubleclick {
            Some(Button::Left)
        } else if middleclick {
            Some(Button::Middle)
        } else {
            Some(Button::Right)
        }
    } else {
        None
    };
    if let Some(button) = button {
        let clicks = if doubleclick { 2 } else { 1 };
        for _ in 0..clicks {
            if enigo.button(button, Direction::Click).is_err() {
                return Err("DeskLink could not send mouse button input".to_string());
            }
        }
        return Ok(());
    }
    if singlehold && enigo.button(Button::Left, Direction::Press).is_err() {
        return Err("DeskLink could not press the mouse button".to_string());
    }
    if singlerelease && enigo.button(Button::Left, Direction::Release).is_err() {
        return Err("DeskLink could not release the mouse button".to_string());
    }
    if singlehold || singlerelease {
        return Ok(());
    }

    if key.is_some() || special_key > 0 {
        let modifiers = [
            ("ctrl", enigo::Key::Control),
            ("alt", enigo::Key::Alt),
            ("shift", enigo::Key::Shift),
            ("super", enigo::Key::Meta),
        ];
        for (name, modifier) in modifiers {
            if packet.get_bool(name).unwrap_or(false)
                && enigo.key(modifier, Direction::Press).is_err()
            {
                return Err("DeskLink could not press a keyboard modifier".to_string());
            }
        }

        if special_key > 0 {
            let special = match special_key {
                1 => Some(enigo::Key::Backspace),
                2 => Some(enigo::Key::Tab),
                3 | 12 => Some(enigo::Key::Return),
                4 => Some(enigo::Key::LeftArrow),
                5 => Some(enigo::Key::UpArrow),
                6 => Some(enigo::Key::RightArrow),
                7 => Some(enigo::Key::DownArrow),
                8 => Some(enigo::Key::PageUp),
                9 => Some(enigo::Key::PageDown),
                10 => Some(enigo::Key::Home),
                11 => Some(enigo::Key::End),
                13 => Some(enigo::Key::Delete),
                14 => Some(enigo::Key::Escape),
                21..=32 => Some(match special_key {
                    21 => enigo::Key::F1,
                    22 => enigo::Key::F2,
                    23 => enigo::Key::F3,
                    24 => enigo::Key::F4,
                    25 => enigo::Key::F5,
                    26 => enigo::Key::F6,
                    27 => enigo::Key::F7,
                    28 => enigo::Key::F8,
                    29 => enigo::Key::F9,
                    30 => enigo::Key::F10,
                    31 => enigo::Key::F11,
                    _ => enigo::Key::F12,
                }),
                _ => None,
            };
            if let Some(key) = special {
                if enigo.key(key, Direction::Click).is_err() {
                    return Err("DeskLink could not send the special key".to_string());
                }
            }
        } else if let Some(key) = key {
            if enigo.text(key).is_err() {
                return Err("DeskLink could not send keyboard text".to_string());
            }
        }

        for (name, modifier) in modifiers {
            if packet.get_bool(name).unwrap_or(false)
                && enigo.key(modifier, Direction::Release).is_err()
            {
                return Err("DeskLink could not release a keyboard modifier".to_string());
            }
        }
        return Ok(());
    }

    if dx != 0.0 || dy != 0.0 {
        if enigo.move_mouse(dx as i32, dy as i32, Coordinate::Rel).is_err() {
            return Err("DeskLink could not move the pointer".to_string());
        }
    }
    Ok(())
}

fn send_packet_reply(stream: &Arc<Mutex<SslStream<TcpStream>>>, packet: &NetworkPacket) {
    if let Ok(mut locked_stream) = stream.lock() {
        if let Ok(line) = packet.serialize_line() {
            let _ = locked_stream.write_all(&line);
            let _ = locked_stream.flush();
        }
    }
}
