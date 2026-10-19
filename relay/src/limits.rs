//! Rate limits, connection caps and the daily TURN quota.
//!
//! Every function takes the current time as an argument instead of reading a
//! clock, so the tests drive time explicitly.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Write};
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::time::Instant;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use openssl::hash::{hash, MessageDigest};
use serde::{Deserialize, Serialize};

use crate::config::Limits;

pub struct TokenBucket {
    rate: f64,
    capacity: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// Starts full, so a fresh client gets its whole burst.
    pub fn new(rate_per_sec: f64, capacity: f64, now: Instant) -> Self {
        Self {
            rate: rate_per_sec,
            capacity,
            tokens: capacity,
            last: now,
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate).min(self.capacity);
        self.last = self.last.max(now);
    }

    pub fn take(&mut self, cost: f64, now: Instant) -> bool {
        self.refill(now);
        if self.tokens < cost {
            return false;
        }
        self.tokens -= cost;
        true
    }

    /// A full bucket carries no history and can be forgotten.
    pub fn is_full(&mut self, now: Instant) -> bool {
        self.refill(now);
        self.tokens >= self.capacity
    }
}

/// The unit a client is limited by: a whole IPv4 address, or an IPv6 /64
/// (one subscriber's prefix; limiting single v6 addresses would be useless,
/// a client can rotate through 2^64 of them).
///
/// An IPv6 client is also counted under its /48 ([`NetKey::V6Wide`]), with
/// larger limits: a routed /48 or /56 holds thousands of /64s, and without
/// the second tier each of them would get a fresh set of limits.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum NetKey {
    V4(Ipv4Addr),
    V6Prefix(u64),
    V6Wide(u64),
}

impl NetKey {
    pub fn of(ip: IpAddr) -> Self {
        // `::ffff:a.b.c.d` from a dual-stack socket is an IPv4 client.
        match ip.to_canonical() {
            IpAddr::V4(v4) => NetKey::V4(v4),
            IpAddr::V6(v6) => NetKey::V6Prefix((u128::from(v6) >> 64) as u64),
        }
    }

    /// Every key this client is counted under: itself, plus its /48 for IPv6.
    pub fn tiers(self) -> impl Iterator<Item = NetKey> {
        let wide = match self {
            NetKey::V6Prefix(prefix) => Some(NetKey::V6Wide(prefix >> 16)),
            _ => None,
        };
        std::iter::once(self).chain(wide)
    }
}

impl fmt::Display for NetKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NetKey::V4(v4) => write!(f, "{v4}"),
            NetKey::V6Prefix(prefix) => write!(f, "{prefix:016x}::/64"),
            NetKey::V6Wide(prefix) => write!(f, "{prefix:012x}::/48"),
        }
    }
}

/// Per-connection frame and byte budget.
pub struct ConnLimits {
    msgs: TokenBucket,
    bytes: TokenBucket,
}

impl ConnLimits {
    pub fn new(limits: &Limits, now: Instant) -> Self {
        Self {
            msgs: TokenBucket::new(limits.msgs_per_sec, limits.msgs_burst, now),
            // The byte burst must fit one maximum-size frame, or a legal frame
            // could never pass.
            bytes: TokenBucket::new(
                limits.bytes_per_sec,
                limits.bytes_per_sec.max(limits.max_frame_bytes as f64),
                now,
            ),
        }
    }

    pub fn allow_frame(&mut self, len: usize, now: Instant) -> bool {
        // Both are charged even if the first refuses: the connection is
        // closed on refusal anyway.
        self.msgs.take(1.0, now) & self.bytes.take(len as f64, now)
    }
}

/// Connection counts and auth attempt buckets per network, plus the global
/// connection count.
pub struct NetLimits {
    limits: Limits,
    total: usize,
    unauthenticated: usize,
    connections: HashMap<NetKey, usize>,
    auths: HashMap<NetKey, TokenBucket>,
}

