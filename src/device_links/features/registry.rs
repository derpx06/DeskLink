use std::collections::HashMap;
use std::sync::Mutex;

use crate::device_links::packet::{
    PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_DIGITIZER, PACKET_TYPE_DIGITIZER_SESSION, PACKET_TYPE_FINDMYPHONE_REQUEST,
    PACKET_TYPE_HANDOFF, PACKET_TYPE_LOCK, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE, PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST, PACKET_TYPE_NOTIFICATION, PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY, PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PING,
    PACKET_TYPE_RUNCOMMAND, PACKET_TYPE_RUNCOMMAND_REQUEST, PACKET_TYPE_SCREEN_ERROR,
    PACKET_TYPE_SCREEN_READY, PACKET_TYPE_SCREEN_REQUEST, PACKET_TYPE_SCREEN_STOP,
    PACKET_TYPE_SETTINGS, PACKET_TYPE_SETTINGS_REQUEST, PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_TELEPHONY, PACKET_TYPE_TELEPHONY_REQUEST, PACKET_TYPE_UNLOCK_REQUEST,
};

/// Packets the desktop can act on when they arrive over the authenticated
/// WebRTC transport. Each entry has a matching arm in
/// `packet_handler::dispatch_webrtc_feature_packet` and a DeskLink Android
/// plugin that actually sends it.
const DESKTOP_INCOMING: [&str; 23] = [
    PACKET_TYPE_BATTERY,
    PACKET_TYPE_CLIPBOARD,
    PACKET_TYPE_CLIPBOARD_CONNECT,
    // The drawing pad: strokes become pointer drags, under the control lease.
    PACKET_TYPE_DIGITIZER,
    PACKET_TYPE_DIGITIZER_SESSION,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    // The phone's Lock button, and its approval of an unlock (with the desk's
    // nonce; see `unlock`). Handled by `packet_handler::apply_lock_request`.
    PACKET_TYPE_LOCK_REQUEST,
    // Android's mousepad plugin is loaded only when the peer advertises this
    // receive capability. The WebRTC dispatcher validates a host-approved
    // portal lease before any input can reach the desktop.
    PACKET_TYPE_MOUSEPAD_REQUEST,
    // "Type on phone" (C2): whether a text field has focus there.
    PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE,
    // The phone's music (state), and the phone controlling the desk's players.
    PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST,
    PACKET_TYPE_NOTIFICATION,
    // The phone dismissing one of this computer's notifications (C1).
    PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PING,
    // The phone asking for the desk's command list, or running one.
    PACKET_TYPE_RUNCOMMAND_REQUEST,
    // Screen sharing. `desklink.screen.frame` is deliberately absent on both
    // sides: video is a WebRTC media track, and the frame type is a vestige of
    // an earlier data-channel design that neither application implements.
    PACKET_TYPE_SCREEN_ERROR,
    PACKET_TYPE_SCREEN_READY,
    PACKET_TYPE_SCREEN_REQUEST,
    PACKET_TYPE_SCREEN_STOP,
    // A phone changing an allow-listed desk setting.
    PACKET_TYPE_SETTINGS_REQUEST,
    PACKET_TYPE_SHARE_REQUEST,
    // The phone's calls: the incoming-call bar, Answer/Decline, the timer.
    PACKET_TYPE_TELEPHONY,
    // The phone asking for an unlock nonce ("Unlock desk").
    PACKET_TYPE_UNLOCK_REQUEST,
];

/// Packets the desktop can construct from a command path, and that a DeskLink
/// Android plugin can consume. `desklink.systemvolume.request` is absent: the
/// phone has no receiver for it.
const DESKTOP_OUTGOING: [&str; 23] = [
    PACKET_TYPE_CLIPBOARD,
    PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    // Lock & Hand Off: the terminal to continue on the phone.
    PACKET_TYPE_HANDOFF,
    // The lock-state confirmation sent back to the phone, and an unlock
    // refusal. Outgoing only: a phone's lock *state* is never an instruction.
    PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST,
    // The desk's players (state), and the desk controlling the phone's.
    PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST,
    // This computer's notifications on the phone (C1).
    PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY,
    PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PING,
    // The desk's command list.
    PACKET_TYPE_RUNCOMMAND,
    PACKET_TYPE_SCREEN_ERROR,
    PACKET_TYPE_SCREEN_READY,
    PACKET_TYPE_SCREEN_REQUEST,
    PACKET_TYPE_SCREEN_STOP,
    // The desk's settings, on feature-ready and on every change.
    PACKET_TYPE_SETTINGS,
    PACKET_TYPE_SHARE_REQUEST,
    // Answer, decline, end, silence, mute and dial a call on the phone.
    PACKET_TYPE_TELEPHONY_REQUEST,
    // The desk's unlock nonce.
    PACKET_TYPE_UNLOCK_REQUEST,
];

