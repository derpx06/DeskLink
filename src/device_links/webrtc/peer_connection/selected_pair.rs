//! The selected ICE pair (`impl DesktopWebRtcPeer` and its types): the path
//! reading the UI shows, the lane address, and the pair read from the ICE agent
//! or from stats.
//!
//! Sibling edges: `rtp_tuning` (stats). Outside: `lan_scope`.

use super::*;

impl DesktopWebRtcPeer {
    /// How the selected ICE pair reaches the peer, per [`path_label`], its
    /// current round-trip time in milliseconds when known, and whether the
    /// pair is on the local network proper (see [`lan_scope`]).
    /// `via_relay` is whether the bootstrap link is the relay.
    /// `None` until a pair is selected.
    pub fn path(&self, via_relay: bool) -> Option<PathReading> {
        let pair = self.selected_pair()?;
        let types = &pair.types;
        // Until both candidates are resolved the path is unknown, not
        // "direct": guessing showed "over the internet" for a moment on
        // every LAN connect.
        if types.len() != 2 || types.iter().any(String::is_empty) {
            return None;
        }
        let label = path_label(types, via_relay);
        let nearby = label == "lan"
            && match (pair.local_ip, pair.remote_ip) {
                (Some(local), Some(remote)) => crate::device_links::lan_scope::is_nearby(
                    local,
                    remote,
                    &crate::device_links::lan_scope::local_interfaces(),
                ),
                _ => false,
            };
        Some(PathReading {
            label,
            rtt: pair.rtt,
            nearby,
        })
    }

    /// The peer's address on the selected ICE pair, when that pair is the
    /// local network proper (host or prflx on both ends, both addresses
    /// private and on an attached subnet): where a fast lane (W18) may
    /// connect. Never a relay or reflexive pair. Also whether the pair goes
    /// through a relay (the transfer says so).
    pub fn lane_address(&self) -> (Option<std::net::IpAddr>, bool) {
        let Some(pair) = self.selected_pair() else {
            return (None, false);
        };
        let relayed = path_label(&pair.types, false) == "turn";
        (
            lane_address_of(&pair, &crate::device_links::lan_scope::local_interfaces()),
            relayed,
        )
    }

    /// The selected pair's two candidate types as webrtcbin spells them, and
    /// its round-trip time in milliseconds when known.
    pub fn selected_pair_types(&self) -> Option<(Vec<String>, Option<u32>)> {
        self.selected_pair().map(|pair| (pair.types, pair.rtt))
    }

    /// Reads webrtcbin's stats, waiting at most [`STATS_WAIT`]: a peer that is
    /// closing, or an ICE agent that never answers, must not hold the thread
    /// that reads every device's path.
    pub fn selected_pair(&self) -> Option<SelectedPair> {
        if self.closed.load(Ordering::Acquire) {
            return None;
        }
        let from_stats = self.selected_pair_from_stats();
        let complete = |pair: &SelectedPair| {
            pair.types.len() == 2 && pair.types.iter().all(|kind| !kind.is_empty())
        };
        if from_stats.as_ref().is_some_and(complete) {
            return from_stats;
        }
        // GStreamer 1.26's ICE transport leaves the selected pair out of the
        // stats (1.28 fills it in): the path read as unknown, so unlocking
        // with the phone was refused as "not on the LAN" and the fast lane
        // was never used. The ICE agent itself still knows the pair. Only
        // asked before 1.28: there the stats carry it, and 1.28's
        // `selected_pair` aborts the process while no pair is selected yet.
        if !ice_fallback_needed(gst::version()) {
            return from_stats;
        }
        match self.selected_pair_from_ice() {
            Some((types, local_ip, remote_ip)) => Some(SelectedPair {
                types,
                local_ip,
                remote_ip,
                rtt: from_stats.and_then(|pair| pair.rtt),
            }),
            None => from_stats,
        }
    }

    /// The selected pair as the ICE agent reports it (the transport the data
    /// channels ride; with max-bundle every section shares it).
    fn selected_pair_from_ice(
        &self,
    ) -> Option<(
        Vec<String>,
        Option<std::net::IpAddr>,
        Option<std::net::IpAddr>,
    )> {
        use gst_webrtc::prelude::*;
        let object = |owner: &gst::glib::Object, name: &str| {
            owner
                .find_property(name)
                .and_then(|_| owner.property::<Option<gst::glib::Object>>(name))
        };
        let ice = object(self.webrtcbin.upcast_ref(), "ice-agent")?
            .downcast::<gst_webrtc::WebRTCICE>()
            .ok()?;
        let sctp = object(self.webrtcbin.upcast_ref(), "sctp-transport")?;
        let dtls = object(&sctp, "transport")?;
        let transport = object(&dtls, "transport")?;
        let stream = object(&transport, "stream")?
            .downcast::<gst_webrtc::WebRTCICEStream>()
            .ok()?;
        let (local, remote) = ice.selected_pair(&stream)?;
        let ip = |stats: &gst_webrtc::WebRTCICECandidateStats| {
            stats
                .ipaddr()
                .and_then(crate::device_links::lan_scope::parse_candidate_ip)
        };
        Some((
            vec![local.type_()?.to_string(), remote.type_()?.to_string()],
            ip(&local),
            ip(&remote),
        ))
    }

