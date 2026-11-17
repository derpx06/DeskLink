//! The two-peer scenarios run through the production pairing commands and
//! inbound dispatch: pairing, unpairing, half-open links, duplicates, reorder
//! and drops.

use super::*;

/// A valid certificate belonging to nobody in the test — the attacker's.
fn foreign_certificate_pem() -> String {
    let dir = temp_dir("intruder");
    let config = Config::load_from_dir(dir.clone()).expect("intruder config");
    let pem = String::from_utf8(config.certificate().to_pem().expect("pem")).expect("utf8");
    let _ = std::fs::remove_dir_all(&dir);
    pem
}

/// Spec scenario 19: drop the socket while paired, reconnect, and the
/// pairing survives. The user must never be asked to re-pair because the
/// WiFi blinked.
#[test]
fn a_paired_session_survives_a_dropped_link_and_reconnects_still_paired() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    phone.accept();
    net.advance(0);
    desktop.pump();
    assert_eq!(desktop.pair_state(), PairState::Paired);

    desktop.drop_link();
    desktop.tick();
    assert_eq!(
        desktop.status(),
        DeviceStatus::Unreachable,
        "a dropped link is reconciled to unreachable, not left reading Paired"
    );

    desktop.relink();
    desktop.tick();
    assert_eq!(
        desktop.pair_state(),
        PairState::Paired,
        "trust is persisted, so the new generation comes back paired"
    );
    assert_eq!(desktop.status(), DeviceStatus::Paired);
    assert!(desktop.is_trusted());
}

/// The pairing deadline is driven from the harness's virtual clock, so a
/// 30s timeout is stepped in a microsecond and nothing sleeps.
#[test]
fn an_unanswered_request_expires_on_the_virtual_clock() {
    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    assert_eq!(desktop.pair_state(), PairState::Requested);

    net.advance(29_000);
    assert!(desktop.tick().is_empty(), "not yet due");
    assert_eq!(desktop.pair_state(), PairState::Requested);

    net.advance(2_000);
    assert_eq!(desktop.tick(), vec![desktop.peer_id()], "now abandoned");
    assert_eq!(desktop.pair_state(), PairState::NotPaired);
}

/// **I1 — the reconciler corrects a stale `trusted` flag, not re-asserts it.**
///
/// It used to decide Paired-vs-Connected from `device.trusted`, the view's
/// own cached copy, while `connection.paired` — the session state the
/// dispatcher actually authorises from — sat unread on the line above. Any
/// divergence was therefore rewritten as truth once a second, forever.
#[test]
fn the_reconciler_corrects_a_stale_trusted_flag_rather_than_re_asserting_it() {
    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");
    assert_eq!(desktop.pair_state(), PairState::NotPaired);

    // Plant exactly the divergence the old code preserved: the view claims
    // trust the session does not have.
    desktop
        .devices
        .lock()
        .unwrap()
        .get_mut(&desktop.peer_id())
        .unwrap()
        .trusted = true;

    desktop.reconcile();

    assert_eq!(
        desktop.status(),
        DeviceStatus::Connected,
        "reachable but not paired, per the session"
    );
    assert!(
        !desktop.devices.lock().unwrap()[&desktop.peer_id()].trusted,
        "and the cached flag is corrected, not left to be re-read next tick"
    );
}

/// **I2 — a send that fails must not leave the state advanced.**
///
/// `accept_packet` moves the handler to `Paired` while *building* the
/// packet, so a failed send used to return early with the session claiming
/// `Paired`, the trust store empty and the UI still showing the prompt. The
/// dispatcher authorises from the session, so that split admitted features
/// from a peer this device had never recorded as trusted.
#[test]
fn an_acceptance_that_could_not_be_sent_leaves_no_half_pairing() {
    let net = TestNetwork::new();
    let (desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    assert_eq!(phone.pair_state(), PairState::RequestedByPeer);

    // The link dies between the prompt appearing and the user tapping
    // Accept — a phone put down for a moment on a flaky network.
    net.drop_end(1);
    phone.accept();

    assert_eq!(
        phone.pair_state(),
        PairState::RequestedByPeer,
        "the prompt is still outstanding; nothing was agreed"
    );
    assert!(
        !phone.is_trusted(),
        "and nothing was persisted on the strength of a packet that never left"
    );
    // **I8.** And it does *not* raise a banner. A closed link is a transport
    // transition the reconciler reports through the row's status within a
    // second; "Session link is closed" adds jargon, not information, and
    // this path is reachable often enough on a flaky network to be noise.
    assert!(
        phone.errors().is_empty(),
        "a transient transport state is not a user-facing error"
    );
}

/// **C5 — an expiring negotiation tells the peer instead of failing quietly.**
///
/// Spec scenario 10: the peer never answers, and *both* sides must end up
/// back at `NotPaired`. Previously expiry was silent, so the two ends could
/// only stay in step by ordering their timeouts against each other.
#[test]
fn an_expiring_request_tells_the_peer_to_stop_waiting() {
    let net = TestNetwork::new();
    let (mut left, mut right) = peer_pair(&net, "desktop-1", "phone-1");
    let (leader, follower) = if left.leads() {
        (&mut left, &mut right)
    } else {
        (&mut right, &mut left)
    };

    leader.request_pair();
    net.advance(0);
    follower.pump();
    assert_eq!(follower.pair_state(), PairState::RequestedByPeer);

    // Nobody touches the prompt.
    net.advance(31_000);
    assert_eq!(leader.tick(), vec![leader.peer_id()]);
    assert_eq!(leader.pair_state(), PairState::NotPaired);

    net.advance(0);
    follower.pump();
    assert_eq!(
        follower.pair_state(),
        PairState::NotPaired,
        "the prompt is withdrawn rather than left on screen"
    );
}

/// **C6 — rejecting a device that is already trusted really untrusts it.**
///
/// `reject_pair` passed `remove_trust = false`, so the session was cleared
/// but the record stayed on disk and the device came back paired on the next
/// reconnect — the user's "no" silently undone.
#[test]
fn rejecting_a_trusted_device_removes_the_persisted_record() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    phone.accept();
    net.advance(0);
    desktop.pump();
    assert!(phone.is_trusted(), "trusted to begin with");

    phone.reject();
    assert!(
        !phone.is_trusted(),
        "reject must clear the persisted trust, not just the session"
    );
    assert_eq!(phone.pair_state(), PairState::NotPaired);
}

/// **The liveness contract: probe the quiet, bury the silent.**
///
/// A link is only ever declared dead on evidence — silence — rather than on
/// this side's own opinion of the socket. `is_closed()` flips only when our
/// reader sees EOF, so a peer that vanishes without a FIN leaves it `false`
/// indefinitely, which is what let both sides hold links to processes that
/// no longer existed.
#[test]
fn a_silent_peer_is_probed_then_declared_dead() {
    use std::time::Duration;

    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");

    // Fresh link: nothing to do.
    let quiet = desktop.liveness(Duration::from_secs(1));
    assert!(quiet.probe.is_empty() && quiet.dead.is_empty());

    // Quiet long enough to be worth a probe, but not yet gone.
    let idle = desktop.liveness(crate::device_links::daemon::HEARTBEAT_IDLE);
    assert_eq!(idle.probe.len(), 1, "a quiet link is probed");
    assert!(idle.dead.is_empty(), "and not yet buried");

    // Silent past the budget: three missed probes.
    let gone = desktop.liveness(crate::device_links::daemon::HEARTBEAT_DEAD);
    assert!(gone.probe.is_empty());
    assert_eq!(gone.dead.len(), 1, "silence past the budget is death");
    assert_eq!(gone.dead[0].device_id, desktop.peer_id());
}

/// A phone's new socket arrives while its old one is still open but has
/// been silent past the answering window (a frozen phone, a Wi-Fi roam).
/// It is adopted in place: the old socket's reader leaves quietly, the
/// session keeps its generation, and the WebRTC transport survives.
/// Re-registering here used to tear down a working peer.
#[test]
fn a_new_socket_replaces_a_silent_one_without_touching_webrtc() {
    use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};
    use std::time::{Duration, Instant};

    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");
    let phone_id = desktop.peer_id();
    let binding = desktop.binding.clone();
    let attempt = uuid::Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop", &phone_id, &attempt).unwrap();
    desktop
        .sessions
        .with_session(&binding, |session| {
            let mut handover = HandoverRuntime::new(attempt.clone(), wire);
            handover.record_offer("offer".into()).unwrap();
            handover.record_answer("answer".into()).unwrap();
            handover.mark_local_authenticated().unwrap();
            handover.mark_remote_authenticated().unwrap();
            handover.mark_local_capabilities_confirmed().unwrap();
            handover.mark_remote_capabilities_confirmed().unwrap();
            handover.mark_local_feature_ready().unwrap();
            handover.mark_remote_feature_ready().unwrap();
            session.webrtc_handover = Some(handover);
            // Silent past the answering window, socket still open.
            session.last_inbound_at = Instant::now() - Duration::from_secs(25);
        })
        .unwrap();

    // The production reader on the old socket.
    let reader = {
        let (binding, devices, sessions, config, events) = (
            binding.clone(),
            Arc::clone(&desktop.devices),
            Arc::clone(&desktop.sessions),
            Arc::clone(&desktop.config),
            Arc::clone(&desktop.events),
        );
        std::thread::spawn(move || {
            let epoch = binding.link.epoch();
            super::super::packet_handler::packet_read_loop(
                binding, epoch, devices, sessions, config, events,
            )
        })
    };

    // The replacement socket goes through the production attach path.
    let replacement: Arc<dyn crate::device_links::core::LinkTransport> =
        Arc::new(crate::device_links::core::link_transport::DeadTransport);
    let link = SessionLink::new(
        Arc::clone(&replacement),
        desktop.peer_certificate_pem.clone(),
        desktop.local_public_der.clone(),
        desktop.remote_public_der.clone(),
        desktop.info.clone(),
    );
    super::super::handshake::attach_bootstrap_link(
        &phone_id,
        link,
        replacement,
        true,
        false,
        Arc::clone(&desktop.config),
        Arc::clone(&desktop.devices),
        Arc::clone(&desktop.sessions),
        Arc::clone(&desktop.events),
    )
    .unwrap();

    // The old reader leaves on its own, without reporting a disconnect.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !reader.is_finished() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(reader.is_finished(), "the old socket's reader left");
    assert!(
        desktop.sessions.is_current(&binding),
        "same session, same generation, same link"
    );
    assert!(
        desktop.sessions.has_ready_webrtc(&phone_id),
        "the WebRTC transport survived the socket swap"
    );
}

