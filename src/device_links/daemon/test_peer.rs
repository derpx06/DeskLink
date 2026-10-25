//! Two real desktop peers, wired to each other through an in-memory link.
//!
//! Each `TestPeer` owns the same four things the daemon owns — a `Config` with
//! a real key and certificate, a `DeviceManager`, a device view map and an
//! error sink — and drives the **production** pairing commands
//! ([`PairingContext`]) and the **production** inbound dispatch
//! ([`apply_lan_packet`]). Nothing is reimplemented, so a bug in either is a
//! failing test here.
//!
//! **Ceiling, stated deliberately.** Both peers are desktop code. This proves
//! the desktop's state machine converges against itself and catches the
//! ping-pong, half-open, duplicate, reorder and drop classes. It does *not*
//! prove desktop↔Android agreement — that needs the shared wire fixtures
//! replayed from both suites.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::handle::DaemonEvent;
use super::packet_handler::apply_lan_packet;
use super::pairing_commands::PairingContext;
use super::state::reconcile_connection_status;
use crate::device_links::config::Config;
use crate::device_links::core::test_link::TestNetwork;
use crate::device_links::core::{DeviceManager, LivenessTick, SessionBinding, SessionLink};
use crate::device_links::device::{DeviceStatus, DeviceView};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::{NetworkPacket, PROTOCOL_VERSION};
use crate::device_links::pairing::PairState;

pub(crate) struct TestPeer {
    pub(crate) sessions: Arc<DeviceManager>,
    pub(crate) devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    pub(crate) config: Arc<Mutex<Config>>,
    pub(crate) errors: Arc<Mutex<Vec<String>>>,
    pub(crate) events: Arc<Mutex<Vec<DaemonEvent>>>,
    pub(crate) binding: SessionBinding,
    peer_id: String,
    config_dir: std::path::PathBuf,
    line: Vec<u8>,
    /// Everything needed to build a *second* link to the same peer.
    ///
    /// Held so [`TestPeer::relink`] can rebuild a `SessionLink` after a drop
    /// without re-deriving keys, and so [`TestPeer::relink_with_certificate`]
    /// can forge one that presents a different certificate — the pairing-hijack
    /// case, which is otherwise unreachable from a test.
    net: Arc<TestNetwork>,
    end: usize,
    info: DeviceInfo,
    local_public_der: Vec<u8>,
    remote_public_der: Vec<u8>,
    peer_certificate_pem: String,
}

/// Builds both peers at once: each needs the other's public key to compute a
/// verification key, so they cannot be constructed independently.
///
/// `left_label` and `right_label` name the temp directories only. **Identity is
/// each `Config`'s own device id**, not the label — the labels used to be the
/// session keys while `local_device_info().id` was an unrelated UUID, which put
/// the two halves of every "am I talking to the right device" comparison in
/// different namespaces and silently defeated `local_goes_first`.
pub(crate) fn peer_pair(
    net: &Arc<TestNetwork>,
    left_label: &str,
    right_label: &str,
) -> (TestPeer, TestPeer) {
    let left_dir = temp_dir(left_label);
    let right_dir = temp_dir(right_label);
    let left_config = Config::load_from_dir(left_dir.clone()).expect("left config");
    let right_config = Config::load_from_dir(right_dir.clone()).expect("right config");
    let left_id = left_config.local_device_info().id;
    let right_id = right_config.local_device_info().id;

    let left_der = left_config
        .key()
        .public_key_to_der()
        .expect("left public key");
    let right_der = right_config
        .key()
        .public_key_to_der()
        .expect("right public key");
    let left_pem = String::from_utf8(left_config.certificate().to_pem().expect("left pem"))
        .expect("left pem utf8");
    let right_pem = String::from_utf8(right_config.certificate().to_pem().expect("right pem"))
        .expect("right pem utf8");

    let left = TestPeer::new(
        left_config,
        left_dir,
        &right_id,
        Arc::clone(net),
        0,
        left_der.clone(),
        right_der.clone(),
        right_pem,
    );
    let right = TestPeer::new(
        right_config,
        right_dir,
        &left_id,
        Arc::clone(net),
        1,
        right_der,
        left_der,
        left_pem,
    );
    (left, right)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "desklink-peer-{tag}-{}",
        uuid::Uuid::new_v4().simple()
    ))
}

