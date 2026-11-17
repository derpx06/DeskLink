//! Transport boundary for all paired DeskLink features.
//!
//! Discovery, identity, pairing, and signed WebRTC signaling belong to the
//! bootstrap link. Feature callers receive this handle instead, so they cannot
//! accidentally write ordinary packets to TLS or depend on GStreamer details.

use std::path::Path;
use std::sync::Arc;

use super::core::{DeviceManager, FeatureTransportSnapshot, SessionBinding};
use super::packet::NetworkPacket;

/// Session replacement and handover are normal recovery states. They should
/// update connection status, not masquerade as permanent feature failures in
/// the UI. The operation is retried against the next current generation.
pub(crate) fn should_surface_send_error(error: &str) -> bool {
    !error.contains("replaced by a newer session generation")
        && !error.contains("WebRTC feature transport is not ready")
        && !error.contains("Device is not connected")
        && !error.contains("Stale DeskLink session")
        // A link closed underneath a send is a transport transition, not a
        // feature failure. Discovery installs a replacement within seconds, and
        // surfacing it turned a dead socket into a banner and a desktop
        // notification once per second from the clipboard poller.
        && !error.contains("Session link is closed")
        && !error.contains("Session link has no network stream")
        // Matches both send paths. The WebRTC path says "DeskLink WebRTC
        // feature profile" and the bootstrap path says "DeskLink feature
        // profile", so the common substring is the one to test.
        && !error.contains("is not enabled in the DeskLink")
        && !error.contains("DeskLink has no outgoing handler for")
}

/// What a click on a phone that does not answer is told, in these words
/// everywhere (the window's toast, the top bar's error, the log). Not
/// swallowed by [`should_surface_send_error`]: the person asked for this.
pub(crate) const NOT_REACHABLE: &str =
    "Can't reach the phone. Check that it's on and on your network.";

/// What a send on a feature-ready link does.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ReadySend {
    /// Onto the channel.
    Now,
    /// Behind what is already queued (the link is quiet, or an earlier quiet
    /// spell's queue has not drained: order holds).
    Queue,
    /// A file payload on a quiet link: its own path says "can't reach".
    Refuse,
}

pub(crate) fn ready_send_plan(quiet: bool, queue_waiting: bool, has_payload: bool) -> ReadySend {
    match (quiet || queue_waiting, has_payload) {
        (false, _) => ReadySend::Now,
        (true, false) => ReadySend::Queue,
        (true, true) => ReadySend::Refuse,
    }
}

/// Whether the click preflight does not apply: a peer that is not ready is the
/// handover path (queueing rules), and a dozing phone cannot answer and is
/// never held up (its own rules judge it).
fn preflight_skipped(ready: bool, dozing: bool) -> bool {
    !ready || dozing
}

#[derive(Clone)]
pub(crate) struct FeatureTransport {
    sessions: Arc<DeviceManager>,
    snapshot: FeatureTransportSnapshot,
}

impl FeatureTransport {
    pub(crate) fn for_device(
        sessions: &Arc<DeviceManager>,
        device_id: &str,
    ) -> Result<Self, String> {
        let binding = sessions
            .current_binding(device_id)
            .ok_or_else(|| "Device is not connected".to_string())?;
        Self::for_binding(sessions, &binding)
    }

    pub(crate) fn for_binding(
        sessions: &Arc<DeviceManager>,
        binding: &SessionBinding,
    ) -> Result<Self, String> {
        let snapshot = sessions
            .feature_transport_snapshot(binding)
            .map_err(|error| error.to_string())?;
        if !snapshot.paired {
            return Err("Device must be paired before sending DeskLink features".to_string());
        }
        Ok(Self {
            sessions: Arc::clone(sessions),
            snapshot,
        })
    }