/// F20: back from a short Wi-Fi drop, the phone dials in while the desk
/// still holds its old socket, open (no FIN crossed the drop) and quiet
/// since. The phone dials only when it has no link of its own, so its
/// link takes over once the old one has been quiet a few seconds, not
/// after the 20 s answering window; one heard just now is kept.
#[test]
fn a_link_the_phone_dials_replaces_one_quiet_for_a_few_seconds() {
    use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};
    use std::time::{Duration, Instant};

    let net = TestNetwork::new();
    let (desktop, _phone) = paired_pair(&net);
    let phone_id = desktop.peer_id();
    let binding = desktop.sessions.current_binding(&phone_id).unwrap();
    let attempt = uuid::Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop", &phone_id, &attempt).unwrap();
    let heard = |ago: Duration| {
        desktop
            .sessions
            .with_session(&binding, |session| {
                session.last_inbound_at = Instant::now() - ago;
            })
            .unwrap();
    };
    desktop
        .sessions
        .with_session(&binding, |session| {
            let mut handover = HandoverRuntime::new(attempt.clone(), wire);
            handover.record_offer("offer".into()).unwrap();
            handover.record_answer("answer".into()).unwrap();
            handover.mark_local_authenticated().unwrap();
            handover.mark_remote_authenticated().unwrap();
            handover.mark_local_capabilities_confirmed().unwrap();
            handover.mark_remote_capabilities_confirmed().unwrap();
            handover.mark_local_feature_ready().unwrap();
            handover.mark_remote_feature_ready().unwrap();
            session.webrtc_handover = Some(handover);
        })
        .unwrap();
    struct Quiet(std::sync::atomic::AtomicBool);
    impl crate::device_links::core::LinkTransport for Quiet {
        fn send_packet(&self, _: &NetworkPacket) -> Result<(), String> {
            Ok(())
        }
        fn read_byte(&self) -> Result<crate::device_links::core::ReadOutcome, String> {
            if self.0.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(crate::device_links::core::ReadOutcome::Closed);
            }
            std::thread::sleep(Duration::from_millis(20));
            Ok(crate::device_links::core::ReadOutcome::Idle)
        }
        fn shutdown(&self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let phone_dials = || {
        let transport: Arc<dyn crate::device_links::core::LinkTransport> =
            Arc::new(Quiet(std::sync::atomic::AtomicBool::new(false)));
        let link = SessionLink::new(
            Arc::clone(&transport),
            desktop.peer_certificate_pem.clone(),
            desktop.local_public_der.clone(),
            desktop.remote_public_der.clone(),
            desktop.info.clone(),
        )
        .dialed_by_us(false);
        super::super::handshake::attach_bootstrap_link(
            &phone_id,
            link,
            transport,
            true,
            false,
            Arc::clone(&desktop.config),
            Arc::clone(&desktop.devices),
            Arc::clone(&desktop.sessions),
            Arc::clone(&desktop.events),
        )
        .unwrap();
    };
    // Outside the link-race window, which decides two links dialed together.
    std::thread::sleep(Duration::from_millis(1_100));
    let epoch = binding.link.epoch();
    heard(Duration::ZERO);
    phone_dials();
    assert_eq!(binding.link.epoch(), epoch, "a link heard just now is kept");

    heard(Duration::from_secs(5));
    phone_dials();
    assert!(desktop.sessions.is_current(&binding), "same generation");
    assert_ne!(
        binding.link.epoch(),
        epoch,
        "the phone's new socket was adopted"
    );
    assert!(desktop.sessions.has_ready_webrtc(&phone_id));
    binding.link.close();
}

/// F20: a beacon from a phone whose link has been quiet a few seconds
/// probes that link at once. A socket the phone dropped with the network
/// is answered by a reset as soon as the phone is back, which ends it and
/// lets the redialer replace it, instead of waiting for the 10 s probe.
#[test]
fn a_beacon_probes_a_link_quiet_for_a_few_seconds() {
    use std::time::{Duration, Instant};
    let net = TestNetwork::new();
    let (desktop, _phone) = paired_pair(&net);
    let phone_id = desktop.peer_id();
    let binding = desktop.sessions.current_binding(&phone_id).unwrap();
    let heartbeats = || {
        net.sent_by(0)
            .iter()
            .filter(|p| p.packet_type == crate::device_links::packet::PACKET_TYPE_HEARTBEAT)
            .count()
    };
    let before = heartbeats();
    assert!(
        super::super::discovery::probe_silent_link(&desktop.sessions, &phone_id).is_none(),
        "a link heard just now is not probed"
    );
    desktop
        .sessions
        .with_session(&binding, |session| {
            session.last_inbound_at = Instant::now() - Duration::from_secs(5);
        })
        .unwrap();
    super::super::discovery::probe_silent_link(&desktop.sessions, &phone_id)
        .expect("probed")
        .join()
        .unwrap();
    assert_eq!(heartbeats(), before + 1);
}

/// Two dials of ours to one phone (its IPv4 and IPv6, both due at once)
/// that finish one after the other: the second must not replace the
/// first while the first is open and answering, or every reconnect
/// throws away the negotiation the first link started. A link the
/// phone dialed is still taken (it would not dial a link it can use).
#[test]
fn a_second_dial_of_ours_does_not_replace_an_answering_link() {
    let net = TestNetwork::new();
    let (desktop, _phone) = paired_pair(&net);
    let phone_id = desktop.peer_id();
    let binding = desktop.sessions.current_binding(&phone_id).unwrap();
    binding.link.note_attached(true);
    // A socket that stays open and quiet until shut down: with a dead
    // one, the new link's reader tore the session down at once, racing
    // the next step of the test (the flake).
    struct Quiet(std::sync::atomic::AtomicBool);
    impl crate::device_links::core::LinkTransport for Quiet {
        fn send_packet(&self, _: &NetworkPacket) -> Result<(), String> {
            Ok(())
        }
        fn read_byte(&self) -> Result<crate::device_links::core::ReadOutcome, String> {
            if self.0.load(std::sync::atomic::Ordering::Acquire) {
                return Ok(crate::device_links::core::ReadOutcome::Closed);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            Ok(crate::device_links::core::ReadOutcome::Idle)
        }
        fn shutdown(&self) {
            self.0.store(true, std::sync::atomic::Ordering::Release);
        }
    }
    let attach = |dialed_by_us: bool| {
        let transport: Arc<dyn crate::device_links::core::LinkTransport> =
            Arc::new(Quiet(std::sync::atomic::AtomicBool::new(false)));
        let link = SessionLink::new(
            Arc::clone(&transport),
            desktop.peer_certificate_pem.clone(),
            desktop.local_public_der.clone(),
            desktop.remote_public_der.clone(),
            desktop.info.clone(),
        )
        .dialed_by_us(dialed_by_us);
        super::super::handshake::attach_bootstrap_link(
            &phone_id,
            link,
            transport,
            true,
            false,
            Arc::clone(&desktop.config),
            Arc::clone(&desktop.devices),
            Arc::clone(&desktop.sessions),
            Arc::clone(&desktop.events),
        )
    };
    assert!(desktop.is_trusted());
    attach(true).unwrap();
    assert!(
        desktop.sessions.is_current(&binding),
        "our second dial replaced the answering link"
    );
    // Silent past the answering window: the new dial is welcome.
    desktop
        .sessions
        .with_session(&binding, |session| {
            session.last_inbound_at =
                std::time::Instant::now() - std::time::Duration::from_secs(25);
        })
        .unwrap();
    attach(true).unwrap();
    assert!(
        !desktop.sessions.is_current(&binding),
        "a silent link is replaced"
    );
    // A link the phone dialed replaces an answering one (outside the
    // link-race window, which decides two links dialed together).
    let current = desktop.sessions.current_binding(&phone_id).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1_100));
    desktop
        .sessions
        .with_session(&current, |session| {
            session.last_inbound_at = std::time::Instant::now();
        })
        .unwrap();
    assert!(
        !current.link.is_closed(),
        "the incumbent is open and answering"
    );
    attach(false).unwrap();
    assert!(!desktop.sessions.is_current(&current));
    // Its reader ends with the test.
    if let Some(last) = desktop.sessions.current_binding(&phone_id) {
        last.link.close();
    }
}

/// The phone's restart request arrives while a start of ours is under
/// way (a rebuild timer, a beacon): that start is the fresh offer the
/// phone asks for, so it is left alone. Clearing it, then losing the
/// one-start-at-a-time claim to it, left no peer, no timer and no error.
#[test]
fn a_restart_request_never_tears_down_a_start_already_under_way() {
    use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};
    let net = TestNetwork::new();
    let (desktop, _phone) = paired_pair(&net);
    let phone_id = desktop.peer_id();
    let binding = desktop.sessions.current_binding(&phone_id).unwrap();
    let attempt = uuid::Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop", &phone_id, &attempt).unwrap();
    desktop
        .sessions
        .with_session(&binding, |session| {
            session.webrtc_attempt_id = Some(attempt.clone());
            session.webrtc_handover = Some(HandoverRuntime::new(attempt.clone(), wire));
        })
        .unwrap();
    let in_flight = crate::device_links::webrtc::coordinator::claim_start(&phone_id).unwrap();
    crate::device_links::webrtc::coordinator::restart_as_initiator(
        &binding,
        Arc::clone(&desktop.sessions),
        Arc::clone(&desktop.config),
        Arc::clone(&desktop.devices),
        Arc::clone(&desktop.events),
    )
    .unwrap();
    assert_eq!(
        desktop
            .sessions
            .with_session(&binding, |s| s.webrtc_attempt_id.clone())
            .unwrap(),
        Some(attempt),
        "the start under way was torn down"
    );
    drop(in_flight);
}

