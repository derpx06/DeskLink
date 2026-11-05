//! Everything this desk keeps *per phone*, forgotten in one place.
//!
//! A phone is unpaired from the phone's side, from this desk, replaced by a
//! reinstall, or comes back with a new identity: each of those used to clean
//! some of the stores and not others (a remote unpair skipped the addresses
//! and the revs, and contacts came back empty). They all call
//! [`forget_peer`] now, and a test keeps the list of stores complete.

/// Forgets every in-memory record this desk holds for `device_id` (what it
/// proved, what it was sent, what was remembered of it). The pairing record
/// itself, and its files, are the trust store's own business
/// (`Config::untrust_device`).
pub(crate) fn forget_peer(device_id: &str) {
    crate::device_links::webrtc::transfer_board::finish_device(device_id, "The phone was unpaired");
    forget_identity_state(device_id);
    forget_live_and_connection_state(device_id);
}

/// What must not survive a new identity behind the same id (a changed
/// certificate): consents, nonces, back-off and records held in memory or in
/// the transfer store. Plain memory and files only: it never waits on another
/// thread, so it may run under the configuration lock.
pub(crate) fn forget_identity_state(device_id: &str) {
    use crate::device_links::{daemon, glide, mpris, webrtc};
    webrtc::transfer_manager::forget_device(device_id);
    webrtc::file_lane::forget_peer(device_id);
    crate::device_links::folders::forget_device(device_id);
    crate::device_links::unlock::forget(device_id);
    glide::forget_phone(device_id);
    crate::device_links::walk_away::forget(device_id);
    crate::device_links::dnd::forget(device_id);
    mpris::forget_art_sent(device_id);
    daemon::clipboard::forget_sent_to(device_id);
}

