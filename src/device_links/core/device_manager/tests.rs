//! Tests of the session registry: generations and stale bindings, link
//! replacement and adoption, disconnects, WebRTC install and readiness, the
//! feature queue, signals and envelopes taken once, reconnect leases.

use super::*;

fn mark_features_ready(manager: &DeviceManager, binding: &SessionBinding) {
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop", &binding.device_id, &attempt_id).unwrap();
    manager
        .with_session(binding, |session| {
            let mut handover = HandoverRuntime::new(attempt_id.clone(), wire);
            handover.record_offer("offer".to_string()).unwrap();
            handover.record_answer("answer".to_string()).unwrap();
            handover.mark_local_authenticated().unwrap();
            handover.mark_remote_authenticated().unwrap();
            handover.mark_local_capabilities_confirmed().unwrap();
            handover.mark_remote_capabilities_confirmed().unwrap();
            handover.mark_local_feature_ready().unwrap();
            handover.mark_remote_feature_ready().unwrap();
            session.webrtc_handover = Some(handover);
        })
        .unwrap();
}

fn link(device_id: &str) -> SessionLink {
    SessionLink::test_placeholder(device_id)
}

#[test]
fn a_new_manager_has_no_current_binding() {
    let manager = DeviceManager::new();

    assert!(manager.current_binding("phone-1").is_none());
}

#[test]
fn replacement_keeps_session_id_and_makes_old_binding_stale() {
    let manager = DeviceManager::new();
    let first = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap();
    let second = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap();

    assert_eq!(first.binding.session_id, second.binding.session_id);
    assert_eq!(second.binding.generation, first.binding.generation + 1);
    assert!(!manager.is_current(&first.binding));
    assert!(manager.is_current(&second.binding));
    assert!(second.replaced_link.unwrap().is_closed());
    assert!(first.binding.cancellation.load(Ordering::Acquire));
}

#[test]
fn transport_snapshot_becomes_stale_after_link_replacement() {
    let manager = DeviceManager::new();
    let first = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let snapshot = manager.feature_transport_snapshot(&first).unwrap();
    let replacement = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;

    assert!(snapshot.paired);
    assert!(!snapshot.web_rtc_ready);
    assert!(!manager.is_current(&snapshot.binding));
    assert!(manager.is_current(&replacement));
}

/// A signed request id is taken once per session, whatever attempt it
/// names: a replay is refused even after another peer was installed (the
/// per-attempt cache is cleared by an install, which let a recorded offer
/// replace the live peer).
#[test]
fn a_signaling_request_id_is_taken_once_even_across_installs() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    assert!(manager
        .note_signal_request(&binding, "request-1".into())
        .is_ok());
    assert!(matches!(
        manager.note_signal_request(&binding, "request-1".into()),
        Err(SessionError::ReplayDetected)
    ));
    // What an install does to the per-attempt cache does not touch it.
    let _ = manager.with_session(&binding, |session| session.seen_webrtc_request_ids.clear());
    assert!(matches!(
        manager.note_signal_request(&binding, "request-1".into()),
        Err(SessionError::ReplayDetected)
    ));
    assert!(manager
        .note_signal_request(&binding, "request-2".into())
        .is_ok());
    // An unpaired session takes nothing.
    let stranger = manager
        .register_link("stranger".to_string(), link("stranger"), false)
        .unwrap()
        .binding;
    assert!(matches!(
        manager.note_signal_request(&stranger, "x".into()),
        Err(SessionError::NotPaired)
    ));
}

#[test]
fn stale_disconnect_cannot_clear_a_replacement() {
    let manager = DeviceManager::new();
    let first = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap();
    let second = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap();

    let stale = manager.disconnect_if_current(&first.binding, "old reader ended".to_string());

    assert!(!stale.was_current);
    assert!(manager.is_current(&second.binding));
}

#[test]
fn pairing_state_follows_the_trust_store_across_transport_replacement() {
    let manager = DeviceManager::new();
    let paired = |binding: &SessionBinding| {
        manager
            .with_session(binding, |session| {
                session.pairing.state == crate::device_links::pairing::PairState::Paired
            })
            .unwrap()
    };
    let first = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap();
    let second = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap();
    assert!(paired(&second.binding));
    assert_eq!(first.binding.session_id, second.binding.session_id);

    // Unpaired while it was away: the next link must not resume `Paired`,
    // or any certificate claiming this id would be accepted unpinned.
    let third = manager
        .register_link("phone-1".to_string(), link("phone-1"), false)
        .unwrap();
    assert!(!paired(&third.binding));
}

