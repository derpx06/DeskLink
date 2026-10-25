//! ICE server configuration for the desktop peer.
//!
//! Empty by default, and deliberately so: operation on a local network must
//! never require a server. STUN and TURN matter only once the two devices are
//! not on the same network, which is a later, opt-in capability.
//!
//! The change that actually makes local ICE reliable is not a server at all —
//! see [`IceServerConfig::local_ips`].

use std::net::IpAddr;

use gst::prelude::*;

/// Most entries accepted from configuration.
///
/// A bound rather than a policy: the list is applied to a real ICE agent, and
/// each entry costs candidate-gathering time.
const MAX_SERVERS: usize = 8;

/// The `webrtcbin` property holding the `GstWebRTCICE` agent.
const ICE_AGENT_PROPERTY: &str = "ice-agent";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct IceServerConfig {
    pub stun_servers: Vec<String>,
    pub turn_servers: Vec<String>,
    /// The local address known to reach this peer, when there is one.
    ///
    /// `GstWebRTCICE` gathers on *every* local address when none is added. On a
    /// developer machine that means `docker0`, `virbr0`, `tun0` and every stale
    /// bridge — dozens of candidate pairs, mostly black holes, worked through
    /// serially. The bootstrap TLS socket already knows the answer: its local
    /// address has a working route to this peer by construction, so restricting
    /// gathering to it collapses the search to one candidate and one check.
    ///
    /// Off the LAN (a relay-bootstrapped peer) these are instead the
    /// default-route addresses, IPv6 and IPv4: the paths the internet will
    /// actually use, still excluding the virtual bridges. Empty means gather
    /// everywhere.
    pub local_ips: Vec<IpAddr>,
    /// Debug only: relay-only ICE (`DESKLINK_ICE_RELAY_ONLY=1`), to exercise
    /// TURN on one network.
    pub relay_only: bool,
}

/// libnice asks the router for a port mapping (UPnP IGD) by default: an SSDP
/// socket on every interface and a `gupnp-igd` thread per ICE agent, for a
/// LAN link that needs neither. Off, before gathering starts.
fn disable_upnp(webrtcbin: &gst::Element) {
    if !webrtcbin.has_property(ICE_AGENT_PROPERTY, None) {
        return;
    }
    let ice = webrtcbin.property::<gst_webrtc::WebRTCICE>(ICE_AGENT_PROPERTY);
    if !ice.has_property("agent", None) {
        return;
    }
    let agent = ice.property::<gst::glib::Object>("agent");
    if agent.has_property("upnp", None) {
        agent.set_property("upnp", false);
    }
}

/// Whether the agent of `webrtcbin` has UPnP on (tests); `None` when the
/// agent does not expose it.
#[cfg(test)]
fn upnp_enabled(webrtcbin: &gst::Element) -> Option<bool> {
    let ice = webrtcbin.property::<gst_webrtc::WebRTCICE>(ICE_AGENT_PROPERTY);
    ice.has_property("agent", None)
        .then(|| ice.property::<gst::glib::Object>("agent"))
        .filter(|agent| agent.has_property("upnp", None))
        .map(|agent| agent.property::<bool>("upnp"))
}

