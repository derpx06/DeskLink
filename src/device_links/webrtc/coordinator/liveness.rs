//! Data-channel liveness: peer-state classification and its constants, the
//! inbound and silence clocks, dozing and leaving reports, probes,
//! keepalives, the corroborated fast verdict ([`fast_dead`]), the "Checking
//! connection..." tier ([`is_quiet`]), the click preflight ([`await_proof`])
//! and forgetting a device's clocks.
//!
//! The desk-only numbers ([`DESK_PROBE_AFTER`], [`QUIET_AFTER`],
//! [`FAST_DEAD_AFTER`], [`LAN_FIN_DEAD_AFTER`], [`LEAVING_DEAD_AFTER`],
//! [`PREFLIGHT_WAIT`]) are not mirrored: the phone already answers LAN and data
//! probes. None of them applies while the phone dozes. There is no data-channel
//! `on_close` handler on purpose: `chaos::a_remote_close_is_or_is_not_seen_before_liveness`
//! measured, with two real peers, that after one side's `close()` the other's
//! required channels were still `Open` 6 s later, so neither a callback nor a
//! polled `ready_state` would beat these rules.
//!
//! Sibling edges: none (the parent's helpers only). Outside: `packet`,
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

/// Silence after which this desk (never the phone: its own `PEER_PROBE_AFTER`
/// stays 5 s) probes a phone that is *not* dozing. The probe rides the
/// liveness channel, separate from files and events, and a dozing phone is
/// never data-probed, so this costs one tiny reply per 2 s of idle while the
/// phone's screen is on and nothing while it sleeps.
pub(crate) const DESK_PROBE_AFTER: std::time::Duration = std::time::Duration::from_secs(2);
/// Silence (not dozing) after which the UI says "Checking connection…": a
/// display state only, cleared by any inbound packet, tearing nothing down.
pub(crate) const QUIET_AFTER: std::time::Duration = std::time::Duration::from_secs(4);
/// Data-channel silence *and* LAN silence (not dozing) after which the peer
/// is judged gone. Data-only evidence stays [`PEER_DEAD_AFTER`]: a stall on
/// one path is not a verdict, silence on both is.
pub(crate) const FAST_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(8);
/// Data silence after the LAN socket closed while features were ready (not
/// dozing), with a probe sent and unanswered. The retained-FIN rule keeps
/// the peer through a bootstrap-socket recycle; a healthy data path answers
/// the probe well inside this, so only a peer that is really gone reaches it.
pub(crate) const LAN_FIN_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(3);
/// After the phone said it is `leaving`: it is probed at once, and silence
/// this long (one missed probe) means it went.
pub(crate) const LEAVING_DEAD_AFTER: std::time::Duration = std::time::Duration::from_secs(3);
/// How long a click waits for any sign of life from a quiet phone.
pub(crate) const PREFLIGHT_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);

/// `desklink.heartbeat {dozing}`: whether the sender says it is dozing. A
/// state report, never a command; absent or not a boolean is awake (older
/// builds never send it).
pub(super) fn reports_dozing(packet: &NetworkPacket) -> bool {
    packet.get_bool("dozing") == Some(true)
}

