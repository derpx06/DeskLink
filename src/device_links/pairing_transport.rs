//! Which carrier a pairing may travel over, and the one rule that decides it.
//!
//! Shipped pairing runs over the LAN only: a pinned-TLS socket, either the
//! prompt flow (`desklink.pair`) or the QR proof (`desklink.pair.qr`). A relay
//! link is a *bootstrap* link and carries signaling, heartbeats and the
//! `pair:false` unpair notice, never a pairing (docs/RELAY.md section 3).
//!
//! This module is the seam for pairing away from the LAN later
//! (docs/PAIRING-OFF-LAN.md). A new carrier implements
//! [`LinkTransport::pairing_transport`](super::core::LinkTransport::pairing_transport)
//! with its own [`PairingTransportKind`] and adds its arm to [`packet_may_cross`];
//! the pairing state machine (`pairing.rs`) and the trust record
//! (`Config::trust_device`) are not touched. It stays unreachable until
//! [`OFF_LAN_PAIRING_COMPILED`] is turned on in source *and* the hidden
//! `offLanPairing` setting is true: [`off_lan_pairing_enabled`].

use super::packet::{
    NetworkPacket, PACKET_TYPE_HEARTBEAT, PACKET_TYPE_PAIR, PACKET_TYPE_PAIR_QR,
    PACKET_TYPE_WEBRTC_SIGNAL_V1,
};

/// Compile-time switch. `false` in every shipped build: with it off the
/// runtime setting is not even read, so no configuration can open the path.
pub const OFF_LAN_PAIRING_COMPILED: bool = false;

/// The hidden key in `config.json` (kept in `AppSettings::extra`; no UI row).
pub const OFF_LAN_SETTING: &str = "offLanPairing";

/// Where a link's bytes travel, as far as pairing cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairingTransportKind {
    /// A pinned-TLS socket on the local network (and the isolated test wire).
    Lan,
    /// The end-to-end encrypted link through the project's relay.
    Relay,
}

/// Both switches, as a pure function so a test can try every combination.
pub fn flag_open(compiled: bool, requested: bool) -> bool {
    compiled && requested
}

/// Whether pairing may travel off the LAN in this run.
pub fn off_lan_pairing_enabled() -> bool {
    // Short-circuits: a shipped build never reads (or clones) the settings.
    OFF_LAN_PAIRING_COMPILED
        && flag_open(
            true,
            super::config::app_settings()
                .extra
                .get(OFF_LAN_SETTING)
                .and_then(|value| value.as_bool())
                .unwrap_or(false),
        )
}

/// Whether `packet` may cross a link of `kind`. Every pairing packet the
/// state machine reads, and every one it sends, passes this on a relay link.
pub fn packet_may_cross(kind: PairingTransportKind, packet: &NetworkPacket, off_lan: bool) -> bool {
    match kind {
        // The LAN rules (who may send what, before and after pairing) are the
        // dispatcher's; every packet type is carried.
        PairingTransportKind::Lan => true,
        PairingTransportKind::Relay => match packet.packet_type.as_str() {
            PACKET_TYPE_WEBRTC_SIGNAL_V1 | PACKET_TYPE_HEARTBEAT => true,
            PACKET_TYPE_PAIR => {
                off_lan || packet.body.get("pair").and_then(|pair| pair.as_bool()) == Some(false)
            }
            PACKET_TYPE_PAIR_QR => off_lan,
            _ => false,
        },
    }
}

/// [`packet_may_cross`] with this run's flag: the check at the entrance of
/// the pairing state machine, whichever carrier delivered the packet.
pub fn admits(kind: PairingTransportKind, packet: &NetworkPacket) -> bool {
    packet_may_cross(kind, packet, off_lan_pairing_enabled())
}

#[cfg(test)]
mod tests {
    use super::*;
    use PairingTransportKind::{Lan, Relay};

    fn pair(value: Option<bool>, timestamp: bool) -> NetworkPacket {
        let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
        if let Some(value) = value {
            packet.set("pair", value);
        }
        if timestamp {
            packet.set("timestamp", 1_700_000_000i64);
        }
        packet
    }

    /// The relay invariant (CLAUDE.md): with the flag off a relay link carries
    /// signaling, heartbeats and `pair:false`, and nothing of a pairing.
    #[test]
    fn with_the_flag_off_a_relay_carries_no_pairing_beyond_pair_false() {
        for packet in [
            pair(Some(true), false),
            pair(Some(true), true),
            pair(None, true),
            pair(None, false),
            NetworkPacket::new(PACKET_TYPE_PAIR_QR),
            NetworkPacket::new(super::super::packet::PACKET_TYPE_CLIPBOARD),
        ] {
            assert!(
                !packet_may_cross(Relay, &packet, false),
                "{} {:?}",
                packet.packet_type,
                packet.body
            );
        }
        assert!(packet_may_cross(Relay, &pair(Some(false), false), false));
        assert!(packet_may_cross(Relay, &pair(Some(false), true), false));
        assert!(packet_may_cross(
            Relay,
            &NetworkPacket::new(PACKET_TYPE_HEARTBEAT),
            false
        ));
        assert!(packet_may_cross(
            Relay,
            &NetworkPacket::new(PACKET_TYPE_WEBRTC_SIGNAL_V1),
            false
        ));
        // A string is not a boolean.
        let mut text = NetworkPacket::new(PACKET_TYPE_PAIR);
        text.set("pair", "false");
        assert!(!packet_may_cross(Relay, &text, false));
    }

    #[test]
    fn the_flag_opens_only_pairing_and_only_when_both_switches_are_on() {
        assert!(!flag_open(false, true), "compiled out: the setting is moot");
        assert!(!flag_open(true, false), "compiled in, setting off");
        assert!(flag_open(true, true));
        assert!(packet_may_cross(Relay, &pair(Some(true), true), true));
        assert!(packet_may_cross(
            Relay,
            &NetworkPacket::new(PACKET_TYPE_PAIR_QR),
            true
        ));
        // Features never cross a relay, flag or not.
        assert!(!packet_may_cross(
            Relay,
            &NetworkPacket::new(super::super::packet::PACKET_TYPE_CLIPBOARD),
            true
        ));
        assert!(packet_may_cross(Lan, &pair(Some(true), true), false));
    }

    /// The shipped build cannot open the path, whatever `config.json` says.
    #[test]
    fn a_shipped_build_has_the_flag_compiled_out_and_the_setting_changes_nothing() {
        const { assert!(!OFF_LAN_PAIRING_COMPILED) };
        assert!(!off_lan_pairing_enabled());
        assert!(!admits(Relay, &pair(Some(true), true)));
        assert!(admits(Relay, &pair(Some(false), false)));
    }
}
