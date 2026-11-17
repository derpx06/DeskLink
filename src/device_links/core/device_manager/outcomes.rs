//! What the session registry hands back: transport snapshots, install and
//! registration results, errors, reconnect leases, disconnect results, the
//! liveness tick, connection status and adoption decisions. Data only.
//!
//! Sibling edges: none.

use super::*;

/// Immutable transport data copied from the authoritative session while its
/// lock is held.  Callers must perform network I/O only after obtaining this
/// snapshot: GStreamer and TLS callbacks may synchronously trigger lifecycle
/// work, so holding the session mutex across a send can deadlock recovery.
#[derive(Clone)]
pub struct FeatureTransportSnapshot {
    pub binding: SessionBinding,
    pub paired: bool,
    pub web_rtc_peer: Option<Arc<DesktopWebRtcPeer>>,
    pub transfer_manager: Option<Arc<WebRtcTransferManager>>,
    pub web_rtc_wire: Option<WebRtcWireBinding>,
    pub web_rtc_ready: bool,
}

/// What an install of a WebRTC peer did.
#[allow(clippy::large_enum_variant)] // the refused peer is handed back whole, once per race
pub enum Installed {
    /// The peer is the session's now; the one it replaced, if any.
    Replaced(Option<Arc<DesktopWebRtcPeer>>),
    /// The session already had a peer: the new one comes back, unused.
    Occupied(DesktopWebRtcPeer),
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionError {
    UnknownSession,
    StaleBinding,
    TerminatedSession,
    AlreadyClaimed,
    NotEligibleForReconnect,
    NotPaired,
    ReplayDetected,
    Poisoned,
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::UnknownSession => "Device session is unknown",
            Self::StaleBinding => "Connection was replaced by a newer session generation",
            Self::TerminatedSession => "Device session is terminated",
            Self::AlreadyClaimed => "Reconnect is already in progress",
            Self::NotEligibleForReconnect => "Device session is not eligible for reconnect",
            Self::NotPaired => "DeskLink WebRTC requires a paired device session",
            Self::ReplayDetected => "DeskLink WebRTC signaling request was replayed",
            Self::Poisoned => "Device session lock poisoned",
        };
        formatter.write_str(message)
    }
}

#[allow(dead_code)]
pub struct RegistrationResult {
    pub binding: SessionBinding,
    pub replaced_link: Option<Arc<SessionLink>>,
    pub replaced_generation: Option<ConnectionGeneration>,
    pub replaced_webrtc: Option<Arc<DesktopWebRtcPeer>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconnectLease {
    pub device_id: String,
    pub session_id: SessionId,
    pub generation: ConnectionGeneration,
}

pub struct DisconnectResult {
    pub was_current: bool,
    /// The closed socket was only the LAN bootstrap/signaling path.  A
    /// mutually feature-ready WebRTC peer remains authoritative for the
    /// logical session and must not be cancelled with that socket.
    pub feature_transport_retained: bool,
    pub link: Option<Arc<SessionLink>>,
    pub webrtc: Option<Arc<DesktopWebRtcPeer>>,
}

/// The outcome of one liveness pass: who to probe, and who has gone.
#[derive(Default)]
pub struct LivenessTick {
    pub probe: Vec<SessionBinding>,
    pub dead: Vec<SessionBinding>,
}

/// A reachability reading for one device, taken under the session lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceConnection {
    pub device_id: String,
    /// A bootstrap link exists and its socket has not been closed.
    pub linked: bool,
    pub paired: bool,
    pub feature_transport_ready: bool,
    /// How long the LAN link has been silent; `None` when its socket is
    /// closed (or there is none): the retained-FIN case.
    pub lan_silence: Option<std::time::Duration>,
}

impl DeviceConnection {
    /// A device is reachable over the bootstrap link, the authenticated WebRTC
    /// transport, or both. Losing only one of the two is not a disconnect.
    pub fn is_reachable(&self) -> bool {
        self.linked || self.feature_transport_ready
    }
}

/// What to do with a newly authenticated bootstrap socket for a session that
/// may have a feature-ready WebRTC peer to preserve.
pub enum Adoption {
    /// The current socket is open and answering: the newcomer is redundant.
    Keep,
    /// Swapped into the existing session (same generation, same link, so the
    /// WebRTC peer lives): read it from `epoch`, shut `replaced` down.
    Adopted {
        binding: SessionBinding,
        replaced: Arc<dyn LinkTransport>,
        epoch: u64,
    },
    /// Nothing to preserve: register it as a new generation.
    Register,
    /// It presents a different certificate from the session's link: never
    /// swapped into a session another certificate authenticated.
    Refuse,
}
