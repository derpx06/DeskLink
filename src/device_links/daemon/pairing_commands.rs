//! The pairing commands, off `DaemonWorker`.
//!
//! `DaemonWorker` owns real TCP and UDP listeners and spawns threads in its
//! constructor, so nothing that hangs off it can be exercised without a
//! network. That is why pair, accept, reject and unpair had no test coverage at
//! all. They only ever needed four `Arc`s, so they take four `Arc`s, and the
//! worker becomes one caller among two — the other being the two-peer harness.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::state::{mark_error, push_error, update_pair_state};
use crate::device_links::config::Config;
use crate::device_links::core::{DeviceManager, SessionBinding};
use crate::device_links::device::DeviceView;
use crate::device_links::packet::{NetworkPacket, PACKET_TYPE_PAIR};
use crate::device_links::pairing::{PairState, PairingHandler};

/// Abandons every negotiation whose deadline has passed, **telling the peer**.
///
/// The notice is the point. Before it, `expire_if_due` flipped this side's
/// state and said nothing, so the two ends could only stay in step by ordering
/// their timeouts — the asking side had to outlive the asked side, enforced by
/// a compile-time assert on two different constants. Sending `pair:false` on
/// expiry is what upstream does, and it lets both sides use the same 30s.
///
/// Shared by the daemon's 1 Hz reconciler and the two-peer harness, so the
/// tested path and the shipped path are the same code.
pub(crate) fn expire_pairings(
    sessions: &Arc<DeviceManager>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    now_secs: i64,
) -> Vec<String> {
    let expired = sessions.expire_pairing(now_secs);
    for device_id in &expired {
        debug_log!("[Daemon] Pairing request for {device_id} timed out");
        if let Some(binding) = sessions.current_binding(device_id) {
            let mut packet = NetworkPacket::new(PACKET_TYPE_PAIR);
            packet.set("pair", false);
            // Best effort: if it cannot be delivered the peer is gone, and its
            // own deadline will abandon the negotiation anyway.
            let _ = binding.link.send_packet(&packet);
        }
        update_pair_state(devices, device_id, PairState::NotPaired, None);
    }
    expired
}

/// Whether a failed pairing command is worth putting in front of the user.
///
/// Deliberately **narrower** than
/// [`should_surface_send_error`](crate::device_links::transport::should_surface_send_error).
/// That predicate is tuned for the clipboard poller, which fires once a second
/// and must stay quiet about a link that is merely being replaced — so it also
/// swallows "Device is not connected", which for a button the user has just
/// pressed is the single most useful thing you can tell them. Reusing it here
/// would trade banner spam for a button that appears to do nothing.
///
/// Suppressed instead: only the states that resolve themselves on the next
/// tick, and that the device row already reports through its status.
fn worth_showing_the_user(error: &str) -> bool {
    !error.contains("replaced by a newer session generation")
        && !error.contains("Stale DeskLink session")
        && !error.contains("Session link is closed")
        && !error.contains("Session link has no network stream")
}

/// Records a pairing failure against the device, and in the error banner only
/// if the user can do anything about it.
fn report(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    errors: &Arc<Mutex<Vec<String>>>,
    device_id: &str,
    error: String,
) {
    if !worth_showing_the_user(&error) {
        eprintln!("[Daemon] Pairing command for {device_id} hit a transient state: {error}");
        return;
    }
    mark_error(devices, device_id, error.clone());
    push_error(errors, error);
}

pub(crate) struct PairingContext {
    pub(crate) sessions: Arc<DeviceManager>,
    pub(crate) devices: Arc<Mutex<HashMap<String, DeviceView>>>,
    pub(crate) config: Arc<Mutex<Config>>,
    pub(crate) errors: Arc<Mutex<Vec<String>>>,
}

impl PairingContext {
    pub(crate) fn send_pair_request(&self, device_id: &str) {
        let result = self.transition(device_id, |pairing| pairing.request_packet());
        let Some((binding, snapshot, _, state)) = self.finish(device_id, result) else {
            return;
        };
        if state == PairState::Paired {
            // The peer had already accepted: this request was the acknowledgement.
            if self.persist_trust(device_id, &binding, snapshot, state) {
                update_pair_state(&self.devices, device_id, PairState::Paired, None);
            }
            return;
        }
        let timestamp = self
            .sessions
            .with_session(&binding, |session| session.pairing.timestamp())
            .unwrap_or_default();
        update_pair_state(
            &self.devices,
            device_id,
            state,
            Some(binding.link.verification_key(timestamp)),
        );
    }

