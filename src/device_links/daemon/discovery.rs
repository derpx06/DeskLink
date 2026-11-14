use std::collections::HashMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use super::handle::DaemonEvent;
use super::network::{bind_udp_listener, MAX_TCP_PORT, MIN_TCP_PORT};
use super::state::{mark_error, push_error, upsert_from_beacon};
use super::validation::{connect_to_device, should_throttle_connection};
use super::DaemonWorker;
use crate::device_links::config::Config;
use crate::device_links::core::DeviceManager;
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_IDENTITY};

const UDP_PORT: u16 = 1716;

/// A connection worker may lose the normal inbound/outbound race after it has
/// already entered a blocking TLS handshake. Its eventual timeout is stale:
/// publishing it would overwrite the healthy authoritative session in the UI.
fn should_publish_outgoing_failure(sessions: &DeviceManager, device_id: &str) -> bool {
    sessions.wants_bootstrap_connection(device_id)
}

/// Set when the network changes: the broadcaster starts its burst again.
static BEACON_AGAIN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Announces this computer again at once, as at startup (broadcast, then
/// unicast to each paired phone's last address).
pub(super) fn beacon_again() {
    // Every address of every phone gets a fresh chance too.
    if let Ok(mut book) = addresses().lock() {
        book.wake(None);
    }
    BEACON_AGAIN.store(true, Ordering::Release);
    let (lock, wake) = &*BEACON_WAKE;
    let _held = lock.lock().unwrap_or_else(|e| e.into_inner());
    wake.notify_all();
}

/// What the broadcaster sleeps on: a network change wakes it at once, so it
/// no longer polls the flag 4 times a second (240 wakeups a minute).
static BEACON_WAKE: std::sync::LazyLock<(Mutex<()>, std::sync::Condvar)> =
    std::sync::LazyLock::new(|| (Mutex::new(()), std::sync::Condvar::new()));

/// Longest the broadcaster sleeps before it looks at shutdown again.
const BEACON_SLEEP_SLICE: Duration = Duration::from_secs(5);

/// Tests elsewhere: a phone seen at `ip` whose dial just failed, and what is
/// due to be dialled.
#[cfg(test)]
pub(crate) fn test_dead_address(device_id: &str, ip: std::net::IpAddr) {
    if let Ok(mut book) = addresses().lock() {
        book.seen(device_id, ip);
        book.failed(device_id, ip, Instant::now());
    }
}

#[cfg(test)]
pub(crate) fn test_due(device_id: &str) -> Vec<std::net::IpAddr> {
    addresses()
        .lock()
        .map(|book| book.due(device_id, Instant::now()))
        .unwrap_or_default()
}

/// Every address of one phone gets a fresh chance at once ("Try again").
pub(crate) fn wake_addresses(device_id: &str) {
    if let Ok(mut book) = addresses().lock() {
        book.wake(Some(device_id));
    }
}

/// A phone was unpaired: its addresses go.
pub(crate) fn forget_addresses(device_id: &str) {
    if let Ok(mut book) = addresses().lock() {
        book.forget(device_id);
    }
    // What it last said about itself, and where it was last reached.
    if let Ok(mut known) = known_endpoints().lock() {
        known.remove(device_id);
    }
    if let Ok(mut cached) = PERSISTED.lock() {
        if let Some(cached) = cached.as_mut() {
            cached.remove(device_id);
        }
    }
    // And from `endpoints.json`, or its last address would come back after a
    // restart. Written off the caller's thread, like the other writes.
    let device_id = device_id.to_string();
    let _ = thread::Builder::new()
        .name("desklink-endpoints".into())
        .spawn(move || {
            unpersist_endpoint(&device_id);
        });
}

/// Rewrites `endpoints.json` without `device_id`.
fn unpersist_endpoint(device_id: &str) {
    let _writing = WRITING.lock().unwrap_or_else(|p| p.into_inner());
    let mut all = persisted_endpoints();
    if all.remove(device_id).is_none() {
        return;
    }
    if let Ok(mut cached) = PERSISTED.lock() {
        *cached = Some(all.clone());
    }
    write_endpoints(&all);
}

/// What a paired phone last said about itself and the TCP port it listens
/// on, from its identity (a beacon, or an incoming connection's first line).
/// The address comes from its `DeviceView`.
fn known_endpoints() -> &'static Mutex<HashMap<String, (DeviceInfo, u16)>> {
    static KNOWN: std::sync::OnceLock<Mutex<HashMap<String, (DeviceInfo, u16)>>> =
        std::sync::OnceLock::new();
    KNOWN.get_or_init(Default::default)
}

/// Records a paired phone's listening port from an identity packet (a
/// beacon, or an incoming connection's first line). Identities are
/// unauthenticated, so strangers' are not kept: the map would grow with
/// every spoofed id. The address is remembered across restarts only once a
/// paired link authenticates (`persist_paired_endpoint`).
pub(super) fn remember_endpoint(info: &DeviceInfo, packet: &NetworkPacket, trusted: bool) {
    let Some(port) = packet
        .get_i64("tcpPort")
        .and_then(|port| u16::try_from(port).ok())
        .filter(|port| (MIN_TCP_PORT..=MAX_TCP_PORT).contains(port))
    else {
        return;
    };
    if !trusted {
        return;
    }
    if let Ok(mut known) = known_endpoints().lock() {
        known.insert(info.id.clone(), (info.clone(), port));
    }
}

/// Most phones `endpoints.json` remembers.
const MAX_PERSISTED_ENDPOINTS: usize = 32;

/// After a paired link authenticated from `ip`: remembers where the phone
/// listens (that address and the port it announced), for the first dial
/// after a restart. Only paired phones are kept (`trusted`), at most
/// [`MAX_PERSISTED_ENDPOINTS`], written off the caller's thread.
pub(super) fn persist_paired_endpoint(
    device_id: &str,
    ip: std::net::IpAddr,
    trusted: std::collections::HashSet<String>,
) {
    // An emulator behind this computer's NAT arrives from this computer's
    // own address: that is not where it listens.
    if ip.is_unspecified() || is_own_address(ip, &own_addresses()) {
        return;
    }
    let Some(port) = known_endpoints()
        .lock()
        .ok()
        .and_then(|known| known.get(device_id).map(|(_, port)| *port))
    else {
        return;
    };
    note_address(device_id, ip);
    let device_id = device_id.to_string();
    let _ = thread::Builder::new()
        .name("desklink-endpoints".into())
        .spawn(move || {
            // One writer at a time: two links authenticating together must
            // not interleave read-modify-write.
            persist_endpoint(&device_id, SocketAddr::new(ip, port), &trusted);
        });
}

/// Serialises the read-modify-write of `endpoints.json` and its cache. Taken
/// by [`persist_endpoint`] and [`unpersist_endpoint`] themselves.
static WRITING: Mutex<()> = Mutex::new(());

/// Where each paired phone was last reachable, kept across restarts so the
/// first dial after a restart needs no discovery (`endpoints.json` beside
/// the connection history).
fn endpoints_path() -> Option<std::path::PathBuf> {
    if cfg!(test) {
        return Some(
            std::env::temp_dir()
                .join(format!("desklink-test-endpoints-{}", std::process::id()))
                .join("endpoints.json"),
        );
    }
    Some(
        dirs::state_dir()
            .or_else(dirs::data_local_dir)?
            .join("desklink")
            .join("endpoints.json"),
    )
}

