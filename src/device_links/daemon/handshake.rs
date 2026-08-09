use openssl::ssl::SslStream;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::network::read_ssl_packet;
use super::packet_handler::packet_read_loop;
use super::state::mark_status;
use super::validation::{
    enforce_unpaired_link_limit, validate_certificate_device_id, validate_pinned_certificate,
};
use crate::device_links::config::Config;
use crate::device_links::core::device_manager::DeviceManager;
use crate::device_links::core::device_manager::SessionBinding;
use crate::device_links::core::device_session::DeviceConnectionState;
use crate::device_links::core::events::{CoreEvent, EventBus};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::webrtc::negotiation::WebRtcCoordinator;

#[allow(clippy::too_many_arguments)]
pub(super) fn finish_secure_link(
    initial_info: DeviceInfo,
    mut stream: SslStream<TcpStream>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: DeviceManager,
    events: EventBus,
    transfer_cancellations: Arc<Mutex<HashSet<String>>>,
    webrtc: WebRtcCoordinator,
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

    if let Ok(devices) = devices.lock() {
        if let Some(device) = devices.get(&secure_info.id) {
            events.publish(CoreEvent::DeviceChanged {
                device: Box::new(device.clone()),
            });
        }
    }
    events.publish(CoreEvent::ConnectionChanged {
        device_id: secure_info.id.clone(),
        state: if paired {
            DeviceConnectionState::Paired
        } else {
            DeviceConnectionState::Unpaired
        },
        message: None,
    });

    let stream = Arc::new(Mutex::new(stream));
    stream
        .lock()
        .map_err(|_| "Stream lock poisoned".to_string())?
        .get_ref()
        .set_nonblocking(true)
        .map_err(|err| err.to_string())?;
    let link = super::Link {
        stream: Arc::clone(&stream),
        certificate_pem: cert_pem,
        local_public_der,
        remote_public_der,
        info: secure_info.clone(),
    };
    let registration = sessions
        .register_link(secure_info.id.clone(), link, paired)
        .map_err(|error| format!("Could not register device session: {error:?}"))?;
    webrtc.close_for_device(&secure_info.id);
    if let Some(replaced) = &registration.replaced_link {
        replaced.close();
    }
    if let Some(replaced) = &registration.replaced_webrtc_transport {
        replaced.close();
    }
    let read_binding: SessionBinding = registration.binding.clone();
    let read_devices = Arc::clone(&devices);
    let read_config = Arc::clone(&config);
    let read_events = events.clone();
    let read_transfer_cancellations = Arc::clone(&transfer_cancellations);
    let reader_sessions = sessions.clone();

    let reader_webrtc = webrtc.clone();
    thread::spawn(move || {
        packet_read_loop(
            read_binding,
            read_devices,
            read_config,
            read_events,
            read_transfer_cancellations,
            reader_sessions,
            reader_webrtc,
        )
    });

    // Android starts its packet receiver as the control link is installed.
    // Give that receiver a short, bounded chance to install before producing
    // the first SDP offer; the protocol remains deterministic and does not
    // depend on this delay for authorization or identity verification.
    if paired {
        let begin_binding = registration.binding;
        let begin_config = Arc::clone(&config);
        let begin_sessions = sessions;
        let begin_events = events;
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(250));
            webrtc.begin_if_supported(&begin_binding, begin_config, begin_sessions, begin_events);
        });
    }
    Ok(())
}

pub(super) fn handle_disconnect(
    binding: &crate::device_links::core::device_manager::SessionBinding,
    sessions: &DeviceManager,
    webrtc: &WebRtcCoordinator,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &EventBus,
) -> bool {
    webrtc.close_for_binding(binding, sessions);
    let result =
        sessions.disconnect_if_current(binding, "The device connection was closed".to_string());
    if result.was_current {
        if let Some(webrtc_binding) = sessions.current_webrtc_binding(&binding.device_id) {
            if webrtc_binding.session_id == binding.session_id
                && webrtc_binding.generation == binding.generation
            {
                sessions.clear_webrtc_if_current(&webrtc_binding);
            }
        }
        binding.link.close();
        mark_status(devices, &binding.device_id, DeviceStatus::Unreachable);
        if let Ok(devices) = devices.lock() {
            if let Some(device) = devices.get(&binding.device_id) {
                events.publish(CoreEvent::DeviceChanged {
                    device: Box::new(device.clone()),
                });
            }
        }
        events.publish(CoreEvent::ConnectionChanged {
            device_id: binding.device_id.clone(),
            state: DeviceConnectionState::Unreachable,
            message: Some("The device connection was closed".to_string()),
        });
    }
    result.was_current
}