    pub(crate) fn accept_pair(&self, device_id: &str, shown_key: Option<&str>) {
        // The acceptance is for the key the user compared. The UI refreshes
        // once a second, so a link replaced in between would otherwise collect
        // an approval granted to a different certificate.
        if let Some(shown_key) = shown_key {
            let current = self
                .sessions
                .current_binding(device_id)
                .and_then(|binding| {
                    self.sessions
                        .with_session(&binding, |session| {
                            binding.link.verification_key(session.pairing.timestamp())
                        })
                        .ok()
                });
            if current.as_deref() != Some(shown_key) {
                report(
                    &self.devices,
                    &self.errors,
                    device_id,
                    "The pairing key changed before it was accepted; compare the new key"
                        .to_string(),
                );
                return;
            }
        }
        let result = self.transition(device_id, |pairing| pairing.accept_packet());
        let Some((binding, snapshot, _, state)) = self.finish(device_id, result) else {
            return;
        };
        // Persisting trust can still fail. Revert rather than leave the
        // session `Paired` against an empty trust store: the dispatcher
        // authorises features from the *session*, so that split admitted a
        // peer this device had not recorded as trusted, and every reconnect
        // then arrived with `initially_paired = false` while the live
        // session insisted otherwise.
        //
        // The peer, which did receive the acceptance, finds out the next
        // time it sends anything — `notify_peer_not_paired` answers it with
        // `pair:false`. That is the same self-healing path a missed unpair
        // uses, which is why no queue is needed here either.
        if self.persist_trust(device_id, &binding, snapshot, state) {
            update_pair_state(&self.devices, device_id, PairState::Paired, None);
        }
    }

    /// Advances the pairing state and builds its packet under the session
    /// lock, then sends it *outside* the lock. The lock is global — every
    /// reader, liveness tick and WebRTC worker takes it — and a TLS write can
    /// block for its full 10 s timeout, which froze all of them. A failed send
    /// reverts the state, unless something else moved it in the meantime.
    #[allow(clippy::type_complexity)]
    fn transition(
        &self,
        device_id: &str,
        build: impl FnOnce(&mut PairingHandler) -> Result<NetworkPacket, String>,
    ) -> Result<(SessionBinding, PairingHandler, NetworkPacket, PairState), String> {
        let binding = self
            .sessions
            .current_binding(device_id)
            .ok_or_else(|| "Device is not connected".to_string())?;
        let (snapshot, packet, state) = self
            .sessions
            .with_session(&binding, |session| {
                let snapshot = session.pairing.clone();
                let packet = build(&mut session.pairing)?;
                Ok::<_, String>((snapshot, packet, session.pairing.state))
            })
            .map_err(|error| error.to_string())??;
        if let Err(error) = binding.link.send_packet(&packet) {
            self.revert(&binding, snapshot, state);
            return Err(error);
        }
        Ok((binding, snapshot, packet, state))
    }

    #[allow(clippy::type_complexity)]
    fn finish(
        &self,
        device_id: &str,
        result: Result<(SessionBinding, PairingHandler, NetworkPacket, PairState), String>,
    ) -> Option<(SessionBinding, PairingHandler, NetworkPacket, PairState)> {
        result
            .map_err(|error| report(&self.devices, &self.errors, device_id, error))
            .ok()
    }

    /// Records trust for a session that reached `Paired`, reverting on failure.
    fn persist_trust(
        &self,
        device_id: &str,
        binding: &SessionBinding,
        snapshot: PairingHandler,
        state: PairState,
    ) -> bool {
        let trusted = self
            .config
            .lock()
            .map_err(|_| "Config lock poisoned".to_string())
            .and_then(|mut config| {
                config.trust_device(&binding.link.info, binding.link.certificate_pem.clone())
            });
        if let Err(error) = trusted {
            self.revert(binding, snapshot, state);
            report(&self.devices, &self.errors, device_id, error);
            return false;
        }
        true
    }

    fn revert(&self, binding: &SessionBinding, snapshot: PairingHandler, expected: PairState) {
        let _ = self.sessions.with_session(binding, |session| {
            if session.pairing.state == expected {
                session.pairing = snapshot;
            }
        });
    }