impl TestPeer {
    #[allow(clippy::too_many_arguments)]
    fn new(
        config: Config,
        config_dir: std::path::PathBuf,
        peer_id: &str,
        net: Arc<TestNetwork>,
        end: usize,
        local_public_der: Vec<u8>,
        remote_public_der: Vec<u8>,
        peer_certificate_pem: String,
    ) -> Self {
        let link = net.end(end);
        let sessions = Arc::new(DeviceManager::new());
        let devices = Arc::new(Mutex::new(HashMap::new()));

        let info = DeviceInfo {
            id: peer_id.to_string(),
            name: peer_id.to_string(),
            device_type: "phone".to_string(),
            protocol_version: PROTOCOL_VERSION,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        devices.lock().unwrap().insert(
            peer_id.to_string(),
            DeviceView::from_info(&info, "test-link".to_string(), false),
        );

        let session_link = SessionLink::new(
            Arc::new(link),
            peer_certificate_pem.clone(),
            local_public_der.clone(),
            remote_public_der.clone(),
            info.clone(),
        );
        let binding = sessions
            .register_link(peer_id.to_string(), session_link, false)
            .expect("register link")
            .binding;

        Self {
            sessions,
            devices,
            config: Arc::new(Mutex::new(config)),
            errors: Arc::new(Mutex::new(Vec::new())),
            events: Arc::new(Mutex::new(Vec::new())),
            binding,
            peer_id: peer_id.to_string(),
            config_dir,
            line: Vec::new(),
            net,
            end,
            info,
            local_public_der,
            remote_public_der,
            peer_certificate_pem,
        }
    }

    /// Drops this end of the wire, as a peer vanishing without a FIN would.
    ///
    /// The session keeps its `PairingHandler` — that is production behaviour
    /// (`register_link` never downgrades pairing) and it is exactly what makes
    /// the reconnect and hijack cases worth testing.
    pub(crate) fn drop_link(&self) {
        self.net.drop_end(self.end);
        self.sessions
            .disconnect_if_current(&self.binding, "test drop".to_string());
    }

    /// Rebuilds the link after a drop, presenting the same certificate.
    pub(crate) fn relink(&mut self) {
        let pem = self.peer_certificate_pem.clone();
        self.relink_with_certificate(pem);
    }

    /// Rebuilds the link presenting an arbitrary certificate.
    ///
    /// This is the pairing-hijack vector: a second connection claiming the same
    /// device id but carrying a different certificate. `register_link` replaces
    /// the binding and, today, leaves any in-flight `PairingHandler` untouched —
    /// so the attacker inherits the prompt the user is already looking at.
    pub(crate) fn relink_with_certificate(&mut self, certificate_pem: String) {
        self.net.reconnect(self.end);
        self.line.clear();
        let paired = self.is_trusted();
        let session_link = SessionLink::new(
            Arc::new(self.net.end(self.end)),
            certificate_pem,
            self.local_public_der.clone(),
            self.remote_public_der.clone(),
            self.info.clone(),
        );
        self.binding = self
            .sessions
            .register_link(self.peer_id.clone(), session_link, paired)
            .expect("re-register link")
            .binding;
    }

    /// One full reconciler tick at the network's virtual time: pairing expiry
    /// first, then the connection-status rewrite — the same order and the same
    /// functions as `DaemonWorker`'s 1 Hz loop (`daemon/mod.rs`).
    pub(crate) fn tick(&self) -> Vec<String> {
        let expired = self.expire_pairing(self.net.now_secs());
        self.reconcile();
        expired
    }

    /// This peer's own device id — the one the *other* peer addresses.
    pub(crate) fn local_id(&self) -> String {
        self.config.lock().unwrap().local_device_info().id
    }

    /// The device id of the peer on the other end of the wire.
    pub(crate) fn peer_id(&self) -> String {
        self.peer_id.clone()
    }

    /// Whether this peer takes the leading role, by the same rule production
    /// uses. Tests read this rather than assuming, because identity is a real
    /// generated device id and which side leads therefore varies per run.
    pub(crate) fn leads(&self) -> bool {
        crate::device_links::local_goes_first(&self.local_id(), &self.peer_id)
    }

    pub(crate) fn request_pair(&self) {
        self.commands().send_pair_request(&self.peer_id);
    }

    pub(crate) fn accept(&self) {
        self.commands().accept_pair(&self.peer_id, None);
    }

    pub(crate) fn reject(&self) {
        self.commands().reject_pair(&self.peer_id);
    }

    pub(crate) fn unpair(&self) {
        self.commands().unpair(&self.peer_id);
    }

    pub(crate) fn commands(&self) -> PairingContext {
        PairingContext {
            sessions: Arc::clone(&self.sessions),
            devices: Arc::clone(&self.devices),
            config: Arc::clone(&self.config),
            errors: Arc::clone(&self.errors),
        }
    }

    /// Drains everything currently readable and applies it through the
    /// production dispatch. Never blocks and never sleeps: it stops at the
    /// first `Idle`, which is why the harness needs no wall clock.
    ///
    /// Framing is re-implemented rather than reusing `packet_read_loop`,
    /// because that loop sleeps 5ms on an idle read to yield the transport
    /// lock. That sleep belongs to the loop, not here.
    pub(crate) fn pump(&mut self) -> usize {
        use crate::device_links::core::ReadOutcome;
        let mut applied = 0;
        while let Ok(ReadOutcome::Byte(byte)) = self.binding.link.read_byte() {
            self.line.push(byte);
            if byte != b'\n' {
                continue;
            }
            let parsed = NetworkPacket::deserialize(&self.line);
            self.line.clear();
            let Ok(packet) = parsed else { continue };
            apply_lan_packet(
                &self.binding,
                packet,
                &self.devices,
                &self.sessions,
                &self.config,
                &self.events,
            );
            applied += 1;
        }
        applied
    }

    pub(crate) fn pair_state(&self) -> PairState {
        self.sessions
            .with_session(&self.binding, |session| session.pairing.state)
            .expect("session")
    }

    pub(crate) fn verification_key(&self) -> String {
        let timestamp = self
            .sessions
            .with_session(&self.binding, |session| session.pairing.timestamp())
            .expect("session");
        self.binding.link.verification_key(timestamp)
    }

    pub(crate) fn is_trusted(&self) -> bool {
        self.config.lock().unwrap().is_trusted(&self.peer_id)
    }

    /// The certificate actually written to the trust store — the thing that
    /// decides who this peer will accept on every future connection.
    pub(crate) fn trusted_certificate(&self) -> Option<String> {
        self.config
            .lock()
            .unwrap()
            .trusted_certificate_pem(&self.peer_id)
    }

    /// The key the **user is looking at**, as opposed to the one
    /// [`Self::verification_key`] recomputes from the current link. They differ
    /// exactly when a link was replaced mid-negotiation, which is the whole
    /// point of the hijack test.
    pub(crate) fn displayed_verification_key(&self) -> Option<String> {
        self.devices.lock().unwrap()[&self.peer_id]
            .verification_key
            .clone()
    }

    pub(crate) fn status(&self) -> DeviceStatus {
        self.devices.lock().unwrap()[&self.peer_id].status.clone()
    }

    /// Runs the 1 Hz reconciler once. It is a pure function over a connection
    /// snapshot, so the harness calls it wherever a test wants a tick.
    pub(crate) fn reconcile(&self) {
        reconcile_connection_status(&self.devices, &self.sessions.connection_snapshot());
    }

    /// Runs the pairing-expiry half of the reconciler tick at a chosen time,
    /// through the same function the daemon's 1 Hz loop calls.
    pub(crate) fn expire_pairing(&self, now_secs: i64) -> Vec<String> {
        super::pairing_commands::expire_pairings(&self.sessions, &self.devices, now_secs)
    }

    /// One liveness pass, as if `after` had elapsed since now.
    ///
    /// The budget is thirty seconds of silence; stepping the clock is what lets
    /// that be asserted in a microsecond, and the ground rules forbid a test
    /// that sleeps.
    pub(crate) fn liveness(&self, after: std::time::Duration) -> LivenessTick {
        self.sessions.liveness_tick(
            std::time::Instant::now() + after,
            super::HEARTBEAT_IDLE,
            |_| super::HEARTBEAT_DEAD,
        )
    }

    /// How long this peer has been silent, as the socket-replacement guard
    /// measures it.
    pub(crate) fn silence(&self) -> std::time::Duration {
        self.sessions.silence(&self.peer_id).expect("session")
    }

    pub(crate) fn errors(&self) -> Vec<String> {
        self.errors.lock().unwrap().clone()
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.config_dir);
    }
}

#[cfg(test)]
mod tests {
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
                HEARTBEAT_IDLE,
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
        let code = pairing_qr::current_uri().expect("live code");
        let forged = foreign_certificate_pem();
        let (link, transport) = claimed_link(&desktop, &forged);
        let context = context(&desktop);
        let server = std::thread::spawn(move || {
            super::super::handshake::serve_qr_only(
                link,
                transport,
                "test".into(),
                &code,
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
}
