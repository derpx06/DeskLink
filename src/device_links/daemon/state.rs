use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::handle::DaemonEvent;
use crate::device_links::connection_log;
use crate::device_links::core::DeviceConnection;
use crate::device_links::device::{
    BatteryStatus, DeviceNotification, DeviceStatus, DeviceView, MediaStatus, SystemVolumeStatus,
    VolumeSink,
};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::NetworkPacket;
use crate::device_links::pairing::PairState;
use serde_json::Value;

/// Unpaired devices shown at once; more than any real LAN needs.
const MAX_UNTRUSTED_VIEWS: usize = 32;

pub(crate) fn upsert_device(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    info: &DeviceInfo,
    address: String,
    paired: bool,
) {
    if let Ok(mut devices) = devices.lock() {
        // Discovery beacons are unauthenticated: without a bound, spoofed ids
        // grew this map (and the UI list cloned from it each second) forever.
        // Known and trusted devices are always kept.
        if !paired
            && !devices.contains_key(&info.id)
            && devices.values().filter(|device| !device.trusted).count() >= MAX_UNTRUSTED_VIEWS
        {
            return;
        }
        devices
            .entry(info.id.clone())
            .and_modify(|device| {
                device.name = info.name.clone();
                device.device_type = info.device_type.clone();
                device.address = address.clone();
                device.protocol_version = info.protocol_version;
                device.incoming_capabilities = info.incoming_capabilities.clone();
                device.outgoing_capabilities = info.outgoing_capabilities.clone();
                // Trust is a stored fact, so it is safe to refresh here.
                // Reachability is not: a UDP broadcast only proves the peer is
                // powered on. Setting `Paired` here made a device with a dead
                // TCP link keep reporting "Connected" indefinitely, because
                // every discovery beacon overwrote the real disconnect.
                //
                // Assigned, not raised. `if paired { … = true }` could only ever
                // add trust, so a view that a pairing command had just set to
                // `false` was silently re-trusted by the next discovery beacon —
                // and nothing else would lower it. `reconcile_connection_status`
                // overrides this from the live session within a second anyway;
                // `trusted` is not written here: a beacon is not an authority on
                // pairing, and `reconcile_connection_status` projects it from
                // the session (or the trust store) every second.
            })
            .or_insert_with(|| DeviceView::from_info(info, address, paired));
    }
}

/// Lists every paired device from the trust store, so a phone that is away is
/// shown (as offline) from startup, and one that reconnects only through the
/// relay — which never passes discovery or the LAN handshake — still has a row
/// for its battery, notifications, history and session controls.
pub(super) fn seed_trusted_devices(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    trusted: impl Iterator<Item = (String, String, String, i64)>,
) {
    for (id, name, device_type, protocol_version) in trusted {
        let info = DeviceInfo {
            id,
            name,
            device_type,
            protocol_version,
            incoming_capabilities: Vec::new(),
            outgoing_capabilities: Vec::new(),
            instance_id: None,
        };
        upsert_device(devices, &info, String::new(), true);
    }
}