    /// Rejects a pairing request, and drops any persisted trust with it.
    ///
    /// `remove_trust` used to be `false` here, on the assumption that a device
    /// being *asked* about cannot already be trusted. It can: a peer that still
    /// trusts this side re-sends a pair request after its own state is lost, and
    /// `DaemonCommand::RejectPair` validates nothing before arriving. Rejecting
    /// then cleared the session but left the record on disk, so the device came
    /// back paired on the next reconnect and the user's "no" was silently
    /// undone. Reject and unpair now differ in wording, not in effect.
    pub(crate) fn reject_pair(&self, device_id: &str) {
        self.revoke_pairing(device_id, "Pairing request was rejected", true);
    }

    pub(crate) fn unpair(&self, device_id: &str) {
        self.revoke_pairing(device_id, "Device was unpaired", true);
    }

    fn revoke_pairing(&self, device_id: &str, reason: &str, remove_trust: bool) {
        // Terminals outlive the WebRTC peer, so closing the peer below would
        // leave the phone's attachments and write leases behind.
        crate::device_links::webrtc::terminal_bridge::revoke_device(device_id);
        if remove_trust {
            crate::device_links::webrtc::coordinator::forget_liveness(device_id);
        }
        // Offline: nothing to tell the peer, but the session must not keep
        // `Paired` for it to resume on reconnect.
        let offline = self.sessions.current_binding(device_id).is_none();
        if offline {
            if let Some(peer) = self.sessions.forget_pairing(device_id) {
                peer.close();
            }
        }
        let result = self
            .sessions
            .current_binding(device_id)
            .ok_or_else(|| "Device is not connected".to_string())
            .and_then(|binding| {
                let (packet, peer) = self
                    .sessions
                    .revoke_pairing(&binding, reason.to_string())
                    .map_err(|error| error.to_string())?;

                // The pairing packet remains bootstrap traffic.  The local
                // revocation is already committed even if this final packet
                // cannot be delivered, so a stale peer cannot survive.
                let send_result = binding.link.send_packet(&packet);
                if let Some(peer) = peer {
                    peer.close();
                }
                send_result
            });

        if remove_trust {
            if let Err(error) = self
                .config
                .lock()
                .map_err(|_| "Config lock poisoned".to_string())
                .and_then(|mut config| config.untrust_device(device_id))
            {
                report(&self.devices, &self.errors, device_id, error);
                return;
            }
        }

        update_pair_state(&self.devices, device_id, PairState::NotPaired, None);
        if remove_trust {
            // Every unpair forgets the same things (see `peers`).
            crate::device_links::peers::forget_peer(device_id);
        }
        // Unpairing a device that is not connected succeeded: it is told the
        // next time it reaches us (it gets pair:false for its signaling).
        if let Err(error) = if offline { Ok(()) } else { result } {
            report(&self.devices, &self.errors, device_id, error);
        }
    }
}

/// Phones whose pairing in this run was confirmed by a proof: the QR code's
/// MAC, or the code both screens showed and the person accepted. Only these
/// may inherit what waited for the phone they replace.
fn proven_pairings() -> &'static Mutex<std::collections::HashSet<String>> {
    static PROVEN: std::sync::OnceLock<Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    PROVEN.get_or_init(Default::default)
}

pub(super) fn note_proven_pairing(device_id: &str) {
    if let Ok(mut proven) = proven_pairings().lock() {
        proven.insert(device_id.to_string());
    }
}

pub(crate) fn forget_proven(device_id: &str) {
    if let Ok(mut proven) = proven_pairings().lock() {
        proven.remove(device_id);
    }
}

fn pairing_was_proven(device_id: &str) -> bool {
    proven_pairings()
        .lock()
        .is_ok_and(|proven| proven.contains(device_id))
}

/// What becomes of the files that waited for the replaced phone: they move
/// to the new one only when its pairing was proven in this run (and the
/// person is told how many); else they are dropped, and the person is told.
fn carry_waiting_over(old_id: &str, new_id: &str, name: &str) -> Option<String> {
    let count = super::file_transfer::waiting_files(old_id).len();
    if count == 0 {
        return None;
    }
    if pairing_was_proven(new_id) {
        super::file_transfer::move_waiting(old_id, new_id);
        Some(format!(
            "{count} waiting file{} moved to {name}",
            if count == 1 { "" } else { "s" }
        ))
    } else {
        super::file_transfer::forget_waiting(old_id);
        Some(format!(
            "{count} file{} waiting for the replaced phone {} dropped",
            if count == 1 { "" } else { "s" },
            if count == 1 { "was" } else { "were" }
        ))
    }
}

