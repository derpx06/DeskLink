use openssl::ssl::SslStream;
use openssl::ssl::{SslConnector, SslMethod, SslVerifyMode};
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use uuid::Uuid;

use super::network::ssl_acceptor;
use crate::device_links::config::Config;
use crate::device_links::device::ScreenFrame;
use crate::device_links::packet::{
    decode_screen_frame, encode_screen_frame, NetworkPacket, ScreenFrameFormat, ScreenFrameHeader,
    PACKET_TYPE_SCREEN_ERROR, PACKET_TYPE_SCREEN_FRAME, PACKET_TYPE_SCREEN_READY,
};

const MAX_SCREEN_FRAME_BYTES: u64 = 64 * 1024 * 1024;

pub(super) fn start_desktop_screen_stream(
    device_id: String,
    stream: Arc<Mutex<SslStream<TcpStream>>>,
    config: Arc<Mutex<Config>>,
    fps: i64,
) -> Arc<AtomicBool> {
    let running = Arc::new(AtomicBool::new(true));
    let running_thread = Arc::clone(&running);
    thread::spawn(move || {
        let capture = match crate::platform::screen_cast::ScreenCastCapture::new() {
            Ok(capture) => capture,
            Err(error) => {
                let mut packet = NetworkPacket::new(PACKET_TYPE_SCREEN_ERROR);
                packet.set("message", error);
                let _ = send_packet(&stream, &packet);
                return;
            }
        };
        let mut ready = NetworkPacket::new(PACKET_TYPE_SCREEN_READY);
        ready.set("role", "desktop-screen");
        ready.set("format", "png");
        let _ = send_packet(&stream, &ready);

        let frame_interval = Duration::from_millis((1000 / fps.clamp(1, 12)) as u64);
        let mut sequence = 0_u64;

        while running_thread.load(Ordering::Relaxed) {
            let started = Instant::now();
            match capture.next_png(Duration::from_secs(3)) {
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
    let transfer_token = Uuid::new_v4().to_string();
    packet.set("transferToken", transfer_token.clone());
    let mut info = serde_json::Map::new();
    info.insert("port".to_string(), serde_json::Value::from(port));
    packet.payload_transfer_info = Some(info);
    send_packet(stream, &packet)?;

    listener
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let accept_started = Instant::now();
    let (socket, _) = loop {
        match listener.accept() {
            Ok(connection) => break connection,
            Err(error) if error.kind() == ErrorKind::WouldBlock => {
                if accept_started.elapsed() >= Duration::from_secs(15) {
                    return Err("Timed out waiting for the screen payload connection".to_string());
                }
                thread::sleep(Duration::from_millis(25));
            }
            Err(error) => return Err(error.to_string()),
        }
    };
    socket
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|err| err.to_string())?;
    socket
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|err| err.to_string())?;
    let acceptor = ssl_acceptor(config)?;
    let mut payload_stream = acceptor
        .accept(socket)
        .map_err(|err| format!("payload TLS handshake failed for {device_id}: {err}"))?;
    payload_stream
        .write_all(transfer_token.as_bytes())
        .and_then(|_| payload_stream.write_all(b"\n"))
        .map_err(|err| err.to_string())?;
    payload_stream
        .write_all(&payload)
        .map_err(|err| err.to_string())?;
    payload_stream.flush().map_err(|err| err.to_string())
}

pub(super) fn receive_screen_frame_payload(
    device_id: &str,
    peer_ip: &str,
    port: u16,
    size: i64,
    transfer_token: &str,
    config: Arc<Mutex<Config>>,
) -> Result<ScreenFrame, String> {
    let size = u64::try_from(size).map_err(|_| "Screen frame size is invalid".to_string())?;
    if size == 0 || size > MAX_SCREEN_FRAME_BYTES {
        return Err("Screen frame exceeds the maximum supported size".to_string());
    }
    let socket = TcpStream::connect_timeout(
        &format!("{peer_ip}:{port}")
            .parse()
            .map_err(|error| format!("Invalid screen payload address: {error}"))?,
        Duration::from_secs(10),
    )
    .map_err(|error| format!("Could not connect to screen payload: {error}"))?;
    socket
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("Could not configure screen payload timeout: {error}"))?;
    socket
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|error| format!("Could not configure screen payload timeout: {error}"))?;

    let mut connector = SslConnector::builder(SslMethod::tls())
        .map_err(|error| format!("Could not create screen payload TLS connector: {error}"))?;
    connector.set_verify_callback(SslVerifyMode::PEER, |preverify_ok, context| {
        preverify_ok || (context.error_depth() == 0 && context.error().as_raw() == 18)
    });
    {
        let config = config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())?;
        connector
            .set_certificate(config.certificate())
            .map_err(|error| format!("Could not configure screen payload certificate: {error}"))?;
        connector
            .set_private_key(config.key())
            .map_err(|error| format!("Could not configure screen payload key: {error}"))?;
    }
    let ssl = connector
        .build()
        .configure()
        .map_err(|error| format!("Could not configure screen payload TLS: {error}"))?
        .into_ssl(device_id)
        .map_err(|error| format!("Could not create screen payload TLS session: {error}"))?;
    let mut stream = ssl
        .connect(socket)
        .map_err(|error| format!("Screen payload TLS handshake failed: {error}"))?;
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|error| format!("Could not configure screen payload timeout: {error}"))?;
    let certificate = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| "Screen payload peer did not provide a certificate".to_string())?;
    super::validation::validate_certificate_device_id(&certificate, device_id)?;
    super::validation::validate_pinned_certificate(&config, device_id, &certificate)?;
    super::file_transfer::consume_transfer_token(&mut stream, transfer_token)?;

    let mut payload = vec![0_u8; size as usize];
    stream
        .read_exact(&mut payload)
        .map_err(|error| format!("Could not read screen payload: {error}"))?;
    let decoded = decode_screen_frame(&payload).map_err(|error| error.to_string())?;
    if !matches!(
        decoded.header.format,
        ScreenFrameFormat::Png | ScreenFrameFormat::Jpeg
    ) || decoded.header.width == 0
        || decoded.header.height == 0
        || decoded.header.width > 8192
        || decoded.header.height > 8192
    {
        return Err("Screen payload has an unsupported frame format or size".to_string());
    }
    Ok(ScreenFrame {
        width: decoded.header.width,
        height: decoded.header.height,
        sequence: decoded.header.sequence,
        timestamp_millis: decoded.header.timestamp_millis,
        png: decoded.payload,
    })
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

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}