/// Rewrites every device's reachability from the authoritative session state.
///
/// Pairing-negotiation states are left alone: they describe a user-facing
/// dialog, not a transport, and reconciling them would cancel a pair prompt the
/// user is still looking at.
pub(crate) fn reconcile_connection_status(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    connections: &[DeviceConnection],
) {
    let Ok(mut devices) = devices.lock() else {
        return;
    };
    let by_id: HashMap<&str, &DeviceConnection> = connections
        .iter()
        .map(|connection| (connection.device_id.as_str(), connection))
        .collect();

    for (device_id, device) in devices.iter_mut() {
        let connection = by_id.get(device_id.as_str());
        let reachable = connection.is_some_and(|connection| connection.is_reachable());
        let ready = connection
            .is_some_and(|connection| connection.feature_transport_ready && connection.paired);
        connection_log::remember_name(device_id, &device.name);
        // The log is written where the transition happens; this only
        // catches one those paths missed (and repeats nothing).
        connection_log::note_ready(device_id, ready);
        // Phone-owned caches (battery, media, notifications) go stale on
        // the edge: kept, shown as "last seen", acted on only once ready.
        if device.feature_transport_ready && !ready {
            device.last_seen = Some(connection_log::now());
            // A call is never kept stale: it cannot be answered without the
            // link, and would ring here forever. Paused players stay paused
            // until the phone's next snapshot (sent on feature-ready).
            device.call_status = None;
            device.keyboard_focused = None;
            device.keyboard_allowed = None;
        } else if ready {
            device.last_seen = None;
        }
        device.feature_transport_ready = ready;

        let awaiting_pair_decision = matches!(
            device.status,
            DeviceStatus::PairRequested | DeviceStatus::PairRequestedByPeer
        );
        // A pending pair prompt is a user-facing dialog, not a transport, so it
        // is not overwritten while the device is still reachable. It is cleared
        // once the device goes away: pairing cannot complete without a link, and
        // this reconciler is the only thing that notices a peer that vanished
        // without a clean disconnect — leaving the prompt would strand the row
        // on "Pair request" forever.
        if awaiting_pair_decision && reachable {
            continue;
        }

        // `connection.paired` is the session's pairing state — the same value
        // the dispatcher authorises features from, and the only authority on
        // this device's pairing while a session exists. `device.trusted` is a
        // *cached copy* that discovery, the handshake and every pairing command
        // write independently, so reading it here re-asserted whatever they had
        // got wrong, once a second, forever, instead of correcting it.
        //
        // With no session there is nothing live to consult and the stored flag,
        // which came from the config trust store, is the only authority there
        // is. Assigning it back makes `trusted` a projection rather than a
        // fifth place pairing is remembered — and, unlike `upsert_device`, this
        // can lower it as well as raise it.
        let paired = connection.map_or(device.trusted, |connection| connection.paired);
        device.trusted = paired;

        device.status = match (reachable, paired) {
            (true, true) => DeviceStatus::Paired,
            (true, false) => DeviceStatus::Connected,
            (false, true) => DeviceStatus::Unreachable,
            (false, false) => DeviceStatus::Discovered,
        };
        if !reachable {
            device.verification_key = None;
        }
    }
}

pub(super) fn update_pair_state(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    state: PairState,
    verification_key: Option<String>,
) {
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.apply_pair_state(state);
            device.verification_key = verification_key;
        }
    }
}

pub(super) fn mark_status(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    status: DeviceStatus,
) {
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.status = status;
        }
    }
}

pub(crate) fn mark_error(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    error: String,
) {
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.last_error = Some(error);
        }
    }
}

pub(crate) fn clear_error(devices: &Arc<Mutex<HashMap<String, DeviceView>>>, device_id: &str) {
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.last_error = None;
        }
    }
}

/// Newest errors the UI has not drained yet.
///
/// The UI only ever displays the most recent entry, and producers can be very
/// fast — remote-control pointer motion emits one failure per motion event — so
/// this is a bounded ring rather than an unbounded log.
const MAX_PENDING_ERRORS: usize = 32;

pub(super) fn push_error(errors: &Arc<Mutex<Vec<String>>>, error: String) {
    if let Ok(mut errors) = errors.lock() {
        // Repeating the same message adds nothing and hides what came before.
        if errors.last().is_some_and(|last| last == &error) {
            return;
        }
        errors.push(error);
        if errors.len() > MAX_PENDING_ERRORS {
            let excess = errors.len() - MAX_PENDING_ERRORS;
            errors.drain(..excess);
        }
    }
}

/// Feature events the UI has not drained yet.
///
/// Every event becomes a desktop notification with a unique id — so they stack
/// rather than replace — plus a four-second toast. An unbounded queue meant a
/// peer sending a hundred pings in a second produced a hundred stacked
/// notifications and several minutes of toasts with no way to dismiss them.
const MAX_PENDING_EVENTS: usize = 16;

pub(crate) fn push_event(events: &Arc<Mutex<Vec<DaemonEvent>>>, event: DaemonEvent) {
    if let Ok(mut events) = events.lock() {
        events.push(event);
        if events.len() > MAX_PENDING_EVENTS {
            let excess = events.len() - MAX_PENDING_EVENTS;
            events.drain(..excess);
        }
    }
    wake_ui();
}