/// Packet types this desktop carries over the authenticated WebRTC feature
/// bridge.
///
/// The Android application has its own list (`WebRtcFeatureProfile.enabled`).
/// They no longer have to be identical: after the handover the types used with
/// a peer are the **intersection** of the two (see [`record_peer_profile`]),
/// so updating one app before the other loses only the features the older one
/// lacks, never ping and the rest. Both lists are still pinned by tests.
///
/// `desklink.screen.*` is advertised but deliberately absent here: the screen
/// control plane is `desklink.remote-session.v1` on the control channel, and
/// the screen packet types exist only so the Android application loads its
/// screen plugin (see `DESKTOP_INCOMING`). They never cross the wire.
const WEBRTC_PROFILE: [&str; 26] = [
    PACKET_TYPE_BATTERY,
    PACKET_TYPE_CLIPBOARD,
    PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_DIGITIZER,
    PACKET_TYPE_DIGITIZER_SESSION,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    PACKET_TYPE_HANDOFF,
    PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_KEYBOARDSTATE,
    PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_MPRIS,
    PACKET_TYPE_MPRIS_REQUEST,
    PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY,
    PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PING,
    PACKET_TYPE_RUNCOMMAND,
    PACKET_TYPE_RUNCOMMAND_REQUEST,
    PACKET_TYPE_SETTINGS,
    PACKET_TYPE_SETTINGS_REQUEST,
    PACKET_TYPE_SHARE_REQUEST,
    PACKET_TYPE_TELEPHONY,
    PACKET_TYPE_TELEPHONY_REQUEST,
    PACKET_TYPE_UNLOCK_REQUEST,
];

/// What a peer said in its handover it carries, filtered by our profile: the
/// types it accepts (`incoming`) and sends (`outgoing`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct PeerProfile {
    incoming: Vec<String>,
    outgoing: Vec<String>,
}

fn peer_profiles() -> &'static Mutex<HashMap<String, PeerProfile>> {
    static PROFILES: std::sync::OnceLock<Mutex<HashMap<String, PeerProfile>>> =
        std::sync::OnceLock::new();
    PROFILES.get_or_init(Default::default)
}

/// Records what `device_id` confirmed in its handover. From then on only the
/// intersection of its lists and ours is sent to it or accepted from it.
pub(crate) fn record_peer_profile(device_id: &str, incoming: &[String], outgoing: &[String]) {
    if let Ok(mut profiles) = peer_profiles().lock() {
        profiles.insert(
            device_id.to_string(),
            PeerProfile {
                incoming: webrtc_capabilities(incoming),
                outgoing: webrtc_capabilities(outgoing),
            },
        );
    }
}

/// Whether `device_id` takes `packet_type`: in our profile, and in what it
/// said it receives. A peer with no handover yet answers by our profile alone
/// (nothing is sent to it before the handover anyway).
pub(crate) fn peer_accepts(device_id: &str, packet_type: &str) -> bool {
    is_webrtc_feature(packet_type)
        && peer_profiles().lock().is_ok_and(|profiles| {
            profiles
                .get(device_id)
                .is_none_or(|peer| peer.incoming.iter().any(|known| known == packet_type))
        })
}

/// Whether `packet_type` from `device_id` is accepted: in our profile, and in
/// what it said it sends.
pub(crate) fn peer_sends(device_id: &str, packet_type: &str) -> bool {
    is_webrtc_feature(packet_type)
        && peer_profiles().lock().is_ok_and(|profiles| {
            profiles
                .get(device_id)
                .is_none_or(|peer| peer.outgoing.iter().any(|known| known == packet_type))
        })
}

/// Whether a peer's handover list may stand for its identity: once both are
/// filtered by our profile, the handover may claim nothing the authenticated
/// identity did not. It may claim less (an older or newer build).
pub(crate) fn handover_list_is_consistent(handover: &[String], identity: &[String]) -> bool {
    let identity = webrtc_capabilities(identity);
    webrtc_capabilities(handover)
        .iter()
        .all(|capability| identity.contains(capability))
}

