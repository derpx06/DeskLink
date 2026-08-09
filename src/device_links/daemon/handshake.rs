use openssl::ssl::SslStream;
use std::collections::HashMap;
use std::io::Write;
use std::net::TcpStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::handle::DaemonEvent;
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
    events: Arc<Mutex<Vec<DaemonEvent>>>,
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

    // Keep the TLS stream in one mode for its entire lifetime.  Toggling an
    // OpenSSL stream between nonblocking and blocking mode from the reader
    // and sender threads races with OpenSSL's internal WANT_READ/WANT_WRITE
    // state and can surface a false "nonblocking read call would have
    // blocked" error during WebRTC signaling.  A short read timeout keeps
    // shutdown responsive without changing the socket mode underneath the
    // active TLS stream.
    stream
        .get_ref()
        .set_nonblocking(false)
        .map_err(|err| err.to_string())?;
    stream
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(250)))
        .map_err(|err| err.to_string())?;
    stream
        .get_ref()
        .set_write_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    let link = SessionLink::new(
        Arc::new(Mutex::new(stream)),
        cert_pem,
        local_public_der,
        remote_public_der,
        secure_info,
    );
    if sessions.has_ready_webrtc(&initial_info.id) {
        // Android may refresh the short-lived LAN bootstrap socket while its
        // authenticated WebRTC peer is healthy.  This socket cannot become a
        // second session: closing it preserves the existing generation and
        // avoids tearing down all paired features for a discovery refresh.
        eprintln!(
            "[DL-WRTC-BOOT] ignoring refreshed bootstrap socket for {}; retaining feature-ready WebRTC session",
            initial_info.id
        );
        link.close();
        return Ok(());
    }
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
    eprintln!(
        "[DL-WRTC-BOOT] secure bootstrap link ready for {} (session {}, generation {})",
        binding.device_id, binding.session_id, binding.generation
    );
    let remote_supports_webrtc = paired
        && binding
            .link
            .info
            .incoming_capabilities
            .iter()
            .any(|capability| capability == PACKET_TYPE_WEBRTC_SIGNAL_V1);
    eprintln!(
        "[DL-WRTC-BOOT] peer WebRTC signaling capability: {}",
        remote_supports_webrtc
    );
    let read_devices = Arc::clone(&devices);
    let read_sessions = Arc::clone(&sessions);
    let read_config = Arc::clone(&config);
    let read_binding = binding.clone();
    let read_events = Arc::clone(&events);
    thread::spawn(move || {
        packet_read_loop(
            read_binding,
            read_devices,
            read_sessions,
            read_config,
            read_events,
        )
    });

    if remote_supports_webrtc {
        schedule_webrtc_bootstrap(
            binding,
            Arc::clone(&sessions),
            Arc::clone(&config),
            Arc::clone(&devices),
            Arc::clone(&events),
        );
    }
    Ok(())
}

/// Start SDP/ICE only after the normal packet reader owns the TLS stream.
/// Both implementations use buffered identity readers during the secure
/// handshake; signaling sent before those readers are retired can otherwise
/// be prefetched and lost. A bounded retry is safe because each request is
/// signed and replay-protected, and it stops as soon as an attempt is active.
fn schedule_webrtc_bootstrap(
    binding: SessionBinding,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) {
    thread::spawn(move || {
        for delay in [250_u64, 1_000, 2_000, 4_000] {
            thread::sleep(Duration::from_millis(delay));
            if !sessions.is_current(&binding) {
                eprintln!(
                    "[DL-WRTC-BOOT] bootstrap scheduler stopped for stale generation {}",
                    binding.generation
                );
                return;
            }
            if sessions
                .active_webrtc_attempt(&binding)
                .ok()
                .flatten()
                .is_some()
            {
                return;
            }
            if let Err(error) = crate::device_links::webrtc::coordinator::start_initiator(
                binding.clone(),
                Arc::clone(&sessions),
                Arc::clone(&config),
                Arc::clone(&devices),
                Arc::clone(&events),
            ) {
                eprintln!("[Daemon] DeskLink WebRTC bootstrap attempt failed: {error}");
            }
        }
    });
}

pub(super) fn handle_disconnect(
    binding: &SessionBinding,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    reason: String,
) -> bool {
    let result = sessions.bootstrap_disconnected_if_current(binding, reason);
    if !result.was_current {
        return false;
    }
    if let Some(link) = result.link {
        link.close();
    }
    if result.feature_transport_retained {
        eprintln!(
            "[DL-WRTC-BOOT] bootstrap link closed for {}; retained authenticated WebRTC feature transport",
            binding.device_id
        );
        return false;
    }
    if let Some(peer) = result.webrtc {
        peer.close();
    }
    mark_status(devices, &binding.device_id, DeviceStatus::Unreachable);
    true
}