#[test]
fn only_one_reconnect_lease_can_be_claimed() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let disconnected = manager.disconnect_if_current(&binding, "network lost".to_string());
    assert!(disconnected.was_current);

    assert!(manager.claim_reconnect("phone-1", Instant::now()).is_some());
    assert!(manager.claim_reconnect("phone-1", Instant::now()).is_none());
}

#[test]
fn registration_after_disconnect_creates_a_live_new_generation() {
    let manager = DeviceManager::new();
    let first = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let disconnected = manager.disconnect_if_current(&first, "bootstrap socket closed".to_string());
    assert!(disconnected.was_current);
    assert!(first.cancellation.load(Ordering::Acquire));

    let reconnected = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;

    assert_eq!(reconnected.session_id, first.session_id);
    assert_eq!(reconnected.generation, first.generation + 1);
    assert!(!reconnected.cancellation.load(Ordering::Acquire));
    assert!(manager.is_current(&reconnected));
}

/// The desktop discards a refreshed bootstrap socket while its WebRTC peer
/// is feature-ready, so a discovery refresh does not tear down every paired
/// feature. That is right only while the retained link is still open.
///
/// Once the phone has closed the old socket, keeping it and discarding the
/// fresh one leaves this side with **no signalling channel** — and when the
/// WebRTC peer subsequently fails, the `RestartRequest` it must send dies
/// with "Session link is closed" against a socket the peer already
/// replaced. Observed on hardware as a device stuck on "Connecting"
/// forever. `finish_secure_link` therefore requires both conditions.
#[test]
fn a_refreshed_socket_is_only_discarded_while_the_retained_link_is_open() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop", "phone-1", &attempt_id).unwrap();
    manager
        .with_session(&binding, |session| {
            let mut handover = HandoverRuntime::new(attempt_id.clone(), wire);
            handover.record_offer("offer".to_string()).unwrap();
            handover.record_answer("answer".to_string()).unwrap();
            handover.mark_local_authenticated().unwrap();
            handover.mark_remote_authenticated().unwrap();
            handover.mark_local_capabilities_confirmed().unwrap();
            handover.mark_remote_capabilities_confirmed().unwrap();
            handover.mark_local_feature_ready().unwrap();
            handover.mark_remote_feature_ready().unwrap();
            session.webrtc_handover = Some(handover);
        })
        .unwrap();

    // Healthy: WebRTC is ready and the bootstrap socket is alive, so a
    // refresh is redundant and is correctly thrown away.
    assert!(manager.has_ready_webrtc("phone-1"));
    assert!(manager.has_live_bootstrap_link("phone-1"));

    // The phone replaces its short-lived socket. WebRTC is still ready, but
    // the retained link is now dead.
    binding.link.close();
    assert!(
        manager.has_ready_webrtc("phone-1"),
        "the authenticated WebRTC transport survives the bootstrap socket"
    );
    assert!(
        !manager.has_live_bootstrap_link("phone-1"),
        "a refreshed socket must now be adopted, or signalling has nowhere to go"
    );
}

/// The other half of the rule above: once the retained socket is dead, the
/// replacement must be *adopted*, not registered.
///
/// `register_link` bumps the generation and calls `clear_webrtc_state`
/// unconditionally, so registering here tore down a working, authenticated
/// transport every time the phone refreshed its bootstrap socket. The
/// handover that followed dropped its own bootstrap socket, which brought
/// the phone straight back — a loop that pinned the UI on "Connecting"
/// while battery and notifications kept arriving over the link underneath.
#[test]
fn a_replacement_socket_is_adopted_without_disturbing_a_ready_webrtc_transport() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    mark_features_ready(&manager, &binding);
    binding.link.close();

    let replacement: Arc<dyn LinkTransport> =
        Arc::new(crate::device_links::core::link_transport::DeadTransport);
    let (adopted, _, _) = adopt(&manager, &replacement)
        .expect("a closed link with a ready WebRTC transport is adoptable");

    assert_eq!(
        adopted.generation, binding.generation,
        "adoption must not bump the generation the WebRTC peer is bound to"
    );
    assert!(
        Arc::ptr_eq(&adopted.link, &binding.link),
        "the link identity every WebRTC callback compares against must survive"
    );
    assert!(
        manager.is_current(&binding),
        "the old binding stays current"
    );
    assert!(
        manager.has_ready_webrtc("phone-1"),
        "the working transport must not be torn down by a socket refresh"
    );
    assert!(
        manager.has_live_bootstrap_link("phone-1"),
        "signalling must have somewhere to go again"
    );
}

