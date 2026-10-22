//! Delta resync (W2): what is sent again when a connection comes back.
//!
//! Every snapshot a side sends carries an opaque `rev` (a hash prefix of its
//! own content; both sides only compare equality with what they sent, never
//! across languages). A receiver remembers the `rev` of each feature it has
//! *fully applied* from the peer's current process, and on every
//! feature-ready tells the peer first, before any feature packet:
//! `desklink.sync.have {peerInstance, have: {feature: rev}}`.
//!
//! Each side then holds its own initial snapshots for at most [`SYNC_WAIT`]
//! for the peer's `have` and sends only the features whose `rev` differs —
//! everything when the peer saw an older process of ours (a new epoch), or
//! when no `have` came. Revs live in memory only, so a restart of either
//! process re-sends everything by construction. Changes after ready are
//! always sent live.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::time::Duration;

use super::packet::{NetworkPacket, PACKET_TYPE_SYNC_HAVE};

/// The features that are digested; battery, telephony, lock and ping are
/// always sent. Fixed set, each key at most [`MAX_FEATURE_CHARS`].
pub(crate) const FEATURES: [&str; 6] = [
    "notifications",
    "contacts",
    "media",
    "notes",
    "snippets",
    "settings",
];
/// How long own initial snapshots wait for the peer's `have`.
pub(crate) const SYNC_WAIT: Duration = Duration::from_millis(1000);
pub(crate) const MAX_REV_CHARS: usize = 64;
pub(crate) const MAX_FEATURE_CHARS: usize = 32;
pub(crate) const MAX_HAVE_ENTRIES: usize = 8;

/// What the peer said it has (its `have` of this connection).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Theirs {
    /// The instance id of the process of ours its revs were received from.
    pub(crate) peer_instance: String,
    pub(crate) have: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
struct PeerSync {
    /// The peer process the revs below were received from.
    received_from: Option<String>,
    /// feature → rev, fully applied from `received_from`.
    have: BTreeMap<String, String>,
    /// The peer's `have` for the current connection.
    theirs: Option<Theirs>,
    /// A connection waiting for the peer's `have` (its token).
    waiting: Option<u64>,
}

fn state() -> &'static Mutex<HashMap<String, PeerSync>> {
    static STATE: std::sync::OnceLock<Mutex<HashMap<String, PeerSync>>> =
        std::sync::OnceLock::new();
    STATE.get_or_init(Default::default)
}

fn with<T>(device_id: &str, job: impl FnOnce(&mut PeerSync) -> T) -> Option<T> {
    let mut all = state().lock().ok()?;
    Some(job(all.entry(device_id.to_string()).or_default()))
}

