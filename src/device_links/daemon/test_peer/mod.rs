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
    peer_pair_with(net, left_label, right_label, &[])
}

/// [`peer_pair`] where each peer's identity advertises `capabilities` (a
/// phone that takes WebRTC signaling, say): what a link's identity says, as
/// production reads it.
pub(crate) fn peer_pair_with(
    net: &Arc<TestNetwork>,
    left_label: &str,
    right_label: &str,
    capabilities: &[&str],
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
        capabilities,
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
        capabilities,
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
        capabilities: &[&str],
    ) -> Self {
        let link = net.end(end);
        let sessions = Arc::new(DeviceManager::new());
        let devices = Arc::new(Mutex::new(HashMap::new()));

        let info = DeviceInfo {
            id: peer_id.to_string(),
            name: peer_id.to_string(),
            device_type: "phone".to_string(),
            protocol_version: PROTOCOL_VERSION,
            incoming_capabilities: capabilities.iter().map(|c| c.to_string()).collect(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
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
            events: Arc::clone(&self.events),
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
            |_| super::HEARTBEAT_IDLE,
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
mod tests;
