//! A peer's life: installing the responder, the event worker thread, the
//! `PeerWorkerGuard` that clears an orphaned peer, resetting device state,
//! rebuilding a peer and rebuilding the peers whose addresses left the network.
//!
//! This is the hub of the coordinator on purpose: the event worker is the one
//! thread that sees every `PeerEvent` of a peer, so it is where liveness,
//! recovery, envelopes, files, terminals and the screen share meet. The others
//! do not call back into it (no cycles); keep new per-peer state out of here
//! and in the module that owns it.
//!
//! Sibling edges: `envelopes`, `liveness`, `recovery`, `ice_setup`,
//! `file_link`, `phone_remote`, `desktop_share`, `signing`, `terminal_pump`.
//! Outside: `core::DeviceManager`, `peer_connection`, `screen`, `capture`,
//! `audio_session`, `transfer_board`, `session_lock`, `sync`.

use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) fn install_responder(
    binding: SessionBinding,
    attempt_id: String,
    local_device_id: &str,
    remote_device_id: &str,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
) -> Result<Arc<DesktopWebRtcPeer>, String> {
    let wire = WebRtcWireBinding::from_attempt(local_device_id, remote_device_id, &attempt_id)?;
    let transfer_manager = new_transfer_manager(&config)?;
    let (sender, receiver) = mpsc::channel();
    let peer = DesktopWebRtcPeer::new(false, sender, &ice_config_for(&binding, &config))?;
    let replaced = sessions
        .install_webrtc_peer(
            &binding,
            attempt_id.clone(),
            wire.clone(),
            peer,
            Arc::clone(&transfer_manager),
        )
        .map_err(|error| error.to_string())?;
    if let Some(replaced) = replaced {
        // The new peer cannot be feature-ready yet, so only the replaced
        // peer's device-keyed state goes; its superseded worker skips this.
        reset_device_webrtc_state(&binding.device_id);
        replaced.close();
    }
    // Attached before the worker starts: a worker that ends at once (its
    // attempt already replaced) detaches and stops the manager on its way
    // out, and an attach after that registered lanes for a stopped engine.
    let peer = sessions
        .active_webrtc_peer(&binding, &attempt_id)
        .map_err(|error| error.to_string())?;
    attach_transfer_manager(&transfer_manager, &peer, &wire, Some(Arc::clone(&config)));
    if let Err(error) = spawn_event_worker(
        binding.clone(),
        attempt_id.clone(),
        wire.clone(),
        receiver,
        Arc::clone(&sessions),
        config,
        devices,
        events,
        Arc::clone(&transfer_manager),
    ) {
        transfer_manager.detach();
        transfer_manager.shut_down();
        return Err(error);
    }
    Ok(peer)
}