/// The remembered addresses, by device id, read from the file.
pub(super) fn persisted_endpoints() -> HashMap<String, SocketAddr> {
    endpoints_path()
        .and_then(|path| std::fs::read_to_string(path).ok())
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

static PERSISTED: Mutex<Option<HashMap<String, SocketAddr>>> = Mutex::new(None);

/// Where `device_id` was last reached, as remembered across runs.
pub(super) fn persisted_ip(device_id: &str) -> Option<std::net::IpAddr> {
    persisted_snapshot().get(device_id).map(SocketAddr::ip)
}

/// The remembered addresses as of now: read once, then kept current by
/// [`persist_endpoint`] (the redialer used to hold the snapshot from its
/// start for ever, so an address learned later was never in it).
fn persisted_snapshot() -> HashMap<String, SocketAddr> {
    let Ok(mut cached) = PERSISTED.lock() else {
        return HashMap::new();
    };
    cached.get_or_insert_with(persisted_endpoints).clone()
}

fn persist_endpoint(
    device_id: &str,
    address: SocketAddr,
    trusted: &std::collections::HashSet<String>,
) {
    // One read-modify-write of file and cache at a time: a concurrent change
    // of another phone's address must not be overwritten by a stale snapshot.
    let _writing = WRITING.lock().unwrap_or_else(|p| p.into_inner());
    let before = persisted_endpoints();
    let all = updated_endpoints(before.clone(), device_id, address, trusted);
    if let Ok(mut cached) = PERSISTED.lock() {
        *cached = Some(all.clone());
    }
    if all == before {
        return;
    }
    write_endpoints(&all);
}

fn write_endpoints(all: &HashMap<String, SocketAddr>) {
    let Some(path) = endpoints_path() else {
        return;
    };
    let written = serde_json::to_string(all)
        .map_err(|error| error.to_string())
        .and_then(|text| {
            // Where phones were seen is private to this user (0700 / 0600).
            path.parent()
                .map_or(Ok(()), crate::device_links::private_fs::create_dir_all)
                .and_then(|()| crate::device_links::private_fs::write(&path, text.as_bytes()))
                .map_err(|error| error.to_string())
        });
    if let Err(error) = written {
        eprintln!("[Daemon] Could not remember the phones' addresses: {error}");
    }
}

/// `all` with `device_id` at `address`, pruned to paired phones and capped.
fn updated_endpoints(
    mut all: HashMap<String, SocketAddr>,
    device_id: &str,
    address: SocketAddr,
    trusted: &std::collections::HashSet<String>,
) -> HashMap<String, SocketAddr> {
    all.retain(|id, _| trusted.contains(id));
    if trusted.contains(device_id) {
        all.insert(device_id.to_string(), address);
    }
    while all.len() > MAX_PERSISTED_ENDPOINTS {
        let Some(extra) = all.keys().find(|id| *id != device_id).cloned() else {
            break;
        };
        all.remove(&extra);
    }
    all
}

/// How often paired phones without a bootstrap link are considered for a
/// redial (each address has its own backoff).
const REDIAL_EVERY: Duration = Duration::from_secs(1);
/// The redialer's tick while no phone is paired.
const REDIAL_IDLE: Duration = Duration::from_secs(15);
/// Addresses kept per phone (the newest IPv4s; one IPv6 on top).
const MAX_ADDRESSES: usize = 3;
/// A dead address is retried after this, doubling per failure up to
/// [`MAX_ADDRESS_BACKOFF`]: not every few seconds for ever.
const FIRST_ADDRESS_BACKOFF: Duration = Duration::from_secs(3);
const MAX_ADDRESS_BACKOFF: Duration = Duration::from_secs(300);
/// How long a dial may be in flight (connect 3 s, pinned handshake
/// [`super::network::HANDSHAKE_LIMIT`], and room to register) before its
/// address may be dialled again without hearing how it ended.
const DIAL_LEASE: Duration = Duration::from_secs(10);

/// Unverified addresses (a beacon's source, an mDNS answer, the card's
/// address) a phone may have in the book at once. They are dialled like any
/// other, but can never push a verified address out: a spoofed beacon could
/// otherwise evict the real ones.
const MAX_CANDIDATES: usize = 2;
/// Gap between unicast beacons to one address, doubling per beacon up to
/// [`MAX_BEACON_GAP`]: an address that never answers is not told about this
/// computer every 15 s for ever.
const FIRST_BEACON_GAP: Duration = Duration::from_secs(15);
const MAX_BEACON_GAP: Duration = Duration::from_secs(300);
/// A beacon from a phone may wake its backed-off addresses and the recovery
/// ladder at most this often. Beacons are not authenticated.
const BEACON_EFFECT_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
struct Address {
    ip: std::net::IpAddr,
    failures: u32,
    retry_at: Option<Instant>,
    /// Seen through a pinned, authenticated handshake (now or in an earlier
    /// run) rather than only claimed by an unauthenticated packet.
    verified: bool,
    /// Unicast beacons sent here since the last sign of the phone.
    beacons: u32,
    beacon_at: Option<Instant>,
    /// A dial to it is in flight until this (see [`DIAL_LEASE`]); a wake
    /// leaves it, so a sign of the phone never starts a second dial beside
    /// the first.
    dialling_until: Option<Instant>,
}

impl Address {
    fn new(ip: std::net::IpAddr, verified: bool) -> Self {
        Self {
            ip,
            failures: 0,
            retry_at: None,
            verified,
            beacons: 0,
            beacon_at: None,
            dialling_until: None,
        }
    }
}

fn unusable(ip: std::net::IpAddr) -> bool {
    // An IPv6 link-local (fe80::/10) needs its interface's scope id, which an
    // `IpAddr` cannot carry: every dial of one failed with EINVAL (seen on
    // the real desk, 17 times in an hour on a phone hotspot).
    let link_local_v6 = matches!(ip, std::net::IpAddr::V6(v6) if v6.is_unicast_link_local());
    ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() || link_local_v6
}

/// `ip` is this computer: loopback, or an address of one of its interfaces
/// (`own`). A phone in an emulator, behind this computer's NAT, connects
/// *from* this computer's address; recorded as the phone's, it made DeskLink
/// dial its own listener every few seconds for ever.
pub(super) fn is_own_address(ip: std::net::IpAddr, own: &[std::net::IpAddr]) -> bool {
    let ip = ip.to_canonical();
    ip.is_loopback() || own.iter().any(|mine| mine.to_canonical() == ip)
}

/// This computer's interface addresses, now.
pub(super) fn own_addresses() -> Vec<std::net::IpAddr> {
    super::network::local_interfaces()
        .into_iter()
        .map(|(_, ip)| ip)
        .collect()
}

/// Where each paired phone was last seen: its last [`MAX_ADDRESSES`] IPv4s
/// (from beacons, authenticated links and mDNS) and its newest IPv6. Phones
/// move between networks and DHCP leases, so dialling only the newest one
/// left a phone on its old address unreachable for minutes.
#[derive(Debug, Default)]
pub(super) struct AddressBook {
    phones: HashMap<String, Vec<Address>>,
    /// This computer's own addresses ([`Self::set_own`]): never a phone's.
    own: Vec<std::net::IpAddr>,
}

impl AddressBook {
    /// Whether `ip` may be recorded (and so dialled) as a phone's address.
    fn usable(&self, ip: std::net::IpAddr) -> bool {
        !unusable(ip) && !is_own_address(ip, &self.own)
    }

    /// This computer's addresses are now `own`: any of them recorded for a
    /// phone (earlier, or before an interface took it) is dropped.
    pub(super) fn set_own(&mut self, own: Vec<std::net::IpAddr>) {
        self.own = own;
        let own = &self.own;
        for list in self.phones.values_mut() {
            list.retain(|known| !is_own_address(known.ip, own));
        }
    }

    /// `ip` is where `device_id` was just seen: newest first, a repeat moves
    /// up and is tried again at once.
    pub(super) fn seen(&mut self, device_id: &str, ip: std::net::IpAddr) {
        if !self.usable(ip) {
            return;
        }
        let list = self.phones.entry(device_id.to_string()).or_default();
        // A dial in flight keeps its lease through a repeat sighting.
        let dialling = list
            .iter()
            .find(|known| known.ip == ip)
            .and_then(|known| known.dialling_until);
        list.retain(|known| known.ip != ip);
        let mut address = Address::new(ip, true);
        address.dialling_until = dialling;
        list.insert(0, address);
        Self::trim_verified(list);
    }

    /// Keeps the newest [`MAX_ADDRESSES`] verified IPv4s and one IPv6.
    fn trim_verified(list: &mut Vec<Address>) {
        let (mut v4, mut v6) = (0, 0);
        list.retain(|known| {
            if !known.verified {
                return true;
            }
            let counter = if known.ip.is_ipv4() { &mut v4 } else { &mut v6 };
            *counter += 1;
            *counter <= if known.ip.is_ipv4() { MAX_ADDRESSES } else { 1 }
        });
    }

    /// An address only an unauthenticated packet vouches for (a beacon, an
    /// mDNS answer, the address on the card): added if absent, last in line,
    /// and never in place of a verified one. It becomes verified when a
    /// pinned handshake over it works ([`Self::worked`]). A known address is
    /// left exactly as it is (its backoff is not reset).
    pub(super) fn candidate(&mut self, device_id: &str, ip: std::net::IpAddr) {
        if !self.usable(ip) {
            return;
        }
        let list = self.phones.entry(device_id.to_string()).or_default();
        if list.iter().any(|known| known.ip == ip) {
            return;
        }
        list.push(Address::new(ip, false));
        while list.iter().filter(|known| !known.verified).count() > MAX_CANDIDATES {
            if let Some(oldest) = list.iter().position(|known| !known.verified) {
                list.remove(oldest);
            }
        }
    }

    /// An address remembered from an earlier run's authenticated link: added
    /// if absent (last in line), its backoff untouched if present.
    pub(super) fn remembered(&mut self, device_id: &str, ip: std::net::IpAddr) {
        if !self.usable(ip) {
            return;
        }
        let list = self.phones.entry(device_id.to_string()).or_default();
        match list.iter_mut().find(|known| known.ip == ip) {
            Some(known) => known.verified = true,
            None => list.push(Address::new(ip, true)),
        }
        Self::trim_verified(list);
    }

    /// The addresses of `device_id` to send a unicast beacon to now, and
    /// when each next one is due: the gap doubles per beacon, so a phone that
    /// never answers costs a packet every five minutes, not every 15 s.
    pub(super) fn beacon_targets(
        &mut self,
        device_id: &str,
        now: Instant,
    ) -> Vec<std::net::IpAddr> {
        let Some(list) = self.phones.get_mut(device_id) else {
            return Vec::new();
        };
        let mut due = Vec::new();
        for known in list {
            if known.beacon_at.is_none_or(|at| at <= now) {
                let gap = FIRST_BEACON_GAP
                    .saturating_mul(2u32.saturating_pow(known.beacons.min(10)))
                    .min(MAX_BEACON_GAP);
                known.beacons = known.beacons.saturating_add(1);
                known.beacon_at = Some(now + gap);
                due.push(known.ip);
            }
        }
        due
    }

    /// The addresses to dial now: those whose backoff is over and that are
    /// not being dialled, newest first.
    pub(super) fn due(&self, device_id: &str, now: Instant) -> Vec<std::net::IpAddr> {
        self.phones
            .get(device_id)
            .map(|list| {
                list.iter()
                    .filter(|known| {
                        known.retry_at.is_none_or(|at| at <= now)
                            && known.dialling_until.is_none_or(|at| at <= now)
                    })
                    .map(|known| known.ip)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// [`Self::due`], each marked as being dialled until it is reported
    /// [`Self::failed`] or [`Self::worked`] (or [`DIAL_LEASE`] passes).
    pub(super) fn start_dials(&mut self, device_id: &str, now: Instant) -> Vec<std::net::IpAddr> {
        let due = self.due(device_id, now);
        if let Some(list) = self.phones.get_mut(device_id) {
            for known in list.iter_mut().filter(|known| due.contains(&known.ip)) {
                known.dialling_until = Some(now + DIAL_LEASE);
            }
        }
        due
    }

    /// A dial to `ip` failed: it waits longer next time.
    pub(super) fn failed(&mut self, device_id: &str, ip: std::net::IpAddr, now: Instant) {
        if let Some(known) = self
            .phones
            .get_mut(device_id)
            .and_then(|list| list.iter_mut().find(|known| known.ip == ip))
        {
            known.failures = known.failures.saturating_add(1);
            known.retry_at = Some(now + address_backoff(known.failures));
            known.dialling_until = None;
        }
    }

    /// How a dial to `ip` ended: a pinned handshake verifies the address,
    /// a failure backs it off (that address only: the phone's others are
    /// still tried), and a dial that ended with nothing to verify (the link
    /// was no longer wanted, the phone is unpaired or showed a new
    /// certificate) only frees it.
    pub(super) fn dial_ended(
        &mut self,
        device_id: &str,
        ip: std::net::IpAddr,
        outcome: &Result<bool, String>,
        now: Instant,
    ) {
        match outcome {
            Ok(true) => self.worked(device_id, ip),
            Err(_) => self.failed(device_id, ip, now),
            Ok(false) => self.release(device_id, ip),
        }
    }

    /// A dial to `ip` worked: the others are only fallbacks now, and `ip`
    /// stays first.
    pub(super) fn worked(&mut self, device_id: &str, ip: std::net::IpAddr) {
        self.seen(device_id, ip);
        self.release(device_id, ip);
    }

    #[cfg(test)]
    fn dial_started_for_test(&mut self, device_id: &str, ip: std::net::IpAddr, now: Instant) {
        if let Some(known) = self
            .phones
            .get_mut(device_id)
            .and_then(|list| list.iter_mut().find(|known| known.ip == ip))
        {
            known.dialling_until = Some(now + DIAL_LEASE);
        }
    }

    /// The dial to `ip` ended (or never started) with nothing to learn from.
    pub(super) fn release(&mut self, device_id: &str, ip: std::net::IpAddr) {
        if let Some(known) = self
            .phones
            .get_mut(device_id)
            .and_then(|list| list.iter_mut().find(|known| known.ip == ip))
        {
            known.dialling_until = None;
        }
    }

    /// Any sign of the phone (a network change, a beacon): every address is
    /// tried again at once.
    pub(super) fn wake(&mut self, device_id: Option<&str>) {
        for (id, list) in self.phones.iter_mut() {
            if device_id.is_none_or(|only| only == id) {
                for known in list {
                    known.failures = 0;
                    known.retry_at = None;
                    known.beacons = 0;
                    known.beacon_at = None;
                }
            }
        }
    }

    /// Forgets `device_id` (unpaired).
    pub(super) fn forget(&mut self, device_id: &str) {
        self.phones.remove(device_id);
    }
}

/// Whether a beacon from `device_id` may wake its addresses and the recovery
/// ladder now (at most every [`BEACON_EFFECT_EVERY`] per phone): a beacon is an
/// unauthenticated UDP packet anyone on the network can repeat.
fn beacon_may_act(device_id: &str, now: Instant) -> bool {
    static LAST: Mutex<Option<HashMap<String, Instant>>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return false;
    };
    let last = last.get_or_insert_with(HashMap::new);
    if last.len() > 256 {
        last.retain(|_, at| now.saturating_duration_since(*at) < BEACON_EFFECT_EVERY);
    }
    match last.get(device_id) {
        Some(at) if now.saturating_duration_since(*at) < BEACON_EFFECT_EVERY => false,
        _ => {
            last.insert(device_id.to_string(), now);
            true
        }
    }
}

/// Probes `device_id`'s link if it has been quiet for [`super::QUIET_LINK`]
/// (on the phone's beacon). A socket the phone dropped with its network is
/// answered by a reset now that the phone is back, which ends it and wakes
/// the redialer, instead of waiting for the 10 s liveness probe. The send is
/// on its own thread: the listener must not wait on a socket.
pub(super) fn probe_silent_link(
    sessions: &DeviceManager,
    device_id: &str,
) -> Option<thread::JoinHandle<()>> {
    let binding = sessions.silent_link(device_id, super::QUIET_LINK)?;
    thread::Builder::new()
        .spawn(move || {
            let probe = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
            let _ = binding.link.send_packet(&probe);
        })
        .ok()
}

/// How long a dead address waits after its `failures`-th failure.
pub(super) fn address_backoff(failures: u32) -> Duration {
    FIRST_ADDRESS_BACKOFF
        .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1).min(10)))
        .min(MAX_ADDRESS_BACKOFF)
}

