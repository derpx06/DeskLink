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
/// Matches `MAX_RATE_LIMIT_ENTRIES` in the Android `LanLinkProvider`.
const MAX_RATE_LIMIT_ENTRIES: usize = 255;

/// Rate-limits repeated connection attempts from one discovery beacon.
///
/// The key comes from an unauthenticated UDP packet, so it is fully
/// attacker-controlled. The map is therefore capped and pruned: without that, a
/// stream of spoofed identities grew it without bound. Entries older than the
/// throttle interval are useless, so pruning them first keeps the cap from
/// evicting devices that are genuinely active.
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

    if last_connections.len() >= MAX_RATE_LIMIT_ENTRIES {
        last_connections.retain(|_, last| now.duration_since(*last) < MIN_CONNECTION_INTERVAL);
    }
    while last_connections.len() >= MAX_RATE_LIMIT_ENTRIES {
        // Every entry is still inside its throttle window, which only happens
        // under a flood of spoofed identities. Evict the oldest rather than
        // refusing the new attempt: refusing would let anyone on the network
        // lock out every real device by broadcasting enough fake beacons, which
        // is a worse failure than the unbounded growth this cap replaced.
        let Some(oldest) = last_connections
            .iter()
            .min_by_key(|(_, last)| **last)
            .map(|(id, _)| id.clone())
        else {
            break;
        };
        last_connections.remove(&oldest);
    }
    last_connections.insert(device_id.to_string(), now);
    false
}

/// Unpaired links one network address may hold at once. Without it one host
/// opening 42 sessions under made-up ids kept every real phone out of pairing.
const MAX_UNPAIRED_PER_ADDRESS: usize = 2;

/// Whether `address` already holds its share of the unpaired links, given the
/// addresses of the other unpaired links.
pub(super) fn address_is_full(other_unpaired_addresses: &[String], address: &str) -> bool {
    other_unpaired_addresses
        .iter()
        .filter(|held| held.as_str() == address)
        .count()
        >= MAX_UNPAIRED_PER_ADDRESS
}

pub(super) fn enforce_unpaired_link_limit(
    config: &Arc<Mutex<Config>>,
    sessions: &Arc<DeviceManager>,
    devices: &Arc<Mutex<HashMap<String, crate::device_links::device::DeviceView>>>,
    device_id: &str,
    address: &str,
) -> Result<(), String> {
    if config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .is_trusted(device_id)
    {
        return Ok(());
    }
    let unpaired = sessions.unpaired_session_ids();
    if unpaired.len() >= MAX_UNPAIRED_CONNECTIONS {
        return Err("Too many unpaired devices are connected".to_string());
    }
    let held: Vec<String> = devices
        .lock()
        .map(|devices| {
            unpaired
                .iter()
                .filter(|id| id.as_str() != device_id)
                .filter_map(|id| devices.get(id).map(|device| device.address.clone()))
                .collect()
        })
        .unwrap_or_default();
    if address_is_full(&held, address) {
        return Err("Too many unpaired devices are connected from this address".to_string());
    }
    Ok(())
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
    // A beacon from a restarted peer must be dialled even though this side
    // still holds a session for its dead previous process.
    let wanted = || {
        sessions.wants_bootstrap_connection(&initial_info.id)
            || sessions.peer_restarted(&initial_info.id, initial_info.instance_id.as_deref())
    };
    if !wanted() {
        return Ok(());
    }
    // Without a timeout a peer that vanished costs ~2 minutes of SYN retries.
    let mut stream = TcpStream::connect_timeout(
        &std::net::SocketAddr::new(address.ip(), remote_port),
        Duration::from_secs(3),
    )
    .map_err(|err| err.to_string())?;
    // Keep the transport blocking for the complete TLS handshake. The packet
    // reader switches it to non-blocking only after the secure identity
    // exchange has completed.
    stream
        .set_nonblocking(false)
        .map_err(|err| format!("Could not configure outgoing socket: {err}"))?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|err| err.to_string())?;
    super::network::enable_keepalive(&stream);
    let deadline = super::network::HandshakeDeadline::arm(&stream, super::network::HANDSHAKE_LIMIT);
    let mut identity = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .to_identity_packet(0);
    identity.set("targetDeviceId", initial_info.id.clone());
    identity.set("targetProtocolVersion", PROTOCOL_VERSION);
    if !wanted() {
        return Ok(());
    }
    stream
        .write_all(&identity.serialize_line().map_err(|err| err.to_string())?)
        .map_err(|err| err.to_string())?;

    let acceptor = ssl_acceptor(&config)?;
    let ssl_stream = acceptor.accept(stream).map_err(|err| err.to_string())?;
    finish_secure_link(
        initial_info,
        ssl_stream,
        deadline,
        config,
        devices,
        sessions,
        events,
        // This computer dialed the phone.
        true,
    )
}