#[allow(clippy::too_many_arguments)] // Captures one immutable peer/session context.
pub(super) fn spawn_event_worker(
    binding: SessionBinding,
    attempt_id: String,
    wire: WebRtcWireBinding,
    receiver: Receiver<PeerEvent>,
    sessions: Arc<DeviceManager>,
    config: Arc<Mutex<Config>>,
    devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    events: Arc<Mutex<Vec<DaemonEvent>>>,
    transfer_manager: Arc<WebRtcTransferManager>,
) -> Result<(), String> {
    thread::spawn(move || {
        // Held for the worker's whole life; see `PeerWorkerGuard`.
        let _guard = PeerWorkerGuard {
            binding: binding.clone(),
            attempt_id: attempt_id.clone(),
            sessions: Arc::clone(&sessions),
        };
        let mut disconnected_since: Option<std::time::Instant> = None;
        let mut next_remote_heartbeat =
            std::time::Instant::now() + remote_session::HEARTBEAT_INTERVAL;
        // Data-channel liveness clocks; see `PEER_DEAD_AFTER`. They only count
        // once the handover is feature-ready, so slow ICE is not mistaken for
        // a dead peer — the ICE state machine owns that phase.
        let mut last_inbound = std::time::Instant::now();
        let mut last_probe = std::time::Instant::now();
        let mut was_ready = false;
        let mut ever_ready = false;
        // Since when this peer has been feature-ready (the ladder resets
        // only after it stays so for [`HEALTHY_AFTER`]).
        let mut ready_at: Option<std::time::Instant> = None;
        // File and terminal messages are handled on their own thread, in
        // order, so a heavy one never holds up the liveness clocks and the
        // control traffic this worker serves.
        let lane = match spawn_work_lane() {
            Ok(lane) => lane,
            Err(error) => {
                // Without its lane the peer would drop every file and
                // terminal message while reading as connected: torn down,
                // so the ladder (and the guard) take over.
                eprintln!("[DeskLink] {error}");
                rebuild_peer(
                    &binding,
                    &attempt_id,
                    error,
                    &sessions,
                    &config,
                    &devices,
                    &events,
                );
                return;
            }
        };
        let started = std::time::Instant::now();
        loop {
            // A timed receive lets the transient-disconnect grace window expire
            // even when the peer has gone completely silent.
            let event = match receiver.recv_timeout(std::time::Duration::from_millis(500)) {
                Ok(event) => Some(event),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            };
            if !sessions.is_current(&binding)
                || sessions.active_webrtc_peer(&binding, &attempt_id).is_err()
            {
                break;
            }
            expire_remote_requests(&binding, &attempt_id, &wire, &sessions);
            if std::time::Instant::now() >= next_remote_heartbeat {
                if let Err(error) = send_remote_heartbeat(&binding, &attempt_id, &wire, &sessions) {
                    eprintln!("[DeskLink] Remote-control heartbeat failed: {error}");
                }
                next_remote_heartbeat =
                    std::time::Instant::now() + remote_session::HEARTBEAT_INTERVAL;
            }
            if matches!(
                event,
                Some(PeerEvent::Envelope { .. } | PeerEvent::Binary { .. })
            ) {
                last_inbound = std::time::Instant::now();
                note_data_inbound(&binding.device_id);
                crate::device_links::daemon::state::clear_quiet(&devices, &binding.device_id);
            }
            let ready = sessions
                .webrtc_features_ready(&binding, &attempt_id)
                .unwrap_or(false);
            if ready && !was_ready {
                // Start the silence budget at the moment features go live.
                last_inbound = std::time::Instant::now();
                note_data_inbound(&binding.device_id);
            }
            if ready && !was_ready {
                clear_leaving(&binding.device_id);
                ready_at = Some(std::time::Instant::now());
            } else if !ready {
                ready_at = None;
            }
            if ladder_resets(ready_at.map(|at| at.elapsed())) {
                // Healthy for a minute: the next failure starts from the top.
                ready_at = Some(std::time::Instant::now());
                let _ = sessions.with_session(&binding, |session| {
                    session.webrtc_recovery_attempt = 0;
                });
            }
            was_ready = ready;
            ever_ready |= ready;
            // Nothing else ends a negotiation that stalls before feature-ready:
            // a lost offer or answer, a failed set-description or a lost hello
            // left webrtcbin in `checking` forever while LAN heartbeats kept
            // the session looking healthy. Rebuilding sends `RestartRequest`
            // when this side is the responder, so either role recovers.
            if !ever_ready && started.elapsed() >= NEGOTIATION_TIMEOUT {
                rebuild_peer(
                    &binding,
                    &attempt_id,
                    format!(
                        "WebRTC negotiation not feature-ready after {}s",
                        NEGOTIATION_TIMEOUT.as_secs()
                    ),
                    &sessions,
                    &config,
                    &devices,
                    &events,
                );
                break;
            }
            if ready {
                let dozing = is_dozing(&binding.device_id);
                let woke = take_probe_now(&binding.device_id);
                last_inbound = clock_after_wake(woke, last_inbound, std::time::Instant::now());
                if woke {
                    let _ = sessions.with_session(&binding, |session| {
                        session.webrtc_recovery_attempt = 0;
                    });
                    last_probe = std::time::Instant::now();
                    if let Err(error) =
                        send_keepalive(&binding, &attempt_id, &wire, &sessions, false)
                    {
                        eprintln!("[DeskLink] WebRTC wake probe failed: {error}");
                    }
                }
                if take_probe_asap(&binding.device_id) {
                    last_probe = std::time::Instant::now();
                    if let Err(error) =
                        send_keepalive(&binding, &attempt_id, &wire, &sessions, false)
                    {
                        eprintln!("[DeskLink] WebRTC asked probe failed: {error}");
                    }
                }
                let leaving = is_leaving(&binding.device_id);
                // `None`: the LAN socket is closed (the retained-FIN case).
                let lan = (!binding.link.is_closed())
                    .then(|| sessions.silence(&binding.device_id))
                    .flatten();
                let silence = effective_silence(last_inbound.elapsed(), lan, dozing);
                let dead_fast = fast_dead(
                    last_inbound.elapsed(),
                    lan,
                    probe_unanswered(last_probe > last_inbound, last_probe.elapsed()),
                    dozing,
                    leaving,
                );
                let verdict = if dead_fast {
                    Liveness::Dead
                } else {
                    liveness(silence, last_probe.elapsed(), dozing)
                };
                match verdict {
                    Liveness::Fine => {}
                    Liveness::Probe => {
                        last_probe = std::time::Instant::now();
                        debug_log!(
                            "[DL-LIVE] {} probe {}: data silent {:?}, lan {:?}",
                            stamp(),
                            binding.device_id,
                            last_inbound.elapsed(),
                            lan
                        );
                        if let Err(error) =
                            send_keepalive(&binding, &attempt_id, &wire, &sessions, false)
                        {
                            eprintln!("[DeskLink] WebRTC heartbeat failed: {error}");
                        }
                    }
                    Liveness::Dead => {
                        debug_log!(
                            "[DL-LIVE] {} dead {}: data silent {:?}, lan {:?}, fast {dead_fast}",
                            stamp(),
                            binding.device_id,
                            last_inbound.elapsed(),
                            lan
                        );
                        rebuild_peer(
                            &binding,
                            &attempt_id,
                            format!(
                                "WebRTC peer silent for {}s on every data channel{}",
                                last_inbound.elapsed().as_secs(),
                                if dozing {
                                    " (dozing)"
                                } else if dead_fast {
                                    " (corroborated)"
                                } else {
                                    ""
                                }
                            ),
                            &sessions,
                            &config,
                            &devices,
                            &events,
                        );
                        break;
                    }
                }
            }
            if let Some(event) = event {
                match event {
                    PeerEvent::LocalDescription { message_type, sdp } => {
                        #[cfg(debug_assertions)]
                        for line in sdp.lines().filter(|line| {
                            line.starts_with("m=")
                                || line.starts_with("a=extmap")
                                || line.starts_with("a=rtcp-fb")
                                || line.starts_with("a=rtpmap")
                                || line.starts_with("a=sendrecv")
                                || line.starts_with("a=recvonly")
                                || line.starts_with("a=sendonly")
                                || line.starts_with("a=inactive")
                                || line.starts_with("a=msid")
                                || line.starts_with("a=ssrc")
                                || line.starts_with("a=mid")
                        }) {
                            debug_log!("[DL-SDP] local {message_type:?}: {line}");
                        }
                        if let Err(error) =
                            sessions.with_webrtc_handover(&binding, &attempt_id, |handover| {
                                match message_type {
                                    SignalingMessageType::Offer => {
                                        handover.record_offer(sdp.clone())
                                    }
                                    SignalingMessageType::Answer => {
                                        handover.record_answer(sdp.clone())
                                    }
                                    _ => unreachable!("peer only creates SDP offers and answers"),
                                }
                            })
                        {
                            eprintln!("[DeskLink] WebRTC local SDP rejected: {error}");
                            break;
                        }
                        let mut payload = Map::new();
                        payload.insert("sdp".to_string(), Value::String(sdp));
                        if let Err(error) = send_signed_signal(
                            &binding,
                            &wire,
                            &attempt_id,
                            message_type,
                            payload,
                            &config,
                        ) {
                            // Never the end of the worker (see "Signal send failures").
                            eprintln!("[DeskLink] WebRTC local description failed: {error}");
                        }
                    }
                    PeerEvent::IceCandidate {
                        sdp_m_line_index,
                        candidate,
                    } => {
                        let mut payload = Map::new();
                        payload.insert(
                            "sdpMLineIndex".to_string(),
                            Value::from(i64::from(sdp_m_line_index)),
                        );
                        payload.insert("candidate".to_string(), Value::String(candidate));
                        if let Err(error) = send_signed_signal(
                            &binding,
                            &wire,
                            &attempt_id,
                            SignalingMessageType::IceCandidate,
                            payload,
                            &config,
                        ) {
                            // Never the end of the worker (see "Signal send failures").
                            eprintln!("[DeskLink] WebRTC ICE signaling failed: {error}");
                        }
                    }
                    PeerEvent::EndOfCandidates => {
                        let _ = send_signed_signal(
                            &binding,
                            &wire,
                            &attempt_id,
                            SignalingMessageType::EndOfCandidates,
                            Map::new(),
                            &config,
                        );
                    }
                    PeerEvent::Envelope {
                        channel: channel @ (WebRtcChannel::FileControl | WebRtcChannel::Terminal),
                        bytes,
                        credit,
                    } => {
                        let job: WorkJob = {
                            let (binding, attempt_id, wire) =
                                (binding.clone(), attempt_id.clone(), wire.clone());
                            let (sessions, config, devices, events, transfer_manager) = (
                                Arc::clone(&sessions),
                                Arc::clone(&config),
                                Arc::clone(&devices),
                                Arc::clone(&events),
                                Arc::clone(&transfer_manager),
                            );
                            Box::new(move || {
                                // Held until handled: the backlog budget counts it.
                                let _credit = credit;
                                if let Err(error) = handle_peer_envelope(
                                    &binding,
                                    &attempt_id,
                                    &wire,
                                    channel,
                                    &bytes,
                                    &sessions,
                                    &config,
                                    &devices,
                                    &events,
                                    &transfer_manager,
                                ) {
                                    crate::device_links::limits::log_limited(
                                        "rejected-envelope",
                                        &format!("Rejected WebRTC data-channel message: {error}"),
                                    );
                                }
                            })
                        };
                        if lane.send(job).is_err() {
                            eprintln!("[DeskLink] The peer's work thread has stopped");
                            rebuild_peer(
                                &binding,
                                &attempt_id,
                                "The peer's work thread stopped".to_string(),
                                &sessions,
                                &config,
                                &devices,
                                &events,
                            );
                            break;
                        }
                    }
                    PeerEvent::Envelope { channel, bytes, .. } => {
                        if let Err(error) = handle_peer_envelope(
                            &binding,
                            &attempt_id,
                            &wire,
                            channel,
                            &bytes,
                            &sessions,
                            &config,
                            &devices,
                            &events,
                            &transfer_manager,
                        ) {
                            crate::device_links::limits::log_limited(
                                "rejected-envelope",
                                &format!("Rejected WebRTC data-channel message: {error}"),
                            );
                        }
                    }
                    PeerEvent::Binary {
                        channel: WebRtcChannel::FileData,
                        bytes,
                        ..
                    } => {
                        // A frame of file data: queued for the file thread,
                        // which writes and hashes it. Nothing else happens here.
                        if sessions
                            .webrtc_features_ready(&binding, &attempt_id)
                            .unwrap_or(false)
                        {
                            transfer_manager.on_frame(bytes);
                        }
                    }
                    PeerEvent::Binary { channel, bytes, .. } => {
                        crate::device_links::limits::log_limited(
                            "rejected-binary",
                            &format!(
                                "Rejected raw WebRTC binary payload on {} ({} bytes)",
                                channel.label(),
                                bytes.len(),
                            ),
                        );
                    }
                    PeerEvent::ScreenStream(stream) => {
                        debug_log!(
                            "[DeskLink] Screen stream received from {}",
                            binding.device_id
                        );
                        crate::device_links::webrtc::screen::publish_stream(
                            &binding.device_id,
                            stream,
                        );
                    }
                    // Most peer errors are not fatal (an oversized candidate, an
                    // empty message); a negotiation that truly cannot finish is
                    // caught by NEGOTIATION_TIMEOUT instead.
                    PeerEvent::Error(error) => eprintln!("[DeskLink] WebRTC peer error: {error}"),
                    PeerEvent::ConnectionChanged(state) => {
                        debug_log!("[DeskLink] WebRTC peer state: {state}");
                        match classify_peer_state(&state) {
                            PeerStateAction::Ignore => {
                                if state.to_ascii_lowercase().contains("connected") {
                                    disconnected_since = None;
                                }
                            }
                            PeerStateAction::WaitForRecovery => {
                                if disconnected_since.is_none() {
                                    debug_log!(
                                    "[DeskLink] WebRTC peer disconnected; waiting {}s for it to recover",
                                    TRANSIENT_DISCONNECT_GRACE.as_secs()
                                );
                                    disconnected_since = Some(std::time::Instant::now());
                                }
                            }
                            PeerStateAction::Rebuild => {
                                rebuild_peer(
                                    &binding,
                                    &attempt_id,
                                    format!("WebRTC peer entered terminal state: {state}"),
                                    &sessions,
                                    &config,
                                    &devices,
                                    &events,
                                );
                                break;
                            }
                        }
                    }
                    PeerEvent::ChannelOpened(WebRtcChannel::Control) => {
                        if let Err(error) = begin_handover(&binding, &attempt_id, &wire, &sessions)
                        {
                            eprintln!("[DeskLink] Could not start WebRTC authentication: {error}");
                        }
                    }
                    PeerEvent::ChannelOpened(WebRtcChannel::Terminal) => {
                        // Terminal output gets its own thread; this worker is a
                        // single thread with a 500 ms timeout serving every
                        // other channel too.
                        spawn_terminal_pump(
                            binding.clone(),
                            attempt_id.clone(),
                            wire.clone(),
                            Arc::clone(&sessions),
                        );
                    }
                    PeerEvent::ChannelOpened(opened)
                        if WebRtcChannel::REQUIRED.contains(&opened) =>
                    {
                        // The handover may have been waiting for this
                        // channel: send our `feature-ready` now, and if the
                        // phone's already came, features are live.
                        match send_feature_ready_if_possible(
                            &binding,
                            &attempt_id,
                            &wire,
                            &sessions,
                        ) {
                            Ok(true) => {
                                if sessions
                                    .webrtc_features_ready(&binding, &attempt_id)
                                    .unwrap_or(false)
                                {
                                    finish_feature_ready(
                                        &binding,
                                        &sessions,
                                        &devices,
                                        &transfer_manager,
                                    );
                                }
                            }
                            Ok(false) => {}
                            Err(error) => {
                                eprintln!("[DeskLink] Could not send feature-ready: {error}")
                            }
                        }
                    }
                    PeerEvent::ChannelOpened(_) => {}
                    PeerEvent::Closed => break,
                }
            }

            // Evaluated every iteration, after any event has been applied.
            // Checking it only when the receive times out let a peer that keeps
            // emitting events — a failing SCTP association produces a steady
            // stream of channel errors — defer the window forever, so a dead
            // peer was never rebuilt. Applying the event first is what lets a
            // recovery notification on the expiry tick clear the timer instead
            // of being discarded with the peer it reported healthy.
            if expired(disconnected_since) {
                rebuild_peer(
                    &binding,
                    &attempt_id,
                    "WebRTC peer did not recover from a transient disconnect".to_string(),
                    &sessions,
                    &config,
                    &devices,
                    &events,
                );
                break;
            }
        }

        // Nothing services this peer's events once the worker exits, so it must
        // not be left installed. An orphaned peer is worse than no peer: every
        // inbound feature is dropped in silence, and because the handover still
        // reports feature-ready, `wants_bootstrap_connection` stays false — so
        // discovery never dials a replacement and an inbound socket is
        // discarded too. The device then reads as connected forever.
        //
        // Doing it here rather than at each `break` makes that structurally
        // impossible instead of a property of every exit path remembering to.
        // Exits that already cleared their attempt get `StaleBinding` back,
        // which is a no-op.
        //
        // The transfers ride this peer: receivers checkpoint now (a resume
        // loses less than a block), and nothing is sent into a closed channel.
        transfer_manager.detach();
        // And its thread ends with it, whoever else still holds the manager.
        transfer_manager.shut_down();
    });
    Ok(())
}