/// The rest of [`forget_peer`]: ends live shares (which waits on the main
/// loop: never under the configuration lock), and drops what was learned of
/// the connection.
fn forget_live_and_connection_state(device_id: &str) {
    use crate::device_links::{daemon, features, session_lock, sync, webrtc};
    webrtc::terminal_bridge::revoke_device(device_id);
    crate::device_links::calls::hold_media(device_id, false);
    webrtc::coordinator::forget_liveness(device_id);
    daemon::discovery::forget_addresses(device_id);
    daemon::file_transfer::forget_waiting(device_id);
    daemon::pairing_commands::forget_proven(device_id);
    sync::forget_device(device_id);
    session_lock::forget_unlock(device_id);
    crate::device_links::gallery::forget(device_id);
    features::forget_peer_profile(device_id);
    webrtc::audio_session::end_all(device_id);
    webrtc::capture::end(device_id);
    webrtc::remote_session::forget(device_id);
    webrtc::screen::clear_stream(device_id);
    webrtc::screen::forget_parked(device_id);
    crate::device_links::contacts::forget(device_id);
    crate::device_links::notes::forget_peer(device_id);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `static` map or set keyed by a device id in the sources is either
    /// cleared by [`forget_peer`] (listed in `FORGOTTEN`) or deliberately not
    /// (`KEPT`, with the reason). A new store fails this test until it is
    /// listed, which is how a forgotten one is noticed.
    #[test]
    fn every_per_device_store_is_forgotten_or_kept_on_purpose() {
        const FORGOTTEN: &[&str] = &[
            "device_links/sync.rs:STATE",
            "device_links/mpris.rs:ART_SENT",
            "device_links/features/registry.rs:PROFILES",
            "device_links/webrtc/remote_session.rs:SESSIONS",
            "device_links/webrtc/screen.rs:STREAMS",
            "device_links/webrtc/coordinator.rs:USED",
            "device_links/webrtc/coordinator.rs:DOZING",
            "device_links/daemon/pairing_commands.rs:PROVEN",
            "device_links/webrtc/coordinator.rs:HEARD",
            "device_links/webrtc/screen.rs:PARKED",
            "device_links/webrtc/coordinator.rs:PROBE_NOW",
            "device_links/webrtc/coordinator.rs:STREAKS",
            "device_links/glide.rs:ALLOWED",
            "device_links/daemon/discovery.rs:KNOWN",
            "device_links/daemon/discovery.rs:PERSISTED",
            "device_links/daemon/clipboard.rs:LAST_SENT_BY_DEVICE",
            "device_links/webrtc/audio_session.rs:SESSIONS",
            "device_links/webrtc/capture.rs:SESSIONS",
            "device_links/webrtc/terminal_bridge.rs:BRIDGES",
            "device_links/calls.rs:HOLD",
            "device_links/contacts.rs:STORE",
            "device_links/daemon/discovery.rs:BOOK",
            "device_links/dnd.rs:ENGINE",
            "device_links/folders.rs:REFUSED",
            "device_links/folders.rs:WANTED",
            "device_links/gallery.rs:LEDGER",
            "device_links/session_lock.rs:LOCK",
            "device_links/walk_away.rs:WATCH",
            "device_links/webrtc/file_lane.rs:GRANTS",
            "device_links/webrtc/file_lane.rs:PENALTIES",
            "device_links/webrtc/transfer_board.rs:BOARD",
        ];
        const KEPT: &[&str] = &[
            // Windows and widgets (they close with their device's page).
            "remote_control.rs:OPEN_WINDOWS",
            "shell_dbus.rs:LIVE_SINCE",
            "window/transfers.rs:SHOWN",
            "window/transfers.rs:FAILED",
            "window/transfers.rs:WAITING",
            // Owned by the live share/negotiation, ended by the peer's reset.
            "device_links/webrtc/coordinator.rs:RETIRING",
            "device_links/webrtc/coordinator.rs:SERVICES",
            "device_links/webrtc/coordinator.rs:STARTING",
            // A share waiting for the portal's answer; its guard ends the entry when the worker does.
            "device_links/webrtc/coordinator.rs:WAITING",
            "device_links/webrtc/coordinator.rs:LAST",
            // Throttles that expire by themselves.
            "device_links/daemon/discovery.rs:LAST",
            // When each unpaired card was last heard from: bounded (cleared at 1024), and only
            // decides which card makes room.
            "device_links/daemon/state.rs:UNTRUSTED_SEEN",
            // History keeps names of phones that are gone, on purpose.
            "device_links/connection_log.rs:NAMES",
            "device_links/connection_log.rs:READY",
            // The trust store's own mirrors.
            "device_links/config.rs:MIRROR",
            "device_links/config.rs:TOKENS",
            // A snapshot being assembled; times out.
            "device_links/contacts.rs:PENDING",
            // A marker that a deleted book must not come back from a late
            // reply; cleared by the phone's next snapshot.
            "device_links/contacts.rs:FORGOTTEN",
            // Rebuilt from the transfer board's rows at every sample, which
            // `finish_device` has emptied.
            "bucket_progress.rs:TRACKER",
            // The daemon's own device table, not a record of a phone.
            "device_links/desk_features.rs:DAEMON",
            // Keyed by D-Bus sender/serial and by app, not by phone.
            "device_links/desk_notifications.rs:STATE",
            // Keyed by media player.
            "device_links/mpris.rs:SHOWN",
            // Terminal sessions outlive their phone while DeskLink runs, by
            // design.
            "device_links/pty/registry.rs:REGISTRY",
            // Registered receivers leave with their transfer manager; the
            // open-lane counts are balanced by `lane_closed`.
            "device_links/webrtc/file_lane.rs:REGISTRY",
            // Keyed by verb, and the page's own browsing state (the thread's
            // UI).
            "send_to.rs:LIMITER",
            "window/bucket.rs:BROWSE",
        ];
        /// A map or set keyed by a device id, or by a tuple starting with one.
        fn keyed(text: &str) -> bool {
            ["HashMap", "HashSet", "BTreeMap", "BTreeSet"]
                .iter()
                .any(|kind| {
                    text.contains(&format!("{kind}<String"))
                        || text.contains(&format!("{kind}<(String"))
                })
        }
        /// The source without its `#[cfg(test)]` items: a test module (it
        /// ends at the first `}` in column 0, as rustfmt writes it) or a
        /// test-only static. Code after a test module is still scanned.
        fn production(text: &str) -> String {
            let mut kept = String::new();
            let mut lines = text.lines();
            while let Some(line) = lines.next() {
                if line.trim() == "#[cfg(test)]" {
                    let Some(item) = lines.next() else { break };
                    if item.starts_with("mod ") && !item.trim_end().ends_with(';') {
                        for inner in lines.by_ref() {
                            if inner == "}" {
                                break;
                            }
                        }
                    }
                    continue;
                }
                kept.push_str(line);
                kept.push('\n');
            }
            kept
        }
        /// Whether `struct <name>` in `text` holds a device-keyed map or set.
        fn struct_holds_keyed(text: &str, name: &str) -> bool {
            let Some(start) = text
                .find(&format!("struct {name} "))
                .or_else(|| text.find(&format!("struct {name}\n")))
            else {
                return false;
            };
            let body = text[start..].split("\n}").next().unwrap_or_default();
            keyed(body)
        }
        fn scan(dir: &std::path::Path, root: &std::path::Path, found: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    scan(&path, root, found);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    let text = production(&std::fs::read_to_string(&path).unwrap());
                    let mut lines = text.lines().peekable();
                    while let Some(line) = lines.next() {
                        let trimmed = line.trim_start();
                        let Some(rest) = trimmed
                            .strip_prefix("static ")
                            .or_else(|| trimmed.strip_prefix("pub static "))
                        else {
                            continue;
                        };
                        // The type may continue on the next line.
                        let mut declaration = rest.to_string();
                        if !declaration.contains('<') {
                            continue;
                        }
                        if !declaration.contains('=') {
                            if let Some(next) = lines.peek() {
                                declaration.push_str(next);
                            }
                        }
                        // `Mutex<Penalties>`: a struct of this file that holds the map.
                        let wrapped = declaration.split(['<', '>', ' ', ',', '=']).any(|word| {
                            word.starts_with(|c: char| c.is_ascii_uppercase())
                                && !word.chars().all(|c| c.is_ascii_uppercase() || c == '_')
                                && struct_holds_keyed(&text, word)
                        });
                        if keyed(&declaration) || wrapped {
                            let name = rest.split(':').next().unwrap_or_default().trim();
                            let relative = path.strip_prefix(root).unwrap();
                            found.push(format!("{}:{name}", relative.display()));
                        }
                    }
                }
            }
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        scan(&root, &root, &mut found);
        found.sort();
        let mut known: Vec<&str> = FORGOTTEN.iter().chain(KEPT).copied().collect();
        known.sort_unstable();
        let unlisted: Vec<_> = found
            .iter()
            .filter(|name| !known.contains(&name.as_str()))
            .collect();
        assert!(
            unlisted.is_empty(),
            "per-device stores not listed in forget_peer's test: {unlisted:?}"
        );
        // And the lists name only stores that exist.
        let stale: Vec<_> = known
            .iter()
            .filter(|name| !found.iter().any(|f| f == *name))
            .collect();
        assert!(stale.is_empty(), "listed but not found: {stale:?}");
    }

    /// What was remembered of a phone is gone after `forget_peer`, whatever
    /// way it was unpaired.
    #[test]
    fn forgetting_a_peer_clears_what_it_proved_and_what_was_remembered() {
        let id = format!("phone-forget-{}", uuid::Uuid::new_v4().simple());
        // A rev, a proved password, a doze report and a glide report.
        let mut snapshot = crate::device_links::packet::NetworkPacket::new("desklink.contacts");
        snapshot.set("rev", "R1");
        crate::device_links::sync::note_received(&id, Some("inst"), "contacts", &snapshot);
        let mut beat = crate::device_links::packet::NetworkPacket::new("desklink.heartbeat");
        beat.set("dozing", true);
        crate::device_links::webrtc::coordinator::note_heartbeat(&id, &beat);
        let mut allowed =
            crate::device_links::packet::NetworkPacket::new("desklink.shareinputdevices");
        allowed.set("allowed", true);
        crate::device_links::glide::on_phone_packet(&id, &allowed);
        assert!(crate::device_links::webrtc::coordinator::is_dozing(&id));
        assert_eq!(crate::device_links::glide::phone_allows(&id), Some(true));
        forget_peer(&id);
        let holds = crate::device_links::sync::have_packet(&id, Some("inst"))
            .body
            .get("have")
            .and_then(|have| have.as_object())
            .is_some_and(|have| !have.is_empty());
        assert!(!holds);
        assert!(!crate::device_links::webrtc::coordinator::is_dozing(&id));
        assert_eq!(crate::device_links::glide::phone_allows(&id), None);
    }
}
