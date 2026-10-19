use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::handle::DaemonEvent;
use crate::device_links::core::DeviceConnection;
use crate::device_links::device::{
    BatteryStatus, DeviceNotification, DeviceStatus, DeviceView, MediaStatus, RemoteCommand,
    SftpStatus, SystemVolumeStatus, VolumeSink,
};
use crate::device_links::device_info::DeviceInfo;
use crate::device_links::packet::NetworkPacket;
use crate::device_links::pairing::PairState;
use serde_json::Value;

/// Unpaired devices shown at once; more than any real LAN needs.
const MAX_UNTRUSTED_VIEWS: usize = 32;

pub(super) fn upsert_device(
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
                // this keeps the interim value honest rather than optimistic.
                device.trusted = paired;
            })
            .or_insert_with(|| DeviceView::from_info(info, address, paired));
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
        device.feature_transport_ready = connection
            .is_some_and(|connection| connection.feature_transport_ready && connection.paired);

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

pub(super) fn push_event(events: &Arc<Mutex<Vec<DaemonEvent>>>, event: DaemonEvent) {
    if let Ok(mut events) = events.lock() {
        events.push(event);
        if events.len() > MAX_PENDING_EVENTS {
            let excess = events.len() - MAX_PENDING_EVENTS;
            events.drain(..excess);
        }
    }
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

        let mut media = device.media_status.clone().unwrap_or_else(|| MediaStatus {
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
            device.battery_status = Some(BatteryStatus {
                current_charge: packet.get_i64("currentCharge").unwrap_or(-1).clamp(-1, 100),
                is_charging: packet.get_bool("isCharging").unwrap_or(false),
                battery_quantity: packet.get_i64("batteryQuantity"),
                threshold_event: packet.get_i64("thresholdEvent"),
            });
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

pub(super) fn update_remote_commands(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) {
    let Some(commands) = packet
        .get_str("commandList")
        .and_then(parse_remote_commands)
        .or_else(|| {
            packet
                .body
                .get("commandList")
                .and_then(parse_remote_commands_value)
        })
    else {
        return;
    };

    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.available_commands = commands;
        }
    }
}

pub(super) fn update_sftp_status(
    devices: &Arc<Mutex<HashMap<String, DeviceView>>>,
    device_id: &str,
    packet: &NetworkPacket,
) {
    let status = SftpStatus {
        user: packet
            .get_str("user")
            .map(|value| sanitize_text(value, 120)),
        port: packet.get_i64("port"),
        path: packet
            .get_str("path")
            .map(|value| sanitize_text(value, 500)),
        password: packet
            .get_str("password")
            .map(|value| sanitize_text(value, 500)),
        directories: parse_sftp_directories(packet),
        error: packet
            .get_str("errorMessage")
            .map(|value| sanitize_text(value, 500)),
    };

    if let Ok(mut devices) = devices.lock() {
        if let Some(device) = devices.get_mut(device_id) {
            device.sftp_status = Some(status);
        }
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

fn parse_remote_commands(value: &str) -> Option<Vec<RemoteCommand>> {
    let parsed = serde_json::from_str::<Value>(value).ok()?;
    parse_remote_commands_value(&parsed)
}

fn parse_remote_commands_value(value: &Value) -> Option<Vec<RemoteCommand>> {
    let object = value.as_object()?;
    let mut commands = object
        .iter()
        .filter_map(|(key, value)| {
            let object = value.as_object()?;
            Some(RemoteCommand {
                key: sanitize_text(key, 120),
                name: object
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|value| sanitize_text(value, 120))
                    .unwrap_or_else(|| sanitize_text(key, 120)),
                command: object
                    .get("command")
                    .and_then(Value::as_str)
                    .map(|value| sanitize_text(value, 500)),
            })
        })
        .collect::<Vec<_>>();
    commands.sort_by(|left, right| left.name.cmp(&right.name));
    Some(commands)
}

fn parse_sftp_directories(packet: &NetworkPacket) -> Vec<(String, String)> {
    let paths = string_vec(packet.body.get("multiPaths")).unwrap_or_default();
    let names = string_vec(packet.body.get("pathNames")).unwrap_or_default();
    paths.into_iter().zip(names).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn remote_command_packet_updates_available_commands() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_RUNCOMMAND);
        packet.set(
            "commandList",
            r#"{"lock":{"name":"Lock Phone","command":"loginctl lock-session"}}"#,
        );

        update_remote_commands(&devices, &info.id, &packet);

        let devices = devices.lock().unwrap();
        let commands = &devices.get(&info.id).unwrap().available_commands;
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].key, "lock");
        assert_eq!(commands[0].name, "Lock Phone");
    }

    #[test]
    fn sftp_packet_updates_connection_status() {
        let (devices, info) = test_devices();
        let mut packet = NetworkPacket::new(crate::device_links::packet::PACKET_TYPE_SFTP);
        packet.set("user", "kdeconnect");
        packet.set("port", 1739);
        packet.set("path", "/");
        packet.set("multiPaths", json!(["/DCIM"]));
        packet.set("pathNames", json!(["Camera"]));

        update_sftp_status(&devices, &info.id, &packet);

        let devices = devices.lock().unwrap();
        let sftp = devices.get(&info.id).unwrap().sftp_status.as_ref().unwrap();
        assert_eq!(sftp.user.as_deref(), Some("kdeconnect"));
        assert_eq!(sftp.port, Some(1739));
        assert_eq!(
            sftp.directories,
            vec![("/DCIM".to_string(), "Camera".to_string())]
        );
    }
}