/// A rebuild (or a beacon's recovery sign) while the bootstrap socket is
/// closed: the offer could never be delivered, so no peer is built, and
/// above all none that asks a public STUN server (LAN operation never
/// needs a server). The next link starts the negotiation.
#[test]
fn no_peer_is_built_over_a_closed_bootstrap_link_and_none_asks_a_server() {
    let net = TestNetwork::new();
    let (desktop, _phone) = paired_pair(&net);
    let phone_id = desktop.peer_id();
    let binding = desktop.sessions.current_binding(&phone_id).unwrap();
    binding.link.close();
    let ice = crate::device_links::webrtc::coordinator::ice_config_for(&binding, &desktop.config);
    assert!(
        ice.stun_servers.is_empty() && ice.turn_servers.is_empty(),
        "a LAN-only build asked {:?} / {:?}",
        ice.stun_servers,
        ice.turn_servers
    );
    crate::device_links::webrtc::coordinator::start_initiator(
        binding.clone(),
        Arc::clone(&desktop.sessions),
        Arc::clone(&desktop.config),
        Arc::clone(&desktop.devices),
        Arc::clone(&desktop.events),
    )
    .unwrap();
    assert_eq!(
        desktop.sessions.active_webrtc_attempt(&binding).unwrap(),
        None,
        "a peer whose offer can never leave"
    );
}

/// The LAN link's budget when the phone is awake: probed every 10 s, every 2 s
/// once its data channel went quiet (10 s while dozing: the deliberate thaw), buried after 30 s, or 8 s
/// when the data channel has been silent as well, or 3 s when it said it is
/// leaving. Dozing keeps 90 s whatever else is true.
#[test]
fn an_awake_phone_is_probed_every_two_seconds_and_judged_on_both_paths() {
    use crate::device_links::daemon::{
        heartbeat_idle, lan_dead_after, HEARTBEAT_DEAD, HEARTBEAT_DEAD_DOZING, HEARTBEAT_IDLE,
        HEARTBEAT_IDLE_AWAKE,
    };
    use std::time::Duration;
    let s = Duration::from_secs;
    // A healthy phone sees the old LAN rate; only a data channel that did not
    // answer the desk's probe escalates it.
    assert_eq!(heartbeat_idle(false, None), HEARTBEAT_IDLE);
    assert_eq!(
        heartbeat_idle(false, Some(s(2))),
        HEARTBEAT_IDLE,
        "answering"
    );
    assert_eq!(heartbeat_idle(false, Some(s(3))), HEARTBEAT_IDLE_AWAKE);
    assert_eq!(
        heartbeat_idle(true, Some(s(60))),
        HEARTBEAT_IDLE,
        "the thaw stays 10 s"
    );
    assert_eq!(lan_dead_after(false, false, None), HEARTBEAT_DEAD);
    assert_eq!(lan_dead_after(false, false, Some(s(7))), HEARTBEAT_DEAD);
    assert_eq!(lan_dead_after(false, false, Some(s(8))), s(8));
    assert_eq!(lan_dead_after(false, true, Some(s(0))), s(3));
    assert_eq!(
        lan_dead_after(true, true, Some(s(60))),
        HEARTBEAT_DEAD_DOZING
    );

    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");
    let tick = |silence: Duration, dozing: bool, data: Option<Duration>| {
        desktop.sessions.liveness_tick(
            std::time::Instant::now() + silence,
            |_| heartbeat_idle(dozing, data),
            |_| lan_dead_after(dozing, false, data),
        )
    };
    assert!(
        tick(s(5), false, Some(s(1))).probe.is_empty(),
        "healthy data: 10 s"
    );
    assert_eq!(tick(s(10), false, Some(s(1))).probe.len(), 1);
    assert!(tick(s(1), false, Some(s(4))).probe.is_empty());
    assert_eq!(
        tick(s(2), false, Some(s(4))).probe.len(),
        1,
        "suspicious: 2 s"
    );
    assert!(
        tick(s(2), true, Some(s(60))).probe.is_empty(),
        "dozing: 10 s"
    );
    assert_eq!(tick(s(10), true, Some(s(60))).probe.len(), 1);
    assert!(tick(s(8), false, None).dead.is_empty(), "LAN alone: 30 s");
    assert_eq!(
        tick(s(8), false, Some(s(9))).dead.len(),
        1,
        "both silent: 8 s"
    );
    assert!(tick(s(8), true, Some(s(9))).dead.is_empty(), "dozing: 90 s");
}

/// A dozing phone (frozen by its OS) gets 90 s of LAN silence before its
/// link is buried; an awake one keeps the 30 s budget.
#[test]
fn a_dozing_phone_keeps_its_lan_link_for_ninety_seconds() {
    use crate::device_links::daemon::{heartbeat_dead, HEARTBEAT_DEAD, HEARTBEAT_IDLE};
    use std::time::Duration;
    assert_eq!(heartbeat_dead(false), Duration::from_secs(30));
    assert_eq!(heartbeat_dead(true), Duration::from_secs(90));

    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");
    let after = |silence: Duration, dozing: bool| {
        desktop.sessions.liveness_tick(
            std::time::Instant::now() + silence,
            |_| HEARTBEAT_IDLE,
            |_| heartbeat_dead(dozing),
        )
    };
    assert_eq!(after(HEARTBEAT_DEAD, false).dead.len(), 1, "awake: 30 s");
    let dozing = after(Duration::from_secs(60), true);
    assert!(dozing.dead.is_empty(), "dozing: kept at 60 s");
    assert_eq!(
        dozing.probe.len(),
        1,
        "and still sent the thawing heartbeat"
    );
    assert_eq!(
        after(Duration::from_secs(90), true).dead.len(),
        1,
        "dozing: 90 s"
    );
}

/// A link that never pairs is cut after a minute (its heartbeats keep it
/// "alive" otherwise); a paired one is not.
#[test]
fn an_unpaired_link_idle_for_a_minute_is_closed() {
    use crate::device_links::daemon::UNPAIRED_LINK_MAX;
    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");
    let now = std::time::Instant::now();
    assert!(desktop
        .sessions
        .idle_unpaired_links(now, UNPAIRED_LINK_MAX, |_| false)
        .is_empty());
    let later = now + UNPAIRED_LINK_MAX;
    assert_eq!(
        desktop
            .sessions
            .idle_unpaired_links(later, UNPAIRED_LINK_MAX, |_| false)
            .len(),
        1
    );
    // A phone paired in the trust store is never swept, even if its
    // session has not yet marked it paired (it is redialling).
    let id = desktop.binding.device_id.clone();
    assert!(desktop
        .sessions
        .idle_unpaired_links(later, UNPAIRED_LINK_MAX, |candidate| candidate == id)
        .is_empty());
}

/// A heartbeat is consumed as liveness and never dispatched as a feature.
///
/// It arrives on an **unpaired** session here on purpose. Anything reaching
/// the feature path in that state is answered with `pair:false`, so a
/// heartbeat that fell through would tell a perfectly good peer it was not
/// paired — turning the mechanism that exists to keep links alive into one
/// that tears pairings down, once every ten seconds.
#[test]
fn a_heartbeat_is_liveness_and_never_a_feature_packet() {
    use crate::device_links::packet::PACKET_TYPE_HEARTBEAT;

    let net = TestNetwork::new();
    let (mut desktop, phone) = peer_pair(&net, "desktop-1", "phone-1");
    assert_eq!(desktop.pair_state(), PairState::NotPaired);

    for _ in 0..3 {
        phone
            .binding
            .link
            .send_packet(&NetworkPacket::new(PACKET_TYPE_HEARTBEAT))
            .expect("phone sends");
        net.advance(0);
        desktop.pump();
    }

    assert!(
        desktop.errors().is_empty(),
        "a heartbeat is not a rejected feature"
    );
    let answered = net
        .sent_by(0)
        .into_iter()
        .filter(|packet| packet.get_bool("pair") == Some(false))
        .count();
    assert_eq!(answered, 0, "and must never provoke a not-paired notice");
}

/// An unpaired link's heartbeats are liveness and nothing more: a stranger
/// alternating `dozing` true and false (as a real phone does on every
/// screen off and on) must leave no recovery state, wake no ladder and
/// start no WebRTC peer (a pipeline, a transfer manager and its thread).
#[test]
fn an_unpaired_links_heartbeats_start_nothing() {
    use crate::device_links::packet::PACKET_TYPE_HEARTBEAT;
    let net = TestNetwork::new();
    let (mut desktop, phone) = peer_pair(&net, "desktop-unpaired-hb", "phone-unpaired-hb");
    assert_eq!(desktop.pair_state(), PairState::NotPaired);
    let id = desktop.peer_id();
    for dozing in [true, false, true, false] {
        let mut beat = NetworkPacket::new(PACKET_TYPE_HEARTBEAT);
        beat.set("dozing", dozing);
        phone.binding.link.send_packet(&beat).expect("phone sends");
        net.advance(0);
        desktop.pump();
        assert!(
            !crate::device_links::webrtc::coordinator::is_dozing(&id),
            "a stranger's report is not recorded"
        );
    }
    assert!(
        desktop
            .sessions
            .active_webrtc_attempt(&desktop.binding)
            .ok()
            .flatten()
            .is_none(),
        "no peer was started for an unpaired link"
    );
    // And the explicit entry points refuse too.
    crate::device_links::webrtc::coordinator::recovery_sign(
        &id,
        &desktop.sessions,
        &desktop.config,
        &desktop.devices,
        &desktop.events,
    );
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(desktop
        .sessions
        .active_webrtc_attempt(&desktop.binding)
        .ok()
        .flatten()
        .is_none());
    assert!(crate::device_links::webrtc::coordinator::start_initiator(
        desktop.binding.clone(),
        Arc::clone(&desktop.sessions),
        Arc::clone(&desktop.config),
        Arc::clone(&desktop.devices),
        Arc::clone(&desktop.events),
    )
    .is_err());
}

/// **The livelock.** A retained link that has gone silent must not be able
/// to block the replacement the peer is offering.
///
/// Observed in both directions on real hardware: once with the desktop
/// discarding 170 fresh sockets while holding a dead one, and once with the
/// phone refusing to dial a desktop that sat idle waiting for it.
#[test]
fn a_silent_incumbent_stops_looking_alive_to_the_replacement_guard() {
    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");

    // The guard keeps a retained link only while the incumbent is still
    // answering, measured the same way here as in `finish_secure_link`.
    let threshold = crate::device_links::daemon::HEARTBEAT_IDLE * 2;
    assert!(
        desktop.silence() < threshold,
        "a fresh link counts as answering"
    );

    // The socket is still open as far as this side can tell — which is
    // precisely the state that used to be indistinguishable from healthy.
    assert!(!desktop.binding.link.is_closed());
    assert!(
        desktop
            .liveness(crate::device_links::daemon::HEARTBEAT_DEAD)
            .dead
            .len()
            == 1,
        "yet silence alone is enough to convict it"
    );
}