impl NetLimits {
    pub fn new(limits: Limits) -> Self {
        Self {
            limits,
            total: 0,
            unauthenticated: 0,
            connections: HashMap::new(),
            auths: HashMap::new(),
        }
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// Taken before a connection thread is spawned, so a flood cannot
    /// exhaust threads before any per-network check can run. A new socket is
    /// unauthenticated, and those may fill only part of the cap: a flood of
    /// idle sockets must not lock out devices that can prove who they are.
    pub fn open_global(&mut self) -> bool {
        let cap = self.limits.global_connections;
        let unauthenticated_cap = (cap as f64 * self.limits.unauthenticated_share) as usize;
        if self.total >= cap || self.unauthenticated >= unauthenticated_cap {
            return false;
        }
        self.total += 1;
        self.unauthenticated += 1;
        true
    }

    /// The socket proved its identity and moves to the reserved share.
    pub fn authenticated(&mut self) {
        self.unauthenticated = self.unauthenticated.saturating_sub(1);
    }

    pub fn close_global(&mut self, authenticated: bool) {
        self.total = self.total.saturating_sub(1);
        if !authenticated {
            self.unauthenticated = self.unauthenticated.saturating_sub(1);
        }
    }

    fn connection_cap(&self, key: NetKey) -> usize {
        match key {
            NetKey::V4(_) => self.limits.connections_per_ipv4,
            NetKey::V6Prefix(_) => self.limits.connections_per_ipv6_64,
            NetKey::V6Wide(_) => self.limits.connections_per_ipv6_48,
        }
    }

    pub fn open_network(&mut self, key: NetKey) -> bool {
        let full = key.tiers().any(|tier| {
            self.connections.get(&tier).copied().unwrap_or(0) >= self.connection_cap(tier)
        });
        if full {
            return false;
        }
        for tier in key.tiers() {
            *self.connections.entry(tier).or_insert(0) += 1;
        }
        true
    }

    pub fn close_network(&mut self, key: NetKey) {
        for tier in key.tiers() {
            if let Some(count) = self.connections.get_mut(&tier) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.connections.remove(&tier);
                }
            }
        }
    }

    /// Charged per `auth` frame, before the signature check, because the
    /// check is the expensive part. Every tier is charged even when one
    /// refuses; the connection is closed on refusal anyway.
    pub fn allow_auth(&mut self, key: NetKey, now: Instant) -> bool {
        let mut allowed = true;
        for tier in key.tiers() {
            let per_min = match tier {
                NetKey::V4(_) => self.limits.auths_per_min_ipv4,
                NetKey::V6Prefix(_) => self.limits.auths_per_min_ipv6_64,
                NetKey::V6Wide(_) => self.limits.auths_per_min_ipv6_48,
            };
            allowed &= self
                .auths
                .entry(tier)
                .or_insert_with(|| TokenBucket::new(per_min / 60.0, per_min, now))
                .take(1.0, now);
        }
        allowed
    }

    /// Forgets networks whose auth bucket has refilled, so the map does not
    /// grow with every address ever seen.
    pub fn prune(&mut self, now: Instant) {
        self.auths.retain(|_, bucket| !bucket.is_full(now));
    }
}

const SECONDS_PER_DAY: u64 = 86_400;

#[derive(Debug, PartialEq, Eq)]
pub enum QuotaRefusal {
    Pair,
    Network,
}

/// A charged grant, handed back to [`TurnQuota::refund`] if the backend fails
/// so an outage does not burn a pair's whole day.
#[derive(Debug)]
pub struct Grant {
    day: u64,
    pair: String,
    networks: Vec<String>,
}