pub(super) fn addresses() -> &'static Mutex<AddressBook> {
    static BOOK: std::sync::OnceLock<Mutex<AddressBook>> = std::sync::OnceLock::new();
    BOOK.get_or_init(Default::default)
}

/// Records where `device_id` was seen (a paired phone's beacon, link or mDNS
/// answer).
pub(super) fn note_address(device_id: &str, ip: std::net::IpAddr) {
    if let Ok(mut book) = addresses().lock() {
        book.seen(device_id, ip);
    }
}

/// Records an address an unauthenticated packet claims (an mDNS answer): a
/// candidate, dialled but never in place of a verified address.
pub(super) fn note_candidate_address(device_id: &str, ip: std::net::IpAddr) {
    // Only this network: an answer naming a public host would make this
    // computer dial (and tell) a stranger.
    if !beacon_source_ok(ip) {
        return;
    }
    if let Ok(mut book) = addresses().lock() {
        book.candidate(device_id, ip);
    }
}

/// The paired phones to dial now: no live bootstrap link (whether or not
/// their WebRTC peer survives), a last-known address and listening port.
///
/// Heard from this run (`known`): its address and port. Otherwise, after a
/// restart before any beacon, the address remembered from last time
/// (`persisted`), with what the trust store says about the phone.
fn redial_targets(
    devices: &HashMap<String, DeviceView>,
    known: &HashMap<String, (DeviceInfo, u16)>,
    persisted: &HashMap<String, SocketAddr>,
    book: &mut AddressBook,
    now: Instant,
    wants: impl Fn(&str) -> bool,
) -> Vec<(DeviceInfo, std::net::IpAddr, u16)> {
    let mut targets = Vec::new();
    for device in devices
        .values()
        .filter(|device| device.trusted && wants(&device.id))
    {
        // Where it listens, and who it is: what it announced this run, else
        // what the trust store and the remembered address say.
        let (info, port) = match (known.get(&device.id), persisted.get(&device.id)) {
            (Some((info, port)), _) => (info.clone(), *port),
            (None, Some(address)) => (
                DeviceInfo {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    device_type: device.device_type.clone(),
                    protocol_version: device.protocol_version,
                    incoming_capabilities: Vec::new(),
                    outgoing_capabilities: Vec::new(),
                    instance_id: None,
                    app_version: None,
                },
                address.port(),
            ),
            (None, None) => continue,
        };
        // Every address it was seen on, newest first. The address on its
        // `DeviceView` and the remembered one are put in the book (when
        // absent, without touching a backoff) so that they wait out their
        // failures like any other: dialled straight from the card or the
        // file, a dead one was retried every second for ever.
        if let Ok(card) = device.address.parse() {
            book.candidate(&device.id, card);
        }
        if let Some(remembered) = persisted.get(&device.id) {
            book.remembered(&device.id, remembered.ip());
        }
        for ip in book.start_dials(&device.id, now) {
            targets.push((info.clone(), ip, port));
        }
    }
    targets
}

