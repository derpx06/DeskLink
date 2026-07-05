use openssl::ssl::SslStream;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::network::ssl_acceptor;
use crate::device_links::config::Config;
use crate::device_links::packet::{
    encode_screen_frame, NetworkPacket, ScreenFrameFormat, ScreenFrameHeader,
    PACKET_TYPE_SCREEN_ERROR, PACKET_TYPE_SCREEN_FRAME, PACKET_TYPE_SCREEN_READY,
};

pub(super) fn start_desktop_screen_stream(
    device_id: String,
    stream: Arc<Mutex<SslStream<TcpStream>>>,
    config: Arc<Mutex<Config>>,
    fps: i64,
) -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let running_thread = Arc::clone(&running);
    thread::spawn(move || {
        let mut ready = NetworkPacket::new(PACKET_TYPE_SCREEN_READY);
        ready.set("role", "desktop-screen");
        ready.set("format", "png");
        let _ = send_packet(&stream, &ready);

        let frame_interval = Duration::from_millis((1000 / fps.clamp(1, 12)) as u64);
        let mut sequence = 0_u64;

        while running_thread.load(Ordering::Relaxed) {
            let started = Instant::now();
            match capture_png_frame() {
                Ok((png, width, height)) => {
                    let header = ScreenFrameHeader {
                        stream_id: "desktop-screen".to_string(),
                        sequence,
                        width,
                        height,
                        format: ScreenFrameFormat::Png,
                        timestamp_millis: now_millis(),
                    };
                    sequence = sequence.saturating_add(1);

                    match encode_screen_frame(&header, &png) {
                        Ok(encoded) => {
                            if let Err(error) =
                                send_payload_packet(&device_id, &stream, &config, encoded)
                            {
                                eprintln!("[Daemon] Failed to send screen frame: {error}");
                                break;
                            }
                        }
                        Err(error) => {
                            let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_ERROR);
                            packet.set("message", error.to_string());
                            let _ = send_packet(&stream, &packet);
                            break;
                        }
                    }
                }
                Err(error) => {
                    let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_ERROR);
                    packet.set("message", error);
                    let _ = send_packet(&stream, &packet);
                    break;
                }
            }

            let elapsed = started.elapsed();
            if elapsed < frame_interval {
                thread::sleep(frame_interval - elapsed);
            }
        }
    });
    running
}

fn send_payload_packet(
    device_id: &str,
    stream: &Arc<Mutex<SslStream<TcpStream>>>,
    config: &Arc<Mutex<Config>>,
    payload: Vec<u8>,
) -> Result<(), String> {
    let listener = TcpListener::bind("0.0.0.0:0").map_err(|err| err.to_string())?;
    let port = listener.local_addr().map_err(|err| err.to_string())?.port();

    let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_FRAME);
    packet.payload_size = Some(payload.len() as i64);
    let mut info = serde_json::Map::new();
    info.insert("port".to_string(), serde_json::Value::from(port));
    packet.payload_transfer_info = Some(info);
    send_packet(stream, &packet)?;

    listener
        .set_nonblocking(false)
        .map_err(|err| err.to_string())?;
    let (socket, _) = listener.accept().map_err(|err| err.to_string())?;
    let acceptor = ssl_acceptor(config)?;
    let mut payload_stream = acceptor
        .accept(socket)
        .map_err(|err| format!("payload TLS handshake failed for {device_id}: {err}"))?;
    payload_stream
        .write_all(&payload)
        .map_err(|err| err.to_string())?;
    payload_stream.flush().map_err(|err| err.to_string())
}

fn send_packet(
    stream: &Arc<Mutex<SslStream<TcpStream>>>,
    packet: &NetworkPacket,
) -> Result<(), String> {
    let mut stream = stream
        .lock()
        .map_err(|_| "Stream lock poisoned".to_string())?;
    let _ = stream.get_ref().set_nonblocking(false);
    let result = stream
        .write_all(&packet.serialize_line().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string());
    let _ = stream.get_ref().set_nonblocking(true);
    result
}

fn capture_png_frame() -> Result<(Vec<u8>, u32, u32), String> {
    let path = std::env::temp_dir().join(format!(
        "desklink-screen-{}-{}.png",
        std::process::id(),
        now_millis()
    ));
    let filename = path
        .to_str()
        .ok_or_else(|| "Temporary screenshot path is invalid".to_string())?;

    let gnome = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.gnome.Shell.Screenshot",
            "--object-path",
            "/org/gnome/Shell/Screenshot",
            "--method",
            "org.gnome.Shell.Screenshot.Screenshot",
            "false",
            "false",
            filename,
        ])
        .output();

    let captured = gnome
        .ok()
        .filter(|output| output.status.success() && path.exists())
        .is_some()
        || std::process::Command::new("import")
            .args(["-window", "root", filename])
            .status()
            .map(|status| status.success() && path.exists())
            .unwrap_or(false);

    if !captured {
        return Err("Could not capture the desktop screen".to_string());
    }

    let png = std::fs::read(&path).map_err(|err| err.to_string())?;
    let _ = std::fs::remove_file(&path);
    let (width, height) = png_dimensions(&png)?;
    Ok((png, width, height))
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), String> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" || &bytes[12..16] != b"IHDR" {
        return Err("Captured frame is not a PNG image".to_string());
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().map_err(|_| "Invalid PNG width")?);
    let height = u32::from_be_bytes(bytes[20..24].try_into().map_err(|_| "Invalid PNG height")?);
    Ok((width, height))
}

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
