use crate::device_links::packet::{
    PACKET_TYPE_BATTERY, PACKET_TYPE_CLIPBOARD, PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST, PACKET_TYPE_LOCK, PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST, PACKET_TYPE_NOTIFICATION, PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY, PACKET_TYPE_NOTIFICATION_REQUEST, PACKET_TYPE_PING,
    PACKET_TYPE_SCREEN_ERROR, PACKET_TYPE_SCREEN_READY, PACKET_TYPE_SCREEN_REQUEST,
    PACKET_TYPE_SCREEN_STOP, PACKET_TYPE_SFTP, PACKET_TYPE_SFTP_REQUEST, PACKET_TYPE_SHARE_REQUEST,
};

/// Packets the desktop can act on when they arrive over the authenticated
/// WebRTC transport. Each entry has a matching arm in
/// `packet_handler::dispatch_webrtc_feature_packet` and a DeskLink Android
/// plugin that actually sends it.
const DESKTOP_INCOMING: [&str; 13] = [
    PACKET_TYPE_BATTERY,
    PACKET_TYPE_CLIPBOARD,
    PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    // The phone's Lock button. Handled identically on both transports by
    // `packet_handler::apply_lock_request`; the Android lock plugin is enabled
    // by our outgoing advertisement, so without this its request was rejected
    // and the button silently did nothing.
    PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_PING,
    // Screen sharing. `desklink.screen.frame` is deliberately absent on both
    // sides: video is a WebRTC media track, and the frame type is a vestige of
    // an earlier data-channel design that neither application implements.
    PACKET_TYPE_SCREEN_ERROR,
    PACKET_TYPE_SCREEN_READY,
    PACKET_TYPE_SCREEN_REQUEST,
    PACKET_TYPE_SCREEN_STOP,
    PACKET_TYPE_SFTP,
    PACKET_TYPE_SHARE_REQUEST,
];

/// Packets the desktop can construct from a command path, and that a DeskLink
/// Android plugin can consume. `desklink.mpris.request`,
/// `desklink.systemvolume.request` and `desklink.runcommand.request` are
/// deliberately absent: the Android application implements those plugins in the
/// opposite direction (phone controls desktop) and has no receiver for them, so
/// advertising them would only produce rejected packets.
const DESKTOP_OUTGOING: [&str; 16] = [
    PACKET_TYPE_CLIPBOARD,
    PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    // The lock-state confirmation sent back to the phone. Outgoing only.
    //
    // There is no status-only handler for it: both handler arms match
    // `PACKET_TYPE_LOCK | PACKET_TYPE_LOCK_REQUEST` and route straight into
    // `apply_lock_request`, so admitting it here would feed the phone's status
    // report to the code that performs a lock. `apply_lock_request` now reads
    // `setLocked` only, which makes that inert — but the missing handler is the
    // reason this stays outgoing, and displaying the phone's lock state means
    // writing that handler first.
    PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY,
    PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PING,
    PACKET_TYPE_SCREEN_ERROR,
    PACKET_TYPE_SCREEN_READY,
    PACKET_TYPE_SCREEN_REQUEST,
    PACKET_TYPE_SCREEN_STOP,
    PACKET_TYPE_SFTP_REQUEST,
    PACKET_TYPE_SHARE_REQUEST,
];

/// Packet types carried over the authenticated WebRTC feature bridge.
///
/// **This is not the same set as the advertised capabilities, and it must not
/// become one.** It has to stay byte-identical to `WebRtcFeatureProfile.enabled`
/// in the Android application, because the handover compares each peer's
/// filtered capability list for exact equality — one extra entry fails the
/// handshake outright and takes every feature down with it.
///
/// `desklink.screen.*` is advertised but deliberately absent here: the screen
/// control plane is `desklink.remote-session.v1` on the control channel, and
/// the screen packet types exist only so the Android application loads its
/// screen plugin (see `DESKTOP_INCOMING`). They never cross the wire.
const WEBRTC_PROFILE: [&str; 15] = [
    PACKET_TYPE_BATTERY,
    PACKET_TYPE_CLIPBOARD,
    PACKET_TYPE_CLIPBOARD_CONNECT,
    PACKET_TYPE_FINDMYPHONE_REQUEST,
    PACKET_TYPE_LOCK,
    PACKET_TYPE_LOCK_REQUEST,
    PACKET_TYPE_MOUSEPAD_REQUEST,
    PACKET_TYPE_NOTIFICATION,
    PACKET_TYPE_NOTIFICATION_ACTION,
    PACKET_TYPE_NOTIFICATION_REPLY,
    PACKET_TYPE_NOTIFICATION_REQUEST,
    PACKET_TYPE_PING,
    PACKET_TYPE_SFTP,
    PACKET_TYPE_SFTP_REQUEST,
    PACKET_TYPE_SHARE_REQUEST,
];

pub(crate) fn is_webrtc_feature(packet_type: &str) -> bool {
    WEBRTC_PROFILE.contains(&packet_type)
}

