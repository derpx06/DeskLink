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
        eprintln!("[Daemon] Pairing request for {device_id} timed out");
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
        // Unpairing a device that is not connected succeeded: it is told the
        // next time it reaches us (it gets pair:false for its signaling).
        if let Err(error) = if offline { Ok(()) } else { result } {
            report(&self.devices, &self.errors, device_id, error);
        }
    }
}