/// TURN grants per UTC day.
///
/// Keys are hashed with a salt that is replaced at every day rollover, so the
/// file on disk never holds raw fingerprints or IP addresses, and yesterday's
/// counters cannot be linked to today's. (IPv4 keys could still be brute
/// forced against the salt in the same file; that file is root-only state.)
// ponytail: single-process quota file; move to Redis when running more than one relay
#[derive(Default, Serialize, Deserialize)]
pub struct TurnQuota {
    day: u64,
    salt: String,
    pairs: HashMap<String, u32>,
    networks: HashMap<String, u32>,
}

impl TurnQuota {
    fn roll(&mut self, unix_secs: u64) {
        let day = unix_secs / SECONDS_PER_DAY;
        if day == self.day && !self.salt.is_empty() {
            return;
        }
        let mut salt = [0u8; 16];
        // A failed RNG only weakens the pseudonymisation; the quota still works.
        let _ = openssl::rand::rand_bytes(&mut salt);
        *self = TurnQuota {
            day,
            salt: URL_SAFE_NO_PAD.encode(salt),
            ..TurnQuota::default()
        };
    }

    fn hashed(&self, label: &str) -> String {
        let input = format!("{}\n{label}", self.salt);
        match hash(MessageDigest::sha256(), input.as_bytes()) {
            Ok(digest) => URL_SAFE_NO_PAD.encode(&digest[..16]),
            Err(_) => input,
        }
    }

    pub fn try_grant(
        &mut self,
        a: &str,
        b: &str,
        network: NetKey,
        unix_secs: u64,
        limits: &Limits,
    ) -> Result<Grant, QuotaRefusal> {
        self.roll(unix_secs);
        let (low, high) = if a <= b { (a, b) } else { (b, a) };
        let pair = self.hashed(&format!("pair\n{low}\n{high}"));
        if self.pairs.get(&pair).copied().unwrap_or(0) >= limits.turn_per_pair_per_day {
            return Err(QuotaRefusal::Pair);
        }
        let mut networks = Vec::new();
        for tier in network.tiers() {
            let cap = match tier {
                NetKey::V6Wide(_) => limits.turn_per_ipv6_48_per_day,
                _ => limits.turn_per_network_per_day,
            };
            let key = self.hashed(&format!("net\n{tier}"));
            if self.networks.get(&key).copied().unwrap_or(0) >= cap {
                return Err(QuotaRefusal::Network);
            }
            networks.push(key);
        }
        *self.pairs.entry(pair.clone()).or_insert(0) += 1;
        for key in &networks {
            *self.networks.entry(key.clone()).or_insert(0) += 1;
        }
        Ok(Grant {
            day: self.day,
            pair,
            networks,
        })
    }

    pub fn refund(&mut self, grant: Grant) {
        if grant.day != self.day {
            return;
        }
        if let Some(count) = self.pairs.get_mut(&grant.pair) {
            *count = count.saturating_sub(1);
        }
        for key in grant.networks {
            if let Some(count) = self.networks.get_mut(&key) {
                *count = count.saturating_sub(1);
            }
        }
    }

