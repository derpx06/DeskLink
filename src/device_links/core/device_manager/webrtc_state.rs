//! The registry's WebRTC state (`impl DeviceManager`): installing the one
//! current peer, readiness, the active attempt, the feature queue before
//! feature-ready, handover state, and taking signals and envelopes once.
//!
//! Sibling edges: `outcomes`. Outside: `webrtc` (the peer, handover, replay
//! cache).

use super::*;

impl DeviceManager {
    /// Atomically installs the only current WebRTC peer for a current DeskLink
    /// session. The caller closes a replaced peer after this manager lock is
    /// released so GStreamer callbacks cannot deadlock the session core.
    pub fn install_webrtc_peer(
        &self,
        binding: &SessionBinding,
        attempt_id: String,
        wire_binding: WebRtcWireBinding,
        peer: DesktopWebRtcPeer,
        transfer_manager: Arc<WebRtcTransferManager>,
    ) -> Result<Option<Arc<DesktopWebRtcPeer>>, SessionError> {
        match self.install_webrtc_peer_with(
            binding,
            attempt_id,
            wire_binding,
            peer,
            transfer_manager,
            false,
        )? {
            Installed::Replaced(replaced) => Ok(replaced),
            Installed::Occupied(_) => unreachable!("a replacing install is never refused"),
        }
    }

    /// Like [`Self::install_webrtc_peer`], but only if the session has no peer
    /// yet, decided under the same lock as the install: two starters (a wake, a
    /// beacon and a network change within milliseconds) can both have seen "no
    /// peer", and the second used to replace the first's half-built pipeline.
    /// A refused install gives the peer back for the caller to close.
    pub fn install_webrtc_peer_if_none(
        &self,
        binding: &SessionBinding,
        attempt_id: String,
        wire_binding: WebRtcWireBinding,
        peer: DesktopWebRtcPeer,
        transfer_manager: Arc<WebRtcTransferManager>,
    ) -> Result<Installed, SessionError> {
        self.install_webrtc_peer_with(
            binding,
            attempt_id,
            wire_binding,
            peer,
            transfer_manager,
            true,
        )
    }