/// The `rev` of `packets` (one snapshot): a SHA-256 prefix of their types and
/// bodies, without any `rev` and without the volatile `skip` keys.
pub(crate) fn rev_of(packets: &[NetworkPacket], skip: &[&str]) -> String {
    let mut bytes = Vec::new();
    for packet in packets {
        bytes.extend_from_slice(packet.packet_type.as_bytes());
        bytes.push(0);
        let mut body = packet.body.clone();
        body.remove("rev");
        for key in skip {
            body.remove(*key);
        }
        bytes.extend_from_slice(serde_json::to_string(&body).unwrap_or_default().as_bytes());
        bytes.push(1);
    }
    openssl::sha::sha256(&bytes)
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Puts `rev` on every part of a snapshot.
pub(crate) fn stamp(packets: &mut [NetworkPacket], rev: &str) {
    for packet in packets {
        packet.set("rev", rev);
    }
}

/// A snapshot received from the peer is fully applied: remembered, unless it
/// is not a feature we digest or the rev is malformed. A different `instance`
/// than before means the peer restarted: what was remembered is forgotten.
pub(crate) fn note_received(
    device_id: &str,
    instance: Option<&str>,
    feature: &str,
    packet: &NetworkPacket,
) {
    let Some(rev) = packet
        .get_str("rev")
        .filter(|rev| !rev.is_empty() && rev.chars().count() <= MAX_REV_CHARS)
    else {
        return;
    };
    let (Some(instance), true) = (instance, FEATURES.contains(&feature)) else {
        return;
    };
    with(device_id, |peer| {
        if peer.received_from.as_deref() != Some(instance) {
            peer.have.clear();
            peer.received_from = Some(instance.to_string());
        }
        peer.have.insert(feature.to_string(), rev.to_string());
    });
}

/// Forgets everything about `device_id` (unpaired).
pub(crate) fn forget_device(device_id: &str) {
    if let Ok(mut all) = state().lock() {
        all.remove(device_id);
    }
}

/// The connection to `device_id` ended: what the peer said it had is stale.
/// (What was received stays: a quick reconnect of the same process resends
/// only what changed.)
pub(crate) fn forget_connection(device_id: &str) {
    with(device_id, |peer| {
        peer.theirs = None;
        peer.waiting = None;
    });
}

/// The `desklink.sync.have` this side sends first on feature-ready:
/// the revs held from `peer_instance`, or none when the peer is another
/// process than the one they came from.
pub(crate) fn have_packet(device_id: &str, peer_instance: Option<&str>) -> NetworkPacket {
    let (instance, have) = with(device_id, |peer| {
        match (peer_instance, &peer.received_from) {
            (Some(now), Some(then)) if now == then => (now.to_string(), peer.have.clone()),
            (Some(now), _) => (now.to_string(), BTreeMap::new()),
            (None, _) => (String::new(), BTreeMap::new()),
        }
    })
    .unwrap_or_default();
    let mut packet = NetworkPacket::new(PACKET_TYPE_SYNC_HAVE);
    packet.set("peerInstance", instance);
    packet.set("have", serde_json::to_value(have).unwrap_or_default());
    packet
}

/// A peer's `have`, bounded: at most [`MAX_HAVE_ENTRIES`] known features
/// with sane revs; anything else is ignored.
pub(crate) fn parse_have(packet: &NetworkPacket) -> Option<Theirs> {
    let peer_instance = packet.get_str("peerInstance")?.to_string();
    let have = packet
        .body
        .get("have")?
        .as_object()?
        .iter()
        .take(MAX_HAVE_ENTRIES)
        .filter(|(feature, _)| feature.chars().count() <= MAX_FEATURE_CHARS)
        .filter_map(|(feature, rev)| {
            let rev = rev.as_str()?;
            (FEATURES.contains(&feature.as_str()) && rev.chars().count() <= MAX_REV_CHARS)
                .then(|| (feature.clone(), rev.to_string()))
        })
        .collect();
    Some(Theirs {
        peer_instance,
        have,
    })
}

/// A `have` arrived: remembered for this connection. Whether a send was
/// waiting for it (and so is released now).
pub(crate) fn on_have(device_id: &str, packet: &NetworkPacket) -> bool {
    let Some(theirs) = parse_have(packet) else {
        return false;
    };
    with(device_id, |peer| {
        peer.theirs = Some(theirs);
        peer.waiting.take().is_some()
    })
    .unwrap_or(false)
}

/// Starts waiting for the peer's `have`. `None` when it already came (the
/// snapshots can go now); else the token the timer releases with.
pub(crate) fn begin_wait(device_id: &str) -> Option<u64> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    with(device_id, |peer| {
        if peer.theirs.is_some() {
            return None;
        }
        let token = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        peer.waiting = Some(token);
        Some(token)
    })
    .flatten()
}

/// The wait ended without a `have`: whether `token` was still waiting (so the
/// snapshots go now, all of them).
pub(crate) fn release_wait(device_id: &str, token: u64) -> bool {
    with(device_id, |peer| {
        if peer.waiting == Some(token) {
            peer.waiting = None;
            true
        } else {
            false
        }
    })
    .unwrap_or(false)
}

pub(crate) fn theirs(device_id: &str) -> Option<Theirs> {
    with(device_id, |peer| peer.theirs.clone()).flatten()
}

