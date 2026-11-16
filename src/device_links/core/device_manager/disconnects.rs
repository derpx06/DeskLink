//! The registry's disconnect paths (`impl DeviceManager`): a current
//! generation disconnecting, the bootstrap link or its reader going away
//! (a feature-ready generation survives its bootstrap socket).
//!
//! Sibling edges: `outcomes`.

use super::*;

impl DeviceManager {
    pub fn disconnect_if_current(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> DisconnectResult {
        let Ok(mut sessions) = self.sessions.lock() else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let matches = session.session_id == binding.session_id
            && session.connection_generation == binding.generation
            && session
                .active_link
                .as_ref()
                .is_some_and(|link| Arc::ptr_eq(link, &binding.link))
            && Arc::ptr_eq(&session.cancellation, &binding.cancellation);
        if !matches || session.state == SessionState::Terminated {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        }
        session.state = SessionState::Disconnected;
        session.last_disconnect_reason = Some(reason);
        session.connected_at = None;
        session.cancellation.store(true, Ordering::Release);
        let link = session.active_link.take();
        let webrtc = session.clear_webrtc_state();
        DisconnectResult {
            was_current: true,
            feature_transport_retained: false,
            link,
            webrtc,
        }
    }

    /// Handles loss of the LAN discovery/pairing/signaling socket without
    /// conflating it with loss of an already authenticated WebRTC feature
    /// peer.  The current binding intentionally retains its link identity:
    /// WebRTC callbacks are generation-bound to that binding, while the
    /// socket itself is closed after this manager lock is released.
    pub fn bootstrap_disconnected_if_current(
        &self,
        binding: &SessionBinding,
        reason: String,
    ) -> DisconnectResult {
        self.bootstrap_disconnected(binding, reason, None)
    }

    /// [`Self::bootstrap_disconnected_if_current`] for the reader of socket
    /// `epoch`: once the link carries a newer socket, that reader's socket
    /// ending is not the link's (checked under the session lock).
    pub fn reader_disconnected(
        &self,
        binding: &SessionBinding,
        epoch: u64,
        reason: String,
    ) -> DisconnectResult {
        self.bootstrap_disconnected(binding, reason, Some(epoch))
    }

    fn bootstrap_disconnected(
        &self,
        binding: &SessionBinding,
        reason: String,
        epoch: Option<u64>,
    ) -> DisconnectResult {
        let Ok(mut sessions) = self.sessions.lock() else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let Some(session) = sessions.get_mut(&binding.device_id) else {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        };
        let matches = session.session_id == binding.session_id
            && session.connection_generation == binding.generation
            && session
                .active_link
                .as_ref()
                .is_some_and(|link| Arc::ptr_eq(link, &binding.link))
            && Arc::ptr_eq(&session.cancellation, &binding.cancellation)
            && epoch.is_none_or(|epoch| binding.link.epoch() == epoch);
        if !matches || session.state == SessionState::Terminated {
            return DisconnectResult {
                was_current: false,
                feature_transport_retained: false,
                link: None,
                webrtc: None,
            };
        }

        // `HandoverRuntime::features_ready` is reached only after the
        // installed peer has completed mutual authentication, capabilities,
        // and feature-ready confirmation.  Peer and handover are installed
        // and cleared together by this manager.
        let web_rtc_ready = session
            .webrtc_handover
            .as_ref()
            .is_some_and(HandoverRuntime::features_ready);
        if web_rtc_ready {
            // Do not cancel the generation or clear peer/handover state.  The
            // retained Arc is returned only so the caller can close the dead
            // bootstrap socket without doing I/O under the session lock.
            //
            // The link is deliberately *kept* on the session: WebRTC callbacks
            // are generation-bound to a binding that holds this exact Arc, and
            // taking it here would make every one of them stale. `SessionLink`
            // records its own closed flag instead, so `connection_snapshot` and
            // the signaling path can both tell that the socket is gone while
            // the binding identity stays intact.
            session.state = SessionState::Ready;
            session.last_disconnect_reason = None;
            return DisconnectResult {
                was_current: true,
                feature_transport_retained: true,
                link: session.active_link.as_ref().cloned(),
                webrtc: None,
            };
        }
        drop(sessions);
        self.disconnect_if_current(binding, reason)
    }
}