/// Adoption is for preserving something. With no feature-ready transport
/// there is nothing to preserve and a plain registration is correct. A
/// live (silent) incumbent is swapped out in place: same link, a new
/// epoch, and the old socket handed back to be shut down.
#[test]
fn adoption_is_refused_without_a_ready_transport_and_swaps_a_live_socket() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let replacement: Arc<dyn LinkTransport> =
        Arc::new(crate::device_links::core::link_transport::DeadTransport);

    binding.link.close();
    assert!(
        adopt(&manager, &replacement).is_none(),
        "no WebRTC transport to preserve"
    );

    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    mark_features_ready(&manager, &binding);
    let epoch = binding.link.epoch();
    // Open and still answering: the newcomer is redundant.
    assert!(matches!(
        manager.adopt_or_keep(
            "phone-1",
            &replacement,
            &binding.link.certificate_pem,
            std::time::Duration::from_secs(20)
        ),
        Adoption::Keep
    ));
    let (adopted, _old, adopted_epoch) =
        adopt(&manager, &replacement).expect("a silent live incumbent is swapped in place");
    assert_eq!(
        adopted_epoch,
        binding.link.epoch(),
        "the new reader's socket"
    );
    assert!(Arc::ptr_eq(&adopted.link, &binding.link));
    assert_eq!(adopted.generation, binding.generation);
    assert_ne!(binding.link.epoch(), epoch, "the old reader sees it");
    assert_eq!(
        binding.link.read_byte_at(epoch).unwrap(),
        crate::device_links::core::ReadOutcome::Closed,
        "and never reads a byte of the new socket"
    );
    assert!(manager.has_ready_webrtc("phone-1"));
}

/// A socket with another certificate is never swapped into the session,
/// silent incumbent or not.
#[test]
fn adoption_needs_the_sessions_own_certificate() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    mark_features_ready(&manager, &binding);
    binding.link.close();
    let replacement: Arc<dyn LinkTransport> =
        Arc::new(crate::device_links::core::link_transport::DeadTransport);
    assert!(matches!(
        manager.adopt_or_keep(
            "phone-1",
            &replacement,
            "-----BEGIN CERTIFICATE-----forged",
            std::time::Duration::ZERO
        ),
        Adoption::Refuse
    ));
    assert!(manager.is_current(&binding), "the session is untouched");
    assert!(manager.holds_paired_transport("phone-1"));
    assert!(
        adopt(&manager, &replacement).is_some(),
        "its own certificate is"
    );
}

/// Adopts with no answering window (the incumbent counts as silent).
fn adopt(
    manager: &DeviceManager,
    transport: &Arc<dyn LinkTransport>,
) -> Option<(SessionBinding, Arc<dyn LinkTransport>, u64)> {
    match manager.adopt_or_keep("phone-1", transport, "", std::time::Duration::ZERO) {
        Adoption::Adopted {
            binding,
            replaced,
            epoch,
        } => Some((binding, replaced, epoch)),
        _ => None,
    }
}

/// The old socket's reader ending after an adoption must not close the
/// new socket: its disconnect names its own (old) epoch.
#[test]
fn a_late_end_of_the_old_socket_never_closes_the_new_one() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    mark_features_ready(&manager, &binding);
    let old_epoch = binding.link.epoch();
    let replacement: Arc<dyn LinkTransport> =
        Arc::new(crate::device_links::core::link_transport::DeadTransport);
    let (_, _, new_epoch) = adopt(&manager, &replacement).unwrap();
    let late = manager.reader_disconnected(&binding, old_epoch, "old socket ended".into());
    assert!(!late.was_current, "the old reader is not the link");
    assert!(!binding.link.is_closed() && manager.has_live_bootstrap_link("phone-1"));
    let real = manager.reader_disconnected(&binding, new_epoch, "new socket ended".into());
    assert!(real.was_current, "the current reader is");
    // Two sockets adopted in turn: each reader has its own epoch.
    let (_, _, first) = adopt(&manager, &replacement).unwrap();
    let (_, _, second) = adopt(&manager, &replacement).unwrap();
    assert_ne!(first, second);
    assert_eq!(
        binding.link.read_byte_at(first).unwrap(),
        crate::device_links::core::ReadOutcome::Closed,
        "the earlier reader stops; only the latest reads"
    );
}