/// What to do with an older record that a new pairing of the same name and
/// type might replace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Replacement {
    /// Offline, and the same machine by a second signal: replaced.
    Replace,
    /// The older phone is connected right now: it is a different phone.
    KeepOnline,
    /// Offline, but nothing says it is the same machine: two phones of one
    /// model are both "realme 14 5G". Kept; the user can remove it.
    KeepTwin,
}

/// The decision, from facts only. `same_machine`: the old record was last
/// reached at the address the new phone paired from.
pub(super) fn replacement_verdict(old_is_live: bool, same_machine: bool) -> Replacement {
    if old_is_live {
        Replacement::KeepOnline
    } else if same_machine {
        Replacement::Replace
    } else {
        Replacement::KeepTwin
    }
}

/// Whether the old record's last known address (its card's, or the remembered
/// one) is the address the new phone is on.
pub(super) fn same_machine(
    old_card: Option<std::net::IpAddr>,
    old_remembered: Option<std::net::IpAddr>,
    new_card: Option<std::net::IpAddr>,
) -> bool {
    let Some(new_card) = new_card else {
        return false;
    };
    old_card == Some(new_card) || old_remembered == Some(new_card)
}

/// A pairing succeeded for `candidate.new_id`; `candidate.old_id` has the same
/// name and type. Replaced only when its record has no live session and the
/// new phone is at its last known address; the user is told which pairing
/// was replaced. (The old record's identity is not kept: a phone that was
/// replaced by mistake is told `pair:false` when it next sends anything and
/// has to be paired again.)
pub(super) fn consider_replacement(
    sessions: &Arc<DeviceManager>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    config: &Arc<Mutex<crate::device_links::config::Config>>,
    events: &Arc<Mutex<Vec<super::DaemonEvent>>>,
    errors: &Arc<Mutex<Vec<String>>>,
    candidate: crate::device_links::config::ReplacementCandidate,
) {
    let address = |id: &str| {
        devices.lock().ok().and_then(|devices| {
            devices
                .get(id)
                .and_then(|device| device.address.parse().ok())
        })
    };
    let remembered = super::discovery::persisted_ip(&candidate.old_id);
    let verdict = replacement_verdict(
        sessions.current_binding(&candidate.old_id).is_some(),
        same_machine(
            address(&candidate.old_id),
            remembered,
            address(&candidate.new_id),
        ),
    );
    if verdict != Replacement::Replace {
        debug_log!(
            "[Pairing] kept the older pairing of {} next to the new one: {verdict:?}",
            candidate.name
        );
        return;
    }
    let removed = config
        .lock()
        .map_err(|_| "Config lock poisoned".to_string())
        .and_then(|mut config| config.untrust_device(&candidate.old_id));
    if let Err(error) = removed {
        super::state::push_error(errors, error);
        return;
    }
    // Files that waited for the old phone: carried over only on a proven
    // pairing, and the person is told either way.
    if let Some(told) = carry_waiting_over(&candidate.old_id, &candidate.new_id, &candidate.name) {
        super::state::push_error(errors, told);
    }
    forget_replaced(sessions, devices, &candidate.old_id);
    super::state::push_event(
        events,
        super::DaemonEvent::PairingReplaced {
            name: candidate.name,
        },
    );
}

/// Everything the daemon holds for a pairing that a newer one replaced (or
/// that was removed): its session and peer, its card, the addresses it was
/// dialled at and what was remembered of its snapshots.
pub(super) fn forget_replaced(
    sessions: &Arc<DeviceManager>,
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
) {
    if let Some(binding) = sessions.current_binding(device_id) {
        if let Ok((packet, peer)) =
            sessions.revoke_pairing(&binding, "Replaced by a newer pairing".to_string())
        {
            // Told, not just cut off: this `pair:false` was discarded.
            let _ = binding.link.send_packet(&packet);
            if let Some(peer) = peer {
                peer.close();
            }
        }
        binding.link.close();
    } else if let Some(peer) = sessions.forget_pairing(device_id) {
        peer.close();
    }
    if let Ok(mut devices) = devices.lock() {
        devices.remove(device_id);
    }
    crate::device_links::peers::forget_peer(device_id);
}