/// Ends everything keyed by device rather than by attempt: desktop screen
/// share, the remote-control session, an open phone-file dialog and transfer
/// rows. Run when a peer goes away for good, and when a new peer replaces a
/// live one (the replaced worker skips it, see `PeerWorkerGuard`).
pub(super) fn reset_device_webrtc_state(device_id: &str) {
    stop_desktop_portal_session(device_id, None);
    // What the phone proved to the session password belonged to this peer.
    crate::device_links::session_lock::forget_unlock(device_id);
    // A lost or replaced peer ends every sound session (the phone asks
    // again on its next link).
    crate::device_links::webrtc::audio_session::end_all(device_id);
    crate::device_links::webrtc::capture::end(device_id);
    // What the phone said it had belongs to the connection that ended.
    crate::device_links::sync::forget_connection(device_id);
    // The session state must not outlive its peer: a stale `Controlling`
    // lease survived a rebuild, and this side's own heartbeats kept it alive
    // while the UI read "Control active". Stopped before the picture goes,
    // so a picture being shown again meanwhile sees it and goes too.
    if let Ok(mut open) = remote_session::sessions().lock() {
        if let Some(session) = open.get_mut(device_id) {
            session.stop();
        }
    }
    crate::device_links::webrtc::screen::clear_stream(device_id);
    crate::device_links::webrtc::screen::forget_parked(device_id);
    // Transfers ride this peer. Both sides keep their checkpoints and the
    // phone resumes them when it reconnects (this side re-offers its own
    // sends): shown as paused, not as failures.
    crate::device_links::webrtc::transfer_board::pause_device(device_id);
}