pub(crate) fn is_webrtc_feature(packet_type: &str) -> bool {
    WEBRTC_PROFILE.contains(&packet_type)
}

/// Returns the enabled WebRTC subset of an authenticated identity capability
/// list, sorted. The identity may describe future or legacy features; only
/// our profile is ever activated, and with a peer only the intersection.
pub(crate) fn webrtc_capabilities(capabilities: &[String]) -> Vec<String> {
    let mut result: Vec<_> = capabilities
        .iter()
        .filter(|capability| is_webrtc_feature(capability))
        .cloned()
        .collect();
    result.sort();
    result.dedup();
    result
}

/// The capability contract for the desktop feature layer.  A packet is listed
/// only when the current desktop process has a handler that can consume it or
/// a command path that can construct it.  Portal and host-backend dependent
/// features are deliberately absent until their adapters are installed.
#[derive(Debug, Clone)]
pub struct FeatureRegistry {
    incoming: Vec<String>,
    outgoing: Vec<String>,
}

impl FeatureRegistry {
    /// The process-wide desktop capability contract.
    ///
    /// Built once. It is consulted on every feature send, including the
    /// remote-input path that runs at pointer-refresh rate, so rebuilding and
    /// re-sorting two `Vec<String>`s per call was pure waste.
    pub fn desktop() -> &'static Self {
        static DESKTOP: std::sync::OnceLock<FeatureRegistry> = std::sync::OnceLock::new();
        DESKTOP.get_or_init(|| Self {
            incoming: capabilities(&DESKTOP_INCOMING),
            outgoing: capabilities(&DESKTOP_OUTGOING),
        })
    }

    pub fn incoming_capabilities(&self) -> &[String] {
        &self.incoming
    }

    pub fn outgoing_capabilities(&self) -> &[String] {
        &self.outgoing
    }
}

