//! Recovery: restart requests, the backoff ladder and when it resets, the words
//! for an unavailable or out-of-date phone, lacking-channel streaks, start
//! claims and signs of the phone.
//!
//! Sibling edges: `liveness`. Outside: `core`, `daemon::discovery`,
//! `daemon::state` (errors).

use super::*;

/// What a phone's restart request does here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestartAction {
    Ignore,
    Restart,
}

/// Only the deterministic initiator rebuilds on request, and only the attempt
/// the request is about: a late request about one that has since been
/// replaced (both sides declared silence in the same second, and this side
/// already rebuilt) must not kill the healthy newer peer. So two sides that
/// both give up produce one rebuild, not two.
pub(super) fn restart_request_action(
    local_initiates: bool,
    active_attempt: Option<&str>,
    requested_attempt: &str,
) -> RestartAction {
    if !local_initiates {
        // This side answers; the initiator rebuilds on its own.
        return RestartAction::Ignore;
    }
    if active_attempt.is_some_and(|active| active != requested_attempt) {
        return RestartAction::Ignore;
    }
    RestartAction::Restart
}

/// How long a peer must stay feature-ready before the recovery ladder starts
/// over. Mirrored by Android's `WebRtcRecoveryPolicy`.
pub(crate) const HEALTHY_AFTER: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether a peer that has been feature-ready for `ready_for` has earned a
/// fresh ladder.
pub(super) fn ladder_resets(ready_for: Option<std::time::Duration>) -> bool {
    ready_for.is_some_and(|ready_for| ready_for >= HEALTHY_AFTER)
}

/// What the unavailable state says (the ladder is exhausted).
pub(crate) fn unavailable_words(phone: &str) -> String {
    format!("Can't reach {phone} · Try again")
}

/// Whether `error` is the unavailable state, which has a "Try Again" control.
pub(crate) fn is_unavailable(error: &str) -> bool {
    error.starts_with("Can't reach ")
}

/// What the state says when the phone's app is too old to connect: a build
/// from before the liveness channel never opens it, so the link can never
/// become feature-ready however often it is retried.
pub(crate) fn update_words(phone: &str) -> String {
    format!("Update DeskLink on {phone}")
}

/// Whether a peer that was torn down shows the phone lacks the liveness
/// channel: the others were open and it never was.
pub(super) fn lacks_liveness_channel(control: bool, events: bool, liveness: bool) -> bool {
    control && events && !liveness
}

/// Consecutive torn-down peers, per phone, that lacked the liveness channel.
pub(super) fn lacking_streaks() -> &'static Mutex<HashMap<String, u32>> {
    static STREAKS: std::sync::OnceLock<Mutex<HashMap<String, u32>>> = std::sync::OnceLock::new();
    STREAKS.get_or_init(Default::default)
}

/// Two in a row are a phone with an older app, not a slow network.
const LACKING_STREAK_FOR_UPDATE: u32 = 2;

pub(super) fn note_channels_of(device_id: &str, peer: &DesktopWebRtcPeer) {
    let lacks = lacks_liveness_channel(
        peer.channel_open(WebRtcChannel::Control),
        peer.channel_open(WebRtcChannel::Events),
        peer.channel_open(WebRtcChannel::Liveness),
    );
    note_lacking(device_id, lacks);
}

pub(super) fn note_lacking(device_id: &str, lacks: bool) {
    if let Ok(mut streaks) = lacking_streaks().lock() {
        if lacks {
            *streaks.entry(device_id.to_string()).or_default() += 1;
        } else {
            streaks.remove(device_id);
        }
    }
}

/// What the exhausted ladder says for `device_id`: "Update DeskLink on …" when
/// the phone keeps lacking the liveness channel, else "Can't reach …".
pub(super) fn exhausted_words(device_id: &str, phone: &str) -> String {
    let lacking = lacking_streaks()
        .lock()
        .ok()
        .and_then(|streaks| streaks.get(device_id).copied())
        .unwrap_or(0);
    if lacking >= LACKING_STREAK_FOR_UPDATE {
        update_words(phone)
    } else {
        unavailable_words(phone)
    }
}

/// "Try again" in the Overview card, the tray and the shell: the phone is
/// tried now, however long the ladder had been given up.
pub(crate) fn reconnect(
    device_id: &str,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    crate::device_links::daemon::discovery::wake_addresses(device_id);
    recovery_sign(device_id, sessions, config, devices, events);
}

/// Marks a start of the WebRTC connection to `device_id` as under way; dropped
/// when it ends.
pub(crate) struct Starting(String);

impl Drop for Starting {
    fn drop(&mut self) {
        if let Ok(mut starting) = starting().lock() {
            starting.remove(&self.0);
        }
    }
}

fn starting() -> &'static Mutex<std::collections::HashSet<String>> {
    static STARTING: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    STARTING.get_or_init(Default::default)
}

/// The claim to start a connection to `device_id`, or `None` when one is
/// already under way.
pub(crate) fn claim_start(device_id: &str) -> Option<Starting> {
    starting()
        .lock()
        .ok()?
        .insert(device_id.to_string())
        .then(|| Starting(device_id.to_string()))
}