/// **C1/C2 — a one-sided unpair converges the next time the peer speaks.**
///
/// Spec scenario 29. The local revocation is committed even when its
/// `pair:false` cannot be delivered, so a peer that was offline keeps
/// believing it is paired. Rather than persist an outbox and retry it, the
/// side that unpaired answers the peer's next feature packet with
/// `pair:false` — upstream's design, and the reason it needs no queue.
#[test]
fn a_peer_that_missed_an_unpair_is_told_the_next_time_it_sends_anything() {
    use crate::device_links::packet::PACKET_TYPE_PING;

    let net = TestNetwork::new();
    let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    phone.accept();
    net.advance(0);
    desktop.pump();
    assert!(desktop.is_trusted() && phone.is_trusted());

    // The desktop unpairs while its `pair:false` cannot reach the phone —
    // a socket that accepts writes and delivers nothing, which is exactly
    // what a phone that walked out of range looks like.
    net.half_open(0);
    desktop.unpair();
    net.advance(1_000);
    phone.pump();
    assert!(
        !desktop.is_trusted(),
        "the desktop revoked locally regardless"
    );
    assert_eq!(
        phone.pair_state(),
        PairState::Paired,
        "the phone never heard, and has no way to know"
    );

    // The wire comes back and the phone uses the connection as normal.
    net.reconnect(0);
    let mut ping = NetworkPacket::new(PACKET_TYPE_PING);
    ping.set("message", "still here");
    phone.binding.link.send_packet(&ping).expect("phone sends");
    net.advance(0);
    desktop.pump();

    net.advance(0);
    phone.pump();
    assert_eq!(
        phone.pair_state(),
        PairState::NotPaired,
        "refused, told why, and converged on first use"
    );
}

fn paired_pair(net: &Arc<TestNetwork>) -> (TestPeer, TestPeer) {
    let (mut desktop, mut phone) = peer_pair(net, "desktop-1", "phone-1");
    desktop.request_pair();
    net.advance(0);
    phone.pump();
    phone.accept();
    net.advance(0);
    desktop.pump();
    assert!(desktop.is_trusted() && phone.is_trusted());
    (desktop, phone)
}

/// The phone unpairs: the desk drops the record on the same link.
#[test]
fn a_peers_unpair_drops_the_pairing() {
    let net = TestNetwork::new();
    let (mut desktop, phone) = paired_pair(&net);
    phone.unpair();
    net.advance(0);
    desktop.pump();
    assert!(!desktop.is_trusted(), "the trust record is gone");
    assert!(!desktop.sessions.holds_paired_transport(&desktop.peer_id()));
}

/// The first link died (a surviving WebRTC peer keeps the session); the
/// phone unpairs on the replacement link. The desk must honour it there.
#[test]
fn an_unpair_on_a_replacement_link_is_honoured() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = paired_pair(&net);
    desktop.drop_link();
    phone.drop_link();
    desktop.relink();
    phone.relink();
    phone.unpair();
    net.advance(0);
    desktop.pump();
    assert!(!desktop.is_trusted());
}

/// An unpair that was written on a link the desk has since replaced still
/// came from the pinned, authenticated peer: it is honoured, not dropped
/// as stale.
#[test]
fn an_unpair_that_arrives_on_a_superseded_link_is_honoured() {
    let net = TestNetwork::new();
    let (mut desktop, _phone) = paired_pair(&net);
    let old = desktop.binding.clone();
    // A replacement link is adopted while the old reader holds the line.
    desktop.relink();
    let mut unpair = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_PAIR);
    unpair.set("pair", false);
    apply_lan_packet(
        &old,
        unpair,
        &desktop.devices,
        &desktop.sessions,
        &desktop.config,
        &desktop.events,
    );
    assert!(!desktop.is_trusted());
    assert_eq!(desktop.pair_state(), PairState::NotPaired);
}

/// The notice is damped: a peer that still believes it is paired will send
/// at typing speed, and answering every packet would turn a stale pairing
/// into a packet storm.
#[test]
fn the_not_paired_notice_is_sent_once_not_once_per_packet() {
    use crate::device_links::packet::PACKET_TYPE_PING;

    let net = TestNetwork::new();
    let (mut desktop, phone) = peer_pair(&net, "desktop-1", "phone-1");

    // The desktop never paired; the phone behaves as though it did.
    for _ in 0..5 {
        let mut ping = NetworkPacket::new(PACKET_TYPE_PING);
        ping.set("message", "spam");
        phone.binding.link.send_packet(&ping).expect("phone sends");
        net.advance(0);
        desktop.pump();
    }

    let notices = net
        .sent_by(0)
        .into_iter()
        .filter(|packet| packet.get_bool("pair") == Some(false))
        .count();
    assert_eq!(notices, 1, "five refusals, one notice");
}

/// **S2/S3 — both users tap Pair at the same moment.**
///
/// Spec scenarios 13 and 15. Previously both sides read the other's
/// *request* as an *acceptance* and went straight to `Paired`, so the
/// pairing completed with **neither user ever shown a verification key** —
/// and each side kept its own timestamp, so the two screens would have
/// disagreed had anyone looked.
///
/// The role tie-break resolves it: whichever device id sorts lower leads,
/// and the follower abandons its own request and prompts instead. Identity
/// is a real generated device id, so the test asks which side leads rather
/// than assuming — and thereby exercises the comparison in both directions
/// across runs.
#[test]
fn crossing_pair_requests_still_produce_exactly_one_prompt_and_one_key() {
    let net = TestNetwork::new();
    let (mut left, mut right) = peer_pair(&net, "desktop-1", "phone-1");
    let (leader, follower) = if left.leads() {
        (&mut left, &mut right)
    } else {
        (&mut right, &mut left)
    };

    // Both users tap Pair before either packet has crossed.
    leader.request_pair();
    follower.request_pair();
    assert_eq!(leader.pair_state(), PairState::Requested);
    assert_eq!(follower.pair_state(), PairState::Requested);

    net.advance(0);
    leader.pump();
    follower.pump();

    assert_eq!(
        leader.pair_state(),
        PairState::Requested,
        "the leader's request stands; it ignores the crossing one"
    );
    assert_eq!(
        follower.pair_state(),
        PairState::RequestedByPeer,
        "the follower adopts the leader's negotiation and prompts"
    );
    assert!(
        !leader.is_trusted() && !follower.is_trusted(),
        "nothing is trusted until a human confirms the key"
    );
    assert_eq!(
        leader.verification_key(),
        follower.verification_key(),
        "both screens show the same key, derived from the leader's timestamp"
    );

    // And the ordinary acceptance still completes the pairing.
    follower.accept();
    net.advance(0);
    leader.pump();
    assert_eq!(leader.pair_state(), PairState::Paired);
    assert_eq!(follower.pair_state(), PairState::Paired);
}

