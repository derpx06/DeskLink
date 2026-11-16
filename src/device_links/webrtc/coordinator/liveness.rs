//! Data-channel liveness: peer-state classification and its constants, the
//! inbound and silence clocks, dozing reports, probes, keepalives, and
//! forgetting a device's liveness state.
//!
//! Sibling edges: `recovery`, `ice_setup` (forgetting their per-device state). Outside: `packet`,
//! `peer_connection`.

use super::*;

/// How the desktop should react to a `webrtcbin` connection-state change.
///
/// `disconnected` is explicitly transient in WebRTC and appears routinely when
/// the phone turns its screen off or switches Wi-Fi power state. Treating it as
/// terminal tore the peer down on every blip and started a fresh signed attempt
/// while Android was still inside its own grace window, which is what made the
/// two ends flap against each other. The Android application waits
/// `TRANSIENT_DISCONNECT_GRACE` before rebuilding; this mirrors that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PeerStateAction {
    Ignore,
    WaitForRecovery,
    Rebuild,
}

pub(crate) const TRANSIENT_DISCONNECT_GRACE: std::time::Duration =
    std::time::Duration::from_secs(12);

/// How long a fresh peer may take to reach feature-ready before it is rebuilt.
/// The measured LAN path is under 1 s; 10 s leaves room for a slow phone.
pub(crate) const NEGOTIATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// Data-channel liveness, mirrored by `startLivenessLocked` on Android. Once the
/// handover is feature-ready, a side that has heard nothing on any data
/// channel for `PEER_PROBE_AFTER` sends a `desklink.heartbeat`; the other side
/// answers with `reply: true`. Silence for `PEER_DEAD_AFTER` means the peer is
/// gone — typically its process restarted — and it is rebuilt at once instead
/// of waiting ~25 s for ICE consent freshness to notice.
pub(crate) const PEER_PROBE_AFTER: std::time::Duration = std::time::Duration::from_secs(5);
pub(crate) const PEER_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(15);
/// [`PEER_DEAD_AFTER`] while the phone reports `dozing`. A phone whose screen
/// is off is frozen by its OS (ColorOS: cgroup freezer, scene LcdOff), and only
/// inbound TCP thaws it — the desk's 10 s LAN heartbeat, which stays the
/// deliberate thaw — while data-channel UDP does not. 15 s left a 4–5 s margin
/// and every missed thaw rebuilt the peer.
pub(crate) const PEER_DEAD_AFTER_DOZING: std::time::Duration = std::time::Duration::from_secs(35);

/// `desklink.heartbeat {dozing}`: whether the sender says it is dozing. A
/// state report, never a command; absent or not a boolean is awake (older
/// builds never send it).
pub(super) fn reports_dozing(packet: &NetworkPacket) -> bool {
    packet.get_bool("dozing") == Some(true)
}

/// When each phone last sent anything on its data channel: features travel
/// there and never on the LAN link, so this is the clock that says whether a
/// phone being driven (glide) is still answering. The LAN clock
/// (`DeviceManager::silence`) is deliberately left to the LAN heartbeat, whose
/// silence is what thaws a dozing phone.
fn data_inbound() -> &'static Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static HEARD: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    HEARD.get_or_init(Default::default)
}

/// Records data-channel traffic from `device_id` now.
pub(super) fn note_data_inbound(device_id: &str) {
    if let Ok(mut heard) = data_inbound().lock() {
        heard.insert(device_id.to_string(), std::time::Instant::now());
    }
}

/// How long the data channel of `device_id` has been silent, if it ever spoke.
pub(crate) fn data_silence(device_id: &str) -> Option<std::time::Duration> {
    data_inbound()
        .lock()
        .ok()?
        .get(device_id)
        .map(std::time::Instant::elapsed)
}

/// The device is gone (unpaired): what is remembered of its liveness goes.
pub(crate) fn forget_liveness(device_id: &str) {
    if let Ok(mut heard) = data_inbound().lock() {
        heard.remove(device_id);
    }
    if let Ok(mut dozing) = dozing_devices().lock() {
        dozing.remove(device_id);
    }
    if let Ok(mut woke) = probe_now().lock() {
        woke.remove(device_id);
    }
    if let Ok(mut streaks) = lacking_streaks().lock() {
        streaks.remove(device_id);
    }
    if let Ok(mut used) = ice_addresses().lock() {
        used.remove(device_id);
    }
}

/// The silence of a link seen through both clocks: whichever heard the phone
/// most recently (a phone talks on the data channel, and answers the LAN
/// probe).
pub(crate) fn link_silence(
    lan: Option<std::time::Duration>,
    data: Option<std::time::Duration>,
) -> Option<std::time::Duration> {
    match (lan, data) {
        (Some(lan), Some(data)) => Some(lan.min(data)),
        (lan, data) => lan.or(data),
    }
}

fn dozing_devices() -> &'static Mutex<std::collections::HashSet<String>> {
    static DOZING: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    DOZING.get_or_init(Default::default)
}