    fn selected_pair_from_stats(&self) -> Option<SelectedPair> {
        let stats = webrtcbin_stats(&self.webrtcbin)?;
        let field = |stats: &gst::Structure, name: &str| stats.get::<String>(name).ok();
        let by_id = |id: &str| {
            stats
                .iter()
                .find(|entry| field(entry, "id").as_deref() == Some(id))
        };
        let pair_id = stats
            .iter()
            .find_map(|entry| field(entry, "selected-candidate-pair-id"))?;
        let pair = by_id(&pair_id)?;
        let candidates: Vec<&gst::Structure> = ["local-candidate-id", "remote-candidate-id"]
            .iter()
            .filter_map(|key| field(pair, key))
            .filter_map(|id| by_id(&id))
            .collect();
        let types: Vec<String> = candidates
            .iter()
            .filter_map(|candidate| field(candidate, "candidate-type"))
            .collect();
        let address = |candidate: Option<&&gst::Structure>| {
            candidate
                .and_then(|candidate| {
                    field(candidate, "address").or_else(|| field(candidate, "ip"))
                })
                .and_then(|text| crate::device_links::lan_scope::parse_candidate_ip(&text))
        };
        let rtt = pair
            .get::<f64>("current-round-trip-time")
            .ok()
            .filter(|seconds| seconds.is_finite() && *seconds > 0.0)
            .map(|seconds| (seconds * 1000.0).round().max(1.0) as u32);
        Some(SelectedPair {
            local_ip: address(candidates.first()),
            remote_ip: address(candidates.get(1)),
            types,
            rtt,
        })
    }
}

/// What the selected ICE pair says about the connection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedPair {
    pub types: Vec<String>,
    pub rtt: Option<u32>,
    pub local_ip: Option<std::net::IpAddr>,
    pub remote_ip: Option<std::net::IpAddr>,
}

/// Where a fast lane (W18) may connect on `pair`: the remote address, only
/// when the pair is host or peer-reflexive on both ends, both addresses are
/// private (not public, loopback or carrier-grade), the remote is on a subnet
/// of one of our own `interfaces`, and it is not a scoped link-local IPv6
/// address (which cannot be dialled without its zone). Anything else is
/// `None`: the lane is never offered.
pub(super) fn lane_address_of(
    pair: &SelectedPair,
    interfaces: &[(std::net::IpAddr, u8)],
) -> Option<std::net::IpAddr> {
    if pair.types.len() != 2 || path_label(&pair.types, false) != "lan" {
        return None;
    }
    let (local, remote) = (pair.local_ip?, pair.remote_ip?);
    let scoped = matches!(remote, std::net::IpAddr::V6(ip) if ip.segments()[0] & 0xffc0 == 0xfe80);
    // A phone behind this computer's NAT (an emulator) is seen at this
    // computer's own address: a lane dialled there reaches only itself.
    let own = interfaces
        .iter()
        .any(|(mine, _)| mine.to_canonical() == remote.to_canonical());
    (!scoped && !own && crate::device_links::lan_scope::is_nearby(local, remote, interfaces))
        .then_some(remote)
}

/// The path of the selected pair: its label (shared with the phone), its
/// round-trip time, and whether it is the local network proper.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathReading {
    pub label: &'static str,
    pub rtt: Option<u32>,
    pub nearby: bool,
}

/// A candidate type in its short spelling: webrtcbin's stats say
/// "peer-reflexive", "server-reflexive" and "relayed", libwebrtc's SDP
/// "prflx", "srflx" and "relay" (contract B5).
fn candidate_kind(raw: &str) -> &str {
    match raw {
        "peer-reflexive" => "prflx",
        "server-reflexive" => "srflx",
        "relayed" => "relay",
        other => other,
    }
}

/// The connection-path rule, identical on both apps. `candidate_types` is
/// the selected pair's `[local, remote]` candidate types:
/// - either is `relay` → `"turn"`;
/// - both are `host`/`prflx` and the bootstrap link is not the relay → `"lan"`;
/// - anything else, including an unknown or incomplete pair → `"direct"`.
///
/// Either spelling of each type counts (see [`candidate_kind`]).
pub(super) fn path_label(candidate_types: &[String], via_relay: bool) -> &'static str {
    let kinds: Vec<&str> = candidate_types
        .iter()
        .map(|kind| candidate_kind(kind))
        .collect();
    if kinds.contains(&"relay") {
        "turn"
    } else if !via_relay
        && kinds.len() == 2
        && kinds.iter().all(|kind| *kind == "host" || *kind == "prflx")
    {
        "lan"
    } else {
        "direct"
    }
}

/// Whether the ICE agent must be asked for the selected pair: only GStreamer
/// before 1.28 leaves it out of the stats.
pub(super) fn ice_fallback_needed((major, minor, _, _): (u32, u32, u32, u32)) -> bool {
    (major, minor) < (1, 28)
}
