//! The bridges of every device and the pump's view of them: begin/end with a
//! token, pumping under a budget, the envelope entry point with its gates,
//! revoking and ending a device's sessions, finished-job alerts and the idle
//! reaper.
//!
//! Sibling edges: `bridge`. Outside: `pty`, `session_lock` (the W7 gates),
//! `desk_features`, `config`, `core`.

use super::*;

/// Terminal bridges in progress, keyed by peer device id.
///
/// Shared for the same reason as `screen::streams` and
/// `remote_session::sessions`: the producer is a data-channel callback on the
/// peer worker thread and the consumers are the pump thread and the GTK main
/// loop. **Lock order is bridges, then registry** — everywhere.
pub(super) fn bridges() -> &'static Mutex<HashMap<String, TerminalBridge>> {
    static BRIDGES: OnceLock<Mutex<HashMap<String, TerminalBridge>>> = OnceLock::new();
    BRIDGES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Something may be waiting for a pump (a request, a reply, an alert, a
/// revoke): wakes every idle one. Pumps share the terminals' change signal,
/// so new output wakes them too.
pub(super) fn wake_pumps() {
    crate::device_links::pty::session::changed();
}

/// The wake counter now; hand it to [`wait_for_work`] *before* looking for
/// work, so a wake in between is not lost.
pub(crate) fn wake_generation() -> u64 {
    crate::device_links::pty::session::change_generation()
}

/// Sleeps until a pump is woken after `seen`, or `timeout` passes.
pub(crate) fn wait_for_work(seen: u64, timeout: std::time::Duration) {
    crate::device_links::pty::session::wait_for_change(seen, timeout);
}

/// Whether the connection holding `token` has nothing a pump could send, now
/// or by itself later: no attached terminal (only attached terminals produce
/// output on their own) and no queued reply. `None` once superseded.
pub(crate) fn pump_is_idle(device_id: &str, token: u64) -> Option<bool> {
    let open = bridges().lock().ok()?;
    let bridge = open.get(device_id).filter(|bridge| bridge.token == token)?;
    Some(bridge.attached.is_empty() && bridge.outbox.is_empty())
}

/// Identifies one connection's claim on its device's bridge. A reconnect takes
/// a new one, and everything the old connection's pump thread does afterwards
/// is keyed by its own, now stale, token — so it can end its own attachments
/// and never the newer connection's.
pub(super) static NEXT_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Claims the bridge for a connection whose terminal channel just opened.
///
/// Called on the peer worker thread *before* the pump thread is spawned, so no
/// message on this connection can arrive ahead of it. Whatever an earlier
/// connection from the same device left attached is released first: a phone
/// that reconnects has no memory of what it was watching.
pub(crate) fn begin(wire: &WebRtcWireBinding) -> u64 {
    ensure_idle_reaper();
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    let (Ok(mut open), Ok(mut registry)) = (bridges().lock(), registry().lock()) else {
        return token;
    };
    if let Some(previous) = open.get_mut(&wire.peer_device_id) {
        previous.detach_all(&mut registry);
    }
    open.insert(
        wire.peer_device_id.clone(),
        TerminalBridge::new(wire.clone(), token),
    );
    token
}

/// Releases a connection's attachments and leases when its pump ends.
///
/// Only if `token` is still the bridge's: a pump thread that notices it has been
/// replaced runs this a moment *after* the new connection may have attached,
/// and keyed by device id alone it detached that newer attachment.
pub(crate) fn end(device_id: &str, token: u64) {
    let (Ok(mut open), Ok(mut registry)) = (bridges().lock(), registry().lock()) else {
        return;
    };
    if open
        .get(device_id)
        .is_some_and(|bridge| bridge.token == token)
    {
        if let Some(mut bridge) = open.remove(device_id) {
            bridge.detach_all(&mut registry);
        }
    }
}

/// One pump for the connection that holds `token`, or `None` once it has been
/// superseded and its pump thread should stop.
pub(crate) fn pump_once(device_id: &str, token: u64, budget: usize) -> Option<Vec<TerminalEvent>> {
    let mut open = bridges().lock().ok()?;
    let elsewhere = attached_elsewhere(&open, device_id);
    let bridge = open
        .get_mut(device_id)
        .filter(|bridge| bridge.token == token)?;
    bridge.elsewhere = elsewhere;
    let mut registry = registry().lock().ok()?;
    Some(bridge.pump(budget, &mut registry))
}

/// Runs `operation` on the bridge serving exactly `wire`, with the registry.
pub(super) fn with_bridge<R>(
    wire: &WebRtcWireBinding,
    operation: impl FnOnce(&mut TerminalBridge, &mut TerminalRegistry) -> Result<R, String>,
) -> Result<R, String> {
    let mut open = bridges()
        .lock()
        .map_err(|_| "DeskLink terminal bridge lock poisoned".to_string())?;
    let elsewhere = attached_elsewhere(&open, &wire.peer_device_id);
    let bridge = open
        .get_mut(&wire.peer_device_id)
        .filter(|bridge| bridge.wire == *wire)
        .ok_or_else(|| "DeskLink terminal connection was replaced".to_string())?;
    bridge.elsewhere = elsewhere;
    let mut registry = registry()
        .lock()
        .map_err(|_| "DeskLink terminal registry lock poisoned".to_string())?;
    let result = operation(bridge, &mut registry);
    drop((registry, open));
    wake_pumps();
    result
}

/// Handles one `desklink-terminal-v1` envelope from a peer.
///
/// Checked in this order, each before the next can matter:
///
/// 1. the channel and the handover — the peer must have finished
///    authenticating, the same gate the file handler applies; this also proves
///    the binding is the *current* one (`with_session` refuses a stale binding),
///    for input chunks as much as for control messages;
/// 2. the envelope's own binding;
/// 3. **the user's consent** — "Allow terminal access" for this phone. Without
///    it the request gets `terminal-not-allowed` and nothing else happens: no
///    session is listed, created or attached, no registry lock is taken.
///
/// Control messages go through the session replay cache; **chunks deliberately
/// do not**. That cache holds 4096 entries shared by every feature, and a busy
/// terminal would wrap it in about a minute, evicting the replay protection of
/// everything else. A chunk is safe without it: the channel is ordered and
/// reliable, and the envelope is already bound to device, session and
/// generation, so there is no replay for the cache to catch.
///
/// Replies are queued on the peer's bridge; the pump sends them.
pub(crate) fn process_envelope(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    envelope: WebRtcEnvelope,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    if envelope.channel != WebRtcChannel::Terminal.label() {
        return Err("DeskLink terminal message arrived on the wrong channel".to_string());
    }
    if !sessions
        .webrtc_features_ready(binding, attempt_id)
        .map_err(|error| error.to_string())?
    {
        return Err("DeskLink terminal message arrived before feature handover".to_string());
    }
    let payload = envelope.validate(wire)?;

    match envelope.message_type.as_str() {
        TERMINAL_CONTROL_MESSAGE_TYPE => {
            sessions
                .accept_webrtc_envelope(binding, attempt_id, envelope.message_id.clone())
                .map_err(|error| error.to_string())?;
            let control = TerminalControl::from_json(&payload)?;
            control.validate(wire)?;
            if !terminal_access_allowed(&wire.peer_device_id) {
                return with_open_bridge(wire, |bridge| bridge.deny(control.terminal_id.clone()));
            }
            // Leaving is always allowed; everything else needs the session
            // password when one is set. Checked outside every lock: hashing
            // takes a moment.
            if control.action != TerminalAction::Detach {
                if let Err(reason) = crate::device_links::session_lock::check(
                    &wire.peer_device_id,
                    wire.session_id,
                    control.password.as_deref(),
                ) {
                    return with_open_bridge(wire, |bridge| {
                        bridge.refuse(control.terminal_id.clone(), &reason)
                    });
                }
            }
            let start = Start::of(&control);
            let handled = with_bridge(wire, |bridge, registry| {
                bridge.handle_control_in(&control, registry, &start)
            });
            // Remembered after every lock is released (it writes the config).
            if let (Ok(()), Start::In(path)) = (&handled, start) {
                let folder = crate::device_links::pty::registry::tilde(&path);
                crate::device_links::desk_features::run_later(move || {
                    crate::device_links::desk_features::remember_folder(&folder)
                });
            }
            handled
        }
        TERMINAL_CHUNK_MESSAGE_TYPE => {
            let chunk = TerminalChunk::decode(&payload)?;
            // Dropped without a reply: a phone typing into a terminal it has no
            // right to has already been told, and an answer per keystroke chunk
            // would only feed the channel a flood.
            if !terminal_access_allowed(&wire.peer_device_id)
                || !crate::device_links::session_lock::is_unlocked(
                    &wire.peer_device_id,
                    wire.session_id,
                )
            {
                return Ok(());
            }
            with_bridge(wire, |bridge, registry| {
                bridge.handle_chunk(&chunk, registry)
            })
        }
        _ => Err("Unsupported DeskLink terminal message type".to_string()),
    }
}

/// Like [`with_bridge`] but for operations that need no registry — a denial
/// must not depend on, or wait for, the machinery it is denying access to.
pub(super) fn with_open_bridge(
    wire: &WebRtcWireBinding,
    operation: impl FnOnce(&mut TerminalBridge),
) -> Result<(), String> {
    let mut open = bridges()
        .lock()
        .map_err(|_| "DeskLink terminal bridge lock poisoned".to_string())?;
    let bridge = open
        .get_mut(&wire.peer_device_id)
        .filter(|bridge| bridge.wire == *wire)
        .ok_or_else(|| "DeskLink terminal connection was replaced".to_string())?;
    operation(bridge);
    drop(open);
    wake_pumps();
    Ok(())
}

/// Takes `device_id`'s terminal access away immediately.
///
/// Call after the setting has been turned off. See [`TerminalBridge::revoke`].
pub(crate) fn revoke_device(device_id: &str) {
    let (Ok(mut open), Ok(mut registry)) = (bridges().lock(), registry().lock()) else {
        return;
    };
    match open.get_mut(device_id) {
        Some(bridge) => bridge.revoke(&mut registry),
        None => registry.release_writer(device_id),
    }
    drop((open, registry));
    wake_pumps();
}

/// Detaches `device_id` from every terminal it watches, keeping its access:
/// "End phone sessions" at the desk. The sessions keep running.
pub(crate) fn end_device_sessions(device_id: &str) {
    let (Ok(mut open), Ok(mut registry)) = (bridges().lock(), registry().lock()) else {
        return;
    };
    match open.get_mut(device_id) {
        Some(bridge) => bridge.end(&mut registry, TERMINAL_ENDED),
        None => registry.release_writer(device_id),
    }
    drop((open, registry));
    wake_pumps();
}

/// Terminals some phone other than `device_id` is attached to.
fn attached_elsewhere(open: &HashMap<String, TerminalBridge>, device_id: &str) -> HashSet<String> {
    open.iter()
        .filter(|(device, _)| device.as_str() != device_id)
        .flat_map(|(_, bridge)| bridge.attached.keys().cloned())
        .collect()
}

/// A long job in `terminal_id` finished: every phone with the terminal
/// channel open is told (it stays quiet if that terminal is on screen).
pub(crate) fn announce_finished(terminal_id: &str, job: &str, secs: u64) {
    let settings = crate::device_links::config::app_settings();
    let Ok(mut open) = bridges().lock() else {
        return;
    };
    for bridge in open.values_mut() {
        // Only phones that could open the terminal now, and want to hear.
        let device = bridge.device();
        if !settings.alerts_commands_to(device)
            || !terminal_access_allowed(device)
            || !crate::device_links::session_lock::is_unlocked(device, bridge.wire.session_id)
        {
            continue;
        }
        let mut finished = TerminalControl::new(TerminalAction::Finished, &bridge.wire);
        finished.terminal_id = Some(terminal_id.to_string());
        finished.job = Some(job.to_string());
        finished.secs = Some(secs);
        bridge.queue_always(TerminalEvent::SendControl(finished));
    }
    drop(open);
    wake_pumps();
}

/// How many terminals `device_id` is watching right now.
pub(crate) fn attached_count(device_id: &str) -> usize {
    bridges()
        .lock()
        .map(|open| {
            open.get(device_id)
                .map_or(0, |bridge| bridge.attached.len())
        })
        .unwrap_or(0)
}

/// The devices attached to session `terminal_id` right now, for the desk's
/// terminal tiles. Local only: the phone is never told.
pub(crate) fn attached_devices(terminal_id: &str) -> Vec<String> {
    let mut devices: Vec<String> = bridges()
        .lock()
        .map(|open| {
            open.iter()
                .filter(|(_, bridge)| bridge.attached.contains_key(terminal_id))
                .map(|(device, _)| device.clone())
                .collect()
        })
        .unwrap_or_default();
    #[cfg(debug_assertions)]
    if let Ok(fake) = DEBUG_ATTACHED.lock() {
        devices.extend(
            fake.iter()
                .filter(|(terminal, _)| terminal == terminal_id)
                .map(|(_, device)| device.clone()),
        );
    }
    devices.sort();
    devices
}

/// Debug builds only: (terminal, device) attachments shown without a phone,
/// for screenshots.
#[cfg(debug_assertions)]
pub(crate) static DEBUG_ATTACHED: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// Starts the background sweep that closes forgotten sessions, once.
fn ensure_idle_reaper() {
    static STARTED: Once = Once::new();
    STARTED.call_once(|| {
        std::thread::spawn(|| loop {
            std::thread::sleep(REAP_INTERVAL);
            sweep(idle_timeout());
        });
    });
}

/// One pass of the idle reaper: closes sessions nobody is attached to that have
/// been quiet for `idle_for`, and drops ones whose program has exited.
///
/// Holds the bridges lock throughout, so nobody can attach to a session between
/// the attached set being read and that session being closed.
fn sweep(idle_for: Duration) -> Vec<String> {
    let Ok(open) = bridges().lock() else {
        return Vec::new();
    };
    let Ok(mut registry) = registry().lock() else {
        return Vec::new();
    };
    sweep_in(&open, &mut registry, idle_for)
}

pub(super) fn sweep_in(
    open: &HashMap<String, TerminalBridge>,
    registry: &mut TerminalRegistry,
    idle_for: Duration,
) -> Vec<String> {
    let attached: HashSet<String> = open
        .values()
        .flat_map(|bridge| bridge.attached_ids().cloned())
        .collect();
    registry.reap();
    let closed = registry.reap_idle(&attached, idle_for);
    for id in &closed {
        audit("idle-close", "reaper", Some(id));
    }
    closed
}
