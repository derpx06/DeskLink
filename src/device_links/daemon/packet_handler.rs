use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse, Settings};
use openssl::ssl::SslStream;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::clipboard::set_clipboard_text_from_remote;
use super::file_transfer::receive_file_payload;
use super::handshake::handle_disconnect;
use super::state::{
    remove_notification, update_battery_status, update_media_from_packet, update_pair_state,
    update_remote_commands, update_sftp_status, update_volume_status, upsert_notification,
};
use crate::device_links::config::Config;
use crate::device_links::device::DeviceView;
use crate::device_links::packet::{
    NetworkPacket, PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS, PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PAIR, PACKET_TYPE_PING, PACKET_TYPE_RUNCOMMAND,
    PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SFTP, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_SYSTEMVOLUME, PACKET_TYPE_SYSTEMVOLUME_REQUEST,
};
use crate::device_links::pairing::PairState;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PointerMotion {
    None,
    Relative { dx: i32, dy: i32 },
    Absolute { x: i32, y: i32 },
}

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
    device_id: String,
    stream: Arc<Mutex<SslStream<TcpStream>>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    links: Arc<Mutex<HashMap<String, super::Link>>>,
    config: Arc<Mutex<Config>>,
) {
    let mut enigo_opt = Enigo::new(&Settings::default()).ok();
    let mut line = Vec::new();
    let mut byte = [0u8; 1];

    loop {
        let read_result = {
            let Ok(mut locked_stream) = stream.lock() else {
                handle_disconnect(&device_id, &stream, &devices, &links);
                break;
            };
            locked_stream.read(&mut byte)
        };

        match read_result {
            Ok(0) => {
                if handle_disconnect(&device_id, &stream, &devices, &links) {
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
                    handle_disconnect(&device_id, &stream, &devices, &links);
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

                if packet.packet_type == PACKET_TYPE_PAIR {
                    eprintln!(
                        "[Daemon] Received pair packet from device {}: pair={:?}, timestamp={:?}",
                        device_id,
                        packet.get_bool("pair"),
                        packet.get_i64("timestamp")
                    );
                    if let Ok(mut links) = links.lock() {
                        if let Some(link) = links.get_mut(&device_id) {
                            let previous_state = link.pairing.state;
                            link.pairing.receive(&packet);
                            eprintln!(
                                "[Daemon] Pairing state transitioned from {:?} to {:?}",
                                previous_state, link.pairing.state
                            );
                            if link.pairing.state == PairState::Paired {
                                if let Ok(mut config) = config.lock() {
                                    eprintln!("[Daemon] Trusting device {}", device_id);
                                    let _ = config
                                        .trust_device(&link.info, link.certificate_pem.clone());
                                }
                            } else if previous_state == PairState::Paired
                                && link.pairing.state == PairState::NotPaired
                            {
                                if let Ok(mut config) = config.lock() {
                                    eprintln!(
                                        "[Daemon] Untrusting device {} due to state transition",
                                        device_id
                                    );
                                    let _ = config.untrust_device(&device_id);
                                }
                            }
                            let key = if link.pairing.state == PairState::RequestedByPeer {
                                Some(link.verification_key())
                            } else {
                                None
                            };
                            update_pair_state(&devices, &device_id, link.pairing.state, key);
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
                    let pointer_motion = mousepad_pointer_motion(&packet);

                    eprintln!(
                        "[Daemon] Received remote input request: dx={:?}, dy={:?}, x={:?}, y={:?}, pointer={:?}, scroll={:?}, key={:?}, specialKey={:?}",
                        dx, dy, x, y, pointer_motion, scroll, key, special_key
                    );

                    if let Some(ref mut enigo) = enigo_opt {
                        let has_discrete_action = singleclick
                            || doubleclick
                            || middleclick
                            || rightclick
                            || singlehold
                            || singlerelease
                            || key.is_some()
                            || special_key > 0;
                        if !scroll && has_discrete_action {
                            apply_pointer_motion(enigo, pointer_motion);
                        }

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
                        } else {
                            apply_pointer_motion(enigo, pointer_motion);
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
                    if let (Some(size), Some(info)) =
                        (packet.payload_size, &packet.payload_transfer_info)
                    {
                        let filename = packet
                            .get_str("filename")
                            .unwrap_or("received_file")
                            .to_string();
                        let port = info.get("port").and_then(|v| v.as_i64()).unwrap_or(0) as u16;
                        eprintln!("[Daemon] Incoming file transfer request: filename={}, size={} bytes, port={}", filename, size, port);

                        if port > 0 {
                            if let Ok(Ok(peer_ip)) = stream.lock().map(|s| s.get_ref().peer_addr())
                            {
                                let ip = peer_ip.ip().to_string();
                                let device_id_clone = device_id.clone();
                                let config_clone = Arc::clone(&config);

                                thread::spawn(move || {
                                    if let Err(e) = receive_file_payload(
                                        &device_id_clone,
                                        &ip,
                                        port,
                                        size,
                                        &filename,
                                        config_clone,
                                    ) {
                                        eprintln!("[Daemon] File download failed: {}", e);
                                    }
                                });
                            }
                        }
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
                if handle_disconnect(&device_id, &stream, &devices, &links) {
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

fn send_packet_reply(stream: &Arc<Mutex<SslStream<TcpStream>>>, packet: &NetworkPacket) {
    if let Ok(mut locked_stream) = stream.lock() {
        if let Ok(line) = packet.serialize_line() {
            let _ = locked_stream.write_all(&line);
            let _ = locked_stream.flush();
        }
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

fn apply_pointer_motion(enigo: &mut Enigo, motion: PointerMotion) {
    match motion {
        PointerMotion::None => {}
        PointerMotion::Relative { dx, dy } => {
            let _ = enigo.move_mouse(dx, dy, Coordinate::Rel);
        }
        PointerMotion::Absolute { x, y } => {
            let _ = enigo.move_mouse(x, y, Coordinate::Abs);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

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