/// Releases a WebRTC peer when its event worker stops, however it stops.
///
/// Nothing services a peer's events once the worker is gone, so it must not be
/// left installed. An orphaned peer is worse than no peer: every inbound
/// feature is dropped in silence, and because the handover still reports
/// feature-ready, `wants_bootstrap_connection` stays false — so discovery never
/// dials a replacement and an inbound socket is discarded too. The device then
/// reads as connected forever.
///
/// This is a `Drop` guard rather than a statement after the loop so the
/// guarantee also covers an unwinding panic. It is not hypothetical:
/// `thread::spawn` panics when the process hits its thread limit, and the lock
/// handler spawns from inside this worker.
///
/// Exits that already cleared their own attempt get `StaleBinding` back here,
/// which is a no-op — including after `rebuild_peer`, whose scheduled retry
/// therefore cannot be cancelled by this.
pub(super) struct PeerWorkerGuard {
    pub(super) binding: SessionBinding,
    pub(super) attempt_id: String,
    pub(super) sessions: Arc<DeviceManager>,
}

impl Drop for PeerWorkerGuard {
    fn drop(&mut self) {
        // The teardown below is keyed by device, not attempt. When a fresh
        // offer has already installed a newer peer, running it would tear down
        // that peer's screen share, remote session and transfers — so a
        // superseded worker only releases its own (already replaced) peer.
        let superseded = matches!(
            self.sessions.active_webrtc_attempt(&self.binding),
            Ok(Some(ref active)) if active != &self.attempt_id
        );
        if superseded {
            return;
        }
        reset_device_webrtc_state(&self.binding.device_id);
        if let Ok(Some(peer)) = self.sessions.clear_webrtc_if_current(
            &self.binding,
            &self.attempt_id,
            "The WebRTC event worker ended".to_string(),
        ) {
            debug_log!("[DeskLink] WebRTC worker ended; released its peer");
            peer.close();
        }
    }
}