#[cfg(test)]
mod per_address_tests {
    use super::*;

    /// One address may hold two unpaired links, no more; others are unaffected.
    #[test]
    fn one_address_cannot_fill_the_unpaired_slots() {
        let held = |addresses: &[&str]| addresses.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        assert!(!address_is_full(&held(&[]), "192.168.1.9"));
        assert!(!address_is_full(&held(&["192.168.1.9"]), "192.168.1.9"));
        assert!(address_is_full(
            &held(&["192.168.1.9", "192.168.1.9"]),
            "192.168.1.9"
        ));
        assert!(!address_is_full(
            &held(&["192.168.1.9", "192.168.1.9"]),
            "192.168.1.20"
        ));
    }
}

#[cfg(test)]
mod accept_path_tests {
    use super::*;
    use crate::device_links::device_info::DeviceInfo;
    use openssl::asn1::Asn1Time;
    use openssl::ec::{EcGroup, EcKey};
    use openssl::hash::MessageDigest;
    use openssl::pkey::PKey;
    use openssl::x509::X509NameBuilder;

    fn certificate(common_name: &str) -> X509 {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
        let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
        let mut name = X509NameBuilder::new().unwrap();
        name.append_entry_by_nid(Nid::COMMONNAME, common_name)
            .unwrap();
        let name = name.build();
        let mut builder = X509::builder().unwrap();
        builder.set_version(2).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&key).unwrap();
        builder
            .set_not_before(&Asn1Time::days_from_now(0).unwrap())
            .unwrap();
        builder
            .set_not_after(&Asn1Time::days_from_now(1).unwrap())
            .unwrap();
        builder.sign(&key, MessageDigest::sha256()).unwrap();
        builder.build()
    }

    /// The certificate presented on a link must be the one pinned at
    /// pairing; an unknown device is judged by pairing, not here.
    #[test]
    fn a_paired_device_must_present_its_pinned_certificate() {
        let dir = std::env::temp_dir().join(format!("desklink-pin-{}", uuid::Uuid::new_v4()));
        let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
        let id = "abcdefgh_1234_abcdefgh_1234_abcd";
        let pinned = certificate(id);
        let pem = String::from_utf8(pinned.to_pem().unwrap()).unwrap();
        assert!(
            validate_pinned_certificate(&config, id, &pinned).is_ok(),
            "not paired yet: nothing to compare"
        );
        config
            .lock()
            .unwrap()
            .trust_device(&DeviceInfo::local(id.into(), "Phone".into()), pem)
            .unwrap();
        assert!(validate_pinned_certificate(&config, id, &pinned).is_ok());
        let forged = certificate(id);
        assert_eq!(
            validate_pinned_certificate(&config, id, &forged).unwrap_err(),
            "Trusted device certificate changed"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_certificate_name_must_be_the_identitys_device_id() {
        let cert = certificate("device-a");
        assert!(validate_certificate_device_id(&cert, "device-a").is_ok());
        assert!(validate_certificate_device_id(&cert, "device-b").is_err());
        let nameless = {
            let mut builder = X509::builder().unwrap();
            let name = X509NameBuilder::new().unwrap().build();
            let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1).unwrap();
            let key = PKey::from_ec_key(EcKey::generate(&group).unwrap()).unwrap();
            builder.set_subject_name(&name).unwrap();
            builder.set_issuer_name(&name).unwrap();
            builder.set_pubkey(&key).unwrap();
            builder.sign(&key, MessageDigest::sha256()).unwrap();
            builder.build()
        };
        assert!(validate_certificate_device_id(&nameless, "device-a").is_err());
    }

    #[test]
    fn a_second_attempt_inside_the_interval_is_throttled_and_the_map_stays_bounded() {
        let map = Arc::new(Mutex::new(HashMap::new()));
        assert!(!should_throttle_connection(&map, "phone"));
        assert!(should_throttle_connection(&map, "phone"), "too soon");
        // A flood of spoofed identities cannot grow it, nor lock real ones out.
        for n in 0..(MAX_RATE_LIMIT_ENTRIES * 2) {
            assert!(!should_throttle_connection(&map, &format!("spoof-{n}")));
        }
        assert!(map.lock().unwrap().len() <= MAX_RATE_LIMIT_ENTRIES);
    }
}
