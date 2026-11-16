//! Which ICE servers and local addresses a peer gathers on, the addresses each
//! peer used, and forgetting them.
//!
//! Sibling edges: none. Outside: `ice`, `config`, `relay_link`,
//! `core::SessionBinding`.

use super::*;

/// ICE configuration for one attempt against `binding`'s peer.
///
/// The local address is whatever the link reports about itself. For the LAN
/// bootstrap that is the TLS socket's address, which by construction has a
/// working route to this peer. A transport that is *not* the L3 path the media
/// will take — Bluetooth, or a relayed link — reports `None`, and must: a host
/// candidate with no route to the peer costs ICE its full timeout before it is
/// abandoned. See [`IceServerConfig::local_ip`] for why this matters far more
/// than STUN here.
pub(crate) fn ice_config_for(
    binding: &SessionBinding,
    config: &Arc<Mutex<Config>>,
) -> IceServerConfig {
    let mut ice = config
        .lock()
        .ok()
        .map(|config| config.ice_servers())
        .unwrap_or_default();
    match binding.link.local_ip() {
        Some(ip) => ice.local_ips = vec![ip],
        // No local address and no relay in this build: a closed LAN link.
        // The default route's addresses, and never a server (LAN operation
        // needs none); `start_initiator` builds nothing over a closed link.
        None if !crate::device_links::relay_link::RELAY_ENABLED => {
            ice.local_ips = crate::device_links::relay_link::route::default_route_addresses();
        }
        // Bootstrapped through the relay: the internet path, with STUN for
        // the public address and the relay's TURN for when nothing direct
        // works.
        None => {
            use crate::device_links::relay_link::{client, route};
            ice.local_ips = route::default_route_addresses();
            let relay_servers = client::ice_servers_for(&binding.device_id);
            let (stun, turn) = client::webrtcbin_uris(&relay_servers);
            if ice.stun_servers.is_empty() {
                ice.stun_servers = stun;
            }
            if ice.stun_servers.is_empty() {
                ice.stun_servers.push(DEFAULT_STUN_SERVER.to_string());
            }
            ice.turn_servers.extend(turn);
        }
    }
    // TURN comes with the relay and only with it: the LAN never needs it.
    if !crate::device_links::relay_link::RELAY_ENABLED {
        ice.turn_servers.clear();
    }
    ice.relay_only =
        cfg!(debug_assertions) && std::env::var("DESKLINK_ICE_RELAY_ONLY").as_deref() == Ok("1");
    if let Ok(mut used) = ice_addresses().lock() {
        used.insert(binding.device_id.clone(), ice.local_ips.clone());
    }
    ice
}

/// The local addresses each device's current peer gathers on.
pub(super) fn ice_addresses() -> &'static Mutex<HashMap<String, Vec<std::net::IpAddr>>> {
    static USED: std::sync::OnceLock<Mutex<HashMap<String, Vec<std::net::IpAddr>>>> =
        std::sync::OnceLock::new();
    USED.get_or_init(Default::default)
}

/// Whether a peer gathering on `used` lost every one of them.
pub(super) fn addresses_vanished(used: &[std::net::IpAddr], present: &[std::net::IpAddr]) -> bool {
    !used.is_empty() && used.iter().all(|ip| !present.contains(ip))
}

/// STUN for relay-bootstrapped peers when neither the relay nor the user
/// supplied one: the same provider as the default TURN service. Never used on
/// the LAN, which needs no server.
const DEFAULT_STUN_SERVER: &str = "stun://stun.cloudflare.com:3478";