impl DaemonWorker {
    /// Keeps a TCP link to every paired phone it can reach. The phone often
    /// cannot dial in: on its own hotspot it hears only its own beacons, and
    /// a phone whose screen is off is frozen by its OS until *inbound* TCP
    /// arrives — this link (and its 10 s heartbeat) is how it is thawed. So
    /// the desk dials the addresses the phone was seen on itself (each on its
    /// own backoff, a dead one less and less often), including while a
    /// WebRTC peer survives without its bootstrap socket (the new socket is
    /// adopted into the session; see `attach_bootstrap_link`). Failures are
    /// expected and not shown.
    pub(super) fn start_redialer(&self) {
        let devices = Arc::clone(&self.devices);
        let sessions = Arc::clone(&self.sessions);
        let config = Arc::clone(&self.config);
        let events = Arc::clone(&self.events);
        let shutdown = Arc::clone(&self.shutdown);
        thread::spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                // Nothing paired: nothing to redial, so it parks (a network
                // change or beacon wakes it) instead of ticking every second.
                let paired = devices
                    .lock()
                    .is_ok_and(|devices| devices.values().any(|device| device.trusted));
                let (lock, wake) = &*BEACON_WAKE;
                let held = lock.lock().unwrap_or_else(|e| e.into_inner());
                let _ = wake.wait_timeout(held, if paired { REDIAL_EVERY } else { REDIAL_IDLE });
                if !paired {
                    continue;
                }
                // Read before any lock: interfaces come and go (a hotspot,
                // a VPN), and this computer never dials itself.
                let own = own_addresses();
                let targets = {
                    let (Ok(devices), Ok(known)) = (devices.lock(), known_endpoints().lock())
                    else {
                        continue;
                    };
                    let Ok(mut book) = addresses().lock() else {
                        continue;
                    };
                    let persisted = persisted_snapshot();
                    book.set_own(own);
                    redial_targets(
                        &devices,
                        &known,
                        &persisted,
                        &mut book,
                        Instant::now(),
                        |id| sessions.wants_bootstrap_connection(id),
                    )
                };
                for (info, ip, port) in targets {
                    let Some(slot) = super::network::HandshakeSlot::claim(ip) else {
                        // No dial started: the lease goes back at once.
                        if let Ok(mut book) = addresses().lock() {
                            book.release(&info.id, ip);
                        }
                        continue;
                    };
                    let (release_id, release_ip) = (info.id.clone(), ip);
                    let (config, devices, sessions, events) = (
                        Arc::clone(&config),
                        Arc::clone(&devices),
                        Arc::clone(&sessions),
                        Arc::clone(&events),
                    );
                    let spawned = thread::Builder::new().spawn(move || {
                        let _slot = slot;
                        let id = info.id.clone();
                        let outcome = connect_to_device(
                            info,
                            SocketAddr::new(ip, port),
                            port,
                            config,
                            devices,
                            sessions,
                            events,
                        );
                        if let Ok(mut book) = addresses().lock() {
                            book.dial_ended(&id, ip, &outcome, Instant::now());
                        }
                        if let Err(error) = outcome {
                            debug_log!(
                                "[DL-WRTC-BOOT] redial of {id} at {ip}:{port} failed: {error}"
                            );
                        }
                    });
                    if spawned.is_err() {
                        if let Ok(mut book) = addresses().lock() {
                            book.release(&release_id, release_ip);
                        }
                    }
                }
            }
        });
    }

    pub(super) fn start_udp_listener(&self) -> Result<(), String> {
        let socket = bind_udp_listener()?;
        socket
            // Only to look at `shutdown` again: 5 s, not 2 wakeups a second.
            .set_read_timeout(Some(Duration::from_secs(5)))
            .map_err(|err| err.to_string())?;
        socket.set_broadcast(true).map_err(|err| err.to_string())?;
        let config = Arc::clone(&self.config);
        let devices = Arc::clone(&self.devices);
        let sessions = Arc::clone(&self.sessions);
        let errors = Arc::clone(&self.errors);
        let events = Arc::clone(&self.events);
        let shutdown = Arc::clone(&self.shutdown);
        let tcp_port = self.tcp_port;
        let last_connections = Arc::new(Mutex::new(HashMap::new()));

        thread::spawn(move || {
            let mut buffer = [0u8; 65536];
            while !shutdown.load(Ordering::Acquire) {
                match socket.recv_from(&mut buffer) {
                    Ok((len, address)) => {
                        if let Ok(packet) = NetworkPacket::deserialize(&buffer[..len]) {
                            if packet.packet_type == PACKET_TYPE_IDENTITY {
                                handle_identity_packet(
                                    packet,
                                    address,
                                    tcp_port,
                                    &config,
                                    &devices,
                                    &sessions,
                                    &errors,
                                    &events,
                                    &last_connections,
                                );
                            }
                        }
                    }
                    Err(err)
                        if err.kind() == std::io::ErrorKind::WouldBlock
                            || err.kind() == std::io::ErrorKind::TimedOut
                            // A signal (suspend/resume) interrupted the wait: just read again.
                            || err.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(err) => {
                        push_error(&errors, format!("UDP discovery failed: {err}"));
                        // A socket that keeps failing must not spin this thread.
                        thread::sleep(Duration::from_millis(250));
                    }
                }
            }
        });

        Ok(())
    }

    pub(super) fn start_broadcaster(&self) {
        let config = Arc::clone(&self.config);
        let errors = Arc::clone(&self.errors);
        let shutdown = Arc::clone(&self.shutdown);
        let devices = Arc::clone(&self.devices);
        let tcp_port = self.tcp_port;
        thread::spawn(move || {
            // Burst at 0, 1, 3 and 7 s, then every 15 s. A single startup
            // beacon is easily lost or lands before the phone's listener is
            // up, and the next one 15 s later was most of a cold start.
            let mut sent: usize = 0;
            while !shutdown.load(Ordering::Acquire) {
                if let Err(error) = broadcast_identity(&config, tcp_port) {
                    push_error(&errors, error);
                }
                beacon_unreachable_paired(&config, &devices, tcp_port);
                let wait_ms = BEACON_BURST_GAPS_MS.get(sent).copied().unwrap_or(15_000);
                sent = sent.saturating_add(1);
                let until = Instant::now() + Duration::from_millis(wait_ms);
                loop {
                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                    // The network changed: start the burst over, now.
                    if BEACON_AGAIN.swap(false, Ordering::AcqRel) {
                        sent = 0;
                        break;
                    }
                    let left = until.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    let (lock, wake) = &*BEACON_WAKE;
                    let held = lock.lock().unwrap_or_else(|e| e.into_inner());
                    // Re-checked under the lock a wake needs: none is lost.
                    if BEACON_AGAIN.load(Ordering::Acquire) {
                        continue;
                    }
                    let _ = wake.wait_timeout(held, left.min(BEACON_SLEEP_SLICE));
                }
            }
        });
    }
}