#[cfg(test)]
mod carry_over_tests {
    use super::*;

    fn waiting(old: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("desklink-carry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("f.txt");
        std::fs::write(&file, "x").unwrap();
        super::super::file_transfer::wait_for_phone_for_tests(old, "Old", file.clone());
        file
    }

    /// Files that waited for a replaced phone follow only a proven pairing.
    #[test]
    fn waiting_files_follow_a_proven_replacement_and_are_dropped_otherwise() {
        let file = waiting("carry-old-1");
        let told = carry_waiting_over("carry-old-1", "carry-new-unproven", "Pixel").unwrap();
        assert!(told.contains("dropped"), "{told}");
        assert!(super::super::file_transfer::waiting_files("carry-old-1").is_empty());
        assert!(super::super::file_transfer::waiting_files("carry-new-unproven").is_empty());

        let file2 = waiting("carry-old-2");
        note_proven_pairing("carry-new-proven");
        let told = carry_waiting_over("carry-old-2", "carry-new-proven", "Pixel").unwrap();
        assert_eq!(told, "1 waiting file moved to Pixel");
        assert_eq!(
            super::super::file_transfer::waiting_files("carry-new-proven"),
            vec![file2]
        );
        super::super::file_transfer::forget_waiting("carry-new-proven");
        assert!(
            carry_waiting_over("carry-old-3", "x", "y").is_none(),
            "nothing waiting: nothing said"
        );
        let _ = file;
    }
}

#[cfg(test)]
mod replaced_tests {
    use super::*;
    use crate::device_links::core::SessionLink;

    fn info(id: &str, name: &str) -> crate::device_links::device_info::DeviceInfo {
        crate::device_links::device_info::DeviceInfo {
            id: id.into(),
            name: name.into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        }
    }

    struct Replacing {
        sessions: Arc<DeviceManager>,
        devices: Arc<Mutex<HashMap<String, DeviceView>>>,
        config: Arc<Mutex<crate::device_links::config::Config>>,
        events: Arc<Mutex<Vec<super::super::DaemonEvent>>>,
        errors: Arc<Mutex<Vec<String>>>,
        dir: std::path::PathBuf,
    }

    impl Drop for Replacing {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    /// Two paired records of one name: `old` at `old_address`, `new` just
    /// paired at `new_address`.
    fn replacing(old_address: &str, new_address: &str) -> Replacing {
        let dir = std::env::temp_dir().join(format!("desklink-c-{}", uuid::Uuid::new_v4()));
        let mut config = crate::device_links::config::Config::load_from_dir(dir.clone()).unwrap();
        config
            .trust_device(&info("old-phone", "realme 14 5G"), "cert-old".into())
            .unwrap();
        config
            .trust_device(&info("new-phone", "realme 14 5G"), "cert-new".into())
            .unwrap();
        let devices: Arc<Mutex<HashMap<String, DeviceView>>> = Arc::default();
        for (id, address) in [("old-phone", old_address), ("new-phone", new_address)] {
            devices.lock().unwrap().insert(
                id.into(),
                DeviceView::from_info(&info(id, "realme 14 5G"), address.into(), true),
            );
        }
        Replacing {
            sessions: Arc::new(DeviceManager::new()),
            devices,
            config: Arc::new(Mutex::new(config)),
            events: Arc::default(),
            errors: Arc::default(),
            dir,
        }
    }

    fn candidate() -> crate::device_links::config::ReplacementCandidate {
        crate::device_links::config::ReplacementCandidate {
            old_id: "old-phone".into(),
            new_id: "new-phone".into(),
            name: "realme 14 5G".into(),
        }
    }

    fn run(setup: &Replacing) {
        consider_replacement(
            &setup.sessions,
            &setup.devices,
            &setup.config,
            &setup.events,
            &setup.errors,
            candidate(),
        );
    }