/// Clears a dead peer and schedules one backed-off replacement attempt.
///
/// The wait happens on a detached thread so a GStreamer callback thread is
/// never blocked, and the attempt is re-validated against the current session
/// generation after the delay: a socket replacement during the wait supersedes
/// this recovery rather than racing it.
#[allow(clippy::too_many_arguments)]
pub(super) fn rebuild_peer(
    binding: &SessionBinding,
    attempt_id: &str,
    reason: String,
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    match sessions.clear_webrtc_if_current(binding, attempt_id, reason.clone()) {
        Ok(Some(peer)) => {
            note_channels_of(&binding.device_id, &peer);
            peer.close();
        }
        Ok(None) => {}
        Err(error) => {
            eprintln!("[DeskLink] Ignored stale WebRTC peer failure: {error}");
            return;
        }
    }

    // The counter lives on the session, not on this worker: each rebuild ends
    // its worker and starts a new one, so a worker-local counter would reset
    // the backoff to zero on every single failure.
    let recovery_attempt = sessions
        .with_session(binding, |session| {
            let attempt = session.webrtc_recovery_attempt;
            session.webrtc_recovery_attempt = attempt.saturating_add(1);
            attempt
        })
        .unwrap_or(0);
    let Some(delay) = recovery_delay(recovery_attempt) else {
        // The ladder is exhausted. Stop dialling and say so, rather than
        // retrying every 30 seconds forever behind a UI that reads
        // "Connecting". Inbound offers are still accepted — the phone may well
        // be the deterministic initiator — and one resets this counter.
        debug_log!(
            "[DeskLink] {reason}; giving up after {} attempts. Waiting for the peer to offer.",
            recovery_attempt
        );
        mark_error(
            devices,
            &binding.device_id,
            exhausted_words(
                &binding.device_id,
                &device_name(devices, &binding.device_id),
            ),
        );
        return;
    };
    debug_log!(
        "[DeskLink] {reason}; rebuilding the WebRTC peer in {}ms (attempt {})",
        delay.as_millis(),
        recovery_attempt + 1
    );

    let binding = binding.clone();
    let sessions = Arc::clone(sessions);
    let config = Arc::clone(config);
    let devices = Arc::clone(devices);
    let events = Arc::clone(events);
    thread::spawn(move || {
        thread::sleep(delay);
        if !sessions.is_current(&binding) {
            debug_log!("[DeskLink] WebRTC recovery superseded by a newer session generation");
            return;
        }
        if sessions
            .active_webrtc_attempt(&binding)
            .ok()
            .flatten()
            .is_some()
        {
            return;
        }
        if let Err(error) = start_initiator(binding, sessions, config, devices, events) {
            eprintln!("[DeskLink] WebRTC recovery attempt failed: {error}");
        }
    });
}

/// The network changed: rebuilds every peer whose local addresses are all
/// gone (its ICE candidates can never work again), instead of waiting for
/// silence to bury it.
pub(crate) fn rebuild_peers_off_the_network(
    present: &[std::net::IpAddr],
    sessions: &Arc<DeviceManager>,
    config: &Arc<Mutex<Config>>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    events: &Arc<Mutex<Vec<DaemonEvent>>>,
) {
    for binding in sessions.current_bindings() {
        let used = ice_addresses()
            .lock()
            .ok()
            .and_then(|used| used.get(&binding.device_id).cloned())
            .unwrap_or_default();
        if !addresses_vanished(&used, present) {
            continue;
        }
        let Ok(Some(attempt_id)) = sessions.active_webrtc_attempt(&binding) else {
            continue;
        };
        rebuild_peer(
            &binding,
            &attempt_id,
            "This computer's network changed".to_string(),
            sessions,
            config,
            devices,
            events,
        );
    }
}