/// What the window registers to be refreshed at once (see [`wake_ui`]).
static UI_WAKE: std::sync::OnceLock<fn()> = std::sync::OnceLock::new();

/// Called once by the window: `refresh` runs on the main loop when an
/// event arrives.
pub(crate) fn set_ui_wake(refresh: fn()) {
    let _ = UI_WAKE.set(refresh);
}

/// Has the window handle an event now instead of at its next 1 s tick, so a
/// ringing call (or a ping) shows within a frame. Coalesced: one refresh per
/// burst.
pub(crate) fn wake_ui() {
    static PENDING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let Some(refresh) = UI_WAKE.get().copied() else {
        return;
    };
    if PENDING.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return;
    }
    gtk::glib::MainContext::default().invoke(move || {
        PENDING.store(false, std::sync::atomic::Ordering::Release);
        refresh();
    });
}

pub(super) fn update_media_from_packet(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) {
    if let Ok(mut devices) = devices.lock() {
        let Some(device) = devices.get_mut(device_id) else {
            return;
        };

        // A `playerList` is the phone's whole set of players: what was
        // cached for a player that is no longer in it is dropped, so nothing
        // from a player that is gone lingers. The current player's state is
        // kept while it is still listed (a list alone says nothing about it).
        let listed = string_vec(packet.body.get("playerList"));
        let cached = device.media_status.clone().filter(|media| {
            listed
                .as_ref()
                .is_none_or(|players| players.contains(&media.player))
        });
        let mut media = cached.unwrap_or_else(|| MediaStatus {
            player: String::new(),
            player_list: Vec::new(),
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            is_playing: false,
            volume: None,
            position: None,
            length: None,
            can_seek: false,
            album_art_url: None,
        });

        if let Some(players) = string_vec(packet.body.get("playerList")) {
            // No players on the phone: nothing to show.
            if players.is_empty() {
                device.media_status = None;
                return;
            }
            media.player_list = players;
            if media.player.is_empty() {
                media.player = media.player_list.first().cloned().unwrap_or_default();
            }
        }
        if let Some(player) = packet.get_str("player") {
            media.player = sanitize_text(player, 120);
        }
        if let Some(title) = packet.get_str("title") {
            media.title = sanitize_text(title, 240);
        }
        if let Some(artist) = packet.get_str("artist") {
            media.artist = sanitize_text(artist, 240);
        }
        if let Some(album) = packet.get_str("album") {
            media.album = sanitize_text(album, 240);
        }
        if let Some(is_playing) = packet
            .get_bool("isPlaying")
            .or_else(|| packet.get_bool("playing"))
        {
            media.is_playing = is_playing;
        }
        if let Some(volume) = packet.get_i64("volume") {
            media.volume = Some(volume.clamp(0, 150));
        }
        if let Some(position) = packet.get_i64("pos").or_else(|| packet.get_i64("position")) {
            media.position = Some(position.max(0));
        }
        if let Some(length) = packet.get_i64("length") {
            media.length = Some(length.max(0));
        }
        if let Some(can_seek) = packet.get_bool("canSeek") {
            media.can_seek = can_seek;
        }
        if let Some(album_art_url) = packet.get_str("albumArtUrl") {
            media.album_art_url = Some(sanitize_text(album_art_url, 1024));
        }

        device.media_status = Some(media);
    }
}

pub(super) fn update_battery_status(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) {
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            let charge = packet.get_i64("currentCharge").unwrap_or(-1).clamp(-1, 100);
            // `-1`: the phone has no battery to report (or turned it off).
            if charge < 0 {
                device.battery_status = None;
                return;
            }
            device.battery_status = Some(BatteryStatus {
                current_charge: charge,
                is_charging: packet.get_bool("isCharging").unwrap_or(false),
                battery_quantity: packet.get_i64("batteryQuantity"),
                threshold_event: packet.get_i64("thresholdEvent"),
                charge_time_remaining: packet.get_i64("chargeTimeRemaining").filter(|ms| *ms >= 0),
            });
        }
    }
}