/// Returns the enabled WebRTC subset of an authenticated identity capability
/// list.  The identity may describe future or legacy features, but the
/// handover must activate only the profile that both applications implement.
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
    use super::{is_webrtc_feature, webrtc_capabilities, FeatureRegistry};

    #[test]
    fn feature_registry_does_not_advertise_unavailable_handlers() {
        let registry = FeatureRegistry::desktop();

        // The desktop consumes clipboard and battery, but has no receiver for
        // remote input: mousepad is an outgoing-only capability here.
        assert!(registry
            .incoming_capabilities()
            .contains(&"desklink.clipboard".to_string()));
        assert!(!registry
            .incoming_capabilities()
            .contains(&"desklink.mousepad.request".to_string()));
        assert!(registry
            .outgoing_capabilities()
            .contains(&"desklink.mousepad.request".to_string()));
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
            vec![
                "desklink.battery".to_string(),
                "desklink.clipboard".to_string(),
                "desklink.clipboard.connect".to_string(),
                "desklink.findmyphone.request".to_string(),
                "desklink.lock.request".to_string(),
                "desklink.notification".to_string(),
                "desklink.ping".to_string(),
                "desklink.screen.error".to_string(),
                "desklink.screen.ready".to_string(),
                "desklink.screen.request".to_string(),
                "desklink.screen.stop".to_string(),
                "desklink.sftp".to_string(),
                "desklink.share.request".to_string(),
            ]
        );
        assert_eq!(
            registry.outgoing_capabilities(),
            vec![
                "desklink.clipboard".to_string(),
                "desklink.clipboard.connect".to_string(),
                "desklink.findmyphone.request".to_string(),
                "desklink.lock".to_string(),
                "desklink.lock.request".to_string(),
                "desklink.mousepad.request".to_string(),
                "desklink.notification.action".to_string(),
                "desklink.notification.reply".to_string(),
                "desklink.notification.request".to_string(),
                "desklink.ping".to_string(),
                "desklink.screen.error".to_string(),
                "desklink.screen.ready".to_string(),
                "desklink.screen.request".to_string(),
                "desklink.screen.stop".to_string(),
                "desklink.sftp.request".to_string(),
                "desklink.share.request".to_string(),
            ]
        );
    }

    /// The Android application filters its own capabilities with the identical
    /// set, and the handover compares the two lists for exact equality. This
    /// pins the wire contract so a one-sided edit fails here instead of
    /// silently breaking every paired feature at runtime.
    ///
    /// Note this is `WEBRTC_PROFILE`, **not** the advertised capabilities. The
    /// two were the same list until screen sharing needed capabilities that are
    /// advertised (so the Android screen plugin loads) but never carried on the
    /// feature bridge (their control plane is `desklink.remote-session.v1`).
    #[test]
    fn profile_matches_the_android_webrtc_feature_profile() {
        let mut profile: Vec<&str> = super::WEBRTC_PROFILE.to_vec();
        profile.sort_unstable();
        profile.dedup();

        assert_eq!(
            profile,
            vec![
                "desklink.battery",
                "desklink.clipboard",
                "desklink.clipboard.connect",
                "desklink.findmyphone.request",
                "desklink.lock",
                "desklink.lock.request",
                "desklink.mousepad.request",
                "desklink.notification",
                "desklink.notification.action",
                "desklink.notification.reply",
                "desklink.notification.request",
                "desklink.ping",
                "desklink.sftp",
                "desklink.sftp.request",
                "desklink.share.request",
            ]
        );
    }

    /// Advertised capabilities are a superset of the feature profile. The screen
    /// types are the difference: the Android application only loads its screen
    /// plugin when the peer advertises them, but they are never sent — so they
    /// must stay out of the profile, which has to match Android's byte for byte.
    #[test]
    fn screen_capabilities_are_advertised_but_never_bridged() {
        for screen in [
            "desklink.screen.request",
            "desklink.screen.ready",
            "desklink.screen.stop",
            "desklink.screen.error",
        ] {
            assert!(
                super::DESKTOP_INCOMING.contains(&screen)
                    && super::DESKTOP_OUTGOING.contains(&screen),
                "{screen} must be advertised so the Android screen plugin loads"
            );
            assert!(
                !is_webrtc_feature(screen),
                "{screen} must not ride the feature bridge"
            );
        }
    }

    #[test]
    fn capability_filtering_drops_types_outside_the_profile() {
        assert!(is_webrtc_feature("desklink.clipboard"));
        assert!(!is_webrtc_feature("desklink.mpris"));
        assert_eq!(
            webrtc_capabilities(&[
                "desklink.mpris".to_string(),
                "desklink.ping".to_string(),
                "desklink.battery".to_string(),
                "desklink.ping".to_string(),
            ]),
            vec!["desklink.battery".to_string(), "desklink.ping".to_string()]
        );
    }
}
