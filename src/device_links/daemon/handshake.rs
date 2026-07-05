use openssl::ssl::SslStream;
use std::collections::HashMap;
use std::io::Write;
use std::net::{Shutdown, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use super::network::{read_ssl_packet, send_packet};
use super::packet_handler::packet_read_loop;
use super::state::mark_status;
use super::validation::{
    enforce_unpaired_link_limit, validate_certificate_device_id, validate_pinned_certificate,
};
use crate::device_links::config::Config;
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_NOTIFICATION_REQUEST};
use crate::device_links::pairing::PairingHandler;

pub(super) fn finish_secure_link(
    initial_info: DeviceInfo,
    mut stream: SslStream<TcpStream>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    links: Arc<Mutex<HashMap<String, super::Link>>>,
) -> Result<(), String> {
    let identity = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .to_identity_packet(0);
    stream
        .write_all(&identity.serialize_line().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;

    let secure_packet = read_ssl_packet(&mut stream)?;
    let secure_info = DeviceInfo::from_identity_packet(&secure_packet)
        .ok_or_else(|| "Invalid secure identity".to_string())?;
    if secure_info.id != initial_info.id
        || secure_info.protocol_version != initial_info.protocol_version
    {
        return Err("Secure identity changed during handshake".to_string());
    }

    let cert = stream
        .ssl()
        .peer_certificate()
        .ok_or_else(|| "Remote peer did not provide a certificate".to_string())?;
    validate_certificate_device_id(&cert, &secure_info.id)?;
    let cert_pem = String::from_utf8(cert.to_pem().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;
    let remote_public_der = cert
        .public_key()
        .and_then(|key| key.public_key_to_der())
        .map_err(|err| err.to_string())?;
    let local_public_der = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .key()
        .public_key_to_der()
        .map_err(|err| err.to_string())?;
    validate_pinned_certificate(&config, &secure_info.id, &cert)?;
    enforce_unpaired_link_limit(&config, &links, &secure_info.id)?;

    let paired = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .is_trusted(&secure_info.id);
    let mut view = DeviceView::from_info(
        &secure_info,
        stream
            .get_ref()
            .peer_addr()
            .map(|addr| addr.ip().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        paired,
    );
    view.status = if paired {
        DeviceStatus::Paired
    } else {
        DeviceStatus::Connected
    };
    devices
        .lock()
        .map_err(|_| "Device lock poisoned".to_string())?
        .insert(secure_info.id.clone(), view);

    let stream = Arc::new(Mutex::new(stream));
    stream
        .lock()
        .map_err(|_| "Stream lock poisoned".to_string())?
        .get_ref()
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let read_stream = Arc::clone(&stream);
    let read_devices = Arc::clone(&devices);
    let read_links = Arc::clone(&links);
    let read_config = Arc::clone(&config);
    let device_id = secure_info.id.clone();
    let pairing = PairingHandler::new(paired);

    if let Ok(mut links) = links.lock() {
        if let Some(existing) = links.remove(&secure_info.id) {
            if let Ok(existing_stream) = existing.stream.lock() {
                let _ = existing_stream.get_ref().shutdown(Shutdown::Both);
            }
        }

        links.insert(
            secure_info.id.clone(),
            super::Link {
                stream: Arc::clone(&stream),
                certificate_pem: cert_pem,
                local_public_der,
                remote_public_der,
                pairing,
                info: secure_info.clone(),
            },
        );
    } else {
        return Err("Link lock poisoned".to_string());
    }

    if paired {
        let mut request = NetworkPacket::new(PACKET_TYPE_NOTIFICATION_REQUEST);
        request.set("request", true);
        if let Err(error) = send_packet(&stream, &request) {
            eprintln!(
                "[Daemon] Failed to request current notifications from {}: {}",
                secure_info.id, error
            );
        }
    }

    thread::spawn(move || {
        packet_read_loop(
            device_id,
            read_stream,
            read_devices,
            read_links,
            read_config,
        )
    });
    Ok(())
}

pub(super) fn handle_disconnect(
    device_id: &str,
    stream: &Arc<Mutex<SslStream<TcpStream>>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    links: &Arc<Mutex<HashMap<String, super::Link>>>,
) -> bool {
    let mut is_active = false;
    if let Ok(l) = links.lock() {
        if let Some(link) = l.get(device_id) {
            if Arc::ptr_eq(&link.stream, stream) {
                is_active = true;
            }
        }
    }
    if is_active {
        mark_status(devices, device_id, DeviceStatus::Unreachable);
        if let Ok(mut l) = links.lock() {
            if let Some(link) = l.get(device_id) {
                if Arc::ptr_eq(&link.stream, stream) {
                    l.remove(device_id);
                }
            }
        }
    }
    is_active
}