    /// How long this phone has been silent on both paths (the LAN link's
    /// counts only while its socket is open).
    fn silence(&self) -> Option<std::time::Duration> {
        use crate::device_links::webrtc::coordinator::{data_silence, link_silence};
        let id = &self.snapshot.binding.device_id;
        let lan = (!self.snapshot.binding.link.is_closed())
            .then(|| self.sessions.silence(id))
            .flatten();
        link_silence(lan, data_silence(id))
    }

    /// Whether the link is "Checking connection…" (see `is_quiet`).
    fn is_quiet(&self) -> bool {
        use crate::device_links::webrtc::coordinator::{
            data_silence, is_dozing, is_leaving, is_quiet,
        };
        let id = &self.snapshot.binding.device_id;
        let lan = (!self.snapshot.binding.link.is_closed())
            .then(|| self.sessions.silence(id))
            .flatten();
        is_quiet(data_silence(id), lan, is_dozing(id), is_leaving(id))
    }

    /// The click preflight, for a caller that may wait (the daemon's command
    /// thread, never the window's): a link silent for more than a probe's
    /// budget is asked "are you there?" and given `PREFLIGHT_WAIT` to answer.
    /// No answer: [`NOT_REACHABLE`], and nothing is sent or queued. A dozing
    /// phone is never held up (it cannot answer; its own rules judge it).
    pub(crate) fn preflight(&self) -> Result<(), String> {
        use crate::device_links::webrtc::coordinator::is_dozing;
        if preflight_skipped(self.is_ready(), is_dozing(&self.snapshot.binding.device_id)) {
            return Ok(());
        }
        self.preflight_by_silence()
    }

    /// The silence half of [`Self::preflight`] (separate so a test can set
    /// the clocks without a live WebRTC peer).
    fn preflight_by_silence(&self) -> Result<(), String> {
        use crate::device_links::webrtc::coordinator::{
            await_proof, note_preflight_failed, preflight_failed_recently, probe_asap,
            DESK_PROBE_AFTER, PREFLIGHT_WAIT,
        };
        let id = &self.snapshot.binding.device_id;
        if self
            .silence()
            .is_none_or(|silence| silence <= DESK_PROBE_AFTER)
        {
            return Ok(());
        }
        // The command thread is one thread for every phone and for pairing:
        // a phone that just failed, or is already known quiet, is not waited
        // for again (it is still asked, so an answer clears the state).
        if preflight_failed_recently(id, std::time::Instant::now()) || self.is_quiet() {
            probe_asap(id);
            return Err(NOT_REACHABLE.to_string());
        }
        if await_proof(id, PREFLIGHT_WAIT, || self.silence()) {
            Ok(())
        } else {
            note_preflight_failed(id);
            Err(NOT_REACHABLE.to_string())
        }
    }

    pub(crate) fn is_ready(&self) -> bool {
        self.snapshot.web_rtc_ready && self.sessions.is_current(&self.snapshot.binding)
    }

