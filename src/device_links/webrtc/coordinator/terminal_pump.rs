//! Terminal envelopes and the pump that sends terminal output with
//! backpressure.
//!
//! Sibling edges: none (the parent's helpers only). Outside: `terminal_bridge`, `terminal_protocol`, `transport`.

use super::*;

/// Queue ceiling for terminal output on one peer.
///
/// libwebrtc tears a data channel down with `RESOURCE_EXHAUSTED` around 16 MiB
/// of queued data, which would take the whole peer connection with it — not
/// just the terminal. Staying two orders of magnitude below that leaves room
/// for the other channels sharing the transport.
const TERMINAL_HIGH_WATER: u64 = 256 * 1024;

/// Interval between output pushes. Mosh measured ~8 ms as the point where a
/// higher rate stops being perceptible, and coalescing at that interval keeps a
/// flooding program from turning into a message storm.
const TERMINAL_PUMP_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8);
/// How long an idle pump sleeps when nothing wakes it.
const TERMINAL_PUMP_IDLE: std::time::Duration = std::time::Duration::from_secs(1);

/// Handles one `desklink-terminal-v1` envelope.
///
/// Everything that matters — the handover and binding gates, the user's
/// per-device consent, the replay cache — lives in
/// [`terminal_bridge::process_envelope`], where it can be tested without a peer
/// connection. Replies are queued on the peer's bridge and go out through
/// [`run_terminal_pump`], the only thing that sends terminal data, so they are
/// ordered with the output and subject to the same backpressure.
pub(super) fn handle_terminal_envelope(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    envelope: WebRtcEnvelope,
    sessions: &Arc<DeviceManager>,
) -> Result<(), String> {
    terminal_bridge::process_envelope(binding, attempt_id, wire, envelope, sessions)
}

/// Encodes `events` and sends them on the terminal channel, in order.
///
/// A message that cannot be encoded — too large for an envelope, say — is
/// dropped and logged: it is one bad message, and stopping the pump for it
/// would silence the terminal for good. A message that cannot be *sent* ends
/// the pump, because the channel itself is gone.
fn send_terminal_events(
    peer: &DesktopWebRtcPeer,
    wire: &WebRtcWireBinding,
    events: Vec<TerminalEvent>,
) -> Result<(), String> {
    for event in events {
        let frame = match terminal_frame(wire, event) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("[DeskLink] Dropped an unsendable terminal message: {error}");
                continue;
            }
        };
        peer.send_text(WebRtcChannel::Terminal, &frame)?;
    }
    Ok(())
}

fn terminal_frame(wire: &WebRtcWireBinding, event: TerminalEvent) -> Result<String, String> {
    let (message_type, payload) = match event {
        TerminalEvent::SendControl(control) => (TERMINAL_CONTROL_MESSAGE_TYPE, control.to_json()?),
        TerminalEvent::SendChunk(chunk) => (TERMINAL_CHUNK_MESSAGE_TYPE, chunk.encode()?),
    };
    let envelope = WebRtcEnvelope::create(
        wire,
        WebRtcChannel::Terminal,
        message_type,
        &payload,
        now_millis(),
    )?;
    String::from_utf8(envelope.to_json()?)
        .map_err(|_| "DeskLink terminal envelope was not UTF-8".to_string())
}

/// Pushes terminal output to one peer until its connection is replaced.
///
/// A dedicated thread on purpose. The peer event worker is a single thread with
/// a 500 ms timeout serving SDP, ICE, file chunks and handover; interactive
/// terminal output there would both feel slow and starve the rest.
///
/// The bridge is claimed *here*, on the worker thread, before the pump thread
/// exists: a message on this connection cannot then arrive ahead of it. The
/// claim's token is what lets the thread's own cleanup end only its own
/// attachments — a phone that reconnects gets a new bridge and a new token, and
/// this thread noticing a moment later must not tear that down.
pub(super) fn spawn_terminal_pump(
    binding: SessionBinding,
    attempt_id: String,
    wire: WebRtcWireBinding,
    sessions: Arc<DeviceManager>,
) {
    let token = terminal_bridge::begin(&wire);
    thread::spawn(move || {
        run_terminal_pump(&binding, &attempt_id, &wire, &sessions, token);
        // Whatever ended the pump, this connection is no longer receiving output
        // and no longer holds the right to type. Leaving the lease behind would
        // lock the next client out of a session nobody is driving.
        terminal_bridge::end(&wire.peer_device_id, token);
    });
}

fn run_terminal_pump(
    binding: &SessionBinding,
    attempt_id: &str,
    wire: &WebRtcWireBinding,
    sessions: &Arc<DeviceManager>,
    token: u64,
) {
    let mut seen = terminal_bridge::wake_generation();
    // The last pump found nothing to send.
    let mut quiet = false;
    loop {
        // Nothing to send (nothing attached and nothing queued, or the last
        // look found nothing): sleep until a request touches the bridge or a
        // terminal writes, resizes or ends (or a second passes, to notice a
        // peer that went away) instead of polling 125 times a second. While
        // output flows it is batched every few milliseconds.
        match terminal_bridge::pump_is_idle(&wire.peer_device_id, token) {
            Some(idle) if idle || quiet => terminal_bridge::wait_for_work(seen, TERMINAL_PUMP_IDLE),
            Some(_) => thread::sleep(TERMINAL_PUMP_INTERVAL),
            None => break,
        }
        // Before looking: anything after this wakes the next wait.
        seen = terminal_bridge::wake_generation();
        quiet = false;
        if !sessions.is_current(binding) {
            break;
        }
        let Ok(peer) = sessions.active_webrtc_peer(binding, attempt_id) else {
            break;
        };
        // Read the queue depth first and send only what fits under the ceiling.
        // Stopping here is what makes the PTY reader stop too: the ring stops
        // being drained, the kernel buffer fills, and the writing program
        // blocks in `write(2)` instead of us buffering it. Replies and screen
        // snapshots are charged against the same budget.
        let buffered = peer.buffered_amount(WebRtcChannel::Terminal);
        let budget = usize::try_from(TERMINAL_HIGH_WATER.saturating_sub(buffered))
            .unwrap_or(usize::MAX)
            .min(terminal_bridge::DEFAULT_PUMP_BUDGET);
        if budget == 0 {
            continue;
        }
        // `None`: a newer connection from this device has claimed the bridge.
        let Some(events) = terminal_bridge::pump_once(&wire.peer_device_id, token, budget) else {
            break;
        };
        if events.is_empty() {
            quiet = true;
            continue;
        }
        if let Err(error) = send_terminal_events(&peer, wire, events) {
            if crate::device_links::transport::should_surface_send_error(&error) {
                eprintln!("[DeskLink] Terminal pump stopped: {error}");
            }
            break;
        }
    }
}