#[test]
fn bootstrap_disconnect_preserves_feature_ready_generation() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let attempt_id = uuid::Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop", "phone-1", &attempt_id).unwrap();
    manager
        .with_session(&binding, |session| {
            let mut handover = HandoverRuntime::new(attempt_id.clone(), wire);
            handover.record_offer("offer".to_string()).unwrap();
            handover.record_answer("answer".to_string()).unwrap();
            handover.mark_local_authenticated().unwrap();
            handover.mark_remote_authenticated().unwrap();
            handover.mark_local_capabilities_confirmed().unwrap();
            handover.mark_remote_capabilities_confirmed().unwrap();
            handover.mark_local_feature_ready().unwrap();
            handover.mark_remote_feature_ready().unwrap();
            session.webrtc_handover = Some(handover);
            // The concrete GStreamer peer is not needed by this manager
            // test. Presence is represented by retaining the handover
            // runtime, while production only reaches this state with an
            // installed peer.
        })
        .unwrap();

    let disconnected =
        manager.bootstrap_disconnected_if_current(&binding, "bootstrap socket closed".to_string());
    assert!(disconnected.was_current);
    assert!(disconnected.feature_transport_retained);
    assert!(manager.is_current(&binding));
    assert!(!binding.cancellation.load(Ordering::Acquire));
    assert_eq!(
        manager
            .with_session(&binding, |session| session.connection_generation)
            .unwrap(),
        binding.generation,
    );
}

/// Discovery decides whether to reconnect from `has_live_bootstrap_link`,
/// not from the presence of a binding. A session keeps a closed link
/// installed while WebRTC outlives the socket, and using binding presence
/// meant such a device was never offered a replacement connection — so once
/// its WebRTC peer failed too, it stayed offline until a restart.
#[test]
fn a_closed_bootstrap_link_does_not_count_as_a_live_connection() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    assert!(manager.has_live_bootstrap_link("phone-1"));

    binding.link.close();

    assert!(!manager.has_live_bootstrap_link("phone-1"));
    // The binding itself deliberately survives so WebRTC callbacks that are
    // generation-bound to it stay valid.
    assert!(manager.current_binding("phone-1").is_some());
}

#[test]
fn an_unknown_device_has_no_live_bootstrap_link() {
    let manager = DeviceManager::new();
    assert!(!manager.has_live_bootstrap_link("phone-1"));
}

/// The reconciler reads reachability from here, so a closed socket with no
/// WebRTC transport must read as unreachable.
#[test]
fn connection_snapshot_reports_a_closed_socket_as_unreachable() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;

    let snapshot = manager.connection_snapshot();
    assert_eq!(snapshot.len(), 1);
    assert!(snapshot[0].linked);
    assert!(snapshot[0].paired);
    assert!(snapshot[0].is_reachable());

    binding.link.close();

    let snapshot = manager.connection_snapshot();
    assert!(!snapshot[0].linked);
    assert!(!snapshot[0].feature_transport_ready);
    assert!(!snapshot[0].is_reachable());
}

#[test]
fn stale_webrtc_failure_cannot_clear_a_new_attempt() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    manager
        .with_session(&binding, |session| {
            session.webrtc_attempt_id = Some("current-attempt".to_string());
        })
        .unwrap();

    assert!(matches!(
        manager.clear_webrtc_if_current(&binding, "stale-attempt", "old peer failed".to_string()),
        Err(SessionError::StaleBinding)
    ));
    assert_eq!(
        manager
            .with_session(&binding, |session| session.webrtc_attempt_id.clone())
            .unwrap(),
        Some("current-attempt".to_string()),
    );
}

#[test]
fn unpairing_clears_the_active_webrtc_attempt() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    manager
        .with_session(&binding, |session| {
            session.webrtc_attempt_id = Some("old-attempt".to_string());
        })
        .unwrap();

    manager
        .revoke_pairing(&binding, "user unpaired the device".to_string())
        .unwrap();

    assert_eq!(manager.active_webrtc_attempt(&binding).unwrap(), None);
}

#[test]
fn terminate_all_cancels_each_current_binding() {
    let manager = DeviceManager::new();
    let first = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let second = manager
        .register_link("phone-2".to_string(), link("phone-2"), false)
        .unwrap()
        .binding;

    let links = manager.terminate_all();

    assert_eq!(links.len(), 2);
    assert!(first.cancellation.load(Ordering::Acquire));
    assert!(second.cancellation.load(Ordering::Acquire));
    assert!(manager.current_binding("phone-1").is_none());
    assert!(manager.current_binding("phone-2").is_none());
}

#[test]
fn replacement_discards_the_previous_handover_state() {
    use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};

    let manager = DeviceManager::new();
    let first = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    manager
        .with_session(&first, |session| {
            let wire = WebRtcWireBinding::from_attempt(
                "desktop",
                "phone-1",
                "01234567-89ab-cdef-0123-456789abcdef",
            )
            .unwrap();
            session.webrtc_handover = Some(HandoverRuntime::new("attempt".to_string(), wire));
        })
        .unwrap();

    let replacement = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    let handover_present = manager
        .with_session(&replacement, |session| session.webrtc_handover.is_some())
        .unwrap();

    assert!(!handover_present);
}