/// `desklink.telephony {calls, muted, silenced}`: the phone's calls now.
/// Returns whether it is in a call.
/// `desklink.mousepad.keyboardstate {state}`: a text field has focus on
/// the phone, or not.
pub(super) fn update_keyboard_state(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    focused: Option<bool>,
    allowed: Option<bool>,
) {
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.keyboard_focused = focused;
            device.keyboard_allowed = allowed.filter(|allowed| !allowed);
        }
    }
}

pub(super) fn update_call_status(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    status: Option<crate::device_links::device::CallStatus>,
) -> bool {
    let in_call = status.is_some();
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.call_status = status;
        }
    }
    in_call
}

/// `desklink.notification {snapshot: true, ids}`: the phone's full list of
/// ids. Every cached notification not in it is dropped; the bodies follow.
pub(super) fn replace_notifications(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) {
    let ids = string_vec(packet.body.get("ids")).unwrap_or_default();
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device
                .notifications
                .retain(|notification| ids.contains(&notification.id));
        }
    }
}

pub(super) fn upsert_notification(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) -> Option<DeviceNotification> {
    let id = sanitize_text(packet.get_str("id")?, 240);
    let notification = DeviceNotification {
        id: id.clone(),
        app_name: sanitize_text(
            packet
                .get_str("appName")
                .or_else(|| packet.get_str("app"))
                .unwrap_or("Phone"),
            120,
        ),
        title: sanitize_text(packet.get_str("title").unwrap_or_default(), 240),
        text: sanitize_text(
            packet
                .get_str("text")
                .or_else(|| packet.get_str("message"))
                .unwrap_or_default(),
            500,
        ),
        ticker: sanitize_text(packet.get_str("ticker").unwrap_or_default(), 500),
        is_clearable: packet.get_bool("isClearable").unwrap_or(true),
        request_reply_id: packet
            .get_str("requestReplyId")
            .map(|value| sanitize_text(value, 240)),
        actions: string_vec(packet.body.get("actions")).unwrap_or_default(),
        silent: packet.get_bool("silent").unwrap_or(false),
    };

    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            if let Some(existing) = device
                .notifications
                .iter_mut()
                .find(|existing| existing.id == id)
            {
                *existing = notification.clone();
            } else {
                device.notifications.insert(0, notification.clone());
                device.notifications.truncate(25);
            }
        }
    }

    Some(notification)
}

pub(super) fn remove_notification(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    notification_id: &str,
) {
    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device
                .notifications
                .retain(|notification| notification.id != notification_id);
        }
    }
}

pub(super) fn update_volume_status(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) {
    if let Ok(mut devices) = devices.lock() {
        let Some(device) = devices.get_mut(device_id) else {
            return;
        };

        let mut status = device
            .volume_status
            .clone()
            .unwrap_or(SystemVolumeStatus { sinks: Vec::new() });

        if let Some(sinks) = parse_sink_list(packet.body.get("sinkList")) {
            status.sinks = sinks;
        } else if let Some(name) = packet.get_str("name") {
            let name = sanitize_text(name, 240);
            let sink = status
                .sinks
                .iter_mut()
                .find(|sink| sink.name == name)
                .map(|sink| {
                    if let Some(volume) = packet.get_i64("volume") {
                        sink.volume = volume.max(0);
                    }
                    if let Some(muted) = packet.get_bool("muted") {
                        sink.muted = muted;
                    }
                    if let Some(enabled) = packet.get_bool("enabled") {
                        sink.enabled = enabled;
                    }
                });
            if sink.is_none() {
                status.sinks.push(VolumeSink {
                    name,
                    description: packet
                        .get_str("description")
                        .map(|value| sanitize_text(value, 240))
                        .unwrap_or_else(|| "Audio device".to_string()),
                    volume: packet.get_i64("volume").unwrap_or(0).max(0),
                    max_volume: packet.get_i64("maxVolume"),
                    muted: packet.get_bool("muted").unwrap_or(false),
                    enabled: packet.get_bool("enabled").unwrap_or(false),
                });
            }
        }

        device.volume_status = Some(status);
    }
}

fn sanitize_text(value: &str, max_chars: usize) -> String {
    value
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .take(max_chars)
        .collect()
}

