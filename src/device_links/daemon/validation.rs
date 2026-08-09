use openssl::nid::Nid;
use openssl::x509::X509;
use std::collections::HashMap;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::handle::DaemonEvent;
use super::handshake::finish_secure_link;
use super::network::ssl_acceptor;
use crate::device_links::config::Config;
use crate::device_links::core::DeviceManager;
use crate::device_links::device::DeviceView;
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::PROTOCOL_VERSION;

const MAX_UNPAIRED_CONNECTIONS: usize = 42;
const MIN_CONNECTION_INTERVAL: Duration = Duration::from_millis(500);

pub(super) fn should_throttle_connection(
    last_connections: &Arc<Mutex<HashMap<String, Instant>>>,
    device_id: &str,
) -> bool {
    let Ok(mut last_connections) = last_connections.lock() else {
        return false;
    };
    let now = Instant::now();
    if last_connections
        .get(device_id)
        .is_some_and(|last| now.duration_since(*last) < MIN_CONNECTION_INTERVAL)
    {
        return true;
    }
    last_connections.insert(device_id.to_string(), now);
    false
}

pub(super) fn enforce_unpaired_link_limit(
    config: &Arc<Mutex<Config>>,
    sessions: &Arc<DeviceManager>,
    device_id: &str,
) -> Result<(), String> {
    if config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .is_trusted(device_id)
    {
        return Ok(());
    }
    let unpaired_count = sessions.unpaired_session_count();
    if unpaired_count >= MAX_UNPAIRED_CONNECTIONS {
        Err("Too many unpaired devices are connected".to_string())
    } else {
        Ok(())
    }
}

pub(super) fn validate_pinned_certificate(
    config: &Arc<Mutex<Config>>,
    device_id: &str,
    certificate: &X509,
) -> Result<(), String> {
    let Some(expected) = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .trusted_certificate_pem(device_id)
    else {
        return Ok(());
    };
    let actual = String::from_utf8(certificate.to_pem().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;
    if expected == actual {
        Ok(())
    } else {
        Err("Trusted device certificate changed".to_string())
    }
}

pub(super) fn validate_certificate_device_id(
    certificate: &X509,
    device_id: &str,
) -> Result<(), String> {
    let subject = certificate.subject_name();
    let Some(common_name) = subject.entries_by_nid(Nid::COMMONNAME).next() else {
        return Err("Remote certificate has no device id".to_string());
    };
    let common_name = common_name
        .data()
        .to_string()
        .map_err(|err| err.to_string())?;
    if common_name == device_id {
        Ok(())
    } else {
        Err("Remote certificate device id does not match identity".to_string())
    }
}

pub(super) fn connect_to_device(
    initial_info: DeviceInfo,
    address: SocketAddr,
    remote_port: u16,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: Arc<DeviceManager>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<(), String> {
    // Discovery and an inbound connection can race. If the peer became
    // authoritative after discovery scheduled this worker, do not create a
    // duplicate TLS handshake that will be discarded by Android.
    if sessions.current_binding(&initial_info.id).is_some() {
        return Ok(());
    }
    let mut stream =
        TcpStream::connect((address.ip(), remote_port)).map_err(|err| err.to_string())?;
    // Keep the transport blocking for the complete TLS handshake. The packet
    // reader switches it to non-blocking only after the secure identity
    // exchange has completed.
    stream
        .set_nonblocking(false)
        .map_err(|err| format!("Could not configure outgoing socket: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    let mut identity = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .to_identity_packet(0);
    identity.set("targetDeviceId", initial_info.id.clone());
    identity.set("targetProtocolVersion", PROTOCOL_VERSION);
    if sessions.current_binding(&initial_info.id).is_some() {
        return Ok(());
    }
    stream
        .write_all(&identity.serialize_line().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;

    let acceptor = ssl_acceptor(&config)?;
    let ssl_stream = acceptor.accept(stream).map_err(|err| err.to_string())?;
    finish_secure_link(initial_info, ssl_stream, config, devices, sessions, events)
}