/// Signs of a phone closer together than this are one sign.
const SIGN_DEBOUNCE: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether a sign of `device_id` at `now` is a new one (see [`SIGN_DEBOUNCE`]).
pub(super) fn sign_is_new(device_id: &str, now: std::time::Instant) -> bool {
    static LAST: Mutex<Option<HashMap<String, std::time::Instant>>> = Mutex::new(None);
    let Ok(mut last) = LAST.lock() else {
        return true;
    };
    let last = last.get_or_insert_with(HashMap::new);
    match last.get(device_id) {
        Some(at) if now.saturating_duration_since(*at) < SIGN_DEBOUNCE => false,
        _ => {
            last.insert(device_id.to_string(), now);
            true
        }
    }
}

/// Any sign of the phone — a network change, the phone waking, a beacon, an
/// inbound dial or offer — starts the recovery ladder over and, when no peer
/// is being built, reconnects at once instead of waiting out the backoff.
pub(crate) fn recovery_sign(
    device_id: &str,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    let Some(binding) = sessions
        .current_bindings()
        .into_iter()
        .find(|binding| binding.device_id == device_id)
    else {
        // The session has no link (it was cut): still a sign of a paired
        // phone, so "Try again" and a network change reset the ladder and the
        // "Can't reach" words now, not when a link next registers.
        if sign_is_new(device_id, std::time::Instant::now()) {
            if let Some(attempt) = sessions.reset_recovery(device_id) {
                if recovery_delay(attempt).is_none() || shows_unavailable(devices, device_id) {
                    clear_error(devices, device_id);
                }
            }
        }
        return;
    };
    // Only for a phone that is paired: an unpaired link must never make this
    // side build a pipeline, a transfer manager and a thread.
    if !is_paired(sessions, &binding) {
        return;
    }
    if !sign_is_new(device_id, std::time::Instant::now()) {
        return;
    }
    let was_exhausted = sessions
        .with_session(&binding, |session| {
            let exhausted = recovery_delay(session.webrtc_recovery_attempt).is_none();
            session.webrtc_recovery_attempt = 0;
            exhausted
        })
        .unwrap_or(false);
    if was_exhausted || shows_unavailable(devices, device_id) {
        clear_error(devices, device_id);
    }
    if sessions
        .active_webrtc_attempt(&binding)
        .ok()
        .flatten()
        .is_some()
    {
        return;
    }
    let (sessions, config, devices, events) = (
        Arc::clone(sessions),
        Arc::clone(config),
        Arc::clone(devices),
        Arc::clone(events),
    );
    thread::spawn(move || {
        if let Err(error) = start_initiator(binding, sessions, config, devices, events) {
            eprintln!("[DeskLink] Reconnecting at a sign of the phone failed: {error}");
        }
    });
}

/// Whether the card still says the phone can't be reached (a new link resets
/// the ladder but not the words).
fn shows_unavailable(devices: &Arc<Mutex<HashMap<String, DeviceView>>>, device_id: &str) -> bool {
    devices.lock().is_ok_and(|devices| {
        devices
            .get(device_id)
            .and_then(|device| device.last_error.as_deref())
            .is_some_and(is_unavailable)
    })
}

/// Bounded reconnect backoff, matching `WebRtcRecoveryPolicy` on Android.
/// Rebuilding immediately and unconditionally allocated a fresh GStreamer
/// pipeline and seven data channels per failure, so a peer that could not be
/// reached at all span the CPU instead of settling into a retry cadence.
/// Mirrored by Android's `WebRtcRecoveryPolicy` (contract B3): a healthy
/// rebuild takes under a second, so the first rung is short.
pub(super) const RECOVERY_BACKOFF_MILLIS: [u64; 6] = [250, 1_000, 2_000, 4_000, 8_000, 16_000];

/// The delay before retry `attempt`, or `None` once the ladder is exhausted.
///
/// Previously this clamped to the last rung and retried every 30 seconds
/// forever. Nothing ever concluded that the connection was not going to happen,
/// so `feature_transport_ready` stayed false indefinitely and the UI sat on
/// "Connecting" for as long as the application ran — the state the user
/// reported. Returning `None` is what lets the caller surface an honest
/// terminal state instead.
///
/// This bounds *initiation only*. The desktop must keep accepting inbound
/// offers afterwards: the phone is the deterministic initiator whenever its
/// device id sorts lower, and `WebRtcRecoveryPolicy` on Android never gives up
/// on attempt count. A new or adopted bootstrap link, a sign of the phone or
/// "Try again" resets the ladder; a restart request from the responder only
/// caps it at the last rung.
pub(crate) fn recovery_delay(attempt: u32) -> Option<std::time::Duration> {
    RECOVERY_BACKOFF_MILLIS
        .get(attempt as usize)
        .map(|millis| std::time::Duration::from_millis(*millis))
}

// Signal send failures: a bootstrap signal that cannot be sent never ends
// the peer's event worker. Losing the bootstrap socket while an
// authenticated WebRTC transport is retained is a designed state, and a late
// ICE candidate written into that socket fails as a matter of course: as
// "Session link is closed", or as a raw EPIPE / ECONNRESET when the socket
// died before its reader noticed. Ending the worker for the latter tore down
// a peer that could be healthy and feature-ready, with no rebuild and no
// error. A negotiation the lost signal stalls is rebuilt by
// `NEGOTIATION_TIMEOUT`; a ready peer never needed the signal.

/// Forgets `device_id`'s lacking-channel streak (see [`forget_liveness`]).
pub(super) fn forget(device_id: &str) {
    if let Ok(mut streaks) = lacking_streaks().lock() {
        streaks.remove(device_id);
    }
}