fn string_vec(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(|value| {
        value.as_array().map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|item| sanitize_text(item, 240))
                .collect()
        })
    })
}

fn parse_sink_list(value: Option<&Value>) -> Option<Vec<VolumeSink>> {
    let value = value?;
    let parsed;
    let array = if let Some(array) = value.as_array() {
        array
    } else if let Some(json) = value.as_str() {
        parsed = serde_json::from_str::<Value>(json).ok()?;
        parsed.as_array()?
    } else {
        return None;
    };

    Some(
        array
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let name = object.get("name")?.as_str()?;
                Some(VolumeSink {
                    name: sanitize_text(name, 240),
                    description: object
                        .get("description")
                        .and_then(Value::as_str)
                        .map(|value| sanitize_text(value, 240))
                        .unwrap_or_else(|| sanitize_text(name, 240)),
                    volume: object.get("volume").and_then(Value::as_i64).unwrap_or(0),
                    max_volume: object.get("maxVolume").and_then(Value::as_i64),
                    muted: object
                        .get("muted")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                    enabled: object
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A paired phone that is away must still have a row from startup — the
    /// relay never passes discovery, so nothing else would create it.
    #[test]
    fn paired_devices_are_listed_from_the_trust_store_at_startup() {
        let devices = Arc::new(Mutex::new(HashMap::new()));
        seed_trusted_devices(
            &devices,
            [(
                "phone0000000000000000000000000000".to_string(),
                "Pixel".to_string(),
                "phone".to_string(),
                9,
            )]
            .into_iter(),
        );
        let devices = devices.lock().unwrap();
        let view = &devices["phone0000000000000000000000000000"];
        assert!(view.trusted);
        assert_eq!(view.name, "Pixel");
        assert_eq!(view.device_type, "phone");
        assert!(!view.feature_transport_ready);
    }
    use serde_json::json;

    fn test_devices() -> (Arc<Mutex<HashMap<String, DeviceView>>>, DeviceInfo) {
        let info = DeviceInfo::local(
            "0123456789abcdef0123456789abcdef".to_string(),
            "DeskLink".to_string(),
        );
        let devices = Arc::new(Mutex::new(HashMap::new()));
        upsert_device(&devices, &info, "127.0.0.1".to_string(), true);
        (devices, info)
    }

    fn connection(device_id: &str, linked: bool, ready: bool) -> DeviceConnection {
        DeviceConnection {
            device_id: device_id.to_string(),
            linked,
            paired: true,
            feature_transport_ready: ready,
        }
    }

    /// `allowed: false` sticks until a report without it.
    #[test]
    fn a_refused_typing_permission_is_cleared_by_the_next_report() {
        let (devices, info) = test_devices();
        let allowed = |devices: &Arc<Mutex<HashMap<String, DeviceView>>>| {
            devices.lock().unwrap()[&info.id].keyboard_allowed
        };
        update_keyboard_state(&devices, &info.id, Some(false), Some(false));
        assert_eq!(allowed(&devices), Some(false));
        update_keyboard_state(&devices, &info.id, Some(true), None);
        assert_eq!(allowed(&devices), None);
        update_keyboard_state(&devices, &info.id, Some(false), Some(true));
        assert_eq!(allowed(&devices), None, "only a refusal is kept");
    }

    /// The reported symptom: the app kept showing "Connected" for a paired
    /// device whose link was gone, because every discovery beacon rewrote the
    /// status back to `Paired`.
    #[test]
    fn a_paired_device_with_no_session_is_reported_unreachable() {
        let (devices, info) = test_devices();
        reconcile_connection_status(&devices, &[connection(&info.id, true, true)]);
        assert_eq!(
            devices.lock().unwrap().get(&info.id).unwrap().status,
            DeviceStatus::Paired
        );

        // The link drops and no handler observed it.
        reconcile_connection_status(&devices, &[]);

        let devices = devices.lock().unwrap();
        let device = devices.get(&info.id).unwrap();
        assert_eq!(device.status, DeviceStatus::Unreachable);
        assert!(!device.feature_transport_ready);
        assert!(device.trusted, "trust must survive a disconnect");
    }

    #[test]
    fn discovery_alone_never_reports_a_paired_device_as_connected() {
        let (devices, info) = test_devices();
        reconcile_connection_status(&devices, &[]);

        // A discovery beacon arrives from a powered-on but unconnected phone.
        upsert_device(&devices, &info, "192.168.1.20".to_string(), true);

        let devices = devices.lock().unwrap();
        let device = devices.get(&info.id).unwrap();
        assert_eq!(device.status, DeviceStatus::Unreachable);
        assert!(device.trusted);
        assert_eq!(device.address, "192.168.1.20");
    }

    /// Losing the bootstrap socket while the authenticated WebRTC transport is
    /// alive is not a disconnect, and must not be reported as one.
    #[test]
    fn a_retained_webrtc_transport_keeps_the_device_reachable() {
        let (devices, info) = test_devices();

        reconcile_connection_status(&devices, &[connection(&info.id, false, true)]);

        let devices = devices.lock().unwrap();
        let device = devices.get(&info.id).unwrap();
        assert_eq!(device.status, DeviceStatus::Paired);
        assert!(device.feature_transport_ready);
    }

    /// The inverse: a live bootstrap link before handover is reachable, but the
    /// feature transport must not be advertised as ready.
    #[test]
    fn a_bootstrap_only_link_is_reachable_but_not_feature_ready() {
        let (devices, info) = test_devices();

        reconcile_connection_status(&devices, &[connection(&info.id, true, false)]);

        let devices = devices.lock().unwrap();
        let device = devices.get(&info.id).unwrap();
        assert_eq!(device.status, DeviceStatus::Paired);
        assert!(!device.feature_transport_ready);
    }

    #[test]
    fn reconciliation_leaves_a_pending_pair_prompt_alone_while_reachable() {
        let (devices, info) = test_devices();
        update_pair_state(
            &devices,
            &info.id,
            PairState::RequestedByPeer,
            Some("ABCD1234".to_string()),
        );

        reconcile_connection_status(&devices, &[connection(&info.id, true, false)]);

        let devices = devices.lock().unwrap();
        let device = devices.get(&info.id).unwrap();
        assert_eq!(device.status, DeviceStatus::PairRequestedByPeer);
        assert_eq!(device.verification_key.as_deref(), Some("ABCD1234"));
    }

    /// Pairing cannot complete without a link, so a prompt for a device that
    /// disappeared must be cleared rather than left on screen forever.
    #[test]
    fn a_pair_prompt_is_cleared_once_the_device_disappears() {
        let (devices, info) = test_devices();
        update_pair_state(
            &devices,
            &info.id,
            PairState::RequestedByPeer,
            Some("ABCD1234".to_string()),
        );

        reconcile_connection_status(&devices, &[]);

        let devices = devices.lock().unwrap();
        let device = devices.get(&info.id).unwrap();
        assert_ne!(device.status, DeviceStatus::PairRequestedByPeer);
        assert_eq!(device.verification_key, None);
    }

    #[test]
    fn update_battery_status_updates_existing_device_snapshot() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_BATTERY);
        packet.set("currentCharge", 87);
        packet.set("isCharging", true);
        packet.set("batteryQuantity", 1);

        update_battery_status(&devices, &info.id, &packet);

        let devices = devices.lock().unwrap();
        let battery = devices
            .get(&info.id)
            .unwrap()
            .battery_status
            .as_ref()
            .unwrap();
        assert_eq!(battery.current_charge, 87);
        assert!(battery.is_charging);
        assert_eq!(battery.battery_quantity, Some(1));
    }

    #[test]
    fn phone_media_shows_until_the_phone_has_no_players() {
        let (devices, info) = test_devices();
        let mut playing = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        playing.set("playerList", serde_json::json!(["Spotify"]));
        playing.set("player", "Spotify");
        playing.set("title", "Blurred");
        playing.set("isPlaying", true);
        update_media_from_packet(&devices, &info.id, &playing);
        let title = |devices: &Arc<Mutex<HashMap<String, DeviceView>>>| {
            devices.lock().unwrap()[&info.id]
                .media_status
                .as_ref()
                .map(|media| media.title.clone())
        };
        assert_eq!(title(&devices).as_deref(), Some("Blurred"));
        let mut none = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        none.set("playerList", serde_json::json!([]));
        update_media_from_packet(&devices, &info.id, &none);
        assert_eq!(title(&devices), None);
    }

    #[test]
    fn time_to_full_is_read_when_given_and_unknown_is_none() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_BATTERY);
        packet.set("currentCharge", 40);
        packet.set("isCharging", true);
        packet.set("chargeTimeRemaining", 1_500_000);
        update_battery_status(&devices, &info.id, &packet);
        let remaining = |devices: &Arc<Mutex<HashMap<String, DeviceView>>>| {
            devices.lock().unwrap()[&info.id]
                .battery_status
                .as_ref()
                .unwrap()
                .charge_time_remaining
        };
        assert_eq!(remaining(&devices), Some(1_500_000));
        packet.set("chargeTimeRemaining", -1);
        update_battery_status(&devices, &info.id, &packet);
        assert_eq!(remaining(&devices), None, "-1 means unknown");
        packet.body.remove("chargeTimeRemaining");
        update_battery_status(&devices, &info.id, &packet);
        assert_eq!(remaining(&devices), None, "older phones omit it");
    }

    fn notification(id: &str) -> NetworkPacket {
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_NOTIFICATION);
        packet.set("id", id);
        packet.set("appName", "Chat");
        packet
    }

    #[test]
    fn a_notification_snapshot_replaces_the_list_and_resyncs_are_silent() {
        let (devices, info) = test_devices();
        for id in ["n1", "n2", "n3"] {
            upsert_notification(&devices, &info.id, &notification(id));
        }
        let mut snapshot =
            NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_NOTIFICATION);
        snapshot.set("snapshot", true);
        snapshot.set("ids", json!(["n2", "n4"]));
        replace_notifications(&devices, &info.id, &snapshot);
        let ids = |devices: &Arc<Mutex<HashMap<String, DeviceView>>>| {
            devices.lock().unwrap()[&info.id]
                .notifications
                .iter()
                .map(|n| (n.id.clone(), n.silent))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(&devices),
            [("n2".to_string(), false)],
            "dismissed while away"
        );
        let mut resent = notification("n4");
        resent.set("silent", true);
        upsert_notification(&devices, &info.id, &resent);
        assert_eq!(
            ids(&devices),
            [("n4".to_string(), true), ("n2".to_string(), false)],
            "a resync is kept, marked silent"
        );
        // An empty snapshot (the feature turned off) clears everything.
        snapshot.set("ids", json!([]));
        replace_notifications(&devices, &info.id, &snapshot);
        assert!(ids(&devices).is_empty());
    }

    #[test]
    fn a_player_list_replaces_the_phones_media() {
        let (devices, info) = test_devices();
        let mut first = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        first.set("playerList", json!(["Spotify"]));
        first.set("player", "Spotify");
        first.set("title", "Old song");
        first.set("isPlaying", true);
        update_media_from_packet(&devices, &info.id, &first);
        let mut replaced = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        replaced.set("playerList", json!(["Podcasts"]));
        update_media_from_packet(&devices, &info.id, &replaced);
        let devices = devices.lock().unwrap();
        let media = devices[&info.id].media_status.as_ref().unwrap();
        assert_eq!(media.player, "Podcasts");
        assert!(
            media.title.is_empty() && !media.is_playing,
            "nothing of Spotify lingers"
        );
    }

    #[test]
    fn a_list_that_still_has_the_player_keeps_its_state() {
        let (devices, info) = test_devices();
        let mut first = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        first.set("playerList", json!(["Spotify"]));
        first.set("player", "Spotify");
        first.set("title", "Song");
        first.set("isPlaying", true);
        update_media_from_packet(&devices, &info.id, &first);
        let mut list = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        list.set("playerList", json!(["Spotify", "Podcasts"]));
        update_media_from_packet(&devices, &info.id, &list);
        let devices = devices.lock().unwrap();
        let media = devices[&info.id].media_status.as_ref().unwrap();
        assert_eq!(media.title, "Song");
        assert!(media.is_playing);
        assert_eq!(media.player_list, ["Spotify", "Podcasts"]);
    }

    #[test]
    fn a_battery_of_minus_one_clears_it() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_BATTERY);
        packet.set("currentCharge", 50);
        update_battery_status(&devices, &info.id, &packet);
        assert!(devices.lock().unwrap()[&info.id].battery_status.is_some());
        packet.set("currentCharge", -1);
        update_battery_status(&devices, &info.id, &packet);
        assert!(devices.lock().unwrap()[&info.id].battery_status.is_none());
    }

    #[test]
    fn losing_the_link_marks_the_phones_state_stale_once() {
        let (devices, info) = test_devices();
        let connection = |ready: bool| DeviceConnection {
            device_id: info.id.clone(),
            linked: ready,
            paired: true,
            feature_transport_ready: ready,
        };
        reconcile_connection_status(&devices, &[connection(true)]);
        assert_eq!(devices.lock().unwrap()[&info.id].last_seen, None);
        reconcile_connection_status(&devices, &[connection(false)]);
        let seen = devices.lock().unwrap()[&info.id].last_seen;
        assert!(seen.is_some(), "stale from the edge");
        reconcile_connection_status(&devices, &[connection(false)]);
        assert_eq!(
            devices.lock().unwrap()[&info.id].last_seen,
            seen,
            "kept, not moved"
        );
        reconcile_connection_status(&devices, &[connection(true)]);
        assert_eq!(
            devices.lock().unwrap()[&info.id].last_seen,
            None,
            "live again"
        );
    }

    #[test]
    fn a_beacon_never_changes_trust() {
        let (devices, info) = test_devices();
        upsert_device(&devices, &info, "10.0.0.9".to_string(), false);
        let devices = devices.lock().unwrap();
        assert!(
            devices[&info.id].trusted,
            "only the session or trust store decides"
        );
        assert_eq!(devices[&info.id].address, "10.0.0.9");
    }

    #[test]
    fn upsert_notification_replaces_existing_notification() {
        let (devices, info) = test_devices();
        let mut first = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_NOTIFICATION);
        first.set("id", "n1");
        first.set("appName", "Chat");
        first.set("title", "Old");
        upsert_notification(&devices, &info.id, &first);

        let mut second = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_NOTIFICATION);
        second.set("id", "n1");
        second.set("appName", "Chat");
        second.set("title", "New");
        upsert_notification(&devices, &info.id, &second);

        let devices = devices.lock().unwrap();
        let notifications = &devices.get(&info.id).unwrap().notifications;
        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].title, "New");
    }

    #[test]
    fn mpris_packet_updates_player_list_and_status() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_MPRIS);
        packet.set("playerList", json!(["vlc", "firefox"]));
        packet.set("player", "vlc");
        packet.set("title", "Track");
        packet.set("volume", 55);
        packet.set("pos", 1234);
        packet.set("length", 9999);
        packet.set("canSeek", true);

        update_media_from_packet(&devices, &info.id, &packet);

        let devices = devices.lock().unwrap();
        let media = devices
            .get(&info.id)
            .unwrap()
            .media_status
            .as_ref()
            .unwrap();
        assert_eq!(media.player_list, vec!["vlc", "firefox"]);
        assert_eq!(media.title, "Track");
        assert_eq!(media.volume, Some(55));
        assert_eq!(media.position, Some(1234));
        assert_eq!(media.length, Some(9999));
        assert!(media.can_seek);
    }

    #[test]
    fn volume_packet_updates_sink_list() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SYSTEMVOLUME);
        packet.set(
            "sinkList",
            json!([{"name":"sink1","description":"Speakers","volume":40,"maxVolume":100,"muted":false,"enabled":true}]),
        );

        update_volume_status(&devices, &info.id, &packet);

        let devices = devices.lock().unwrap();
        let volume = devices
            .get(&info.id)
            .unwrap()
            .volume_status
            .as_ref()
            .unwrap();
        assert_eq!(volume.sinks.len(), 1);
        assert_eq!(volume.sinks[0].name, "sink1");
        assert_eq!(volume.sinks[0].volume, 40);
    }
}