/// Records what `device_id`'s latest heartbeat (LAN or data channel) said.
/// A phone that just woke (`dozing: false` after `true`) is probed on the
/// data channel at once: its link may have died while it slept.
/// Whether the phone just woke (a sign of it: see [`recovery_sign`]).
pub(crate) fn note_heartbeat(device_id: &str, packet: &NetworkPacket) -> bool {
    let woke = set_dozing(device_id, reports_dozing(packet));
    if woke {
        if let Ok(mut woke) = probe_now().lock() {
            woke.insert(device_id.to_string());
        }
    }
    woke
}

/// Probes `device_id`'s data channel at the next pass instead of after the
/// silence budget: this computer (not the phone) just came back from sleep or
/// changed network, so a peer it still believes in may be dead.
pub(crate) fn probe_soon(device_id: &str) {
    if let Ok(mut woke) = probe_now().lock() {
        woke.insert(device_id.to_string());
    }
}

/// The silence clock after the phone woke: restarted, so the silence that
/// built up over the whole doze is not judged at once (a woken phone needs
/// about one probe cycle to answer). Otherwise unchanged.
pub(super) fn clock_after_wake(
    woke: bool,
    last_inbound: std::time::Instant,
    now: std::time::Instant,
) -> std::time::Instant {
    if woke {
        now
    } else {
        last_inbound
    }
}

fn probe_now() -> &'static Mutex<std::collections::HashSet<String>> {
    static PROBE_NOW: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    PROBE_NOW.get_or_init(Default::default)
}

/// Whether `device_id` woke since this was last asked (and forgets it).
pub(super) fn take_probe_now(device_id: &str) -> bool {
    probe_now()
        .lock()
        .is_ok_and(|mut woke| woke.remove(device_id))
}

/// Records the phone's doze state. Only the phone clears it: a finished
/// handover says nothing about whether its screen is on. Whether it woke.
fn set_dozing(device_id: &str, dozing: bool) -> bool {
    let Ok(mut devices) = dozing_devices().lock() else {
        return false;
    };
    if dozing {
        devices.insert(device_id.to_string());
        false
    } else {
        devices.remove(device_id)
    }
}

/// The silence liveness judges by: the data channel's, or while the phone
/// dozes the more recent of the data channel's and the LAN link's (a frozen
/// phone thawed by the LAN heartbeat answers on either).
pub(super) fn effective_silence(
    data: std::time::Duration,
    lan: Option<std::time::Duration>,
    dozing: bool,
) -> std::time::Duration {
    match lan {
        Some(lan) if dozing => data.min(lan),
        _ => data,
    }
}

pub(crate) fn is_dozing(device_id: &str) -> bool {
    dozing_devices()
        .lock()
        .is_ok_and(|devices| devices.contains(device_id))
}

/// What the worker should do about data-channel silence right now.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Liveness {
    Fine,
    Probe,
    Dead,
}

/// While `dozing`, the dead time is [`PEER_DEAD_AFTER_DOZING`] and nothing is
/// probed: a frozen phone cannot answer, and a probe does not thaw it.
pub(crate) fn liveness(
    silence: std::time::Duration,
    since_probe: std::time::Duration,
    dozing: bool,
) -> Liveness {
    let dead_after = if dozing {
        PEER_DEAD_AFTER_DOZING
    } else {
        PEER_DEAD_AFTER
    };
    if silence >= dead_after {
        Liveness::Dead
    } else if !dozing && silence >= PEER_PROBE_AFTER && since_probe >= PEER_PROBE_AFTER {
        Liveness::Probe
    } else {
        Liveness::Fine
    }
}

/// Sends a data-channel heartbeat; `reply` marks an answer, which is never
/// answered in turn.
pub(super) fn send_keepalive(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &DeviceManager,
    reply: bool,
) -> Result<(), String> {
    let peer = sessions
        .active_webrtc_peer(binding, attempt_id)
        .map_err(|error| error.to_string())?;
    let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_HEARTBEAT);
    if reply {
        packet.set("reply", true);
    }
    let envelope = WebRtcPacketBridge::encode(wire, &packet, now_millis())?;
    let text = String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink heartbeat serialization was not UTF-8".to_string())?;
    // On its own channel, behind nothing; skipped when its queue is backed up
    // (the next one supersedes it).
    peer.send_heartbeat(&text).map(|_| ())
}

pub(super) fn expired(since: Option<std::time::Instant>) -> bool {
    since.is_some_and(|since| since.elapsed() >= TRANSIENT_DISCONNECT_GRACE)
}

pub(crate) fn classify_peer_state(state: &str) -> PeerStateAction {
    let state = state.to_ascii_lowercase();
    if state.contains("failed") || state.contains("closed") {
        PeerStateAction::Rebuild
    } else if state.contains("disconnected") {
        PeerStateAction::WaitForRecovery
    } else {
        PeerStateAction::Ignore
    }
}