fn capabilities(values: &[&str]) -> Vec<String> {
    let mut result: Vec<_> = values.iter().map(|value| (*value).to_string()).collect();
    result.sort();
    result.dedup();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    #[test]
    fn feature_registry_advertises_each_capability_once() {
        let registry = FeatureRegistry::desktop();
        for list in [
            registry.incoming_capabilities(),
            registry.outgoing_capabilities(),
        ] {
            let mut sorted = list.to_vec();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted, list);
        }
    }

    #[test]
    fn stable_webrtc_profile_advertises_only_implemented_directions() {
        let registry = FeatureRegistry::desktop();
        assert_eq!(
            registry.incoming_capabilities(),
            strings(&[
                "desklink.battery",
                "desklink.clipboard",
                "desklink.clipboard.connect",
                "desklink.digitizer",
                "desklink.digitizer.session",
                "desklink.findmyphone.request",
                "desklink.lock.request",
                "desklink.mousepad.keyboardstate",
                "desklink.mousepad.request",
                "desklink.mpris",
                "desklink.mpris.request",
                "desklink.notification",
                "desklink.notification.request",
                "desklink.ping",
                "desklink.runcommand.request",
                "desklink.screen.error",
                "desklink.screen.ready",
                "desklink.screen.request",
                "desklink.screen.stop",
                "desklink.settings.request",
                "desklink.share.request",
                "desklink.telephony",
                "desklink.unlock.request",
            ])
        );
        assert_eq!(
            registry.outgoing_capabilities(),
            strings(&[
                "desklink.clipboard",
                "desklink.clipboard.connect",
                "desklink.findmyphone.request",
                "desklink.handoff",
                "desklink.lock",
                "desklink.lock.request",
                "desklink.mousepad.request",
                "desklink.mpris",
                "desklink.mpris.request",
                "desklink.notification",
                "desklink.notification.action",
                "desklink.notification.reply",
                "desklink.notification.request",
                "desklink.ping",
                "desklink.runcommand",
                "desklink.screen.error",
                "desklink.screen.ready",
                "desklink.screen.request",
                "desklink.screen.stop",
                "desklink.settings",
                "desklink.share.request",
                "desklink.telephony.request",
                "desklink.unlock.request",
            ])
        );
    }

    /// Pinned so a one-sided edit is noticed; Android pins the same list in
    /// `profileMatchesTheDesktopFeatureRegistry`. A mismatch no longer breaks
    /// the handover (the intersection is used), but it does lose features.
    #[test]
    fn profile_matches_the_android_webrtc_feature_profile() {
        let mut profile: Vec<&str> = WEBRTC_PROFILE.to_vec();
        profile.sort_unstable();
        profile.dedup();
        assert_eq!(
            profile,
            vec![
                "desklink.battery",
                "desklink.clipboard",
                "desklink.clipboard.connect",
                "desklink.digitizer",
                "desklink.digitizer.session",
                "desklink.findmyphone.request",
                "desklink.handoff",
                "desklink.lock",
                "desklink.lock.request",
                "desklink.mousepad.keyboardstate",
                "desklink.mousepad.request",
                "desklink.mpris",
                "desklink.mpris.request",
                "desklink.notification",
                "desklink.notification.action",
                "desklink.notification.reply",
                "desklink.notification.request",
                "desklink.ping",
                "desklink.runcommand",
                "desklink.runcommand.request",
                "desklink.settings",
                "desklink.settings.request",
                "desklink.share.request",
                "desklink.telephony",
                "desklink.telephony.request",
                "desklink.unlock.request",
            ]
        );
        assert!(!profile.contains(&"desklink.sftp"), "SFTP is gone");
    }

    /// Every type each registry lists, except the screen types, rides the
    /// bridge; nothing is in the profile that no direction implements.
    #[test]
    fn the_profile_is_what_the_registries_implement() {
        let registry = FeatureRegistry::desktop();
        for capability in WEBRTC_PROFILE {
            assert!(
                registry
                    .incoming_capabilities()
                    .iter()
                    .any(|c| c == capability)
                    || registry
                        .outgoing_capabilities()
                        .iter()
                        .any(|c| c == capability),
                "{capability} is in the profile but not implemented"
            );
        }
    }

    #[test]
    fn screen_capabilities_are_advertised_but_never_bridged() {
        for screen in [
            "desklink.screen.request",
            "desklink.screen.ready",
            "desklink.screen.stop",
            "desklink.screen.error",
        ] {
            assert!(DESKTOP_INCOMING.contains(&screen) && DESKTOP_OUTGOING.contains(&screen));
            assert!(!is_webrtc_feature(screen));
        }
    }

    #[test]
    fn capability_filtering_drops_types_outside_the_profile() {
        assert!(is_webrtc_feature("desklink.mpris"));
        assert!(!is_webrtc_feature("desklink.sftp"));
        assert_eq!(
            webrtc_capabilities(&strings(&[
                "desklink.sftp",
                "desklink.ping",
                "desklink.battery",
                "desklink.ping",
            ])),
            strings(&["desklink.battery", "desklink.ping"])
        );
    }

    /// An older phone (a subset) still gets everything both have; a type only
    /// one side has is neither sent nor accepted.
    #[test]
    fn a_peer_with_a_subset_still_gets_the_common_features() {
        let device = "intersection-peer";
        record_peer_profile(
            device,
            &strings(&["desklink.ping", "desklink.clipboard", "desklink.sftp"]),
            &strings(&["desklink.ping", "desklink.battery"]),
        );
        assert!(peer_accepts(device, "desklink.ping"));
        assert!(peer_accepts(device, "desklink.clipboard"));
        assert!(
            !peer_accepts(device, "desklink.mpris"),
            "the old phone lacks it"
        );
        assert!(!peer_accepts(device, "desklink.sftp"), "we lack it");
        assert!(peer_sends(device, "desklink.battery"));
        assert!(!peer_sends(device, "desklink.mpris.request"));
        // A device with no handover yet is judged by our profile.
        assert!(peer_accepts("never-handed-over", "desklink.ping"));
        assert!(!peer_accepts("never-handed-over", "desklink.sftp"));
    }

    #[test]
    fn a_handover_may_claim_less_than_the_identity_never_more() {
        let identity = strings(&[
            "desklink.ping",
            "desklink.battery",
            "desklink.screen.request",
        ]);
        assert!(handover_list_is_consistent(
            &strings(&["desklink.ping"]),
            &identity
        ));
        // Types outside our profile do not count either way.
        assert!(handover_list_is_consistent(
            &strings(&["desklink.ping", "desklink.future.thing"]),
            &identity
        ));
        assert!(!handover_list_is_consistent(
            &strings(&["desklink.ping", "desklink.mpris"]),
            &identity
        ));
    }
}