/// `desklink.heartbeat {leaving}`: the sender says it is about to go away
/// (Wi-Fi switching off, airplane mode, service stopping). A state report like
/// `dozing`, never a command; absent or not a boolean is not leaving. `reason`
/// is for logs only.
pub(super) fn reports_leaving(packet: &NetworkPacket) -> bool {
    packet.get_bool("leaving") == Some(true)
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

/// [`note_data_inbound`] as if it had been `ago` ago (tests step the clock by
/// moving the past, never by sleeping).
#[cfg(test)]
pub(crate) fn note_data_inbound_ago(device_id: &str, ago: std::time::Duration) {
    if let Ok(mut heard) = data_inbound().lock() {
        heard.insert(device_id.to_string(), std::time::Instant::now() - ago);
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

/// Forgets `device_id`'s liveness clocks (see [`forget_liveness`]).
pub(super) fn forget(device_id: &str) {
    if let Ok(mut heard) = data_inbound().lock() {
        heard.remove(device_id);
    }
    if let Ok(mut dozing) = dozing_devices().lock() {
        dozing.remove(device_id);
    }
    if let Ok(mut woke) = probe_now().lock() {
        woke.remove(device_id);
    }
    if let Ok(mut leaving) = leaving_devices().lock() {
        leaving.remove(device_id);
    }
    if let Ok(mut asked) = probe_asap_set().lock() {
        asked.remove(device_id);
    }
    if let Ok(mut failed) = preflight_failures().lock() {
        failed.remove(device_id);
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

fn leaving_devices() -> &'static Mutex<std::collections::HashSet<String>> {
    static LEAVING: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    LEAVING.get_or_init(Default::default)
}

/// Whether `device_id` said it is leaving and has not spoken since.
pub(crate) fn is_leaving(device_id: &str) -> bool {
    leaving_devices()
        .lock()
        .is_ok_and(|devices| devices.contains(device_id))
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
    // A phone that says it is leaving is probed at once; the next missed
    // probe is the verdict ([`LEAVING_DEAD_AFTER`]). Any later heartbeat
    // without the field (its reply to that probe) says it is still here.
    let leaving = reports_leaving(packet);
    if let Ok(mut devices) = leaving_devices().lock() {
        if leaving {
            devices.insert(device_id.to_string());
        } else {
            devices.remove(device_id);
        }
    }
    if leaving {
        debug_log!(
            "[DL-LIVE] {} reports leaving ({})",
            device_id,
            log_reason(packet.get_str("reason"))
        );
        probe_asap(device_id);
    }
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

/// A phone's `reason`, for the log only: printable, at most 32 characters.
fn log_reason(reason: Option<&str>) -> String {
    let text: String = reason
        .unwrap_or("no reason")
        .chars()
        .filter(|c| !c.is_control())
        .take(32)
        .collect();
    text
}

/// A new link or a ready peer is the phone being here again: whatever it said
/// about leaving is history (it would otherwise show "Checking" on the new
/// link and give it only [`LEAVING_DEAD_AFTER`]).
pub(crate) fn clear_leaving(device_id: &str) {
    if let Ok(mut devices) = leaving_devices().lock() {
        devices.remove(device_id);
    }
}

/// How long a failed click preflight is remembered: the next click on the
/// same dead phone fails at once instead of making the command thread wait
/// again.
pub(crate) const PREFLIGHT_MEMORY: std::time::Duration = std::time::Duration::from_secs(2);

fn preflight_failures() -> &'static Mutex<std::collections::HashMap<String, std::time::Instant>> {
    static FAILED: std::sync::OnceLock<
        Mutex<std::collections::HashMap<String, std::time::Instant>>,
    > = std::sync::OnceLock::new();
    FAILED.get_or_init(Default::default)
}

pub(crate) fn note_preflight_failed(device_id: &str) {
    if let Ok(mut failed) = preflight_failures().lock() {
        failed.insert(device_id.to_string(), std::time::Instant::now());
    }
}

/// Whether a preflight for `device_id` failed within [`PREFLIGHT_MEMORY`] of `now`.
pub(crate) fn preflight_failed_recently(device_id: &str, now: std::time::Instant) -> bool {
    preflight_failures().lock().is_ok_and(|failed| {
        failed
            .get(device_id)
            .is_some_and(|at| now.saturating_duration_since(*at) < PREFLIGHT_MEMORY)
    })
}

/// A probe counts as unanswered only once it has had [`PROBE_ANSWER_WAIT`] to
/// be answered (`probe_after_inbound`: it was sent after the last thing heard).
pub(crate) const PROBE_ANSWER_WAIT: std::time::Duration = std::time::Duration::from_secs(1);

pub(crate) fn probe_unanswered(probe_after_inbound: bool, probe_age: std::time::Duration) -> bool {
    probe_after_inbound && probe_age >= PROBE_ANSWER_WAIT
}

/// Asks for a probe at the next pass *without* restarting the silence clock
/// (unlike [`probe_soon`]): the question is "are you there?", and not
/// hearing back must still count. A click and a `leaving` report use this.
pub(crate) fn probe_asap(device_id: &str) {
    if let Ok(mut asked) = probe_asap_set().lock() {
        asked.insert(device_id.to_string());
    }
}

fn probe_asap_set() -> &'static Mutex<std::collections::HashSet<String>> {
    static ASAP: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    ASAP.get_or_init(Default::default)
}

/// Whether a probe was asked for since this was last called (and forgets it).
pub(super) fn take_probe_asap(device_id: &str) -> bool {
    probe_asap_set()
        .lock()
        .is_ok_and(|mut asked| asked.remove(device_id))
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
    } else if !dozing && silence >= DESK_PROBE_AFTER && since_probe >= DESK_PROBE_AFTER {
        Liveness::Probe
    } else {
        Liveness::Fine
    }
}

/// The corroborated fast verdict, never while `dozing` (a frozen phone
/// answers nothing and its numbers stay 35 s / 90 s). `lan` is the LAN link's
/// silence, `None` when its socket is closed; `unanswered` says a probe was
/// sent after the last thing heard.
///
/// - LAN socket closed: [`LAN_FIN_DEAD_AFTER`] of data silence with the probe
///   unanswered (the FIN is evidence, not a verdict: a recycled bootstrap
///   socket with a healthy data path answers the probe and stays).
/// - LAN open: [`FAST_DEAD_AFTER`] of silence on both paths.
/// - `leaving`: [`LEAVING_DEAD_AFTER`] of silence on both.
pub(crate) fn fast_dead(
    data: std::time::Duration,
    lan: Option<std::time::Duration>,
    unanswered: bool,
    dozing: bool,
    leaving: bool,
) -> bool {
    if dozing {
        return false;
    }
    let both = |limit| data >= limit && lan.is_none_or(|lan| lan >= limit);
    (lan.is_none() && unanswered && data >= LAN_FIN_DEAD_AFTER)
        || both(FAST_DEAD_AFTER)
        || (leaving && both(LEAVING_DEAD_AFTER))
}

/// Whether the UI should say "Checking connection…": the link has been
/// silent on both paths for [`QUIET_AFTER`], or the phone said it is leaving.
/// Display only; never while dozing (a sleeping phone is silent by design).
pub(crate) fn is_quiet(
    data: Option<std::time::Duration>,
    lan: Option<std::time::Duration>,
    dozing: bool,
    leaving: bool,
) -> bool {
    !dozing && (leaving || link_silence(lan, data).is_some_and(|silence| silence >= QUIET_AFTER))
}

/// A wall-clock stamp (UTC, `HH:MM:SS.mmm`) for liveness debug lines: the
/// live logs carry none, so a real run could not be measured.
pub(crate) fn stamp() -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() % 86_400_000);
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        millis / 3_600_000,
        millis / 60_000 % 60,
        millis / 1000 % 60,
        millis % 1000
    )
}

/// Waits up to `wait` for any sign of life from `device_id` (a probe is sent
/// at once; any inbound packet on either path counts) and says whether it
/// came. `silence` reads the link's current silence.
pub(crate) fn await_proof(
    device_id: &str,
    wait: std::time::Duration,
    silence: impl Fn() -> Option<std::time::Duration>,
) -> bool {
    let fresh = |silence: Option<std::time::Duration>| {
        silence.is_some_and(|silence| silence < DESK_PROBE_AFTER)
    };
    if fresh(silence()) {
        return true;
    }
    probe_asap(device_id);
    let started = std::time::Instant::now();
    while started.elapsed() < wait {
        std::thread::sleep(std::time::Duration::from_millis(50));
        if fresh(silence()) {
            return true;
        }
    }
    false
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