/// Whether a snapshot of `feature` with `rev` is sent: unless the peer's
/// `have` is for this very process of ours and already holds this rev.
pub(crate) fn should_send(
    theirs: Option<&Theirs>,
    own_instance: &str,
    feature: &str,
    rev: &str,
) -> bool {
    match theirs {
        None => true,
        Some(theirs) if theirs.peer_instance != own_instance => true,
        Some(theirs) => theirs.have.get(feature).map(String::as_str) != Some(rev),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(feature_body: &str) -> NetworkPacket {
        let mut packet = NetworkPacket::new("desklink.settings");
        packet.set("keepAwake", feature_body == "on");
        packet
    }

    fn have(instance: &str, entries: &[(&str, &str)]) -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_SYNC_HAVE);
        packet.set("peerInstance", instance);
        packet.set(
            "have",
            serde_json::to_value(
                entries
                    .iter()
                    .map(|(feature, rev)| (feature.to_string(), rev.to_string()))
                    .collect::<BTreeMap<_, _>>(),
            )
            .unwrap(),
        );
        packet
    }

    #[test]
    fn a_rev_follows_content_and_ignores_itself_and_what_moves() {
        let on = snapshot("on");
        let rev = rev_of(std::slice::from_ref(&on), &[]);
        assert!(rev.len() <= MAX_REV_CHARS && rev.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(rev, rev_of(std::slice::from_ref(&on), &[]), "stable");
        assert_ne!(rev, rev_of(&[snapshot("off")], &[]), "content changes it");
        // A stamped copy has the same rev (the rev is not part of itself).
        let mut stamped = vec![on.clone()];
        stamp(&mut stamped, &rev);
        assert_eq!(stamped[0].get_str("rev"), Some(rev.as_str()));
        assert_eq!(rev_of(&stamped, &[]), rev);
        // Volatile keys are left out (the media position).
        let mut playing = NetworkPacket::new("desklink.mpris");
        playing.set("title", "x");
        playing.set("pos", 1);
        let mut later = playing.clone();
        later.set("pos", 2);
        assert_eq!(
            rev_of(&[playing.clone()], &["pos"]),
            rev_of(&[later], &["pos"])
        );
        // Every part of a multi-part snapshot gets the same rev.
        let mut parts = vec![snapshot("on"), snapshot("off")];
        let whole = rev_of(&parts, &[]);
        stamp(&mut parts, &whole);
        assert!(parts
            .iter()
            .all(|part| part.get_str("rev") == Some(whole.as_str())));
    }

    #[test]
    fn only_what_the_peers_have_names_as_current_is_skipped() {
        let own = "desk-instance";
        let theirs = Theirs {
            peer_instance: own.into(),
            have: [("settings".to_string(), "r1".to_string())].into(),
        };
        // The same process of ours and the same rev: skipped.
        assert!(!should_send(Some(&theirs), own, "settings", "r1"));
        // A different rev, or a feature it does not list: sent.
        assert!(should_send(Some(&theirs), own, "settings", "r2"));
        assert!(should_send(Some(&theirs), own, "media", "r1"));
        // It saw an older process of ours (a new epoch): everything.
        assert!(should_send(Some(&theirs), "restarted", "settings", "r1"));
        // No `have` (an old build, or it timed out): everything.
        assert!(should_send(None, own, "settings", "r1"));
    }

    #[test]
    fn revs_are_kept_per_peer_process_and_a_restart_forgets_them() {
        let device = "sync-test-a";
        let mut packet = snapshot("on");
        packet.set("rev", "abc");
        note_received(device, Some("phone-1"), "contacts", &packet);
        note_received(device, Some("phone-1"), "notes", &packet);
        let held = have_packet(device, Some("phone-1"));
        assert_eq!(held.get_str("peerInstance"), Some("phone-1"));
        let entries = parse_have(&held).unwrap();
        assert_eq!(entries.have.len(), 2);
        assert_eq!(entries.have["contacts"], "abc");
        // The phone restarted: nothing held counts, and the new process
        // starts from nothing.
        let after = have_packet(device, Some("phone-2"));
        assert_eq!(after.get_str("peerInstance"), Some("phone-2"));
        assert!(parse_have(&after).unwrap().have.is_empty());
        note_received(device, Some("phone-2"), "notes", &packet);
        let fresh = parse_have(&have_packet(device, Some("phone-2"))).unwrap();
        assert_eq!(fresh.have.len(), 1, "the old process's revs were dropped");
        // Not digested, no rev, no instance: not remembered.
        note_received(device, Some("phone-2"), "battery", &packet);
        note_received(device, Some("phone-2"), "contacts", &snapshot("on"));
        note_received(device, None, "contacts", &packet);
        assert_eq!(
            parse_have(&have_packet(device, Some("phone-2")))
                .unwrap()
                .have
                .len(),
            1
        );
        // A rev too long is refused.
        let mut long = snapshot("on");
        long.set("rev", "x".repeat(MAX_REV_CHARS + 1));
        note_received(device, Some("phone-2"), "settings", &long);
        assert_eq!(
            parse_have(&have_packet(device, Some("phone-2")))
                .unwrap()
                .have
                .len(),
            1
        );
        forget_device(device);
        assert!(parse_have(&have_packet(device, Some("phone-2")))
            .unwrap()
            .have
            .is_empty());
    }

    #[test]
    fn a_have_is_bounded_and_only_known_features_count() {
        let many: Vec<(String, String)> = (0..20)
            .map(|n| (format!("f{n}"), "r".to_string()))
            .collect();
        let refs: Vec<(&str, &str)> = many.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        assert!(
            parse_have(&have("i", &refs)).unwrap().have.is_empty(),
            "unknown features are ignored"
        );
        let ok = parse_have(&have("i", &[("settings", "r"), ("media", "m")])).unwrap();
        assert_eq!(ok.have.len(), 2);
        // An empty `have` is valid: "send me everything".
        assert!(parse_have(&have("i", &[])).unwrap().have.is_empty());
        let long_rev = "x".repeat(MAX_REV_CHARS + 1);
        assert!(parse_have(&have("i", &[("settings", long_rev.as_str())]))
            .unwrap()
            .have
            .is_empty());
        // Not a have at all.
        assert!(parse_have(&NetworkPacket::new(PACKET_TYPE_SYNC_HAVE)).is_none());
        // Small enough for any message: 6 features, 64-char revs.
        let all: Vec<(&str, String)> = FEATURES
            .iter()
            .map(|f| (*f, "x".repeat(MAX_REV_CHARS)))
            .collect();
        let pairs: Vec<(&str, &str)> = all.iter().map(|(a, b)| (*a, b.as_str())).collect();
        assert!(crate::device_links::webrtc::fits_on_wire(&have(
            "i".repeat(64).as_str(),
            &pairs
        )));
        assert!(FEATURES.len() <= MAX_HAVE_ENTRIES);
        assert!(FEATURES
            .iter()
            .all(|feature| feature.len() <= MAX_FEATURE_CHARS));
    }

    #[test]
    fn snapshots_wait_for_the_peers_have_but_never_longer_than_a_second() {
        assert_eq!(SYNC_WAIT, Duration::from_millis(1000));
        let device = "sync-test-b";
        forget_device(device);
        // Their `have` has not come: wait; it comes and releases the send.
        let token = begin_wait(device).expect("waiting");
        assert!(
            on_have(device, &have("desk", &[("settings", "r1")])),
            "released by the have"
        );
        assert!(
            !release_wait(device, token),
            "the timer finds nothing left to release"
        );
        assert_eq!(theirs(device).unwrap().have["settings"], "r1");
        // Their `have` came first (before our ready): no waiting at all.
        assert_eq!(begin_wait(device), None);
        // A new connection: what they said belongs to the old one.
        forget_connection(device);
        assert!(theirs(device).is_none());
        // Nothing came in time: the timer releases everything, once.
        let token = begin_wait(device).expect("waiting again");
        assert!(release_wait(device, token));
        assert!(!release_wait(device, token), "once");
        // A late `have` after that does not release a second time.
        assert!(!on_have(device, &have("desk", &[])));
        // A malformed one changes nothing.
        assert!(!on_have(device, &NetworkPacket::new(PACKET_TYPE_SYNC_HAVE)));
        forget_device(device);
    }
}