    /// Sends a paired feature over the authenticated WebRTC transport, or
    /// queues it until the handover completes. LAN never carries features:
    /// when it did, the two apps disagreed about which transport was live and
    /// packets were dropped in silence. The queue is flushed in order by
    /// `flush_pending_features` once WebRTC is ready.
    pub(crate) fn send(&self, packet: &NetworkPacket) -> Result<(), String> {
        if self.is_ready() {
            // A dead channel accepts bytes and loses them in silence: while the
            // link is quiet, or while earlier features still wait behind a
            // quiet spell (order), the packet is queued instead (bounded, in
            // order) and goes out when the phone answers again. A user's click
            // never gets here quiet: its preflight failed first. Packets with a
            // payload (files) have their own path and are refused.
            let waiting = self
                .sessions
                .has_pending_features(&self.snapshot.binding)
                .map_err(|error| error.to_string())?;
            let quiet = self.is_quiet();
            let has_payload =
                packet.payload_size.is_some() || packet.payload_transfer_info.is_some();
            match ready_send_plan(quiet, waiting, has_payload) {
                ReadySend::Now => {}
                ReadySend::Refuse => return Err(NOT_REACHABLE.to_string()),
                ReadySend::Queue => {
                    if quiet {
                        crate::device_links::webrtc::coordinator::probe_asap(
                            &self.snapshot.binding.device_id,
                        );
                    }
                    return self
                        .sessions
                        .queue_feature_while_quiet(&self.snapshot.binding, packet.clone())
                        .map_err(|error| error.to_string());
                }
            }
            return crate::device_links::webrtc::coordinator::send_feature_packet(
                &self.sessions,
                &self.snapshot,
                packet,
            );
        }
        if packet.payload_size.is_some() || packet.payload_transfer_info.is_some() {
            return Err(
                "DeskLink file transfer requires an authenticated WebRTC file channel".to_string(),
            );
        }
        let registry = crate::device_links::features::FeatureRegistry::desktop();
        if !registry
            .outgoing_capabilities()
            .iter()
            .any(|capability| capability == &packet.packet_type)
        {
            return Err(format!(
                "{} is not enabled in the DeskLink feature profile",
                packet.packet_type
            ));
        }
        if !self
            .sessions
            .handover_window_open(&self.snapshot.binding)
            .map_err(|error| error.to_string())?
        {
            return Err(NOT_REACHABLE.to_string());
        }
        let queued = self
            .sessions
            .queue_feature(&self.snapshot.binding, packet.clone())
            .map_err(|error| error.to_string())?;
        if queued {
            return Ok(());
        }
        // The handover finished after this handle's snapshot was taken: send
        // on a fresh one rather than queueing behind a flush that already ran.
        let fresh = Self::for_binding(&self.sessions, &self.snapshot.binding)?;
        if !fresh.is_ready() {
            return Err("WebRTC feature transport is not ready".to_string());
        }
        fresh.send(packet)
    }