#[test]
fn a_webrtc_envelope_is_accepted_only_once_per_attempt() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    manager
        .with_session(&binding, |session| {
            session.webrtc_attempt_id = Some("attempt".to_string());
        })
        .unwrap();

    manager
        .accept_webrtc_envelope(&binding, "attempt", "message-1".to_string())
        .unwrap();
    assert_eq!(
        manager
            .accept_webrtc_envelope(&binding, "attempt", "message-1".to_string())
            .unwrap_err(),
        SessionError::ReplayDetected
    );
}

#[test]
fn a_feature_is_not_queued_once_the_handover_is_ready() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone".to_string(), link("phone"), true)
        .unwrap()
        .binding;
    let ping = crate::device_links::packet::NetworkPacket::new("desklink.ping");
    assert!(manager.queue_feature(&binding, ping.clone()).unwrap());
    mark_features_ready(&manager, &binding);
    // Ready: the caller must send directly, so nothing strands in the queue.
    assert!(!manager.queue_feature(&binding, ping).unwrap());
    assert_eq!(manager.take_pending_features(&binding).len(), 1);
}

#[test]
fn stale_queued_features_are_dropped_not_delivered() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone".to_string(), link("phone"), true)
        .unwrap()
        .binding;
    manager
        .with_session(&binding, |session| {
            let old = Instant::now()
                - super::super::device_session::PENDING_FEATURE_MAX_AGE
                - std::time::Duration::from_secs(1);
            session.pending_features.push_back((
                old,
                crate::device_links::packet::NetworkPacket::new("desklink.lock.request"),
            ));
        })
        .unwrap();
    manager
        .queue_feature(
            &binding,
            crate::device_links::packet::NetworkPacket::new("desklink.ping"),
        )
        .unwrap();
    let taken = manager.take_pending_features(&binding);
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].packet_type, "desklink.ping");
}

#[test]
fn a_new_instance_id_means_the_peer_restarted() {
    let manager = DeviceManager::new();
    let mut first = link("phone");
    first.info.instance_id = Some("epoch-a".to_string());
    manager
        .register_link("phone".to_string(), first, true)
        .unwrap();

    assert!(!manager.peer_restarted("phone", Some("epoch-a")));
    assert!(manager.peer_restarted("phone", Some("epoch-b")));
    // An older build that sends no epoch never counts as a restart.
    assert!(!manager.peer_restarted("phone", None));
    assert!(!manager.peer_restarted("unknown", Some("epoch-b")));
}

#[test]
fn a_new_link_resets_an_exhausted_recovery_ladder() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone".to_string(), link("phone"), true)
        .unwrap()
        .binding;
    manager
        .with_session(&binding, |session| session.webrtc_recovery_attempt = 6)
        .unwrap();
    let replaced = manager
        .register_link("phone".to_string(), link("phone"), true)
        .unwrap()
        .binding;
    assert_eq!(
        manager
            .with_session(&replaced, |session| session.webrtc_recovery_attempt)
            .unwrap(),
        0
    );
}

/// A click queues behind a handover only while it is young: a stalled attempt
/// (older than the negotiation timeout) is a phone that is not coming, and
/// the send says so instead of holding the packet for 30 s. With no attempt
/// yet, the open bootstrap link is the window.
#[test]
fn a_feature_queues_only_in_the_real_handover_window() {
    let manager = DeviceManager::new();
    let binding = manager
        .register_link("phone-1".to_string(), link("phone-1"), true)
        .unwrap()
        .binding;
    assert!(
        manager.handover_window_open(&binding).unwrap(),
        "no attempt, link open"
    );
    let begin = |started_ago: std::time::Duration| {
        manager
            .with_session(&binding, |session| {
                let wire = WebRtcWireBinding::from_attempt(
                    "desktop",
                    "phone-1",
                    "01234567-89ab-cdef-0123-456789abcdef",
                )
                .unwrap();
                let mut handover = HandoverRuntime::new("attempt".to_string(), wire);
                handover.started_at = std::time::Instant::now() - started_ago;
                session.webrtc_handover = Some(handover);
            })
            .unwrap();
    };
    begin(std::time::Duration::from_secs(1));
    assert!(
        manager.handover_window_open(&binding).unwrap(),
        "a young attempt"
    );
    begin(crate::device_links::webrtc::coordinator::NEGOTIATION_TIMEOUT);
    assert!(
        !manager.handover_window_open(&binding).unwrap(),
        "a stalled attempt"
    );
}
