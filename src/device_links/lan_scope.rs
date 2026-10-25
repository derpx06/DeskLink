//! "Nearby": whether the ICE pair a phone is connected over is really the
//! local network, for Unlock-with-phone.
//!
//! The connection-path *label* ("lan" = both candidates host/prflx) is shared
//! with the phone and only describes the route type. A host/host pair can also
//! be a global IPv6 path, or a VPN that routes to this machine's subnet, so
//! unlock additionally requires both addresses to be private or link-local
//! AND the phone's address to sit on a subnet a local interface is directly
//! attached to.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// RFC 1918 and 169.254/16 (IPv4); fe80::/10 and fc00::/7 (IPv6). Not
/// loopback, not CGNAT (100.64/10: a carrier's shared space), not global.
pub(crate) fn is_private_or_link_local(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => ip.is_private() || ip.is_link_local(),
        IpAddr::V6(ip) => {
            let first = ip.segments()[0];
            first & 0xffc0 == 0xfe80
                || first & 0xfe00 == 0xfc00
                || ip
                    .to_ipv4_mapped()
                    .is_some_and(|v4| v4.is_private() || v4.is_link_local())
        }
    }
}

/// Whether `ip` is within the first `prefix` bits of `net`.
fn within(ip: IpAddr, net: IpAddr, prefix: u8) -> bool {
    match (ip, net) {
        (IpAddr::V4(ip), IpAddr::V4(net)) => {
            let prefix = u32::from(prefix.min(32));
            let mask = if prefix == 0 {
                0
            } else {
                u32::MAX << (32 - prefix)
            };
            u32::from(ip) & mask == u32::from(net) & mask
        }
        (IpAddr::V6(ip), IpAddr::V6(net)) => {
            let prefix = u32::from(prefix.min(128));
            let mask = if prefix == 0 {
                0
            } else {
                u128::MAX << (128 - prefix)
            };
            u128::from(ip) & mask == u128::from(net) & mask
        }
        _ => false,
    }
}

/// The pair is local: both ends private or link-local, this end is one of our
/// own interface addresses, and the phone is on that interface's subnet.
pub(crate) fn is_nearby(local: IpAddr, remote: IpAddr, interfaces: &[(IpAddr, u8)]) -> bool {
    is_private_or_link_local(local)
        && is_private_or_link_local(remote)
        && interfaces
            .iter()
            .any(|(own, prefix)| *own == local && within(remote, *own, *prefix))
}

/// Every address of every local interface, with its prefix length.
pub(crate) fn local_interfaces() -> Vec<(IpAddr, u8)> {
    let mut found = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a linked list that freeifaddrs releases; only
    // fields it documents are read, and every pointer is null-checked.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return found;
        }
        let mut entry = list;
        while !entry.is_null() {
            let current = &*entry;
            if let (Some(ip), Some(mask)) = (
                sockaddr_ip(current.ifa_addr),
                sockaddr_ip(current.ifa_netmask),
            ) {
                found.push((ip, prefix_of(mask)));
            }
            entry = current.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    found
}

/// # Safety
/// `address` is null or points to a valid `sockaddr` of the family it names.
unsafe fn sockaddr_ip(address: *const libc::sockaddr) -> Option<IpAddr> {
    if address.is_null() {
        return None;
    }
    match i32::from((*address).sa_family) {
        libc::AF_INET => {
            let v4 = &*(address as *const libc::sockaddr_in);
            Some(IpAddr::V4(Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr))))
        }
        libc::AF_INET6 => {
            let v6 = &*(address as *const libc::sockaddr_in6);
            Some(IpAddr::V6(Ipv6Addr::from(v6.sin6_addr.s6_addr)))
        }
        _ => None,
    }
}

fn prefix_of(mask: IpAddr) -> u8 {
    match mask {
        IpAddr::V4(mask) => u32::from(mask).count_ones() as u8,
        IpAddr::V6(mask) => u128::from(mask).count_ones() as u8,
    }
}

/// Parses a candidate's address as webrtcbin reports it (a bare IP, maybe
/// with a zone like `fe80::1%wlan0`).
pub(crate) fn parse_candidate_ip(text: &str) -> Option<IpAddr> {
    text.split('%').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    #[test]
    fn only_private_and_link_local_addresses_are_local() {
        for local in [
            "10.0.0.7",
            "172.16.4.1",
            "172.31.255.255",
            "192.168.1.3",
            "169.254.10.10",
            "fe80::1",
            "febf::1",
            "fc00::5",
            "fd12:3456::9",
        ] {
            assert!(is_private_or_link_local(ip(local)), "{local}");
        }
        for global in [
            "8.8.8.8",
            "172.32.0.1",
            "100.64.0.9",
            "127.0.0.1",
            "2001:db8::1",
            "2a00:1450:4001::200e",
            "fec0::1",
            "::1",
            "::",
        ] {
            assert!(!is_private_or_link_local(ip(global)), "{global}");
        }
    }

    #[test]
    fn nearby_needs_a_private_pair_on_a_directly_attached_subnet() {
        let interfaces = vec![
            (ip("192.168.1.20"), 24),
            (ip("fe80::aa"), 64),
            (ip("2001:db8::20"), 64),
            (ip("10.8.0.2"), 24), // a VPN
        ];
        assert!(is_nearby(
            ip("192.168.1.20"),
            ip("192.168.1.3"),
            &interfaces
        ));
        assert!(is_nearby(ip("fe80::aa"), ip("fe80::1234"), &interfaces));
        assert!(!is_nearby(
            ip("192.168.1.20"),
            ip("192.168.7.3"),
            &interfaces
        ));
        assert!(!is_nearby(ip("10.8.0.2"), ip("10.9.0.5"), &interfaces));
        // A global IPv6 host/host pair on a shared prefix: not nearby.
        assert!(!is_nearby(
            ip("2001:db8::20"),
            ip("2001:db8::99"),
            &interfaces
        ));
        let public = vec![(ip("203.0.113.5"), 24)];
        assert!(!is_nearby(ip("203.0.113.5"), ip("203.0.113.9"), &public));
        assert!(!is_nearby(
            ip("192.168.1.99"),
            ip("192.168.1.3"),
            &interfaces
        ));
        assert!(!is_nearby(ip("192.168.1.20"), ip("fe80::1"), &interfaces));
    }

    #[test]
    fn candidate_addresses_parse_with_or_without_a_zone() {
        assert_eq!(parse_candidate_ip("192.168.1.3"), Some(ip("192.168.1.3")));
        assert_eq!(parse_candidate_ip("fe80::1%wlan0"), Some(ip("fe80::1")));
        assert_eq!(parse_candidate_ip("not an address"), None);
        assert_eq!(parse_candidate_ip(""), None);
    }

    #[test]
    fn this_machines_interfaces_can_be_listed() {
        for (_, prefix) in local_interfaces() {
            assert!(prefix <= 128);
        }
    }
}