/// **S3 — asking twice must not change the key the peer is looking at.**
///
/// A repeated request used to restamp this side's timestamp while the peer
/// sitting on the prompt ignored the duplicate and kept the original, so the
/// two users ended up comparing different codes for one pairing.
#[test]
fn asking_again_does_not_change_the_key_the_peer_is_showing() {
    let net = TestNetwork::new();
    let (desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    let shown = phone.verification_key();

    // The user gets impatient and taps Pair again a minute later.
    net.advance(60_000);
    desktop.request_pair();
    net.advance(0);
    phone.pump();

    assert_eq!(
        desktop.verification_key(),
        shown,
        "the negotiation keeps its original timestamp"
    );
    assert_eq!(phone.verification_key(), shown);
}

/// **S1 — the pairing hijack (CVE-2020-26164 in DeskLink).**
///
/// While the user is looking at a pair prompt, a second connection claiming
/// the same device id but presenting a *different* certificate replaces the
/// binding. `register_link` deliberately preserves the `PairingHandler`
/// across a replacement, so the intruder inherits the in-flight
/// `RequestedByPeer` — and `accept_pair` then writes **the intruder's**
/// certificate into the trust store.
///
/// The displayed key is not recomputed on replacement, so the user compares
/// a key that still matches the honest peer and accepts anyway. The
/// verification key is the only defence in the pre-trust window, and this
/// walks straight past it.
#[test]
fn a_replaced_link_cannot_inherit_an_in_flight_pair_prompt() {
    let net = TestNetwork::new();
    let (desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    assert_eq!(phone.pair_state(), PairState::RequestedByPeer);

    let honest_certificate = phone.trusted_certificate();
    assert!(honest_certificate.is_none(), "nothing trusted yet");
    let shown_before = phone.displayed_verification_key();
    assert!(shown_before.is_some(), "the user is shown a key");

    // The intruder connects claiming to be the desktop.
    let intruder = foreign_certificate_pem();
    phone.relink_with_certificate(intruder.clone());

    assert_eq!(
        phone.pair_state(),
        PairState::NotPaired,
        "a link replacement must abandon the negotiation it did not start"
    );

    // Even if the user taps Accept, nothing may be trusted: the prompt the
    // key belonged to is gone.
    phone.accept();
    assert_ne!(
        phone.trusted_certificate().as_deref(),
        Some(intruder.as_str()),
        "the intruder's certificate must never reach the trust store"
    );
}

/// Marks the session's WebRTC feature transport ready, as a finished
/// handover would.
fn mark_ready(peer: &TestPeer) {
    use crate::device_links::webrtc::{HandoverRuntime, WebRtcWireBinding};
    let attempt = uuid::Uuid::new_v4().to_string();
    let wire = WebRtcWireBinding::from_attempt("desktop", peer.peer_id(), &attempt).unwrap();
    peer.sessions
        .with_session(&peer.binding, |session| {
            let mut handover = HandoverRuntime::new(attempt.clone(), wire);
            handover.record_offer("offer".into()).unwrap();
            handover.record_answer("answer".into()).unwrap();
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

/// A desktop paired with its phone over a feature-ready transport, a QR
/// code on screen, and the desk's id and certificate for proofs.
fn paired_desk_showing_a_code(net: &Arc<TestNetwork>) -> (TestPeer, TestPeer, String, Vec<u8>) {
    use crate::device_links::pairing_qr;
    let (mut desktop, mut phone) = peer_pair(net, "desktop-1", "phone-1");
    desktop.request_pair();
    net.advance(0);
    phone.pump();
    phone.accept();
    net.advance(0);
    desktop.pump();
    assert!(desktop.is_trusted());
    mark_ready(&desktop);
    let desk_der = desktop
        .config
        .lock()
        .unwrap()
        .certificate()
        .to_der()
        .unwrap();
    let desk_id = desktop.local_id();
    pairing_qr::start(&desk_id, &desk_der, None, &[], 1716).unwrap();
    (desktop, phone, desk_id, desk_der)
}

/// A link claiming the phone's id with `certificate_pem`, on a wire of
/// its own: what `finish_secure_link` admits QR-only.
fn claimed_link(
    desktop: &TestPeer,
    certificate_pem: &str,
) -> (
    SessionLink,
    Arc<dyn crate::device_links::core::LinkTransport>,
) {
    let wire = TestNetwork::new();
    let transport: Arc<dyn crate::device_links::core::LinkTransport> = Arc::new(wire.end(0));
    let link = SessionLink::new(
        Arc::clone(&transport),
        certificate_pem.to_string(),
        desktop.local_public_der.clone(),
        desktop.remote_public_der.clone(),
        desktop.info.clone(),
    );
    (link, transport)
}

fn qr_proof(desk_id: &str, desk_der: &[u8], phone_id: &str, phone_pem: &str) -> NetworkPacket {
    use crate::device_links::pairing_qr;
    use base64::Engine;
    let secret = pairing_qr::live_secret().expect("code still live");
    let phone_der = openssl::x509::X509::from_pem(phone_pem.as_bytes())
        .unwrap()
        .to_der()
        .unwrap();
    let mac = pairing_qr::proof(
        &secret,
        desk_id,
        phone_id,
        &pairing_qr::fingerprint(desk_der).unwrap(),
        &pairing_qr::fingerprint(&phone_der).unwrap(),
    )
    .unwrap();
    let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_PAIR_QR);
    packet.set(
        "mac",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(mac),
    );
    packet
}

fn context(peer: &TestPeer) -> super::super::handshake::QrContext {
    super::super::handshake::QrContext {
        config: Arc::clone(&peer.config),
        devices: Arc::clone(&peer.devices),
        sessions: Arc::clone(&peer.sessions),
        events: Arc::clone(&peer.events),
    }
}

/// A stranger claiming the paired phone's id with its own certificate,
/// while a QR code is on screen, is admitted QR-only: it cannot unpair
/// the phone, raise a pairing prompt under its name, or displace its
/// session — through the QR-only link or the normal attach path.
#[test]
fn a_forged_link_with_a_new_certificate_cannot_unpair_prompt_or_displace() {
    use super::super::handshake::{attach_bootstrap_link, qr_only_packet, QrOnly};
    use crate::device_links::packet::{PACKET_TYPE_PAIR, PACKET_TYPE_WEBRTC_SIGNAL_V1};
    use crate::device_links::pairing_qr;
    let _one_code = pairing_qr::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let net = TestNetwork::new();
    let (desktop, _phone, _desk_id, _desk_der) = paired_desk_showing_a_code(&net);
    let stored = desktop.trusted_certificate().expect("stored certificate");
    let forged = foreign_certificate_pem();
    let (attacker, transport) = claimed_link(&desktop, &forged);

    let untouched = |desktop: &TestPeer| {
        assert!(desktop.is_trusted(), "still trusted");
        assert_eq!(
            desktop.trusted_certificate().as_deref(),
            Some(stored.as_str())
        );
        assert_eq!(desktop.pair_state(), PairState::Paired);
        assert!(
            desktop.sessions.is_current(&desktop.binding),
            "not displaced"
        );
        assert!(desktop.sessions.holds_paired_transport(&desktop.peer_id()));
        assert_ne!(
            desktop.status(),
            DeviceStatus::PairRequestedByPeer,
            "no prompt"
        );
    };

    for packet in [
        {
            let mut unpair = NetworkPacket::new(PACKET_TYPE_PAIR);
            unpair.set("pair", false);
            unpair
        },
        {
            let mut request = NetworkPacket::new(PACKET_TYPE_PAIR);
            request.set("pair", true);
            request.set("timestamp", 1_700_000_000i64);
            request
        },
        NetworkPacket::new(PACKET_TYPE_WEBRTC_SIGNAL_V1),
        NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_CLIPBOARD),
    ] {
        assert_eq!(
            qr_only_packet(&attacker, &packet, &desktop.config),
            QrOnly::Ignored
        );
        untouched(&desktop);
    }

    // A forged proof is refused and moves nothing.
    let mut bad = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_PAIR_QR);
    bad.set("mac", "AAAA");
    assert_eq!(
        qr_only_packet(&attacker, &bad, &desktop.config),
        QrOnly::Refused
    );
    untouched(&desktop);

    // The normal attach path refuses an unpaired link over the live,
    // paired session instead of registering it.
    assert!(attach_bootstrap_link(
        &desktop.peer_id(),
        attacker,
        transport,
        false,
        false,
        Arc::clone(&desktop.config),
        Arc::clone(&desktop.devices),
        Arc::clone(&desktop.sessions),
        Arc::clone(&desktop.events),
    )
    .is_err());
    untouched(&desktop);
    pairing_qr::cancel();
}

/// A reinstalled phone has a new certificate. While a QR code is on
/// screen its link is admitted QR-only; a valid proof from that link
/// stores the new certificate and makes it the phone's paired link.
#[test]
fn a_reinstalled_phone_re_pairs_by_qr_and_only_a_valid_proof_moves_trust() {
    use super::super::handshake::{promote_qr_link, qr_only_packet, QrOnly};
    use crate::device_links::pairing_qr;
    let _one_code = pairing_qr::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let net = TestNetwork::new();
    let (desktop, _phone, desk_id, desk_der) = paired_desk_showing_a_code(&net);
    let old_binding = desktop.binding.clone();
    let new_pem = foreign_certificate_pem();
    let (link, transport) = claimed_link(&desktop, &new_pem);

    let proof = qr_proof(&desk_id, &desk_der, &desktop.peer_id(), &new_pem);
    assert_eq!(
        qr_only_packet(&link, &proof, &desktop.config),
        QrOnly::Proven
    );
    assert_eq!(
        desktop.trusted_certificate().as_deref(),
        Some(new_pem.as_str()),
        "the proof moved trust to the new certificate"
    );
    assert!(pairing_qr::current_uri().is_none(), "the code is consumed");
    promote_qr_link(link, transport, "test".into(), context(&desktop)).unwrap();
    assert!(
        !desktop.sessions.is_current(&old_binding),
        "it replaced the old link"
    );
    let binding = desktop
        .sessions
        .current_binding(&desktop.peer_id())
        .expect("the new link's session");
    assert_eq!(binding.link.certificate_pem, new_pem);
    assert_eq!(
        desktop
            .sessions
            .with_session(&binding, |session| session.pairing.state)
            .unwrap(),
        PairState::Paired
    );
    assert!(desktop
        .events
        .lock()
        .unwrap()
        .iter()
        .any(|event| matches!(event, DaemonEvent::PairedByQr { .. })));
    binding.link.close();
}

/// A QR-only link lives only as long as its code: withdrawn (or
/// expired, or consumed), the link is closed.
#[test]
fn a_qr_only_link_is_closed_when_its_code_ends() {
    use crate::device_links::pairing_qr;
    let _one_code = pairing_qr::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let net = TestNetwork::new();
    let (desktop, _phone, _desk_id, _desk_der) = paired_desk_showing_a_code(&net);
    let code = pairing_qr::live_generation().expect("live code");
    let forged = foreign_certificate_pem();
    let (link, transport) = claimed_link(&desktop, &forged);
    let context = context(&desktop);
    let server = std::thread::spawn(move || {
        super::super::handshake::serve_qr_only(
            link,
            transport,
            "test".into(),
            code,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            context,
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(50));
    assert!(!server.is_finished(), "open while the code is live");
    pairing_qr::cancel();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !server.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(server.is_finished(), "the code ended, so did the link");
}

#[test]
fn two_peers_complete_a_pairing_handshake() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    assert_eq!(desktop.pair_state(), PairState::Requested);
    assert_eq!(net.advance(0), 1);

    phone.pump();
    assert_eq!(phone.pair_state(), PairState::RequestedByPeer);

    // Both users are shown the same code, computed from the two real keys.
    assert_eq!(desktop.verification_key(), phone.verification_key());
    assert_eq!(desktop.verification_key().len(), 6);

    phone.accept();
    assert_eq!(phone.pair_state(), PairState::Paired);
    net.advance(0);
    desktop.pump();

    assert_eq!(desktop.pair_state(), PairState::Paired);
    assert!(
        desktop.is_trusted(),
        "the desktop persists the phone's trust"
    );
    assert!(phone.is_trusted(), "the phone persists the desktop's trust");

    // The exchange terminates. `needs_pair_reconfirmation` only answers a
    // request, never an acknowledgement — proven here across two peers
    // rather than against one handler in isolation.
    net.advance(60_000);
    assert_eq!(desktop.pump() + phone.pump(), 0);
    assert!(desktop.errors().is_empty());
    assert!(phone.errors().is_empty());
}

#[test]
fn a_rejected_request_leaves_neither_side_paired() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    assert_eq!(phone.pair_state(), PairState::RequestedByPeer);

    phone.reject();
    net.advance(0);
    desktop.pump();

    assert_eq!(desktop.pair_state(), PairState::NotPaired);
    assert_eq!(phone.pair_state(), PairState::NotPaired);
    assert!(!desktop.is_trusted());
    assert!(!phone.is_trusted());
}

#[test]
fn a_duplicated_acceptance_does_not_restart_the_negotiation() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();

    // Every line from here on arrives twice.
    net.set_duplicate(true);
    phone.accept();
    net.advance(0);
    desktop.pump();

    assert_eq!(desktop.pair_state(), PairState::Paired);
    net.advance(60_000);
    desktop.pump();
    phone.pump();
    assert_eq!(desktop.pair_state(), PairState::Paired);
    assert_eq!(phone.pair_state(), PairState::Paired);
}

#[test]
fn a_request_into_a_half_open_link_never_reaches_the_peer() {
    let net = TestNetwork::new();
    let (desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");
    net.half_open(0);

    desktop.request_pair();

    // The sender believes it succeeded — a socket with no FIN gives it
    // nothing to go on. This is precisely what an application-level
    // heartbeat exists to detect, and why the desktop needs a pairing
    // timeout: today it will sit in `Requested` forever.
    assert_eq!(desktop.pair_state(), PairState::Requested);
    assert!(desktop.errors().is_empty());
    assert_eq!(net.advance(120_000), 0);
    assert_eq!(phone.pump(), 0);
    assert_eq!(phone.pair_state(), PairState::NotPaired);
}

#[test]
fn a_dropped_link_is_reconciled_to_unreachable() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.request_pair();
    net.advance(0);
    phone.pump();
    phone.accept();
    net.advance(0);
    desktop.pump();
    assert_eq!(desktop.pair_state(), PairState::Paired);

    desktop.reconcile();
    assert_eq!(desktop.status(), DeviceStatus::Paired);

    net.drop_end(0);
    desktop.binding.link.close();
    desktop.reconcile();
    assert_eq!(
        desktop.status(),
        DeviceStatus::Unreachable,
        "status is reconciled from the live snapshot, never asserted"
    );
}

/// The defect this whole phase started from: the desktop had no pairing
/// timeout, so after Android gave up at 30s this side stayed in `Requested`
/// forever, showing "Waiting…" at a peer that had stopped listening.
#[test]
fn a_request_the_peer_never_answers_is_abandoned_not_left_waiting() {
    let net = TestNetwork::new();
    let (desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");
    net.half_open(0);

    desktop.request_pair();
    assert_eq!(desktop.pair_state(), PairState::Requested);
    assert_eq!(desktop.status(), DeviceStatus::PairRequested);

    // Ticks before the deadline change nothing.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    assert!(desktop
        .expire_pairing(now + crate::device_links::pairing::PAIR_TIMEOUT_SECS - 1)
        .is_empty());
    assert_eq!(desktop.pair_state(), PairState::Requested);

    assert_eq!(
        desktop.expire_pairing(now + crate::device_links::pairing::PAIR_TIMEOUT_SECS),
        vec![desktop.peer_id()]
    );
    assert_eq!(desktop.pair_state(), PairState::NotPaired);
    assert_ne!(
        desktop.status(),
        DeviceStatus::PairRequested,
        "the prompt clears without waiting for the device to go unreachable"
    );

    // The peer never saw any of it, and is not left half-paired.
    assert_eq!(phone.pump(), 0);
    assert_eq!(phone.pair_state(), PairState::NotPaired);
}

/// Regression guard for the rule in `SessionLink::transport`.
///
/// A closed link must fail a send with text `should_surface_send_error`
/// suppresses. If a transport ever invents its own wording, a routine
/// reconnect becomes a UI banner once a second from the clipboard poller.
#[test]
fn a_send_on_a_closed_link_is_never_surfaced_as_a_feature_failure() {
    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "desktop-1", "phone-1");

    desktop.binding.link.close();
    let error = desktop
        .binding
        .link
        .send_packet(&NetworkPacket::new(
            crate::device_links::packet::PACKET_TYPE_PING,
        ))
        .expect_err("a closed link cannot send");
    assert!(!crate::device_links::transport::should_surface_send_error(
        &error
    ));
}