impl IceServerConfig {
    /// Reads comma- or newline-separated URI lists.
    ///
    /// The grammar matches Android's `parseServerList` so one support answer
    /// applies to both applications.
    pub fn parse(stun: &str, turn: &str) -> Result<Self, String> {
        fn split(value: &str) -> Vec<String> {
            value
                .split([',', '\n'])
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(ToOwned::to_owned)
                .collect()
        }
        let config = Self {
            stun_servers: split(stun),
            turn_servers: split(turn),
            ..Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.stun_servers.len() > MAX_SERVERS || self.turn_servers.len() > MAX_SERVERS {
            return Err(format!(
                "At most {MAX_SERVERS} STUN and {MAX_SERVERS} TURN servers may be configured"
            ));
        }
        for uri in &self.stun_servers {
            if !(uri.starts_with("stun://") || uri.starts_with("stuns://")) {
                return Err(format!("Unsupported STUN server URI: {uri}"));
            }
        }
        for uri in &self.turn_servers {
            if !(uri.starts_with("turn://") || uri.starts_with("turns://")) {
                return Err(format!("Unsupported TURN server URI: {uri}"));
            }
        }
        Ok(())
    }

    /// Applies this configuration to a `webrtcbin`.
    ///
    /// Best effort throughout: a rejected local address or TURN server leaves
    /// the element gathering the way it would have anyway, which is worse but
    /// still works. Failing the whole connection over ICE tuning would be a
    /// poor trade.
    pub fn apply_to(&self, webrtcbin: &gst::Element) {
        // webrtcbin accepts exactly one `stun-server` property but any number of
        // TURN servers through a signal. That asymmetry is the element's, not
        // ours.
        if let Some(stun) = self.stun_servers.first() {
            webrtcbin.set_property("stun-server", stun);
            if self.stun_servers.len() > 1 {
                debug_log!(
                    "[DeskLink] webrtcbin accepts one STUN server; ignoring {} more",
                    self.stun_servers.len() - 1
                );
            }
        }

        for turn in &self.turn_servers {
            let accepted = webrtcbin.emit_by_name::<bool>("add-turn-server", &[turn]);
            if !accepted {
                eprintln!("[DeskLink] webrtcbin rejected a TURN server URI");
            }
        }

        if self.relay_only {
            webrtcbin.set_property_from_str("ice-transport-policy", "relay");
        }
        disable_upnp(webrtcbin);
        for local_ip in &self.local_ips {
            // `ice-agent`, not `ice`. The agent has never been exposed under
            // the latter name, and `property()` *panics* on a name webrtcbin
            // does not have — which took down the peer's event worker on every
            // negotiation, so the handover never finished and the coordinator
            // sat in an endless RestartRequest loop. `try_property` keeps this
            // as best-effort as the rest of the function already claims to be.
            if webrtcbin.has_property(ICE_AGENT_PROPERTY, None) {
                let ice = webrtcbin.property::<gst_webrtc::WebRTCICE>(ICE_AGENT_PROPERTY);
                let accepted =
                    ice.emit_by_name::<bool>("add-local-ip-address", &[&local_ip.to_string()]);
                if !accepted {
                    eprintln!(
                        "[DeskLink] Could not restrict ICE to {local_ip}; gathering on all interfaces"
                    );
                }
            } else {
                debug_log!(
                    "[DeskLink] webrtcbin exposes no `{ICE_AGENT_PROPERTY}`; gathering on all interfaces"
                );
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_comma_and_newline_separated_lists() {
        let config =
            IceServerConfig::parse("stun://a:3478, stun://b:3478", "turn://c:3478").unwrap();
        assert_eq!(config.stun_servers.len(), 2);
        assert_eq!(config.turn_servers, vec!["turn://c:3478".to_string()]);
    }

    #[test]
    fn an_empty_configuration_is_valid_because_lan_needs_no_server() {
        let config = IceServerConfig::parse("", "").unwrap();
        assert!(config.stun_servers.is_empty());
        assert!(config.turn_servers.is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn rejects_unsupported_schemes() {
        assert!(IceServerConfig::parse("http://nope", "").is_err());
        assert!(IceServerConfig::parse("", "stun://wrong-list").is_err());
    }

    /// The name is only ever validated by GStreamer at runtime, and getting it
    /// wrong panics the peer's event worker rather than degrading. Pin it
    /// against a real `webrtcbin`.
    #[test]
    fn the_ice_agent_property_exists_on_a_real_webrtcbin() {
        let Ok(()) = gst::init() else {
            return;
        };
        let Ok(webrtcbin) = gst::ElementFactory::make("webrtcbin").build() else {
            return; // gst-plugins-bad absent; nothing to pin against.
        };
        assert!(
            webrtcbin.has_property(ICE_AGENT_PROPERTY, None),
            "webrtcbin must expose the ICE agent as `{ICE_AGENT_PROPERTY}`"
        );

        // And the restriction path itself must not panic.
        let config = IceServerConfig {
            local_ips: vec![std::net::IpAddr::from([127, 0, 0, 1])],
            ..IceServerConfig::default()
        };
        config.apply_to(&webrtcbin);
    }

    /// A LAN link never needs UPnP: every ICE agent has it off, so no SSDP
    /// socket or `gupnp-igd` thread is made per peer.
    #[test]
    fn the_ice_agent_does_not_use_upnp() {
        let Ok(()) = gst::init() else {
            return;
        };
        let Ok(webrtcbin) = gst::ElementFactory::make("webrtcbin").build() else {
            return;
        };
        IceServerConfig::default().apply_to(&webrtcbin);
        match upnp_enabled(&webrtcbin) {
            Some(on) => assert!(!on, "UPnP must be off"),
            None => eprintln!("[test] this libnice has no `upnp` property"),
        }
    }

    #[test]
    fn rejects_more_than_eight_entries() {
        let many = (0..9)
            .map(|index| format!("stun://host{index}:3478"))
            .collect::<Vec<_>>()
            .join(",");
        assert!(IceServerConfig::parse(&many, "").is_err());
    }
}