    /// A missing file is a first start. A corrupt one is reported and
    /// replaced: losing one day of counters is better than refusing to start.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|error| format!("ignoring unreadable quota state: {error}")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(format!("ignoring unreadable quota state: {error}")),
        }
    }

    /// Writes beside the target and renames, so a crash mid-write never
    /// leaves a truncated file. Mode 0600: the salt is in it.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        let bytes = serde_json::to_vec(self).map_err(io::Error::other)?;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        let mut file = options.open(&tmp)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv6Addr;
    use std::time::Duration;

    #[test]
    fn token_bucket_allows_burst_then_refills_at_rate() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(20.0, 100.0, start);
        for _ in 0..100 {
            assert!(bucket.take(1.0, start));
        }
        assert!(!bucket.take(1.0, start));
        let later = start + Duration::from_millis(500);
        for _ in 0..10 {
            assert!(bucket.take(1.0, later));
        }
        assert!(!bucket.take(1.0, later));
        assert!(bucket.is_full(later + Duration::from_secs(5)));
    }

    #[test]
    fn connection_limits_cover_frames_and_bytes() {
        let limits = Limits::default();
        let now = Instant::now();
        let mut conn = ConnLimits::new(&limits, now);
        assert!(conn.allow_frame(limits.max_frame_bytes, now));
        // The byte bucket is spent; even a tiny frame must wait.
        assert!(!conn.allow_frame(10, now));
        let mut conn = ConnLimits::new(&limits, now);
        for _ in 0..100 {
            assert!(conn.allow_frame(10, now));
        }
        assert!(!conn.allow_frame(10, now));
    }

    #[test]
    fn ipv6_is_keyed_by_slash_64_and_mapped_ipv4_is_ipv4() {
        let a: IpAddr = "2001:db8:1:2:aaaa::1".parse().unwrap();
        let b: IpAddr = "2001:db8:1:2:bbbb::9".parse().unwrap();
        let c: IpAddr = "2001:db8:1:3::1".parse().unwrap();
        assert_eq!(NetKey::of(a), NetKey::of(b));
        assert_ne!(NetKey::of(a), NetKey::of(c));
        let mapped = IpAddr::V6(Ipv4Addr::new(100, 64, 1, 2).to_ipv6_mapped());
        assert_eq!(NetKey::of(mapped), NetKey::V4(Ipv4Addr::new(100, 64, 1, 2)));
        assert_ne!(
            NetKey::of("100.64.1.2".parse().unwrap()),
            NetKey::of("100.64.1.3".parse().unwrap())
        );
        assert_eq!(
            NetKey::of(IpAddr::V6(Ipv6Addr::LOCALHOST)).to_string(),
            "0000000000000000::/64"
        );
    }

    #[test]
    fn cgnat_ipv4_gets_generous_limits_and_ipv6_strict_ones() {
        let limits = Limits::default();
        let mut net = NetLimits::new(limits);
        let v4 = NetKey::of("100.64.0.1".parse().unwrap());
        let v6 = NetKey::of("2001:db8::1".parse().unwrap());
        for _ in 0..64 {
            assert!(net.open_network(v4));
        }
        assert!(!net.open_network(v4));
        for _ in 0..8 {
            assert!(net.open_network(v6));
        }
        assert!(!net.open_network(v6));
        net.close_network(v6);
        assert!(net.open_network(v6));

        let now = Instant::now();
        assert_eq!((0..400).filter(|_| net.allow_auth(v4, now)).count(), 300);
        assert_eq!((0..400).filter(|_| net.allow_auth(v6, now)).count(), 30);
        // Thirty per minute refills one every two seconds.
        assert!(net.allow_auth(v6, now + Duration::from_secs(2)));
        net.prune(now + Duration::from_secs(120));
        assert!(net.auths.is_empty());
    }

    #[test]
    fn global_connection_cap_reserves_room_for_authenticated_sockets() {
        let mut net = NetLimits::new(Limits {
            global_connections: 4,
            unauthenticated_share: 0.5,
            ..Limits::default()
        });
        assert!(net.open_global() && net.open_global());
        // Two unauthenticated sockets fill their half.
        assert!(!net.open_global());
        net.authenticated();
        net.authenticated();
        assert!(net.open_global() && net.open_global());
        // Total cap reached.
        assert!(!net.open_global());
        net.close_global(true);
        assert!(!net.open_global(), "the freed slot is in the reserved half");
        net.close_global(false);
        assert!(net.open_global());
        assert_eq!(net.total(), 3);
    }

    #[test]
    fn a_routed_ipv6_48_cannot_multiply_its_limits_by_rotating_64s() {
        let limits = Limits::default();
        let mut net = NetLimits::new(limits);
        let subnet = |n: u16| NetKey::of(format!("2001:db8:7:{n:x}::1").parse().unwrap());
        assert_eq!(
            subnet(1).tiers().nth(1).unwrap().to_string(),
            "20010db80007::/48"
        );
        for n in 0..4 {
            for _ in 0..8 {
                assert!(net.open_network(subnet(n)));
            }
        }
        // 4 x 8 fills the /48's 32 even though /64 number 5 is untouched.
        assert!(!net.open_network(subnet(4)));
        net.close_network(subnet(0));
        assert!(net.open_network(subnet(4)));
        // Another /48 is unaffected.
        assert!(net.open_network(NetKey::of("2001:db8:8::1".parse().unwrap())));

        let now = Instant::now();
        let allowed = (0..10u16)
            .flat_map(|n| std::iter::repeat_n(n, 30))
            .filter(|n| net.allow_auth(subnet(*n), now))
            .count();
        assert_eq!(allowed, 120);

        let mut quota = TurnQuota::default();
        let granted = (0..10u16)
            .flat_map(|n| (0..48).map(move |i| (n, i)))
            .filter(|(n, i)| {
                quota
                    .try_grant("A", &format!("{n}-{i}"), subnet(*n), DAY, &limits)
                    .is_ok()
            })
            .count();
        assert_eq!(granted, 192);
    }

    const DAY: u64 = 20_000 * SECONDS_PER_DAY;

    #[test]
    fn turn_quota_is_per_sorted_pair_and_rolls_over_daily() {
        let limits = Limits::default();
        let mut quota = TurnQuota::default();
        let net = NetKey::of("2001:db8::1".parse().unwrap());
        for i in 0..20 {
            let (a, b) = if i % 2 == 0 { ("A", "B") } else { ("B", "A") };
            assert!(quota.try_grant(a, b, net, DAY + 10, &limits).is_ok());
        }
        assert_eq!(
            quota
                .try_grant("A", "B", net, DAY + 86_399, &limits)
                .unwrap_err(),
            QuotaRefusal::Pair
        );
        // Another pair from the same network still has room.
        assert!(quota.try_grant("A", "C", net, DAY + 20, &limits).is_ok());
        // Midnight UTC resets everything.
        assert!(quota
            .try_grant("A", "B", net, DAY + 86_400, &limits)
            .is_ok());
    }

    #[test]
    fn turn_quota_caps_a_network_across_pairs_and_refunds_failures() {
        let limits = Limits::default();
        let mut quota = TurnQuota::default();
        let net = NetKey::of("100.64.0.1".parse().unwrap());
        for i in 0..48 {
            quota
                .try_grant("A", &format!("peer{i}"), net, DAY, &limits)
                .unwrap();
        }
        assert_eq!(
            quota
                .try_grant("A", "fresh", net, DAY, &limits)
                .unwrap_err(),
            QuotaRefusal::Network
        );
        let other = NetKey::of("100.64.0.2".parse().unwrap());
        let grant = quota.try_grant("A", "fresh", other, DAY, &limits).unwrap();
        quota.refund(grant);
        assert_eq!(quota.pairs.values().copied().max(), Some(1));
        assert!(quota.networks.values().any(|count| *count == 0));
    }

    #[test]
    fn turn_quota_persists_without_raw_identifiers() {
        let limits = Limits::default();
        let mut quota = TurnQuota::default();
        let net = NetKey::of("198.51.100.7".parse().unwrap());
        for _ in 0..20 {
            quota
                .try_grant("fpAAAA", "fpBBBB", net, DAY, &limits)
                .unwrap();
        }
        let path = std::env::temp_dir().join(format!(
            "desklink-relay-quota-{}-{:?}.json",
            std::process::id(),
            std::thread::current().id()
        ));
        quota.save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("fpAAAA") && !text.contains("198.51.100.7"));

        let mut loaded = TurnQuota::load(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(loaded
            .try_grant("fpBBBB", "fpAAAA", net, DAY + 1, &limits)
            .is_err());
        assert!(TurnQuota::load(&path).is_ok_and(|fresh| fresh.pairs.is_empty()));
    }
}