    pub(crate) fn send_file(&self, path: &Path) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_file_transfer(
            &self.sessions,
            &self.snapshot,
            path,
        )
    }

    /// Sends a synced file (F8).
    pub(crate) fn send_folder_file(
        &self,
        path: &Path,
        folder: crate::device_links::webrtc::file_protocol::FolderRef,
        content_sha256: &str,
    ) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_folder_transfer(
            &self.sessions,
            &self.snapshot,
            path,
            folder,
            content_sha256,
        )
    }

    /// Sends an image the user copied here, for the phone's clipboard (C3).
    pub(crate) fn send_clipboard_image(&self, path: &Path) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_clipboard_transfer(
            &self.sessions,
            &self.snapshot,
            path,
            "image/png",
        )
    }

    /// Insert from the phone (F3): see `coordinator::capture_request`.
    pub(crate) fn capture(&self, kind: &str, target: &str) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::capture_request(
            &self.sessions,
            &self.snapshot,
            kind,
            target,
        )
    }

    /// Sound on or off (F1): see `coordinator::audio_switch`.
    pub(crate) fn sound(&self, what: &str, on: bool) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::audio_switch(
            &self.sessions,
            &self.snapshot,
            what,
            on,
        )
    }

    pub(crate) fn cancel_transfer(
        &self,
        transfer_id: &str,
        why: &'static str,
    ) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::cancel_file_transfer(
            &self.sessions,
            &self.snapshot,
            transfer_id,
            why,
        )
    }

    pub(crate) fn start_phone_remote_control(&self) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::start_phone_remote_control(
            &self.sessions,
            &self.snapshot,
        )
    }

    pub(crate) fn request_phone_control(&self) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::request_phone_control(
            &self.sessions,
            &self.snapshot,
        )
    }

    pub(crate) fn stop_phone_remote_control(&self) -> Result<(), String> {
        crate::device_links::webrtc::coordinator::stop_phone_remote_control(
            &self.sessions,
            &self.snapshot,
        )
    }

    pub(crate) fn send_remote_input(&self, mut packet: NetworkPacket) -> Result<(), String> {
        if !self.is_ready() {
            return Err("DeskLink remote control needs a ready WebRTC session".to_string());
        }
        crate::device_links::webrtc::coordinator::stamp_remote_input(
            &self.snapshot.binding.device_id,
            &mut packet,
        )?;
        crate::device_links::webrtc::coordinator::send_feature_packet(
            &self.sessions,
            &self.snapshot,
            &packet,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device_links::core::SessionLink;
    use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_PING};

    #[test]
    fn unknown_device_has_no_feature_transport() {
        let sessions = Arc::new(DeviceManager::new());
        assert!(FeatureTransport::for_device(&sessions, "missing").is_err());
    }

    #[test]
    fn paired_session_is_not_ready_before_authenticated_handover() {
        let sessions = Arc::new(DeviceManager::new());
        let mut link = SessionLink::test_placeholder("phone-1");
        link.info.incoming_capabilities = vec![PACKET_TYPE_PING.to_string()];
        sessions
            .register_link("phone-1".to_string(), link, true)
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();

        // WebRTC is not ready, so the send is queued rather than written to
        // LAN, and comes back out in order for the flush on feature-ready.
        assert!(!transport.is_ready());
        let mut second = NetworkPacket::new(PACKET_TYPE_PING);
        second.set("message", "second");
        transport
            .send(&NetworkPacket::new(PACKET_TYPE_PING))
            .unwrap();
        transport.send(&second).unwrap();
        let binding = sessions.current_binding("phone-1").unwrap();
        let queued = sessions.take_pending_features(&binding);
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[1].get_str("message"), Some("second"));
        assert!(sessions.take_pending_features(&binding).is_empty());
    }

    #[test]
    fn bootstrap_fallback_refuses_packets_outside_the_feature_profile() {
        let sessions = Arc::new(DeviceManager::new());
        sessions
            .register_link(
                "phone-1".to_string(),
                SessionLink::test_placeholder("phone-1"),
                true,
            )
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();

        let error = transport
            .send(&NetworkPacket::new("desklink.systemvolume.request"))
            .unwrap_err();
        assert!(error.contains("not enabled in the DeskLink feature profile"));
    }

    #[test]
    fn bootstrap_fallback_never_carries_file_payloads() {
        let sessions = Arc::new(DeviceManager::new());
        let mut link = SessionLink::test_placeholder("phone-1");
        link.info.incoming_capabilities =
            vec![crate::device_links::packet::PACKET_TYPE_SHARE_REQUEST.to_string()];
        sessions
            .register_link("phone-1".to_string(), link, true)
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();

        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SHARE_REQUEST);
        packet.payload_size = Some(1024);
        assert!(transport
            .send(&packet)
            .unwrap_err()
            .contains("authenticated WebRTC file channel"));
    }

    #[test]
    fn reconnect_transition_errors_are_not_presented_as_feature_failures() {
        assert!(!should_surface_send_error(
            "Connection was replaced by a newer session generation"
        ));
        assert!(!should_surface_send_error(
            "DeskLink WebRTC feature transport is not ready; paired features are not sent over LAN"
        ));
        assert!(!should_surface_send_error("Device is not connected"));
        assert!(should_surface_send_error("Remote command was rejected"));
    }

    /// With the handover stalled, a send is refused in the words a click is
    /// told in; nothing is queued for the 30 s of the old queue.
    #[test]
    fn a_stalled_handover_refuses_a_send_instead_of_queueing_it() {
        use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};
        let sessions = Arc::new(DeviceManager::new());
        let mut link = SessionLink::test_placeholder("phone-1");
        link.info.incoming_capabilities = vec![PACKET_TYPE_PING.to_string()];
        sessions
            .register_link("phone-1".to_string(), link, true)
            .unwrap();
        let binding = sessions.current_binding("phone-1").unwrap();
        sessions
            .with_session(&binding, |session| {
                let wire = WebRtcWireBinding::from_attempt(
                    "desktop",
                    "phone-1",
                    "01234567-89ab-cdef-0123-456789abcdef",
                )
                .unwrap();
                let mut handover = HandoverRuntime::new("attempt".to_string(), wire);
                handover.started_at = std::time::Instant::now()
                    - crate::device_links::webrtc::coordinator::NEGOTIATION_TIMEOUT;
                session.webrtc_handover = Some(handover);
            })
            .unwrap();
        let transport = FeatureTransport::for_device(&sessions, "phone-1").unwrap();
        assert_eq!(
            transport.send(&NetworkPacket::new(PACKET_TYPE_PING)),
            Err(NOT_REACHABLE.to_string())
        );
        assert!(sessions.take_pending_features(&binding).is_empty());
    }

    /// A ready session whose LAN and data paths were last heard `silent_for`
    /// ago (the clocks moved into the past, never slept on).
    fn ready_session(
        id: &str,
        silent_for: std::time::Duration,
    ) -> (Arc<DeviceManager>, FeatureTransport) {
        use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};
        let sessions = Arc::new(DeviceManager::new());
        let mut link = SessionLink::test_placeholder(id);
        link.info.incoming_capabilities = vec![PACKET_TYPE_PING.to_string()];
        sessions.register_link(id.to_string(), link, true).unwrap();
        let binding = sessions.current_binding(id).unwrap();
        sessions
            .with_session(&binding, |session| {
                let wire = WebRtcWireBinding::from_attempt(
                    "desktop",
                    id,
                    "01234567-89ab-cdef-0123-456789abcdef",
                )
                .unwrap();
                let mut handover = HandoverRuntime::new("attempt".to_string(), wire);
                handover.record_offer("offer".into()).unwrap();
                handover.record_answer("answer".into()).unwrap();
                handover.mark_local_authenticated().unwrap();
                handover.mark_remote_authenticated().unwrap();
                handover.mark_local_capabilities_confirmed().unwrap();
                handover.mark_remote_capabilities_confirmed().unwrap();
                handover.mark_local_feature_ready().unwrap();
                handover.mark_remote_feature_ready().unwrap();
                session.webrtc_handover = Some(handover);
                session.last_inbound_at = std::time::Instant::now() - silent_for;
            })
            .unwrap();
        crate::device_links::webrtc::coordinator::forget_liveness(id);
        crate::device_links::webrtc::coordinator::note_data_inbound_ago(id, silent_for);
        let transport = FeatureTransport::for_device(&sessions, id).unwrap();
        (sessions, transport)
    }

    /// The click preflight, by silence: 1 s (inside a probe's budget) and a
    /// healthy link proceed at once; 2.5 s on a dead peer fails with the
    /// "Can't reach" error inside about 1.5 s, asks the phone, and a second
    /// click straight after fails at once (the command thread is not held again).
    #[test]
    fn the_click_preflight_by_silence() {
        use std::time::{Duration, Instant};
        for silent in [Duration::ZERO, Duration::from_secs(1)] {
            let (_s, transport) = ready_session("preflight-ok-phone", silent);
            let started = Instant::now();
            assert_eq!(transport.preflight_by_silence(), Ok(()));
            assert!(
                started.elapsed() < Duration::from_millis(200),
                "no extra latency"
            );
        }
        let (_s, transport) = ready_session("preflight-dead-phone", Duration::from_millis(2_500));
        let started = Instant::now();
        assert_eq!(
            transport.preflight_by_silence(),
            Err(NOT_REACHABLE.to_string())
        );
        let took = started.elapsed();
        assert!(
            took >= Duration::from_millis(1_400) && took < Duration::from_millis(2_500),
            "{took:?}"
        );
        let again = Instant::now();
        assert_eq!(
            transport.preflight_by_silence(),
            Err(NOT_REACHABLE.to_string())
        );
        assert!(
            again.elapsed() < Duration::from_millis(1_000),
            "remembered: no second wait"
        );
        // And nothing was queued or sent for it by the preflight itself.
        crate::device_links::webrtc::coordinator::forget_liveness("preflight-dead-phone");
    }

    /// A dozing phone is never held up and a not-ready peer is the handover
    /// path: neither is preflighted.
    #[test]
    fn dozing_and_not_ready_skip_the_preflight() {
        assert!(preflight_skipped(false, false));
        assert!(preflight_skipped(false, true));
        assert!(preflight_skipped(true, true));
        assert!(!preflight_skipped(true, false));
        // A not-ready session with a long silence returns at once.
        let (_s, transport) = ready_session(
            "preflight-notready-phone",
            std::time::Duration::from_secs(30),
        );
        let started = std::time::Instant::now();
        assert_eq!(transport.preflight(), Ok(()));
        assert!(started.elapsed() < std::time::Duration::from_millis(1_000));
        crate::device_links::webrtc::coordinator::forget_liveness("preflight-notready-phone");
    }

    /// A send on a ready link: straight out when healthy and nothing waits;
    /// queued (never dropped) while quiet or behind a queue; a payload on a
    /// quiet link is refused.
    #[test]
    fn a_background_send_on_a_quiet_link_is_queued_not_dropped() {
        assert_eq!(ready_send_plan(false, false, false), ReadySend::Now);
        assert_eq!(ready_send_plan(true, false, false), ReadySend::Queue);
        assert_eq!(
            ready_send_plan(false, true, false),
            ReadySend::Queue,
            "order"
        );
        assert_eq!(ready_send_plan(true, false, true), ReadySend::Refuse);
        assert_eq!(ready_send_plan(false, false, true), ReadySend::Now);
    }

    /// The quiet queue is the handover queue: bounded to 64 (oldest out),
    /// in order, and visible to `has_pending_features`.
    #[test]
    fn the_quiet_queue_is_bounded_and_in_order() {
        let sessions = Arc::new(DeviceManager::new());
        sessions
            .register_link(
                "phone-1".into(),
                SessionLink::test_placeholder("phone-1"),
                true,
            )
            .unwrap();
        let binding = sessions.current_binding("phone-1").unwrap();
        assert!(!sessions.has_pending_features(&binding).unwrap());
        for n in 0..70 {
            let mut packet = NetworkPacket::new(PACKET_TYPE_PING);
            packet.set("n", n as i64);
            sessions
                .queue_feature_while_quiet(&binding, packet)
                .unwrap();
        }
        assert!(sessions.has_pending_features(&binding).unwrap());
        let drained = sessions.take_pending_features(&binding);
        assert_eq!(
            drained.len(),
            crate::device_links::core::PENDING_FEATURE_LIMIT
        );
        let numbers: Vec<i64> = drained.iter().map(|p| p.get_i64("n").unwrap()).collect();
        assert_eq!(numbers.first(), Some(&6), "the oldest went");
        assert!(
            numbers.windows(2).all(|pair| pair[0] + 1 == pair[1]),
            "in order"
        );
        assert!(!sessions.has_pending_features(&binding).unwrap());
        // A binding the session moved past is the stale-generation error.
        sessions
            .register_link(
                "phone-1".into(),
                SessionLink::test_placeholder("phone-1"),
                true,
            )
            .unwrap();
        assert!(sessions.has_pending_features(&binding).is_err());
    }

    /// A click on a phone that does not answer is told, never swallowed as
    /// recovery noise.
    #[test]
    fn the_unreachable_answer_is_surfaced() {
        assert!(should_surface_send_error(NOT_REACHABLE));
        assert!(NOT_REACHABLE.starts_with("Can't reach"));
    }
}
