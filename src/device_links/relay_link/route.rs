//! Which local addresses the internet path will use.
//!
//! Off the LAN there is no bootstrap socket to say which interface reaches the
//! peer, but gathering on every interface brings back `docker0`/`virbr0`
//! (see `IceServerConfig::local_ips`). "Connecting" a UDP socket sends nothing
//! yet makes the kernel pick the source address of the default route, per
//! family — exactly the interfaces a phone on 5G can reach.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, UdpSocket};

/// Any routable destination works; nothing is sent to it.
const PROBE_V4: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1)), 9);
const PROBE_V6: SocketAddr = SocketAddr::new(
    IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1)),
    9,
);

/// The default-route source addresses, IPv6 first (no NAT, so the likeliest
/// direct path to a phone on mobile data).
pub fn default_route_addresses() -> Vec<IpAddr> {
    [("[::]:0", PROBE_V6), ("0.0.0.0:0", PROBE_V4)]
        .into_iter()
        .filter_map(|(bind, probe)| {
            let socket = UdpSocket::bind(bind).ok()?;
            socket.connect(probe).ok()?;
            let ip = socket.local_addr().ok()?.ip();
            (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn never_returns_unspecified_or_loopback() {
        for ip in super::default_route_addresses() {
            assert!(!ip.is_unspecified() && !ip.is_loopback());
        }
    }
}