    /// Review finding C: an offline twin at the same address is the same
    /// phone reinstalled: replaced, and the user is told.
    #[test]
    fn an_offline_record_at_the_same_address_is_replaced_and_announced() {
        let setup = replacing("10.5.0.7", "10.5.0.7");
        run(&setup);
        let config = setup.config.lock().unwrap();
        assert!(!config.is_trusted("old-phone") && config.is_trusted("new-phone"));
        assert!(setup.devices.lock().unwrap().get("old-phone").is_none());
        assert!(matches!(
            setup.events.lock().unwrap().as_slice(),
            [super::super::DaemonEvent::PairingReplaced { name }] if name == "realme 14 5G"
        ));
    }

    /// Review finding C: two phones of one model must not delete each other.
    #[test]
    fn an_offline_twin_elsewhere_is_kept() {
        let setup = replacing("10.5.0.7", "10.5.0.99");
        run(&setup);
        let config = setup.config.lock().unwrap();
        assert!(config.is_trusted("old-phone") && config.is_trusted("new-phone"));
        assert!(setup.events.lock().unwrap().is_empty(), "nothing replaced");
    }

    /// Review finding C: a record with a live session is never replaced, even
    /// at the same address.
    #[test]
    fn a_connected_record_is_never_replaced() {
        let setup = replacing("10.5.0.7", "10.5.0.7");
        setup
            .sessions
            .register_link(
                "old-phone".into(),
                crate::device_links::core::SessionLink::test_placeholder("old-phone"),
                true,
            )
            .unwrap();
        run(&setup);
        assert!(setup.config.lock().unwrap().is_trusted("old-phone"));
        assert!(setup.events.lock().unwrap().is_empty());
    }

    #[test]
    fn the_replacement_verdict_needs_offline_and_the_same_machine() {
        assert_eq!(replacement_verdict(true, true), Replacement::KeepOnline);
        assert_eq!(replacement_verdict(false, false), Replacement::KeepTwin);
        assert_eq!(replacement_verdict(false, true), Replacement::Replace);
        let ip = |text: &str| text.parse().ok();
        assert!(same_machine(ip("10.0.0.1"), None, ip("10.0.0.1")));
        assert!(same_machine(None, ip("10.0.0.1"), ip("10.0.0.1")));
        assert!(!same_machine(
            ip("10.0.0.2"),
            ip("10.0.0.3"),
            ip("10.0.0.1")
        ));
        assert!(!same_machine(None, None, None));
        assert!(!same_machine(ip("10.0.0.1"), None, None));
    }

    /// A phone that paired again under a new id left a ghost: its session,
    /// card, addresses and remembered revs all go, so nothing redials it.
    #[test]
    fn a_replaced_pairing_leaves_no_session_card_address_or_revs_behind() {
        let sessions = Arc::new(DeviceManager::new());
        let ghost = "ghost-phone";
        let binding = sessions
            .register_link(
                ghost.to_string(),
                SessionLink::test_placeholder(ghost),
                true,
            )
            .unwrap()
            .binding;
        let info = crate::device_links::device_info::DeviceInfo {
            id: ghost.into(),
            name: "Pixel 9".into(),
            device_type: "phone".into(),
            protocol_version: 9,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
            app_version: None,
        };
        let devices: Arc<Mutex<HashMap<String, DeviceView>>> = Arc::default();
        devices.lock().unwrap().insert(
            ghost.into(),
            DeviceView::from_info(&info, "10.0.0.9".into(), true),
        );
        let ip: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        super::super::discovery::note_address(ghost, ip);
        let mut rev = crate::device_links::packet::NetworkPacket::new("desklink.settings");
        rev.set("rev", "r1");
        crate::device_links::sync::note_received(ghost, Some("p"), "contacts", &rev);
        assert!(!binding.link.is_closed());

        forget_replaced(&sessions, &devices, ghost);

        assert!(binding.link.is_closed(), "its link is closed");
        assert!(
            devices.lock().unwrap().get(ghost).is_none(),
            "its card is gone"
        );
        assert!(
            super::super::discovery::addresses()
                .lock()
                .unwrap()
                .due(ghost, std::time::Instant::now())
                .is_empty(),
            "nothing dials it"
        );
        assert!(crate::device_links::sync::theirs(ghost).is_none());
        let held = crate::device_links::sync::have_packet(ghost, Some("p"));
        assert!(crate::device_links::sync::parse_have(&held)
            .unwrap()
            .have
            .is_empty());
    }
}
