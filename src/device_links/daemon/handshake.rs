use openssl::ssl::SslStream;
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;

use super::network::read_ssl_packet;
use super::packet_handler::packet_read_loop;
use super::state::mark_status;
use super::validation::{
    enforce_unpaired_link_limit, validate_certificate_device_id, validate_pinned_certificate,
};
use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, SessionBinding, SessionLink};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::PACKET_TYPE_WEBRTC_SIGNAL_V1;

pub(super) fn finish_secure_link(
    initial_info: DeviceInfo,
    mut stream: SslStream<TcpStream>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
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
    enforce_unpaired_link_limit(&config, &sessions, &secure_info.id)?;

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

    stream
        .get_ref()
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let link = SessionLink::new(
        Arc::new(Mutex::new(stream)),
        cert_pem,
        local_public_der,
        remote_public_der,
        secure_info,
    );
    let registration = sessions
        .register_link(initial_info.id.clone(), link, paired)
        .map_err(|error| error.to_string())?;
    if let Some(replaced) = registration.replaced_link {
        replaced.close();
    }
    if let Some(replaced) = registration.replaced_webrtc {
        replaced.close();
    }

    let binding = registration.binding;
    let remote_supports_webrtc = paired
        && binding
            .link
            .info
            .incoming_capabilities
            .iter()
            .any(|capability| capability == PACKET_TYPE_WEBRTC_SIGNAL_V1);
    if remote_supports_webrtc {
        if let Err(error) = crate::device_links::webrtc::coordinator::start_initiator(
            binding.clone(),
            Arc::clone(&sessions),
            Arc::clone(&config),
            Arc::clone(&devices),
        ) {
            // Bootstrap LAN remains available until mutual feature-ready, so
            // a failed optional negotiation must not tear down a paired link.
            eprintln!("[Daemon] DeskLink WebRTC negotiation unavailable: {error}");
        }
    }
    let read_devices = Arc::clone(&devices);
    let read_sessions = Arc::clone(&sessions);
    let read_config = Arc::clone(&config);
    thread::spawn(move || packet_read_loop(binding, read_devices, read_sessions, read_config));
    Ok(())
}

pub(super) fn handle_disconnect(
    binding: &SessionBinding,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    reason: String,
) -> bool {
    let result = sessions.disconnect_if_current(binding, reason);
    if !result.was_current {
        return false;
    }
    if let Some(link) = result.link {
        link.close();
    }
    if let Some(peer) = result.webrtc {
        peer.close();
    }
    mark_status(devices, &binding.device_id, DeviceStatus::Unreachable);
    true
}