fn pair_request(timestamp: i64) -> NetworkPacket {
    let mut request = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_PAIR);
    request.set("pair", true);
    request.set("timestamp", timestamp);
    request
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// Clock skew: a request stamped more than 5 minutes off used to be
/// dropped without a word, so the asking phone waited out its 30 s and said
/// "timed out". Now it is refused at once and the card says why. A replayed
/// request from hours ago is the same case.
#[test]
fn a_request_from_a_skewed_clock_is_refused_in_words_and_never_prompts() {
    for skew in [-7_200, 7_200, -86_400] {
        let net = TestNetwork::new();
        let (mut desktop, mut phone) = peer_pair(&net, "desktop-1", "phone-1");
        phone
            .binding
            .link
            .send_packet(&pair_request(now_secs() + skew))
            .unwrap();
        net.advance(0);
        desktop.pump();
        assert_eq!(desktop.pair_state(), PairState::NotPaired, "no prompt");
        assert!(desktop.displayed_verification_key().is_none());
        let error = desktop.devices.lock().unwrap()[&desktop.peer_id()]
            .last_error
            .clone()
            .unwrap_or_default();
        assert_eq!(error, crate::device_links::pairing::CLOCKS_DIFFER);
        // The asking side hears it at once, not after its timeout.
        net.advance(0);
        phone.pump();
        assert_eq!(phone.pair_state(), PairState::NotPaired);
        assert!(!desktop.is_trusted() && !phone.is_trusted());
    }
    // Inside the window it prompts as ever.
    let net = TestNetwork::new();
    let (mut desktop, phone) = peer_pair(&net, "desktop-1", "phone-1");
    phone
        .binding
        .link
        .send_packet(&pair_request(now_secs() - 120))
        .unwrap();
    net.advance(0);
    desktop.pump();
    assert_eq!(desktop.pair_state(), PairState::RequestedByPeer);
}