    fn install_webrtc_peer_with(
        &self,
        binding: &SessionBinding,
        attempt_id: String,
        wire_binding: WebRtcWireBinding,
        peer: DesktopWebRtcPeer,
        transfer_manager: Arc<WebRtcTransferManager>,
        only_if_none: bool,
    ) -> Result<Installed, SessionError> {
        self.with_session(binding, |session| {
            if session.pairing.state != crate::device_links::pairing::PairState::Paired {
                return Err(SessionError::NotPaired);
            }
            if only_if_none && session.active_webrtc.is_some() {
                return Ok(Installed::Occupied(peer));
            }
            if session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready)
            {
                crate::device_links::connection_log::note_ready(&session.device_id, false);
            }
            let replaced = session.active_webrtc.replace(Arc::new(peer));
            session.transfer_manager = Some(transfer_manager);
            session.webrtc_handover = Some(HandoverRuntime::new(attempt_id.clone(), wire_binding));
            session.webrtc_attempt_id = Some(attempt_id);
            session.seen_webrtc_request_ids.clear();
            session.seen_webrtc_message_ids.clear();
            Ok(Installed::Replaced(replaced))
        })?
    }

    /// The feature-ready WebRTC peer for `device_id`, and whether its
    /// bootstrap link is the relay (so a host-to-host pair is an internet
    /// IPv6 path, not the LAN).
    pub fn ready_peer(&self, device_id: &str) -> Option<(Arc<DesktopWebRtcPeer>, bool)> {
        let sessions = self.sessions.lock().ok()?;
        let session = sessions.get(device_id)?;
        if !session
            .webrtc_handover
            .as_ref()
            .is_some_and(HandoverRuntime::features_ready)
        {
            return None;
        }
        let via_relay = session
            .active_link
            .as_ref()
            .is_some_and(|link| link.local_ip().is_none());
        Some((Arc::clone(session.active_webrtc.as_ref()?), via_relay))
    }

    pub fn active_webrtc_peer(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
    ) -> Result<Arc<DesktopWebRtcPeer>, SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            session
                .active_webrtc
                .as_ref()
                .cloned()
                .ok_or(SessionError::UnknownSession)
        })?
    }

    /// Returns the signed attempt currently associated with this transport
    /// generation.  Bootstrap SDP/ICE messages must never replace it merely
    /// because they arrived later on a retried TLS socket.
    pub fn active_webrtc_attempt(
        &self,
        binding: &SessionBinding,
    ) -> Result<Option<String>, SessionError> {
        self.with_session(binding, |session| Ok(session.webrtc_attempt_id.clone()))?
    }

    /// Reads the handover state from the session owner. A false value is a
    /// deliberate safe default: LAN feature traffic remains available only
    /// until both peers have completed their authenticated handover.
    #[allow(dead_code)] // Consumed by the shared feature-dispatcher slice.
    pub fn webrtc_features_ready(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
    ) -> Result<bool, SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            Ok(session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready))
        })?
    }

    /// Invalidates one WebRTC attempt without dropping the authenticated LAN
    /// bootstrap session. Only the current attempt may clear the peer; stale
    /// GStreamer callbacks receive `StaleBinding` and cannot affect a newer
    /// generation.
    pub fn clear_webrtc_if_current(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        reason: String,
    ) -> Result<Option<Arc<DesktopWebRtcPeer>>, SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            session.last_disconnect_reason = Some(reason);
            Ok(session.clear_webrtc_state())
        })?
    }

    /// Drops the pairing state of a device that has no live link, so an unpair
    /// done while it was offline cannot be undone by its session on return.
    /// Returns the session's WebRTC peer, if any, for the caller to close
    /// outside this lock — a revoked pairing must not leave a peer running.
    pub fn forget_pairing(&self, device_id: &str) -> Option<Arc<DesktopWebRtcPeer>> {
        let mut sessions = self.sessions.lock().ok()?;
        let session = sessions.get_mut(device_id)?;
        session.pairing = PairingHandler::new(false);
        session.pending_features.clear();
        session.clear_webrtc_state()
    }

    /// Holds a feature until WebRTC is ready, evicting the oldest past
    /// `PENDING_FEATURE_LIMIT`. Returns `false` without queueing when the
    /// handover is already feature-ready: the check and the push share one
    /// lock, so a packet can never land in the queue after the flush ran.
    pub fn queue_feature(
        &self,
        binding: &SessionBinding,
        packet: crate::device_links::packet::NetworkPacket,
    ) -> Result<bool, SessionError> {
        self.with_session(binding, |session| {
            if session
                .webrtc_handover
                .as_ref()
                .is_some_and(HandoverRuntime::features_ready)
            {
                return false;
            }
            push_pending(session, Instant::now(), packet);
            true
        })
    }

    /// Queues a feature for a link that is feature-ready but quiet (silent on
    /// both paths for a few seconds): the same bounded queue (64 entries,
    /// 30 s, in order), drained by the daemon's tick once the phone answers
    /// again, or by the next feature-ready edge if it was rebuilt instead.
    pub fn queue_feature_while_quiet(
        &self,
        binding: &SessionBinding,
        packet: crate::device_links::packet::NetworkPacket,
    ) -> Result<(), SessionError> {
        self.with_session(binding, |session| {
            push_pending(session, Instant::now(), packet);
        })
    }

    /// Puts entries a flush could not send back at the FRONT, in order, with
    /// their original times (the 30 s bound counts from the first queueing).
    pub fn requeue_front(
        &self,
        binding: &SessionBinding,
        entries: Vec<(Instant, crate::device_links::packet::NetworkPacket)>,
    ) -> Result<(), SessionError> {
        self.with_session(binding, |session| {
            for entry in entries.into_iter().rev() {
                session.pending_features.push_front(entry);
                if session.pending_features.len()
                    > crate::device_links::core::device_session::PENDING_FEATURE_LIMIT
                {
                    // Newer entries are the ones that go (the flush's entries are older).
                    session.pending_features.pop_back();
                }
            }
        })
    }

    /// Whether anything is queued for this session (a send that finds a queue
    /// goes behind it, so order holds).
    pub fn has_pending_features(&self, binding: &SessionBinding) -> Result<bool, SessionError> {
        self.with_session(binding, |session| !session.pending_features.is_empty())
    }

    /// Whether a feature may be queued for the handover now: one is under
    /// way and younger than the negotiation timeout, or there is none yet and
    /// the bootstrap link is open. A stalled attempt, or a closed link with
    /// no attempt, is a phone that is not coming: say so, do not queue. A stale
    /// binding is the session error, as for every other send (recovery).
    pub fn handover_window_open(&self, binding: &SessionBinding) -> Result<bool, SessionError> {
        self.with_session(binding, |session| match session.webrtc_handover.as_ref() {
            Some(handover) => {
                handover.started_at.elapsed()
                    < crate::device_links::webrtc::coordinator::NEGOTIATION_TIMEOUT
            }
            None => session
                .active_link
                .as_ref()
                .is_some_and(|link| !link.is_closed()),
        })
    }

    /// [`Self::take_pending_features_stamped`] without the times.
    #[cfg(test)]
    pub fn take_pending_features(
        &self,
        binding: &SessionBinding,
    ) -> Vec<crate::device_links::packet::NetworkPacket> {
        self.take_pending_features_stamped(binding)
            .into_iter()
            .map(|(_, packet)| packet)
            .collect()
    }

    /// Drains the queue with each entry's first queueing time, dropping
    /// anything older than `PENDING_FEATURE_MAX_AGE`.
    pub fn take_pending_features_stamped(
        &self,
        binding: &SessionBinding,
    ) -> Vec<(Instant, crate::device_links::packet::NetworkPacket)> {
        self.with_session(binding, |session| {
            session
                .pending_features
                .drain(..)
                .filter(|(queued_at, packet)| {
                    let fresh = queued_at.elapsed()
                        <= crate::device_links::core::device_session::PENDING_FEATURE_MAX_AGE;
                    if !fresh {
                        debug_log!("[DeskLink] Dropped stale queued {}", packet.packet_type);
                    }
                    fresh
                })
                .collect()
        })
        .unwrap_or_default()
    }

    /// Revokes the local pairing state and every WebRTC object derived from
    /// it.  The returned peer is deliberately closed by the caller only after
    /// the session lock has been released, because peer callbacks can trigger
    /// lifecycle work.
    pub fn revoke_pairing(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> Result<
        (
            crate::device_links::packet::NetworkPacket,
            Option<Arc<DesktopWebRtcPeer>>,
        ),
        SessionError,
    > {
        self.with_session(binding, |session| {
            let packet = session.pairing.reject_packet();
            session.last_disconnect_reason = Some(reason);
            session.pending_features.clear();
            (packet, session.clear_webrtc_state())
        })
    }

    /// Serializes all mutations of the WebRTC handover runtime through the
    /// authoritative session. It prevents a callback from a replaced peer
    /// generation from changing current transport policy.
    pub fn with_webrtc_handover<R>(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        operation: impl FnOnce(&mut HandoverRuntime) -> Result<R, String>,
    ) -> Result<R, String> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err("DeskLink WebRTC session was replaced".to_string());
            }
            let handover = session
                .webrtc_handover
                .as_mut()
                .ok_or_else(|| "DeskLink WebRTC handover is not initialized".to_string())?;
            operation(handover)
        })
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
    }

    /// Returns the active peer only once for a signed signaling request ID.
    /// The bounded replay cache belongs to the session rather than a packet
    /// reader so duplicate LAN links cannot bypass it.
    pub fn accept_webrtc_signal(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        request_id: String,
    ) -> Result<Arc<DesktopWebRtcPeer>, SessionError> {
        self.with_session(binding, |session| {
            if session.pairing.state != crate::device_links::pairing::PairState::Paired {
                return Err(SessionError::NotPaired);
            }
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            if !session.seen_webrtc_request_ids.accept(request_id) {
                return Err(SessionError::ReplayDetected);
            }
            session
                .active_webrtc
                .as_ref()
                .cloned()
                .ok_or(SessionError::UnknownSession)
        })?
    }

    /// Takes a signed signaling request id once per session, before anything
    /// is done for it (offers and restart requests included).
    pub fn note_signal_request(
        &self,
        binding: &SessionBinding,
        request_id: String,
    ) -> Result<(), SessionError> {
        self.with_session(binding, |session| {
            if session.pairing.state != crate::device_links::pairing::PairState::Paired {
                return Err(SessionError::NotPaired);
            }
            if !session.seen_signal_request_ids.accept(request_id) {
                return Err(SessionError::ReplayDetected);
            }
            Ok(())
        })?
    }

    /// Accepts a data-channel envelope message ID exactly once for the active
    /// WebRTC attempt. The cache is session-owned so a stale peer callback
    /// cannot bypass replay protection after a link replacement.
    pub fn accept_webrtc_envelope(
        &self,
        binding: &SessionBinding,
        attempt_id: &str,
        message_id: String,
    ) -> Result<(), SessionError> {
        self.with_session(binding, |session| {
            if session.webrtc_attempt_id.as_deref() != Some(attempt_id) {
                return Err(SessionError::StaleBinding);
            }
            if !session.seen_webrtc_message_ids.accept(message_id) {
                return Err(SessionError::ReplayDetected);
            }
            Ok(())
        })?
    }
}

/// The one place the pending queue grows: bounded to `PENDING_FEATURE_LIMIT`
/// (the oldest goes, and is logged), in order.
fn push_pending(
    session: &mut crate::device_links::core::device_session::DeviceSession,
    at: Instant,
    packet: crate::device_links::packet::NetworkPacket,
) {
    if session.pending_features.len()
        >= crate::device_links::core::device_session::PENDING_FEATURE_LIMIT
    {
        if let Some((_, dropped)) = session.pending_features.pop_front() {
            debug_log!(
                "[DeskLink] Queue full: dropped the oldest queued {}",
                dropped.packet_type
            );
        }
    }
    session.pending_features.push_back((at, packet));
}