impl DaemonWorker {
    /// Advertises `_desklink._udp` over mDNS, the path Android already browses
    /// (`MdnsDiscovery.kt`). Many access points filter broadcast but pass
    /// multicast, so this is a second, independent way to be found. Uses its
    /// own responder rather than Avahi, which is off on many desktops.
    /// Best effort: discovery still works by broadcast if this fails.
    pub(super) fn start_mdns_advertiser(&self) {
        let Ok(info) = self.config.lock().map(|config| config.local_device_info()) else {
            return;
        };
        let shutdown = Arc::clone(&self.shutdown);
        let trusted = Arc::clone(&self.config);
        let own_id = info.id.clone();
        let spawned = thread::Builder::new()
            .name("desklink-mdns".to_string())
            .spawn(move || {
                let daemon = match mdns_sd::ServiceDaemon::new() {
                    Ok(daemon) => daemon,
                    Err(error) => {
                        eprintln!("[Daemon] mDNS advertising unavailable: {error}");
                        return;
                    }
                };
                let protocol = info.protocol_version.to_string();
                let properties = [
                    ("id", info.id.as_str()),
                    ("name", info.name.as_str()),
                    ("type", info.device_type.as_str()),
                    ("protocol", protocol.as_str()),
                ];
                let service = mdns_sd::ServiceInfo::new(
                    MDNS_SERVICE_TYPE,
                    &info.id,
                    &format!("{}.local.", info.id),
                    "",
                    UDP_PORT,
                    &properties[..],
                )
                .map(mdns_sd::ServiceInfo::enable_addr_auto);
                match service
                    .map_err(|error| error.to_string())
                    .and_then(|service| daemon.register(service).map_err(|error| error.to_string()))
                {
                    Ok(()) => debug_log!("[Daemon] Advertising {MDNS_SERVICE_TYPE} over mDNS"),
                    Err(error) => eprintln!("[Daemon] mDNS advertising failed: {error}"),
                }
                // Also browse: a phone found by mDNS is another address to
                // dial (broadcast-filtering networks often pass multicast).
                let browser = daemon.browse(MDNS_SERVICE_TYPE).ok();
                while !shutdown.load(Ordering::Acquire) {
                    let Some(browser) = browser.as_ref() else {
                        thread::sleep(Duration::from_secs(5));
                        continue;
                    };
                    if let Ok(mdns_sd::ServiceEvent::ServiceResolved(found)) =
                        browser.recv_timeout(Duration::from_secs(2))
                    {
                        let Some(id) = found.get_property_val_str("id").map(str::to_string) else {
                            continue;
                        };
                        if id == own_id
                            || !trusted.lock().is_ok_and(|config| config.is_trusted(&id))
                        {
                            continue;
                        }
                        for ip in found.get_addresses() {
                            note_candidate_address(&id, ip.to_ip_addr());
                        }
                    }
                }
                let _ = daemon.shutdown();
            });
        if let Err(error) = spawned {
            eprintln!("[Daemon] could not start mDNS advertising: {error}");
        }
    }
}

/// Must match `DeskLinkProtocol.MDNS_SERVICE_TYPE` on Android.
const MDNS_SERVICE_TYPE: &str = "_desklink._udp.local.";

/// Gaps after each of the first beacons; afterwards every 15 s.
const BEACON_BURST_GAPS_MS: [u64; 3] = [1_000, 2_000, 4_000];

pub(super) fn broadcast_identity(config: &Arc<Mutex<Config>>, tcp_port: u16) -> Result<(), String> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).map_err(|err| err.to_string())?;
    socket.set_broadcast(true).map_err(|err| err.to_string())?;
    let packet = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())?
        .local_device_info()
        .to_identity_packet(tcp_port);
    socket
        .send_to(
            &packet.serialize_line().map_err(|err| err.to_string())?,
            ("255.255.255.255", UDP_PORT),
        )
        .map_err(|err| err.to_string())?;
    Ok(())
}

/// Sends the identity beacon straight to each paired device that is not
/// reachable, at the address it was last seen on. Access points that filter
/// broadcast (and multicast) still pass unicast, and the phone dials back on
/// any beacon it hears.
// ponytail: last addresses live in memory only; persist them if cold starts
// on broadcast-filtering networks turn out to matter.
fn beacon_unreachable_paired(
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    tcp_port: u16,
) {
    let unreachable: Vec<(String, Option<std::net::IpAddr>)> = devices
        .lock()
        .map(|devices| {
            devices
                .values()
                .filter(|device| device.trusted && device.status == DeviceStatus::Unreachable)
                .map(|device| (device.id.clone(), device.address.parse().ok()))
                .collect()
        })
        .unwrap_or_default();
    // Every address a phone was seen on, not only the newest, each on its own
    // backoff: one that never answers is told less and less often.
    let mut targets: Vec<std::net::IpAddr> = Vec::new();
    if let Ok(mut book) = addresses().lock() {
        let now = Instant::now();
        for (id, current) in &unreachable {
            if let Some(current) = current {
                book.candidate(id, *current);
            }
            targets.extend(book.beacon_targets(id, now));
        }
    }
    targets.sort();
    targets.dedup();
    if targets.is_empty() {
        return;
    }
    let Ok(packet) = config.lock().map_err(|_| ()).and_then(|config| {
        config
            .local_device_info()
            .to_identity_packet(tcp_port)
            .serialize_line()
            .map_err(|_| ())
    }) else {
        return;
    };
    let Ok(socket) = UdpSocket::bind(("0.0.0.0", 0)) else {
        return;
    };
    for ip in targets {
        let _ = socket.send_to(&packet, (ip, UDP_PORT));
    }
}

/// Whether a beacon from `ip` may act at all: a private or link-local
/// address (the local network), or loopback (the isolated test harness).
fn beacon_source_ok(ip: std::net::IpAddr) -> bool {
    ip.is_loopback() || crate::device_links::lan_scope::is_private_or_link_local(ip)
}