/// Defence at the pairing state machine's entrance: whatever carrier a
/// pairing packet arrives on, a relay link cannot start, answer or QR-prove
/// a pairing while off-LAN pairing is compiled out; its `pair:false` still
/// unpairs.
#[test]
fn a_relay_link_cannot_start_or_answer_a_pairing_and_can_still_unpair() {
    use crate::device_links::packet::PACKET_TYPE_PAIR;
    use crate::device_links::relay_link::transport::RelayLinkTransport;
    let dir = temp_dir("relay-gate");
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let sessions = Arc::new(DeviceManager::new());
    let devices: Arc<Mutex<HashMap<String, DeviceView>>> = Arc::default();
    let events: Arc<Mutex<Vec<DaemonEvent>>> = Arc::default();
    let (tx, _rx) = std::sync::mpsc::channel();
    let info = DeviceInfo {
        id: "relay-phone".into(),
        name: "relay-phone".into(),
        device_type: "phone".into(),
        protocol_version: PROTOCOL_VERSION,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    let link = SessionLink::new(
        Arc::new(RelayLinkTransport::new("fp".into(), tx)),
        String::new(),
        Vec::new(),
        Vec::new(),
        info,
    );
    let binding = sessions
        .register_link("relay-phone".into(), link, false)
        .unwrap()
        .binding;
    let state = || {
        sessions
            .with_session(&binding, |session| session.pairing.state)
            .unwrap()
    };
    let apply = |packet: NetworkPacket| {
        apply_lan_packet(&binding, packet, &devices, &sessions, &config, &events)
    };
    apply(pair_request(now_secs()));
    assert_eq!(
        state(),
        PairState::NotPaired,
        "a request over a relay is dropped"
    );
    let mut accept = NetworkPacket::new(PACKET_TYPE_PAIR);
    accept.set("pair", true);
    let _ = sessions.with_session(&binding, |session| {
        session.pairing.request_packet().unwrap();
    });
    apply(accept);
    assert_eq!(
        state(),
        PairState::Requested,
        "an acceptance over a relay is dropped"
    );
    let mut unpair = NetworkPacket::new(PACKET_TYPE_PAIR);
    unpair.set("pair", false);
    apply(unpair);
    assert_eq!(
        state(),
        PairState::NotPaired,
        "pair:false is the one pairing packet it carries"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// Re-pair after an unpair, from either side: the record is made again with
/// the same certificate, both prompts show one key, and nothing of the old
/// pairing is needed or left over.
#[test]
fn a_pairing_is_made_again_after_an_unpair_from_either_side() {
    for desk_unpairs in [true, false] {
        let net = TestNetwork::new();
        let (mut desktop, mut phone) = paired_pair(&net);
        let certificate = desktop.trusted_certificate();
        assert!(certificate.is_some());
        if desk_unpairs {
            desktop.unpair();
        } else {
            phone.unpair();
        }
        net.advance(0);
        desktop.pump();
        phone.pump();
        assert!(!desktop.is_trusted() && !phone.is_trusted());
        assert_eq!(desktop.pair_state(), PairState::NotPaired);
        assert_eq!(phone.pair_state(), PairState::NotPaired);

        desktop.request_pair();
        net.advance(0);
        phone.pump();
        assert_eq!(phone.pair_state(), PairState::RequestedByPeer);
        assert_eq!(desktop.verification_key(), phone.verification_key());
        phone.accept();
        net.advance(0);
        desktop.pump();
        assert_eq!(desktop.pair_state(), PairState::Paired);
        assert!(desktop.is_trusted() && phone.is_trusted());
        assert_eq!(desktop.trusted_certificate(), certificate);
        assert!(desktop.errors().is_empty() && phone.errors().is_empty());
    }
}

fn relay_session(
    config: &Arc<Mutex<Config>>,
    sessions: &Arc<DeviceManager>,
    pem: &str,
    paired: bool,
) -> (SessionBinding, DeviceInfo) {
    let _ = config;
    let (tx, _rx) = std::sync::mpsc::channel();
    let info = DeviceInfo {
        id: "relay-phone".into(),
        name: "relay-phone".into(),
        device_type: "phone".into(),
        protocol_version: PROTOCOL_VERSION,
        incoming_capabilities: Vec::new(),
        outgoing_capabilities: Vec::new(),
        instance_id: None,
        app_version: None,
    };
    let link = SessionLink::new(
        Arc::new(
            crate::device_links::relay_link::transport::RelayLinkTransport::new("fp".into(), tx),
        ),
        pem.to_string(),
        Vec::new(),
        Vec::new(),
        info.clone(),
    );
    let binding = sessions
        .register_link("relay-phone".into(), link, paired)
        .unwrap()
        .binding;
    (binding, info)
}

/// The QR leg of the relay gate, with a proof that would pair on the LAN:
/// over a relay link it pairs nothing and does not consume the code; the
/// same proof on a LAN-kind link pairs (so it is the gate that stopped it).
#[test]
fn a_valid_qr_proof_over_a_relay_link_pairs_nothing_and_is_not_consumed() {
    use super::super::handshake::{qr_only_packet, QrOnly};
    use crate::device_links::pairing_qr;
    let _one_code = pairing_qr::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = temp_dir("relay-qr");
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let sessions = Arc::new(DeviceManager::new());
    let devices: Arc<Mutex<HashMap<String, DeviceView>>> = Arc::default();
    let events: Arc<Mutex<Vec<DaemonEvent>>> = Arc::default();
    let (desk_id, desk_der) = {
        let config = config.lock().unwrap();
        (
            config.local_device_info().id,
            config.certificate().to_der().unwrap(),
        )
    };
    pairing_qr::start(&desk_id, &desk_der, None, &[], 1716).unwrap();
    let pem = foreign_certificate_pem();
    let (binding, info) = relay_session(&config, &sessions, &pem, false);
    let proof = qr_proof(&desk_id, &desk_der, "relay-phone", &pem);

    apply_lan_packet(
        &binding,
        proof.clone(),
        &devices,
        &sessions,
        &config,
        &events,
    );
    assert!(!config.lock().unwrap().is_trusted("relay-phone"));
    assert!(
        pairing_qr::live_secret().is_some(),
        "the code is not consumed"
    );
    assert!(events.lock().unwrap().is_empty());

    // The same proof on a LAN-kind link with that certificate pairs.
    let wire = TestNetwork::new();
    let lan = SessionLink::new(Arc::new(wire.end(0)), pem, Vec::new(), Vec::new(), info);
    assert_eq!(qr_only_packet(&lan, &proof, &config), QrOnly::Proven);
    assert!(config.lock().unwrap().is_trusted("relay-phone"));
    pairing_qr::cancel();
    let _ = std::fs::remove_dir_all(dir);
}

/// `pair:false` is the one pairing packet a relay carries: it unpairs a
/// paired phone, drops the trust record and forgets what the desk held
/// for it (here: the unlock nonce it was offered).
#[test]
fn a_pair_false_over_a_relay_link_unpairs_and_forgets_the_phone() {
    use crate::device_links::unlock::{approve, issue, Conditions, Refusal};
    let dir = temp_dir("relay-unpair");
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let sessions = Arc::new(DeviceManager::new());
    let devices: Arc<Mutex<HashMap<String, DeviceView>>> = Arc::default();
    let events: Arc<Mutex<Vec<DaemonEvent>>> = Arc::default();
    let pem = foreign_certificate_pem();
    let (binding, info) = relay_session(&config, &sessions, &pem, true);
    config.lock().unwrap().trust_device(&info, pem).unwrap();
    let nonce = issue("relay-phone");
    let mut unpair = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_PAIR);
    unpair.set("pair", false);

    apply_lan_packet(&binding, unpair, &devices, &sessions, &config, &events);

    assert!(!config.lock().unwrap().is_trusted("relay-phone"));
    assert_eq!(
        sessions
            .with_session(&binding, |session| session.pairing.state)
            .unwrap(),
        PairState::NotPaired
    );
    let all_good = Conditions {
        allowed: true,
        locked: true,
        local: true,
        ready: true,
        unlockable: true,
    };
    assert_eq!(
        approve("relay-phone", Some(&nonce), all_good),
        Err(Refusal::NoNonce),
        "forget_peer ran"
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// A relay link that has closed still says it is a relay link: a queued
/// pairing packet cannot slip through as "LAN".
#[test]
fn a_closed_relay_link_still_reports_the_relay_carrier() {
    use crate::device_links::pairing_transport::PairingTransportKind;
    let dir = temp_dir("relay-closed");
    let config = Arc::new(Mutex::new(Config::load_from_dir(dir.clone()).unwrap()));
    let sessions = Arc::new(DeviceManager::new());
    let (binding, _) = relay_session(&config, &sessions, "", false);
    assert_eq!(
        binding.link.pairing_transport(),
        PairingTransportKind::Relay
    );
    binding.link.close();
    assert_eq!(
        binding.link.pairing_transport(),
        PairingTransportKind::Relay
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The computer's addresses change while a link is already admitted for
/// the code: the refresh must not close it, and its proof still pairs.
#[test]
fn a_qr_only_link_survives_an_address_refresh_and_still_pairs() {
    use crate::device_links::core::LinkTransport;
    use crate::device_links::pairing_qr;
    let _one_code = pairing_qr::TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let net = TestNetwork::new();
    let (desktop, _phone, desk_id, desk_der) = paired_desk_showing_a_code(&net);
    let new_pem = foreign_certificate_pem();
    let wire = TestNetwork::new();
    let transport: Arc<dyn LinkTransport> = Arc::new(wire.end(0));
    let phone_end = wire.end(1);
    let link = SessionLink::new(
        Arc::clone(&transport),
        new_pem.clone(),
        desktop.local_public_der.clone(),
        desktop.remote_public_der.clone(),
        desktop.info.clone(),
    );
    let generation = pairing_qr::live_generation().expect("live code");
    let context = context(&desktop);
    let server = std::thread::spawn(move || {
        super::super::handshake::serve_qr_only(
            link,
            transport,
            "test".into(),
            generation,
            std::time::Instant::now() + std::time::Duration::from_secs(60),
            context,
        )
    });
    std::thread::sleep(std::time::Duration::from_millis(50));

    // The network changed: same code, new addresses.
    let addresses = ["10.9.8.7".parse().unwrap()];
    assert!(pairing_qr::refresh_addresses(
        Some("10.9.8.7:1716".into()),
        &addresses,
        1716
    ));
    assert_eq!(pairing_qr::live_generation(), Some(generation));
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert!(!server.is_finished(), "the refresh did not close the link");

    let proof = qr_proof(&desk_id, &desk_der, &desktop.peer_id(), &new_pem);
    phone_end.send_packet(&proof).unwrap();
    wire.advance(0);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !server.is_finished() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert!(server.is_finished());
    assert_eq!(
        desktop.trusted_certificate().as_deref(),
        Some(new_pem.as_str()),
        "the proof was read after the refresh and paired"
    );
    if let Some(binding) = desktop.sessions.current_binding(&desktop.peer_id()) {
        binding.link.close();
    }
    pairing_qr::cancel();
}

/// Two peers whose identities advertise WebRTC signaling, the first being
/// the one whose device id sorts LOWER (`lower_is_desk`: the desk is then the
/// deterministic WebRTC initiator) or HIGHER. Returns `(desk, phone)`.
fn pair_by_roles(net: &Arc<TestNetwork>, lower_is_desk: bool) -> (TestPeer, TestPeer) {
    let (a, b) = super::peer_pair_with(
        net,
        "desk-role",
        "phone-role",
        &[crate::device_links::packet::PACKET_TYPE_WEBRTC_SIGNAL_V1],
    );
    let a_lower = a.local_id() < b.local_id();
    if a_lower == lower_is_desk {
        (a, b)
    } else {
        (b, a)
    }
}

/// The packet types waiting on `peer`'s end of the wire, read raw (nothing is
/// applied: the phone side of these tests is Android, which reacts to nothing
/// a desktop test peer would).
fn waiting_types(peer: &TestPeer) -> Vec<String> {
    use crate::device_links::core::ReadOutcome;
    let mut line = Vec::new();
    let mut types = Vec::new();
    while let Ok(ReadOutcome::Byte(byte)) = peer.binding.link.read_byte() {
        line.push(byte);
        if byte == b'\n' {
            if let Ok(packet) = NetworkPacket::deserialize(&line) {
                types.push(packet.packet_type);
            }
            line.clear();
        }
    }
    types
}

fn wait_for(within: std::time::Duration, mut done: impl FnMut() -> bool) -> bool {
    let until = std::time::Instant::now() + within;
    while std::time::Instant::now() < until {
        if done() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    done()
}

/// F-QA2-1: pairing STARTED FROM THE PHONE, the desk accepts, and the desk is
/// the deterministic initiator (its id sorts lower). The phone then waits for
/// an offer that nothing ever sent (it started nothing: Android's side of the
/// phone-started pairing only offers when IT is the initiator), the link idled
/// on heartbeats until the phone app restarted. Accepting must start the
/// negotiation, with no reconnect.
#[test]
fn a_phone_started_pairing_the_desk_accepts_starts_the_negotiation_when_the_desk_initiates() {
    let net = TestNetwork::new();
    let (mut desk, phone) = pair_by_roles(&net, true);
    assert!(desk.local_id() < phone.local_id());
    phone.request_pair();
    net.advance(0);
    desk.pump();
    desk.accept();
    assert!(desk.is_trusted());
    assert!(
        wait_for(std::time::Duration::from_secs(5), || desk
            .sessions
            .active_webrtc_attempt(&desk.binding)
            .ok()
            .flatten()
            .is_some()),
        "the desk never offered: the connection waits for a reconnect"
    );
}

/// The mirror: the phone's id sorts lower, so it offers by itself; the desk
/// (the responder) asks once after 3 s if nothing arrived, as for any fresh
/// link. Accepting still schedules that, and it is the only thing asked.
#[test]
fn a_phone_started_pairing_the_desk_accepts_asks_the_phone_to_offer_when_the_phone_initiates() {
    let net = TestNetwork::new();
    let (mut desk, phone) = pair_by_roles(&net, false);
    assert!(desk.local_id() > phone.local_id());
    phone.request_pair();
    net.advance(0);
    desk.pump();
    desk.accept();
    assert!(desk.is_trusted());
    let signal = crate::device_links::packet::PACKET_TYPE_WEBRTC_SIGNAL_V1;
    let mut seen = Vec::new();
    assert!(
        wait_for(std::time::Duration::from_secs(6), || {
            net.advance(0);
            seen.extend(waiting_types(&phone));
            seen.iter().any(|kind| kind == signal)
        }),
        "no restart request reached the phone: {seen:?}"
    );
    assert!(
        desk.sessions
            .active_webrtc_attempt(&desk.binding)
            .ok()
            .flatten()
            .is_none(),
        "the responder never offers"
    );
}

/// A phone whose identity does not take WebRTC signaling is not pushed into a
/// negotiation it cannot have (the capability is the link's identity).
#[test]
fn accepting_a_phone_that_takes_no_webrtc_starts_nothing() {
    let net = TestNetwork::new();
    let (a, b) = peer_pair(&net, "desk-nocap", "phone-nocap");
    let (mut desk, phone) = if a.local_id() < b.local_id() {
        (a, b)
    } else {
        (b, a)
    };
    phone.request_pair();
    net.advance(0);
    desk.pump();
    desk.accept();
    std::thread::sleep(std::time::Duration::from_millis(600));
    assert!(desk
        .sessions
        .active_webrtc_attempt(&desk.binding)
        .ok()
        .flatten()
        .is_none());
}

/// F-QA2-4: the desk unpaired a phone it could not tell (the link was gone).
/// The phone, unpaired for the desk but not for itself, only heartbeats on its
/// next link, and the desk ignores an unpaired session's heartbeats on
/// purpose: it was left on "Connecting..." for minutes. The desk now owes it
/// a `pair:false` and answers on its next IDENTITY, not on heartbeats.
#[test]
fn an_unpair_nobody_heard_is_answered_on_the_devices_next_identity() {
    use crate::device_links::packet::PACKET_TYPE_HEARTBEAT;
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = paired_pair(&net);
    desktop.drop_link();
    desktop.unpair();
    assert!(!desktop.is_trusted());
    phone.drop_link();
    net.advance(0);
    desktop.relink();
    phone.relink();
    assert!(phone.is_trusted(), "the phone never heard");
    // Heartbeats on the new link are not the answer.
    let mut beat = NetworkPacket::new(PACKET_TYPE_HEARTBEAT);
    beat.set("dozing", false);
    phone.binding.link.send_packet(&beat).expect("sends");
    net.advance(0);
    desktop.pump();
    phone.pump();
    assert!(phone.is_trusted(), "a heartbeat does not answer it");
    // The identity does.
    super::super::handshake::answer_owed_not_paired(&desktop.binding, false);
    net.advance(0);
    phone.pump();
    assert!(!phone.is_trusted(), "told on the next identity");
}

/// An unpair that was delivered owes nothing; the debt is bounded in count
/// and age, and a pairing clears it.
#[test]
fn the_owed_unpair_is_bounded_and_cleared_by_a_pairing() {
    use super::super::pairing_commands::{
        clear_owed_not_paired, owe_not_paired, take_owed_not_paired, OWED_MAX_AGE, OWED_MAX_ANSWERS,
    };
    let now = std::time::Instant::now();
    let id = "owed-bounds-phone";
    assert!(!take_owed_not_paired(id, now), "nothing owed");
    owe_not_paired(id);
    for _ in 0..OWED_MAX_ANSWERS {
        assert!(take_owed_not_paired(id, now));
    }
    assert!(
        !take_owed_not_paired(id, now),
        "answered at most {OWED_MAX_ANSWERS} times"
    );
    owe_not_paired(id);
    assert!(
        !take_owed_not_paired(id, now + OWED_MAX_AGE + std::time::Duration::from_secs(1)),
        "too old"
    );
    owe_not_paired(id);
    clear_owed_not_paired(id);
    assert!(!take_owed_not_paired(id, now), "a pairing ends the debt");

    // A delivered unpair (link up) owes nothing.
    let net = TestNetwork::new();
    let (desktop, _phone) = paired_pair(&net);
    desktop.unpair();
    assert!(!take_owed_not_paired(&desktop.peer_id(), now));
}

/// A paired link is never sent the answer (it is for unpaired identities).
#[test]
fn a_paired_link_is_not_told_it_is_unpaired() {
    use super::super::pairing_commands::owe_not_paired;
    let net = TestNetwork::new();
    let (desktop, phone) = paired_pair(&net);
    owe_not_paired(&desktop.peer_id());
    super::super::handshake::answer_owed_not_paired(&desktop.binding, true);
    net.advance(0);
    assert!(phone.is_trusted());
    assert!(waiting_types(&phone).is_empty());
    super::super::pairing_commands::clear_owed_not_paired(&desktop.peer_id());
}

/// The production attach path for a fresh link of `desk`'s phone: what a
/// reconnecting phone goes through (`paired` as the trust store says).
fn attach_fresh_link(desk: &mut TestPeer, net: &Arc<TestNetwork>) {
    net.reconnect(desk.end);
    let transport: Arc<dyn crate::device_links::core::LinkTransport> = Arc::new(net.end(desk.end));
    let link = SessionLink::new(
        Arc::clone(&transport),
        desk.peer_certificate_pem.clone(),
        desk.local_public_der.clone(),
        desk.remote_public_der.clone(),
        desk.info.clone(),
    );
    super::super::handshake::attach_bootstrap_link(
        &desk.peer_id(),
        link,
        transport,
        desk.is_trusted(),
        false,
        Arc::clone(&desk.config),
        Arc::clone(&desk.devices),
        Arc::clone(&desk.sessions),
        Arc::clone(&desk.events),
    )
    .unwrap();
    desk.binding = desk.sessions.current_binding(&desk.peer_id()).unwrap();
}

fn desk_attempt(desk: &TestPeer) -> bool {
    desk.sessions
        .active_webrtc_attempt(&desk.binding)
        .ok()
        .flatten()
        .is_some()
}

/// The owed `pair:false` through the PRODUCTION wiring (`attach_bootstrap_link`
/// on the device's next link), not a hand call.
#[test]
fn the_owed_unpair_is_answered_by_the_production_attach_path() {
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = paired_pair(&net);
    desktop.drop_link();
    desktop.unpair();
    phone.drop_link();
    phone.relink();
    attach_fresh_link(&mut desktop, &net);
    net.advance(0);
    phone.pump();
    assert!(!phone.is_trusted(), "told on its next identity");
}

/// A live QR code: a same-certificate phone re-pairing by scan sends its proof
/// on the link, and a `pair:false` ahead of it would decline the pairing.
#[test]
fn an_owed_unpair_is_not_sent_while_a_qr_code_is_live() {
    use crate::device_links::pairing_qr;
    let net = TestNetwork::new();
    let (mut desktop, mut phone) = paired_pair(&net);
    desktop.drop_link();
    desktop.unpair();
    phone.drop_link();
    phone.relink();
    let der = desktop
        .config
        .lock()
        .unwrap()
        .certificate()
        .to_der()
        .unwrap();
    let id = desktop.local_id();
    pairing_qr::start(&id, &der, None, &[], 1716).unwrap();
    attach_fresh_link(&mut desktop, &net);
    net.advance(0);
    phone.pump();
    pairing_qr::cancel();
    assert!(phone.is_trusted(), "nothing declined the scan");
    // Starting a code (the command) also ends every debt.
    super::super::pairing_commands::owe_not_paired(&desktop.peer_id());
    super::super::pairing_commands::clear_all_owed_not_paired();
    assert!(!super::super::pairing_commands::take_owed_not_paired(
        &desktop.peer_id(),
        std::time::Instant::now()
    ));
}

/// Starting a pairing here ends the debt: a link flap right after "Pair"
/// must not decline the phone's pending request.
#[test]
fn pressing_pair_ends_the_owed_unpair() {
    let net = TestNetwork::new();
    let (desktop, _phone) = peer_pair(&net, "owed-pair-d", "owed-pair-p");
    super::super::pairing_commands::owe_not_paired(&desktop.peer_id());
    desktop.request_pair();
    assert!(!super::super::pairing_commands::take_owed_not_paired(
        &desktop.peer_id(),
        std::time::Instant::now()
    ));
}

/// The sibling orientation of F-QA2-1: the phone asked, and the desk USER
/// presses Pair (not Accept) while the request is pending. `request_packet`
/// in `RequestedByPeer` is an acceptance and must start the negotiation too.
#[test]
fn pressing_pair_on_a_pending_phone_request_starts_the_negotiation() {
    let net = TestNetwork::new();
    let (mut desk, phone) = pair_by_roles(&net, true);
    phone.request_pair();
    net.advance(0);
    desk.pump();
    assert_eq!(desk.pair_state(), PairState::RequestedByPeer);
    desk.request_pair();
    assert!(desk.is_trusted());
    assert!(
        wait_for(std::time::Duration::from_secs(5), || desk_attempt(&desk)),
        "the desk never offered"
    );
}

/// A second Accept finds no request: it errors and spawns nothing more.
#[test]
fn a_double_accept_starts_one_negotiation() {
    let net = TestNetwork::new();
    let (mut desk, phone) = pair_by_roles(&net, true);
    phone.request_pair();
    net.advance(0);
    desk.pump();
    desk.accept();
    assert!(wait_for(
        std::time::Duration::from_secs(5),
        || desk_attempt(&desk)
    ));
    let first = desk.sessions.active_webrtc_attempt(&desk.binding).unwrap();
    let before = desk.errors.lock().unwrap().len();
    desk.accept();
    assert!(
        desk.errors.lock().unwrap().len() > before,
        "no request to accept"
    );
    std::thread::sleep(std::time::Duration::from_millis(1_500));
    assert_eq!(
        desk.sessions.active_webrtc_attempt(&desk.binding).unwrap(),
        first,
        "one attempt"
    );
}

/// Desk-started pairing, both orientations: the phone accepts and the desk
/// negotiates (initiator) or asks the phone to (responder).
#[test]
fn a_desk_started_pairing_reaches_the_negotiation_in_both_orientations() {
    for lower_is_desk in [true, false] {
        let net = TestNetwork::new();
        let (mut desk, mut phone) = pair_by_roles(&net, lower_is_desk);
        desk.request_pair();
        net.advance(0);
        phone.pump();
        phone.accept();
        net.advance(0);
        desk.pump();
        assert!(desk.is_trusted());
        if lower_is_desk {
            assert!(
                wait_for(std::time::Duration::from_secs(5), || desk_attempt(&desk)),
                "the initiating desk offered"
            );
        } else {
            // The phone is the initiator: its own acceptance starts it.
            assert!(
                wait_for(std::time::Duration::from_secs(5), || {
                    net.advance(0);
                    phone
                        .sessions
                        .active_webrtc_attempt(&phone.binding)
                        .ok()
                        .flatten()
                        .is_some()
                }),
                "the initiating phone offered"
            );
        }
    }
}

/// An unpair right after the accept: the scheduled retries see the device is
/// no longer paired and offer nothing.
#[test]
fn an_unpair_right_after_the_accept_stops_the_scheduled_offer() {
    let net = TestNetwork::new();
    let (mut desk, phone) = pair_by_roles(&net, true);
    phone.request_pair();
    net.advance(0);
    desk.pump();
    desk.accept();
    desk.unpair();
    std::thread::sleep(std::time::Duration::from_millis(1_500));
    assert!(!desk_attempt(&desk), "no offer for an unpaired phone");
}