#[allow(clippy::too_many_arguments)]
fn handle_identity_packet(
    packet: NetworkPacket,
    address: SocketAddr,
    local_tcp_port: u16,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    sessions: &Arc<DeviceManager>,
    errors: &Arc<Mutex<Vec<String>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
    last_connections: &Arc<Mutex<HashMap<String, Instant>>>,
) {
    let Some(info) = DeviceInfo::from_identity_packet(&packet) else {
        return;
    };
    let local_id = config
        .lock()
        .ok()
        .map(|config| config.local_device_id().to_string());
    if local_id.as_deref() == Some(info.id.as_str()) {
        return;
    }
    // Only this network: a spoofed source must not make this computer send
    // its identity and a TLS hello to an arbitrary host.
    if !beacon_source_ok(address.ip()) {
        return;
    }
    if should_throttle_connection(last_connections, &info.id) {
        return;
    }

    // A beacon from a paired phone is a sign of it: its addresses get a fresh
    // chance, and a recovery ladder that ran out starts over (and reconnects
    // at once). It is not authenticated, so this happens at most every few
    // seconds per phone, and its source is only a candidate address until a
    // pinned handshake over it works.
    if config
        .lock()
        .is_ok_and(|config| config.is_trusted(&info.id))
        && beacon_may_act(&info.id, Instant::now())
    {
        if let Ok(mut book) = addresses().lock() {
            book.candidate(&info.id, address.ip());
            book.wake(Some(&info.id));
        }
        crate::device_links::webrtc::coordinator::recovery_sign(
            &info.id, sessions, config, devices, events,
        );
        probe_silent_link(sessions, &info.id);
    }

    // A closed link stays installed while a WebRTC transport outlives it, so
    // ask about the socket rather than the binding: otherwise a device whose
    // socket died is never offered a replacement connection.
    // ...unless this beacon comes from a restarted process: then the socket
    // and WebRTC peer this side holds are the dead previous instance's.
    if !sessions.wants_bootstrap_connection(&info.id)
        && !sessions.peer_restarted(&info.id, info.instance_id.as_deref())
    {
        return;
    }
    let paired = config
        .lock()
        .map(|config| config.is_trusted(&info.id))
        .unwrap_or(false);
    if paired
        && config
            .lock()
            .ok()
            .and_then(|config| config.trusted_protocol_version(&info.id))
            .is_some_and(|trusted_version| trusted_version > info.protocol_version)
    {
        push_error(
            errors,
            format!(
                "Refusing downgraded protocol from {}: {}",
                info.name, info.protocol_version
            ),
        );
        return;
    }
    upsert_from_beacon(devices, &info, address.ip().to_string(), paired);
    remember_endpoint(&info, &packet, paired);

    let Some(remote_port) = packet
        .get_i64("tcpPort")
        .and_then(|port| u16::try_from(port).ok())
    else {
        return;
    };
    if !(MIN_TCP_PORT..=MAX_TCP_PORT).contains(&remote_port) {
        return;
    }
    // Dialling this computer's own listener reaches only itself (an emulator
    // behind its NAT beacons from its address).
    if remote_port == local_tcp_port && is_own_address(address.ip(), &own_addresses()) {
        return;
    }

    let config = Arc::clone(config);
    let devices = Arc::clone(devices);
    let sessions = Arc::clone(sessions);
    let events = Arc::clone(events);

    let Some(slot) = super::network::HandshakeSlot::claim(address.ip()) else {
        return;
    };
    let spawned = thread::Builder::new().spawn(move || {
        let _slot = slot;
        let connected = connect_to_device(
            info.clone(),
            address,
            remote_port,
            config,
            devices.clone(),
            Arc::clone(&sessions),
            events,
        );
        if connected == Ok(true) {
            // Authenticated over this address: now it is a verified one.
            if let Ok(mut book) = addresses().lock() {
                book.worked(&info.id, address.ip());
            }
        }
        if let Err(error) = connected {
            if should_publish_outgoing_failure(&sessions, &info.id) {
                mark_error(&devices, &info.id, error.clone());
                eprintln!(
                    "[Daemon] Outgoing TCP connection to {} failed: {}",
                    info.id, error
                );
            } else {
                eprintln!(
                    "[DL-WRTC-BOOT] ignored stale outgoing connection failure for {}: {}",
                    info.id, error
                );
            }
        }
    });
    if let Err(error) = spawned {
        eprintln!("[Daemon] could not start an outgoing connection thread: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paired_phones_without_a_live_link_are_redialled_at_their_last_address() {
        let info = |id: &str| DeviceInfo {
            id: id.into(),
            name: id.into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        let view = |id: &str, address: &str, trusted: bool| {
            DeviceView::from_info(&info(id), address.to_string(), trusted)
        };
        let devices: HashMap<String, DeviceView> = [
            view("linked", "10.0.0.2", true),
            view("unlinked", "10.0.0.3", true),
            view("stranger", "10.0.0.4", false),
            view("no-port", "10.0.0.5", true),
            view("relay-only", "unknown", true),
        ]
        .into_iter()
        .map(|device| (device.id.clone(), device))
        .collect();
        let mut beacon = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        beacon.set("tcpPort", 1716);
        for id in ["linked", "unlinked", "stranger", "relay-only"] {
            remember_endpoint(&info(id), &beacon, true);
        }
        let mut odd = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        odd.set("tcpPort", 22);
        remember_endpoint(&info("no-port"), &odd, true);
        let known = known_endpoints().lock().unwrap().clone();
        // `wants` is the session's view: here only "linked" has a live socket.
        let mut persisted = HashMap::new();
        persisted.insert("restarted".to_string(), "10.0.0.7:1716".parse().unwrap());
        let mut devices = devices;
        let restarted = view("restarted", "", true);
        devices.insert(restarted.id.clone(), restarted);
        let targets = redial_targets(
            &devices,
            &known,
            &persisted,
            &mut AddressBook::default(),
            Instant::now(),
            |id| id != "linked",
        );
        let dialled: Vec<_> = targets
            .iter()
            .map(|(info, ip, port)| (info.id.as_str(), ip.to_string(), *port))
            .collect();
        let mut dialled = dialled;
        dialled.sort();
        assert_eq!(
            dialled,
            [
                ("restarted", "10.0.0.7".to_string(), 1716),
                ("unlinked", "10.0.0.3".to_string(), 1716)
            ],
            "after a restart, the address remembered from last time"
        );
    }
    use crate::device_links::core::SessionLink;

    fn ip(text: &str) -> std::net::IpAddr {
        text.parse().unwrap()
    }

    /// The emulator behind this computer's NAT connected from 10.180.213.94,
    /// this computer's own address; recorded as the phone's, DeskLink dialled
    /// itself every few seconds for ever ("Connection targeted another device").
    #[test]
    fn this_computers_own_addresses_are_never_a_phones() {
        let own = [ip("10.180.213.94"), ip("fe80::5"), ip("172.17.0.1")];
        assert!(is_own_address(ip("10.180.213.94"), &own));
        assert!(is_own_address(ip("::ffff:10.180.213.94"), &own), "mapped");
        assert!(is_own_address(ip("127.0.0.1"), &own));
        assert!(is_own_address(ip("::1"), &[]));
        assert!(!is_own_address(ip("10.180.213.40"), &own));

        let mut book = AddressBook::default();
        let now = Instant::now();
        // Recorded before this computer took the address (an old record, or a
        // hotspot that came up later): dropped once it is known to be ours.
        book.remembered("emulator", ip("10.180.213.94"));
        book.seen("emulator", ip("10.180.213.40"));
        book.set_own(own.to_vec());
        assert_eq!(book.due("emulator", now), [ip("10.180.213.40")]);
        // From then on it is never recorded, whatever vouches for it.
        book.seen("emulator", ip("10.180.213.94"));
        book.candidate("emulator", ip("172.17.0.1"));
        book.remembered("emulator", ip("fe80::5"));
        assert_eq!(book.due("emulator", now), [ip("10.180.213.40")]);
    }

    #[test]
    fn a_phone_keeps_its_last_three_ipv4_addresses_and_one_ipv6() {
        let mut book = AddressBook::default();
        let now = Instant::now();
        for address in [
            "10.0.0.1", "10.0.0.2", "10.0.0.3", "10.0.0.4", "fd00::1", "fd00::2",
        ] {
            book.seen("phone", ip(address));
        }
        assert_eq!(
            book.due("phone", now),
            [
                ip("fd00::2"),
                ip("10.0.0.4"),
                ip("10.0.0.3"),
                ip("10.0.0.2")
            ],
            "newest first; the oldest IPv4 and the older IPv6 are gone"
        );
        // Seen again: moves to the front, once.
        book.seen("phone", ip("10.0.0.2"));
        assert_eq!(book.due("phone", now)[0], ip("10.0.0.2"));
        assert_eq!(book.due("phone", now).len(), 4);
        // Never loopback, unspecified, multicast or an IPv6 link-local (no
        // scope id to dial it with).
        book.seen("phone", ip("fe80::6cf2:9aff:feca:755c"));
        book.seen("phone", ip("127.0.0.1"));
        book.seen("phone", ip("0.0.0.0"));
        book.seen("phone", ip("224.0.0.1"));
        assert_eq!(book.due("phone", now).len(), 4);
        assert!(book.due("phone", now).contains(&ip("fd00::2")));
        book.forget("phone");
        assert!(book.due("phone", now).is_empty());
    }

    #[test]
    fn a_dead_address_backs_off_instead_of_being_redialled_every_few_seconds() {
        let backoffs: Vec<u64> = (1..=9).map(|n| address_backoff(n).as_secs()).collect();
        assert_eq!(backoffs, [3, 6, 12, 24, 48, 96, 192, 300, 300]);
        let mut book = AddressBook::default();
        let start = Instant::now();
        book.seen("phone", ip("10.0.0.2"));
        book.seen("phone", ip("10.0.0.3"));
        // The newest is dead; the other is still dialled at once.
        book.failed("phone", ip("10.0.0.3"), start);
        assert_eq!(book.due("phone", start), [ip("10.0.0.2")]);
        assert!(book.due("phone", start + Duration::from_secs(2)).len() == 1);
        assert_eq!(book.due("phone", start + Duration::from_secs(3)).len(), 2);
        // Each failure doubles the wait.
        book.failed("phone", ip("10.0.0.3"), start);
        assert_eq!(book.due("phone", start + Duration::from_secs(5)).len(), 1);
        assert_eq!(book.due("phone", start + Duration::from_secs(6)).len(), 2);
        // A sign of the phone (a beacon, a network change) clears every wait.
        book.failed("phone", ip("10.0.0.2"), start);
        book.failed("phone", ip("10.0.0.3"), start);
        assert!(book.due("phone", start).is_empty());
        book.wake(Some("phone"));
        assert_eq!(book.due("phone", start).len(), 2);
        // Others are untouched by one phone's wake.
        book.seen("other", ip("10.0.0.9"));
        book.failed("other", ip("10.0.0.9"), start);
        book.wake(Some("phone"));
        assert!(book.due("other", start).is_empty());
        book.wake(None);
        assert_eq!(book.due("other", start).len(), 1);
    }

    /// A network change starts the beacon burst at once and gives every dead
    /// address a fresh chance.
    /// Where phones were seen is written 0600 in a 0700 folder.
    #[test]
    fn remembered_addresses_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let trusted = test_phones();
        persist_endpoint("private-phone", "10.1.2.3:1716".parse().unwrap(), &trusted);
        let path = endpoints_path().unwrap();
        let mode =
            |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(path.parent().unwrap()), 0o700);
    }

    /// An unpaired phone's last address must not return after a restart.
    #[test]
    fn an_unpaired_phones_endpoint_leaves_the_file() {
        let trusted = test_phones();
        persist_endpoint("gone-phone", "10.1.2.4:1716".parse().unwrap(), &trusted);
        persist_endpoint("kept-phone", "10.1.2.5:1716".parse().unwrap(), &trusted);
        unpersist_endpoint("gone-phone");
        let after = persisted_endpoints();
        assert!(!after.contains_key("gone-phone"));
        assert!(after.contains_key("kept-phone"), "only that phone's");
        assert!(!persisted_snapshot().contains_key("gone-phone"));
    }

    /// Every phone the tests persist: `persist_endpoint` prunes the file to
    /// the paired set it is given, so tests running side by side must all
    /// give the same one or they erase each other's entries.
    fn test_phones() -> std::collections::HashSet<String> {
        ["private-phone", "late-phone", "gone-phone", "kept-phone"]
            .iter()
            .map(|id| id.to_string())
            .chain((0..10).flat_map(|n| [format!("race-keep-{n}"), format!("race-gone-{n}")]))
            .collect()
    }

    /// Interleaved persists and unpersists of different phones lose neither.
    #[test]
    fn concurrent_persist_and_unpersist_of_different_phones_lose_neither() {
        let trusted = test_phones();
        for n in 0..10 {
            persist_endpoint(
                &format!("race-gone-{n}"),
                "10.3.0.1:1716".parse().unwrap(),
                &trusted,
            );
        }
        let writer = {
            let trusted = trusted.clone();
            std::thread::spawn(move || {
                for n in 0..10 {
                    persist_endpoint(
                        &format!("race-keep-{n}"),
                        "10.3.0.2:1716".parse().unwrap(),
                        &trusted,
                    );
                }
            })
        };
        for n in 0..10 {
            unpersist_endpoint(&format!("race-gone-{n}"));
        }
        writer.join().unwrap();
        let (file, cache) = (persisted_endpoints(), persisted_snapshot());
        for n in 0..10 {
            for map in [&file, &cache] {
                assert!(map.contains_key(&format!("race-keep-{n}")), "kept {n} lost");
                assert!(
                    !map.contains_key(&format!("race-gone-{n}")),
                    "gone {n} back"
                );
            }
        }
    }

    #[test]
    fn network_up_restarts_the_beacons_and_wakes_every_backed_off_address() {
        let id = "network-up-phone";
        note_address(id, ip("10.9.0.2"));
        note_address(id, ip("10.9.0.3"));
        let now = Instant::now();
        {
            let mut book = addresses().lock().unwrap();
            book.failed(id, ip("10.9.0.2"), now);
            book.failed(id, ip("10.9.0.3"), now);
            assert!(book.due(id, now).is_empty());
        }
        BEACON_AGAIN.store(false, Ordering::Release);
        beacon_again();
        assert!(
            BEACON_AGAIN.swap(false, Ordering::AcqRel),
            "the burst starts over"
        );
        assert_eq!(addresses().lock().unwrap().due(id, now).len(), 2);
        // The unicast burst reaches every address of an unreachable phone.
        let burst = addresses().lock().unwrap().beacon_targets(id, now);
        assert_eq!(burst.len(), 2);
        forget_addresses(id);
    }

    #[test]
    fn the_redialer_dials_every_address_the_phone_was_seen_on() {
        let info = DeviceInfo {
            id: "multi".into(),
            name: "Multi".into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        let mut view = DeviceView::from_info(&info, "10.0.0.8".to_string(), true);
        view.trusted = true;
        let devices: HashMap<String, DeviceView> = [("multi".to_string(), view)].into();
        let known: HashMap<String, (DeviceInfo, u16)> =
            [("multi".to_string(), (info, 1716))].into();
        let mut book = AddressBook::default();
        let now = Instant::now();
        for address in ["10.0.0.5", "10.0.0.6", "10.0.0.7"] {
            book.seen("multi", ip(address));
        }
        book.failed("multi", ip("10.0.0.6"), now);
        let targets = redial_targets(&devices, &known, &HashMap::new(), &mut book, now, |_| true);
        let mut dialled: Vec<String> = targets.iter().map(|(_, ip, _)| ip.to_string()).collect();
        dialled.sort();
        assert_eq!(
            dialled,
            ["10.0.0.5", "10.0.0.7", "10.0.0.8"],
            "the three seen (one backing off) and the address on its card"
        );
        assert!(targets
            .iter()
            .all(|(info, _, port)| info.id == "multi" && *port == 1716));
    }

    /// A dial still in flight (connect 3 s + handshake 5 s) is not started
    /// again on the next 1 s tick: the repeats filled the host's three
    /// handshake slots, which the phone's own dial back needs too.
    #[test]
    fn an_address_being_dialled_is_not_dialled_again_until_that_dial_ends() {
        let info = plain_info("busy");
        let mut view = DeviceView::from_info(&info, "10.0.0.8".to_string(), true);
        view.trusted = true;
        let devices: HashMap<String, DeviceView> = [("busy".to_string(), view)].into();
        let known: HashMap<String, (DeviceInfo, u16)> = [("busy".to_string(), (info, 1716))].into();
        let mut book = AddressBook::default();
        book.seen("busy", ip("10.0.0.8"));
        let now = Instant::now();
        let tick = |at: Instant, book: &mut AddressBook| {
            redial_targets(&devices, &known, &HashMap::new(), book, at, |_| true).len()
        };
        assert_eq!(tick(now, &mut book), 1);
        for second in 1..=8 {
            assert_eq!(
                tick(now + Duration::from_secs(second), &mut book),
                0,
                "at {second} s"
            );
        }
        // The dial failed: the address waits out its backoff, then goes again.
        book.failed("busy", ip("10.0.0.8"), now + Duration::from_secs(8));
        assert_eq!(tick(now + Duration::from_secs(10), &mut book), 0);
        assert_eq!(tick(now + Duration::from_secs(11), &mut book), 1);
        // A wake (a beacon, a network change) does not start a second dial
        // beside the one in flight.
        book.wake(Some("busy"));
        assert_eq!(tick(now + Duration::from_secs(12), &mut book), 0);
        // A repeat sighting of the address (a persisted endpoint) keeps the lease.
        book.dial_started_for_test("busy", ip("10.0.0.8"), now + Duration::from_secs(14));
        book.seen("busy", ip("10.0.0.8"));
        assert_eq!(
            tick(now + Duration::from_secs(15), &mut book),
            0,
            "seen() dropped the lease"
        );
        book.release("busy", ip("10.0.0.8"));
        assert_eq!(
            tick(now + Duration::from_secs(15), &mut book),
            1,
            "a dial that never started is freed"
        );
        // One that worked is dialled again whenever the link is wanted.
        book.worked("busy", ip("10.0.0.8"));
        assert_eq!(tick(now + Duration::from_secs(13), &mut book), 1);
        // A dial that never reports back (a stuck thread) holds it no longer
        // than the lease.
        assert_eq!(
            tick(now + Duration::from_secs(13) + DIAL_LEASE, &mut book),
            1
        );
    }

    /// Only a pinned handshake verifies an address: a dial that ended
    /// without one (the link stopped being wanted while it connected, an
    /// unpaired phone, a new certificate) leaves a beacon's address a
    /// candidate, so it can never push out a verified one.
    #[test]
    fn only_a_pinned_handshake_verifies_the_address_dialled() {
        let mut book = AddressBook::default();
        let now = Instant::now();
        for real in ["10.0.0.1", "10.0.0.2", "10.0.0.3"] {
            book.seen("pinned", ip(real));
        }
        book.candidate("pinned", ip("10.0.0.66"));
        assert_eq!(book.start_dials("pinned", now).len(), 4);
        book.dial_ended("pinned", ip("10.0.0.66"), &Ok(false), now);
        let verified = |book: &AddressBook| {
            book.phones["pinned"]
                .iter()
                .filter(|known| known.verified)
                .map(|known| known.ip.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(verified(&book), ["10.0.0.3", "10.0.0.2", "10.0.0.1"]);
        assert_eq!(
            book.due("pinned", now),
            [ip("10.0.0.66")],
            "freed, not backed off"
        );
        book.dial_ended("pinned", ip("10.0.0.1"), &Err("refused".into()), now);
        assert!(
            !book.due("pinned", now).contains(&ip("10.0.0.1")),
            "backed off"
        );
        book.dial_ended("pinned", ip("10.0.0.66"), &Ok(true), now);
        assert_eq!(verified(&book)[0], "10.0.0.66", "verified and first");
    }

    fn plain_info(id: &str) -> DeviceInfo {
        DeviceInfo {
            id: id.into(),
            name: id.into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        }
    }

    /// Review finding A: the address on the card and the remembered one were
    /// dialled straight from `redial_targets` every second when dead, because
    /// only addresses already in the book had a backoff.
    #[test]
    fn the_cards_address_and_the_remembered_one_back_off_when_dead() {
        let info = plain_info("seeded");
        let view = DeviceView::from_info(&info, "10.1.0.8".to_string(), true);
        let devices: HashMap<String, DeviceView> = [("seeded".to_string(), view)].into();
        let known: HashMap<String, (DeviceInfo, u16)> =
            [("seeded".to_string(), (info, 1716))].into();
        let persisted: HashMap<String, SocketAddr> =
            [("seeded".to_string(), "10.1.0.9:1716".parse().unwrap())].into();
        let mut book = AddressBook::default();
        let start = Instant::now();
        let dialled = |book: &mut AddressBook, at: Instant| {
            let mut ips: Vec<String> =
                redial_targets(&devices, &known, &persisted, book, at, |_| true)
                    .iter()
                    .map(|(_, ip, _)| ip.to_string())
                    .collect();
            ips.sort();
            ips
        };
        assert_eq!(dialled(&mut book, start), ["10.1.0.8", "10.1.0.9"]);
        // Both are dead.
        book.failed("seeded", ip("10.1.0.8"), start);
        book.failed("seeded", ip("10.1.0.9"), start);
        // Every second passes; nothing is dialled until the backoff is over.
        for second in 1..3 {
            assert!(
                dialled(&mut book, start + Duration::from_secs(second)).is_empty(),
                "second {second}"
            );
        }
        assert_eq!(
            dialled(&mut book, start + Duration::from_secs(3)).len(),
            2,
            "then again"
        );
        // The seeds never reset a failure count (and the other dial worked).
        book.failed("seeded", ip("10.1.0.8"), start + Duration::from_secs(3));
        book.worked("seeded", ip("10.1.0.9"));
        assert_eq!(address_backoff(2).as_secs(), 6);
        assert!(dialled(&mut book, start + Duration::from_secs(8)).len() == 1);
    }

    /// Review finding A: the remembered addresses were read once at start.
    #[test]
    fn an_address_remembered_later_is_seen_by_the_redialer() {
        let trusted = test_phones();
        let before = persisted_snapshot();
        assert!(!before.contains_key("late-phone"));
        persist_endpoint("late-phone", "10.2.0.5:1716".parse().unwrap(), &trusted);
        assert_eq!(
            persisted_snapshot().get("late-phone"),
            Some(&"10.2.0.5:1716".parse().unwrap())
        );
    }

    /// Review finding I: a spoofed beacon must not push the real addresses
    /// out, and an unverified address becomes verified only by a handshake.
    #[test]
    fn unverified_addresses_never_evict_verified_ones() {
        let mut book = AddressBook::default();
        let now = Instant::now();
        for real in ["10.3.0.1", "10.3.0.2", "10.3.0.3"] {
            book.seen("phone", ip(real));
        }
        for fake in ["10.3.9.1", "10.3.9.2", "10.3.9.3", "10.3.9.4"] {
            book.candidate("phone", ip(fake));
        }
        let due = book.due("phone", now);
        for real in ["10.3.0.1", "10.3.0.2", "10.3.0.3"] {
            assert!(due.contains(&ip(real)), "{real} is still there");
        }
        assert_eq!(due.len(), 5, "three real, at most two unverified");
        // Verified only once a pinned handshake worked over it.
        book.worked("phone", ip("10.3.9.4"));
        assert!(book.due("phone", now).contains(&ip("10.3.9.4")));
        // A known address is not reset by a candidate sighting.
        book.failed("phone", ip("10.3.0.3"), now);
        book.candidate("phone", ip("10.3.0.3"));
        assert!(!book.due("phone", now).contains(&ip("10.3.0.3")));
    }

    /// Review finding I: a beacon's effect is rate-limited per phone.
    #[test]
    fn a_beacon_wakes_a_phone_at_most_every_five_seconds() {
        let start = Instant::now();
        assert!(beacon_may_act("beacon-phone", start));
        assert!(!beacon_may_act(
            "beacon-phone",
            start + Duration::from_secs(1)
        ));
        assert!(!beacon_may_act(
            "beacon-phone",
            start + Duration::from_secs(4)
        ));
        assert!(beacon_may_act(
            "beacon-phone",
            start + Duration::from_secs(5)
        ));
        assert!(beacon_may_act(
            "other-beacon-phone",
            start + Duration::from_secs(1)
        ));
    }

    /// Review finding I: unicast beacons share the per-address backoff.
    #[test]
    fn unicast_beacons_to_a_silent_address_get_rarer() {
        let mut book = AddressBook::default();
        book.seen("silent", ip("10.4.0.1"));
        let start = Instant::now();
        let sent = |book: &mut AddressBook, at: u64| {
            book.beacon_targets("silent", start + Duration::from_secs(at))
                .len()
        };
        assert_eq!(sent(&mut book, 0), 1);
        assert_eq!(sent(&mut book, 10), 0);
        assert_eq!(sent(&mut book, 15), 1);
        assert_eq!(sent(&mut book, 30), 0, "the gap doubled to 30 s");
        assert_eq!(sent(&mut book, 45), 1);
        // The 300 s cap.
        let mut at = 45;
        for _ in 0..8 {
            at += 301;
            assert_eq!(sent(&mut book, at), 1);
        }
        // A sign of the phone starts it over.
        book.wake(Some("silent"));
        assert_eq!(sent(&mut book, at + 1), 1);
        assert_eq!(sent(&mut book, at + 2), 0);
        book.wake(Some("silent"));
        assert_eq!(sent(&mut book, at + 3), 1);
    }

    /// Only paired phones are remembered, strangers' identities are not
    /// kept even in memory, and the file stays bounded.
    #[test]
    fn only_paired_phones_endpoints_are_kept_and_the_file_is_bounded() {
        let info = |id: &str| DeviceInfo {
            id: id.to_string(),
            name: id.to_string(),
            device_type: "phone".to_string(),
            protocol_version: crate::device_links::packet::PROTOCOL_VERSION,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        let mut beacon = NetworkPacket::new(PACKET_TYPE_IDENTITY);
        beacon.set("tcpPort", 1716);
        remember_endpoint(&info("spoofed-stranger"), &beacon, false);
        assert!(!known_endpoints()
            .lock()
            .unwrap()
            .contains_key("spoofed-stranger"));

        let address: SocketAddr = "10.0.0.9:1716".parse().unwrap();
        let mut trusted: std::collections::HashSet<String> =
            ["phone".to_string(), "kept".to_string()].into();
        let mut all = HashMap::new();
        all.insert("unpaired-since".to_string(), address);
        all.insert("kept".to_string(), address);
        let updated = updated_endpoints(all, "phone", address, &trusted);
        let mut ids: Vec<_> = updated.keys().cloned().collect();
        ids.sort();
        assert_eq!(ids, ["kept", "phone"], "pruned to paired phones");
        assert!(
            updated_endpoints(HashMap::new(), "stranger", address, &trusted).is_empty(),
            "never a stranger"
        );
        let many: HashMap<String, SocketAddr> =
            (0..100).map(|n| (format!("p{n}"), address)).collect();
        trusted.extend(many.keys().cloned());
        let capped = updated_endpoints(many, "phone", address, &trusted);
        assert_eq!(capped.len(), MAX_PERSISTED_ENDPOINTS);
        assert!(capped.contains_key("phone"), "the one just seen stays");
    }

    #[test]
    fn stale_outgoing_failure_is_not_published_after_an_inbound_link_wins() {
        let sessions = DeviceManager::new();
        assert!(should_publish_outgoing_failure(&sessions, "phone-1"));

        sessions
            .register_link(
                "phone-1".to_string(),
                SessionLink::test_placeholder("phone-1"),
                true,
            )
            .unwrap();

        assert!(!should_publish_outgoing_failure(&sessions, "phone-1"));
    }

    /// A beacon from outside this network does nothing, and one for a paired
    /// phone never rewrites its card.
    #[test]
    fn beacons_are_local_and_never_rewrite_a_paired_card() {
        let ip = |text: &str| text.parse::<std::net::IpAddr>().unwrap();
        assert!(beacon_source_ok(ip("192.168.1.3")));
        assert!(beacon_source_ok(ip("fe80::1")));
        assert!(beacon_source_ok(ip("127.0.0.1")));
        assert!(!beacon_source_ok(ip("8.8.8.8")));
        assert!(!beacon_source_ok(ip("2a00:1450::1")));
        let info = |name: &str| DeviceInfo {
            id: "phone-1".into(),
            name: name.into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        let devices = Arc::new(Mutex::new(HashMap::new()));
        upsert_from_beacon(&devices, &info("Pixel"), "192.168.1.3".into(), true);
        // A spoofed beacon for the same id: card unchanged.
        upsert_from_beacon(&devices, &info("EVIL PHONE"), "192.168.1.66".into(), true);
        let card = devices.lock().unwrap().get("phone-1").cloned().unwrap();
        assert_eq!(
            (card.name.as_str(), card.address.as_str()),
            ("Pixel", "192.168.1.3")
        );
    }
}
